//! The agent's picture of every repo, and the events that keep the app in step with it.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::Mutex;

use chrono::{DateTime, TimeDelta, Utc};
use refract_core::classify::{classify_report, HookReport, SessionState};
use refract_core::mass::{masses, MassInput};
use refract_core::preselect::{preselect, resume_reasons, Candidate, RepoFacts, Upstream};
use refract_core::protocol::{
    Event, RefactorInfo, RemoteId, RepoSnapshot, SessionChange, SessionInfo, WorktreeInfo,
};
use refract_core::sessions::{path_within, paths_match};
use refract_core::transcript::SessionSummary;
use refract_core::worktree::{derive_worktree_status, WorktreeFacts};

use crate::config::Config;
use crate::git::{self, HeadInfo, Status};
use crate::procs::{self, ClaudeProcess, LiveProcess};
use crate::scan;
use crate::sessions::SessionReader;
use crate::subagents;

/// Sessions active this recently keep their repo on the liveness timer.
pub const RECENT_ACTIVITY: TimeDelta = TimeDelta::minutes(10);

/// Masses that move less than this do not cause a `repoChanged` for other repos.
const MASS_EPSILON: f64 = 0.005;

/// What to re-read for a repo.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Refresh {
    /// `git status`.
    pub status: bool,
    /// HEAD facts: last commit, tracked files, origin.
    pub head: bool,
    /// Transcripts and liveness.
    pub sessions: bool,
}

impl Refresh {
    /// Everything.
    pub const ALL: Refresh = Refresh {
        status: true,
        head: true,
        sessions: true,
    };

    /// Combines two refreshes.
    pub fn merge(self, other: Refresh) -> Refresh {
        Refresh {
            status: self.status || other.status,
            head: self.head || other.head,
            sessions: self.sessions || other.sessions,
        }
    }
}

/// One repo's raw facts and the snapshot built from them.
#[derive(Debug, Clone)]
pub struct RepoState {
    pub status: Status,
    pub head: HeadInfo,
    pub sessions: Vec<SessionSummary>,
    /// Live sessions by id, with the process running each when that is certain.
    pub live: HashMap<String, Option<LiveProcess>>,
    /// Status hook statuses of this repo's sessions, by session id.
    pub hooks: HashMap<String, HookReport>,
    /// Background subagents and workflow runs running, by live session id; sessions with
    /// none are left out.
    pub background: HashMap<String, u32>,
    pub snapshot: RepoSnapshot,
    /// What the repo was on disk when it was first read.
    pub id: Option<scan::RepoId>,
}

impl RepoState {
    /// Recomputes `snapshot.preselect` for `now` from the stored facts and returns true
    /// when it changed. The preselect depends on the time (sessions age past
    /// [`MAX_RESUME_AGE`](refract_core::preselect::MAX_RESUME_AGE)), so a snapshot
    /// built earlier can be stale even when nothing on disk moved; call this before
    /// answering `listRepos` or `getRepo`.
    pub fn refresh_preselect(&mut self, now: DateTime<Utc>) -> bool {
        let candidates = candidates(
            &self.snapshot.path,
            &self.sessions,
            &self.live,
            &self.hooks,
            &self.background,
        );
        let fresh = preselect(&candidates, &repo_facts(&self.status, &self.head), now);
        if fresh == self.snapshot.preselect {
            return false;
        }
        self.snapshot.preselect = fresh;
        true
    }

    fn wants_live_checks(&self, now: DateTime<Utc>) -> bool {
        !self.live.is_empty()
            || self
                .sessions
                .iter()
                .any(|s| s.last_timestamp.is_some_and(|t| now - t < RECENT_ACTIVITY))
    }
}

/// Every known repo.
#[derive(Debug)]
pub struct Agent {
    config: Config,
    reader: SessionReader,
    repos: BTreeMap<PathBuf, RepoState>,
    procs: Vec<ClaudeProcess>,
    /// Every status the status hook reported, by session id.
    hooks: HashMap<String, HookReport>,
    /// Each repo's refactor, by repo path, as [`crate::refactor`] last worked it out.
    refactors: HashMap<PathBuf, RefactorInfo>,
    /// What live sessions' transcripts said about their background work.
    subagents: subagents::Tracker,
}

impl Agent {
    /// An agent with no repos loaded yet.
    pub fn new(config: Config) -> Agent {
        let reader = SessionReader::new(config.claude_projects.clone());
        Agent {
            config,
            reader,
            repos: BTreeMap::new(),
            procs: Vec::new(),
            hooks: HashMap::new(),
            refactors: HashMap::new(),
            subagents: subagents::Tracker::default(),
        }
    }

    pub fn config(&self) -> &Config {
        &self.config
    }

    /// Reports `repo` from the next [`Agent::rescan`] on, though it is under no root.
    pub fn add_extra(&mut self, repo: PathBuf) {
        if !self.config.extra.contains(&repo) {
            self.config.extra.push(repo);
        }
    }

    /// Reports `repo` from the next [`Agent::rescan`] on, though the config hides it.
    pub fn unhide(&mut self, repo: &Path) {
        self.config.hidden.retain(|p| p != repo);
    }

    /// Stops reporting `repo` from the next [`Agent::rescan`] on.
    pub fn hide(&mut self, repo: PathBuf) {
        if !self.config.hidden.contains(&repo) {
            self.config.hidden.push(repo);
        }
    }

