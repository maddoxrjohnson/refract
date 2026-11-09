//! Reading a repo's Claude Code transcripts and Codex CLI rollouts.
//!
//! Only the start and the end of each transcript are read, and summaries are cached
//! until the file's modification time or size changes. The start says who the session
//! belongs to; the end says how it stands.
//!
//! Codex keeps every repo's rollouts together, by day, in
//! `$CODEX_HOME/sessions/YYYY/MM/DD/rollout-*.jsonl` (codex spec §3), so they are
//! indexed by the `cwd` their first line names. Only that first line is read for the
//! index; the end of a rollout is read when a repo asks for its newest sessions, and
//! after that only the lines appended since (a rollout only grows). The whole tree is
//! listed again when a watch says it changed ([`SessionReader::codex_changed`]) or the
//! last listing is older than [`CODEX_RELIST`]; a watch naming the files that changed
//! looks at just those ([`SessionReader::codex_files_changed`]).

use std::collections::{HashMap, HashSet};
use std::fs::{self, File};
use std::io::{self, Read, Seek, SeekFrom};
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant, SystemTime};

use chrono::{DateTime, Utc};
use refract_core::codex_transcript::{self, CodexHead, CodexParser, CodexSummary};
use refract_core::sessions::{paths_match, project_dir_matches, within_deepest};
use refract_core::transcript::{last_reply, parse_lines, SessionSummary};

/// How much of the end of a transcript is read.
pub const TAIL_BYTES: u64 = 256 * 1024;

/// How far the tail window grows when the last line is longer than [`TAIL_BYTES`].
pub const MAX_TAIL_BYTES: u64 = 8 * 1024 * 1024;

/// How much of the start of a transcript is read for its session id and starting cwd.
pub const HEAD_BYTES: u64 = 64 * 1024;

/// How many of a repo's most recent transcripts are read and reported, per agent.
pub const MAX_SESSIONS: usize = 20;

/// How much of the start of a rollout is read for its first line, `session_meta`, which
/// carries Codex's base instructions and can be tens of KiB long.
pub const CODEX_HEAD_BYTES: u64 = 256 * 1024;

/// How old the listing of the rollouts may get before a read lists them again, when no
/// watch said they changed (one may have been missed, or not be set up yet). Watch events
/// re-read just the files they name, so this is only a fallback, kept long because a
/// listing stats every rollout ever written.
pub const CODEX_RELIST: Duration = Duration::from_secs(60);

/// What the start of a transcript says.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Head {
    pub session_id: Option<String>,
    /// The directory the session started in. Claude may `cd` elsewhere later, so this,
    /// not a cwd in the tail, decides which repo the session belongs to.
    pub cwd: Option<String>,
}

#[derive(Debug)]
struct Cached {
    mtime: SystemTime,
    size: u64,
    head: Head,
    summary: SessionSummary,
}

/// What a listing says about a rollout file.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct RolloutStat {
    mtime: SystemTime,
    size: u64,
    /// A new inode under the same name is a new file.
    ino: u64,
}

impl RolloutStat {
    fn of(meta: &fs::Metadata) -> Option<RolloutStat> {
        Some(RolloutStat {
            mtime: meta.modified().ok()?,
            size: meta.len(),
            ino: meta.ino(),
        })
    }
}

#[derive(Debug)]
struct CachedRollout {
    stat: RolloutStat,
    /// What its first line says, with the session id from the file name when that line
    /// names none. Read again until it names a cwd (the line was still being written).
    head: CodexHead,
    /// Its end as parsed so far, once a repo asked for it.
    tail: Option<RolloutTail>,
}

/// A rollout's end, parsed up to `offset`.
#[derive(Debug)]
struct RolloutTail {
    parser: CodexParser,
    /// Just past the last complete line parsed.
    offset: u64,
}

/// Reads and caches transcript summaries.
#[derive(Debug)]
pub struct SessionReader {
    projects_dir: PathBuf,
    cache: HashMap<PathBuf, Cached>,
    /// Codex's `sessions` directory; `None` reads no rollouts.
    codex_dir: Option<PathBuf>,
    rollouts: HashMap<PathBuf, CachedRollout>,
    /// When the rollouts were last listed; `None` when a watch said they changed.
    codex_listed: Option<Instant>,
    /// The starting cwds of rollouts added, changed or removed since they were last
    /// taken ([`SessionReader::relist_codex`]).
    codex_touched: Vec<String>,
}

impl SessionReader {
    /// A reader for Claude Code's `projects` directory.
    pub fn new(projects_dir: PathBuf) -> SessionReader {
        SessionReader {
            projects_dir,
            cache: HashMap::new(),
            codex_dir: None,
            rollouts: HashMap::new(),
            codex_listed: None,
            codex_touched: Vec::new(),
        }
    }

    /// The same reader, also reading Codex rollouts from `sessions_dir`
    /// (`$CODEX_HOME/sessions`).
    pub fn with_codex(mut self, sessions_dir: PathBuf) -> SessionReader {
        self.codex_dir = Some(sessions_dir);
        self
    }

    /// Codex's `sessions` directory, when rollouts are read.
    pub fn codex_dir(&self) -> Option<&Path> {
        self.codex_dir.as_deref()
    }

    /// Says the rollouts changed, so the next read lists them again.
    pub fn codex_changed(&mut self) {
        self.codex_listed = None;
    }

    /// Lists the rollouts again when they changed or the listing is stale, reading the
    /// first line of new ones. Returns the starting cwds of the rollouts that were
    /// added, changed or removed since the last call (reads in between included), for
    /// the repos whose sessions moved.
    pub fn relist_codex(&mut self) -> Vec<String> {
        self.relist();
        self.take_touched()
    }

