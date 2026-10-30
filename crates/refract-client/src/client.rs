//! The handle the app holds, and the state it shares with the supervisor thread.

use std::collections::HashMap;
use std::io::{self, Write};
use std::process::{Child, ChildStdin};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc::{self, RecvTimeoutError, Sender};
use std::sync::{Arc, Condvar, Mutex, MutexGuard, PoisonError, TryLockError};
use std::thread;
use std::time::{Duration, Instant};

use refract_core::protocol::{code, Method, Outcome, Reply, Request};

use crate::backoff::Backoff;
use crate::supervisor;
use crate::types::{AgentStatus, ClientEvent, RequestError};

/// How long `shutdown` waits for the helper to exit after killing it.
const KILL_WAIT: Duration = Duration::from_secs(2);

/// How often `request` tries again for the helper's stdin while another write holds it.
const STDIN_POLL: Duration = Duration::from_millis(2);

/// Starts the helper process. Called again for each restart.
pub trait Spawn: Send + 'static {
    /// A child with stdin, stdout and stderr all piped.
    fn spawn(&mut self) -> io::Result<Child>;
}

impl<F> Spawn for F
where
    F: FnMut() -> io::Result<Child> + Send + 'static,
{
    fn spawn(&mut self) -> io::Result<Child> {
        self()
    }
}

/// Runs the helper and talks to it. Clones share the same helper.
///
/// The helper keeps running until [`AgentClient::shutdown`]; dropping every handle does
/// not stop it.
#[derive(Clone)]
pub struct AgentClient {
    shared: Arc<Shared>,
}

/// State shared between the handles and the supervisor thread.
pub(crate) struct Shared {
    pub(crate) state: Mutex<State>,
    /// Signalled when `stopping` or `finished` is set.
    pub(crate) wake: Condvar,
    /// The running helper's input. Kept apart from `state` so that a slow write never
    /// holds up reading the helper's output.
    pub(crate) stdin: Mutex<Option<ChildStdin>>,
    /// Held while calling `on_event`, so the app sees events in the order they happened.
    emit: Mutex<()>,
    on_event: Box<dyn Fn(ClientEvent) + Send + Sync>,
    next_id: AtomicU64,
}

pub(crate) struct State {
    pub(crate) status: AgentStatus,
    /// Requests waiting for a response, by id. Dropping a sender tells its caller the
    /// helper is gone.
    pub(crate) pending: HashMap<u64, Sender<Outcome>>,
    /// The running helper, so `shutdown` can kill it.
    pub(crate) child: Option<Child>,
    /// Set by `shutdown`; nothing is started after it.
    pub(crate) stopping: bool,
    /// Set when the supervisor thread is done.
    pub(crate) finished: bool,
}

/// Locks `mutex`, carrying on if another thread panicked while holding it.
pub(crate) fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}

impl AgentClient {
    /// Starts the helper on a supervisor thread and returns at once, with the status
    /// `Starting`. `on_event` receives every status change and every event the helper
    /// sends; it is called from the client's threads and should return quickly.
    pub fn start(
        spawn: impl Spawn,
        on_event: impl Fn(ClientEvent) + Send + Sync + 'static,
        backoff: Backoff,
    ) -> AgentClient {
        let shared = Arc::new(Shared {
            state: Mutex::new(State {
                status: AgentStatus::Starting,
                pending: HashMap::new(),
                child: None,
                stopping: false,
                finished: false,
            }),
            wake: Condvar::new(),
            stdin: Mutex::new(None),
            emit: Mutex::new(()),
            on_event: Box::new(on_event),
            next_id: AtomicU64::new(1),
        });
        let worker = Arc::clone(&shared);
        let started = thread::Builder::new()
            .name("refract-client".into())
            .spawn(move || supervisor::supervise(worker, spawn, backoff));
        if let Err(e) = started {
            eprintln!("refract-client: cannot start the supervisor thread: {e}");
            {
                let mut state = lock(&shared.state);
                state.stopping = true;
                state.finished = true;
            }
            shared.set_status(AgentStatus::Stopped);
        }
        AgentClient { shared }
    }

    /// Sends one request and waits for its response.
    ///
    /// `timeout` covers waiting for the helper's stdin as well as for the answer, so a
    /// helper that has stopped reading its input times requests out rather than holding
    /// them up. The one write that is under way when its pipe fills can still outlast
    /// its timeout, until the helper reads again or `shutdown` kills it; the helper
    /// drains its input as it arrives, so that takes a frozen helper.
    pub fn request(&self, method: Method, timeout: Duration) -> Result<Reply, RequestError> {
        let deadline = Instant::now() + timeout;
        let shared = &*self.shared;
        let id = shared.next_id.fetch_add(1, Ordering::Relaxed);
        let line = serde_json::to_string(&Request {
            id: id.into(),
            method,
        })
        .map_err(|e| RequestError::Agent {
            code: code::INTERNAL_ERROR,
            message: format!("cannot encode the request: {e}"),
        })?;
        let (sender, receiver) = mpsc::channel();
        {
            let mut state = lock(&shared.state);
            if state.stopping || state.status != AgentStatus::Running {
                return Err(RequestError::NotRunning);
            }
            state.pending.insert(id, sender);
        }
        match shared.write_line(&line, deadline) {
            Ok(true) => {}
            Ok(false) => {
                lock(&shared.state).pending.remove(&id);
                return Err(RequestError::Timeout);
            }
            Err(e) => {
                eprintln!("refract-client: cannot write to the helper: {e}");
                lock(&shared.state).pending.remove(&id);
                return Err(RequestError::Disconnected);
            }
        }
        let left = deadline.saturating_duration_since(Instant::now());
        match receiver.recv_timeout(left) {
            Ok(Outcome::Result(reply)) => Ok(reply),
            Ok(Outcome::Error(e)) => Err(RequestError::Agent {
                code: e.code,
                message: e.message,
            }),
            Err(RecvTimeoutError::Timeout) => {
                // A response that comes later finds no entry and is dropped.
                lock(&shared.state).pending.remove(&id);
                Err(RequestError::Timeout)
            }
            Err(RecvTimeoutError::Disconnected) => Err(RequestError::Disconnected),
        }
    }

