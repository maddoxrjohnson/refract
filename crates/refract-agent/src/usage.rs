//! Token usage for the `usage` request, kept up to date in memory.
//!
//! A thread with its own inotify instance watches Claude Code's `projects` directory,
//! every project directory in it, and the deeper directories (`<session>/subagents/…`)
//! that hold recent transcripts. A watched session directory has its `subagents`
//! tree watched whatever its age, since a resumed session adds transcripts there. For each `.jsonl` written in the last 7 days it keeps
//! the byte offset read up to and the usage records found (see
//! [`refract_core::usage`]). A write reads only the bytes after that offset, up to the
//! last complete line; a partial last line is read again once it is finished. A file
//! that shrank or was replaced (another inode) is read again from the start. Records
//! older than 7 days are dropped, so `usage` only adds up what is in memory.
//!
//! The watches are separate from [`crate::watch`]'s, which only cover the project
//! directories of known repos, while usage counts every session, in or out of a repo.
//! The first scan runs on that thread too; until it is done, `usage` answers zeros
//! marked not ready.
//!
//! That thread holds the tracker while it reads, which can take seconds for a big
//! transcript or a rescan. `usage` never waits for it: when the tracker is busy it
//! answers the last summary it computed.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::fs::{self, File};
use std::io::{self, Read, Seek, SeekFrom};
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, MutexGuard, TryLockError};
use std::time::Duration;

use chrono::{DateTime, Local, NaiveTime, TimeDelta, Utc};
use inotify::{EventMask, Inotify, WatchDescriptor, WatchMask, Watches};
use refract_core::usage::{self, UsageParser, UsageSummary, SEVEN_DAYS};

/// How deep under the `projects` directory transcripts are looked for. Subagent
/// transcripts sit at `<project>/<session>/subagents/workflows/<run>/<agent>.jsonl`.
pub const MAX_DEPTH: usize = 6;

/// How much of a transcript is read at once.
const CHUNK: usize = 1024 * 1024;

/// How often records are checked for having aged past 7 days.
const PRUNE_EVERY: TimeDelta = TimeDelta::minutes(1);

/// How often a missing `projects` directory is looked for again.
const MISSING_ROOT_RETRY: Duration = Duration::from_secs(30);

/// Usage state shared between its thread and the main loop.
pub struct Usage {
    shared: Arc<Mutex<Tracker>>,
    /// The last summary computed, answered while the usage thread holds the tracker.
    /// Only `summary` locks it.
    last: Mutex<Option<UsageSummary>>,
}

impl Usage {
    /// Starts the thread that scans and watches `projects`.
    pub fn start(projects: PathBuf) -> Usage {
        let shared = Arc::new(Mutex::new(Tracker::new(projects.clone())));
        let theirs = Arc::clone(&shared);
        let started = std::thread::Builder::new()
            .name("usage".into())
            .spawn(move || run(&theirs, projects));
        if let Err(e) = started {
            eprintln!("refract-agent: cannot start the usage thread: {e}");
        }
        Usage {
            shared,
            last: Mutex::new(None),
        }
    }

    /// Totals for now, with today starting at local midnight. `repos` are the known
    /// repo paths, for the per-repo split. Never waits on the usage thread: while it
    /// is reading, the last summary is answered again with `live_agents` updated, or
    /// zeros marked not ready if there is none yet.
    pub fn summary(&self, repos: &[String], live_agents: u32) -> UsageSummary {
        let now = Utc::now();
        let mut last = self.last.lock().unwrap_or_else(|e| e.into_inner());
        let tracker = match self.shared.try_lock() {
            Ok(tracker) => Some(tracker),
            Err(TryLockError::Poisoned(e)) => Some(e.into_inner()),
            Err(TryLockError::WouldBlock) => None,
        };
        let summary = match (tracker, last.as_ref()) {
            (Some(mut tracker), _) => tracker.summary(repos, now, local_midnight(now), live_agents),
            (None, Some(previous)) => UsageSummary {
                live_agents,
                ..previous.clone()
            },
            (None, None) => {
                Tracker::new(PathBuf::new()).summary(repos, now, local_midnight(now), live_agents)
            }
        };
        *last = Some(summary.clone());
        summary
    }
}

