//! The stdio loop: requests in, responses and events out.
//!
//! Three threads: one reads stdin, one reads inotify, and this one owns all state and is
//! the only writer to stdout. It blocks on a channel, waking only for input, for a
//! debounce deadline, or for the liveness timer while some session is live or recent.
//! A fourth thread runs the first load, so requests are answered while it runs, and a
//! fifth runs `setupServer` requests one at a time, sending each response back through
//! the channel for this thread to write.
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
    code, parse_request, ErrorBody, Event, Hello, Id, Method, Outcome, Reply, RepoList, Response,
    PROTOCOL_VERSION,
};
use refract_core::sessions::{paths_match, project_dir_matches};
use serde::Serialize;

use crate::config::Config;
use crate::launch;
use crate::scan;
use crate::setup;
use crate::state::{self, path_str, Agent, Refresh};
use crate::watch::{self, Change, FsEvent, Watcher};

/// Quiet time after the last file-system event before refreshing.
pub const DEBOUNCE: Duration = Duration::from_millis(300);

/// Longest a refresh waits while events keep arriving.
pub const MAX_DELAY: Duration = Duration::from_millis(1500);

/// How often running processes are checked while a session is live or recent.
pub const LIVE_INTERVAL: Duration = Duration::from_secs(5);

/// Longest request line accepted; longer ones are discarded and answered with an error.
pub const MAX_LINE: usize = 1024 * 1024;

/// Messages waiting for the main thread before the threads feeding it block.
const CHANNEL_CAPACITY: usize = 64;

enum Msg {
    Line(Line),
    Eof,
    Fs(Vec<FsEvent>),
    /// The loading thread finished a phase; `last` when this is the full set.
    Loaded {
        agent: Box<Agent>,
        last: bool,
    },
    /// A response the setup thread finished.
    Answer(Response),
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
    /// Refreshes asked for since the load started, re-applied to each loaded phase.
    touched: HashMap<PathBuf, Refresh>,
    /// `getRepo` requests for repos not loaded yet, answered when they are.
    waiting: Vec<(Id, String)>,
    /// Repos watched before the load, so watches of ones that failed to load can go.
    prewatched: Vec<PathBuf>,
    /// A rescan or reload asked for during the load, run once it is done.
    deferred_rescan: bool,
    deferred_reload: bool,
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
    let mut server = Server {
        out,
        agent: Agent::new(config),
        watcher,
        pending: Pending::default(),
        next_live: None,
        load: Load::NotStarted,
        tx,
        setup_jobs: None,
        touched: HashMap::new(),
        waiting: Vec::new(),
        prewatched: Vec::new(),
        deferred_rescan: false,
        deferred_reload: false,
    };

