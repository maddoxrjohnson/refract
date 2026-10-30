//! File-system watches through inotify.
//!
//! The `inotify` crate is used directly rather than `notify`: the helper only runs on
//! Linux, needs precise per-directory masks (non-recursive, specific files in `.git`)
//! and must idle without polling, and `inotify` does exactly that with two small
//! dependencies.
//!
//! One thread blocks reading events and hands them to the main loop; the main loop owns
//! the [`Watcher`], adds and removes watches, and maps events to [`Change`]s.
//!
//! Watched per repo: the git directory (HEAD, index, packed-refs, ...), `refs/heads` and
//! `refs/remotes` with their subdirectories (in the common git directory for linked
//! worktrees), and every working-tree directory that holds tracked files. Also watched:
//! Claude Code's `projects` directory, the project directory of each repo, every
//! directory the repo search walks (for repos appearing or vanishing), the directory and
//! git directory of each repo the search found but git could not read yet (for it
//! becoming readable; the server also tries such a repo again on a timer, see
//! [`crate::server::RETRY_DELAYS`]), and the status hook's directory of status files.
//!
//! Re-watching a repo only adds and removes the directories that differ, so events
//! already queued for unchanged watches still map to their repo. When the kernel drops a
//! watch (`IN_IGNORED`, because its directory was deleted), its targets are forgotten,
//! so a recreated directory can be watched again.

use std::collections::{HashMap, HashSet};
use std::ffi::OsString;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};

use inotify::{EventMask, Inotify, WatchDescriptor, WatchMask, Watches};

use crate::state::Refresh;

/// Most working-tree directories watched per repo.
pub const MAX_TREE_DIRS: usize = 1000;

/// Most ref directories watched per repo.
const MAX_REF_DIRS: usize = 200;

/// One raw inotify event.
#[derive(Debug, Clone)]
pub struct FsEvent {
    pub wd: i32,
    pub mask: EventMask,
    pub name: Option<OsString>,
}

/// What a watched path stands for.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum Target {
    GitDir(PathBuf),
    /// The common git directory of a linked worktree, for `packed-refs`.
    CommonDir(PathBuf),
    Refs(PathBuf),
    Tree(PathBuf),
    ProjectsRoot,
    /// A project directory, by name, inside Claude Code's `projects` directory.
    ProjectDir(String),
    /// A directory the repo search walks. `descend` is false at the depth limit, where
    /// only a `.git` appearing matters.
    ScanDir {
        descend: bool,
    },
    /// The git directory of a repo the search found but git could not read yet, such as
    /// one `git init` or a copy is still writing. A change in it tries the repo again.
    UnreadGitDir(PathBuf),
    /// The directory of such a repo, for its `.git` being written, replaced or removed,
    /// also when that is a file that does not name a git directory yet.
    UnreadRepo(PathBuf),
    /// The status hook's directory of `<session id>.json` files.
    HookDir,
}

impl Target {
    /// The repo a per-repo target belongs to.
    fn repo(&self) -> Option<&Path> {
        match self {
            Target::GitDir(p) | Target::CommonDir(p) | Target::Refs(p) | Target::Tree(p) => Some(p),
            _ => None,
        }
    }
}

/// What an event means for the agent.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Change {
    Repo(PathBuf, Refresh),
    /// A directory appeared in the repo's working tree; its watches may need adding.
    Rewatch(PathBuf),
    /// Transcripts changed in this project directory.
    Sessions(String),
    /// This status hook file was written or removed.
    Hook(String),
    /// A project directory appeared.
    NewProjectDir(String),
    /// Something changed in a repo git could not read; it may be readable now.
    Retry(PathBuf),
    /// Repos may have appeared or vanished.
    Rescan,
    /// Events were lost; reload everything.
    Overflow,
}

/// Owns the watch list. Cheap to keep on the main thread; the reading thread is
/// separate.
pub struct Watcher {
    watches: Watches,
    targets: HashMap<i32, Entry>,
}

