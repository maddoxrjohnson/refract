//! Runs the client against the fake helper in `src/bin/fake-agent.rs`.

use std::io;
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use refract_client::{AgentClient, AgentStatus, Backoff, ClientEvent, RequestError};
use refract_core::protocol::{Event, Method, Reply, RepoList, PROTOCOL_VERSION};
use serde_json::json;

const WAIT: Duration = Duration::from_secs(5);

/// Collects everything passed to `on_event`.
#[derive(Default)]
struct Recorder {
    events: Mutex<Vec<ClientEvent>>,
    changed: Condvar,
}

impl Recorder {
    fn record(&self, event: ClientEvent) {
        self.events.lock().unwrap().push(event);
        self.changed.notify_all();
    }

    fn statuses(&self) -> Vec<AgentStatus> {
        let events = self.events.lock().unwrap();
        events
            .iter()
            .filter_map(|e| match e {
                ClientEvent::Status(s) => Some(s.clone()),
                ClientEvent::Event(_) => None,
            })
            .collect()
    }

    /// Waits until the recorded events satisfy `done`.
    fn wait_until(&self, what: &str, done: impl Fn(&[ClientEvent]) -> bool) {
        let deadline = Instant::now() + WAIT;
        let mut events = self.events.lock().unwrap();
        while !done(&events) {
            let left = deadline
                .checked_duration_since(Instant::now())
                .unwrap_or_else(|| panic!("timed out waiting for {what}; got {events:?}"));
            events = self.changed.wait_timeout(events, left).unwrap().0;
        }
    }

    /// Waits until some recorded event satisfies `found`, and returns it.
    fn wait_for(&self, what: &str, found: impl Fn(&ClientEvent) -> bool) -> ClientEvent {
        self.wait_until(what, |events| events.iter().any(&found));
        let events = self.events.lock().unwrap();
        events.iter().find(|e| found(e)).cloned().unwrap()
    }

    /// Waits until at least `count` statuses equal to `status` have been recorded.
    fn wait_status_count(&self, status: &AgentStatus, count: usize) {
        self.wait_until(&format!("{status:?} x{count}"), |events| {
            let is = |e: &&ClientEvent| matches!(e, ClientEvent::Status(s) if s == status);
            events.iter().filter(is).count() >= count
        });
    }
}

/// The `attempt` of every `Restarting` status, in order.
fn attempts(statuses: Vec<AgentStatus>) -> Vec<u32> {
    statuses
        .into_iter()
        .filter_map(|s| match s {
            AgentStatus::Restarting { attempt, .. } => Some(attempt),
            _ => None,
        })
        .collect()
}

fn fake(args: &[&str]) -> io::Result<Child> {
    Command::new(env!("CARGO_BIN_EXE_fake-agent"))
        .args(args)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
}

fn fast() -> Backoff {
    Backoff {
        initial: Duration::from_millis(10),
        max: Duration::from_millis(40),
        reset_after: Duration::from_secs(60),
    }
}

/// Starts a client on the fake helper and waits until it is running.
fn start_fake(backoff: Backoff) -> (AgentClient, Arc<Recorder>) {
    let recorder = Arc::new(Recorder::default());
    let sink = Arc::clone(&recorder);
    let client = AgentClient::start(|| fake(&[]), move |e| sink.record(e), backoff);
    recorder.wait_status_count(&AgentStatus::Running, 1);
    (client, recorder)
}

fn get_repo(path: &str) -> Method {
    Method::GetRepo { path: path.into() }
}

fn echoed(path: &str) -> Result<Reply, RequestError> {
    Err(RequestError::Agent {
        code: 1,
        message: path.into(),
    })
}

#[test]
fn ping_round_trip() {
    let (client, recorder) = start_fake(fast());
    assert_eq!(client.status(), AgentStatus::Running);
    match client.request(Method::Ping, WAIT) {
        Ok(Reply::Hello(hello)) => {
            assert_eq!(hello.protocol_version, PROTOCOL_VERSION);
            assert_eq!(hello.agent_version, "fake");
        }
        other => panic!("expected hello, got {other:?}"),
    }
    client.shutdown(WAIT);
    assert_eq!(
        recorder.statuses(),
        [
            AgentStatus::Starting,
            AgentStatus::Running,
            AgentStatus::Stopped
        ]
    );
}