    /// Looks again at just `paths`, rollouts a watch said were written, added or
    /// removed, and returns the starting cwds whose rollouts moved, as
    /// [`SessionReader::relist_codex`] does.
    pub fn codex_files_changed(&mut self, paths: &[PathBuf]) -> Vec<String> {
        let Some(dir) = self.codex_dir.clone() else {
            return Vec::new();
        };
        for path in paths.iter().filter(|p| p.starts_with(&dir)) {
            let stat = fs::metadata(path)
                .ok()
                .filter(fs::Metadata::is_file)
                .and_then(|m| RolloutStat::of(&m));
            match stat {
                Some(stat) => self.note_rollout(path, stat),
                None => {
                    if let Some(gone) = self.rollouts.remove(path) {
                        self.codex_touched.extend(gone.head.cwd);
                    }
                }
            }
        }
        self.take_touched()
    }

    fn take_touched(&mut self) -> Vec<String> {
        let mut touched = std::mem::take(&mut self.codex_touched);
        touched.sort();
        touched.dedup();
        touched
    }

    /// Lists the rollouts again when a watch said they changed or the listing is stale.
    fn relist(&mut self) {
        let fresh = self
            .codex_listed
            .is_some_and(|at| at.elapsed() < CODEX_RELIST);
        let Some(dir) = self.codex_dir.clone().filter(|_| !fresh) else {
            return;
        };
        self.codex_listed = Some(Instant::now());
        let mut files = Vec::new();
        list_rollouts(&dir, 0, &mut files);
        let listed: HashSet<&PathBuf> = files.iter().map(|(p, _)| p).collect();
        let touched = &mut self.codex_touched;
        self.rollouts.retain(|path, cached| {
            let keep = listed.contains(path);
            if !keep {
                touched.extend(cached.head.cwd.clone());
            }
            keep
        });
        for (path, stat) in &files {
            self.note_rollout(path, *stat);
        }
        // Kept until taken, which may be a while without a watch.
        self.codex_touched.sort();
        self.codex_touched.dedup();
    }

    /// Takes in what a listing says about the rollout at `path`. A new or changed one
    /// counts as touched, and its first line is read when it is new, was replaced or
    /// shrank, or named no cwd yet. Its end is read later, when asked for.
    fn note_rollout(&mut self, path: &Path, stat: RolloutStat) {
        if self.rollouts.get(path).is_some_and(|c| c.stat == stat) {
            return;
        }
        let old = self.rollouts.remove(path);
        if let Some(cwd) = old.as_ref().and_then(|c| c.head.cwd.clone()) {
            self.codex_touched.push(cwd);
        }
        // A rollout only grows: one that shrank or is a new file is read afresh.
        let same = old.filter(|c| c.stat.ino == stat.ino && c.stat.size <= stat.size);
        let (head, tail) = match same {
            Some(c) if c.head.cwd.is_some() => (c.head, c.tail),
            same => match read_rollout_head(path) {
                Ok(head) => (head, same.and_then(|c| c.tail)),
                Err(e) => {
                    eprintln!("refract-agent: cannot read {}: {e}", path.display());
                    return;
                }
            },
        };
        self.codex_touched.extend(head.cwd.clone());
        self.rollouts
            .insert(path.to_owned(), CachedRollout { stat, head, tail });
    }

    /// The Codex sessions that started in `repo` or a directory in it (but not in one
    /// of `nested`, known repos inside it, which take their own), newest first, at most
    /// [`MAX_SESSIONS`], leaving out `codex exec` sessions and threads Codex spawned.
    /// Each summary has `session_id`, `byte_size` and `modified` set. Lists the
    /// rollouts again first when needed (see [`SessionReader::relist_codex`]).
    pub fn read_codex_repo(&mut self, repo: &str, nested: &[&str]) -> Vec<CodexSummary> {
        self.relist();
        let mut paths: Vec<(PathBuf, SystemTime)> = self
            .rollouts
            .iter()
            .filter(|(_, c)| !c.head.is_hidden())
            .filter(|(_, c)| {
                c.head
                    .cwd
                    .as_deref()
                    .is_some_and(|cwd| within_deepest(cwd, repo, nested))
            })
            .map(|(path, c)| (path.clone(), c.stat.mtime))
            .collect();
        paths.sort_by_key(|(_, mtime)| std::cmp::Reverse(*mtime));
        paths.truncate(MAX_SESSIONS);
        let mut out = Vec::new();
        for (path, _) in paths {
            let Some(cached) = self.rollouts.get_mut(&path) else {
                continue;
            };
            match read_rollout_tail(&path, cached) {
                Ok(()) => out.push(rollout_summary(cached)),
                Err(e) => eprintln!("refract-agent: cannot read {}: {e}", path.display()),
            }
        }
        out.sort_by_key(|s| std::cmp::Reverse(s.summary.last_timestamp.or(s.summary.modified)));
        out
    }

    /// True when the rollouts listed so far include session `id`.
    pub fn has_rollout(&self, id: &str) -> bool {
        self.rollouts
            .values()
            .any(|c| c.head.session_id.as_deref() == Some(id))
    }

    /// True when session `id`'s rollout, as listed so far, is one Refract leaves out: a
    /// thread Codex spawned or an exec session.
    pub fn is_hidden_rollout(&self, id: &str) -> bool {
        self.rollouts
            .values()
            .any(|c| c.head.session_id.as_deref() == Some(id) && c.head.is_hidden())
    }