/// Local midnight of the day holding `now`, in the helper's time zone. Where a clock
/// change skips midnight, the current offset is used instead.
pub fn local_midnight(now: DateTime<Utc>) -> DateTime<Utc> {
    let local = now.with_timezone(&Local);
    local
        .date_naive()
        .and_time(NaiveTime::MIN)
        .and_local_timezone(Local)
        .earliest()
        .map(|t| t.with_timezone(&Utc))
        .unwrap_or_else(|| usage::local_midnight(now, *local.offset()))
}

fn lock(shared: &Mutex<Tracker>) -> MutexGuard<'_, Tracker> {
    // A panic while updating leaves at worst a partly read file; keep serving.
    shared.lock().unwrap_or_else(|e| e.into_inner())
}

/// What is known about one transcript.
#[derive(Debug)]
struct FileState {
    dev: u64,
    ino: u64,
    mtime: DateTime<Utc>,
    /// Bytes read so far, always at a line boundary.
    offset: u64,
    parser: UsageParser,
}

/// Directories and transcripts found under a directory.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct Walk {
    /// Directories worth watching.
    pub dirs: Vec<PathBuf>,
    /// Transcripts written in the last 7 days.
    pub files: Vec<PathBuf>,
}

/// Per-file usage records under one `projects` directory.
#[derive(Debug)]
pub struct Tracker {
    root: PathBuf,
    files: BTreeMap<PathBuf, FileState>,
    last_prune: Option<DateTime<Utc>>,
    /// Whether the first scan is done, so the totals are real.
    ready: bool,
}

impl Tracker {
    pub fn new(root: PathBuf) -> Tracker {
        Tracker {
            root,
            files: BTreeMap::new(),
            last_prune: None,
            ready: false,
        }
    }

    /// Reads every recent transcript under the root, dropping state for files that are
    /// gone or old. Returns the directories to watch.
    pub fn rescan(&mut self, now: DateTime<Utc>) -> Vec<PathBuf> {
        let walk = self.walk(&self.root.clone(), now);
        let found: HashSet<&PathBuf> = walk.files.iter().collect();
        self.files.retain(|path, _| found.contains(path));
        for file in &walk.files {
            self.update_file(file, now);
        }
        walk.dirs
    }

    /// Marks the totals as real once the first scan is done.
    pub fn set_ready(&mut self) {
        self.ready = true;
    }

    /// Finds the directories and recent transcripts under `dir`, which is the root or
    /// lies in it. Project directories (the root's children) and directories that
    /// were changed or hold a transcript in the last 7 days are worth watching, as is
    /// the `subagents` tree of any session directory worth watching. `dir` itself is
    /// taken as worth watching when it is a project or session directory or lies in
    /// a `subagents` tree.
    pub fn walk(&self, dir: &Path, now: DateTime<Utc>) -> Walk {
        let rel = dir.strip_prefix(&self.root).ok();
        let depth = rel.map_or(0, |rel| rel.components().count());
        let in_subagents = rel
            .and_then(|rel| rel.components().nth(2))
            .is_some_and(|c| c.as_os_str() == "subagents");
        let mut walk = Walk::default();
        walk_dir(
            dir,
            depth,
            now - SEVEN_DAYS,
            depth <= 2 || in_subagents,
            &mut walk,
        );
        walk
    }

