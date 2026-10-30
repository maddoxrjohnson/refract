//! The stdio loop: requests in, responses and events out.
//!
//! Three threads: one reads stdin, one reads inotify, and this one owns all state and is
//! the only writer to stdout. It blocks on a channel, waking only for input, for a
//! debounce deadline, for the liveness timer while some session is live or recent, or
//! to try again a repo git could not read (a few times, and once a change in it has
//! settled; see [`RETRY_DELAYS`] and [`RETRY_QUIET`]).
//! A fourth thread runs the first load, so requests are answered while it runs, a
//! fifth runs `setupServer` requests one at a time, sending each response back through
//! the channel for this thread to write, and a sixth runs `cloneRepo` clones one at a
//! time, sending each result back for this thread to report and answer. A seventh keeps
//! token usage totals up to date (see [`crate::usage`]).
//!
//! Backpressure: the channel is bounded and writes to stdout block. If the app stops
//! reading stdout, this thread stops; the stdin and inotify threads then block on the
//! full channel, the app's writes to stdin block, and the kernel's inotify queue fills
//! and overflows. Nothing grows without bound, and once stdout drains the overflow
//! causes a full reload, so no change is lost.

use std::collections::{HashMap, HashSet};
use std::io::{self, BufRead, Write};
use std::panic::{catch_unwind, AssertUnwindSafe};
use std::path::{Path, PathBuf};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, Sender, SyncSender};
use std::time::{Duration, Instant};

use chrono::Utc;
use refract_core::launch::wsl_path;
use refract_core::protocol::{
    code, parse_request, ErrorBody, Event, Hello, HookInfo, Id, LaunchMode, Method, Outcome, Reply,
    RepoList, RepoSnapshot, Response, PROTOCOL_VERSION,
};
use refract_core::sessions::{is_windows_mount, paths_match, project_dir_matches};
use serde::Serialize;

use crate::config::{self, Config};
use crate::hook;
use crate::launch;
use crate::limits::LimitsFile;
use crate::procs;
use crate::repos;
use crate::scan;
use crate::setup;
use crate::state::{self, path_str, Agent, GitResult, Refresh};
use crate::usage::Usage;
use crate::watch::{self, Change, FsEvent, Watcher};

/// Quiet time after the last file-system event before refreshing.
pub const DEBOUNCE: Duration = Duration::from_millis(300);

/// Longest a refresh waits while events keep arriving.
pub const MAX_DELAY: Duration = Duration::from_millis(1500);

/// How often running processes are checked while a session is live or recent.
pub const LIVE_INTERVAL: Duration = Duration::from_secs(5);

/// How long after the search first found a repo git could not read, and after each timed
/// retry of it that failed, it is tried again with nothing prompting it: three times,
/// then no more. A change in the repo prompts a try of its own (see [`RETRY_QUIET`]).
pub const RETRY_DELAYS: [Duration; 3] = [
    Duration::from_secs(2),
    Duration::from_secs(10),
    Duration::from_secs(60),
];

/// Quiet time after the last change in a repo git could not read before the change
/// prompts a try, so a copy is tried once it is done rather than while it runs.
pub const RETRY_QUIET: Duration = Duration::from_secs(2);

/// Longest a change in such a repo waits for quiet while changes keep coming. Once
/// [`BUSY_FAILURES`] tries prompted by changes have failed in a row, also the least time
/// between such tries, so a busy repo git can never read (owned by another user, from a
/// newer git) is tried about once a minute while written, and not at all while idle.
pub const RETRY_MAX_WAIT: Duration = Duration::from_secs(60);

/// Failed tries prompted by changes, in a row, after which they are spaced by
/// [`RETRY_MAX_WAIT`].
const BUSY_FAILURES: usize = 3;

/// Longest request line accepted; longer ones are discarded and answered with an error.
pub const MAX_LINE: usize = 1024 * 1024;

/// Messages waiting for the main thread before the threads feeding it block.
const CHANNEL_CAPACITY: usize = 64;

/// Repos under `/mnt/` the first load reads at once. Each read waits on the Windows file
/// system, which serves several at once well but slows them all if asked too many.
pub const SLOW_READERS: usize = 4;

enum Msg {
    Line(Line),
    Eof,
    Fs(Vec<FsEvent>),
    /// The loading thread found the repos under `/mnt/` and searched `dirs`; it reads
    /// them once `go` is answered, after they are watched.
    Found {
        repos: Vec<PathBuf>,
        dirs: Vec<(PathBuf, bool)>,
        go: Sender<()>,
    },
    /// The loading thread read some repos; `last` when it has read them all.
    Loaded {
        repos: Vec<(PathBuf, GitResult)>,
        last: bool,
    },
    /// A response the setup thread finished.
    Answer(Response),
    /// A clone the clone thread finished: the repo's path, or why it failed.
    Cloned {
        id: Id,
        result: Result<PathBuf, String>,
    },
}

/// One line of input, without its newline.
#[derive(Debug, PartialEq, Eq)]
pub enum Line {
    Text(String),
    /// Not UTF-8; decoded lossily so the id can still be found.
    BadUtf8(String),
    /// Longer than [`MAX_LINE`]; its content was discarded.
    TooLong,
}

/// Where the first load is.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Load {
    NotStarted,
    Running,
    Done,
}

#[derive(Default)]
struct Pending {
    repos: HashMap<PathBuf, Refresh>,
    project_dirs: HashSet<String>,
    new_project_dirs: HashSet<String>,
    rewatch: HashSet<PathBuf>,
    /// Status hook files written or removed.
    hook_files: HashSet<String>,
    /// Repos git could not read that changed.
    retry: HashSet<PathBuf>,
    rescan: bool,
    reload: bool,
    first: Option<Instant>,
    last: Option<Instant>,
}

impl Pending {
    fn add(&mut self, change: Change, now: Instant) {
        match change {
            Change::Repo(path, refresh) => {
                let entry = self.repos.entry(path).or_default();
                *entry = entry.merge(refresh);
            }
            Change::Sessions(dir) => {
                self.project_dirs.insert(dir);
            }
            Change::NewProjectDir(dir) => {
                self.new_project_dirs.insert(dir);
            }
            Change::Rewatch(path) => {
                self.rewatch.insert(path);
            }
            Change::Hook(name) => {
                self.hook_files.insert(name);
            }
            Change::Retry(path) => {
                self.retry.insert(path);
            }
            Change::Rescan => self.rescan = true,
            Change::Overflow => self.reload = true,
        }
        self.touch(now);
    }

    fn touch(&mut self, now: Instant) {
        self.first.get_or_insert(now);
        self.last = Some(now);
    }

