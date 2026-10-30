//! The agent's picture of every repo, and the events that keep the app in step with it.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Mutex;

use chrono::{DateTime, TimeDelta, Utc};
use refract_core::classify::{classify_report, HookReport, SessionState};
use refract_core::mass::{masses, MassInput};
use refract_core::preselect::{preselect, Candidate, Preselect};
use refract_core::protocol::{Event, RemoteId, RepoSnapshot, SessionChange, SessionInfo};
use refract_core::transcript::SessionSummary;

use crate::config::Config;
use crate::git::{self, HeadInfo, Status};
use crate::procs::{self, ClaudeProcess};
use crate::scan;
use crate::sessions::SessionReader;

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
    pub live: HashSet<String>,
    /// Status hook statuses of this repo's sessions, by session id.
    pub hooks: HashMap<String, HookReport>,
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
        let fresh = repo_preselect(
            &self.snapshot.dirty_paths,
            &self.sessions,
            &self.live,
            &self.hooks,
            now,
        );
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

    pub fn reader(&self) -> &SessionReader {
        &self.reader
    }

    pub fn reader_mut(&mut self) -> &mut SessionReader {
        &mut self.reader
    }

    /// Finds and reads every repo. Git runs for several repos at once.
    pub fn load_all(&mut self) {
        let paths = scan::find_repos(&self.config);
        self.procs = procs::scan();
        let gits = read_git_parallel(&paths);
        self.repos.clear();
        for (path, git) in paths.into_iter().zip(gits) {
            match git {
                Ok((status, head)) => self.insert(path, status, head),
                Err(e) => eprintln!("refract-agent: skipping {}: {e}", path.display()),
            }
        }
        self.update_masses();
    }

    /// Paths of known repos.
    pub fn repo_paths(&self) -> impl Iterator<Item = &PathBuf> {
        self.repos.keys()
    }

    pub fn repo(&self, path: &Path) -> Option<&RepoState> {
        self.repos.get(path)
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
        let old_snapshot = old.snapshot.clone();
        if what.sessions {
            self.procs = procs::scan();
        }
        let (sessions, live) = if what.sessions {
            let sessions = self.reader.read_repo(&path_str(path));
            let live = live_set(&self.procs, path, &sessions);
            (sessions, live)
        } else {
            (old.sessions.clone(), old.live.clone())
        };
        self.store(path.to_owned(), status, head, sessions, live);
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
        let moved = self.update_masses();
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
            let (status, head, sessions, live) = (
                repo.status.clone(),
                repo.head.clone(),
                repo.sessions.clone(),
                repo.live.clone(),
            );
            self.store(path.clone(), status, head, sessions, live);
            events.extend(diff(&old, &self.repos[&path].snapshot));
        }
        events
    }

    /// Re-checks running processes for repos with live or recent sessions and returns
    /// events for sessions whose state changed.
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
            self.store(path.clone(), status, head, sessions, live);
            events.extend(diff(&old, &self.repos[&path].snapshot));
        }
        events
    }

    fn insert(&mut self, path: PathBuf, status: Status, head: HeadInfo) {
        let sessions = self.reader.read_repo(&path_str(&path));
        let live = live_set(&self.procs, &path, &sessions);
        self.store(path, status, head, sessions, live);
    }

    fn store(
        &mut self,
        path: PathBuf,
        status: Status,
        head: HeadInfo,
        sessions: Vec<SessionSummary>,
        live: HashSet<String>,
    ) {
        let old = self.repos.get(&path);
        let mass = old.map_or(0.0, |r| r.snapshot.mass);
        let id = old.map_or_else(|| scan::repo_id(&path), |r| r.id);
        let hooks = hooks_for(&self.hooks, &sessions);
        let mut snapshot =
            build_snapshot(&path, &status, &head, &sessions, &live, &hooks, Utc::now());
        snapshot.mass = mass;
        self.repos.insert(
            path,
            RepoState {
                status,
                head,
                sessions,
                live,
                hooks,
                snapshot,
                id,
            },
        );
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
type GitResult = std::io::Result<(Status, HeadInfo)>;

fn read_git_parallel(paths: &[PathBuf]) -> Vec<GitResult> {
    let workers = std::thread::available_parallelism()
        .map_or(4, |n| n.get())
        .clamp(1, 8)
        .min(paths.len().max(1));
    let next = AtomicUsize::new(0);
    let results: Mutex<Vec<Option<GitResult>>> =
        Mutex::new((0..paths.len()).map(|_| None).collect());
    std::thread::scope(|scope| {
        for _ in 0..workers {
            scope.spawn(|| loop {
                let i = next.fetch_add(1, Ordering::Relaxed);
                let Some(path) = paths.get(i) else {
                    break;
                };
                let result = git::read_status(path).and_then(|s| Ok((s, git::read_head(path)?)));
                results.lock().expect("results lock")[i] = Some(result);
            });
        }
    });
    results
        .into_inner()
        .expect("results lock")
        .into_iter()
        .map(|r| r.expect("every repo read"))
        .collect()
}

fn live_set(procs: &[ClaudeProcess], repo: &Path, sessions: &[SessionSummary]) -> HashSet<String> {
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
/// status hook statuses by session id.
pub fn build_snapshot(
    path: &Path,
    status: &Status,
    head: &HeadInfo,
    sessions: &[SessionSummary],
    live: &HashSet<String>,
    hooks: &HashMap<String, HookReport>,
    now: DateTime<Utc>,
) -> RepoSnapshot {
    let root = path_str(path);
    let dirty_paths: Vec<String> = status
        .paths
        .iter()
        .map(|p| format!("{}/{p}", root.trim_end_matches('/')))
        .collect();
    let states = session_states(sessions, live, hooks);
    let preselect = repo_preselect(&dirty_paths, sessions, live, hooks, now);
    let sessions = sessions
        .iter()
        .zip(&states)
        .map(|(s, &state)| SessionInfo {
            id: s.session_id.clone().unwrap_or_default(),
            state,
            last_timestamp: s.last_timestamp,
            last_assistant_text: s.last_assistant_text.clone(),
            byte_size: s.byte_size.unwrap_or(0),
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
    }
}

fn session_states(
    sessions: &[SessionSummary],
    live: &HashSet<String>,
    hooks: &HashMap<String, HookReport>,
) -> Vec<SessionState> {
    sessions
        .iter()
        .map(|s| {
            let id = s.session_id.as_ref();
            let is_live = id.is_some_and(|id| live.contains(id));
            classify_report(s, is_live, id.and_then(|id| hooks.get(id).copied()))
        })
        .collect()
}

fn repo_preselect(
    dirty_paths: &[String],
    sessions: &[SessionSummary],
    live: &HashSet<String>,
    hooks: &HashMap<String, HookReport>,
    now: DateTime<Utc>,
) -> Preselect {
    let states = session_states(sessions, live, hooks);
    let candidates: Vec<Candidate<'_>> = sessions
        .iter()
        .zip(&states)
        .map(|(summary, &state)| Candidate {
            summary,
            state,
            byte_size: summary.byte_size.unwrap_or(0),
        })
        .collect();
    preselect(&candidates, dirty_paths, now)
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
    use refract_core::classify::{FinishKind, LiveState};

    fn session(id: &str, state: SessionState) -> SessionInfo {
        SessionInfo {
            id: id.into(),
            state,
            last_timestamp: None,
            last_assistant_text: None,
            byte_size: 10,
        }
    }

    fn snapshot() -> RepoSnapshot {
        build_snapshot(
            Path::new("/home/dev/projects/tidepool-api"),
            &Status::default(),
            &HeadInfo::default(),
            &[],
            &HashSet::new(),
            &HashMap::new(),
            Utc::now(),
        )
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
}