    /// Reads what was appended to `path` since the last read, starting over when the
    /// file shrank or is another file now. Forgets files that are gone or were last
    /// written more than 7 days ago.
    pub fn update_file(&mut self, path: &Path, now: DateTime<Utc>) {
        let meta = match fs::metadata(path) {
            Ok(meta) if meta.is_file() => meta,
            _ => {
                self.files.remove(path);
                return;
            }
        };
        let mtime = meta.modified().map_or(now, DateTime::<Utc>::from);
        if mtime < now - SEVEN_DAYS {
            self.files.remove(path);
            return;
        }
        let state = self
            .files
            .entry(path.to_owned())
            .or_insert_with(|| FileState::new(&meta));
        if state.dev != meta.dev() || state.ino != meta.ino() || meta.len() < state.offset {
            *state = FileState::new(&meta);
        }
        state.mtime = mtime;
        if meta.len() == state.offset {
            return;
        }
        if let Err(e) = state.read_new(path) {
            eprintln!("refract-agent: cannot read {}: {e}", path.display());
        }
    }

    /// Forgets `path` and every transcript under it.
    pub fn remove_under(&mut self, path: &Path) {
        self.files.retain(|p, _| !p.starts_with(path));
    }

    /// Drops records older than 7 days, and files with none left that were last
    /// written before then.
    pub fn prune(&mut self, now: DateTime<Utc>) {
        let cutoff = now - SEVEN_DAYS;
        self.files.retain(|_, state| {
            state.parser.prune(cutoff);
            !state.parser.records().is_empty() || state.mtime >= cutoff
        });
        self.last_prune = Some(now);
    }

    /// Adds up the records in memory (see [`usage::summarize`]), pruning first at most
    /// once a minute.
    pub fn summary(
        &mut self,
        repos: &[String],
        now: DateTime<Utc>,
        today_start: DateTime<Utc>,
        live_agents: u32,
    ) -> UsageSummary {
        if self.last_prune.is_none_or(|at| now - at >= PRUNE_EVERY) {
            self.prune(now);
        }
        let files = self
            .files
            .values()
            .map(|f| (f.parser.cwd(), f.parser.records()));
        UsageSummary {
            ready: self.ready,
            ..usage::summarize(files, repos, now, today_start, live_agents)
        }
    }

    /// Transcripts held, for tests and logs.
    pub fn file_count(&self) -> usize {
        self.files.len()
    }
}

impl FileState {
    fn new(meta: &fs::Metadata) -> FileState {
        FileState {
            dev: meta.dev(),
            ino: meta.ino(),
            mtime: DateTime::<Utc>::UNIX_EPOCH,
            offset: 0,
            parser: UsageParser::default(),
        }
    }

    /// Feeds the complete lines after `offset` to the parser and moves `offset` past
    /// them. A trailing line with no newline yet is left for the next read.
    fn read_new(&mut self, path: &Path) -> io::Result<()> {
        let mut file = File::open(path)?;
        file.seek(SeekFrom::Start(self.offset))?;
        let mut buf: Vec<u8> = Vec::with_capacity(CHUNK);
        let mut chunk = vec![0u8; CHUNK];
        loop {
            let n = match file.read(&mut chunk) {
                Ok(0) => return Ok(()),
                Ok(n) => n,
                Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
                Err(e) => return Err(e),
            };
            buf.extend_from_slice(&chunk[..n]);
            let Some(last) = buf.iter().rposition(|&b| b == b'\n') else {
                continue;
            };
            for line in buf[..last].split(|&b| b == b'\n') {
                // Lines that are not UTF-8 are damaged and hold nothing to count.
                if let Ok(line) = std::str::from_utf8(line) {
                    self.parser.push_line(line);
                }
            }
            self.offset += (last + 1) as u64;
            buf.drain(..=last);
        }
    }
}