    /// Makes `extra` and `hidden` entries naming `old` name `new`, as
    /// [`crate::config::replace_path`] does in the file.
    pub fn repoint(&mut self, old: &Path, new: &Path) {
        let old = path_str(old);
        for path in self.config.extra.iter_mut().chain(&mut self.config.hidden) {
            if paths_match(&path_str(path), &old) {
                *path = new.to_owned();
            }
        }
    }

    pub fn reader(&self) -> &SessionReader {
        &self.reader
    }

    pub fn reader_mut(&mut self) -> &mut SessionReader {
        &mut self.reader
    }

    /// Finds and reads every repo. Git runs for several repos at once.
    pub fn load_all(&mut self) {
        let paths = scan::find_repos(&self.config);
        let gits = read_git_parallel(&paths);
        self.repos.clear();
        self.insert_loaded(paths.into_iter().zip(gits).collect());
    }

    /// Takes in repos whose git facts were read elsewhere, as the first load does, reads
    /// their sessions, and recomputes every mass. Returns the repos taken in; one whose
    /// facts could not be read is skipped. Reports no events: the caller compares
    /// snapshots, since other repos' masses can move too.
    pub fn insert_loaded(&mut self, loaded: Vec<(PathBuf, GitResult)>) -> Vec<PathBuf> {
        if loaded.is_empty() {
            return Vec::new();
        }
        self.procs = procs::scan();
        let mut inserted = Vec::new();
        for (path, git) in loaded {
            match git {
                Ok((status, head)) => {
                    self.insert(path.clone(), status, head);
                    inserted.push(path);
                }
                Err(e) => eprintln!("refract-agent: skipping {}: {e}", path.display()),
            }
        }
        self.resolve_worktrees();
        self.update_masses();
        inserted
    }

    /// Paths of known repos.
    pub fn repo_paths(&self) -> impl Iterator<Item = &PathBuf> {
        self.repos.keys()
    }

    pub fn repo(&self, path: &Path) -> Option<&RepoState> {
        self.repos.get(path)
    }

    /// The process running session `id` of the repo at `path`, from a fresh look at
    /// `/proc` and the repo's transcripts, so it works before the first load reaches
    /// the repo and for sessions started since the last liveness check.
    pub fn live_process(&mut self, path: &Path, id: &str) -> Option<LiveProcess> {
        self.procs = procs::scan();
        let sessions = self.read_sessions(path);
        live_set(&self.procs, path, &sessions).remove(id)?
    }

    /// All snapshots, sorted by path.
    pub fn snapshots(&self) -> Vec<RepoSnapshot> {
        self.repos.values().map(|r| r.snapshot.clone()).collect()
    }

    /// Re-reads parts of one repo and returns the events describing what changed.
    pub fn refresh(&mut self, path: &Path, what: Refresh) -> Vec<Event> {
        let Some(old) = self.repos.get(path) else {
            return Vec::new();
        };
        let mut status = old.status.clone();
        let mut head = old.head.clone();
        if what.status || what.head {
            match git::read_status(path) {
                Ok(s) => status = s,
                Err(e) => {
                    eprintln!("refract-agent: git status in {}: {e}", path.display());
                    return Vec::new();
                }
            }
        }
        if what.head {
            match git::read_head(path) {
                Ok(h) => head = h,
                Err(e) => eprintln!("refract-agent: git in {}: {e}", path.display()),
            }
        }
        if what.status || what.head {
            git::finish_worktree(path, &mut status, &head);
        }
        let old_snapshot = old.snapshot.clone();
        if what.sessions {
            self.procs = procs::scan();
        }
        let (sessions, live, background) = if what.sessions {
            let sessions = self.read_sessions(path);
            let live = live_set(&self.procs, path, &sessions);
            let background = self.background_for(path, &live);
            (sessions, live, background)
        } else {
            (
                old.sessions.clone(),
                old.live.clone(),
                old.background.clone(),
            )
        };
        self.store(path.to_owned(), status, head, sessions, live, background);
        let moved = if what.head {
            self.update_masses()
        } else {
            Vec::new()
        };
        let mut events = diff(&old_snapshot, &self.repos[path].snapshot);
        events.extend(self.repo_events(moved.iter().filter(|p| p.as_path() != path)));
        events
    }

    /// Finds repos again, adding new ones and dropping vanished ones. Returns the
    /// events, the paths that were added, so the caller can watch them, and the paths
    /// that were removed, so it can forget their watches.
    ///
    /// A repo put in place of another at the same path (the old one moved away or
    /// deleted, another copied or cloned there) is both removed and added: its watches
    /// still point at the old directories. It is reported as removed only if it cannot
    /// be read.
    pub fn rescan(&mut self) -> (Vec<Event>, Vec<PathBuf>, Vec<PathBuf>) {
        let found: Vec<PathBuf> = scan::find_repos(&self.config);
        let replaced: Vec<PathBuf> = found
            .iter()
            .filter(|p| self.repos.get(*p).is_some_and(|r| r.id != scan::repo_id(p)))
            .cloned()
            .collect();
        let mut removed: Vec<PathBuf> = self
            .repos
            .keys()
            .filter(|p| !found.contains(p))
            .cloned()
            .collect();
        let mut events = Vec::new();
        for path in &removed {
            self.repos.remove(path);
            events.push(Event::RepoRemoved {
                path: path_str(path),
            });
        }
        for path in &replaced {
            self.repos.remove(path);
        }
        let added: Vec<PathBuf> = found
            .into_iter()
            .filter(|p| !self.repos.contains_key(p))
            .collect();
        if !added.is_empty() {
            self.procs = procs::scan();
        }
        let mut inserted = Vec::new();
        for (path, git) in added.iter().zip(read_git_parallel(&added)) {
            match git {
                Ok((status, head)) => {
                    self.insert(path.clone(), status, head);
                    inserted.push(path.clone());
                }
                Err(e) => {
                    eprintln!("refract-agent: skipping {}: {e}", path.display());
                    if replaced.contains(path) {
                        events.push(Event::RepoRemoved {
                            path: path_str(path),
                        });
                    }
                }
            }
        }
        let mut moved = self.update_masses();
        if !inserted.is_empty() || !removed.is_empty() {
            moved.extend(self.resolve_worktrees());
            moved.sort();
            moved.dedup();
        }
        events.extend(self.repo_events(inserted.iter()));
        events.extend(self.repo_events(moved.iter().filter(|p| !inserted.contains(p))));
        removed.extend(replaced);
        (events, inserted, removed)
    }

