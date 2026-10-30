//! The supervisor thread: starts the helper, reads its output until it exits, and
//! starts it again after a delay.
//!
//! The supervisor is also the reader of the running helper's stdout: responses go to
//! the waiting callers and events to `on_event`. A second thread per helper reads its
//! stderr, logging each line and keeping the last few for `last_error`.

use std::collections::VecDeque;
use std::io::{BufRead, BufReader, Read};
use std::process::{Child, ChildStdout, ExitStatus};
use std::sync::mpsc::{self, Receiver};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use refract_core::protocol::{code, ErrorBody, Id, Message, Outcome, Response};

use crate::backoff::Backoff;
use crate::client::{lock, Shared, Spawn};
use crate::types::{AgentStatus, ClientEvent};

/// Lines of stderr kept for `last_error`.
const STDERR_LINES: usize = 20;

/// How long to wait for the helper to exit once its stdout has closed, before killing it.
const REAP_WAIT: Duration = Duration::from_secs(2);

/// How often to check whether the helper has exited during [`REAP_WAIT`].
const REAP_POLL: Duration = Duration::from_millis(20);

/// How long to wait for the rest of stderr after the helper has exited.
const STDERR_WAIT: Duration = Duration::from_millis(500);

/// Runs until shutdown: start the helper, serve it until it exits, wait, repeat.
pub(crate) fn supervise(shared: Arc<Shared>, mut spawn: impl Spawn, backoff: Backoff) {
    let mut attempt = 0;
    while !shared.stopping() {
        shared.set_status(AgentStatus::Starting);
        let (ran_for, last_error) = match spawn.spawn() {
            Ok(child) => {
                let (ran_for, last_error) = serve(&shared, child);
                (Some(ran_for), last_error)
            }
            Err(e) => (None, Some(format!("cannot start the helper: {e}"))),
        };
        if shared.stopping() {
            break;
        }
        attempt = backoff.next_attempt(attempt, ran_for);
        let delay = backoff.delay(attempt);
        if let Some(error) = &last_error {
            eprintln!("refract-client: {error}; restarting in {delay:?}");
        }
        shared.set_status(AgentStatus::Restarting {
            attempt,
            retry_in_ms: u64::try_from(delay.as_millis()).unwrap_or(u64::MAX),
            last_error,
        });
        if shared.sleep(delay) {
            break;
        }
    }
    shared.finish();
}

/// Serves one helper until its stdout closes. Fails its pending requests, reaps it, and
/// returns how long it ran and why it stopped.
fn serve(shared: &Shared, mut child: Child) -> (Duration, Option<String>) {
    let started = Instant::now();
    let tail = Arc::new(Mutex::new(VecDeque::new()));
    let stderr_done = child
        .stderr
        .take()
        .map(|stderr| read_stderr(stderr, Arc::clone(&tail)));
    let (Some(stdin), Some(stdout)) = (child.stdin.take(), child.stdout.take()) else {
        let _ = child.kill();
        let _ = child.wait();
        let error = "the helper's stdin and stdout must be piped".to_owned();
        return (started.elapsed(), Some(error));
    };

    // Stdin goes in before `stopping` is checked: a shutdown that begins after the
    // check is sure to find it and close it.
    *lock(&shared.stdin) = Some(stdin);
    {
        let mut state = lock(&shared.state);
        if state.stopping {
            let _ = child.kill();
        }
        state.child = Some(child);
    }
    shared.set_status(AgentStatus::Running);

    read_messages(shared, stdout);
    let ran_for = started.elapsed();

    lock(&shared.stdin).take();
    let child = {
        let mut state = lock(&shared.state);
        // Dropping the senders wakes every waiting caller with `Disconnected`.
        state.pending.clear();
        state.child.take()
    };
    let status = child.and_then(reap);
    if let Some(done) = stderr_done {
        let _ = done.recv_timeout(STDERR_WAIT);
    }
    let tail = lock(&tail);
    let last_error = if tail.is_empty() {
        status.map(|status| format!("the helper exited ({status})"))
    } else {
        Some(tail.iter().cloned().collect::<Vec<_>>().join("\n"))
    };
    (ran_for, last_error)
}