/// Walks `dir`, `depth` below the root, returning whether it was changed or holds a
/// transcript in the last 7 days. `kept` directories are watched whatever their age;
/// below a session directory (depth 2), its `subagents` tree is kept, and dropped
/// again unless the session directory is recent or kept itself.
fn walk_dir(dir: &Path, depth: usize, cutoff: DateTime<Utc>, kept: bool, walk: &mut Walk) -> bool {
    let Ok(entries) = fs::read_dir(dir) else {
        return false;
    };
    let mut recent = depth == 1
        || fs::metadata(dir)
            .and_then(|m| m.modified())
            .is_ok_and(|t| DateTime::<Utc>::from(t) >= cutoff);
    let at = walk.dirs.len();
    for entry in entries.flatten() {
        let Ok(kind) = entry.file_type() else {
            continue;
        };
        let path = entry.path();
        if kind.is_dir() {
            let child_kept =
                (depth >= 3 && kept) || (depth == 2 && entry.file_name() == "subagents");
            if depth < MAX_DEPTH && walk_dir(&path, depth + 1, cutoff, child_kept, walk) {
                recent = true;
            }
        } else if kind.is_file() && path.extension().is_some_and(|e| e == "jsonl") {
            let written = entry.metadata().and_then(|m| m.modified());
            if written.is_ok_and(|t| DateTime::<Utc>::from(t) >= cutoff) {
                walk.files.push(path);
                recent = true;
            }
        }
    }
    if !recent && !kept {
        // Nothing under here is recent, so all that was added is a kept `subagents`
        // tree of a session that is not watched.
        walk.dirs.truncate(at);
    } else if depth > 0 {
        walk.dirs.insert(at, dir.to_owned());
    }
    recent
}

/// The thread's inotify watches, by descriptor and by path.
struct DirWatches {
    watches: Watches,
    by_wd: HashMap<i32, (WatchDescriptor, PathBuf)>,
    paths: HashSet<PathBuf>,
}

impl DirWatches {
    fn add(&mut self, dir: &Path) -> io::Result<()> {
        if self.paths.contains(dir) {
            return Ok(());
        }
        let mask = WatchMask::MODIFY
            | WatchMask::CLOSE_WRITE
            | WatchMask::CREATE
            | WatchMask::DELETE
            | WatchMask::MOVED_FROM
            | WatchMask::MOVED_TO
            | WatchMask::ONLYDIR
            | WatchMask::DONT_FOLLOW;
        let wd = self.watches.add(dir, mask)?;
        self.by_wd
            .insert(wd.get_watch_descriptor_id(), (wd, dir.to_owned()));
        self.paths.insert(dir.to_owned());
        Ok(())
    }

    fn add_all(&mut self, dirs: &[PathBuf]) {
        for dir in dirs {
            match self.add(dir) {
                Ok(()) => {}
                Err(e) if e.kind() == io::ErrorKind::NotFound => {}
                Err(e) => eprintln!("refract-agent: cannot watch {}: {e}", dir.display()),
            }
        }
    }

    /// Forgets a watch the kernel dropped.
    fn forget(&mut self, wd: i32) {
        if let Some((_, path)) = self.by_wd.remove(&wd) {
            self.paths.remove(&path);
        }
    }

    /// Stops watching `dir` and everything under it, for a directory moved away.
    fn remove_under(&mut self, dir: &Path) {
        let gone: Vec<i32> = self
            .by_wd
            .iter()
            .filter(|(_, (_, p))| p.starts_with(dir))
            .map(|(id, _)| *id)
            .collect();
        for id in gone {
            if let Some((wd, path)) = self.by_wd.remove(&id) {
                let _ = self.watches.remove(wd);
                self.paths.remove(&path);
            }
        }
    }
}

/// What one batch of events asks for.
#[derive(Default)]
struct Batch {
    rescan: bool,
    removed: Vec<PathBuf>,
    moved_away: Vec<PathBuf>,
    new_dirs: Vec<PathBuf>,
    files: Vec<PathBuf>,
}

