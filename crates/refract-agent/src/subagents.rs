//! Background work a live session has running: subagents and workflow runs.
//!
//! A session whose own turn ended still counts as working while such work runs (see
//! [`refract_core::classify::classify_report`]). Beside a session's transcript
//! `<project>/<session>.jsonl`, Claude Code keeps:
//!
//! - `<session>/subagents/agent-<id>.meta.json` for each subagent, saying how it was
//!   started (`"requestShape": "background"`) and, usually, gaining `"finished": true`
//!   when it ends;
//! - `<session>/subagents/workflows/<run>/` for each workflow run, holding its agents
//!   (started as foreground agents of the run, so they say nothing about the run), and
//!   `<session>/workflows/<run>.json` with its `"status"` once it ends.
//!
//! The meta file does not always gain `finished`: a killed agent, or one that stopped
//! while its own background work ran, can end without it. So work also counts as ended
//! when the session's transcript holds a `<task-notification>` for its task id (the
//! agent id, or the workflow's task id from its launch result) written at or after the
//! work was last active. An agent that resumes after a notification rewrites its meta
//! file, and a meta file written after the notification counts it again; its
//! transcript's last write, just after the notification, does not. As a backstop, work
//! inactive for
//! [`STALE_AFTER`] does not count.
//!
//! Only work started since the session's process did counts, so the leftovers of an
//! earlier run that crashed do not. Transcripts are read for notifications only while a
//! session has work that may still run, from where the last read stopped.

use std::collections::HashMap;
use std::fs::{self, File};
use std::io::{BufRead, BufReader, Seek, SeekFrom};
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

use chrono::{DateTime, Utc};
use serde::Deserialize;
use serde_json::Value;

/// Work inactive this long no longer counts, whatever its files say.
pub const STALE_AFTER: Duration = Duration::from_secs(2 * 60 * 60);

/// A notification this much older than the work's last write still ends it: the
/// agent's transcript gets its last lines just after the notification is queued.
const NOTIFY_SLACK: Duration = Duration::from_secs(10);

/// The directories to watch for a session whose transcript directory is `session_dir`:
/// it (for `subagents` and `workflows` appearing), `subagents` (meta files, and
/// `workflows` appearing), `subagents/workflows` (runs starting) and `workflows` (runs
/// ending). The only walk of a session's directories, so watching and counting agree.
pub fn watched_dirs(session_dir: &Path) -> [PathBuf; 4] {
    let subagents = session_dir.join("subagents");
    [
        session_dir.to_owned(),
        subagents.join("workflows"),
        subagents,
        session_dir.join("workflows"),
    ]
}

/// The fields of a subagent's meta file that matter here.
#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct Meta {
    request_shape: Option<String>,
    #[serde(default)]
    finished: bool,
}

/// What one session's transcript says about its background work, read so far.
#[derive(Debug, Default)]
struct Notes {
    /// The transcript's inode and how far it was read, up to the last complete line.
    inode: u64,
    offset: u64,
    /// The latest notification for each task id.
    ended: HashMap<String, SystemTime>,
    /// Task ids of workflow runs, by run id.
    runs: HashMap<String, String>,
}

impl Notes {
    /// Reads the lines added to `transcript` since the last read, or all of it when it
    /// shrank or was replaced.
    fn update(&mut self, transcript: &Path) {
        let Ok(file) = File::open(transcript) else {
            return;
        };
        let Ok(meta) = file.metadata() else {
            return;
        };
        if meta.ino() != self.inode || meta.len() < self.offset {
            *self = Notes {
                inode: meta.ino(),
                ..Notes::default()
            };
        }
        if meta.len() == self.offset {
            return;
        }
        let mut reader = BufReader::new(file);
        if reader.seek(SeekFrom::Start(self.offset)).is_err() {
            return;
        }
        let mut line = Vec::new();
        loop {
            line.clear();
            match reader.read_until(b'\n', &mut line) {
                Ok(0) | Err(_) => break,
                // A partial last line is read again once it is finished.
                Ok(_) if line.last() != Some(&b'\n') => break,
                Ok(n) => {
                    self.offset += n as u64;
                    self.take(&line);
                }
            }
        }
    }

