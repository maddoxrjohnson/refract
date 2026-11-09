//! The agent's picture of every repo, and the events that keep the app in step with it.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::Mutex;

use chrono::{DateTime, TimeDelta, Utc};
use refract_core::classify::{
    classify_report, cut_off, ended_by_clear, Boot, HookReport, SessionState,
};
use refract_core::codex_ready::{readiness, CodexReadiness};
use refract_core::codex_transcript::CodexSummary;
use refract_core::mass::{masses, MassInput};
use refract_core::preselect::{
    live_agents, plan, resume_reasons, AgentFacts, Candidate, RepoFacts, Upstream,
    DEFAULT_CODEX_THRESHOLD,
};
use refract_core::protocol::{
    AgentKind, Event, MissingWorktree, RefactorInfo, RemoteId, RepoSnapshot, SessionChange,
    SessionInfo, WorktreeInfo,
};
use refract_core::sessions::{path_within, paths_match};
use refract_core::transcript::SessionSummary;
use refract_core::worktree::{derive_missing_status, derive_worktree_status, WorktreeFacts};

use crate::config::Config;
use crate::git::{self, HeadInfo, Status};
use crate::procs::{self, AgentProcess, CodexLock, HookRuns, LiveProcess, SessionTimes};
use crate::scan::{self, Instructions};
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

/// What the Codex parts of every snapshot depend on beyond the repo itself.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct CodexEnv {
    /// The Codex CLI was found (see [`crate::codex`]). While it is not, snapshots carry
    /// no readiness and nothing preselects Codex.
    pub installed: bool,
    /// The app's `codexThreshold`, in percent.
    pub threshold: f64,
    /// Claude's highest use of its plan windows, in percent, when known
    /// ([`refract_core::preselect::claude_peak`]).
    pub claude_used_percent: Option<f64>,
}

impl Default for CodexEnv {
    fn default() -> Self {
        CodexEnv {
            installed: false,
            threshold: DEFAULT_CODEX_THRESHOLD,
            claude_used_percent: None,
        }
    }
}

impl CodexEnv {
    /// Claude is at or over the threshold and Codex is installed: an agent Refract
    /// starts on its own (Make Codex-ready) runs as Codex.
    pub fn prefers_codex(&self) -> bool {
        self.installed
            && self
                .claude_used_percent
                .is_some_and(|used| used >= self.threshold)
    }
}

/// What a snapshot's Codex parts are built from.
#[derive(Debug, Clone, Copy)]
pub struct CodexInputs<'a> {
    /// The repo's Codex sessions, newest first, live ones with no rollout yet included.
    pub sessions: &'a [CodexSummary],
    pub instructions: &'a Instructions,
    pub env: CodexEnv,
}

/// One repo's raw facts and the snapshot built from them.
#[derive(Debug, Clone)]
pub struct RepoState {
    pub status: Status,
    pub head: HeadInfo,
    pub sessions: Vec<SessionSummary>,
    /// The repo's Codex rollouts, newest first.
    pub codex_sessions: Vec<CodexSummary>,
    /// Its instruction files, read again with its status.
    pub instructions: Instructions,
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
    /// answering `listRepos` or `getRepo`. The sessions' resume reasons are worked out
    /// again with it, as a cut-off one's lasts only so long, and it returns true when
    /// they changed too, as are the Codex readiness and the New row, which `env` moves.
    /// `boot` is the boot the helper runs in.
    pub fn refresh_preselect(&mut self, now: DateTime<Utc>, boot: Boot, env: CodexEnv) -> bool {
        let codex = with_empty_codex(
            &self.snapshot.path,
            &self.sessions,
            &self.codex_sessions,
            &self.live,
        );
        let candidates = candidates(
            &self.snapshot.path,
            &self.sessions,
            &codex,
            &self.live,
            &self.hooks,
            &self.background,
            boot,
            now,
        );
        let facts = repo_facts(&self.status, &self.head);
        let mut changed = false;
        for (info, reason) in self
            .snapshot
            .sessions
            .iter_mut()
            .zip(resume_reasons(&candidates, &facts))
        {
            if info.resume_reason != reason {
                info.resume_reason = reason;
                changed = true;
            }
        }
        let (codex, fresh) = codex_plan(&candidates, &facts, &self.instructions, env, now);
        if fresh.preselect != self.snapshot.preselect {
            self.snapshot.preselect = fresh.preselect;
            changed = true;
        }
        if fresh.new_agent != self.snapshot.new_agent {
            self.snapshot.new_agent = fresh.new_agent;
            changed = true;
        }
        if codex != self.snapshot.codex {
            self.snapshot.codex = codex;
            changed = true;
        }
        changed
    }

    fn wants_live_checks(&self, now: DateTime<Utc>) -> bool {
        let recent =
            |s: &SessionSummary| s.last_timestamp.is_some_and(|t| now - t < RECENT_ACTIVITY);
        !self.live.is_empty()
            || self.sessions.iter().any(recent)
            || self.codex_sessions.iter().any(|c| recent(&c.summary))
    }
}

/// Every known repo.
#[derive(Debug)]
pub struct Agent {
    config: Config,
    reader: SessionReader,
    repos: BTreeMap<PathBuf, RepoState>,
    procs: Vec<AgentProcess>,
    /// The Codex thread locks held open, from the same look at `/proc` as `procs`.
    codex_locks: Vec<CodexLock>,
    /// What the Codex parts of the snapshots depend on.
    codex: CodexEnv,
    /// Every status the status hook reported, by session id.
    hooks: HashMap<String, HookReport>,
    /// What `hooks` says about `procs`, worked out again when either changes.
    hook_runs: HookRuns,
    /// Each repo's refactor, by repo path, as [`crate::refactor`] last worked it out.
    refactors: HashMap<PathBuf, RefactorInfo>,
    /// What live sessions' transcripts said about their background work.
    subagents: subagents::Tracker,
    /// The boot the helper runs in, read once: a session whose last hook report is from
    /// another one was cut off by a restart.
    boot: Boot,
}

