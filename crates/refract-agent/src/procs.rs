//! Which Claude Code sessions are running, from `/proc`.
//!
//! The interactive CLI shows up with argv[0] `claude` (or a path ending in `/claude`),
//! or as `node .../claude-code/cli.js` for npm installs. The same binary also runs
//! helpers: `claude daemon ...`, and processes that rewrite their title to
//! `claude bg-pty-host` or `claude bg-spare`; those are not sessions. A session id is
//! only known when the command line has `--resume <id>` or `--session-id <id>`.
//!
//! A process's start time is `btime` from `/proc/stat` (boot time, whole seconds) plus
//! field 22 of `/proc/<pid>/stat` (clock ticks since boot). Ticks are taken as 100 per
//! second: `USER_HZ` is 100 on every Linux architecture WSL runs.

use std::collections::HashSet;
use std::fs;

use chrono::{DateTime, TimeDelta, Utc};
use refract_core::sessions::paths_match;

/// Subcommands of `claude` that are not interactive sessions.
const NON_SESSION_SUBCOMMANDS: &[&str] = &[
    "daemon",
    "mcp",
    "config",
    "update",
    "upgrade",
    "doctor",
    "install",
    "migrate-installer",
    "setup-token",
    "plugin",
    "auth",
];

/// A running Claude Code session process.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ClaudeProcess {
    pub pid: u32,
    /// The process's working directory.
    pub cwd: String,
    /// From `--resume` or `--session-id`, when given.
    pub session_id: Option<String>,
    /// When the process started, when `/proc` says.
    pub started: Option<DateTime<Utc>>,
}

/// What a command line says, if it is a Claude Code session.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ClaudeCommand {
    pub session_id: Option<String>,
}

/// Parses a NUL-separated `/proc/<pid>/cmdline`. Returns `None` for anything that is
/// not an interactive or print-mode Claude Code session.
pub fn parse_cmdline(raw: &[u8]) -> Option<ClaudeCommand> {
    let args: Vec<String> = raw
        .split(|&b| b == 0)
        .map(|a| String::from_utf8_lossy(a).into_owned())
        .collect();
    let args = match args.split_last() {
        Some((last, rest)) if last.is_empty() => rest,
        _ => &args[..],
    };
    let program = basename(args.first()?);
    let rest = if program == "claude" {
        &args[1..]
    } else if matches!(program, "node" | "bun") || program.starts_with("node") {
        let script = args.get(1)?;
        let is_claude = basename(script) == "claude"
            || script.contains("claude-code/cli")
            || script.contains("@anthropic-ai/claude-code");
        if !is_claude {
            return None;
        }
        &args[2..]
    } else {
        return None;
    };
    if let Some(first) = rest.first() {
        if NON_SESSION_SUBCOMMANDS.contains(&first.as_str()) {
            return None;
        }
    }
    let forked = rest.iter().any(|a| a == "--fork-session");
    let session_id = if forked { None } else { session_arg(rest) };
    Some(ClaudeCommand { session_id })
}

fn basename(path: &str) -> &str {
    path.rsplit('/').next().unwrap_or(path)
}

/// Finds `--resume <id>`, `-r <id>`, `--session-id <id>` or their `=` forms. Only
/// UUID-shaped values count, since `--resume` alone opens a picker and may be followed
/// by a prompt.
fn session_arg(args: &[String]) -> Option<String> {
    let mut iter = args.iter();
    while let Some(arg) = iter.next() {
        let value = match arg.split_once('=') {
            Some((flag, value)) if is_session_flag(flag) => Some(value.to_owned()),
            _ if is_session_flag(arg) => iter.clone().next().cloned(),
            _ => None,
        };
        if let Some(value) = value.filter(|v| is_uuid(v)) {
            return Some(value);
        }
    }
    None
}

fn is_session_flag(flag: &str) -> bool {
    matches!(flag, "--resume" | "-r" | "--session-id")
}

fn is_uuid(value: &str) -> bool {
    value.len() == 36
        && value.char_indices().all(|(i, c)| match i {
            8 | 13 | 18 | 23 => c == '-',
            _ => c.is_ascii_hexdigit(),
        })
}