/// One inotify watch and what it stands for.
struct Entry {
    wd: WatchDescriptor,
    path: PathBuf,
    targets: Vec<Target>,
}

/// Starts the reading thread. `sink` receives each batch of events and returns false
/// to stop the thread.
pub fn start<F>(mut sink: F) -> io::Result<Watcher>
where
    F: FnMut(Vec<FsEvent>) -> bool + Send + 'static,
{
    let mut inotify = Inotify::init()?;
    let watches = inotify.watches();
    std::thread::Builder::new()
        .name("inotify".into())
        .spawn(move || {
            let mut buffer = vec![0u8; 64 * 1024];
            loop {
                let events = match inotify.read_events_blocking(&mut buffer) {
                    Ok(events) => events,
                    Err(e) => {
                        eprintln!("refract-agent: inotify read failed: {e}");
                        return;
                    }
                };
                let batch = events
                    .map(|e| FsEvent {
                        wd: e.wd.get_watch_descriptor_id(),
                        mask: e.mask,
                        name: e.name.map(|n| n.to_owned()),
                    })
                    .collect();
                if !sink(batch) {
                    return;
                }
            }
        })?;
    Ok(Watcher {
        watches,
        targets: HashMap::new(),
    })
}

impl Watcher {
    /// Watches a repo's git directory, refs and tracked directories. Called again for
    /// the same repo, it adds only directories not yet watched and removes those no
    /// longer wanted. `common_dir` is the common git directory of a linked worktree.
    pub fn watch_repo(
        &mut self,
        repo: &Path,
        git_dir: Option<&Path>,
        common_dir: Option<&Path>,
        tracked_dirs: &[String],
    ) {
        let repo_buf = repo.to_owned();
        let mut wanted: Vec<(PathBuf, WatchMask, Target)> = Vec::new();
        if let Some(git_dir) = git_dir {
            let mask = WatchMask::CLOSE_WRITE
                | WatchMask::MOVED_TO
                | WatchMask::CREATE
                | WatchMask::DELETE
                | WatchMask::DELETE_SELF
                | WatchMask::MOVE_SELF;
            wanted.push((git_dir.to_owned(), mask, Target::GitDir(repo_buf.clone())));
            let refs_dir = match common_dir {
                Some(common) => {
                    let mask = WatchMask::CLOSE_WRITE | WatchMask::MOVED_TO | WatchMask::CREATE;
                    wanted.push((common.to_owned(), mask, Target::CommonDir(repo_buf.clone())));
                    common
                }
                None => git_dir,
            };
            let mut ref_dirs = Vec::new();
            for sub in ["refs/heads", "refs/remotes"] {
                collect_dirs(&refs_dir.join(sub), &mut ref_dirs, MAX_REF_DIRS);
            }
            let mask = WatchMask::MOVED_TO | WatchMask::CREATE | WatchMask::DELETE;
            for dir in ref_dirs {
                wanted.push((dir, mask, Target::Refs(repo_buf.clone())));
            }
        }
        let mask = WatchMask::CLOSE_WRITE
            | WatchMask::CREATE
            | WatchMask::DELETE
            | WatchMask::MOVED_FROM
            | WatchMask::MOVED_TO;
        for dir in tracked_dirs.iter().take(MAX_TREE_DIRS) {
            wanted.push((repo.join(dir), mask, Target::Tree(repo_buf.clone())));
        }
        self.sync(|t| t.repo() == Some(repo), wanted);
    }

    /// Stops watching a repo.
    pub fn unwatch_repo(&mut self, repo: &Path) {
        self.remove_where(|t| t.repo() == Some(repo));
    }

    /// Stops watching a project directory, by name.
    pub fn unwatch_project_dir(&mut self, name: &str) {
        self.remove_where(|t| matches!(t, Target::ProjectDir(n) if n == name));
    }

    /// Project directories watched, by name.
    pub fn project_dirs(&self) -> Vec<String> {
        let mut names: Vec<String> = self
            .targets
            .values()
            .flat_map(|e| &e.targets)
            .filter_map(|t| match t {
                Target::ProjectDir(name) => Some(name.clone()),
                _ => None,
            })
            .collect();
        names.sort();
        names.dedup();
        names
    }