    /// Recomputes every repo's preselect for `now` (see
    /// [`RepoState::refresh_preselect`]) and returns the repos whose preselect changed.
    pub fn refresh_preselects(&mut self, now: DateTime<Utc>) -> Vec<PathBuf> {
        self.repos
            .iter_mut()
            .filter_map(|(path, repo)| repo.refresh_preselect(now).then(|| path.clone()))
            .collect()
    }

    /// True when some repo has a live or recently active session, so liveness should be
    /// checked on a timer.
    pub fn wants_live_checks(&self, now: DateTime<Utc>) -> bool {
        self.repos.values().any(|r| r.wants_live_checks(now))
    }

    /// Statuses from the status hook, by session id.
    pub fn hooks(&self) -> &HashMap<String, HookReport> {
        &self.hooks
    }

    /// Takes in the status hook's statuses and returns events for the sessions whose
    /// state changed with them.
    pub fn set_hooks(&mut self, hooks: HashMap<String, HookReport>) -> Vec<Event> {
        self.hooks = hooks;
        let stale: Vec<PathBuf> = self
            .repos
            .iter()
            .filter(|(_, r)| hooks_for(&self.hooks, &r.sessions) != r.hooks)
            .map(|(p, _)| p.clone())
            .collect();
        let mut events = Vec::new();
        for path in stale {
            let repo = &self.repos[&path];
            let old = repo.snapshot.clone();
            let (status, head, sessions, live, background) = (
                repo.status.clone(),
                repo.head.clone(),
                repo.sessions.clone(),
                repo.live.clone(),
                repo.background.clone(),
            );
            self.store(path.clone(), status, head, sessions, live, background);
            events.extend(diff(&old, &self.repos[&path].snapshot));
        }
        events
    }

    /// Sets the refactor of the repo at `path` (`None`: it has none) and returns a
    /// `repoChanged` when that changes its snapshot.
    pub fn set_refactor(&mut self, path: &Path, info: Option<RefactorInfo>) -> Vec<Event> {
        match &info {
            Some(info) => self.refactors.insert(path.to_owned(), info.clone()),
            None => self.refactors.remove(path),
        };
        match self.repos.get_mut(path) {
            Some(repo) if repo.snapshot.refactor != info => {
                repo.snapshot.refactor = info;
                vec![Event::RepoChanged(Box::new(repo.snapshot.clone()))]
            }
            _ => Vec::new(),
        }
    }

    /// Re-checks running processes for repos with live or recent sessions and returns
    /// events for sessions whose state changed. Sessions with background work running
    /// are counted again too, since work falls silent without any file changing (see
    /// [`subagents::STALE_AFTER`]).
    pub fn recheck_live(&mut self, now: DateTime<Utc>) -> Vec<Event> {
        self.procs = procs::scan();
        let watched: Vec<PathBuf> = self
            .repos
            .iter()
            .filter(|(_, r)| r.wants_live_checks(now))
            .map(|(p, _)| p.clone())
            .collect();
        let mut events = Vec::new();
        for path in watched {
            let repo = &self.repos[&path];
            let live = live_set(&self.procs, &path, &repo.sessions);
            if live == repo.live {
                continue;
            }
            let old = repo.snapshot.clone();
            let (status, head, sessions) = (
                repo.status.clone(),
                repo.head.clone(),
                repo.sessions.clone(),
            );
            let background = self.background_for(&path, &live);
            self.store(path.clone(), status, head, sessions, live, background);
            events.extend(diff(&old, &self.repos[&path].snapshot));
        }
        events.extend(self.recount(|r| !r.background.is_empty()));
        events
    }

    /// Counts the background work of live sessions again, of those in `only` or of
    /// every live session, and returns events for the sessions whose state changed.
    pub fn refresh_background(&mut self, only: Option<&HashSet<String>>) -> Vec<Event> {
        self.recount(|r| {
            r.live
                .keys()
                .any(|id| only.is_none_or(|ids| ids.contains(id)))
        })
    }