/// The usage thread: first scan, then updates on every batch of events.
fn run(shared: &Mutex<Tracker>, root: PathBuf) {
    let mut inotify = match Inotify::init() {
        Ok(inotify) => inotify,
        Err(e) => {
            eprintln!("refract-agent: inotify unavailable, usage will not update: {e}");
            let mut tracker = Tracker::new(root);
            tracker.rescan(Utc::now());
            tracker.set_ready();
            *lock(shared) = tracker;
            return;
        }
    };
    let mut watches = DirWatches {
        watches: inotify.watches(),
        by_wd: HashMap::new(),
        paths: HashSet::new(),
    };
    // Claude Code creates `projects` on first use; wait for it.
    loop {
        match watches.add(&root) {
            Ok(()) => break,
            Err(e) if e.kind() == io::ErrorKind::NotFound => {
                // With no transcripts at all, the zeros are real.
                lock(shared).set_ready();
                std::thread::sleep(MISSING_ROOT_RETRY);
            }
            Err(e) => {
                eprintln!(
                    "refract-agent: cannot watch {}, usage will not update: {e}",
                    root.display()
                );
                break;
            }
        }
    }

    let started = std::time::Instant::now();
    let mut tracker = Tracker::new(root.clone());
    let now = Utc::now();
    for file in watch_and_walk(&tracker, &mut watches, &root, now) {
        tracker.update_file(&file, now);
    }
    eprintln!(
        "refract-agent: usage read {} transcripts in {} ms, {} watches",
        tracker.file_count(),
        started.elapsed().as_millis(),
        watches.by_wd.len()
    );
    tracker.set_ready();
    *lock(shared) = tracker;

    let mut buffer = vec![0u8; 64 * 1024];
    loop {
        let mut batch = Batch::default();
        match inotify.read_events_blocking(&mut buffer) {
            Ok(events) => {
                for event in events {
                    let wd = event.wd.get_watch_descriptor_id();
                    classify(&mut batch, &mut watches, wd, event.mask, event.name);
                }
            }
            Err(e) => {
                eprintln!("refract-agent: usage inotify read failed: {e}");
                return;
            }
        }
        apply(&mut lock(shared), &mut watches, batch, Utc::now());
    }
}

/// Watches `dir` and the directories under it worth watching, then walks it again so
/// that transcripts written before their directory was watched are found too. Returns
/// the recent transcripts; reading one twice from its offset is harmless.
fn watch_and_walk(
    tracker: &Tracker,
    watches: &mut DirWatches,
    dir: &Path,
    now: DateTime<Utc>,
) -> Vec<PathBuf> {
    if dir != tracker.root {
        watches.add_all(std::slice::from_ref(&dir.to_path_buf()));
    }
    watches.add_all(&tracker.walk(dir, now).dirs);
    let walk = tracker.walk(dir, now);
    watches.add_all(&walk.dirs);
    walk.files
}

fn classify(
    batch: &mut Batch,
    watches: &mut DirWatches,
    wd: i32,
    mask: EventMask,
    name: Option<&std::ffi::OsStr>,
) {
    if mask.contains(EventMask::Q_OVERFLOW) {
        batch.rescan = true;
        return;
    }
    let Some((_, dir)) = watches.by_wd.get(&wd) else {
        return;
    };
    let dir = dir.clone();
    if mask.contains(EventMask::IGNORED) {
        watches.forget(wd);
        batch.removed.push(dir);
        return;
    }
    let Some(name) = name else { return };
    let path = dir.join(name);
    let away = mask.intersects(EventMask::DELETE | EventMask::MOVED_FROM);
    if mask.contains(EventMask::ISDIR) {
        if mask.contains(EventMask::MOVED_FROM) {
            batch.moved_away.push(path.clone());
        }
        if away {
            batch.removed.push(path);
        } else {
            batch.new_dirs.push(path);
        }
    } else if path.extension().is_some_and(|e| e == "jsonl") {
        if away {
            batch.removed.push(path);
        } else {
            batch.files.push(path);
        }
    }
}