/// Clock ticks per second in `/proc/<pid>/stat` (`USER_HZ`).
const TICKS_PER_SECOND: u64 = 100;

/// Field 22 (`starttime`) of a `/proc/<pid>/stat` line: clock ticks after boot. The
/// command name in field 2 is in parentheses and may itself hold spaces and
/// parentheses, so fields are counted from the last `)`.
pub fn parse_stat_start_ticks(stat: &str) -> Option<u64> {
    let (_, rest) = stat.rsplit_once(')')?;
    // `rest` starts at field 3 (state), so field 22 is the 20th.
    rest.split_whitespace().nth(19)?.parse().ok()
}

/// The `btime` line of `/proc/stat`: boot time in seconds since the epoch.
pub fn parse_btime(stat: &str) -> Option<i64> {
    stat.lines()
        .find_map(|line| line.strip_prefix("btime "))
        .and_then(|v| v.trim().parse().ok())
}

/// A process start time from the boot time and its start ticks.
pub fn start_time(btime: i64, ticks: u64) -> Option<DateTime<Utc>> {
    let millis = i64::try_from(ticks.checked_mul(1000)? / TICKS_PER_SECOND).ok()?;
    DateTime::from_timestamp(btime, 0)?.checked_add_signed(TimeDelta::milliseconds(millis))
}

/// Lists running Claude Code sessions by walking `/proc`. Processes whose details
/// cannot be read (other users, exited) are skipped.
pub fn scan() -> Vec<ClaudeProcess> {
    let me = std::process::id();
    let Ok(entries) = fs::read_dir("/proc") else {
        return Vec::new();
    };
    let btime = fs::read_to_string("/proc/stat")
        .ok()
        .and_then(|s| parse_btime(&s));
    entries
        .flatten()
        .filter_map(|entry| {
            let pid: u32 = entry.file_name().to_str()?.parse().ok()?;
            if pid == me {
                return None;
            }
            let dir = entry.path();
            let command = parse_cmdline(&fs::read(dir.join("cmdline")).ok()?)?;
            let cwd = fs::read_link(dir.join("cwd")).ok()?;
            let started = fs::read_to_string(dir.join("stat"))
                .ok()
                .and_then(|stat| parse_stat_start_ticks(&stat))
                .zip(btime)
                .and_then(|(ticks, btime)| start_time(btime, ticks));
            Some(ClaudeProcess {
                pid,
                cwd: cwd.to_string_lossy().into_owned(),
                session_id: command.session_id,
                started,
            })
        })
        .collect()
}

/// Decides which of a working directory's sessions are live.
///
/// A session is live when a process names its id. Each process in `cwd` that names no
/// id is matched to the most recent session no other process claimed and that was
/// written at or after the process started, so a fresh session with no transcript yet
/// claims nothing. Later-started processes choose first, since fewer sessions fit them.
/// A process whose start time is unknown may take any session. A process naming an id
/// that is not on disk yet (a fresh `--session-id`) marks nothing. `sessions` holds
/// (id, when its transcript was last written) pairs.
pub fn live_sessions(
    procs: &[ClaudeProcess],
    cwd: &str,
    sessions: &[(&str, Option<DateTime<Utc>>)],
) -> HashSet<String> {
    let known: HashSet<&str> = sessions.iter().map(|(id, _)| *id).collect();
    let mut live: HashSet<String> = procs
        .iter()
        .filter_map(|p| p.session_id.as_deref())
        .filter(|id| known.contains(id))
        .map(str::to_owned)
        .collect();
    let mut unnamed: Vec<&ClaudeProcess> = procs
        .iter()
        .filter(|p| p.session_id.is_none() && paths_match(&p.cwd, cwd))
        .collect();
    // `None` sorts first, so reversed the unknown start times come last.
    unnamed.sort_by_key(|p| std::cmp::Reverse(p.started));
    let mut recent: Vec<_> = sessions.iter().collect();
    recent.sort_by_key(|s| std::cmp::Reverse(s.1));
    for process in unnamed {
        let claim = recent.iter().find(|(id, written)| {
            !live.contains(*id)
                && process
                    .started
                    .is_none_or(|started| written.is_some_and(|w| w >= started))
        });
        if let Some((id, _)) = claim {
            live.insert((*id).to_owned());
        }
    }
    live
}