    /// The helper's current status.
    pub fn status(&self) -> AgentStatus {
        lock(&self.shared.state).status.clone()
    }

    /// Sends `shutdown`, closes stdin, waits up to `grace` for exit, then kills. No
    /// restart afterwards. Idempotent.
    ///
    /// Never blocks on the helper's stdin: the request is written on a thread of its
    /// own, and is skipped when a stuck request holds stdin. Killing the helper ends
    /// any write still under way.
    pub fn shutdown(&self, grace: Duration) {
        let shared = &*self.shared;
        let first = {
            let mut state = lock(&shared.state);
            let first = !state.stopping;
            state.stopping = true;
            shared.wake.notify_all();
            first
        };
        if first {
            // Taking stdin closes it once the request is written, so a helper that
            // misses the request still sees the end of its input.
            let stdin = match shared.stdin.try_lock() {
                Ok(mut stdin) => stdin.take(),
                Err(TryLockError::Poisoned(e)) => e.into_inner().take(),
                Err(TryLockError::WouldBlock) => None,
            };
            if let Some(stdin) = stdin {
                let id = shared.next_id.fetch_add(1, Ordering::Relaxed);
                let request = Request {
                    id: id.into(),
                    method: Method::Shutdown,
                };
                if let Ok(line) = serde_json::to_string(&request) {
                    // If the thread cannot start, `stdin` is dropped and so closed.
                    let _ = thread::Builder::new()
                        .name("refract-client-shutdown".into())
                        .spawn(move || {
                            let mut stdin = stdin;
                            let _ = writeln!(stdin, "{line}").and_then(|()| stdin.flush());
                        });
                }
            }
        }
        let state = lock(&shared.state);
        let (mut state, _) = shared
            .wake
            .wait_timeout_while(state, grace, |s| !s.finished)
            .unwrap_or_else(PoisonError::into_inner);
        if !state.finished {
            if let Some(child) = state.child.as_mut() {
                let _ = child.kill();
            }
            let _ = shared
                .wake
                .wait_timeout_while(state, KILL_WAIT, |s| !s.finished)
                .unwrap_or_else(PoisonError::into_inner);
        } else {
            drop(state);
        }
        shared.set_status(AgentStatus::Stopped);
    }
}

impl Shared {
    pub(crate) fn stopping(&self) -> bool {
        lock(&self.state).stopping
    }

    /// Records a new status and tells the app. `Stopped` is final, and only `Stopped`
    /// is recorded once shutdown has begun.
    pub(crate) fn set_status(&self, status: AgentStatus) {
        let _emit = lock(&self.emit);
        {
            let mut state = lock(&self.state);
            let stopped = state.status == AgentStatus::Stopped;
            if stopped || (state.stopping && status != AgentStatus::Stopped) {
                return;
            }
            state.status = status.clone();
        }
        (self.on_event)(ClientEvent::Status(status));
    }

    /// Passes an event on to the app.
    pub(crate) fn emit(&self, event: ClientEvent) {
        let _emit = lock(&self.emit);
        (self.on_event)(event);
    }

    /// Waits up to `delay`, returning early with true if shutdown begins.
    pub(crate) fn sleep(&self, delay: Duration) -> bool {
        let state = lock(&self.state);
        let (state, _) = self
            .wake
            .wait_timeout_while(state, delay, |s| !s.stopping)
            .unwrap_or_else(PoisonError::into_inner);
        state.stopping
    }

    /// Marks the supervisor as done and reports `Stopped`.
    pub(crate) fn finish(&self) {
        {
            let mut state = lock(&self.state);
            state.finished = true;
            self.wake.notify_all();
        }
        self.set_status(AgentStatus::Stopped);
    }

    /// Writes one line to the helper. False if another write held stdin until
    /// `deadline`.
    fn write_line(&self, line: &str, deadline: Instant) -> io::Result<bool> {
        let mut stdin = loop {
            match self.stdin.try_lock() {
                Ok(stdin) => break stdin,
                Err(TryLockError::Poisoned(e)) => break e.into_inner(),
                Err(TryLockError::WouldBlock) => {}
            }
            let left = deadline.saturating_duration_since(Instant::now());
            if left.is_zero() {
                return Ok(false);
            }
            thread::sleep(left.min(STDIN_POLL));
        };
        let stdin = stdin
            .as_mut()
            .ok_or_else(|| io::Error::new(io::ErrorKind::BrokenPipe, "the helper has exited"))?;
        writeln!(stdin, "{line}")?;
        stdin.flush()?;
        Ok(true)
    }
}