fn apply(tracker: &mut Tracker, watches: &mut DirWatches, batch: Batch, now: DateTime<Utc>) {
    if batch.rescan {
        let dirs = tracker.rescan(now);
        watches.add_all(&dirs);
        return;
    }
    for dir in &batch.moved_away {
        watches.remove_under(dir);
    }
    for path in &batch.removed {
        tracker.remove_under(path);
    }
    let mut new_dirs = batch.new_dirs;
    let mut files: Vec<PathBuf> = batch.files;
    files.sort();
    files.dedup();
    // A resumed session can start new subagent transcripts in an old session
    // directory that is not watched yet.
    for file in &files {
        let session_dir = file.with_extension("");
        if !watches.paths.contains(&session_dir) && session_dir.is_dir() {
            new_dirs.push(session_dir);
        }
    }
    for dir in new_dirs {
        files.extend(watch_and_walk(tracker, watches, &dir, now));
    }
    files.sort();
    files.dedup();
    for file in &files {
        tracker.update_file(file, now);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    fn record(id: &str, at: DateTime<Utc>, input: u64) -> String {
        format!(
            r#"{{"type":"assistant","timestamp":"{}","cwd":"/home/dev/projects/kelp-cli","requestId":"req_{id}","message":{{"id":"{id}","usage":{{"input_tokens":{input},"output_tokens":0}}}}}}"#,
            at.to_rfc3339()
        )
    }

    fn input_total(tracker: &mut Tracker, now: DateTime<Utc>) -> u64 {
        tracker
            .summary(&[], now, now - TimeDelta::hours(1), 0)
            .last_7_days
            .input
    }

    fn append(path: &Path, text: &str) {
        let mut f = fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(path)
            .unwrap();
        f.write_all(text.as_bytes()).unwrap();
    }

    #[test]
    fn appends_are_read_from_the_offset_and_partial_lines_wait() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().join("-home-dev-projects-kelp-cli");
        fs::create_dir_all(&dir).unwrap();
        let path = dir.join("a.jsonl");
        let now = Utc::now();
        append(&path, &(record("m1", now, 5) + "\n"));
        let mut tracker = Tracker::new(tmp.path().to_owned());
        let dirs = tracker.rescan(now);
        assert_eq!(dirs, [dir]);
        assert_eq!(input_total(&mut tracker, now), 5);

        let line = record("m2", now, 7);
        let (head, tail) = line.split_at(line.len() / 2);
        append(&path, head);
        tracker.update_file(&path, now);
        assert_eq!(input_total(&mut tracker, now), 5);
        append(&path, &format!("{tail}\n"));
        tracker.update_file(&path, now);
        assert_eq!(input_total(&mut tracker, now), 12);
        // Nothing new: nothing changes.
        tracker.update_file(&path, now);
        assert_eq!(input_total(&mut tracker, now), 12);
    }

    #[test]
    fn a_shrunk_or_replaced_file_is_read_again() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("a.jsonl");
        let now = Utc::now();
        append(
            &path,
            &(record("m1", now, 5) + "\n" + &record("m2", now, 7) + "\n"),
        );
        let mut tracker = Tracker::new(tmp.path().to_owned());
        tracker.update_file(&path, now);
        assert_eq!(input_total(&mut tracker, now), 12);

        fs::write(&path, record("m3", now, 1) + "\n").unwrap();
        tracker.update_file(&path, now);
        assert_eq!(input_total(&mut tracker, now), 1);

        let other = tmp.path().join("b.tmp");
        let long = (record("m4", now, 100) + "\n").repeat(3);
        fs::write(&other, long).unwrap();
        fs::rename(&other, &path).unwrap();
        tracker.update_file(&path, now);
        assert_eq!(input_total(&mut tracker, now), 100);

        fs::remove_file(&path).unwrap();
        tracker.update_file(&path, now);
        assert_eq!(tracker.file_count(), 0);
    }

    #[test]
    fn old_files_are_skipped_and_old_records_pruned() {
        let tmp = tempfile::tempdir().unwrap();
        let project = tmp.path().join("-home-dev-projects-kelp-cli");
        let session = project.join("3f6c1a52-0000-4000-8000-000000000001");
        let subagents = session.join("subagents");
        fs::create_dir_all(&subagents).unwrap();
        let now = Utc::now();
        let old = project.join("old.jsonl");
        append(&old, &(record("m1", now - TimeDelta::days(9), 5) + "\n"));
        let nine_days = std::time::SystemTime::now() - Duration::from_secs(9 * 24 * 3600);
        File::options()
            .write(true)
            .open(&old)
            .unwrap()
            .set_modified(nine_days)
            .unwrap();
        let sub = subagents.join("agent-1.jsonl");
        append(
            &sub,
            &(record("m2", now - TimeDelta::days(8), 3) + "\n" + &record("m3", now, 4) + "\n"),
        );
        let mut tracker = Tracker::new(tmp.path().to_owned());
        let dirs = tracker.rescan(now);
        assert_eq!(dirs, [project, session, subagents]);
        assert_eq!(tracker.file_count(), 1);
        assert_eq!(input_total(&mut tracker, now), 4);
        // Records age out as time passes, even with no new writes.
        let later = now + TimeDelta::days(8);
        assert_eq!(input_total(&mut tracker, later), 0);
        assert_eq!(tracker.file_count(), 0);
    }

    fn age(path: &Path, days: u64) {
        let then = std::time::SystemTime::now() - Duration::from_secs(days * 24 * 3600);
        File::open(path).unwrap().set_modified(then).unwrap();
    }

    #[test]
    fn subagent_dirs_of_a_watched_session_are_kept_whatever_their_age() {
        let tmp = tempfile::tempdir().unwrap();
        let project = tmp.path().join("-home-dev-projects-kelp-cli");
        let session = project.join("3f6c1a52-0000-4000-8000-000000000001");
        let subagents = session.join("subagents");
        let run = subagents.join("workflows/run-1");
        fs::create_dir_all(&run).unwrap();
        fs::create_dir_all(session.join("tool-results")).unwrap();
        for dir in [
            &run,
            &run.parent().unwrap().to_owned(),
            &subagents,
            &session,
        ] {
            age(dir, 30);
        }
        age(&session.join("tool-results"), 30);
        let now = Utc::now();
        let tracker = Tracker::new(tmp.path().to_owned());
        // An old session is not watched, nor its subagents.
        assert_eq!(tracker.walk(tmp.path(), now).dirs, [project]);
        // Once it is (a resumed session), its whole subagents tree is.
        assert_eq!(
            tracker.walk(&session, now).dirs,
            [
                session.clone(),
                subagents.clone(),
                subagents.join("workflows"),
                run.clone()
            ]
        );
        assert_eq!(
            tracker.walk(&subagents, now).dirs,
            [subagents.clone(), subagents.join("workflows"), run]
        );
    }

    #[test]
    fn usage_is_not_ready_until_the_first_scan_and_never_waits() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("a.jsonl");
        let now = Utc::now();
        append(&path, &(record("m1", now, 5) + "\n"));
        let usage = Usage {
            shared: Arc::new(Mutex::new(Tracker::new(tmp.path().to_owned()))),
            last: Mutex::new(None),
        };
        let first = usage.summary(&[], 0);
        assert!(!first.ready);
        assert_eq!(first.last_7_days.input, 0);

        // Busy before any summary: zeros, not ready.
        let held = lock(&usage.shared);
        let busy = usage.summary(&[], 1);
        assert!(!busy.ready);
        assert_eq!(busy.live_agents, 1);
        drop(held);

        {
            let mut tracker = lock(&usage.shared);
            tracker.update_file(&path, now);
            tracker.set_ready();
        }
        let done = usage.summary(&[], 2);
        assert!(done.ready);
        assert_eq!(done.last_7_days.input, 5);

        // Busy again: the last summary, with the live agents of now.
        let mut held = lock(&usage.shared);
        held.remove_under(tmp.path());
        let busy = usage.summary(&[], 3);
        assert!(busy.ready);
        assert_eq!(busy.last_7_days.input, 5);
        assert_eq!(busy.live_agents, 3);
        assert_eq!(busy.computed_at, done.computed_at);
        drop(held);
        assert_eq!(usage.summary(&[], 3).last_7_days.input, 0);
    }

    #[test]
    fn local_midnight_is_at_or_before_now() {
        let now = Utc::now();
        let midnight = local_midnight(now);
        assert!(midnight <= now);
        assert!(now - midnight < TimeDelta::hours(26));
    }
}