#[cfg(test)]
mod tests {
    use super::*;

    const ID: &str = "3f6c1a52-0000-4000-8000-000000000001";
    const ID2: &str = "3f6c1a52-0000-4000-8000-000000000002";
    const REPO: &str = "/home/dev/projects/tidepool-api";

    fn cmd(args: &[&str]) -> Option<ClaudeCommand> {
        let mut raw = args.join("\0").into_bytes();
        raw.push(0);
        parse_cmdline(&raw)
    }

    fn session(id: Option<&str>) -> Option<ClaudeCommand> {
        Some(ClaudeCommand {
            session_id: id.map(str::to_owned),
        })
    }

    #[test]
    fn plain_and_resumed_sessions() {
        assert_eq!(cmd(&["claude"]), session(None));
        assert_eq!(
            cmd(&["/home/dev/.local/bin/claude", "--resume", ID]),
            session(Some(ID))
        );
        assert_eq!(cmd(&["claude", "-r", ID]), session(Some(ID)));
        assert_eq!(
            cmd(&["claude", &format!("--session-id={ID}")]),
            session(Some(ID))
        );
        assert_eq!(
            cmd(&["claude", "--model", "opus", "--session-id", ID]),
            session(Some(ID))
        );
        assert_eq!(cmd(&["claude", "fix the tide tables"]), session(None));
    }

    #[test]
    fn resume_without_an_id_or_with_a_fork_has_no_id() {
        assert_eq!(cmd(&["claude", "--resume"]), session(None));
        assert_eq!(cmd(&["claude", "--resume", "--verbose"]), session(None));
        assert_eq!(cmd(&["claude", "--resume", "tide"]), session(None));
        assert_eq!(
            cmd(&["claude", "--resume", ID, "--fork-session"]),
            session(None)
        );
    }

    #[test]
    fn node_installs_are_recognised() {
        assert_eq!(
            cmd(&[
                "node",
                "/usr/lib/node_modules/@anthropic-ai/claude-code/cli.js",
                "-r",
                ID
            ]),
            session(Some(ID))
        );
        assert_eq!(
            cmd(&["node", "/home/dev/.npm-global/bin/claude"]),
            session(None)
        );
        assert_eq!(
            cmd(&["node", "/home/dev/projects/tidepool-api/server.js"]),
            None
        );
    }

    #[test]
    fn helpers_and_other_programs_are_not_sessions() {
        assert_eq!(
            cmd(&["claude", "daemon", "run", "--origin", "transient"]),
            None
        );
        assert_eq!(cmd(&["claude", "mcp", "serve"]), None);
        assert_eq!(
            cmd(&["claude bg-pty-host", "--bg-pty-host", "/tmp/x.sock"]),
            None
        );
        assert_eq!(cmd(&["claude bg-spare", "--bg-spare", "/tmp/y.sock"]), None);
        assert_eq!(cmd(&["vim", "claude"]), None);
        assert_eq!(cmd(&["/usr/bin/claudette"]), None);
        assert_eq!(parse_cmdline(b""), None);
    }

    fn proc(cwd: &str, id: Option<&str>) -> ClaudeProcess {
        ClaudeProcess {
            pid: 1,
            cwd: cwd.into(),
            session_id: id.map(str::to_owned),
            started: None,
        }
    }

    fn started_at(cwd: &str, hour: u32) -> ClaudeProcess {
        ClaudeProcess {
            started: at(hour),
            ..proc(cwd, None)
        }
    }

    fn at(hour: u32) -> Option<DateTime<Utc>> {
        Some(format!("2026-03-14T{hour:02}:00:00Z").parse().unwrap())
    }