    fn take(&mut self, line: &[u8]) {
        // Parsing is skipped for most lines, among them the large request blobs that
        // quote notifications, from one pass over the bytes.
        let [notification, queued, attached, launch] = contains_each(
            line,
            [
                b"<task-notification>",
                b"\"type\":\"queue-operation\"",
                b"\"type\":\"attachment\"",
                b"\"async_launched\"",
            ],
        );
        let notification = notification && (queued || attached);
        if !notification && !launch {
            return;
        }
        let Ok(entry) = serde_json::from_slice::<Value>(line) else {
            return;
        };
        if launch {
            let result = &entry["toolUseResult"];
            if let (Some(run), Some(task)) = (result["runId"].as_str(), result["taskId"].as_str()) {
                self.runs.insert(run.to_owned(), task.to_owned());
            }
        }
        // Queued as soon as the work ends; shown to the agent later as an attachment.
        let text = match entry["type"].as_str() {
            Some("queue-operation") if entry["operation"] == "enqueue" => &entry["content"],
            Some("attachment") => &entry["attachment"]["prompt"],
            _ => return,
        };
        let (Some(text), Some(at)) = (text.as_str(), entry["timestamp"].as_str()) else {
            return;
        };
        // A notification is the whole text; a prompt that quotes one is not.
        let Some(task) = text
            .trim_start()
            .strip_prefix("<task-notification>")
            .and_then(|rest| rest.split_once("<task-id>"))
            .and_then(|(_, rest)| rest.split_once("</task-id>"))
            .map(|(id, _)| id.trim())
        else {
            return;
        };
        let Ok(at) = DateTime::parse_from_rfc3339(at) else {
            return;
        };
        let at = SystemTime::from(at.with_timezone(&Utc));
        let latest = self.ended.entry(task.to_owned()).or_insert(at);
        *latest = (*latest).max(at);
    }

    /// True when a notification for `task` came after `started` (when its meta file or
    /// record was last written: a resumed agent rewrites its meta file) and at or after
    /// `wrote` (when its transcripts were last written), give or take [`NOTIFY_SLACK`]
    /// for that, since an agent's transcript gets its last write just after.
    fn ended(&self, task: &str, started: SystemTime, wrote: Option<SystemTime>) -> bool {
        self.ended.get(task).is_some_and(|&at| {
            started <= at && wrote.is_none_or(|wrote| at + NOTIFY_SLACK >= wrote)
        })
    }
}

/// Whether `line` contains each of `needles`, from one pass over it.
fn contains_each<const N: usize>(line: &[u8], needles: [&[u8]; N]) -> [bool; N] {
    let mut found = [false; N];
    for i in 0..line.len() {
        for (needle, seen) in needles.iter().zip(found.iter_mut()) {
            if !*seen && line[i..].starts_with(needle) {
                *seen = true;
            }
        }
    }
    found
}

/// Background work that may still run, before the transcript is asked.
struct Candidate {
    /// The agent id, or the run id of a workflow run.
    id: String,
    workflow: bool,
    /// When its meta file was last written (a resume rewrites it), or for a workflow run
    /// when its directory was.
    started: SystemTime,
    /// When its transcripts were last written, if it has any.
    wrote: Option<SystemTime>,
}

impl Candidate {
    /// When it last wrote anything.
    fn active(&self) -> SystemTime {
        self.wrote.map_or(self.started, |w| w.max(self.started))
    }
}

/// Counts live sessions' background work, keeping what their transcripts said.
#[derive(Debug, Default)]
pub struct Tracker {
    notes: HashMap<PathBuf, Notes>,
}