    fn deadline(&self) -> Option<Instant> {
        Some((self.last? + DEBOUNCE).min(self.first? + MAX_DELAY))
    }
}

/// A repo the search found but git could not read, and when to try it again. Timed
/// retries ([`RETRY_DELAYS`]) and tries prompted by changes ([`RETRY_QUIET`],
/// [`RETRY_MAX_WAIT`]) are counted apart, and a search made for any other reason counts
/// for neither, so none of them puts off a try a change asks for.
#[derive(Debug, Default)]
struct Unread {
    /// Timed retries that failed in a row, the first sighting included.
    failures: usize,
    /// The next timed retry, while any are left.
    retry_at: Option<Instant>,
    /// The first and the last change not tried for yet.
    changes: Option<(Instant, Instant)>,
    /// Tries prompted by changes that failed in a row, and when the last one began.
    change_failures: usize,
    change_tried: Option<Instant>,
    /// What the coming search tries the repo for: its timer, its changes.
    trying: (bool, bool),
}

impl Unread {
    /// A repo a search found and could not read.
    fn new(now: Instant) -> Unread {
        Unread {
            failures: 1,
            retry_at: Some(now + RETRY_DELAYS[0]),
            ..Unread::default()
        }
    }

    /// Takes in a change in the repo.
    fn changed(&mut self, now: Instant) {
        let first = self.changes.map_or(now, |(first, _)| first);
        self.changes = Some((first, now));
    }

    /// When the changes not tried for yet prompt a try.
    fn change_at(&self) -> Option<Instant> {
        let (first, last) = self.changes?;
        let at = (last + RETRY_QUIET).min(first + RETRY_MAX_WAIT);
        match self.change_tried {
            Some(tried) if self.change_failures >= BUSY_FAILURES => {
                Some(at.max(tried + RETRY_MAX_WAIT))
            }
            _ => Some(at),
        }
    }

    /// When the repo is next tried, if ever.
    fn next(&self) -> Option<Instant> {
        [self.retry_at, self.change_at()]
            .into_iter()
            .flatten()
            .min()
    }

    /// Notes the tries due by `now`, for the search about to run. True if any are.
    fn start(&mut self, now: Instant) -> bool {
        if self.retry_at.is_some_and(|at| at <= now) {
            self.retry_at = None;
            self.trying.0 = true;
        }
        if self.change_at().is_some_and(|at| at <= now) {
            self.changes = None;
            self.change_tried = Some(now);
            self.trying.1 = true;
        }
        self.trying.0 || self.trying.1
    }

    /// Takes in a search that could not read the repo. Only the tries it was started for
    /// count.
    fn failed(&mut self, now: Instant) {
        let (timed, changed) = std::mem::take(&mut self.trying);
        if timed {
            self.failures += 1;
            self.retry_at = RETRY_DELAYS.get(self.failures - 1).map(|d| now + *d);
        }
        if changed {
            self.change_failures += 1;
        }
    }
}

struct Server<W: Write> {
    out: W,
    agent: Agent,
    watcher: Option<Watcher>,
    pending: Pending,
    next_live: Option<Instant>,
    load: Load,
    /// For the loading and setup threads.
    tx: SyncSender<Msg>,
    /// Queues `setupServer` jobs for the setup thread, once it is started.
    setup_jobs: Option<Sender<setup::Job>>,
    /// Queues `cloneRepo` jobs for the clone thread, once it is started.
    clone_jobs: Option<Sender<repos::Job>>,
    /// Refreshes asked for since the load started, re-applied to each repo it reads.
    touched: HashMap<PathBuf, Refresh>,
    /// `getRepo` requests for repos not loaded yet, answered when they are.
    waiting: Vec<(Id, String)>,
    /// Repos watched before the load, so watches of ones that failed to load can go.
    prewatched: Vec<PathBuf>,
    /// Directories the first load has searched so far, all watched.
    load_dirs: Vec<(PathBuf, bool)>,
    /// A rescan or reload asked for during the load, run once it is done.
    deferred_rescan: bool,
    deferred_reload: bool,
    /// Repos the search found but git could not read, by path.
    unread: HashMap<PathBuf, Unread>,
    /// Token usage, kept up to date by its own thread.
    usage: Usage,
    /// Plan limits from the status line's file, added to each `usage` answer.
    limits: LimitsFile,
}

/// Runs the helper until `shutdown` or the end of `input`.
pub fn run<R, W>(config: Config, mut input: R, out: W) -> io::Result<()>
where
    R: BufRead + Send + 'static,
    W: Write,
{
    let (tx, rx) = mpsc::sync_channel(CHANNEL_CAPACITY);
    let stdin_tx = tx.clone();
    std::thread::Builder::new()
        .name("stdin".into())
        .spawn(move || {
            let mut buf = Vec::new();
            while let Ok(Some(line)) = read_line(&mut input, &mut buf) {
                if stdin_tx.send(Msg::Line(line)).is_err() {
                    return;
                }
            }
            let _ = stdin_tx.send(Msg::Eof);
        })?;
    let fs_tx = tx.clone();
    let watcher = match watch::start(move |batch| fs_tx.send(Msg::Fs(batch)).is_ok()) {
        Ok(w) => Some(w),
        Err(e) => {
            eprintln!("refract-agent: inotify unavailable, changes will not be pushed: {e}");
            None
        }
    };
    let mut server = Server::new(config, out, watcher, tx);

    loop {
        let deadline = [
            server.pending.deadline(),
            server.next_live,
            server.next_retry(),
        ]
        .into_iter()
        .flatten()
        .min();
        let msg = match deadline {
            Some(at) => rx.recv_timeout(at.saturating_duration_since(Instant::now())),
            None => rx.recv().map_err(|_| RecvTimeoutError::Disconnected),
        };
        match msg {
            Ok(Msg::Line(line)) => {
                if !server.handle_line(line)? {
                    return Ok(());
                }
            }
            Ok(Msg::Eof) | Err(RecvTimeoutError::Disconnected) => return Ok(()),
            Ok(Msg::Fs(batch)) => server.queue(&batch),
            Ok(Msg::Found { repos, dirs, go }) => server.found(repos, dirs, go),
            Ok(Msg::Loaded { repos, last }) => server.loaded(repos, last)?,
            Ok(Msg::Answer(response)) => server.send(&response)?,
            Ok(Msg::Cloned { id, result }) => server.cloned(id, result)?,
            Err(RecvTimeoutError::Timeout) => {}
        }
        let now = Instant::now();
        if server.next_retry().is_some_and(|d| d <= now) {
            server.retry_due(now);
        }
        if server.pending.deadline().is_some_and(|d| d <= now) {
            server.flush()?;
        }
        if server.next_live.is_some_and(|d| d <= now) {
            let events = server.agent.recheck_live(Utc::now());
            server.send_all(&events)?;
            server.next_live = None;
        }
        server.schedule_live();
    }
}