    /// Watches Claude Code's `projects` directory for new project directories.
    pub fn watch_projects_root(&mut self, dir: &Path) {
        let mask = WatchMask::CREATE | WatchMask::MOVED_TO | WatchMask::ONLYDIR;
        self.add(dir, mask, Target::ProjectsRoot);
    }

    /// Watches one project directory for transcript writes. Watching it twice is fine.
    pub fn watch_project_dir(&mut self, dir: &Path) {
        let Some(name) = dir.file_name() else {
            return;
        };
        let target = Target::ProjectDir(name.to_string_lossy().into_owned());
        if self.targets.values().any(|e| e.targets.contains(&target)) {
            return;
        }
        let mask = WatchMask::MODIFY
            | WatchMask::CLOSE_WRITE
            | WatchMask::CREATE
            | WatchMask::DELETE
            | WatchMask::MOVED_TO
            | WatchMask::MOVED_FROM
            | WatchMask::DELETE_SELF
            | WatchMask::ONLYDIR;
        self.add(dir, mask, target);
    }

    /// Watches the directories the repo search walked (see [`crate::scan::Walk`]) for
    /// directories and `.git` appearing or vanishing, and the repos it found but git
    /// could not read (`unread`) and their git directories, replacing the previous sets.
    ///
    /// Returns true when it watched a directory that was not watched before. What changed
    /// there between the search and the watch was not seen, so the caller searches again.
    pub fn watch_scan_dirs(&mut self, dirs: &[(PathBuf, bool)], unread: &[PathBuf]) -> bool {
        let mask = WatchMask::CREATE
            | WatchMask::DELETE
            | WatchMask::MOVED_FROM
            | WatchMask::MOVED_TO
            | WatchMask::ONLYDIR;
        let mut wanted: Vec<_> = dirs
            .iter()
            .map(|(dir, descend)| (dir.clone(), mask, Target::ScanDir { descend: *descend }))
            .collect();
        // `ATTRIB`: a `chmod` or `chown` can make a repo readable.
        let repo_mask = WatchMask::CREATE
            | WatchMask::CLOSE_WRITE
            | WatchMask::MOVED_TO
            | WatchMask::MOVED_FROM
            | WatchMask::DELETE
            | WatchMask::ATTRIB
            | WatchMask::ONLYDIR;
        let git_mask = WatchMask::CREATE
            | WatchMask::CLOSE_WRITE
            | WatchMask::MOVED_TO
            | WatchMask::ATTRIB
            | WatchMask::DELETE_SELF
            | WatchMask::MOVE_SELF
            | WatchMask::ONLYDIR;
        for repo in unread {
            wanted.push((repo.clone(), repo_mask, Target::UnreadRepo(repo.clone())));
            if let Some(git_dir) = crate::scan::git_dir(repo) {
                wanted.push((git_dir, git_mask, Target::UnreadGitDir(repo.clone())));
            }
        }
        self.sync(
            |t| {
                matches!(
                    t,
                    Target::ScanDir { .. } | Target::UnreadGitDir(_) | Target::UnreadRepo(_)
                )
            },
            wanted,
        )
    }

    /// Watches the status hook's directory for status files being written or removed.
    pub fn watch_hook_dir(&mut self, dir: &Path) {
        let mask = WatchMask::CLOSE_WRITE
            | WatchMask::MOVED_TO
            | WatchMask::MOVED_FROM
            | WatchMask::DELETE
            | WatchMask::ONLYDIR;
        self.add(dir, mask, Target::HookDir);
    }

    /// Number of inotify watches held.
    pub fn len(&self) -> usize {
        self.targets.len()
    }

    /// True when nothing is watched.
    pub fn is_empty(&self) -> bool {
        self.targets.is_empty()
    }

