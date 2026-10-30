//! A stand-in for `refract-agent` used by this crate's tests. It speaks the same
//! protocol but knows no repos; `getRepo` paths are commands instead:
//!
//! - `crash`: write a line to stderr and exit with status 3 without answering.
//! - `slow:<ms>`: answer after `<ms>` milliseconds, without holding up other requests.
//! - `garbage`: write a line that is not JSON and an error with a null id first.
//! - `badreply`: answer with a result of the wrong shape (`42`) instead.
//! - anything else: answer at once.
//!
//! Every `getRepo` answer is error 1 with the path as its message, so callers can tell
//! which answer is theirs. `listRepos` answers with an empty list still loading, then
//! sends `loadComplete`. With `--exit-after-ms=<ms>` the helper exits with status 1
//! after that long. With `--never-read` it never reads its input and never answers.

use std::io::{self, BufRead, Write};
use std::process::exit;
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::Duration;

use refract_core::protocol::{
    code, parse_request, Event, Hello, Message, Method, Reply, RepoList, Response, PROTOCOL_VERSION,
};

type Out = Arc<Mutex<io::Stdout>>;

fn send(out: &Out, message: &Message) {
    let Ok(line) = serde_json::to_string(message) else {
        return;
    };
    send_raw(out, &line);
}

fn send_raw(out: &Out, line: &str) {
    let mut out = out.lock().unwrap_or_else(|e| e.into_inner());
    let _ = writeln!(out, "{line}").and_then(|()| out.flush());
}

fn main() {
    let out: Out = Arc::new(Mutex::new(io::stdout()));
    for arg in std::env::args().skip(1) {
        if let Some(ms) = arg.strip_prefix("--exit-after-ms=") {
            let ms = ms.parse().unwrap_or(0);
            thread::spawn(move || {
                thread::sleep(Duration::from_millis(ms));
                exit(1);
            });
        }
        if arg == "--never-read" {
            loop {
                thread::sleep(Duration::from_secs(3600));
            }
        }
    }
    for line in io::stdin().lock().lines() {
        let Ok(line) = line else { break };
        let request = match parse_request(&line) {
            Ok(request) => request,
            Err(e) => {
                send(&out, &Message::Response(e.response()));
                continue;
            }
        };
        let id = request.id;
        match request.method {
            Method::Ping => {
                let hello = Hello {
                    protocol_version: PROTOCOL_VERSION,
                    agent_version: "fake".into(),
                };
                send(
                    &out,
                    &Message::Response(Response::ok(id, Reply::Hello(hello))),
                );
            }
            Method::ListRepos => {
                let list = RepoList {
                    repos: Vec::new(),
                    loading: true,
                };
                send(
                    &out,
                    &Message::Response(Response::ok(id, Reply::Repos(list))),
                );
                send(&out, &Message::Event(Event::LoadComplete));
            }
            Method::GetRepo { path } => {
                if path == "crash" {
                    eprintln!("fake-agent: crashing on request");
                    exit(3);
                }
                if path == "badreply" {
                    let reply = serde_json::json!({"id": &id, "result": 42});
                    send_raw(&out, &reply.to_string());
                    continue;
                }
                if path == "garbage" {
                    send_raw(&out, "this is not json");
                    send_raw(
                        &out,
                        r#"{"id":null,"error":{"code":-32700,"message":"bad"}}"#,
                    );
                }
                let delay = path
                    .strip_prefix("slow:")
                    .and_then(|ms| ms.parse().ok())
                    .map(Duration::from_millis);
                let response = Response::err(id, code::REPO_NOT_FOUND, path);
                match delay {
                    Some(delay) => {
                        let out = Arc::clone(&out);
                        thread::spawn(move || {
                            thread::sleep(delay);
                            send(&out, &Message::Response(response));
                        });
                    }
                    None => send(&out, &Message::Response(response)),
                }
            }
            Method::Shutdown => {
                send(&out, &Message::Response(Response::ok(id, Reply::Done)));
                return;
            }
            _ => {
                let response = Response::err(id, code::METHOD_NOT_FOUND, "not supported");
                send(&out, &Message::Response(response));
            }
        }
    }
}