    loop {
        let deadline = [server.pending.deadline(), server.next_live]
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
            Ok(Msg::Loaded { agent, last }) => server.loaded(*agent, last)?,
            Ok(Msg::Answer(response)) => server.send(&response)?,
            Err(RecvTimeoutError::Timeout) => {}
        }
        let now = Instant::now();
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

/// The configs loaded one after the other: Linux repos first when some roots are under
/// `/mnt/`, where git is much slower, then everything.
fn load_phases(config: &Config) -> Vec<Config> {
    let slow = |p: &PathBuf| p.starts_with("/mnt/");
    let has_slow = config.roots.iter().chain(&config.extra).any(slow);
    let has_fast = config.roots.iter().chain(&config.extra).any(|p| !slow(p));
    let mut phases = Vec::new();
    if has_slow && has_fast {
        let mut fast = config.clone();
        fast.roots.retain(|p| !slow(p));
        fast.extra.retain(|p| !slow(p));
        phases.push(fast);
    }
    phases.push(config.clone());
    phases
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

impl<W: Write> Server<W> {
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
                match launch::prepare(&path, &mode, &dir, distro.as_deref(), Utc::now()) {
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

    /// Starts the first load in the background, after watching every directory it will
    /// read, so changes made while it runs are not lost.
    fn ensure_loaded(&mut self) -> io::Result<()> {
        if self.load != Load::NotStarted {
            return Ok(());
        }
        self.load = Load::Running;
        let config = self.agent.config().clone();
        let walk = scan::walk_roots(&config);
        if let Some(watcher) = &mut self.watcher {
            watcher.watch_projects_root(&config.claude_projects);
            watcher.watch_scan_dirs(&walk.dirs);
            for repo in &walk.repos {
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
        }
        self.prewatched = walk.repos;
        let phases = load_phases(&config);
        let tx = self.tx.clone();
        std::thread::Builder::new()
            .name("load".into())
            .spawn(move || {
                let count = phases.len();
                for (i, config) in phases.into_iter().enumerate() {
                    let mut agent = Agent::new(config);
                    agent.load_all();
                    let last = i + 1 == count;
                    let msg = Msg::Loaded {
                        agent: Box::new(agent),
                        last,
                    };
                    if tx.send(msg).is_err() {
                        return;
                    }
                }
            })?;
        Ok(())
    }

    /// Takes in one loaded phase: reports what it adds or changes, re-applies refreshes
    /// asked for since the load started, and watches the repos fully.
    fn loaded(&mut self, agent: Agent, last: bool) -> io::Result<()> {
        let old = std::mem::replace(&mut self.agent, agent);
        let mut events = Vec::new();
        let gone: Vec<PathBuf> = old
            .repo_paths()
            .filter(|p| self.agent.repo(p).is_none())
            .cloned()
            .collect();
        for path in &gone {
            events.push(Event::RepoRemoved {
                path: path_str(path),
            });
        }
        let repos: Vec<PathBuf> = self.agent.repo_paths().cloned().collect();
        for path in &repos {
            self.watch_repo(path);
        }
        // Only the root and git directory were watched while git ran, so an edit deeper
        // in the tree could have been missed: read status again now that the tree is
        // watched. Skipped under `/mnt/`, where it is slow and would block requests.
        let mut again = self.touched.clone();
        for path in repos.iter().filter(|p| old.repo(p).is_none()) {
            if !path.starts_with("/mnt/") {
                let entry = again.entry(path.clone()).or_default();
                entry.status = true;
            }
        }
        let mut again: Vec<(PathBuf, Refresh)> = again.into_iter().collect();
        again.sort_by(|a, b| a.0.cmp(&b.0));
        for (path, what) in again {
            // What changed is reported below, against what the app saw before.
            self.agent.refresh(&path, what);
        }
        for path in &repos {
            let new = &self.agent.repo(path).expect("listed repo").snapshot;
            match old.repo(path) {
                Some(before) => events.extend(state::diff(&before.snapshot, new)),
                None => events.push(Event::RepoChanged(Box::new(new.clone()))),
            }
        }
        if last {
            self.load = Load::Done;
            self.touched.clear();
            let unloaded: Vec<PathBuf> = std::mem::take(&mut self.prewatched)
                .into_iter()
                .filter(|p| self.agent.repo(p).is_none())
                .collect();
            self.forget_repos(&gone);
            self.forget_repos(&unloaded);
            if let Some(watcher) = &self.watcher {
                eprintln!(
                    "refract-agent: {} repos, {} watches",
                    repos.len(),
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

        if loading {
            // The loaded phases replace the repo list; rescan once they are in.
            self.deferred_rescan |= pending.rescan;
            self.deferred_reload |= pending.reload;
        } else if pending.rescan || pending.reload {
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
            if let Some(watcher) = &mut self.watcher {
                watcher.watch_scan_dirs(&scan::walk_roots(self.agent.config()).dirs);
            }
        }
        if pending.reload && !loading {
            for path in self.agent.repo_paths() {
                refresh.insert(path.clone(), Refresh::ALL);
            }
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

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;

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

    #[test]
    fn linux_repos_load_first() {
        let config = Config {
            roots: vec![
                "/home/dev/projects".into(),
                "/mnt/c/Users/dev/Desktop".into(),
            ],
            extra: vec!["/mnt/d/aurora-notes".into()],
            hidden: vec![],
            depth: 3,
            claude_projects: "/home/dev/.claude/projects".into(),
        };
        let phases = load_phases(&config);
        assert_eq!(phases.len(), 2);
        assert_eq!(phases[0].roots, [PathBuf::from("/home/dev/projects")]);
        assert!(phases[0].extra.is_empty());
        assert_eq!(phases[1], config);

        let linux_only = Config {
            roots: vec!["/home/dev/projects".into()],
            extra: vec![],
            ..config
        };
        assert_eq!(load_phases(&linux_only), [linux_only]);
    }
}