    /// Maps an event to what changed.
    pub fn changes(&self, event: &FsEvent) -> Vec<Change> {
        if event.mask.contains(EventMask::Q_OVERFLOW) {
            return vec![Change::Overflow];
        }
        let Some(entry) = self.targets.get(&event.wd) else {
            return Vec::new();
        };
        entry
            .targets
            .iter()
            .flat_map(|t| changes_for(t, event.mask, event.name.as_deref()))
            .collect()
    }

    /// Forgets a watch the kernel dropped (`IN_IGNORED`), so its directory can be watched
    /// again if it is recreated. Call after [`Watcher::changes`] for each event.
    pub fn forget_dropped(&mut self, event: &FsEvent) {
        if event.mask.contains(EventMask::IGNORED) {
            self.targets.remove(&event.wd);
        }
    }

    /// Adds `target` to the watch on `path`. Returns true when that is a new watch, not
    /// one the directory already had.
    fn add(&mut self, path: &Path, mask: WatchMask, target: Target) -> bool {
        let mask = mask | WatchMask::MASK_ADD | WatchMask::DONT_FOLLOW;
        match self.watches.add(path, mask) {
            Ok(wd) => {
                let id = wd.get_watch_descriptor_id();
                let new = !self.targets.contains_key(&id);
                let entry = self.targets.entry(id).or_insert_with(|| Entry {
                    wd,
                    path: path.to_owned(),
                    targets: Vec::new(),
                });
                if !entry.targets.contains(&target) {
                    entry.targets.push(target);
                }
                new
            }
            Err(e) if e.kind() == io::ErrorKind::NotFound => false,
            Err(e) => {
                eprintln!("refract-agent: cannot watch {}: {e}", path.display());
                false
            }
        }
    }

    /// Makes the targets matching `owned` exactly `wanted`: removes the others and adds
    /// only the missing ones, leaving unchanged watches (and their descriptors) alone.
    /// Returns true when a directory got a new watch.
    fn sync(
        &mut self,
        owned: impl Fn(&Target) -> bool,
        wanted: Vec<(PathBuf, WatchMask, Target)>,
    ) -> bool {
        let keep: HashSet<(&Path, &Target)> =
            wanted.iter().map(|(p, _, t)| (p.as_path(), t)).collect();
        let mut have: HashSet<(PathBuf, Target)> = HashSet::new();
        let mut emptied = Vec::new();
        for (id, entry) in self.targets.iter_mut() {
            let path = &entry.path;
            entry.targets.retain(|t| {
                if !owned(t) {
                    return true;
                }
                let kept = keep.contains(&(path.as_path(), t));
                if kept {
                    have.insert((path.clone(), t.clone()));
                }
                kept
            });
            if entry.targets.is_empty() {
                emptied.push(*id);
            }
        }
        self.drop_watches(emptied);
        let mut added = false;
        for (path, mask, target) in wanted {
            if !have.contains(&(path.clone(), target.clone())) {
                added |= self.add(&path, mask, target);
            }
        }
        added
    }

    fn remove_where(&mut self, remove: impl Fn(&Target) -> bool) {
        let mut emptied = Vec::new();
        for (id, entry) in self.targets.iter_mut() {
            entry.targets.retain(|t| !remove(t));
            if entry.targets.is_empty() {
                emptied.push(*id);
            }
        }
        self.drop_watches(emptied);
    }

    fn drop_watches(&mut self, ids: Vec<i32>) {
        for id in ids {
            if let Some(entry) = self.targets.remove(&id) {
                // Fails harmlessly when the directory is already gone.
                let _ = self.watches.remove(entry.wd);
            }
        }
    }
}

/// Everything an event on `target` means: [`change_for`], plus a [`Change::Rewatch`]
/// when a directory appeared in a working tree.
pub fn changes_for(
    target: &Target,
    mask: EventMask,
    name: Option<&std::ffi::OsStr>,
) -> Vec<Change> {
    let mut changes: Vec<Change> = change_for(target, mask, name).into_iter().collect();
    if let Target::Tree(repo) = target {
        if mask.contains(EventMask::ISDIR)
            && mask.intersects(EventMask::CREATE | EventMask::MOVED_TO)
        {
            changes.push(Change::Rewatch(repo.clone()));
        }
    }
    changes
}