/// Reads one line of at most [`MAX_LINE`] bytes. Returns `None` at the end of input. A
/// longer line is consumed up to its newline and reported as [`Line::TooLong`].
pub fn read_line<R: BufRead>(input: &mut R, buf: &mut Vec<u8>) -> io::Result<Option<Line>> {
    buf.clear();
    let mut too_long = false;
    let mut read_any = false;
    loop {
        let chunk = match input.fill_buf() {
            Ok(chunk) => chunk,
            Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
            Err(e) => return Err(e),
        };
        if chunk.is_empty() {
            if !read_any {
                return Ok(None);
            }
            break;
        }
        read_any = true;
        let (part, used, done) = match chunk.iter().position(|&b| b == b'\n') {
            Some(i) => (&chunk[..i], i + 1, true),
            None => (chunk, chunk.len(), false),
        };
        if !too_long {
            if buf.len() + part.len() > MAX_LINE {
                too_long = true;
                buf.clear();
            } else {
                buf.extend_from_slice(part);
            }
        }
        input.consume(used);
        if done {
            break;
        }
    }
    if too_long {
        return Ok(Some(Line::TooLong));
    }
    if buf.last() == Some(&b'\r') {
        buf.pop();
    }
    Ok(Some(match std::str::from_utf8(buf) {
        Ok(text) => Line::Text(text.to_owned()),
        Err(_) => Line::BadUtf8(String::from_utf8_lossy(buf).into_owned()),
    }))
}

/// True for repos the first load reads after the others, one report each: those under
/// `/mnt/`, where git is much slower.
fn is_slow(repo: &Path) -> bool {
    is_windows_mount(&path_str(repo)) || state::under_test_slow_prefix(repo)
}

/// Splits `config` into one searching only the roots and extra repos under `/mnt/`,
/// and one searching the rest.
fn split_slow(config: &Config) -> (Config, Config) {
    let mut slow = config.clone();
    let mut fast = config.clone();
    (slow.roots, fast.roots) = config.roots.iter().cloned().partition(|p| is_slow(p));
    (slow.extra, fast.extra) = config.extra.iter().cloned().partition(|p| is_slow(p));
    (slow, fast)
}

/// Watches a repo's root and git directory, before git reads it.
fn prewatch(watcher: &mut Watcher, repo: &Path) {
    let git_dir = scan::git_dir(repo);
    let common = git_dir.as_deref().and_then(scan::common_dir);
    // The root only for now; the tracked directories once git has listed them.
    watcher.watch_repo(
        repo,
        git_dir.as_deref(),
        common.as_deref(),
        &[String::new()],
    );
}

/// Ends the first load when dropped, also when the loading thread panics, so the main
/// thread always sends `loadComplete` and answers the `getRepo` requests it holds.
struct LoadDone(SyncSender<Msg>);

impl Drop for LoadDone {
    fn drop(&mut self) {
        let _ = self.0.send(Msg::Loaded {
            repos: Vec::new(),
            last: true,
        });
    }
}

fn error_response(id: Option<Id>, code: i32, message: impl Into<String>) -> Response {
    Response {
        id,
        outcome: Outcome::Error(ErrorBody {
            code,
            message: message.into(),
        }),
    }
}

/// Answers a status hook request.
fn hook_response(id: Id, result: Result<HookInfo, String>) -> Response {
    match result {
        Ok(info) => Response::ok(id, Reply::Hook(info)),
        Err(message) => Response::err(id, code::HOOK_FAILED, message),
    }
}

impl<W: Write> Server<W> {
    fn new(config: Config, out: W, watcher: Option<Watcher>, tx: SyncSender<Msg>) -> Server<W> {
        let usage = Usage::start(config.claude_projects.clone());
        let limits = LimitsFile::new(config.limits_file.clone());
        Server {
            out,
            agent: Agent::new(config),
            watcher,
            pending: Pending::default(),
            next_live: None,
            load: Load::NotStarted,
            tx,
            setup_jobs: None,
            clone_jobs: None,
            touched: HashMap::new(),
            waiting: Vec::new(),
            prewatched: Vec::new(),
            load_dirs: Vec::new(),
            deferred_rescan: false,
            deferred_reload: false,
            unread: HashMap::new(),
            usage,
            limits,
        }
    }