    /// Counts the background work of the live sessions of repos `which` picks again.
    fn recount(&mut self, which: impl Fn(&RepoState) -> bool) -> Vec<Event> {
        let picked: Vec<(PathBuf, HashMap<String, Option<LiveProcess>>)> = self
            .repos
            .iter()
            .filter(|(_, r)| !r.live.is_empty() && which(r))
            .map(|(p, r)| (p.clone(), r.live.clone()))
            .collect();
        let mut events = Vec::new();
        for (path, live) in picked {
            let background = self.background_for(&path, &live);
            let repo = &self.repos[&path];
            if background == repo.background {
                continue;
            }
            let old = repo.snapshot.clone();
            let (status, head, sessions) = (
                repo.status.clone(),
                repo.head.clone(),
                repo.sessions.clone(),
            );
            self.store(path.clone(), status, head, sessions, live, background);
            events.extend(diff(&old, &self.repos[&path].snapshot));
        }
        events
    }

    /// Every live session's id and the directory beside its transcript, which says what
    /// background work it runs (see [`subagents::watched_dirs`]). Forgets what the
    /// transcripts of other sessions said.
    pub fn live_session_dirs(&mut self) -> Vec<(String, PathBuf)> {
        let mut dirs: Vec<(String, PathBuf)> = self
            .repos
            .values()
            .flat_map(|r| r.live.keys())
            .filter_map(|id| Some((id.clone(), self.reader.session_dir(id)?)))
            .collect();
        dirs.sort();
        dirs.dedup();
        let kept: Vec<PathBuf> = dirs.iter().map(|(_, d)| d.clone()).collect();
        self.subagents.retain(&kept);
        dirs
    }

    /// Background work running for session `id`, live in `cwd` in `process` when that
    /// is known (see [`crate::subagents`]). Only work started since the process did
    /// counts; when the process is not known, since the earliest of the processes in
    /// `cwd` that name no session, one of which runs it. `procs` are the running
    /// `claude` processes.
    pub fn background_count(
        &mut self,
        procs: &[ClaudeProcess],
        cwd: &str,
        id: &str,
        process: Option<&LiveProcess>,
    ) -> u32 {
        let Some(dir) = self.reader.session_dir(id) else {
            return 0;
        };
        let since = match process {
            Some(p) => p.started,
            None => procs::earliest_unnamed_start(procs, cwd),
        };
        self.subagents
            .count(&dir, since, std::time::SystemTime::now())
    }

    /// [`Agent::background_count`] for each session in `live`, the repo at `repo`'s.
    /// Sessions with none are left out.
    fn background_for(
        &mut self,
        repo: &Path,
        live: &HashMap<String, Option<LiveProcess>>,
    ) -> HashMap<String, u32> {
        let procs = std::mem::take(&mut self.procs);
        let cwd = path_str(repo);
        let counts = live
            .iter()
            .filter_map(|(id, process)| {
                let count = self.background_count(&procs, &cwd, id, process.as_ref());
                (count > 0).then(|| (id.clone(), count))
            })
            .collect();
        self.procs = procs;
        counts
    }

    fn insert(&mut self, path: PathBuf, status: Status, head: HeadInfo) {
        let sessions = self.read_sessions(&path);
        let live = live_set(&self.procs, &path, &sessions);
        let background = self.background_for(&path, &live);
        self.store(path, status, head, sessions, live, background);
    }

    fn store(
        &mut self,
        path: PathBuf,
        status: Status,
        head: HeadInfo,
        sessions: Vec<SessionSummary>,
        live: HashMap<String, Option<LiveProcess>>,
        background: HashMap<String, u32>,
    ) {
        let old = self.repos.get(&path);
        let mass = old.map_or(0.0, |r| r.snapshot.mass);
        let id = old.map_or_else(|| scan::repo_id(&path), |r| r.id);
        let hooks = hooks_for(&self.hooks, &sessions);
        let mut snapshot = build_snapshot(
            &path,
            &status,
            &head,
            &sessions,
            &live,
            &hooks,
            &background,
            Utc::now(),
        );
        snapshot.mass = mass;
        snapshot.refactor = self.refactors.get(&path).cloned();
        self.set_worktree(&path, &status, &head, &mut snapshot);
        self.repos.insert(
            path,
            RepoState {
                status,
                head,
                sessions,
                live,
                hooks,
                background,
                snapshot,
                id,
            },
        );
    }

    /// Sets the worktree parts of `snapshot`, the snapshot of the repo at `path`, when it
    /// is a linked worktree: its main repo as a known repo names it, and its facts, which
    /// hold a repo also when another known repo lies inside it (spec §5 rule 5).
    fn set_worktree(
        &self,
        path: &Path,
        status: &Status,
        head: &HeadInfo,
        snapshot: &mut RepoSnapshot,
    ) {
        let Some(worktree) = &status.worktree else {
            return;
        };
        let mut facts = git::worktree_facts(worktree, head);
        facts.holds_repo |= self
            .repos
            .keys()
            .any(|known| known != path && known.starts_with(path));
        snapshot.worktree_of = Some(path_str(&self.known_form(&worktree.main)));
        snapshot.worktree = Some(worktree_info(&facts, &worktree.ignored));
    }

    /// Sets the worktree parts of every snapshot again, once a batch of repos is in: a
    /// worktree's main repo, or a repo inside it, may have come in after it. Returns the
    /// repos whose snapshot changed.
    fn resolve_worktrees(&mut self) -> Vec<PathBuf> {
        let paths: Vec<PathBuf> = self
            .repos
            .iter()
            .filter(|(_, r)| r.status.worktree.is_some())
            .map(|(p, _)| p.clone())
            .collect();
        let mut changed = Vec::new();
        for path in paths {
            let repo = &self.repos[&path];
            let mut snapshot = repo.snapshot.clone();
            self.set_worktree(&path, &repo.status, &repo.head, &mut snapshot);
            if snapshot != self.repos[&path].snapshot {
                self.repos.get_mut(&path).expect("known repo").snapshot = snapshot;
                changed.push(path);
            }
        }
        changed
    }