    /// The assistant's last reply in session `id`'s transcript, in full, from its last
    /// [`TAIL_BYTES`], for a brief; `None` when the transcript is not known or has none.
    pub fn last_reply(&self, id: &str) -> Option<String> {
        let (path, cached) = self
            .cache
            .iter()
            .find(|(_, c)| c.summary.session_id.as_deref() == Some(id))?;
        let mut file = File::open(path).ok()?;
        let start = cached.size.saturating_sub(TAIL_BYTES);
        file.seek(SeekFrom::Start(start)).ok()?;
        let mut buf = Vec::new();
        file.take(TAIL_BYTES).read_to_end(&mut buf).ok()?;
        let body = if start > 0 {
            buf.iter()
                .position(|&b| b == b'\n')
                .map_or(&buf[buf.len()..], |i| &buf[i + 1..])
        } else {
            &buf[..]
        };
        last_reply(String::from_utf8_lossy(body).lines())
    }

    /// The `projects` directory this reader looks in.
    pub fn projects_dir(&self) -> &Path {
        &self.projects_dir
    }

    /// Project directories that may hold transcripts for `repo`. Usually one; more
    /// under `/mnt/` where the same folder was opened with different casing.
    pub fn project_dirs(&self, repo: &str) -> Vec<PathBuf> {
        let Ok(entries) = fs::read_dir(&self.projects_dir) else {
            return Vec::new();
        };
        entries
            .flatten()
            .filter(|e| project_dir_matches(&e.file_name().to_string_lossy(), repo))
            .map(|e| e.path())
            .collect()
    }

    /// Summaries of the most recently written sessions that belong to `repo`, ordered
    /// by last activity, newest first. Each has `session_id`, `byte_size` and
    /// `modified` set, and `cwd` is the directory the session started in.
    ///
    /// A session belongs to `repo` when it started there (the first `cwd` at the start
    /// of the file). When the start names no `cwd`, a `cwd` in the tail at or under
    /// `repo` counts instead. Transcripts that cannot be read are left out.
    pub fn read_repo(&mut self, repo: &str) -> Vec<SessionSummary> {
        self.read_repo_within(repo, &[])
    }

    /// Like [`SessionReader::read_repo`], but a session whose `cwd` lies in one of
    /// `nested` (known repos inside `repo`, such as a worktree in its folder) belongs to
    /// that repo instead, the deepest one.
    pub fn read_repo_within(&mut self, repo: &str, nested: &[&str]) -> Vec<SessionSummary> {
        let mut files: Vec<(PathBuf, SystemTime, u64)> = self
            .project_dirs(repo)
            .iter()
            .filter_map(|dir| fs::read_dir(dir).ok())
            .flat_map(|entries| entries.flatten())
            .filter(|e| e.path().extension().is_some_and(|ext| ext == "jsonl"))
            .filter_map(|e| {
                let meta = e.metadata().ok().filter(|m| m.is_file())?;
                Some((e.path(), meta.modified().ok()?, meta.len()))
            })
            .collect();
        files.sort_by_key(|f| std::cmp::Reverse(f.1));
        files.truncate(MAX_SESSIONS);

        let mut out = Vec::new();
        for (path, mtime, size) in files {
            let fresh = self
                .cache
                .get(&path)
                .is_some_and(|c| c.mtime == mtime && c.size == size);
            if !fresh {
                if let Err(e) = self.load(&path, mtime, size) {
                    eprintln!("refract-agent: cannot read {}: {e}", path.display());
                    continue;
                }
            }
            let cached = &self.cache[&path];
            let belongs = match &cached.head.cwd {
                Some(start) => paths_match(start, repo),
                None => cached
                    .summary
                    .cwd
                    .as_deref()
                    .is_some_and(|cwd| within_deepest(cwd, repo, nested)),
            };
            if belongs {
                out.push(cached.summary.clone());
            }
        }
        out.sort_by_key(|s| std::cmp::Reverse(s.last_timestamp));
        out
    }

    /// The directory beside the transcript of session `id` that holds its subagents and
    /// tool results, from the transcripts read so far. It may not exist yet.
    pub fn session_dir(&self, id: &str) -> Option<PathBuf> {
        self.cache
            .iter()
            .find(|(_, c)| c.summary.session_id.as_deref() == Some(id))
            .map(|(path, _)| path.with_extension(""))
    }

    /// Drops cached summaries of transcripts in `dir`, for when it was removed.
    pub fn forget_dir(&mut self, dir: &Path) {
        self.cache.retain(|path, _| !path.starts_with(dir));
    }

    /// Reads a transcript into the cache. A transcript only grows, so the head is kept
    /// unless the file shrank (was rewritten).
    fn load(&mut self, path: &Path, mtime: SystemTime, size: u64) -> io::Result<()> {
        let old = self.cache.remove(path).filter(|c| c.size <= size);
        let head = match old.as_ref().map(|c| c.head.clone()) {
            Some(head) if head.cwd.is_some() => head,
            _ => read_head(path)?,
        };
        let tail = read_window(path, size)?;
        let mut summary = tail.summary;
        if summary.session_id.is_none() {
            summary.session_id = head.session_id.clone().or_else(|| file_session_id(path));
        }
        if head.cwd.is_some() {
            summary.cwd = head.cwd.clone();
        }
        let modified = DateTime::<Utc>::from(mtime);
        summary.modified = Some(modified);
        if summary.entry_count == 0 && tail.start > 0 {
            // The last line is longer than MAX_TAIL_BYTES, so how the session stands is
            // unknown. Its tail stays `None` (finished, asking nothing) and
            // the file's mtime stands in for its last activity, so it keeps its place.
            summary.last_timestamp = Some(modified);
        }

        self.cache.insert(
            path.to_owned(),
            Cached {
                mtime,
                size,
                head,
                summary,
            },
        );
        Ok(())
    }
}