    /// Handles one request line. Returns false when the helper should exit.
    fn handle_line(&mut self, line: Line) -> io::Result<bool> {
        let (text, utf8) = match line {
            Line::Text(text) => (text, true),
            Line::BadUtf8(text) => (text, false),
            Line::TooLong => {
                let message = format!("line longer than {MAX_LINE} bytes");
                self.send(&error_response(None, code::INVALID_REQUEST, message))?;
                return Ok(true);
            }
        };
        if text.trim().is_empty() {
            return Ok(true);
        }
        let parsed = parse_request(&text);
        if !utf8 {
            let id = match parsed {
                Ok(request) => Some(request.id),
                Err(e) => e.id,
            };
            self.send(&error_response(id, code::PARSE_ERROR, "invalid UTF-8"))?;
            return Ok(true);
        }
        let request = match parsed {
            Ok(request) => request,
            Err(e) => {
                self.send(&e.response())?;
                return Ok(true);
            }
        };
        let id = request.id;
        let response = match request.method {
            Method::Ping => Response::ok(
                id,
                Reply::Hello(Hello {
                    protocol_version: PROTOCOL_VERSION,
                    agent_version: env!("CARGO_PKG_VERSION").to_owned(),
                }),
            ),
            Method::ListRepos => {
                self.ensure_loaded()?;
                // The list carries every preselect, so no events are needed.
                self.agent.refresh_preselects(Utc::now());
                Response::ok(
                    id,
                    Reply::Repos(RepoList {
                        repos: self.agent.snapshots(),
                        loading: self.load == Load::Running,
                    }),
                )
            }
            Method::GetRepo { path } => {
                self.ensure_loaded()?;
                self.refresh_preselects()?;
                match self.get_repo(id, path) {
                    Some(response) => response,
                    None => return Ok(true),
                }
            }
            Method::PrepareLaunch { path, mode } => {
                let distro = std::env::var("WSL_DISTRO_NAME").ok();
                let dir = launch::launch_dir();
                let running = match &mode {
                    LaunchMode::Focus { session_id } => {
                        let repo = Path::new(path.trim_end_matches('/'));
                        self.agent.live_process(repo, session_id)
                    }
                    _ => None,
                };
                match launch::prepare(&path, &mode, running, &dir, distro.as_deref(), Utc::now()) {
                    Ok(ticket) => Response::ok(id, Reply::Launch(ticket)),
                    Err(e) => Response::err(id, e.code, e.message),
                }
            }
            Method::SetupServer { vsix, commit } => match wsl_path(&vsix) {
                Some(path) => {
                    let job = setup::Job {
                        id,
                        vsix: path.into(),
                        commit,
                    };
                    self.start_setup(job)?;
                    return Ok(true);
                }
                None => Response::err(
                    id,
                    code::INVALID_PARAMS,
                    format!("vsix is not an absolute Linux, Windows or WSL path: {vsix:?}"),
                ),
            },
            Method::CloneRepo { url, name } => {
                self.start_clone(repos::Job { id, url, name })?;
                return Ok(true);
            }
            Method::AddRepo { path } => match wsl_path(&path) {
                Some(linux) => match repos::work_tree_root(Path::new(&linux)) {
                    Ok(repo) => match self.add_repo(&repo)? {
                        Ok(()) => Response::ok(id, Reply::Done),
                        Err(message) => Response::err(id, code::INTERNAL_ERROR, message),
                    },
                    Err(message) => Response::err(id, code::REPO_NOT_FOUND, message),
                },
                None => Response::err(
                    id,
                    code::INVALID_PARAMS,
                    format!("path is not an absolute Linux, Windows or WSL path: {path:?}"),
                ),
            },
            Method::Usage => {
                let repos: Vec<String> = self.agent.repo_paths().map(|p| path_str(p)).collect();
                let live = procs::scan().len() as u32;
                let mut summary = self.usage.summary(&repos, live);
                summary.limits = self.limits.read().map(Box::new);
                Response::ok(id, Reply::Usage(summary))
            }
            Method::HookInfo => hook_response(id, hook::info(&hook::Paths::from_env())),
            Method::InstallHook | Method::UninstallHook => {
                let paths = hook::Paths::from_env();
                let result = if matches!(request.method, Method::InstallHook) {
                    hook::install(&paths)
                } else {
                    hook::uninstall(&paths)
                };
                self.watch_hook_dir();
                // Statuses count only while the hook is installed.
                let events = self.agent.set_hooks(hook::statuses(&paths));
                self.send(&hook_response(id, result))?;
                self.send_all(&events)?;
                return Ok(true);
            }
            Method::Migrate(_) | Method::MigrateFinish(_) | Method::MigrateCancel(_) => {
                Response::err(
                    id,
                    code::METHOD_NOT_FOUND,
                    "migration is not implemented yet",
                )
            }
            Method::Shutdown => {
                self.send(&Response::ok(id, Reply::Done))?;
                return Ok(false);
            }
        };
        self.send(&response)?;
        Ok(true)
    }

    /// Queues `job` for the setup thread, starting it on first use. Jobs run one at a
    /// time, so two installs never race, and each response arrives as [`Msg::Answer`].
    /// If the thread cannot start, `job` is answered with `SETUP_FAILED` at once.
    fn start_setup(&mut self, job: setup::Job) -> io::Result<()> {
        let job = match &self.setup_jobs {
            Some(jobs) => match jobs.send(job) {
                Ok(()) => return Ok(()),
                // The thread is gone (it panicked); start another.
                Err(mpsc::SendError(job)) => job,
            },
            None => job,
        };
        let (jobs, rx) = mpsc::channel::<setup::Job>();
        let tx = self.tx.clone();
        let started = std::thread::Builder::new()
            .name("setup".into())
            .spawn(move || run_setup_jobs(rx, &tx, setup::run));
        if let Err(e) = started {
            let message = format!("cannot start the setup thread: {e}");
            return self.send(&Response::err(job.id, code::SETUP_FAILED, message));
        }
        // The thread holds the receiver until it has run a job, so this cannot fail.
        let _ = jobs.send(job);
        self.setup_jobs = Some(jobs);
        Ok(())
    }

    /// Queues `job` for the clone thread, starting it on first use. Clones run one at a
    /// time, so two clones never race for one folder, and each result arrives as
    /// [`Msg::Cloned`]. If the thread cannot start, `job` is answered with
    /// `CLONE_FAILED` at once.
    fn start_clone(&mut self, job: repos::Job) -> io::Result<()> {
        let job = match &self.clone_jobs {
            Some(jobs) => match jobs.send(job) {
                Ok(()) => return Ok(()),
                // The thread is gone (it panicked); start another.
                Err(mpsc::SendError(job)) => job,
            },
            None => job,
        };
        let (jobs, rx) = mpsc::channel::<repos::Job>();
        let tx = self.tx.clone();
        let started = std::thread::Builder::new()
            .name("clone".into())
            .spawn(move || run_clone_jobs(rx, &tx, repos::run));
        if let Err(e) = started {
            let message = format!("cannot start the clone thread: {e}");
            return self.send(&Response::err(job.id, code::CLONE_FAILED, message));
        }
        // The thread holds the receiver until it has run a job, so this cannot fail.
        let _ = jobs.send(job);
        self.clone_jobs = Some(jobs);
        Ok(())
    }

    /// Reports a finished clone, then answers its request with the path.
    fn cloned(&mut self, id: Id, result: Result<PathBuf, String>) -> io::Result<()> {
        let path = match result {
            Ok(path) => path,
            Err(message) => return self.send(&Response::err(id, code::CLONE_FAILED, message)),
        };
        if let Err(message) = self.add_repo(&path)? {
            // The clone is there either way; outside the roots it is just not kept.
            eprintln!("refract-agent: {message}");
        }
        self.send(&Response::ok(id, Reply::Path(path_str(&path))))
    }

    /// Reports `repo` from now on. Unless a root covers it, it is added to `extra`, in
    /// memory and in the config file; if `hidden` names it, it is removed from there,
    /// since the user asked for it. Then the repos are searched again, so a new one
    /// is reported now, or once the first load is done; one already reported is sent
    /// again as `repoChanged`. The inner error, from the config file, is a message for
    /// the app.
    fn add_repo(&mut self, repo: &Path) -> io::Result<Result<(), String>> {
        let config = self.agent.config();
        if !config.extra.iter().any(|p| p == repo) && !scan::under_roots(config, repo) {
            if let Err(message) = config::add_extra(&config::file_path(), repo) {
                return Ok(Err(message));
            }
            self.agent.add_extra(repo.to_owned());
        }
        if self.agent.config().hidden.iter().any(|p| p == repo) {
            if let Err(message) = config::remove_hidden(&config::file_path(), repo) {
                return Ok(Err(message));
            }
            self.agent.unhide(repo);
        }
        // Before the first load starts, it will find the repo itself.
        if self.load == Load::NotStarted {
            return Ok(Ok(()));
        }
        let known = self.agent.repo(repo).is_some();
        self.pending.rescan = true;
        self.pending.touch(Instant::now());
        self.flush()?;
        if known {
            if let Some(state) = self.agent.repo(repo) {
                let event = Event::RepoChanged(Box::new(state.snapshot.clone()));
                self.send(&event)?;
            }
        }
        Ok(Ok(()))
    }

