//! Background subagents a live session has running.
//!
//! Claude Code keeps each subagent of a session as
//! `<project>/<session>/subagents/…/agent-<id>.meta.json` beside its transcript, at any
//! depth (workflow runs nest under `subagents/workflows/<run>/`). The meta file names
//! how the agent was started (`"requestShape": "background"`) and gains
//! `"finished": true` when the agent ends. A session whose own turn ended still counts
//! as working while such an agent runs (see [`refract_core::classify::classify_report`]).
//!
//! Only live sessions are counted, and only meta files written since their process
//! started, so the leftovers of an earlier run that crashed do not count.

use std::fs;
use std::path::{Path, PathBuf};
use std::time::SystemTime;

use chrono::{DateTime, Utc};
use serde::Deserialize;

/// How deep under `subagents` meta files are looked for.
pub const MAX_DEPTH: usize = 4;

/// Most directories under one session's `subagents` that are read or watched.
pub const MAX_DIRS: usize = 200;

/// The fields of a meta file that matter here.
#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct Meta {
    request_shape: Option<String>,
    #[serde(default)]
    finished: bool,
}

/// The `subagents` directory of a session whose transcript directory is `session_dir`.
pub fn dir(session_dir: &Path) -> PathBuf {
    session_dir.join("subagents")
}

/// Background subagents still running under `subagents`: meta files that say
/// `"requestShape": "background"` and not `"finished": true`, written at or after
/// `since` (the process start; `None` counts them all). Missing directories and
/// unreadable or garbled meta files count as none.
pub fn count_running(subagents: &Path, since: Option<DateTime<Utc>>) -> u32 {
    let since = since.map(SystemTime::from);
    let mut count = 0;
    let mut dirs = vec![(subagents.to_owned(), 0)];
    let mut seen = 0;
    while let Some((dir, depth)) = dirs.pop() {
        seen += 1;
        if seen > MAX_DIRS {
            break;
        }
        let Ok(entries) = fs::read_dir(&dir) else {
            continue;
        };
        for entry in entries.flatten() {
            let Ok(kind) = entry.file_type() else {
                continue;
            };
            if kind.is_dir() {
                if depth < MAX_DEPTH {
                    dirs.push((entry.path(), depth + 1));
                }
                continue;
            }
            let name = entry.file_name();
            if !kind.is_file() || !name.to_string_lossy().ends_with(".meta.json") {
                continue;
            }
            let fresh = since.is_none_or(|since| {
                entry
                    .metadata()
                    .and_then(|m| m.modified())
                    .is_ok_and(|m| m >= since)
            });
            if fresh && is_running(&entry.path()) {
                count += 1;
            }
        }
    }
    count
}

fn is_running(meta: &Path) -> bool {
    fs::read(meta)
        .ok()
        .and_then(|raw| serde_json::from_slice::<Meta>(&raw).ok())
        .is_some_and(|m| m.request_shape.as_deref() == Some("background") && !m.finished)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    fn meta(shape: &str, finished: bool) -> String {
        let finished = if finished { ",\"finished\":true" } else { "" };
        format!(
            "{{\"agentType\":\"implementer\",\"description\":\"Tidy the tide tables\",\
             \"toolUseId\":\"toolu_01\",\"spawnDepth\":1,\"requestShape\":\"{shape}\",\
             \"requestNonInteractive\":true{finished}}}"
        )
    }

    #[test]
    fn counts_unfinished_background_agents_written_since_the_process_started() {
        let tmp = tempfile::tempdir().unwrap();
        let subagents = dir(&tmp.path().join("5f0c2a9e-1b7d-4c3e-9a61-0d2e8f4b7c11"));
        let nested = subagents.join("workflows/run-kelp");
        fs::create_dir_all(&nested).unwrap();
        let write = |path: &Path, body: &str| fs::write(path, body).unwrap();
        write(
            &subagents.join("agent-old.meta.json"),
            &meta("background", false),
        );
        let old = SystemTime::now() - Duration::from_secs(3600);
        fs::File::options()
            .write(true)
            .open(subagents.join("agent-old.meta.json"))
            .unwrap()
            .set_modified(old)
            .unwrap();
        let started = DateTime::<Utc>::from(SystemTime::now() - Duration::from_secs(60));

        write(
            &subagents.join("agent-a.meta.json"),
            &meta("background", false),
        );
        write(&subagents.join("agent-a.jsonl"), "{}\n");
        write(
            &subagents.join("agent-b.meta.json"),
            &meta("background", true),
        );
        write(
            &subagents.join("agent-c.meta.json"),
            &meta("foreground", false),
        );
        write(&subagents.join("agent-d.meta.json"), "{\"requestShape\":");
        write(
            &nested.join("agent-e.meta.json"),
            &meta("background", false),
        );

        assert_eq!(count_running(&subagents, Some(started)), 2);
        // Without a start time the earlier run's agent counts too.
        assert_eq!(count_running(&subagents, None), 3);
        assert_eq!(count_running(&tmp.path().join("missing"), None), 0);
    }
}