/// Reads the session id and starting cwd from the first [`HEAD_BYTES`] of a transcript.
/// Fields the head does not name (a very long first line) stay `None`.
pub fn read_head(path: &Path) -> io::Result<Head> {
    let mut buf = Vec::new();
    File::open(path)?.take(HEAD_BYTES).read_to_end(&mut buf)?;
    // Only complete lines: a line cut by the window would just be malformed.
    let end = buf.iter().rposition(|&b| b == b'\n').map_or(0, |i| i + 1);
    let summary = parse_lines(String::from_utf8_lossy(&buf[..end]).lines());
    Ok(Head {
        session_id: summary.session_id,
        cwd: summary.cwd,
    })
}

/// Parses the last [`TAIL_BYTES`] of a transcript, starting at a line boundary, or the
/// whole file when it is smaller. When the window holds no whole entry (a last line
/// longer than the window) it doubles, up to [`MAX_TAIL_BYTES`]. Sets `byte_size` and
/// falls back to the file name for `session_id`.
pub fn read_tail(path: &Path, size: u64) -> io::Result<SessionSummary> {
    let mut summary = read_window(path, size)?.summary;
    if summary.session_id.is_none() {
        summary.session_id = file_session_id(path);
    }
    Ok(summary)
}

fn file_session_id(path: &Path) -> Option<String> {
    path.file_stem()
        .map(|stem| stem.to_string_lossy().into_owned())
}

/// Lists `rollout-*.jsonl` files under `dir`, three levels of directories deep
/// (`YYYY/MM/DD`), with what their metadata says.
fn list_rollouts(dir: &Path, depth: usize, out: &mut Vec<(PathBuf, RolloutStat)>) {
    let Ok(entries) = fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let Ok(kind) = entry.file_type() else {
            continue;
        };
        let path = entry.path();
        if kind.is_dir() && depth < 3 {
            list_rollouts(&path, depth + 1, out);
        } else if kind.is_file() && depth == 3 {
            let name = entry.file_name();
            let name = name.to_string_lossy();
            if !(name.starts_with("rollout-") && name.ends_with(".jsonl")) {
                continue;
            }
            if let Some(stat) = entry.metadata().ok().and_then(|m| RolloutStat::of(&m)) {
                out.push((path, stat));
            }
        }
    }
}

/// Reads a rollout's first line, up to [`CODEX_HEAD_BYTES`], with the session id from
/// the file name when the line names none.
fn read_rollout_head(path: &Path) -> io::Result<CodexHead> {
    let mut file = File::open(path)?;
    let mut buf = Vec::new();
    let mut chunk = [0u8; 16 * 1024];
    while (buf.len() as u64) < CODEX_HEAD_BYTES {
        let n = file.read(&mut chunk)?;
        if n == 0 {
            break;
        }
        let seen = buf.len();
        buf.extend_from_slice(&chunk[..n]);
        if let Some(i) = buf[seen..].iter().position(|&b| b == b'\n') {
            buf.truncate(seen + i + 1);
            break;
        }
    }
    let mut head = codex_transcript::parse_head(&buf);
    if head.session_id.is_none() {
        head.session_id = path
            .file_name()
            .and_then(|n| n.to_str())
            .and_then(codex_transcript::file_session_id)
            .map(str::to_owned);
    }
    Ok(head)
}

/// Brings a rollout's parsed end up to date. Lines appended since the last read are fed
/// to its parser. The first read takes the last [`TAIL_BYTES`], or the whole file when
/// smaller, and doubles the window, up to [`MAX_TAIL_BYTES`], until it holds a turn
/// marker, so a long turn's start is seen. Only complete lines are parsed; a line still
/// being written waits for the next read.
fn read_rollout_tail(path: &Path, cached: &mut CachedRollout) -> io::Result<()> {
    let mut file = File::open(path)?;
    let size = cached.stat.size.min(file.metadata()?.len());
    if let Some(tail) = cached.tail.as_mut().filter(|t| t.offset <= size) {
        if tail.offset < size {
            let buf = read_range(&mut file, tail.offset, size)?;
            tail.offset += feed_lines(&mut tail.parser, &buf) as u64;
        }
        return Ok(());
    }
    let mut window = TAIL_BYTES;
    loop {
        let start = size.saturating_sub(window);
        // From one byte early, as in `read_window`, so a line starting right at the
        // window is kept: the first, cut, line is dropped.
        let from = start.saturating_sub(1);
        let buf = read_range(&mut file, from, size)?;
        let body = if start > 0 {
            buf.iter()
                .position(|&b| b == b'\n')
                .map_or(buf.len(), |i| i + 1)
        } else {
            0
        };
        let mut parser = if start > 0 {
            CodexParser::partial()
        } else {
            CodexParser::default()
        };
        let used = feed_lines(&mut parser, &buf[body..]);
        let marked = parser.summary().end.is_some();
        if marked || start == 0 || window >= MAX_TAIL_BYTES {
            cached.tail = Some(RolloutTail {
                parser,
                offset: from + (body + used) as u64,
            });
            return Ok(());
        }
        window *= 2;
    }
}

/// The bytes of `file` from `from` to `to`.
fn read_range(file: &mut File, from: u64, to: u64) -> io::Result<Vec<u8>> {
    file.seek(SeekFrom::Start(from))?;
    let mut buf = Vec::with_capacity((to - from) as usize);
    file.take(to - from).read_to_end(&mut buf)?;
    Ok(buf)
}