    /// `path` as a known repo names it, when one is the same directory under another
    /// path (through a symlink); else `path` unchanged.
    pub fn known_form(&self, path: &Path) -> PathBuf {
        if self.repos.contains_key(path) {
            return path.to_owned();
        }
        let real = std::fs::canonicalize(path).unwrap_or_else(|_| path.to_owned());
        self.repos
            .keys()
            .find(|known| std::fs::canonicalize(known).is_ok_and(|k| k == real))
            .cloned()
            .unwrap_or_else(|| path.to_owned())
    }

    /// Reads the sessions of `repo`, leaving out those that belong to a known repo
    /// nested in it, such as a worktree inside its folder.
    fn read_sessions(&mut self, repo: &Path) -> Vec<SessionSummary> {
        let root = path_str(repo);
        let nested: Vec<String> = self
            .repos
            .keys()
            .map(|p| path_str(p))
            .filter(|p| !paths_match(p, &root) && path_within(p, &root))
            .collect();
        let nested: Vec<&str> = nested.iter().map(String::as_str).collect();
        self.reader.read_repo_within(&root, &nested)
    }

    fn repo_events<'a>(&self, paths: impl Iterator<Item = &'a PathBuf>) -> Vec<Event> {
        paths
            .map(|p| Event::RepoChanged(Box::new(self.repos[p].snapshot.clone())))
            .collect()
    }

    /// Recomputes every repo's mass and returns the repos whose mass moved noticeably.
    fn update_masses(&mut self) -> Vec<PathBuf> {
        let inputs: Vec<MassInput> = self
            .repos
            .values()
            .map(|r| MassInput {
                tracked_bytes: r.head.tracked_bytes,
                commits_30d: r.head.commits_30d,
            })
            .collect();
        let mut moved = Vec::new();
        for ((path, repo), mass) in self.repos.iter_mut().zip(masses(&inputs)) {
            if (repo.snapshot.mass - mass).abs() >= MASS_EPSILON {
                moved.push(path.clone());
            }
            repo.snapshot.mass = mass;
        }
        moved
    }
}

/// Git facts for one repo, or why they could not be read.
pub type GitResult = std::io::Result<(Status, HeadInfo)>;

/// Tests set this to a path prefix to have repos under it loaded and read as if on a
/// Windows drive.
pub const TEST_SLOW_PREFIX: &str = "REFRACT_AGENT_TEST_SLOW_PREFIX";

/// True when `path` is under [`TEST_SLOW_PREFIX`].
pub fn under_test_slow_prefix(path: &Path) -> bool {
    std::env::var_os(TEST_SLOW_PREFIX).is_some_and(|p| !p.is_empty() && path.starts_with(p))
}

/// Reads one repo's git facts. On a Windows drive the status and the HEAD facts are
/// read at the same time: there each git call mostly waits on the file system, one
/// file at a time, so two calls take little longer than one.
pub fn read_git(path: &Path) -> GitResult {
    read_git_with(
        path,
        git::on_windows_drive(path) || under_test_slow_prefix(path),
    )
}

/// Reads one repo's git facts, the status and the HEAD facts at the same time when
/// `concurrent`, else one after the other. Either way the HEAD reads not started yet are
/// skipped once the status cannot be read. If no thread can be started, the reads run
/// one after the other.
pub fn read_git_with(path: &Path, concurrent: bool) -> GitResult {
    if concurrent {
        let cancel = AtomicBool::new(false);
        let read = std::thread::scope(|scope| {
            let head = std::thread::Builder::new()
                .name("git-head".into())
                .spawn_scoped(scope, || git::read_head_unless(path, &cancel))
                .ok()?;
            let status = git::read_status(path);
            if status.is_err() {
                cancel.store(true, Ordering::Relaxed);
            }
            let head = head
                .join()
                .unwrap_or_else(|_| Err(std::io::Error::other("reading HEAD panicked")));
            Some(status.and_then(|s| Ok((s, head?))))
        });
        if let Some(read) = read {
            return read.map(|(s, h)| finish(path, s, h));
        }
    }
    git::read_status(path)
        .and_then(|s| Ok((s, git::read_head(path)?)))
        .map(|(s, h)| finish(path, s, h))
}

/// The status and HEAD facts with what a removable worktree needs read too (see
/// [`git::finish_worktree`]).
fn finish(path: &Path, mut status: Status, head: HeadInfo) -> (Status, HeadInfo) {
    git::finish_worktree(path, &mut status, &head);
    (status, head)
}

/// Reads every repo in `paths`, several at once.
pub fn read_git_parallel(paths: &[PathBuf]) -> Vec<GitResult> {
    let workers = std::thread::available_parallelism()
        .map_or(4, |n| n.get())
        .clamp(1, 8);
    let results: Mutex<Vec<Option<GitResult>>> =
        Mutex::new((0..paths.len()).map(|_| None).collect());
    read_git_each(paths, workers, |i, result| {
        results.lock().expect("results lock")[i] = Some(result);
        true
    });
    results
        .into_inner()
        .expect("results lock")
        .into_iter()
        .map(|r| r.expect("every repo read"))
        .collect()
}