    /// Answers `getRepo`, or returns `None` and holds the request when the repo may
    /// still be loading.
    fn get_repo(&mut self, id: Id, path: String) -> Option<Response> {
        let found = self
            .agent
            .repo_paths()
            .find(|p| paths_match(&path_str(p), &path))
            .cloned();
        match found.and_then(|p| self.agent.repo(&p)) {
            Some(repo) => Some(Response::ok(
                id,
                Reply::Repo(Box::new(repo.snapshot.clone())),
            )),
            None if self.load == Load::Running => {
                self.waiting.push((id, path));
                None
            }
            None => Some(Response::err(
                id,
                code::REPO_NOT_FOUND,
                format!("no repo at {path}"),
            )),
        }
    }

    /// Recomputes preselects for the current time, which can change without any file
    /// changing, and reports the repos whose choice moved.
    fn refresh_preselects(&mut self) -> io::Result<()> {
        let changed = self.agent.refresh_preselects(Utc::now());
        for path in changed {
            if let Some(repo) = self.agent.repo(&path) {
                let event = Event::RepoChanged(Box::new(repo.snapshot.clone()));
                self.send(&event)?;
            }
        }
        Ok(())
    }

    /// Starts the first load in the background. Every directory it reads is watched
    /// first, so changes made while it runs are not lost. Only the search outside
    /// `/mnt/` and its watches happen here, before `listRepos` is answered: the loading
    /// thread searches `/mnt/` while git reads the Linux repos, and the repos there are
    /// watched (see [`Server::found`]) once the Linux repos are reported, since both
    /// take long on Windows drives.
    fn ensure_loaded(&mut self) -> io::Result<()> {
        if self.load != Load::NotStarted {
            return Ok(());
        }
        self.load = Load::Running;
        let (slow_config, fast_config) = split_slow(self.agent.config());
        let walk = scan::walk_roots(&fast_config);
        // A Linux root can still reach a Windows drive (a root of `/`, or `/mnt` without
        // the slash, or a test prefix); such repos are prewatched in `found()` like the rest.
        let (slow_found, fast): (Vec<PathBuf>, Vec<PathBuf>) =
            walk.repos.into_iter().partition(|p| is_slow(p));
        if let Some(watcher) = &mut self.watcher {
            watcher.watch_projects_root(&fast_config.claude_projects);
            watcher.watch_scan_dirs(&walk.dirs, &[]);
            for repo in &fast {
                prewatch(watcher, repo);
            }
        }
        self.load_dirs = walk.dirs;
        self.prewatched = fast.clone();
        self.watch_hook_dir();
        self.agent
            .set_hooks(hook::statuses(&hook::Paths::from_env()));
        let tx = self.tx.clone();
        std::thread::Builder::new()
            .name("load".into())
            .spawn(move || {
                let done = LoadDone(tx);
                let tx = &done.0;
                std::thread::scope(|scope| {
                    // `/mnt/` is searched while git reads the Linux repos.
                    let search = std::thread::Builder::new()
                        .name("search".into())
                        .spawn_scoped(scope, || scan::walk_roots(&slow_config))
                        .ok();
                    // Linux repos in one report, so their masses settle at once.
                    if !fast.is_empty() {
                        let read = state::read_git_parallel(&fast);
                        let msg = Msg::Loaded {
                            repos: fast.into_iter().zip(read).collect(),
                            last: false,
                        };
                        if tx.send(msg).is_err() {
                            return;
                        }
                    }
                    let walk = match search {
                        Some(search) => match search.join() {
                            Ok(walk) => walk,
                            Err(_) => return,
                        },
                        None => scan::walk_roots(&slow_config),
                    };
                    let mut slow = slow_found;
                    slow.extend(walk.repos);
                    slow.sort();
                    slow.dedup();
                    if slow.is_empty() && walk.dirs.is_empty() {
                        return;
                    }
                    // Read only once the main thread has watched them.
                    let (go, ready) = mpsc::channel();
                    let msg = Msg::Found {
                        repos: slow.clone(),
                        dirs: walk.dirs,
                        go,
                    };
                    if tx.send(msg).is_err() || ready.recv().is_err() {
                        return;
                    }
                    // Each slow repo as soon as it is read.
                    state::read_git_each(&slow, SLOW_READERS, |i, result| {
                        let msg = Msg::Loaded {
                            repos: vec![(slow[i].clone(), result)],
                            last: false,
                        };
                        tx.send(msg).is_ok()
                    });
                });
            })?;
        Ok(())
    }

    /// Watches what the loading thread found under `/mnt/`, as [`Server::ensure_loaded`]
    /// does for the rest, then lets it read the repos.
    fn found(&mut self, repos: Vec<PathBuf>, dirs: Vec<(PathBuf, bool)>, go: Sender<()>) {
        if let Some(watcher) = &mut self.watcher {
            for repo in &repos {
                prewatch(watcher, repo);
            }
        }
        self.prewatched.extend(repos);
        // Fails only when the thread panicked, and then its guard ends the load.
        let _ = go.send(());
        // The directories searched are watched while git reads: a repo appearing in
        // one before then is found by the search at the end of the load.
        self.load_dirs.extend(dirs);
        if let Some(watcher) = &mut self.watcher {
            watcher.watch_scan_dirs(&self.load_dirs, &[]);
        }
    }