impl Tracker {
    /// Background work running for the session whose transcript directory is
    /// `session_dir`, started at or after `since` (the process start; `None` counts it
    /// all), as of `now`. Missing directories and unreadable or garbled files count as
    /// nothing running.
    pub fn count(
        &mut self,
        session_dir: &Path,
        since: Option<DateTime<Utc>>,
        now: SystemTime,
    ) -> u32 {
        let since = since.map(SystemTime::from);
        let fresh = |t: SystemTime| since.is_none_or(|s| t >= s);
        let live = |t: SystemTime| now.duration_since(t).map_or(true, |d| d < STALE_AFTER);
        let [_, runs_dir, subagents, ends_dir] = watched_dirs(session_dir);
        let mut candidates = Vec::new();
        for (path, modified) in files(&subagents) {
            let name = path.file_name().unwrap_or_default().to_string_lossy();
            let Some(id) = name
                .strip_prefix("agent-")
                .and_then(|n| n.strip_suffix(".meta.json"))
            else {
                continue;
            };
            if !fresh(modified) || !is_running(&path) {
                continue;
            }
            let transcript = subagents.join(format!("agent-{id}.jsonl"));
            candidates.push(Candidate {
                id: id.to_owned(),
                workflow: false,
                started: modified,
                wrote: mtime(&transcript),
            });
        }
        for run in dirs(&runs_dir) {
            let id = run.file_name().unwrap_or_default().to_string_lossy();
            if run_ended(&ends_dir.join(format!("{id}.json"))) {
                continue;
            }
            let Some(started) = mtime(&run) else {
                continue;
            };
            let wrote = files(&run).into_iter().map(|(_, t)| t).max();
            let candidate = Candidate {
                id: id.into_owned(),
                workflow: true,
                started,
                wrote,
            };
            if fresh(candidate.active()) {
                candidates.push(candidate);
            }
        }
        candidates.retain(|c| live(c.active()));
        if candidates.is_empty() {
            return 0;
        }
        let notes = self.notes.entry(session_dir.to_owned()).or_default();
        notes.update(&session_dir.with_extension("jsonl"));
        let running = candidates.iter().filter(|c| {
            // A workflow run does not resume; files written in its directory as it ends
            // get the slack.
            let (task, started, wrote) = if c.workflow {
                let task = notes.runs.get(&c.id).map(String::as_str);
                (task, SystemTime::UNIX_EPOCH, Some(c.active()))
            } else {
                (Some(c.id.as_str()), c.started, c.wrote)
            };
            !task.is_some_and(|task| notes.ended(task, started, wrote))
        });
        running.count() as u32
    }

    /// Forgets what the transcripts of sessions not in `session_dirs` said.
    pub fn retain(&mut self, session_dirs: &[PathBuf]) {
        self.notes.retain(|dir, _| session_dirs.contains(dir));
    }
}

fn mtime(path: &Path) -> Option<SystemTime> {
    fs::metadata(path).and_then(|m| m.modified()).ok()
}

/// The files directly in `dir`, with when each was last written.
fn files(dir: &Path) -> Vec<(PathBuf, SystemTime)> {
    let Ok(entries) = fs::read_dir(dir) else {
        return Vec::new();
    };
    entries
        .flatten()
        .filter_map(|e| {
            let meta = e.metadata().ok().filter(|m| m.is_file())?;
            Some((e.path(), meta.modified().ok()?))
        })
        .collect()
}

/// The directories directly in `dir`.
fn dirs(dir: &Path) -> Vec<PathBuf> {
    let Ok(entries) = fs::read_dir(dir) else {
        return Vec::new();
    };
    entries
        .flatten()
        .filter(|e| e.file_type().is_ok_and(|t| t.is_dir()))
        .map(|e| e.path())
        .collect()
}

fn is_running(meta: &Path) -> bool {
    fs::read(meta)
        .ok()
        .and_then(|raw| serde_json::from_slice::<Meta>(&raw).ok())
        .is_some_and(|m| m.request_shape.as_deref() == Some("background") && !m.finished)
}