/// Reads every repo in `paths`, at most `workers` at once, in order, and hands each
/// result to `done` with its index as soon as it is read, from the thread that read it.
/// Once `done` returns false, no further repo is started. If no worker thread can be
/// started, the repos are read on this one.
pub fn read_git_each(
    paths: &[PathBuf],
    workers: usize,
    done: impl Fn(usize, GitResult) -> bool + Sync,
) {
    let workers = workers.clamp(1, paths.len().max(1));
    let next = AtomicUsize::new(0);
    let stop = AtomicBool::new(false);
    let work = || {
        while !stop.load(Ordering::Relaxed) {
            let i = next.fetch_add(1, Ordering::Relaxed);
            let Some(path) = paths.get(i) else {
                break;
            };
            if !done(i, read_git(path)) {
                stop.store(true, Ordering::Relaxed);
            }
        }
    };
    std::thread::scope(|scope| {
        let started = (0..workers)
            .take_while(|_| {
                std::thread::Builder::new()
                    .name("git".into())
                    .spawn_scoped(scope, work)
                    .is_ok()
            })
            .count();
        if started == 0 {
            work();
        }
    });
}

fn live_set(
    procs: &[ClaudeProcess],
    repo: &Path,
    sessions: &[SessionSummary],
) -> HashMap<String, Option<LiveProcess>> {
    let pairs: Vec<(&str, Option<DateTime<Utc>>)> = sessions
        .iter()
        .filter_map(|s| Some((s.session_id.as_deref()?, s.modified.or(s.last_timestamp))))
        .collect();
    procs::live_sessions(procs, &path_str(repo), &pairs)
}

/// The statuses in `hooks` of `sessions`.
fn hooks_for(
    hooks: &HashMap<String, HookReport>,
    sessions: &[SessionSummary],
) -> HashMap<String, HookReport> {
    sessions
        .iter()
        .filter_map(|s| {
            let id = s.session_id.as_ref()?;
            Some((id.clone(), *hooks.get(id)?))
        })
        .collect()
}

/// Builds a snapshot with mass 0; the caller fills in mass across repos. `hooks` holds
/// status hook statuses by session id, and `background` the number of background
/// subagents running by live session id.
#[allow(clippy::too_many_arguments)]
pub fn build_snapshot(
    path: &Path,
    status: &Status,
    head: &HeadInfo,
    sessions: &[SessionSummary],
    live: &HashMap<String, Option<LiveProcess>>,
    hooks: &HashMap<String, HookReport>,
    background: &HashMap<String, u32>,
    now: DateTime<Utc>,
) -> RepoSnapshot {
    let root = path_str(path);
    let dirty_paths: Vec<String> = status
        .paths
        .iter()
        .map(|p| format!("{}/{p}", root.trim_end_matches('/')))
        .collect();
    // Reasons and preselect come from the same candidates and facts, so they agree.
    let candidates = candidates(&root, sessions, live, hooks, background);
    let facts = repo_facts(status, head);
    let reasons = resume_reasons(&candidates, &facts);
    let preselect = preselect(&candidates, &facts, now);
    let sessions = candidates
        .iter()
        .zip(reasons)
        .map(|(c, resume_reason)| SessionInfo {
            id: c.summary.session_id.clone().unwrap_or_default(),
            state: c.state,
            last_timestamp: c.summary.last_timestamp,
            last_assistant_text: c.summary.last_assistant_text.clone(),
            byte_size: c.summary.byte_size.unwrap_or(0),
            focusable: c.focusable,
            resume_reason,
        })
        .collect();
    let (ahead, behind) = match status.ahead_behind {
        Some((a, b)) => (Some(a), Some(b)),
        None => (None, None),
    };
    RepoSnapshot {
        name: path
            .file_name()
            .map_or_else(|| root.clone(), |n| n.to_string_lossy().into_owned()),
        path: root,
        remote: head.origin_url.as_deref().and_then(RemoteId::parse),
        origin_url: head.origin_url.clone(),
        branch: status.branch.clone(),
        ahead,
        behind,
        changed_files: dirty_paths.len() as u32,
        dirty_paths,
        last_commit: head.last_commit.clone(),
        language: head.language.clone(),
        tracked_bytes: head.tracked_bytes,
        commits_30d: head.commits_30d,
        mass: 0.0,
        sessions,
        preselect,
        refactor: None,
        worktree_of: None,
        worktree: None,
    }
}

/// The snapshot part of a linked worktree's facts, with its status worked out.
fn worktree_info(facts: &WorktreeFacts, ignored: &[String]) -> WorktreeInfo {
    let (status, removable) = derive_worktree_status(facts);
    WorktreeInfo {
        locked: facts.locked,
        operation: facts.operation,
        untracked_checked: facts.untracked_checked,
        only_here: facts.only_here.unwrap_or(0),
        merged: facts.merged.clone(),
        release: facts.release.clone(),
        // Listed only while it could be removed, as the confirm is the only reader.
        ignored: if removable {
            ignored.to_vec()
        } else {
            Vec::new()
        },
        status,
        removable,
    }
}

fn session_states(
    sessions: &[SessionSummary],
    live: &HashMap<String, Option<LiveProcess>>,
    hooks: &HashMap<String, HookReport>,
    background: &HashMap<String, u32>,
) -> Vec<SessionState> {
    sessions
        .iter()
        .map(|s| {
            let id = s.session_id.as_ref();
            let is_live = id.is_some_and(|id| live.contains_key(id));
            classify_report(
                s,
                is_live,
                id.and_then(|id| hooks.get(id).copied()),
                id.and_then(|id| background.get(id).copied()).unwrap_or(0),
            )
        })
        .collect()
}