impl Agent {
    /// An agent with no repos loaded yet.
    pub fn new(config: Config) -> Agent {
        let reader = SessionReader::new(config.claude_projects.clone())
            .with_codex(config.codex_home.join("sessions"));
        Agent {
            config,
            reader,
            repos: BTreeMap::new(),
            procs: Vec::new(),
            codex_locks: Vec::new(),
            codex: CodexEnv::default(),
            hooks: HashMap::new(),
            hook_runs: HookRuns::default(),
            refactors: HashMap::new(),
            subagents: subagents::Tracker::default(),
            boot: procs::current_boot(),
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

    /// Forgets the repo at `path`, as when git dropped its worktree but its folder is
    /// still there: the event that says so, when it was known.
    pub fn drop_repo(&mut self, path: &Path) -> Option<Event> {
        self.repos.remove(path)?;
        Some(Event::RepoRemoved {
            path: path_str(path),
        })
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
        self.scan_procs();
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
        self.scan_procs();
        let sessions = self.read_sessions(path);
        let codex = self.read_codex(path);
        self.live_set(path, &sessions, &codex).remove(id)?
    }

    /// What the Codex parts of the snapshots depend on now.
    pub fn codex_env(&self) -> CodexEnv {
        self.codex
    }

    /// Takes in what the Codex parts of the snapshots depend on, and returns the repos
    /// whose snapshot changed with it.
    pub fn set_codex_env(&mut self, env: CodexEnv, now: DateTime<Utc>) -> Vec<PathBuf> {
        if env == self.codex {
            return Vec::new();
        }
        self.codex = env;
        self.refresh_preselects(now)
    }

    /// Lists the Codex rollouts again after a watch said they changed, and returns the
    /// repos whose rollouts were added, written or removed.
    pub fn codex_rollouts_changed(&mut self) -> Vec<PathBuf> {
        self.reader.codex_changed();
        let cwds = self.reader.relist_codex();
        self.repos
            .keys()
            .filter(|p| {
                let repo = path_str(p);
                cwds.iter().any(|cwd| paths_match(cwd, &repo))
            })
            .cloned()
            .collect()
    }

    /// The repos a Codex thread lock appearing or going may concern, after a fresh look
    /// at `/proc`: those with a `codex` process in their folder or a live Codex session.
    pub fn codex_lock_repos(&mut self) -> Vec<PathBuf> {
        self.scan_procs();
        let folders: Vec<&str> = self
            .procs
            .iter()
            .filter(|p| p.agent == AgentKind::Codex)
            .map(|p| p.cwd.as_str())
            .collect();
        self.repos
            .iter()
            .filter(|(path, repo)| {
                let root = path_str(path);
                folders.iter().any(|cwd| paths_match(cwd, &root))
                    || repo.snapshot.sessions.iter().any(|s| {
                        s.agent == AgentKind::Codex && matches!(s.state, SessionState::Live(_))
                    })
            })
            .map(|(p, _)| p.clone())
            .collect()
    }

    /// All snapshots, sorted by path.
    pub fn snapshots(&self) -> Vec<RepoSnapshot> {
        self.repos.values().map(|r| r.snapshot.clone()).collect()
    }

    /// Re-reads parts of one repo and returns the events describing what changed.
    pub fn refresh(&mut self, path: &Path, what: Refresh) -> Vec<Event> {
        self.refresh_reading(path, what, true)
    }

    /// Re-reads the status of the worktree at `path`, with its ignored entries read again
    /// rather than kept from the last read (see [`git::finish_worktree_from`]), and
    /// returns the events describing what changed: after a removal was refused for new
    /// ignored files, so the next confirm lists them.
    pub fn refresh_ignored(&mut self, path: &Path) -> Vec<Event> {
        let what = Refresh {
            status: true,
            head: false,
            sessions: false,
        };
        self.refresh_reading(path, what, false)
    }

    /// [`Agent::refresh`]; `keep_ignored` takes the ignored entries from the last read
    /// when neither the status nor HEAD changed.
    fn refresh_reading(&mut self, path: &Path, what: Refresh, keep_ignored: bool) -> Vec<Event> {
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
        if (what.status || what.head) && keep_ignored {
            git::finish_worktree_from(path, &mut status, &head, (&old.status, &old.head));
        } else if what.status || what.head {
            git::finish_worktree(path, &mut status, &head);
        }
        let old_snapshot = old.snapshot.clone();
        let instructions = if what.status || what.head {
            scan::read_instructions(path)
        } else {
            old.instructions.clone()
        };
        let old_codex = old.codex_sessions.clone();
        if what.sessions {
            self.scan_procs();
        }
        let (sessions, codex, live, background) = if what.sessions {
            let sessions = self.read_sessions(path);
            let codex = self.read_codex(path);
            let live = self.live_set(path, &sessions, &codex);
            let background = self.background_for(path, &live);
            (sessions, codex, live, background)
        } else {
            let old = &self.repos[path];
            (
                old.sessions.clone(),
                old_codex,
                old.live.clone(),
                old.background.clone(),
            )
        };
        self.store(
            path.to_owned(),
            status,
            head,
            (sessions, codex),
            live,
            background,
            instructions,
        );
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
        // Main checkouts whose worktrees come or go: their missing worktrees change.
        let mut mains: Vec<PathBuf> = removed
            .iter()
            .chain(&replaced)
            .filter_map(|p| Some(self.repos.get(p)?.status.worktree.as_ref()?.main.clone()))
            .collect();
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
            self.scan_procs();
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
            mains.extend(
                inserted.iter().filter_map(|p| {
                    Some(self.repos.get(p)?.status.worktree.as_ref()?.main.clone())
                }),
            );
            for main in mains {
                let main = self.known_form(&main);
                if self.reread_missing(&main) {
                    moved.push(main);
                }
            }
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
        let (boot, env) = (self.boot, self.codex);
        self.repos
            .iter_mut()
            .filter_map(|(path, repo)| repo.refresh_preselect(now, boot, env).then(|| path.clone()))
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

    /// Scans `/proc` for agent processes and Codex locks again, and works out what the
    /// hook says about them.
    fn scan_procs(&mut self) {
        let scan = procs::scan_all();
        self.procs = scan.procs;
        self.codex_locks = scan.codex_locks;
        self.hook_runs = HookRuns::new(&self.procs, &self.hooks);
    }

    /// Which of the repo at `repo`'s sessions are live (see [`procs::live_sessions`] and
    /// [`procs::codex_live`]), from the processes last scanned.
    fn live_set(
        &self,
        repo: &Path,
        sessions: &[SessionSummary],
        codex: &[CodexSummary],
    ) -> HashMap<String, Option<LiveProcess>> {
        let root = path_str(repo);
        let mut live = procs::live_sessions(
            &self.procs,
            &root,
            &session_times(sessions),
            &self.hook_runs,
        );
        let ids: Vec<&str> = codex
            .iter()
            .filter_map(|c| c.summary.session_id.as_deref())
            .collect();
        live.extend(procs::codex_live(
            &self.procs,
            &self.codex_locks,
            &root,
            &ids,
            |id| self.reader.has_rollout(id),
        ));
        live
    }

    /// Takes in the status hook's statuses and returns events for the sessions whose
    /// state changed with them. The pids in them say which session a process runs (see
    /// [`procs::live_sessions`]), so which sessions are live is worked out again, from
    /// the processes last scanned.
    pub fn set_hooks(&mut self, hooks: HashMap<String, HookReport>) -> Vec<Event> {
        self.hooks = hooks;
        self.hook_runs = HookRuns::new(&self.procs, &self.hooks);
        let stale: Vec<(PathBuf, HashMap<String, Option<LiveProcess>>)> = self
            .repos
            .iter()
            .filter_map(|(p, r)| {
                let live = self.live_set(p, &r.sessions, &r.codex_sessions);
                (hooks_for(&self.hooks, &r.sessions) != r.hooks || live != r.live)
                    .then(|| (p.clone(), live))
            })
            .collect();
        let mut events = Vec::new();
        for (path, live) in stale {
            let repo = &self.repos[&path];
            let old = repo.snapshot.clone();
            let (status, head, sessions, instructions) = (
                repo.status.clone(),
                repo.head.clone(),
                (repo.sessions.clone(), repo.codex_sessions.clone()),
                repo.instructions.clone(),
            );
            let background = if live == repo.live {
                repo.background.clone()
            } else {
                self.background_for(&path, &live)
            };
            self.store(
                path.clone(),
                status,
                head,
                sessions,
                live,
                background,
                instructions,
            );
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
        self.scan_procs();
        let watched: Vec<PathBuf> = self
            .repos
            .iter()
            .filter(|(_, r)| r.wants_live_checks(now))
            .map(|(p, _)| p.clone())
            .collect();
        let mut events = Vec::new();
        for path in watched {
            let repo = &self.repos[&path];
            let live = self.live_set(&path, &repo.sessions, &repo.codex_sessions);
            if live == repo.live {
                continue;
            }
            let old = repo.snapshot.clone();
            let (status, head, sessions, instructions) = (
                repo.status.clone(),
                repo.head.clone(),
                (repo.sessions.clone(), repo.codex_sessions.clone()),
                repo.instructions.clone(),
            );
            let background = self.background_for(&path, &live);
            self.store(
                path.clone(),
                status,
                head,
                sessions,
                live,
                background,
                instructions,
            );
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
            let (status, head, sessions, instructions) = (
                repo.status.clone(),
                repo.head.clone(),
                (repo.sessions.clone(), repo.codex_sessions.clone()),
                repo.instructions.clone(),
            );
            self.store(
                path.clone(),
                status,
                head,
                sessions,
                live,
                background,
                instructions,
            );
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
        procs: &[AgentProcess],
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
        let codex = self.read_codex(&path);
        let live = self.live_set(&path, &sessions, &codex);
        let background = self.background_for(&path, &live);
        let instructions = scan::read_instructions(&path);
        self.store(
            path,
            status,
            head,
            (sessions, codex),
            live,
            background,
            instructions,
        );
    }

    /// Builds and keeps the repo's snapshot. `sessions` are its Claude and its Codex
    /// sessions.
    #[allow(clippy::too_many_arguments)]
    fn store(
        &mut self,
        path: PathBuf,
        status: Status,
        head: HeadInfo,
        sessions: (Vec<SessionSummary>, Vec<CodexSummary>),
        live: HashMap<String, Option<LiveProcess>>,
        background: HashMap<String, u32>,
        instructions: Instructions,
    ) {
        let (sessions, codex_sessions) = sessions;
        let old = self.repos.get(&path);
        let mass = old.map_or(0.0, |r| r.snapshot.mass);
        let id = old.map_or_else(|| scan::repo_id(&path), |r| r.id);
        let hooks = hooks_for(&self.hooks, &sessions);
        let codex = with_empty_codex(&path_str(&path), &sessions, &codex_sessions, &live);
        let mut snapshot = build_snapshot_with(
            &path,
            &status,
            &head,
            &sessions,
            &live,
            &hooks,
            &background,
            Utc::now(),
            self.boot,
            &CodexInputs {
                sessions: &codex,
                instructions: &instructions,
                env: self.codex,
            },
        );
        snapshot.mass = mass;
        snapshot.refactor = self.refactors.get(&path).cloned();
        self.set_worktree(&path, &status, &head, &mut snapshot);
        self.set_missing(&head, &mut snapshot);
        self.repos.insert(
            path,
            RepoState {
                status,
                head,
                sessions,
                codex_sessions,
                instructions,
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

    /// Sets the missing worktrees of `snapshot` from `head`, the HEAD facts of a main
    /// checkout: each with its status (spec §10.2), moved when a known repo's git dir is
    /// its admin dir. Those of Refract's refactors are left out.
    fn set_missing(&self, head: &HeadInfo, snapshot: &mut RepoSnapshot) {
        let refactors: Vec<&str> = self
            .refactors
            .values()
            .map(|r| r.worktree.as_str())
            .collect();
        // Only a linked worktree's git dir can be an admin dir.
        let linked: Vec<&PathBuf> = self
            .repos
            .iter()
            .filter(|(_, r)| r.status.worktree.is_some())
            .map(|(p, _)| p)
            .collect();
        snapshot.missing_worktrees = head
            .missing
            .iter()
            .filter(|m| {
                let path = path_str(&m.path);
                !refactors.iter().any(|w| paths_match(w, &path))
            })
            .map(|m| {
                let mut facts = m.facts.clone();
                facts.moved_to =
                    git::moved_to(&m.admin, linked.iter().copied()).map(|p| path_str(&p));
                let (status, cleanable) = derive_missing_status(&facts);
                MissingWorktree {
                    path: path_str(&m.path),
                    branch: m.branch.clone(),
                    status,
                    cleanable,
                }
            })
            .collect();
    }

    /// Reads the missing worktrees of the main checkout at `main` again, when it is
    /// known, as when one of its tracked worktrees was dropped or added. Returns whether
    /// its snapshot changed.
    fn reread_missing(&mut self, main: &Path) -> bool {
        let Some(repo) = self.repos.get(main) else {
            return false;
        };
        let missing = git::read_missing(main);
        if missing == repo.head.missing {
            return false;
        }
        let mut head = repo.head.clone();
        head.missing = missing;
        let mut snapshot = repo.snapshot.clone();
        self.set_missing(&head, &mut snapshot);
        let repo = self.repos.get_mut(main).expect("known repo");
        repo.head = head;
        let changed = snapshot != repo.snapshot;
        repo.snapshot = snapshot;
        changed
    }

    /// Sets the worktree parts of every snapshot again, once a batch of repos is in: a
    /// worktree's main repo, or a repo inside it, or the repo a missing worktree moved
    /// to, may have come in after it. Returns the repos whose snapshot changed.
    fn resolve_worktrees(&mut self) -> Vec<PathBuf> {
        let paths: Vec<PathBuf> = self
            .repos
            .iter()
            .filter(|(_, r)| r.status.worktree.is_some() || !r.head.missing.is_empty())
            .map(|(p, _)| p.clone())
            .collect();
        let mut changed = Vec::new();
        for path in paths {
            let repo = &self.repos[&path];
            let mut snapshot = repo.snapshot.clone();
            self.set_worktree(&path, &repo.status, &repo.head, &mut snapshot);
            self.set_missing(&repo.head, &mut snapshot);
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

    /// Reads the Codex sessions that started in `repo`.
    fn read_codex(&mut self, repo: &Path) -> Vec<CodexSummary> {
        self.reader.read_codex_repo(&path_str(repo))
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

/// What [`procs::live_sessions`] needs of `sessions`.
pub fn session_times(sessions: &[SessionSummary]) -> Vec<SessionTimes<'_>> {
    sessions
        .iter()
        .filter_map(|s| {
            Some(SessionTimes {
                id: s.session_id.as_deref()?,
                modified: s.modified.or(s.last_timestamp),
                first: s.first_timestamp,
            })
        })
        .collect()
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
/// status hook statuses by session id, `background` the number of background
/// subagents running by live session id, and `boot` the boot the helper runs in. Codex
/// is taken as not installed (see [`build_snapshot_with`]).
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
    boot: Boot,
) -> RepoSnapshot {
    let instructions = Instructions::default();
    let codex = CodexInputs {
        sessions: &[],
        instructions: &instructions,
        env: CodexEnv::default(),
    };
    build_snapshot_with(
        path, status, head, sessions, live, hooks, background, now, boot, &codex,
    )
}

/// [`build_snapshot`] with the repo's Codex sessions among its sessions, its Codex
/// readiness (`None` while Codex is not installed) and the New row the preselect means.
#[allow(clippy::too_many_arguments)]
pub fn build_snapshot_with(
    path: &Path,
    status: &Status,
    head: &HeadInfo,
    sessions: &[SessionSummary],
    live: &HashMap<String, Option<LiveProcess>>,
    hooks: &HashMap<String, HookReport>,
    background: &HashMap<String, u32>,
    now: DateTime<Utc>,
    boot: Boot,
    codex: &CodexInputs<'_>,
) -> RepoSnapshot {
    let root = path_str(path);
    let dirty_paths: Vec<String> = status
        .paths
        .iter()
        .map(|p| format!("{}/{p}", root.trim_end_matches('/')))
        .collect();
    // Reasons and preselect come from the same candidates and facts, so they agree.
    let candidates = candidates(
        &root,
        sessions,
        codex.sessions,
        live,
        hooks,
        background,
        boot,
        now,
    );
    let facts = repo_facts(status, head);
    let reasons = resume_reasons(&candidates, &facts);
    let (readiness, plan) = codex_plan(&candidates, &facts, codex.instructions, codex.env, now);
    let sessions = candidates
        .iter()
        .zip(reasons)
        .map(|(c, resume_reason)| SessionInfo {
            id: c.summary.session_id.clone().unwrap_or_default(),
            agent: c.agent,
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
        codex: readiness,
        new_agent: plan.new_agent,
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
        preselect: plan.preselect,
        refactor: None,
        worktree_of: None,
        worktree: None,
        missing_worktrees: Vec::new(),
    }
}

/// The snapshot part of a linked worktree's facts, with its status worked out.
fn worktree_info(facts: &WorktreeFacts, ignored: &[String]) -> WorktreeInfo {
    let (status, removable, checkable) = derive_worktree_status(facts);
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
        checkable,
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

/// The repo's Codex readiness, when Codex is installed, and the preselect and New row
/// with the hand-back and nudge rules (codex spec §5).
fn codex_plan(
    candidates: &[Candidate<'_>],
    facts: &RepoFacts,
    instructions: &Instructions,
    env: CodexEnv,
    now: DateTime<Utc>,
) -> (Option<CodexReadiness>, refract_core::preselect::Plan) {
    let ready = env.installed.then(|| {
        let notes = instructions.note_refs();
        readiness(&instructions.files(&notes), &live_agents(candidates))
    });
    let agents = AgentFacts {
        claude_used_percent: env.claude_used_percent,
        codex_threshold: env.threshold,
        codex: ready.as_ref().map(|r| r.state),
    };
    let plan = plan(candidates, facts, &agents, now);
    (ready, plan)
}

/// `codex`, with an empty summary added for each live Codex session in `live` that has
/// no rollout yet (it had no message), so it shows as it runs.
fn with_empty_codex(
    repo: &str,
    sessions: &[SessionSummary],
    codex: &[CodexSummary],
    live: &HashMap<String, Option<LiveProcess>>,
) -> Vec<CodexSummary> {
    let known = |id: &str| {
        sessions
            .iter()
            .map(|s| &s.session_id)
            .chain(codex.iter().map(|c| &c.summary.session_id))
            .any(|s| s.as_deref() == Some(id))
    };
    let mut empty: Vec<CodexSummary> = live
        .iter()
        .filter(|(id, _)| !known(id))
        .map(|(id, process)| {
            let started = process.as_ref().and_then(|p| p.started);
            let mut summary = CodexSummary::default();
            summary.summary.session_id = Some(id.clone());
            summary.summary.cwd = Some(repo.to_owned());
            summary.summary.last_timestamp = started;
            summary.summary.modified = started;
            summary.head.session_id = Some(id.clone());
            summary.head.cwd = Some(repo.to_owned());
            summary
        })
        .collect();
    if empty.is_empty() {
        return codex.to_vec();
    }
    empty.sort_by(|a, b| a.summary.session_id.cmp(&b.summary.session_id));
    let mut all = codex.to_vec();
    all.extend(empty);
    all.sort_by_key(|c| std::cmp::Reverse(c.summary.last_timestamp));
    all
}

/// The repo's sessions of both agents, classified, newest first, as the preselect rule
/// takes them. `boot`, the boot the helper runs in, and `now` tell which Claude sessions
/// a restart cut off. A Codex session's state comes from its rollout
/// ([`CodexSummary::state`]); Codex has no status hook or background count yet.
#[allow(clippy::too_many_arguments)]
fn candidates<'a>(
    repo: &str,
    sessions: &'a [SessionSummary],
    codex: &'a [CodexSummary],
    live: &HashMap<String, Option<LiveProcess>>,
    hooks: &HashMap<String, HookReport>,
    background: &HashMap<String, u32>,
    boot: Boot,
    now: DateTime<Utc>,
) -> Vec<Candidate<'a>> {
    let states = session_states(sessions, live, hooks, background);
    let mut all: Vec<Candidate<'a>> = sessions
        .iter()
        .zip(states)
        .map(|(summary, state)| {
            let id = summary.session_id.as_ref();
            let stopped = !matches!(state, SessionState::Live(_));
            let hook = id.and_then(|id| hooks.get(id).copied());
            Candidate {
                agent: AgentKind::Claude,
                summary,
                state,
                focusable: is_focusable(repo, summary, state, live),
                cleared: stopped && ended_by_clear(summary, hook),
                cut_off: stopped && cut_off(hook, boot, now.timestamp()),
            }
        })
        .collect();
    if codex.is_empty() {
        return all;
    }
    all.extend(codex.iter().map(|c| {
        let summary = &c.summary;
        let is_live = summary
            .session_id
            .as_ref()
            .is_some_and(|id| live.contains_key(id));
        let state = c.state(is_live);
        Candidate {
            agent: AgentKind::Codex,
            summary,
            state,
            focusable: is_focusable(repo, summary, state, live),
            cleared: false,
            cut_off: false,
        }
    }));
    // Stable, so each agent's own order stays as it was.
    all.sort_by_key(|c| std::cmp::Reverse(c.summary.last_timestamp));
    all
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
        new_agent: new.new_agent.clone(),
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
                    new_agent: new.new_agent.clone(),
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
    use refract_core::preselect::{Preselect, ResumeReason};
    use refract_core::transcript::Tail;
    use std::fs;

    fn session(id: &str, state: SessionState) -> SessionInfo {
        SessionInfo {
            agent: AgentKind::Claude,
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
            Boot::default(),
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
            Boot::default(),
        );
        let focusable: Vec<bool> = snapshot.sessions.iter().map(|s| s.focusable).collect();
        assert_eq!(focusable, [true, false, false, false]);
        assert!(matches!(snapshot.sessions[2].state, SessionState::Live(_)));
        assert_eq!(snapshot.preselect, Preselect::NewAgent);
    }

    /// A snapshot of a repo with Claude session "a", live with `hook` as its status
    /// hook report, and a stopped Codex session, as [`build_snapshot_with`] builds it.
    fn codex_snapshot(env: CodexEnv, hook: HookStatus) -> RepoSnapshot {
        const REPO: &str = "/home/dev/projects/tidepool-api";
        let now: DateTime<Utc> = "2026-11-08T18:00:00Z".parse().unwrap();
        let at = |m: i64| Some(now - TimeDelta::minutes(m));
        let sessions = vec![SessionSummary {
            session_id: Some("a".into()),
            last_timestamp: at(1),
            ..SessionSummary::default()
        }];
        let mut codex = CodexSummary::default();
        codex.summary.session_id = Some("c".into());
        codex.summary.last_timestamp = at(30);
        let live: HashMap<String, Option<LiveProcess>> = [("a".to_owned(), None)].into();
        let hooks: HashMap<String, HookReport> =
            [("a".to_owned(), HookReport::new(hook, Some(now.timestamp())))].into();
        let instructions = Instructions {
            agents_md: Some("# Tidepool\n\nRun the tests.\n".into()),
            claude_md: Some("@AGENTS.md\n".into()),
            ..Instructions::default()
        };
        build_snapshot_with(
            Path::new(REPO),
            &Status::default(),
            &HeadInfo::default(),
            &sessions,
            &live,
            &hooks,
            &HashMap::new(),
            now,
            Boot::default(),
            &CodexInputs {
                sessions: std::slice::from_ref(&codex),
                instructions: &instructions,
                env,
            },
        )
    }

    #[test]
    fn readiness_is_reported_only_while_codex_is_installed() {
        use refract_core::codex_ready::{Gate, ReadyState};
        let installed = CodexEnv {
            installed: true,
            ..CodexEnv::default()
        };
        let snapshot = codex_snapshot(installed, HookStatus::Working);
        let codex = snapshot.codex.as_ref().expect("readiness");
        // Claude is working: Codex is busy, and Claude's own row is open.
        assert_eq!(codex.state, ReadyState::Busy);
        assert!(matches!(&codex.gates.codex, Gate::Busy(l) if l.session_id == "a"));
        assert!(codex.gates.claude.is_open());
        // Both agents' sessions are listed, tagged, newest first.
        let tagged: Vec<(&str, AgentKind)> = snapshot
            .sessions
            .iter()
            .map(|s| (s.id.as_str(), s.agent))
            .collect();
        assert_eq!(tagged, [("a", AgentKind::Claude), ("c", AgentKind::Codex)]);
        let idle = codex_snapshot(installed, HookStatus::Finished);
        let codex = idle.codex.as_ref().unwrap();
        assert_eq!(codex.state, ReadyState::Ready);
        assert_eq!(
            codex.gates.codex,
            Gate::StopIdle {
                session_ids: vec!["a".into()]
            }
        );
        assert!(codex_snapshot(CodexEnv::default(), HookStatus::Working)
            .codex
            .is_none());
    }

    #[test]
    fn a_live_session_moving_a_codex_gate_gives_a_repo_event() {
        let installed = CodexEnv {
            installed: true,
            ..CodexEnv::default()
        };
        let working = codex_snapshot(installed, HookStatus::Working);
        let idle = codex_snapshot(installed, HookStatus::Finished);
        // The app reads the gates only from `codex`, which `sessionChanged` lacks.
        match &diff(&working, &idle)[..] {
            [Event::RepoChanged(repo)] => assert_eq!(repo.codex, idle.codex),
            other => panic!("unexpected {other:?}"),
        }
        // Without Codex, the same change is a session event, as before.
        let working = codex_snapshot(CodexEnv::default(), HookStatus::Working);
        let idle = codex_snapshot(CodexEnv::default(), HookStatus::Finished);
        let events = diff(&working, &idle);
        assert!(
            matches!(&events[..], [Event::SessionChanged(_)]),
            "{events:?}"
        );
    }

    #[test]
    fn the_nudge_preselects_codex_when_claude_is_over_the_threshold() {
        use refract_core::preselect::NewAgentReason;
        let near = CodexEnv {
            installed: true,
            threshold: 90.0,
            claude_used_percent: Some(97.4),
        };
        // With Claude idle, Codex is launchable and the nudge applies.
        let snapshot = codex_snapshot(near, HookStatus::Finished);
        assert_eq!(snapshot.new_agent.agent, AgentKind::Codex);
        assert_eq!(
            snapshot.new_agent.reason,
            Some(NewAgentReason::Nudge { used_percent: 97 })
        );
        assert!(near.prefers_codex());
        let below = CodexEnv {
            claude_used_percent: Some(80.0),
            ..near
        };
        assert!(!below.prefers_codex());
        assert_eq!(
            codex_snapshot(below, HookStatus::Finished).new_agent.agent,
            AgentKind::Claude
        );
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
            codex_home: tmp.path().join("codex"),
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
        let finished = HookReport::new(HookStatus::Finished, None);
        agent.hooks.insert(id.to_owned(), finished);
        let (status, head) = (Status::default(), HeadInfo::default());
        agent.store(
            repo.clone(),
            status,
            head,
            (sessions, Vec::new()),
            live,
            HashMap::new(),
            Instructions::default(),
        );
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

    #[test]
    fn a_session_ended_by_clear_is_not_offered_to_resume() {
        const REPO: &str = "/home/dev/projects/tidepool-api";
        let id = "5f0c2a9e-1b7d-4c3e-9a61-0d2e8f4b7c11";
        let last = Utc::now() - TimeDelta::minutes(5);
        // The prompt was cut off by a shutdown, then `claude --resume` and /clear.
        let sessions = [SessionSummary {
            session_id: Some(id.into()),
            last_timestamp: Some(last),
            entry_count: 2,
            tail: Some(Tail::UserPrompt),
            ..SessionSummary::default()
        }];
        let build = |hooks: &HashMap<String, HookReport>| {
            build_snapshot(
                Path::new(REPO),
                &Status::default(),
                &HeadInfo::default(),
                &sessions,
                &HashMap::new(),
                hooks,
                &HashMap::new(),
                Utc::now(),
                Boot::default(),
            )
        };
        let interrupted = build(&HashMap::new());
        assert!(interrupted.sessions[0].state.is_unfinished());
        assert!(interrupted.sessions[0].resume_reason.is_some());
        assert_eq!(interrupted.preselect, Preselect::Resume(id.into()));

        let mut cleared = HookReport::new(HookStatus::Ended, Some(Utc::now().timestamp()));
        cleared.cleared = true;
        let snapshot = build(&[(id.to_owned(), cleared)].into());
        let session = &snapshot.sessions[0];
        assert_eq!(session.state, SessionState::Finished(FinishKind::Completed));
        assert_eq!(session.resume_reason, None);
        assert_ne!(snapshot.preselect, Preselect::Resume(id.into()));

        // In a dirty repo it is not the last conversation either, so it gets no
        // uncommitted-changes reason.
        let dirty = Status {
            paths: vec!["src/tides.rs".into()],
            ..Status::default()
        };
        let build_dirty = |hooks: &HashMap<String, HookReport>| {
            build_snapshot(
                Path::new(REPO),
                &dirty,
                &HeadInfo::default(),
                &sessions,
                &HashMap::new(),
                hooks,
                &HashMap::new(),
                Utc::now(),
                Boot::default(),
            )
        };
        let snapshot = build_dirty(&[(id.to_owned(), cleared)].into());
        assert_eq!(snapshot.sessions[0].resume_reason, None);
        assert_ne!(snapshot.preselect, Preselect::Resume(id.into()));
        let mut finished = sessions[0].clone();
        finished.tail = Some(Tail::AssistantReply);
        let unclear = build_snapshot(
            Path::new(REPO),
            &dirty,
            &HeadInfo::default(),
            std::slice::from_ref(&finished),
            &HashMap::new(),
            &HashMap::new(),
            &HashMap::new(),
            Utc::now(),
            Boot::default(),
        );
        assert!(unclear.sessions[0].resume_reason.is_some());
        let snapshot = build_snapshot(
            Path::new(REPO),
            &dirty,
            &HeadInfo::default(),
            std::slice::from_ref(&finished),
            &HashMap::new(),
            &[(id.to_owned(), cleared)].into(),
            &HashMap::new(),
            Utc::now(),
            Boot::default(),
        );
        assert_eq!(snapshot.sessions[0].resume_reason, None);
        assert_ne!(snapshot.preselect, Preselect::Resume(id.into()));
    }

    /// A report of `status` written at `at` in boot `id` with PID 1 started at `init`.
    fn report_in(status: HookStatus, at: DateTime<Utc>, id: u128, init: u64) -> HookReport {
        let mut report = HookReport::new(status, Some(at.timestamp()));
        report.boot = id;
        report.init = init;
        report
    }

    #[test]
    fn a_session_cut_off_by_a_restart_is_offered_to_resume() {
        const REPO: &str = "/home/dev/projects/tidepool-api";
        let id = "5f0c2a9e-1b7d-4c3e-9a61-0d2e8f4b7c11";
        let now = Utc::now();
        let boot = Boot {
            id: 0x5346b8e5_5fc0_4caf_b9ca_2e442e237168,
            init: 5068,
            btime: Some((now - TimeDelta::hours(1)).timestamp()),
        };
        // Idle at the prompt when the machine went down: the turn finished.
        let sessions = [SessionSummary {
            session_id: Some(id.into()),
            last_timestamp: Some(now - TimeDelta::hours(2)),
            entry_count: 2,
            tail: Some(Tail::AssistantReply),
            ..SessionSummary::default()
        }];
        let build = |live: &HashMap<String, Option<LiveProcess>>, report: Option<HookReport>| {
            let hooks: HashMap<String, HookReport> =
                report.map(|r| (id.to_owned(), r)).into_iter().collect();
            build_snapshot(
                Path::new(REPO),
                &Status::default(),
                &HeadInfo::default(),
                &sessions,
                live,
                &hooks,
                &HashMap::new(),
                now,
                boot,
            )
        };
        let before = now - TimeDelta::hours(2);
        let earlier = |status| Some(report_in(status, before, boot.id + 1, boot.init));
        let stopped = HashMap::new();

        let snapshot = build(&stopped, earlier(HookStatus::Finished));
        let session = &snapshot.sessions[0];
        assert_eq!(session.resume_reason, Some(ResumeReason::CutOff));
        // No chip: it is still finished.
        assert_eq!(session.state, SessionState::Finished(FinishKind::Completed));
        assert_eq!(snapshot.preselect, Preselect::Resume(id.into()));

        let reason = |live, report| build(live, report).sessions[0].resume_reason;
        // The distro restarted inside the same VM.
        let restarted = report_in(HookStatus::Finished, before, boot.id, 9000);
        assert_eq!(
            reason(&stopped, Some(restarted)),
            Some(ResumeReason::CutOff)
        );
        // This boot, though the clock says before it: resumed since, or the clock moved.
        let this_boot = report_in(HookStatus::Finished, before, boot.id, boot.init);
        assert_eq!(reason(&stopped, Some(this_boot)), None);
        // Closed before the restart.
        assert_eq!(reason(&stopped, earlier(HookStatus::Ended)), None);
        // Running.
        let live = [(id.to_owned(), None)].into();
        assert_eq!(reason(&live, earlier(HookStatus::Finished)), None);
        // Older than the 7 days reports are kept.
        let old = report_in(
            HookStatus::Finished,
            now - TimeDelta::days(8),
            boot.id + 1,
            1,
        );
        assert_eq!(reason(&stopped, Some(old)), None);
        // A script from before boot ids: its time against the boot time.
        let untagged = |at| Some(HookReport::new(HookStatus::Finished, Some(at)));
        let at = before.timestamp();
        assert_eq!(reason(&stopped, untagged(at)), Some(ResumeReason::CutOff));
        let after = (now - TimeDelta::minutes(30)).timestamp();
        assert_eq!(reason(&stopped, untagged(after)), None);
        // No hook.
        assert_eq!(reason(&stopped, None), None);
        assert_eq!(build(&stopped, None).preselect, Preselect::NoPicker);
    }

    #[test]
    fn the_agent_offers_a_cut_off_session_until_its_report_is_a_week_old() {
        let tmp = tempfile::tempdir().unwrap();
        let repo = PathBuf::from("/home/dev/projects/tidepool-api");
        let id = "5f0c2a9e-1b7d-4c3e-9a61-0d2e8f4b7c11";
        let mut agent = Agent::new(Config {
            roots: vec![],
            extra: vec![],
            hidden: vec![],
            depth: 1,
            claude_projects: tmp.path().to_owned(),
            limits_file: tmp.path().join("usage-state.json"),
            codex_home: tmp.path().join("codex"),
        });
        let now = Utc::now();
        agent.boot = Boot {
            id: 0x5346b8e5_5fc0_4caf_b9ca_2e442e237168,
            init: 5068,
            btime: Some((now - TimeDelta::hours(1)).timestamp()),
        };
        let ended = now - TimeDelta::days(3);
        let report = report_in(HookStatus::Finished, ended, agent.boot.id + 1, 5068);
        agent.hooks.insert(id.to_owned(), report);
        let sessions = vec![SessionSummary {
            session_id: Some(id.into()),
            last_timestamp: Some(ended),
            entry_count: 2,
            tail: Some(Tail::AssistantReply),
            ..SessionSummary::default()
        }];
        // Dirty, without commits: once the cut-off lapses the changes are its reason.
        let dirty = Status {
            paths: vec!["src/tides.rs".into()],
            ..Status::default()
        };
        let head = HeadInfo::default();
        agent.store(
            repo.clone(),
            dirty,
            head,
            (sessions, Vec::new()),
            HashMap::new(),
            HashMap::new(),
            Instructions::default(),
        );
        let reason = |agent: &Agent| agent.repos[&repo].snapshot.sessions[0].resume_reason;
        assert_eq!(reason(&agent), Some(ResumeReason::CutOff));
        // Too old to preselect either way.
        assert_eq!(agent.repos[&repo].snapshot.preselect, Preselect::NewAgent);
        assert!(agent.refresh_preselects(now).is_empty());

        // The report turns a week old: the reason changes though the preselect doesn't.
        assert_eq!(
            agent.refresh_preselects(now + TimeDelta::days(5)),
            std::slice::from_ref(&repo)
        );
        assert_eq!(reason(&agent), Some(ResumeReason::UncommittedChanges));
        assert_eq!(agent.repos[&repo].snapshot.preselect, Preselect::NewAgent);
    }

    #[test]
    fn a_hook_report_with_pids_moves_a_resumed_process_to_its_new_session() {
        let tmp = tempfile::tempdir().unwrap();
        let repo = PathBuf::from("/home/dev/projects/tidepool-api");
        let (old, new) = (
            "5f0c2a9e-1b7d-4c3e-9a61-0d2e8f4b7c11",
            "5f0c2a9e-1b7d-4c3e-9a61-0d2e8f4b7c12",
        );
        let project = tmp.path().join("-home-dev-projects-tidepool-api");
        fs::create_dir_all(&project).unwrap();
        for (id, hour) in [(old, "09"), (new, "10")] {
            let line = format!(
                "{{\"type\":\"user\",\"sessionId\":\"{id}\",\"cwd\":\"{}\",\
                 \"timestamp\":\"2026-11-01T{hour}:00:00Z\",\"message\":{{\"role\":\"user\",\
                 \"content\":\"chart the tides\"}}}}\n",
                repo.display()
            );
            fs::write(project.join(format!("{id}.jsonl")), line).unwrap();
        }
        let mut agent = Agent::new(Config {
            roots: vec![],
            extra: vec![],
            hidden: vec![],
            depth: 1,
            claude_projects: tmp.path().to_owned(),
            limits_file: tmp.path().join("usage-state.json"),
            codex_home: tmp.path().join("codex"),
        });
        // `claude --resume <old>`, then /clear; its start time is not known, so nothing
        // but the hook can move it.
        agent.procs = vec![AgentProcess {
            agent: AgentKind::Claude,
            pid: 812,
            cwd: path_str(&repo),
            session_id: Some(old.to_owned()),
            started: None,
            start_ticks: Some(4242),
            refract_folder: None,
        }];
        let sessions = agent.reader.read_repo(&path_str(&repo));
        let live = agent.live_set(&repo, &sessions, &[]);
        let (status, head) = (Status::default(), HeadInfo::default());
        agent.store(
            repo.clone(),
            status,
            head,
            (sessions, Vec::new()),
            live,
            HashMap::new(),
            Instructions::default(),
        );
        let live_ids = |agent: &Agent| {
            let mut ids: Vec<String> = agent.repos[&repo].live.keys().cloned().collect();
            ids.sort();
            ids
        };
        assert_eq!(live_ids(&agent), [old]);

        let mut working = HookReport::new(HookStatus::Working, Some(Utc::now().timestamp()));
        working.pids[..2].copy_from_slice(&[900, 812]);
        working.starts[..2].copy_from_slice(&[4300, 4242]);
        let events = agent.set_hooks([(new.to_owned(), working)].into());
        assert!(!events.is_empty());
        assert_eq!(live_ids(&agent), [new]);
        let states: HashMap<String, SessionState> = agent.repos[&repo]
            .snapshot
            .sessions
            .iter()
            .map(|s| (s.id.clone(), s.state))
            .collect();
        assert_eq!(states[new], SessionState::Live(LiveState::Working));
        assert!(!matches!(states[old], SessionState::Live(_)));
    }
}