/// Reads the helper's stdout until it closes, routing each message.
fn read_messages(shared: &Shared, stdout: ChildStdout) {
    let mut reader = BufReader::new(stdout);
    let mut buf = Vec::new();
    loop {
        buf.clear();
        match reader.read_until(b'\n', &mut buf) {
            Ok(0) => return,
            Ok(_) => {}
            Err(e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
            Err(e) => {
                eprintln!("refract-client: cannot read the helper's output: {e}");
                return;
            }
        }
        let line = buf.trim_ascii();
        if line.is_empty() {
            continue;
        }
        match serde_json::from_slice::<Message>(line) {
            Ok(Message::Response(response)) => deliver(shared, response),
            Ok(Message::Event(event)) => shared.emit(ClientEvent::Event(event)),
            Err(e) => {
                eprintln!(
                    "refract-client: skipping a line from the helper ({e}): {}",
                    excerpt(line)
                );
                fail_unreadable(shared, line, &e);
            }
        }
    }
}

/// Bytes of an unreadable line that are logged. The rest could hold Claude's replies
/// or paths, which do not belong in a log.
const EXCERPT: usize = 200;

/// The start of `line` for the log: up to [`EXCERPT`] bytes, cut on a character
/// boundary, and the full length when anything was left out.
fn excerpt(line: &[u8]) -> String {
    let text = String::from_utf8_lossy(line);
    if text.len() <= EXCERPT {
        return text.into_owned();
    }
    let mut end = EXCERPT;
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    format!("{}… ({} bytes)", &text[..end], line.len())
}

/// Fails the request an unreadable line answers, when its `id` can still be read, so
/// the caller hears at once rather than at its timeout.
fn fail_unreadable(shared: &Shared, line: &[u8], error: &serde_json::Error) {
    let id = serde_json::from_slice::<serde_json::Value>(line)
        .ok()
        .and_then(|value| value.get("id")?.as_u64());
    let Some(id) = id else {
        return;
    };
    let sender = lock(&shared.state).pending.remove(&id);
    if let Some(sender) = sender {
        let _ = sender.send(Outcome::Error(ErrorBody {
            code: code::INTERNAL_ERROR,
            message: format!("unreadable response: {error}"),
        }));
    }
}

/// Hands a response to the caller waiting for it. A response nobody is waiting for
/// (its request timed out, or it answers `shutdown`) is dropped.
fn deliver(shared: &Shared, response: Response) {
    let id = match &response.id {
        Some(Id::Number(n)) => n.as_u64(),
        _ => None,
    };
    let Some(id) = id else {
        eprintln!(
            "refract-client: the helper answered a request it could not read: {:?}",
            response.outcome
        );
        return;
    };
    let sender = lock(&shared.state).pending.remove(&id);
    if let Some(sender) = sender {
        let _ = sender.send(response.outcome);
    }
}

/// Waits for a helper whose stdout has closed to exit, killing it if it lingers.
fn reap(mut child: Child) -> Option<ExitStatus> {
    let deadline = Instant::now() + REAP_WAIT;
    while Instant::now() < deadline {
        match child.try_wait() {
            Ok(Some(status)) => return Some(status),
            Ok(None) => thread::sleep(REAP_POLL),
            Err(_) => break,
        }
    }
    let _ = child.kill();
    child.wait().ok()
}

/// Reads stderr on its own thread, logging each line and keeping the last
/// [`STDERR_LINES`] in `tail`. The returned receiver disconnects when stderr closes.
fn read_stderr(
    stderr: impl Read + Send + 'static,
    tail: Arc<Mutex<VecDeque<String>>>,
) -> Receiver<()> {
    let (done, finished) = mpsc::channel::<()>();
    let started = thread::Builder::new()
        .name("refract-agent-stderr".into())
        .spawn(move || {
            let _done = done;
            let mut reader = BufReader::new(stderr);
            let mut buf = Vec::new();
            loop {
                buf.clear();
                match reader.read_until(b'\n', &mut buf) {
                    Ok(0) | Err(_) => return,
                    Ok(_) => {}
                }
                let line = String::from_utf8_lossy(buf.trim_ascii_end()).into_owned();
                eprintln!("refract-agent: {line}");
                let mut tail = lock(&tail);
                if tail.len() == STDERR_LINES {
                    tail.pop_front();
                }
                tail.push_back(line);
            }
        });
    if let Err(e) = started {
        eprintln!("refract-client: cannot start the stderr reader: {e}");
    }
    finished
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unreadable_lines_are_cut_short() {
        assert_eq!(excerpt(b"{\"id\": 3, oops"), "{\"id\": 3, oops");
        let long = format!("{}{}", "x".repeat(199), "\u{e9}".repeat(300));
        let cut = excerpt(long.as_bytes());
        assert_eq!(cut, format!("{}… (799 bytes)", "x".repeat(199)));
        let exact = "y".repeat(EXCERPT);
        assert_eq!(excerpt(exact.as_bytes()), exact);
    }
}