#[test]
fn concurrent_requests_reach_their_callers() {
    let (client, _recorder) = start_fake(fast());
    // Later threads ask for shorter delays, so answers come back out of order.
    let threads: Vec<_> = (0..8)
        .map(|i| {
            let client = client.clone();
            thread::spawn(move || {
                let path = format!("slow:{}", (8 - i) * 15);
                (client.request(get_repo(&path), WAIT), path)
            })
        })
        .collect();
    let ping = client.request(Method::Ping, WAIT);
    assert!(matches!(ping, Ok(Reply::Hello(_))), "{ping:?}");
    for thread in threads {
        let (result, path) = thread.join().unwrap();
        assert_eq!(result, echoed(&path));
    }
    client.shutdown(WAIT);
}

#[test]
fn events_are_forwarded() {
    let (client, recorder) = start_fake(fast());
    let reply = client.request(Method::ListRepos, WAIT);
    let expected = RepoList {
        repos: Vec::new(),
        loading: true,
    };
    assert_eq!(reply, Ok(Reply::Repos(expected)));
    recorder.wait_for("loadComplete", |e| {
        matches!(e, ClientEvent::Event(Event::LoadComplete))
    });
    client.shutdown(WAIT);
}

#[test]
fn unreadable_lines_are_skipped() {
    let (client, _recorder) = start_fake(fast());
    assert_eq!(client.request(get_repo("garbage"), WAIT), echoed("garbage"));
    assert!(matches!(
        client.request(Method::Ping, WAIT),
        Ok(Reply::Hello(_))
    ));
    client.shutdown(WAIT);
}

#[test]
fn an_unreadable_answer_fails_its_request_at_once() {
    let (client, _recorder) = start_fake(fast());
    let started = Instant::now();
    match client.request(get_repo("badreply"), WAIT) {
        Err(RequestError::Agent { code, message }) => {
            assert_eq!(code, -32603);
            assert!(message.starts_with("unreadable response: "), "{message}");
        }
        other => panic!("expected an agent error, got {other:?}"),
    }
    assert!(started.elapsed() < Duration::from_secs(1));
    assert!(matches!(
        client.request(Method::Ping, WAIT),
        Ok(Reply::Hello(_))
    ));
    client.shutdown(WAIT);
}

#[test]
fn a_helper_that_never_reads_cannot_hold_up_shutdown() {
    let recorder = Arc::new(Recorder::default());
    let sink = Arc::clone(&recorder);
    let client = AgentClient::start(|| fake(&["--never-read"]), move |e| sink.record(e), fast());
    recorder.wait_status_count(&AgentStatus::Running, 1);

    // Far more than a pipe holds, so this write blocks while holding stdin.
    let stuck = {
        let client = client.clone();
        thread::spawn(move || client.request(get_repo(&"x".repeat(4 << 20)), WAIT))
    };
    thread::sleep(Duration::from_millis(200));

    // A request that cannot get at stdin still times out on time.
    let started = Instant::now();
    assert_eq!(
        client.request(Method::Ping, Duration::from_millis(100)),
        Err(RequestError::Timeout)
    );
    assert!(started.elapsed() < Duration::from_secs(1));

    let started = Instant::now();
    client.shutdown(Duration::from_millis(200));
    assert!(
        started.elapsed() < Duration::from_secs(3),
        "{:?}",
        started.elapsed()
    );
    assert_eq!(client.status(), AgentStatus::Stopped);
    assert_eq!(stuck.join().unwrap(), Err(RequestError::Disconnected));
}