/// What an event on `target` means. Pure, so it is unit-tested.
pub fn change_for(
    target: &Target,
    mask: EventMask,
    name: Option<&std::ffi::OsStr>,
) -> Option<Change> {
    let name = name.map(|n| n.to_string_lossy());
    let name = name.as_deref();
    let gone = mask.intersects(EventMask::DELETE_SELF | EventMask::MOVE_SELF);
    match target {
        Target::GitDir(repo) => {
            if gone {
                return Some(Change::Rescan);
            }
            git_dir_refresh(name?).map(|r| Change::Repo(repo.clone(), r))
        }
        Target::CommonDir(repo) => (name? == "packed-refs").then(|| {
            Change::Repo(
                repo.clone(),
                Refresh {
                    status: true,
                    head: true,
                    sessions: false,
                },
            )
        }),
        Target::Refs(repo) => {
            let name = name?;
            (!name.ends_with(".lock")).then(|| {
                Change::Repo(
                    repo.clone(),
                    Refresh {
                        status: true,
                        head: true,
                        sessions: false,
                    },
                )
            })
        }
        Target::Tree(repo) => Some(Change::Repo(
            repo.clone(),
            Refresh {
                status: true,
                ..Refresh::default()
            },
        )),
        Target::ProjectsRoot => name
            .filter(|_| mask.contains(EventMask::ISDIR))
            .map(|n| Change::NewProjectDir(n.to_owned())),
        Target::ProjectDir(dir) => (gone || name.is_some_and(|n| n.ends_with(".jsonl")))
            .then(|| Change::Sessions(dir.clone())),
        Target::ScanDir { descend } => {
            let name = name?;
            let new_repo = name == ".git";
            let new_dir =
                *descend && mask.contains(EventMask::ISDIR) && !crate::scan::is_skipped(name);
            (new_repo || new_dir).then_some(Change::Rescan)
        }
        Target::UnreadGitDir(repo) => {
            if gone {
                return Some(Change::Rescan);
            }
            // Lock files come before the rename that matters, and a fetch or the reflog
            // does not make a repo readable.
            let noise = |n: &str| n.ends_with(".lock") || n == "FETCH_HEAD" || n == "logs";
            let attrib = name.is_none() && mask.contains(EventMask::ATTRIB);
            (attrib || name.is_some_and(|n| !noise(n))).then(|| Change::Retry(repo.clone()))
        }
        // Without a `.git` the directory is no repo, and the search walks it again.
        Target::UnreadRepo(repo) => (name? == ".git").then(|| {
            if mask.intersects(EventMask::DELETE | EventMask::MOVED_FROM) {
                Change::Rescan
            } else {
                Change::Retry(repo.clone())
            }
        }),
        // The script writes a temporary file and renames it, so only `.json` names
        // matter; a name that is no session id is skipped when read.
        Target::HookDir => name
            .filter(|n| n.ends_with(".json") && !n.starts_with('.'))
            .map(|n| Change::Hook(n.to_owned())),
    }
}

/// Which files in a git directory matter, and what they affect. `FETCH_HEAD` is left
/// out: every fetch rewrites it, and what a fetch changes shows up in `refs/remotes`.
fn git_dir_refresh(name: &str) -> Option<Refresh> {
    match name {
        "index" => Some(Refresh {
            status: true,
            ..Refresh::default()
        }),
        "HEAD" | "ORIG_HEAD" | "MERGE_HEAD" | "packed-refs" | "config" => Some(Refresh {
            status: true,
            head: true,
            sessions: false,
        }),
        _ => None,
    }
}