/// True when a workflow run's record exists with a status other than `running`. A
/// record that cannot be read yet (still being written) says nothing.
fn run_ended(record: &Path) -> bool {
    #[derive(Deserialize)]
    struct Run {
        status: Option<String>,
    }
    fs::read(record)
        .ok()
        .and_then(|raw| serde_json::from_slice::<Run>(&raw).ok())
        .is_some_and(|r| r.status.as_deref() != Some("running"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    const SESSION: &str = "5f0c2a9e-1b7d-4c3e-9a61-0d2e8f4b7c11";

    fn meta(shape: &str, finished: bool) -> String {
        let finished = if finished { ",\"finished\":true" } else { "" };
        format!(
            "{{\"agentType\":\"implementer\",\"description\":\"Tidy the tide tables\",\
             \"toolUseId\":\"toolu_01\",\"spawnDepth\":1,\"requestShape\":\"{shape}\",\
             \"requestNonInteractive\":true{finished}}}"
        )
    }

    fn set_mtime(path: &Path, at: SystemTime) {
        File::options()
            .write(true)
            .open(path)
            .unwrap()
            .set_modified(at)
            .unwrap();
    }

    fn ago(secs: u64) -> SystemTime {
        SystemTime::now() - Duration::from_secs(secs)
    }

    /// A queued task notification line, as Claude Code writes it.
    fn notification(task: &str, at: SystemTime) -> String {
        let content = format!(
            "<task-notification>\n<task-id>{task}</task-id>\n<status>completed</status>\n\
             <summary>Agent \"Tidy the tide tables\" finished</summary>\n</task-notification>"
        );
        serde_json::json!({
            "type": "queue-operation",
            "operation": "enqueue",
            "timestamp": DateTime::<Utc>::from(at).to_rfc3339(),
            "sessionId": SESSION,
            "content": content,
        })
        .to_string()
            + "\n"
    }

    fn append(path: &Path, text: &str) {
        let mut file = File::options()
            .create(true)
            .append(true)
            .open(path)
            .unwrap();
        file.write_all(text.as_bytes()).unwrap();
    }

    #[test]
    fn counts_unfinished_background_agents_started_since_the_process() {
        let tmp = tempfile::tempdir().unwrap();
        let session = tmp.path().join(SESSION);
        let subagents = session.join("subagents");
        fs::create_dir_all(&subagents).unwrap();
        let write = |name: &str, body: &str| fs::write(subagents.join(name), body).unwrap();
        write("agent-old.meta.json", &meta("background", false));
        set_mtime(&subagents.join("agent-old.meta.json"), ago(600));
        let started = DateTime::<Utc>::from(ago(60));
        write("agent-a.meta.json", &meta("background", false));
        write("agent-a.jsonl", "{}\n");
        write("agent-b.meta.json", &meta("background", true));
        write("agent-c.meta.json", &meta("foreground", false));
        write("agent-d.meta.json", "{\"requestShape\":");
        // A foreground agent of a workflow run is not counted on its own.
        let run = subagents.join("workflows/wf_kelp-001");
        fs::create_dir_all(&run).unwrap();
        fs::write(run.join("agent-e.meta.json"), meta("foreground", false)).unwrap();

        let mut tracker = Tracker::default();
        let now = SystemTime::now();
        // Agent a and the workflow run.
        assert_eq!(tracker.count(&session, Some(started), now), 2);
        // Without a start time the earlier run's agent counts too.
        assert_eq!(tracker.count(&session, None, now), 3);
        assert_eq!(tracker.count(&tmp.path().join("missing"), None, now), 0);
        // No transcript was read for work that cannot be running.
        assert!(!tracker.notes.contains_key(&tmp.path().join("missing")));
    }

    #[test]
    fn a_notification_or_a_long_silence_ends_an_agent_without_finished() {
        let tmp = tempfile::tempdir().unwrap();
        let session = tmp.path().join(SESSION);
        let transcript = tmp.path().join(format!("{SESSION}.jsonl"));
        let subagents = session.join("subagents");
        fs::create_dir_all(&subagents).unwrap();
        fs::write(
            subagents.join("agent-a1.meta.json"),
            meta("background", false),
        )
        .unwrap();
        fs::write(subagents.join("agent-a1.jsonl"), "{}\n").unwrap();
        append(&transcript, "{\"type\":\"user\"}\n");
        let mut tracker = Tracker::default();
        assert_eq!(tracker.count(&session, None, SystemTime::now()), 1);

        // Killed: the notification comes, `finished` never does. A partial line waits.
        let line = notification("a1", SystemTime::now());
        let (start, end) = line.split_at(40);
        append(&transcript, start);
        assert_eq!(tracker.count(&session, None, SystemTime::now()), 1);
        append(&transcript, end);
        assert_eq!(tracker.count(&session, None, SystemTime::now()), 0);

        // Resumed: it writes again well after the notification, and runs again.
        append(&subagents.join("agent-a1.jsonl"), "{}\n");
        set_mtime(
            &subagents.join("agent-a1.jsonl"),
            SystemTime::now() + NOTIFY_SLACK * 2,
        );
        let later = SystemTime::now() + NOTIFY_SLACK * 3;
        assert_eq!(tracker.count(&session, None, later), 1);

        // Silent for longer than `STALE_AFTER`: no longer counted.
        assert_eq!(tracker.count(&session, None, later + STALE_AFTER), 0);
    }

    #[test]
    fn a_meta_file_rewritten_after_the_notification_is_a_resume() {
        let tmp = tempfile::tempdir().unwrap();
        let session = tmp.path().join(SESSION);
        let transcript = tmp.path().join(format!("{SESSION}.jsonl"));
        let subagents = session.join("subagents");
        fs::create_dir_all(&subagents).unwrap();
        let meta_path = subagents.join("agent-a1.meta.json");
        let jsonl = subagents.join("agent-a1.jsonl");
        fs::write(&meta_path, meta("background", false)).unwrap();
        fs::write(&jsonl, "{}\n").unwrap();
        let notified = SystemTime::now() - Duration::from_secs(60);
        append(&transcript, &notification("a1", notified));
        let mut tracker = Tracker::default();
        let now = SystemTime::now();

        // Killed: its transcript gets a last write just after the notification, and its
        // meta file is older.
        set_mtime(&meta_path, notified - Duration::from_secs(900));
        set_mtime(&jsonl, notified + Duration::from_millis(100));
        assert_eq!(tracker.count(&session, None, now), 0);

        // Resumed 2 s after the notification: its meta file is rewritten then, which
        // counts with no slack.
        set_mtime(&meta_path, notified + Duration::from_secs(2));
        set_mtime(&jsonl, notified + Duration::from_secs(3));
        assert_eq!(tracker.count(&session, None, now), 1);
    }

    #[test]
    fn a_prompt_quoting_a_notification_ends_nothing() {
        let mut notes = Notes::default();
        let at = DateTime::<Utc>::from(SystemTime::now()).to_rfc3339();
        let typed = serde_json::json!({
            "type": "queue-operation",
            "operation": "enqueue",
            "timestamp": at,
            "content": "why did <task-notification><task-id>a1</task-id> arrive?",
        });
        notes.take(typed.to_string().as_bytes());
        // A request blob quoting one is not even parsed.
        let blob = serde_json::json!({
            "type": "api-request-blob",
            "message": "<task-notification>\n<task-id>a2</task-id>\n</task-notification>",
        });
        notes.take(blob.to_string().as_bytes());
        assert!(notes.ended.is_empty());
        notes.take(notification("a3", SystemTime::now()).as_bytes());
        assert_eq!(notes.ended.keys().collect::<Vec<_>>(), ["a3"]);
    }

    #[test]
    fn a_workflow_run_runs_until_its_record_or_notification() {
        let tmp = tempfile::tempdir().unwrap();
        let session = tmp.path().join(SESSION);
        let transcript = tmp.path().join(format!("{SESSION}.jsonl"));
        let run = session.join("subagents/workflows/wf_kelp-001");
        fs::create_dir_all(&run).unwrap();
        fs::write(run.join("agent-e.jsonl"), "{}\n").unwrap();
        let launch = serde_json::json!({
            "type": "user",
            "toolUseResult": {"status": "async_launched", "taskId": "wtide01", "runId": "wf_kelp-001"},
        });
        append(&transcript, &format!("{launch}\n"));
        let mut tracker = Tracker::default();
        assert_eq!(tracker.count(&session, None, SystemTime::now()), 1);

        let record = session.join("workflows/wf_kelp-001.json");
        fs::create_dir_all(record.parent().unwrap()).unwrap();
        fs::write(
            &record,
            "{\"runId\":\"wf_kelp-001\",\"status\":\"running\"}",
        )
        .unwrap();
        assert_eq!(tracker.count(&session, None, SystemTime::now()), 1);
        fs::write(
            &record,
            "{\"runId\":\"wf_kelp-001\",\"status\":\"completed\"}",
        )
        .unwrap();
        assert_eq!(tracker.count(&session, None, SystemTime::now()), 0);

        fs::remove_file(&record).unwrap();
        append(&transcript, &notification("wtide01", SystemTime::now()));
        assert_eq!(tracker.count(&session, None, SystemTime::now()), 0);
    }
}