/// Feeds the complete lines of `bytes` to `parser` and returns how many bytes they took.
fn feed_lines(parser: &mut CodexParser, bytes: &[u8]) -> usize {
    let used = bytes.iter().rposition(|&b| b == b'\n').map_or(0, |i| i + 1);
    for line in String::from_utf8_lossy(&bytes[..used]).lines() {
        parser.push_line(line);
    }
    used
}

/// A rollout's summary from its head and its parsed end.
fn rollout_summary(cached: &CachedRollout) -> CodexSummary {
    let tail = cached.tail.as_ref().map(|t| t.parser.summary());
    let mut summary = tail.unwrap_or_default().with_head(&cached.head);
    summary.summary = summary.summary.with_byte_size(cached.stat.size);
    let modified = DateTime::<Utc>::from(cached.stat.mtime);
    summary.summary.modified = Some(modified);
    if summary.summary.last_timestamp.is_none() {
        summary.summary.last_timestamp = Some(modified);
    }
    summary
}

/// A parsed tail window.
struct Window {
    summary: SessionSummary,
    /// Where the window starts; 0 when the whole file was read.
    start: u64,
}

fn read_window(path: &Path, size: u64) -> io::Result<Window> {
    let mut file = File::open(path)?;
    // The file may have shrunk since `size` was read; never read past its end.
    let size = size.min(file.metadata()?.len());
    let mut window = TAIL_BYTES;
    loop {
        let start = size.saturating_sub(window);
        // Read from one byte early so a window that starts right after a newline
        // keeps its first line. Cutting only at a newline also keeps multi-byte
        // characters whole.
        let from = start.saturating_sub(1);
        file.seek(SeekFrom::Start(from))?;
        let mut buf = Vec::with_capacity((size - from) as usize);
        (&mut file).take(size - from).read_to_end(&mut buf)?;
        let body = if start > 0 {
            buf.iter()
                .position(|&b| b == b'\n')
                .map_or(&buf[buf.len()..], |i| &buf[i + 1..])
        } else {
            &buf[..]
        };
        let summary = parse_lines(String::from_utf8_lossy(body).lines());
        if summary.entry_count > 0 || start == 0 || window >= MAX_TAIL_BYTES {
            return Ok(Window {
                summary: summary.with_byte_size(size),
                start,
            });
        }
        window *= 2;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use refract_core::transcript::Tail;
    use std::io::Write;

    const REPO: &str = "/home/dev/projects/tidepool-api";

    fn line(cwd: &str, text: &str) -> String {
        format!(
            r#"{{"type":"assistant","sessionId":"3f6c1a52-0000-4000-8000-000000000001","cwd":"{cwd}","timestamp":"2026-03-14T12:00:00Z","message":{{"id":"m-{text}","role":"assistant","content":[{{"type":"text","text":"{text}"}}]}}}}"#
        )
    }

    #[test]
    fn tail_of_large_file_starts_at_a_line_boundary() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp
            .path()
            .join("3f6c1a52-0000-4000-8000-000000000001.jsonl");
        let mut f = File::create(&path).unwrap();
        let filler = line(REPO, &"x".repeat(1000));
        while f.metadata().unwrap().len() < TAIL_BYTES * 2 {
            writeln!(f, "{filler}").unwrap();
        }
        writeln!(f, "{}", line(REPO, "last reply")).unwrap();
        let size = f.metadata().unwrap().len();
        let summary = read_tail(&path, size).unwrap();
        assert_eq!(summary.malformed_lines, 0);
        assert_eq!(summary.last_assistant_text.as_deref(), Some("last reply"));
        assert_eq!(summary.byte_size, Some(size));
        assert!(summary.entry_count < 300);
    }

    #[test]
    fn small_file_is_read_whole_and_named_by_file_when_needed() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp
            .path()
            .join("3f6c1a52-0000-4000-8000-0000000000aa.jsonl");
        fs::write(&path, "{\"type\":\"mode\",\"mode\":\"default\"}\n").unwrap();
        let summary = read_tail(&path, 33).unwrap();
        assert_eq!(
            summary.session_id.as_deref(),
            Some("3f6c1a52-0000-4000-8000-0000000000aa")
        );
        assert_eq!(summary.cwd, None);
    }

    #[test]
    fn repo_sessions_are_confirmed_by_cwd() {
        let tmp = tempfile::tempdir().unwrap();
        // `tidepool_api` encodes to the same directory as `tidepool-api`.
        let dir = tmp.path().join("-home-dev-projects-tidepool-api");
        fs::create_dir_all(&dir).unwrap();
        fs::write(dir.join("a.jsonl"), line(REPO, "mine") + "\n").unwrap();
        fs::write(
            dir.join("b.jsonl"),
            line("/home/dev/projects/tidepool_api", "other") + "\n",
        )
        .unwrap();
        fs::write(dir.join("notes.txt"), "ignored").unwrap();
        let mut reader = SessionReader::new(tmp.path().to_owned());
        let sessions = reader.read_repo(REPO);
        assert_eq!(sessions.len(), 1);
        assert_eq!(sessions[0].last_assistant_text.as_deref(), Some("mine"));
        let other = reader.read_repo("/home/dev/projects/tidepool_api");
        assert_eq!(other[0].last_assistant_text.as_deref(), Some("other"));
    }

    const ID: &str = "3f6c1a52-0000-4000-8000-000000000001";

    fn write_lines(path: &Path, lines: &[String]) {
        fs::write(path, lines.join("\n") + "\n").unwrap();
    }

    fn pending_tool(cwd: &str) -> String {
        format!(
            r#"{{"type":"assistant","sessionId":"{ID}","cwd":"{cwd}","timestamp":"2026-03-14T12:30:00Z","message":{{"id":"m-bash","role":"assistant","content":[{{"type":"tool_use","id":"t-bash","name":"Bash","input":{{"command":"cargo test"}}}}]}}}}"#
        )
    }

    fn big_tool_result(cwd: &str, bytes: usize) -> String {
        format!(
            r#"{{"type":"user","sessionId":"{ID}","cwd":"{cwd}","timestamp":"2026-03-14T12:31:00Z","message":{{"role":"user","content":[{{"type":"tool_result","tool_use_id":"t-bash","content":"{}"}}]}}}}"#,
            "A".repeat(bytes)
        )
    }

    fn repo_dir(tmp: &Path) -> PathBuf {
        let dir = tmp.join("-home-dev-projects-tidepool-api");
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn session_that_moved_into_a_subdirectory_still_belongs_to_its_repo() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = repo_dir(tmp.path());
        let sub = format!("{REPO}/src");
        let mut lines = vec![line(REPO, "start")];
        // Enough later lines in the subdirectory that the tail never sees the start.
        let filler = line(&sub, &"y".repeat(1000));
        while lines.len() * filler.len() < (TAIL_BYTES * 2) as usize {
            lines.push(filler.clone());
        }
        lines.push(line(&sub, "done in src"));
        write_lines(&dir.join(format!("{ID}.jsonl")), &lines);
        let mut reader = SessionReader::new(tmp.path().to_owned());
        let sessions = reader.read_repo(REPO);
        assert_eq!(sessions.len(), 1);
        assert_eq!(sessions[0].cwd.as_deref(), Some(REPO));
        assert_eq!(
            sessions[0].last_assistant_text.as_deref(),
            Some("done in src")
        );
        // Cached: the second read gives the same answer.
        assert_eq!(reader.read_repo(REPO), sessions);
    }

    #[test]
    fn session_that_left_the_repo_still_belongs_to_it() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = repo_dir(tmp.path());
        let lines = vec![line(REPO, "start"), line("/tmp", "elsewhere")];
        write_lines(&dir.join(format!("{ID}.jsonl")), &lines);
        let mut reader = SessionReader::new(tmp.path().to_owned());
        assert_eq!(reader.read_repo(REPO).len(), 1);
        assert!(reader
            .read_repo("/home/dev/projects/tidepool_api")
            .is_empty());
    }

    #[test]
    fn final_line_larger_than_the_tail_window_is_still_read() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = repo_dir(tmp.path());
        let lines = vec![
            line(REPO, "looking"),
            pending_tool(REPO),
            big_tool_result(REPO, 400 * 1024),
        ];
        write_lines(&dir.join(format!("{ID}.jsonl")), &lines);
        let mut reader = SessionReader::new(tmp.path().to_owned());
        let sessions = reader.read_repo(REPO);
        assert_eq!(sessions.len(), 1);
        assert_eq!(sessions[0].tail, Some(Tail::UnansweredToolResult));
        assert_eq!(sessions[0].malformed_lines, 0);
    }

    #[test]
    fn final_line_larger_than_the_cap_keeps_what_the_head_says() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join(format!("{ID}.jsonl"));
        let lines = vec![
            line(REPO, "looking"),
            big_tool_result(REPO, (MAX_TAIL_BYTES + 1024) as usize),
        ];
        write_lines(&path, &lines);
        let size = fs::metadata(&path).unwrap().len();
        let summary = read_tail(&path, size).unwrap();
        assert_eq!(summary.tail, None);
        assert!(!summary.asks_user);
        assert_eq!(summary.byte_size, Some(size));
    }

    #[test]
    fn window_starting_inside_a_multibyte_character_is_clean() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join(format!("{ID}.jsonl"));
        // A line of three-byte characters that the window starts in the middle of.
        let wide = line(REPO, &"漢".repeat(TAIL_BYTES as usize / 2));
        let lines = vec![wide, line(REPO, "after")];
        write_lines(&path, &lines);
        let size = fs::metadata(&path).unwrap().len();
        for offset in 0..3 {
            let summary = read_tail(&path, size - offset).unwrap();
            assert!(summary.malformed_lines <= 1, "offset {offset}");
        }
        let summary = read_tail(&path, size).unwrap();
        assert_eq!(summary.malformed_lines, 0);
        assert_eq!(summary.last_assistant_text.as_deref(), Some("after"));
    }

    const KELP: &str = "/home/dev/projects/kelp-cli";

    /// A rollout of session `id` that started in `cwd` from `source`, its last turn
    /// finished with `reply` at minute `minute`, or still open when `reply` is `None`.
    fn rollout(id: &str, cwd: &str, source: &str, minute: u32, reply: Option<&str>) -> String {
        let at = |m: u32| format!("2026-11-08T17:{m:02}:00.000Z");
        let originator = if source == "exec" {
            "codex_exec"
        } else {
            "codex-tui"
        };
        let mut lines = vec![
            serde_json::json!({"timestamp": at(0), "ordinal": 0, "type": "session_meta",
                "payload": {"id": id, "cwd": cwd, "originator": originator, "source": source,
                    "base_instructions": {"text": "x".repeat(70_000)}}}),
            serde_json::json!({"timestamp": at(minute), "ordinal": 1, "type": "event_msg",
                "payload": {"type": "task_started", "turn_id": "t1"}}),
            serde_json::json!({"timestamp": at(minute), "ordinal": 2, "type": "response_item",
                "payload": {"type": "message", "role": "user",
                    "content": [{"type": "input_text", "text": "Chart the tides"}]}}),
        ];
        if let Some(reply) = reply {
            lines.push(serde_json::json!({"timestamp": at(minute), "ordinal": 3,
                "type": "event_msg", "payload": {"type": "task_complete", "turn_id": "t1",
                    "last_agent_message": reply}}));
        }
        lines.iter().map(|l| l.to_string() + "\n").collect()
    }

    fn rollout_path(sessions: &Path, day: &str, id: &str) -> PathBuf {
        let dir = sessions.join(day);
        fs::create_dir_all(&dir).unwrap();
        dir.join(format!("rollout-2026-11-08T17-00-00-{id}.jsonl"))
    }

    #[test]
    fn codex_rollouts_are_indexed_by_their_starting_cwd() {
        const A: &str = "0b7e4d2c-5a6f-4e1b-8c9d-3f2a1b0c9d81";
        const B: &str = "0b7e4d2c-5a6f-4e1b-8c9d-3f2a1b0c9d82";
        const EXEC: &str = "0b7e4d2c-5a6f-4e1b-8c9d-3f2a1b0c9d83";
        const OTHER: &str = "0b7e4d2c-5a6f-4e1b-8c9d-3f2a1b0c9d84";
        let tmp = tempfile::tempdir().unwrap();
        let sessions = tmp.path().join("codex/sessions");
        let day = "2026/11/08";
        fs::write(
            rollout_path(&sessions, day, A),
            rollout(A, REPO, "cli", 5, Some("Tides done.")),
        )
        .unwrap();
        fs::write(
            rollout_path(&sessions, "2026/11/07", B),
            rollout(B, REPO, "cli", 9, None),
        )
        .unwrap();
        fs::write(
            rollout_path(&sessions, day, EXEC),
            rollout(EXEC, REPO, "exec", 20, Some("x")),
        )
        .unwrap();
        fs::write(
            rollout_path(&sessions, day, OTHER),
            rollout(OTHER, KELP, "cli", 30, Some("y")),
        )
        .unwrap();
        // Not a rollout, and a rollout outside a day directory: both left out.
        fs::write(sessions.join(day).join("notes.jsonl"), "{}\n").unwrap();
        fs::write(sessions.join(format!("rollout-x-{A}.jsonl")), "{}\n").unwrap();
        let mut reader =
            SessionReader::new(tmp.path().join("projects")).with_codex(sessions.clone());
        let mine = reader.read_codex_repo(REPO, &[]);
        let ids: Vec<&str> = mine
            .iter()
            .filter_map(|c| c.summary.session_id.as_deref())
            .collect();
        // Newest first; the exec session and the other repo's are left out.
        assert_eq!(ids, [B, A]);
        assert_eq!(mine[1].last_message.as_deref(), Some("Tides done."));
        assert!(mine[0].summary.byte_size.is_some() && mine[0].summary.modified.is_some());
        assert_eq!(mine[0].summary.cwd.as_deref(), Some(REPO));
        assert!(
            reader.has_rollout(OTHER)
                && !reader.has_rollout("0b7e4d2c-0000-4000-8000-000000000000")
        );
        assert_eq!(reader.read_codex_repo(KELP, &[]).len(), 1);
        // The cwds read so far are named once; then a listing is fresh for a while. A
        // watch saying they changed lists again, and names the cwds whose rollouts moved.
        assert_eq!(reader.relist_codex(), [KELP, REPO]);
        assert!(reader.relist_codex().is_empty());
        fs::write(
            rollout_path(&sessions, "2026/11/07", B),
            rollout(B, REPO, "cli", 9, Some("Now done.")),
        )
        .unwrap();
        fs::remove_file(rollout_path(&sessions, day, OTHER)).unwrap();
        reader.codex_changed();
        assert_eq!(reader.relist_codex(), [KELP, REPO]);
        assert!(reader.read_codex_repo(KELP, &[]).is_empty());
        assert_eq!(
            reader.read_codex_repo(REPO, &[])[0].last_message.as_deref(),
            Some("Now done.")
        );
        // Without a Codex directory nothing is read.
        let mut plain = SessionReader::new(tmp.path().join("projects"));
        assert!(plain.read_codex_repo(REPO, &[]).is_empty());
    }

    fn json_line(minute: u32, kind: &str, payload: serde_json::Value) -> String {
        serde_json::json!({"timestamp": format!("2026-11-08T17:{minute:02}:00.000Z"),
            "ordinal": minute, "type": kind, "payload": payload})
        .to_string()
            + "\n"
    }

    fn append(path: &Path, text: &str) {
        use std::io::Write;
        let mut f = fs::OpenOptions::new().append(true).open(path).unwrap();
        f.write_all(text.as_bytes()).unwrap();
    }

    #[test]
    fn a_turn_longer_than_the_tail_window_is_still_working() {
        use refract_core::classify::{LiveState, SessionState};
        use refract_core::codex_transcript::TurnEnd;
        const A: &str = "0b7e4d2c-5a6f-4e1b-8c9d-3f2a1b0c9d91";
        let tmp = tempfile::tempdir().unwrap();
        let sessions = tmp.path().join("codex/sessions");
        let path = rollout_path(&sessions, "2026/11/08", A);
        // Started, then far more tool output than the window holds.
        fs::write(&path, rollout(A, REPO, "cli", 5, None)).unwrap();
        let output = serde_json::json!({"type": "function_call_output",
            "output": "x".repeat(4000)});
        let mut filler = String::new();
        while (filler.len() as u64) < TAIL_BYTES * 3 {
            filler.push_str(&json_line(6, "response_item", output.clone()));
        }
        append(&path, &filler);
        let mut reader =
            SessionReader::new(tmp.path().join("projects")).with_codex(sessions.clone());
        let read = reader.read_codex_repo(REPO, &[]);
        // The window grew back to the task_started.
        assert_eq!(read[0].end, Some(TurnEnd::MidTurn));
        assert_eq!(read[0].state(true), SessionState::Live(LiveState::Working));
        // Appended lines are read on from where the last read stopped, the turn carried.
        append(&path, &filler[..filler.len() / 4]);
        append(&path, "{\"half\": ");
        reader.codex_changed();
        let read = reader.read_codex_repo(REPO, &[]);
        assert_eq!(read[0].end, Some(TurnEnd::MidTurn));
        assert_eq!(read[0].summary.malformed_lines, 0);
        append(
            &path,
            &("1}\n".to_owned()
                + &json_line(
                    9,
                    "event_msg",
                    serde_json::json!({"type": "task_complete",
                    "last_agent_message": "Charted."}),
                )),
        );
        let files = [path.clone()];
        assert_eq!(reader.codex_files_changed(&files), [REPO]);
        let read = reader.read_codex_repo(REPO, &[]);
        assert_eq!(read[0].end, Some(TurnEnd::Complete));
        assert_eq!(read[0].last_message.as_deref(), Some("Charted."));
        assert_eq!(read[0].state(true), SessionState::Live(LiveState::Idle));
        // Rewritten as a new file: read afresh.
        fs::remove_file(&path).unwrap();
        fs::write(&path, rollout(A, REPO, "cli", 20, Some("Fresh."))).unwrap();
        reader.codex_files_changed(&files);
        let read = reader.read_codex_repo(REPO, &[]);
        assert_eq!(read[0].last_message.as_deref(), Some("Fresh."));
        // Gone: dropped, and its cwd named.
        fs::remove_file(&path).unwrap();
        assert_eq!(reader.codex_files_changed(&files), [REPO]);
        assert!(reader.read_codex_repo(REPO, &[]).is_empty());
    }

    #[test]
    fn a_head_read_before_its_line_was_complete_is_read_again() {
        const A: &str = "0b7e4d2c-5a6f-4e1b-8c9d-3f2a1b0c9d92";
        let tmp = tempfile::tempdir().unwrap();
        let sessions = tmp.path().join("codex/sessions");
        let path = rollout_path(&sessions, "2026/11/08", A);
        let whole = rollout(A, REPO, "cli", 5, Some("Done."));
        fs::write(&path, &whole[..1000]).unwrap();
        let mut reader =
            SessionReader::new(tmp.path().join("projects")).with_codex(sessions.clone());
        assert!(reader.read_codex_repo(REPO, &[]).is_empty());
        // Known by the file name meanwhile.
        assert!(reader.has_rollout(A));
        fs::write(&path, &whole).unwrap();
        reader.codex_changed();
        let read = reader.read_codex_repo(REPO, &[]);
        assert_eq!(read.len(), 1);
        assert_eq!(read[0].last_message.as_deref(), Some("Done."));
    }

    #[test]
    fn codex_sessions_in_a_subdirectory_belong_to_the_deepest_repo() {
        const A: &str = "0b7e4d2c-5a6f-4e1b-8c9d-3f2a1b0c9d93";
        const B: &str = "0b7e4d2c-5a6f-4e1b-8c9d-3f2a1b0c9d94";
        const SUB: &str = "0b7e4d2c-5a6f-4e1b-8c9d-3f2a1b0c9d95";
        let tmp = tempfile::tempdir().unwrap();
        let sessions = tmp.path().join("codex/sessions");
        let day = "2026/11/08";
        let nested = format!("{REPO}/vendor/charts");
        let src = format!("{REPO}/src");
        fs::write(
            rollout_path(&sessions, day, A),
            rollout(A, &src, "cli", 5, Some("a")),
        )
        .unwrap();
        fs::write(
            rollout_path(&sessions, day, B),
            rollout(B, &nested, "cli", 6, Some("b")),
        )
        .unwrap();
        // A thread Codex spawned is left out.
        let spawned = rollout(SUB, REPO, "cli", 7, Some("s")).replacen(
            "\"source\":\"cli\"",
            "\"source\":\"cli\",\"thread_source\":\"subagent\"",
            1,
        );
        fs::write(rollout_path(&sessions, day, SUB), spawned).unwrap();
        let mut reader =
            SessionReader::new(tmp.path().join("projects")).with_codex(sessions.clone());
        let ids = |read: Vec<CodexSummary>| -> Vec<String> {
            read.into_iter()
                .filter_map(|c| c.summary.session_id)
                .collect()
        };
        assert_eq!(ids(reader.read_codex_repo(REPO, &[&nested])), [A]);
        assert_eq!(ids(reader.read_codex_repo(&nested, &[])), [B]);
        assert!(reader.is_hidden_rollout(SUB) && !reader.is_hidden_rollout(A));
    }

    #[test]
    fn file_that_shrank_since_it_was_listed_is_read_as_it_is_now() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join(format!("{ID}.jsonl"));
        fs::write(&path, line(REPO, "rewritten") + "\n").unwrap();
        let summary = read_tail(&path, TAIL_BYTES * 3).unwrap();
        assert_eq!(summary.malformed_lines, 0);
        assert_eq!(summary.last_assistant_text.as_deref(), Some("rewritten"));
        assert_eq!(summary.byte_size, Some(fs::metadata(&path).unwrap().len()));
    }
}