/// The repo's sessions, classified, as the preselect rule takes them.
fn candidates<'a>(
    repo: &str,
    sessions: &'a [SessionSummary],
    live: &HashMap<String, Option<LiveProcess>>,
    hooks: &HashMap<String, HookReport>,
    background: &HashMap<String, u32>,
) -> Vec<Candidate<'a>> {
    let states = session_states(sessions, live, hooks, background);
    sessions
        .iter()
        .zip(states)
        .map(|(summary, state)| Candidate {
            summary,
            state,
            focusable: is_focusable(repo, summary, state, live),
        })
        .collect()
}

/// What the preselect rule needs to know about the repo's git state.
fn repo_facts(status: &Status, head: &HeadInfo) -> RepoFacts {
    RepoFacts {
        dirty: !status.paths.is_empty(),
        head_time: head.last_commit.as_ref().map(|c| c.time),
        has_origin: head.origin_url.is_some(),
        on_branch: status.branch.is_some(),
        upstream: match (status.upstream, status.ahead_behind) {
            (false, _) => Upstream::Missing(head.off_origin),
            (true, None) => Upstream::Gone,
            (true, Some((ahead, _))) => Upstream::Ahead(ahead),
        },
    }
}

/// True when the session is live and its process is known and runs in a terminal of
/// the Refract window on exactly `repo`.
fn is_focusable(
    repo: &str,
    summary: &SessionSummary,
    state: SessionState,
    live: &HashMap<String, Option<LiveProcess>>,
) -> bool {
    matches!(state, SessionState::Live(_))
        && summary
            .session_id
            .as_ref()
            .and_then(|id| live.get(id)?.as_ref())
            .is_some_and(|p| p.in_refract_window(repo))
}

/// Events that take a client from `old` to `new`: a `sessionChanged` per changed session
/// when only sessions changed, otherwise one `repoChanged`.
pub fn diff(old: &RepoSnapshot, new: &RepoSnapshot) -> Vec<Event> {
    if old == new {
        return Vec::new();
    }
    let same_ids = old.sessions.len() == new.sessions.len()
        && old
            .sessions
            .iter()
            .zip(&new.sessions)
            .all(|(a, b)| a.id == b.id);
    let rest_same = RepoSnapshot {
        sessions: Vec::new(),
        preselect: new.preselect.clone(),
        ..old.clone()
    } == RepoSnapshot {
        sessions: Vec::new(),
        ..new.clone()
    };
    let changed: Vec<&SessionInfo> = new
        .sessions
        .iter()
        .zip(&old.sessions)
        .filter(|(n, o)| n != o)
        .map(|(n, _)| n)
        .collect();
    if same_ids && rest_same && !changed.is_empty() {
        return changed
            .into_iter()
            .map(|session| {
                Event::SessionChanged(SessionChange {
                    repo_path: new.path.clone(),
                    session: session.clone(),
                    preselect: new.preselect.clone(),
                })
            })
            .collect();
    }
    vec![Event::RepoChanged(Box::new(new.clone()))]
}

/// A path as the protocol carries it.
pub fn path_str(path: &Path) -> String {
    path.to_string_lossy().into_owned()
}

#[cfg(test)]
mod tests {
    use super::*;
    use refract_core::classify::{FinishKind, HookStatus, LiveState};
    use refract_core::preselect::Preselect;
    use std::fs;

    fn session(id: &str, state: SessionState) -> SessionInfo {
        SessionInfo {
            id: id.into(),
            state,
            last_timestamp: None,
            last_assistant_text: None,
            byte_size: 10,
            focusable: false,
            resume_reason: None,
        }
    }

    fn snapshot() -> RepoSnapshot {
        build_snapshot(
            Path::new("/home/dev/projects/tidepool-api"),
            &Status::default(),
            &HeadInfo::default(),
            &[],
            &HashMap::new(),
            &HashMap::new(),
            &HashMap::new(),
            Utc::now(),
        )
    }

    #[test]
    fn only_a_known_process_in_the_window_on_this_repo_is_focusable() {
        const REPO: &str = "/home/dev/projects/tidepool-api";
        let ids = ["a", "b", "c", "d"];
        let sessions: Vec<SessionSummary> = ids
            .iter()
            .map(|id| SessionSummary {
                session_id: Some((*id).into()),
                ..SessionSummary::default()
            })
            .collect();
        let process = |folder: &str| {
            Some(LiveProcess {
                pid: 812,
                started: None,
                refract_folder: Some(folder.into()),
            })
        };
        let live: HashMap<String, Option<LiveProcess>> = [
            ("a".to_owned(), process(REPO)),
            ("b".to_owned(), process("/home/dev/projects")),
            ("c".to_owned(), None),
        ]
        .into();
        let snapshot = build_snapshot(
            Path::new(REPO),
            &Status::default(),
            &HeadInfo::default(),
            &sessions,
            &live,
            &HashMap::new(),
            &HashMap::new(),
            Utc::now(),
        );
        let focusable: Vec<bool> = snapshot.sessions.iter().map(|s| s.focusable).collect();
        assert_eq!(focusable, [true, false, false, false]);
        assert!(matches!(snapshot.sessions[2].state, SessionState::Live(_)));
        assert_eq!(snapshot.preselect, Preselect::NewAgent);
    }