#[test]
fn crash_fails_pending_requests_and_restarts() {
    let (client, recorder) = start_fake(fast());
    let waiting = {
        let client = client.clone();
        thread::spawn(move || client.request(get_repo("slow:3000"), WAIT))
    };
    // Give the slow request time to be written before the crash.
    thread::sleep(Duration::from_millis(100));
    assert_eq!(
        client.request(get_repo("crash"), WAIT),
        Err(RequestError::Disconnected)
    );
    assert_eq!(waiting.join().unwrap(), Err(RequestError::Disconnected));

    recorder.wait_status_count(&AgentStatus::Running, 2);
    let statuses = recorder.statuses();
    assert_eq!(statuses.len(), 5, "{statuses:?}");
    assert_eq!(statuses[..2], [AgentStatus::Starting, AgentStatus::Running]);
    match &statuses[2] {
        AgentStatus::Restarting {
            attempt: 1,
            retry_in_ms: 10,
            last_error: Some(error),
        } => assert!(error.contains("crashing on request"), "{error}"),
        other => panic!("expected restarting, got {other:?}"),
    }
    assert_eq!(statuses[3..], [AgentStatus::Starting, AgentStatus::Running]);

    assert!(matches!(
        client.request(Method::Ping, WAIT),
        Ok(Reply::Hello(_))
    ));
    client.shutdown(WAIT);
}

#[test]
fn spawn_failure_backs_off_and_caps() {
    let recorder = Arc::new(Recorder::default());
    let sink = Arc::clone(&recorder);
    let spawn = || -> io::Result<Child> {
        Err(io::Error::new(io::ErrorKind::NotFound, "wsl.exe not found"))
    };
    let client = AgentClient::start(spawn, move |e| sink.record(e), fast());
    recorder.wait_for("fifth restart", |e| {
        matches!(
            e,
            ClientEvent::Status(AgentStatus::Restarting { attempt: 5, .. })
        )
    });
    assert_eq!(
        client.request(Method::Ping, WAIT),
        Err(RequestError::NotRunning)
    );
    client.shutdown(WAIT);

    let restarts: Vec<(u32, u64)> = recorder
        .statuses()
        .into_iter()
        .filter_map(|s| match s {
            AgentStatus::Restarting {
                attempt,
                retry_in_ms,
                last_error,
            } => {
                let error = last_error.unwrap_or_default();
                assert!(error.contains("wsl.exe not found"), "{error}");
                Some((attempt, retry_in_ms))
            }
            _ => None,
        })
        .collect();
    assert_eq!(restarts[..5], [(1, 10), (2, 20), (3, 40), (4, 40), (5, 40)]);
    assert_eq!(client.status(), AgentStatus::Stopped);
}

#[test]
fn missing_program_reports_the_spawn_error() {
    let recorder = Arc::new(Recorder::default());
    let sink = Arc::clone(&recorder);
    let spawn = || {
        Command::new("/nonexistent/refract-agent")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
    };
    let client = AgentClient::start(spawn, move |e| sink.record(e), fast());
    let event = recorder.wait_for("restarting", |e| {
        matches!(e, ClientEvent::Status(AgentStatus::Restarting { .. }))
    });
    let ClientEvent::Status(AgentStatus::Restarting { last_error, .. }) = event else {
        unreachable!()
    };
    let error = last_error.unwrap_or_default();
    assert!(error.starts_with("cannot start the helper: "), "{error}");
    client.shutdown(WAIT);
}

#[test]
fn a_long_healthy_run_resets_the_backoff() {
    let recorder = Arc::new(Recorder::default());
    let sink = Arc::clone(&recorder);
    let backoff = Backoff {
        initial: Duration::from_millis(10),
        max: Duration::from_millis(1000),
        reset_after: Duration::from_millis(150),
    };
    // Spawns 1 and 2 exit at once; spawn 3 runs past `reset_after`; spawn 4 exits at once.
    let spawns = Arc::new(AtomicUsize::new(0));
    let counter = Arc::clone(&spawns);
    let spawn = move || match counter.fetch_add(1, Ordering::SeqCst) {
        2 => fake(&["--exit-after-ms=300"]),
        _ => fake(&["--exit-after-ms=0"]),
    };
    let client = AgentClient::start(spawn, move |e| sink.record(e), backoff);
    recorder.wait_until("four restarts", |events| {
        let restarting =
            |e: &&ClientEvent| matches!(e, ClientEvent::Status(AgentStatus::Restarting { .. }));
        events.iter().filter(restarting).count() >= 4
    });
    client.shutdown(WAIT);
    assert_eq!(attempts(recorder.statuses())[..4], [1, 2, 1, 2]);
}

