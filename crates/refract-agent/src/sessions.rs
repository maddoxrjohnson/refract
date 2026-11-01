//! Reading a repo's Claude Code transcripts.
//!
//! Only the start and the end of each transcript are read, and summaries are cached
//! until the file's modification time or size changes. The start says who the session
//! belongs to; the end says how it stands.

use std::collections::HashMap;
use std::fs::{self, File};
use std::io::{self, Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};
use std::time::SystemTime;

use chrono::{DateTime, Utc};
use refract_core::sessions::{paths_match, project_dir_matches, within_deepest};
use refract_core::transcript::{parse_lines, SessionSummary};

/// How much of the end of a transcript is read.
pub const TAIL_BYTES: u64 = 256 * 1024;

/// How far the tail window grows when the last line is longer than [`TAIL_BYTES`].
pub const MAX_TAIL_BYTES: u64 = 8 * 1024 * 1024;

/// How much of the start of a transcript is read for its session id and starting cwd.
pub const HEAD_BYTES: u64 = 64 * 1024;

/// How many of a repo's most recent transcripts are read and reported.
pub const MAX_SESSIONS: usize = 20;

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

/// Reads and caches transcript summaries.
#[derive(Debug)]
pub struct SessionReader {
    projects_dir: PathBuf,
    cache: HashMap<PathBuf, Cached>,
}

impl SessionReader {
    /// A reader for Claude Code's `projects` directory.
    pub fn new(projects_dir: PathBuf) -> SessionReader {
        SessionReader {
            projects_dir,
            cache: HashMap::new(),
        }
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