    /// Takes in repos the load read: reports them and the repos whose snapshots they
    /// change (masses move), re-applies refreshes asked for while they were read, and
    /// watches them fully. Once `last`, the load is done.
    fn loaded(&mut self, repos: Vec<(PathBuf, GitResult)>, last: bool) -> io::Result<()> {
        let before: HashMap<PathBuf, RepoSnapshot> = self
            .agent
            .repo_paths()
            .filter_map(|p| Some((p.clone(), self.agent.repo(p)?.snapshot.clone())))
            .collect();
        let inserted = self.agent.insert_loaded(repos);
        for path in &inserted {
            self.watch_repo(path);
        }
        // Only the root and git directory were watched while git ran, so an edit deeper
        // in the tree could have been missed: read status again now that the tree is
        // watched. Skipped for slow repos, where it would take as long again.
        for path in &inserted {
            let mut what = self.touched.remove(path).unwrap_or_default();
            what.status |= !is_slow(path);
            if what != Refresh::default() {
                // What changed is reported below, against what the app saw before.
                self.agent.refresh(path, what);
                if what.head {
                    // A checkout while git ran can have added tracked directories.
                    self.watch_repo(path);
                }
            }
        }
        let mut events = Vec::new();
        for path in self.agent.repo_paths() {
            let Some(new) = self.agent.repo(path).map(|r| &r.snapshot) else {
                continue;
            };
            match before.get(path) {
                Some(old) => events.extend(state::diff(old, new)),
                None => events.push(Event::RepoChanged(Box::new(new.clone()))),
            }
        }
        if last {
            self.load = Load::Done;
            self.touched.clear();
            self.load_dirs.clear();
            let unloaded: Vec<PathBuf> = std::mem::take(&mut self.prewatched)
                .into_iter()
                .filter(|p| self.agent.repo(p).is_none())
                .collect();
            self.forget_repos(&unloaded);
            self.watch_search();
            if let Some(watcher) = &self.watcher {
                eprintln!(
                    "refract-agent: {} repos, {} watches",
                    self.agent.repo_paths().count(),
                    watcher.len()
                );
            }
            if self.deferred_rescan || self.deferred_reload {
                self.pending.rescan |= std::mem::take(&mut self.deferred_rescan);
                self.pending.reload |= std::mem::take(&mut self.deferred_reload);
                self.pending.touch(Instant::now());
            }
        }
        self.send_all(&events)?;
        for (id, path) in std::mem::take(&mut self.waiting) {
            if let Some(response) = self.get_repo(id, path) {
                self.send(&response)?;
            }
        }
        if last {
            self.send(&Event::LoadComplete)?;
        }
        self.schedule_live();
        Ok(())
    }

    /// Watches the directories the repo search walks, and the repos it finds that git
    /// could not read (`git init` or a copy may still be writing them) with their git
    /// directories, so that a change in either tries it again. Call after each search:
    /// it also takes in the failed reads that set when such a repo is tried again (see
    /// [`Unread`]). When it adds a watch, it searches once more: what changed in the
    /// directory before it was watched was not seen.
    fn watch_search(&mut self) {
        let now = Instant::now();
        let walk = scan::walk_roots(self.agent.config());
        let unread: Vec<PathBuf> = walk
            .repos
            .into_iter()
            .filter(|repo| self.agent.repo(repo).is_none())
            .collect();
        self.unread.retain(|repo, _| unread.contains(repo));
        for repo in &unread {
            match self.unread.get_mut(repo) {
                Some(known) => known.failed(now),
                None => {
                    self.unread.insert(repo.clone(), Unread::new(now));
                }
            }
        }
        let Some(watcher) = &mut self.watcher else {
            return;
        };
        if watcher.watch_scan_dirs(&walk.dirs, &unread) {
            self.pending.rescan = true;
            self.pending.touch(now);
        }
    }

    /// When a repo git could not read is next tried.
    fn next_retry(&self) -> Option<Instant> {
        self.unread.values().filter_map(Unread::next).min()
    }

    /// Searches again for the repos git could not read that are due a try.
    fn retry_due(&mut self, now: Instant) {
        let mut due = false;
        for unread in self.unread.values_mut() {
            due |= unread.start(now);
        }
        if due {
            self.pending.rescan = true;
            self.pending.touch(now);
        }
    }

    /// Watches the status hook's directory, when there is one.
    fn watch_hook_dir(&mut self) {
        if let Some(watcher) = &mut self.watcher {
            watcher.watch_hook_dir(&hook::status_dir());
        }
    }

    fn watch_repo(&mut self, repo: &Path) {
        let Some(watcher) = &mut self.watcher else {
            return;
        };
        let Some(state) = self.agent.repo(repo) else {
            return;
        };
        let git_dir = scan::git_dir(repo);
        let common = git_dir.as_deref().and_then(scan::common_dir);
        watcher.watch_repo(
            repo,
            git_dir.as_deref(),
            common.as_deref(),
            &state.head.tracked_dirs,
        );
        for dir in self.agent.reader().project_dirs(&path_str(repo)) {
            watcher.watch_project_dir(&dir);
        }
    }

    /// Drops the watches of repos that are gone, including project directories no
    /// remaining repo uses.
    fn forget_repos(&mut self, removed: &[PathBuf]) {
        let Some(watcher) = &mut self.watcher else {
            return;
        };
        let remaining: Vec<String> = self.agent.repo_paths().map(|p| path_str(p)).collect();
        for path in removed {
            watcher.unwatch_repo(path);
            let repo = path_str(path);
            for name in watcher.project_dirs() {
                if project_dir_matches(&name, &repo)
                    && !remaining.iter().any(|r| project_dir_matches(&name, r))
                {
                    watcher.unwatch_project_dir(&name);
                    let dir = self.agent.reader().projects_dir().join(&name);
                    self.agent.reader_mut().forget_dir(&dir);
                }
            }
        }
    }

    fn queue(&mut self, batch: &[FsEvent]) {
        let Some(watcher) = &mut self.watcher else {
            return;
        };
        let now = Instant::now();
        for event in batch {
            for change in watcher.changes(event) {
                self.pending.add(change, now);
            }
            watcher.forget_dropped(event);
        }
    }