#[test]
fn late_response_after_timeout_is_dropped() {
    let (client, _recorder) = start_fake(fast());
    let started = Instant::now();
    assert_eq!(
        client.request(get_repo("slow:300"), Duration::from_millis(50)),
        Err(RequestError::Timeout)
    );
    assert!(started.elapsed() < Duration::from_millis(250));
    thread::sleep(Duration::from_millis(400));
    assert_eq!(client.request(get_repo("after"), WAIT), echoed("after"));
    assert_eq!(client.status(), AgentStatus::Running);
    client.shutdown(WAIT);
}

#[test]
fn shutdown_during_backoff_is_prompt() {
    let recorder = Arc::new(Recorder::default());
    let sink = Arc::clone(&recorder);
    let spawns = Arc::new(AtomicUsize::new(0));
    let counter = Arc::clone(&spawns);
    let spawn = move || -> io::Result<Child> {
        counter.fetch_add(1, Ordering::SeqCst);
        Err(io::Error::other("no helper"))
    };
    let backoff = Backoff {
        initial: Duration::from_secs(30),
        ..Backoff::default()
    };
    let client = AgentClient::start(spawn, move |e| sink.record(e), backoff);
    recorder.wait_for("restarting", |e| {
        matches!(e, ClientEvent::Status(AgentStatus::Restarting { .. }))
    });
    let started = Instant::now();
    client.shutdown(Duration::from_secs(5));
    assert!(
        started.elapsed() < Duration::from_secs(1),
        "{:?}",
        started.elapsed()
    );
    assert_eq!(client.status(), AgentStatus::Stopped);
    assert_eq!(recorder.statuses().last(), Some(&AgentStatus::Stopped));

    thread::sleep(Duration::from_millis(100));
    assert_eq!(spawns.load(Ordering::SeqCst), 1);
    assert_eq!(
        client.request(Method::Ping, WAIT),
        Err(RequestError::NotRunning)
    );
    // Idempotent: a second shutdown returns at once and reports nothing new.
    let count = recorder.statuses().len();
    client.shutdown(Duration::from_secs(5));
    assert_eq!(recorder.statuses().len(), count);
}

#[test]
fn shutdown_while_running_stops_the_helper() {
    let (client, recorder) = start_fake(fast());
    let started = Instant::now();
    client.shutdown(WAIT);
    assert!(started.elapsed() < Duration::from_secs(1));
    assert_eq!(client.status(), AgentStatus::Stopped);
    thread::sleep(Duration::from_millis(100));
    assert_eq!(recorder.statuses().last(), Some(&AgentStatus::Stopped));
    assert_eq!(recorder.statuses().len(), 3);
}

#[test]
fn serialized_forms_are_camel_case() {
    let restarting = ClientEvent::Status(AgentStatus::Restarting {
        attempt: 2,
        retry_in_ms: 2000,
        last_error: Some("gone".into()),
    });
    assert_eq!(
        serde_json::to_value(&restarting).unwrap(),
        json!({"kind": "status", "state": "restarting", "attempt": 2, "retryInMs": 2000, "lastError": "gone"})
    );
    assert_eq!(
        serde_json::to_value(ClientEvent::Status(AgentStatus::Running)).unwrap(),
        json!({"kind": "status", "state": "running"})
    );
    assert_eq!(
        serde_json::to_value(ClientEvent::Event(Event::LoadComplete)).unwrap(),
        json!({"kind": "event", "event": "loadComplete"})
    );
    assert_eq!(
        serde_json::to_value(RequestError::Agent {
            code: 1,
            message: "no".into()
        })
        .unwrap(),
        json!({"kind": "agent", "code": 1, "message": "no"})
    );
    assert_eq!(
        serde_json::to_value(RequestError::NotRunning).unwrap(),
        json!({"kind": "notRunning"})
    );
}