fn collect_dirs(dir: &Path, out: &mut Vec<PathBuf>, limit: usize) {
    if out.len() >= limit || !dir.is_dir() {
        return;
    }
    out.push(dir.to_owned());
    let Ok(entries) = fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        if entry.file_type().is_ok_and(|t| t.is_dir()) {
            collect_dirs(&entry.path(), out, limit);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::ffi::OsStr;

    fn repo() -> PathBuf {
        PathBuf::from("/home/dev/projects/tidepool-api")
    }

    fn change(target: Target, mask: EventMask, name: &str) -> Option<Change> {
        change_for(&target, mask, Some(OsStr::new(name)))
    }

    #[test]
    fn git_dir_events() {
        let status_only = Refresh {
            status: true,
            ..Refresh::default()
        };
        assert_eq!(
            change(Target::GitDir(repo()), EventMask::MOVED_TO, "index"),
            Some(Change::Repo(repo(), status_only))
        );
        let head = change(Target::GitDir(repo()), EventMask::MOVED_TO, "HEAD");
        assert!(matches!(head, Some(Change::Repo(_, r)) if r.head && r.status));
        assert_eq!(
            change(Target::GitDir(repo()), EventMask::CREATE, "index.lock"),
            None
        );
        assert_eq!(
            change(
                Target::GitDir(repo()),
                EventMask::CLOSE_WRITE,
                "COMMIT_EDITMSG"
            ),
            None
        );
        assert_eq!(
            change_for(&Target::GitDir(repo()), EventMask::DELETE_SELF, None),
            Some(Change::Rescan)
        );
        assert_eq!(
            change(Target::GitDir(repo()), EventMask::CLOSE_WRITE, "FETCH_HEAD"),
            None
        );
    }

    #[test]
    fn common_dir_events() {
        let packed = change(
            Target::CommonDir(repo()),
            EventMask::MOVED_TO,
            "packed-refs",
        );
        assert!(matches!(packed, Some(Change::Repo(_, r)) if r.head && r.status));
        assert_eq!(
            change(Target::CommonDir(repo()), EventMask::MOVED_TO, "HEAD"),
            None
        );
    }

    #[test]
    fn ref_tree_and_session_events() {
        assert!(change(Target::Refs(repo()), EventMask::MOVED_TO, "main").is_some());
        assert_eq!(
            change(Target::Refs(repo()), EventMask::CREATE, "main.lock"),
            None
        );
        let tree = change(Target::Tree(repo()), EventMask::CLOSE_WRITE, "lib.rs");
        assert!(matches!(tree, Some(Change::Repo(_, r)) if r.status && !r.head));
        let new_dir = changes_for(
            &Target::Tree(repo()),
            EventMask::CREATE | EventMask::ISDIR,
            Some(OsStr::new("tides")),
        );
        assert_eq!(new_dir.len(), 2);
        assert_eq!(new_dir[1], Change::Rewatch(repo()));
        let dir = "-home-dev-projects-tidepool-api".to_owned();
        assert_eq!(
            change(
                Target::ProjectDir(dir.clone()),
                EventMask::MODIFY,
                "a.jsonl"
            ),
            Some(Change::Sessions(dir.clone()))
        );
        assert_eq!(
            change(Target::ProjectDir(dir), EventMask::CREATE, "a.tmp"),
            None
        );
    }

    #[test]
    fn directory_events_in_roots() {
        let dir = EventMask::CREATE | EventMask::ISDIR;
        assert_eq!(
            change(Target::ProjectsRoot, dir, "-home-dev-x"),
            Some(Change::NewProjectDir("-home-dev-x".into()))
        );
        let scan = Target::ScanDir { descend: true };
        let leaf = Target::ScanDir { descend: false };
        assert_eq!(change(scan.clone(), dir, "kelp-cli"), Some(Change::Rescan));
        assert_eq!(change(scan.clone(), EventMask::CREATE, "notes.txt"), None);
        assert_eq!(change(scan.clone(), dir, "node_modules"), None);
        assert_eq!(change(scan, dir, ".git"), Some(Change::Rescan));
        // At the depth limit only a `.git` (directory or worktree file) matters.
        assert_eq!(change(leaf.clone(), dir, "kelp-cli"), None);
        assert_eq!(change(leaf.clone(), dir, ".git"), Some(Change::Rescan));
        assert_eq!(
            change(leaf, EventMask::CREATE, ".git"),
            Some(Change::Rescan)
        );
        // In a git directory git could not read yet, what is written tries it again.
        let retry = Some(Change::Retry(repo()));
        let unread = Target::UnreadGitDir(repo());
        assert_eq!(change(unread.clone(), EventMask::MOVED_TO, "HEAD"), retry);
        assert_eq!(change(unread.clone(), dir, "objects"), retry);
        assert_eq!(change(unread.clone(), EventMask::ATTRIB, "config"), retry);
        assert_eq!(change_for(&unread, EventMask::ATTRIB, None), retry);
        assert_eq!(change(unread.clone(), EventMask::CREATE, "HEAD.lock"), None);
        assert_eq!(
            change(unread.clone(), EventMask::CLOSE_WRITE, "FETCH_HEAD"),
            None
        );
        assert_eq!(change(unread.clone(), dir, "logs"), None);
        assert_eq!(
            change_for(&unread, EventMask::DELETE_SELF, None),
            Some(Change::Rescan)
        );
        assert_eq!(change_for(&unread, EventMask::IGNORED, None), None);
        // In its directory only `.git` matters; without one it is no repo.
        let unread = Target::UnreadRepo(repo());
        assert_eq!(
            change(unread.clone(), EventMask::CLOSE_WRITE, ".git"),
            retry
        );
        assert_eq!(change(unread.clone(), EventMask::MOVED_TO, ".git"), retry);
        assert_eq!(change(unread.clone(), EventMask::ATTRIB, ".git"), retry);
        assert_eq!(
            change(unread.clone(), EventMask::DELETE, ".git"),
            Some(Change::Rescan)
        );
        assert_eq!(change(unread, EventMask::CLOSE_WRITE, "README.md"), None);
    }

    #[test]
    fn hook_dir_events() {
        let name = "3f6c1a52-0000-4000-8000-000000000001.json";
        assert_eq!(
            change(Target::HookDir, EventMask::MOVED_TO, name),
            Some(Change::Hook(name.into()))
        );
        assert_eq!(
            change(
                Target::HookDir,
                EventMask::CLOSE_WRITE,
                ".3f6c1a52-0000-4000-8000-000000000001.77.tmp"
            ),
            None
        );
        assert_eq!(
            change_for(&Target::HookDir, EventMask::DELETE_SELF, None),
            None
        );
    }

    fn fs_events(rx: &std::sync::mpsc::Receiver<Vec<FsEvent>>) -> Vec<FsEvent> {
        let mut all = Vec::new();
        while let Ok(batch) = rx.recv_timeout(std::time::Duration::from_millis(300)) {
            all.extend(batch);
        }
        all
    }

    #[test]
    fn rewatching_keeps_descriptors_and_dropped_watches_are_forgotten() {
        let tmp = tempfile::tempdir().unwrap();
        let repo = tmp.path().join("tidepool-api");
        fs::create_dir_all(repo.join(".git/refs/heads")).unwrap();
        fs::create_dir_all(repo.join("src")).unwrap();
        fs::create_dir_all(repo.join("docs")).unwrap();
        let (tx, rx) = std::sync::mpsc::channel();
        let mut watcher = start(move |batch| tx.send(batch).is_ok()).unwrap();
        let git = repo.join(".git");
        let dirs = |names: &[&str]| names.iter().map(|n| n.to_string()).collect::<Vec<_>>();
        watcher.watch_repo(&repo, Some(&git), None, &dirs(&["", "src"]));
        let before: HashMap<PathBuf, i32> = watcher
            .targets
            .iter()
            .map(|(id, e)| (e.path.clone(), *id))
            .collect();
        assert_eq!(before.len(), 4);

        watcher.watch_repo(&repo, Some(&git), None, &dirs(&["", "docs"]));
        let after: HashMap<PathBuf, i32> = watcher
            .targets
            .iter()
            .map(|(id, e)| (e.path.clone(), *id))
            .collect();
        assert_eq!(after.len(), 4);
        assert!(!after.contains_key(&repo.join("src")));
        for path in [repo.clone(), git.clone(), git.join("refs/heads")] {
            assert_eq!(before[&path], after[&path], "{}", path.display());
        }

        // Deleting a watched directory drops its watch; recreating it can be watched.
        fs::remove_dir(repo.join("docs")).unwrap();
        for event in fs_events(&rx) {
            watcher.changes(&event);
            watcher.forget_dropped(&event);
        }
        assert!(!watcher
            .targets
            .values()
            .any(|e| e.path == repo.join("docs")));
        fs::create_dir_all(repo.join("docs")).unwrap();
        watcher.watch_repo(&repo, Some(&git), None, &dirs(&["", "docs"]));
        assert!(watcher
            .targets
            .values()
            .any(|e| e.path == repo.join("docs")));

        watcher.unwatch_repo(&repo);
        assert!(watcher.is_empty());
    }

    #[test]
    fn project_dirs_can_be_unwatched() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().join("-home-dev-projects-tidepool-api");
        fs::create_dir_all(&dir).unwrap();
        let mut watcher = start(|_| true).unwrap();
        watcher.watch_project_dir(&dir);
        assert_eq!(watcher.project_dirs(), ["-home-dev-projects-tidepool-api"]);
        watcher.unwatch_project_dir("-home-dev-projects-tidepool-api");
        assert!(watcher.is_empty());
    }

    #[test]
    fn an_unread_repo_is_tried_again_after_a_chmod() {
        use std::os::unix::fs::PermissionsExt;
        let tmp = tempfile::tempdir().unwrap();
        let kelp = tmp.path().join("kelp-cli");
        fs::create_dir_all(kelp.join(".git")).unwrap();
        let (tx, rx) = std::sync::mpsc::channel();
        let mut watcher = start(move |batch| tx.send(batch).is_ok()).unwrap();
        watcher.watch_scan_dirs(&[], std::slice::from_ref(&kelp));
        let mode = fs::Permissions::from_mode(0o700);
        fs::set_permissions(kelp.join(".git"), mode).unwrap();
        let changes: Vec<Change> = fs_events(&rx)
            .iter()
            .flat_map(|e| watcher.changes(e))
            .collect();
        assert!(changes.contains(&Change::Retry(kelp)), "{changes:?}");
    }

    #[test]
    fn scan_dirs_report_only_new_watches() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().to_owned();
        let group = root.join("group");
        let kelp = root.join("kelp-cli");
        fs::create_dir_all(&group).unwrap();
        fs::create_dir_all(kelp.join(".git")).unwrap();
        let mut watcher = start(|_| true).unwrap();
        let dirs = vec![(root.clone(), true)];
        assert!(watcher.watch_scan_dirs(&dirs, &[]));
        assert!(!watcher.watch_scan_dirs(&dirs, &[]));

        // A directory already watched for something else is not new.
        watcher.watch_repo(&group, None, None, &[String::new()]);
        let dirs = vec![(root.clone(), true), (group.clone(), true)];
        assert!(!watcher.watch_scan_dirs(&dirs, &[]));

        // A repo git cannot read: its directory and its git directory.
        let unread = std::slice::from_ref(&kelp);
        assert!(watcher.watch_scan_dirs(&dirs, unread));
        assert_eq!(watcher.len(), 4);
        assert!(!watcher.watch_scan_dirs(&dirs, unread));
        assert!(!watcher.watch_scan_dirs(&dirs, &[root.join("missing")]));
        assert_eq!(watcher.len(), 2);

        // A `.git` file that names no git directory yet: the directory alone.
        fs::remove_dir(kelp.join(".git")).unwrap();
        fs::write(kelp.join(".git"), "").unwrap();
        assert!(watcher.watch_scan_dirs(&dirs, unread));
        assert_eq!(watcher.len(), 3);
    }
}