    fn flush(&mut self) -> io::Result<()> {
        let pending = std::mem::take(&mut self.pending);
        let loading = self.load == Load::Running;
        let mut events = Vec::new();
        let mut refresh = pending.repos;

        // A change in a repo git could not read prompts a try once the repo is quiet
        // (see `Unread`); one the search has not taken in yet, at once.
        let now = Instant::now();
        let mut rescan = pending.rescan;
        for repo in &pending.retry {
            match self.unread.get_mut(repo) {
                Some(unread) => unread.changed(now),
                None => rescan = true,
            }
        }

        if loading {
            // The loaded phases replace the repo list; rescan once they are in.
            self.deferred_rescan |= rescan;
            self.deferred_reload |= pending.reload;
        } else if rescan || pending.reload {
            let (found, added, removed) = self.agent.rescan();
            events.extend(found);
            self.forget_repos(&removed);
            for path in &removed {
                refresh.remove(path);
            }
            for path in &added {
                self.watch_repo(path);
                refresh.remove(path);
            }
            self.watch_search();
        }
        if pending.reload && !loading {
            for path in self.agent.repo_paths() {
                refresh.insert(path.clone(), Refresh::ALL);
            }
        }

        if pending.reload || !pending.hook_files.is_empty() {
            let paths = hook::Paths::from_env();
            let dir = &paths.status_dir;
            let hooks = if !hook::enabled(&paths) {
                HashMap::new()
            } else if pending.reload {
                hook::read_statuses(dir)
            } else {
                let mut hooks = self.agent.hooks().clone();
                hook::update_statuses(dir, &mut hooks, &pending.hook_files);
                hooks
            };
            events.extend(self.agent.set_hooks(hooks));
        }

        let repos: Vec<PathBuf> = self.agent.repo_paths().cloned().collect();
        for dir in pending.new_project_dirs {
            let matching: Vec<&PathBuf> = repos
                .iter()
                .filter(|r| project_dir_matches(&dir, &path_str(r)))
                .collect();
            if matching.is_empty() {
                continue;
            }
            if let Some(watcher) = &mut self.watcher {
                watcher.watch_project_dir(&self.agent.config().claude_projects.join(&dir));
            }
            for repo in matching {
                mark_sessions(&mut refresh, repo);
            }
        }
        for dir in pending.project_dirs {
            for repo in repos
                .iter()
                .filter(|r| project_dir_matches(&dir, &path_str(r)))
            {
                mark_sessions(&mut refresh, repo);
            }
        }
        if loading {
            for (path, what) in &refresh {
                let entry = self.touched.entry(path.clone()).or_default();
                *entry = entry.merge(*what);
            }
        }

        let mut order: Vec<_> = refresh.into_iter().collect();
        order.sort_by(|a, b| a.0.cmp(&b.0));
        let mut rewatch = pending.rewatch;
        for (path, what) in order {
            events.extend(self.agent.refresh(&path, what));
            if what.head || rewatch.remove(&path) {
                self.watch_repo(&path);
            }
        }
        for path in rewatch {
            self.watch_repo(&path);
        }
        self.send_all(&events)
    }

    /// Arms the liveness timer when a session is live or recent, and disarms it
    /// otherwise, so an idle helper sleeps.
    fn schedule_live(&mut self) {
        if self.load == Load::NotStarted || !self.agent.wants_live_checks(Utc::now()) {
            self.next_live = None;
        } else if self.next_live.is_none() {
            self.next_live = Some(Instant::now() + LIVE_INTERVAL);
        }
    }

    fn send_all(&mut self, events: &[Event]) -> io::Result<()> {
        for event in events {
            self.send(event)?;
        }
        Ok(())
    }

    /// Writes one message. Blocks while the app is not reading stdout (see the module
    /// documentation for what that does to the other threads).
    fn send<T: Serialize>(&mut self, message: &T) -> io::Result<()> {
        serde_json::to_writer(&mut self.out, message)?;
        self.out.write_all(b"\n")?;
        self.out.flush()
    }
}

fn mark_sessions(refresh: &mut HashMap<PathBuf, Refresh>, repo: &Path) {
    let entry = refresh.entry(repo.to_owned()).or_default();
    entry.sessions = true;
}

/// The setup thread: runs each job with `run` and sends its response to the main
/// thread, until either side goes away. A job that panics is answered with
/// `SETUP_FAILED` and the thread goes on to the next.
fn run_setup_jobs(
    jobs: Receiver<setup::Job>,
    tx: &SyncSender<Msg>,
    run: impl Fn(setup::Job) -> Response,
) {
    for job in jobs {
        let id = job.id.clone();
        let response = catch_unwind(AssertUnwindSafe(|| run(job)))
            .unwrap_or_else(|_| Response::err(id, code::SETUP_FAILED, "internal error"));
        if tx.send(Msg::Answer(response)).is_err() {
            return;
        }
    }
}