    fn sorted(set: HashSet<String>) -> Vec<String> {
        let mut v: Vec<_> = set.into_iter().collect();
        v.sort();
        v
    }

    #[test]
    fn named_process_marks_its_session_only() {
        let sessions = [(ID, at(1)), (ID2, at(5))];
        let live = live_sessions(&[proc(REPO, Some(ID))], REPO, &sessions);
        assert_eq!(sorted(live), [ID]);
    }

    #[test]
    fn unnamed_process_marks_most_recent_session_in_its_cwd() {
        let sessions = [(ID, at(1)), (ID2, at(5))];
        assert_eq!(
            sorted(live_sessions(&[proc(REPO, None)], REPO, &sessions)),
            [ID2]
        );
        let elsewhere = proc("/home/dev/projects/aurora-notes", None);
        assert!(live_sessions(&[elsewhere], REPO, &sessions).is_empty());
    }

    #[test]
    fn unnamed_processes_skip_sessions_already_claimed() {
        let sessions = [(ID, at(1)), (ID2, at(5))];
        let procs = [proc(REPO, Some(ID2)), proc(REPO, None)];
        assert_eq!(sorted(live_sessions(&procs, REPO, &sessions)), [ID, ID2]);
    }

    #[test]
    fn new_session_not_on_disk_yet_marks_nothing() {
        let sessions = [(ID, at(1))];
        let fresh = "3f6c1a52-0000-4000-8000-0000000000ff";
        assert!(live_sessions(&[proc(REPO, Some(fresh))], REPO, &sessions).is_empty());
    }

    #[test]
    fn no_processes_means_nothing_live() {
        assert!(live_sessions(&[], REPO, &[(ID, at(1))]).is_empty());
    }

    #[test]
    fn unnamed_process_ignores_sessions_written_before_it_started() {
        // A fresh session with no transcript yet must not claim an old one.
        let sessions = [(ID, at(1)), (ID2, at(5))];
        assert!(live_sessions(&[started_at(REPO, 6)], REPO, &sessions).is_empty());
        assert_eq!(
            sorted(live_sessions(&[started_at(REPO, 4)], REPO, &sessions)),
            [ID2]
        );
        // Written at the very second it started still counts.
        assert_eq!(
            sorted(live_sessions(&[started_at(REPO, 5)], REPO, &sessions)),
            [ID2]
        );
    }

    #[test]
    fn later_started_processes_choose_first() {
        // The process started at 4 can only own ID2; the one started at 0 takes ID.
        let sessions = [(ID, at(1)), (ID2, at(5))];
        let procs = [started_at(REPO, 0), started_at(REPO, 4)];
        assert_eq!(sorted(live_sessions(&procs, REPO, &sessions)), [ID, ID2]);
    }

    #[test]
    fn stat_start_ticks_parse_after_the_command_name() {
        let tail = "S 1 2 3 0 -1 4194560 100 0 0 0 5 6 0 0 20 0 12 0 4242 1000000 250";
        assert_eq!(
            parse_stat_start_ticks(&format!("812 (claude) {tail}")),
            Some(4242)
        );
        // The command name may hold spaces and parentheses.
        assert_eq!(
            parse_stat_start_ticks(&format!("812 (my (odd) name) {tail}")),
            Some(4242)
        );
        assert_eq!(parse_stat_start_ticks("812 (claude) S 1 2"), None);
        assert_eq!(parse_stat_start_ticks("garbage"), None);
    }

    #[test]
    fn boot_time_parses_from_proc_stat() {
        let stat =
            "cpu  10 0 20 300 0 0 0 0 0 0\nintr 1 2 3\nctxt 999\nbtime 1773446400\nprocesses 77\n";
        assert_eq!(parse_btime(stat), Some(1_773_446_400));
        assert_eq!(parse_btime("cpu 1 2 3\n"), None);
    }

    #[test]
    fn start_time_is_boot_time_plus_ticks() {
        let started = start_time(1_773_446_400, 4250).unwrap();
        assert_eq!(started.to_rfc3339(), "2026-03-14T00:00:42.500+00:00");
    }
}