    #[test]
    fn identical_snapshots_give_no_events() {
        assert!(diff(&snapshot(), &snapshot()).is_empty());
    }

    #[test]
    fn session_only_changes_give_session_events() {
        let mut old = snapshot();
        old.sessions = vec![
            session("a", SessionState::Live(LiveState::Unknown)),
            session("b", SessionState::Finished(FinishKind::Completed)),
        ];
        let mut new = old.clone();
        new.sessions[0].state = SessionState::Finished(FinishKind::Completed);
        new.preselect = Preselect::NewAgent;
        let events = diff(&old, &new);
        assert_eq!(events.len(), 1);
        match &events[0] {
            Event::SessionChanged(change) => {
                assert_eq!(change.session.id, "a");
                assert_eq!(change.preselect, Preselect::NewAgent);
            }
            other => panic!("unexpected {other:?}"),
        }
    }

    #[test]
    fn new_sessions_or_git_changes_give_repo_events() {
        let old = snapshot();
        let mut new = old.clone();
        new.sessions = vec![session("a", SessionState::Live(LiveState::Unknown))];
        assert!(matches!(diff(&old, &new)[..], [Event::RepoChanged(_)]));
        let mut new = old.clone();
        new.changed_files = 3;
        assert!(matches!(diff(&old, &new)[..], [Event::RepoChanged(_)]));
    }

    #[test]
    fn a_refactor_change_gives_a_repo_event_even_with_a_session_change() {
        let mut old = snapshot();
        old.sessions = vec![session("a", SessionState::Live(LiveState::Unknown))];
        let mut new = old.clone();
        new.sessions[0].state = SessionState::Finished(FinishKind::Completed);
        new.refactor = Some(refract_core::protocol::RefactorInfo {
            state: refract_core::protocol::RefactorState::Starting,
            worktree: "/home/dev/projects/.refract-worktrees/tidepool-api-20261031-1405".into(),
            folder_uri: String::new(),
            branch: "refactor/20261031-1405".into(),
            target_branch: "main".into(),
            base_commit: "a1b2c3d".into(),
            session_id: String::new(),
            created_at: Utc::now(),
            live: false,
            interrupted: false,
            commits: vec![],
            files_changed: 0,
            insertions: 0,
            deletions: 0,
            checks: None,
            summary: None,
            blocked: None,
        });
        assert!(matches!(diff(&old, &new)[..], [Event::RepoChanged(_)]));
    }

    #[test]
    fn a_background_agent_starting_or_finishing_reclassifies_its_live_session() {
        let tmp = tempfile::tempdir().unwrap();
        let repo = PathBuf::from("/home/dev/projects/tidepool-api");
        let id = "5f0c2a9e-1b7d-4c3e-9a61-0d2e8f4b7c11";
        let project = tmp.path().join("-home-dev-projects-tidepool-api");
        fs::create_dir_all(&project).unwrap();
        let line = format!(
            "{{\"type\":\"user\",\"sessionId\":\"{id}\",\"cwd\":\"{}\",\
             \"timestamp\":\"2026-11-01T09:00:00Z\",\"message\":{{\"role\":\"user\",\
             \"content\":\"chart the tides\"}}}}\n",
            repo.display()
        );
        fs::write(project.join(format!("{id}.jsonl")), line).unwrap();
        let mut agent = Agent::new(Config {
            roots: vec![],
            extra: vec![],
            hidden: vec![],
            depth: 1,
            claude_projects: tmp.path().to_owned(),
            limits_file: tmp.path().join("usage-state.json"),
        });
        let sessions = agent.reader.read_repo(&path_str(&repo));
        assert_eq!(sessions.len(), 1);
        let live: HashMap<String, Option<LiveProcess>> = [(
            id.to_owned(),
            Some(LiveProcess {
                pid: 812,
                started: Some(Utc::now() - TimeDelta::minutes(1)),
                refract_folder: None,
            }),
        )]
        .into();
        let finished = HookReport {
            status: HookStatus::Finished,
            at: None,
        };
        agent.hooks.insert(id.to_owned(), finished);
        let (status, head) = (Status::default(), HeadInfo::default());
        agent.store(repo.clone(), status, head, sessions, live, HashMap::new());
        let state = |agent: &Agent| agent.repos[&repo].snapshot.sessions[0].state;
        assert_eq!(state(&agent), SessionState::Live(LiveState::Idle));
        let session_dir = project.join(id);
        assert_eq!(
            agent.live_session_dirs(),
            [(id.to_owned(), session_dir.clone())]
        );

        let meta = session_dir.join("subagents/agent-a1.meta.json");
        fs::create_dir_all(meta.parent().unwrap()).unwrap();
        let running = r#"{"agentType":"implementer","requestShape":"background"}"#;
        fs::write(&meta, running).unwrap();
        let ids: HashSet<String> = [id.to_owned()].into();
        let changed = |events: &[Event]| match events {
            [Event::SessionChanged(change)] => change.session.state,
            other => panic!("unexpected {other:?}"),
        };
        let events = agent.refresh_background(Some(&ids));
        assert_eq!(changed(&events), SessionState::Live(LiveState::Working));
        assert!(agent.refresh_background(None).is_empty());

        let done = r#"{"agentType":"implementer","requestShape":"background","finished":true}"#;
        fs::write(&meta, done).unwrap();
        let events = agent.refresh_background(Some(&ids));
        assert_eq!(changed(&events), SessionState::Live(LiveState::Idle));
    }
}