/// The clone thread: runs each job with `run` and sends its result to the main thread,
/// until either side goes away. A job that panics fails with "internal error" and the
/// thread goes on to the next.
fn run_clone_jobs(
    jobs: Receiver<repos::Job>,
    tx: &SyncSender<Msg>,
    run: impl Fn(&repos::Job) -> Result<PathBuf, String>,
) {
    for job in jobs {
        let result = catch_unwind(AssertUnwindSafe(|| run(&job)))
            .unwrap_or_else(|_| Err("internal error".into()));
        if tx.send(Msg::Cloned { id: job.id, result }).is_err() {
            return;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;
    use std::process::Command;

    #[test]
    fn a_panicking_setup_job_is_answered_and_the_worker_goes_on() {
        let (jobs, rx) = mpsc::channel();
        let (tx, answers) = mpsc::sync_channel(4);
        for n in [1u64, 2] {
            let job = setup::Job {
                id: n.into(),
                vsix: "/tmp/refract.vsix".into(),
                commit: None,
            };
            jobs.send(job).unwrap();
        }
        drop(jobs);
        run_setup_jobs(rx, &tx, |job| {
            if job.id == 1u64.into() {
                panic!("setup blew up");
            }
            Response::ok(job.id, Reply::Done)
        });
        let answers: Vec<Response> = answers
            .try_iter()
            .map(|msg| match msg {
                Msg::Answer(response) => response,
                _ => panic!("expected an answer"),
            })
            .collect();
        assert_eq!(
            answers,
            [
                Response::err(1u64, code::SETUP_FAILED, "internal error"),
                Response::ok(2u64, Reply::Done),
            ]
        );
    }

    fn lines(input: &[u8]) -> Vec<Line> {
        let mut input = Cursor::new(input.to_vec());
        let mut buf = Vec::new();
        let mut out = Vec::new();
        while let Some(line) = read_line(&mut input, &mut buf).unwrap() {
            out.push(line);
        }
        out
    }

    #[test]
    fn lines_survive_bad_utf8_and_crlf() {
        assert_eq!(
            lines(b"{\"id\":1}\r\n{\"x\":\"\xff\"}\nlast"),
            [
                Line::Text("{\"id\":1}".into()),
                Line::BadUtf8("{\"x\":\"\u{fffd}\"}".into()),
                Line::Text("last".into()),
            ]
        );
    }

    #[test]
    fn overlong_lines_are_discarded_up_to_their_newline() {
        let mut input = vec![b'x'; MAX_LINE + 10];
        input.extend_from_slice(b"\nping\n");
        assert_eq!(lines(&input), [Line::TooLong, Line::Text("ping".into())]);
        let exact = vec![b'y'; MAX_LINE];
        assert!(matches!(&lines(&exact)[..], [Line::Text(t)] if t.len() == MAX_LINE));
    }

    /// Runs the server's side of [`Unread`] over `from..to` milliseconds after `t`, a step
    /// of `step` at a time, with a change in the repo at each step `changes` is true for,
    /// and a search that fails 300 ms after each try. Returns when each try began, in
    /// seconds.
    fn tries(
        unread: &mut Unread,
        t: Instant,
        (from, to, step): (u64, u64, u64),
        changes: impl Fn(u64) -> bool,
    ) -> Vec<f64> {
        let mut tries = Vec::new();
        let mut searching = None;
        for ms in (from..to).step_by(step as usize) {
            let at = t + Duration::from_millis(ms);
            if changes(ms) {
                unread.changed(at);
            }
            if searching.is_some_and(|began| ms >= began + 300) {
                unread.failed(at);
                searching = None;
            }
            if searching.is_none() && unread.next().is_some_and(|next| next <= at) {
                assert!(unread.start(at));
                tries.push(ms as f64 / 1000.0);
                searching = Some(ms);
            }
        }
        tries
    }

    #[test]
    fn timed_retries_back_off_and_stop() {
        let t = Instant::now();
        let s = Duration::from_secs;
        let mut unread = Unread::new(t);
        assert_eq!(unread.next(), Some(t + s(2)));
        // A search made for another reason counts for nothing.
        unread.failed(t + s(1));
        assert_eq!(unread.next(), Some(t + s(2)));
        let seen = tries(&mut unread, t, (0, 200_000, 50), |_| false);
        assert_eq!(seen, [2.0, 12.3, 72.6]);
        assert_eq!(unread.next(), None);
    }

    #[test]
    fn a_change_is_tried_once_the_repo_is_quiet() {
        let t = Instant::now();
        let mut unread = Unread::new(t);
        // The timed retries have grown long when a copy ends at 20 s with a burst of
        // writes: the try comes 2 s after the last one, not at the next timed retry.
        let seen = tries(&mut unread, t, (0, 19_000, 50), |_| false);
        assert_eq!(seen, [2.0, 12.3]);
        let copy = |ms| (20_000..20_500).contains(&ms);
        assert_eq!(tries(&mut unread, t, (19_000, 30_000, 50), copy), [22.45]);
        // A failed try prompted by a change does not put off the next change.
        let more = |ms| ms == 40_000 || ms == 50_000;
        assert_eq!(
            tries(&mut unread, t, (30_000, 60_000, 50), more),
            [42.0, 52.0]
        );
    }

    #[test]
    fn a_busy_repo_git_cannot_read_is_tried_about_once_a_minute() {
        // Written every quarter second for five minutes, then left alone: after the timed
        // retries, a minute between tries, and one last try once the writes stop.
        let t = Instant::now();
        let mut unread = Unread::new(t);
        let seen = tries(&mut unread, t, (0, 400_000, 250), |ms| ms < 300_000);
        assert_eq!(seen[..4], [2.0, 12.5, 60.0, 73.0]);
        assert!(
            seen[3..].windows(2).all(|w| w[1] - w[0] >= 47.0),
            "{seen:?}"
        );
        assert!(seen.len() <= 8, "{seen:?}");
        assert!(*seen.last().unwrap() <= 302.0, "{seen:?}");
        assert_eq!(unread.next(), None);

        // Written in bursts with pauses: three tries for bursts, then, once the timed
        // retries are over too, a minute between them.
        let mut unread = Unread::new(t);
        let bursts = |ms| ms % 5_000 < 500;
        let seen = tries(&mut unread, t, (100_000, 400_000, 250), bursts);
        assert_eq!(seen[..5], [100.0, 102.25, 107.25, 110.5, 112.25]);
        let late: Vec<f64> = seen.into_iter().filter(|s| *s > 180.0).collect();
        assert!(late.windows(2).all(|w| w[1] - w[0] >= 60.0), "{late:?}");
        assert!(late.len() <= 4, "{late:?}");
    }

    #[test]
    fn a_change_in_a_repo_git_cannot_read_waits_for_quiet() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().join("projects");
        let kelp = root.join("kelp-cli");
        std::fs::create_dir_all(kelp.join(".git")).unwrap();
        let projects = tmp.path().join("claude/projects");
        std::fs::create_dir_all(&projects).unwrap();
        let config = Config {
            roots: vec![root.clone()],
            extra: vec![],
            hidden: vec![],
            depth: 3,
            claude_projects: projects,
            limits_file: tmp.path().join("usage-state.json"),
        };
        let (tx, _rx) = mpsc::sync_channel(CHANNEL_CAPACITY);
        let mut server = Server::new(config, Vec::new(), None, tx);
        server.load = Load::Done;
        // The search found kelp-cli and git could not read it.
        server.watch_search();
        // A repo only a search would find.
        let aurora = root.join("aurora-notes");
        std::fs::create_dir_all(&aurora).unwrap();
        let init = Command::new("git")
            .args(["init", "-q", "-b", "main"])
            .arg(&aurora)
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .status()
            .unwrap();
        assert!(init.success());

        // A change in kelp-cli does not search at once.
        server.pending.add(Change::Retry(kelp), Instant::now());
        server.flush().unwrap();
        assert!(
            server.out.is_empty(),
            "{}",
            String::from_utf8_lossy(&server.out)
        );

        // Once the repo has been quiet, it does.
        let at = server.next_retry().unwrap();
        assert!(at >= Instant::now() + RETRY_QUIET - Duration::from_millis(500));
        server.retry_due(at);
        server.flush().unwrap();
        assert!(String::from_utf8_lossy(&server.out).contains("aurora-notes"));
    }

    #[test]
    fn repos_on_windows_drives_load_last() {
        assert!(is_slow(Path::new("/mnt/c/Users/dev/Desktop/tidepool-api")));
        assert!(is_slow(Path::new("/mnt/d/aurora-notes")));
        assert!(!is_slow(Path::new("/home/dev/projects/kelp-cli")));
        assert!(!is_slow(Path::new("/home/dev/mnt/kelp-cli")));
    }

    #[test]
    fn the_first_load_searches_windows_drives_apart() {
        let config = Config {
            roots: vec![
                "/home/dev/projects".into(),
                "/mnt/c/Users/dev/Desktop".into(),
            ],
            extra: vec!["/mnt/d/aurora-notes".into(), "/home/dev/kelp-cli".into()],
            hidden: vec!["/home/dev/projects/reef-maps".into()],
            depth: 2,
            claude_projects: "/home/dev/.claude/projects".into(),
            limits_file: "/home/dev/.claude/usage-state.json".into(),
        };
        let (slow, fast) = split_slow(&config);
        assert_eq!(slow.roots, [PathBuf::from("/mnt/c/Users/dev/Desktop")]);
        assert_eq!(slow.extra, [PathBuf::from("/mnt/d/aurora-notes")]);
        assert_eq!(fast.roots, [PathBuf::from("/home/dev/projects")]);
        assert_eq!(fast.extra, [PathBuf::from("/home/dev/kelp-cli")]);
        for half in [&slow, &fast] {
            assert_eq!(half.hidden, config.hidden);
            assert_eq!(half.depth, 2);
        }
    }
}
