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
//!
//! A process runs in a terminal of a Refract VS Code window when its environment has
//! `REFRACT_FOLDER`: the Refract extension sets it, to the window's single folder, on
//! every terminal the window opens, and every child inherits it.

use std::collections::{HashMap, HashSet};
use std::fs;
use std::io::Read;
use std::path::Path;

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
    /// `REFRACT_FOLDER` from its environment: the folder of the Refract window whose
    /// terminal it runs in.
    pub refract_folder: Option<String>,
}

/// The process running a live session.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LiveProcess {
    pub pid: u32,
    /// When it started, to tell it from a later process given the same pid.
    pub started: Option<DateTime<Utc>>,
    /// See [`ClaudeProcess::refract_folder`].
    pub refract_folder: Option<String>,
}

impl LiveProcess {
    /// True when it runs in a terminal of the Refract window on exactly `repo`.
    pub fn in_refract_window(&self, repo: &str) -> bool {
        self.refract_folder
            .as_deref()
            .is_some_and(|folder| paths_match(folder, repo))
    }
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

/// Field 4 (`ppid`) of a `/proc/<pid>/stat` line, counted from the last `)` as in
/// [`parse_stat_start_ticks`].
pub fn parse_stat_ppid(stat: &str) -> Option<u32> {
    let (_, rest) = stat.rsplit_once(')')?;
    rest.split_whitespace().nth(1)?.parse().ok()
}

/// The most `/proc/<pid>/environ` that is read. An environment is usually a few KB.
const MAX_ENVIRON_BYTES: u64 = 256 * 1024;

/// The variable the Refract extension sets on its windows' terminals.
pub const FOLDER_VAR: &str = "REFRACT_FOLDER";

/// The value of `name` in a NUL-separated `/proc/<pid>/environ`, if it is set and not
/// empty.
pub fn environ_value(raw: &[u8], name: &str) -> Option<String> {
    raw.split(|&b| b == 0).find_map(|entry| {
        let value = entry.strip_prefix(name.as_bytes())?.strip_prefix(b"=")?;
        (!value.is_empty()).then(|| String::from_utf8_lossy(value).into_owned())
    })
}

/// `REFRACT_FOLDER` of the process. An environment that cannot be read (another
/// user's process, exited) gives none.
pub fn refract_folder(pid: u32) -> Option<String> {
    read_bounded(&Path::new("/proc").join(pid.to_string()).join("environ"))
        .and_then(|raw| environ_value(&raw, FOLDER_VAR))
}

fn read_bounded(path: &Path) -> Option<Vec<u8>> {
    let mut raw = Vec::new();
    fs::File::open(path)
        .ok()?
        .take(MAX_ENVIRON_BYTES)
        .read_to_end(&mut raw)
        .ok()?;
    Some(raw)
}

/// The most processes [`ancestors`] lists.
pub const MAX_ANCESTORS: usize = 16;

/// `pid` and its ancestors, nearest first, by following `parent`. Stops before pid 1
/// (init, or WSL's own init), at a pid already listed, at an unknown parent, or after
/// [`MAX_ANCESTORS`].
pub fn ancestors(pid: u32, parent: impl Fn(u32) -> Option<u32>) -> Vec<u32> {
    let mut chain = Vec::new();
    let mut next = Some(pid);
    while let Some(pid) = next {
        if pid <= 1 || chain.contains(&pid) || chain.len() == MAX_ANCESTORS {
            break;
        }
        chain.push(pid);
        next = parent(pid);
    }
    chain
}

/// [`ancestors`] from `/proc/<pid>/stat`.
pub fn proc_ancestors(pid: u32) -> Vec<u32> {
    ancestors(pid, |pid| {
        fs::read_to_string(format!("/proc/{pid}/stat"))
            .ok()
            .and_then(|stat| parse_stat_ppid(&stat))
    })
}

/// True when `pid` still runs a Claude Code session.
pub fn is_claude(pid: u32) -> bool {
    fs::read(format!("/proc/{pid}/cmdline")).is_ok_and(|raw| parse_cmdline(&raw).is_some())
}

/// When `pid` started, from `/proc`.
pub fn process_started(pid: u32) -> Option<DateTime<Utc>> {
    let btime = fs::read_to_string("/proc/stat")
        .ok()
        .and_then(|s| parse_btime(&s))?;
    let ticks = fs::read_to_string(format!("/proc/{pid}/stat"))
        .ok()
        .and_then(|stat| parse_stat_start_ticks(&stat))?;
    start_time(btime, ticks)
}

/// True when a process started at `now_started` is the one recorded as starting at
/// `recorded`. An unknown start time on either side is no proof, so it says no.
pub fn same_process(recorded: Option<DateTime<Utc>>, now_started: Option<DateTime<Utc>>) -> bool {
    recorded.is_some() && recorded == now_started
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
                refract_folder: refract_folder(pid),
            })
        })
        .collect()
}

/// Decides which of a working directory's sessions are live, and which process runs
/// each when that is certain.
///
/// A session is live when a process names its id. Each process in `cwd` that names no
/// id is matched to the most recent session no other process claimed and that was
/// written at or after the process started, so a fresh session with no transcript yet
/// claims nothing. Later-started processes choose first, since fewer sessions fit them.
/// A process whose start time is unknown may take any session. A process naming an id
/// that is not on disk yet (a fresh `--session-id`) marks nothing. `sessions` holds
/// (id, when its transcript was last written) pairs.
///
/// The process is known for certain when it names the session id (of two naming the
/// same id, the first listed is kept), or when it is the only process in `cwd` naming
/// no id. A session matched to one of several such processes is live with no process:
/// the match decides liveness well enough, but not which terminal it runs in.
pub fn live_sessions(
    procs: &[ClaudeProcess],
    cwd: &str,
    sessions: &[(&str, Option<DateTime<Utc>>)],
) -> HashMap<String, Option<LiveProcess>> {
    let known: HashSet<&str> = sessions.iter().map(|(id, _)| *id).collect();
    let mut live: HashMap<String, Option<LiveProcess>> = HashMap::new();
    for process in procs {
        if let Some(id) = process
            .session_id
            .as_deref()
            .filter(|id| known.contains(id))
        {
            live.entry(id.to_owned())
                .or_insert_with(|| Some(live_process(process)));
        }
    }
    let mut unnamed: Vec<&ClaudeProcess> = procs
        .iter()
        .filter(|p| p.session_id.is_none() && paths_match(&p.cwd, cwd))
        .collect();
    let certain = unnamed.len() == 1;
    // `None` sorts first, so reversed the unknown start times come last.
    unnamed.sort_by_key(|p| std::cmp::Reverse(p.started));
    let mut recent: Vec<_> = sessions.iter().collect();
    recent.sort_by_key(|s| std::cmp::Reverse(s.1));
    for process in unnamed {
        let claim = recent.iter().find(|(id, written)| {
            !live.contains_key(*id)
                && process
                    .started
                    .is_none_or(|started| written.is_some_and(|w| w >= started))
        });
        if let Some((id, _)) = claim {
            live.insert((*id).to_owned(), certain.then(|| live_process(process)));
        }
    }
    live
}

fn live_process(process: &ClaudeProcess) -> LiveProcess {
    LiveProcess {
        pid: process.pid,
        started: process.started,
        refract_folder: process.refract_folder.clone(),
    }
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
            refract_folder: None,
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

    fn sorted(live: HashMap<String, Option<LiveProcess>>) -> Vec<String> {
        let mut v: Vec<_> = live.into_keys().collect();
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
    fn live_sessions_keep_the_process_running_each() {
        let sessions = [(ID, at(1)), (ID2, at(5))];
        let named = ClaudeProcess {
            pid: 812,
            refract_folder: Some(REPO.into()),
            ..started_at(REPO, 0)
        };
        let named = ClaudeProcess {
            session_id: Some(ID.into()),
            ..named
        };
        let unnamed = ClaudeProcess {
            pid: 907,
            ..proc(REPO, None)
        };
        let live = live_sessions(&[named, unnamed], REPO, &sessions);
        assert_eq!(
            live[ID],
            Some(LiveProcess {
                pid: 812,
                started: at(0),
                refract_folder: Some(REPO.into()),
            })
        );
        // The only process naming no id in the repo is certain too.
        assert_eq!(live[ID2].as_ref().map(|p| p.pid), Some(907));
    }

    #[test]
    fn a_guess_among_several_unnamed_processes_records_no_process() {
        let sessions = [(ID, at(1)), (ID2, at(5))];
        let procs = [
            ClaudeProcess {
                pid: 812,
                ..started_at(REPO, 0)
            },
            ClaudeProcess {
                pid: 907,
                ..started_at(REPO, 4)
            },
        ];
        let live = live_sessions(&procs, REPO, &sessions);
        assert_eq!(sorted(live.clone()), [ID, ID2]);
        assert_eq!(live[ID], None);
        assert_eq!(live[ID2], None);
        // A process naming its id stays certain beside an unnamed one, which is then
        // the only unnamed one.
        let procs = [
            ClaudeProcess {
                pid: 812,
                ..proc(REPO, Some(ID))
            },
            ClaudeProcess {
                pid: 907,
                ..proc(REPO, None)
            },
        ];
        let live = live_sessions(&procs, REPO, &sessions);
        assert_eq!(live[ID].as_ref().map(|p| p.pid), Some(812));
        assert_eq!(live[ID2].as_ref().map(|p| p.pid), Some(907));
    }

    #[test]
    fn unnamed_processes_elsewhere_do_not_make_a_guess_uncertain() {
        let sessions = [(ID, at(1))];
        let procs = [
            ClaudeProcess {
                pid: 812,
                ..proc(REPO, None)
            },
            proc("/home/dev/projects/aurora-notes", None),
        ];
        let live = live_sessions(&procs, REPO, &sessions);
        assert_eq!(live[ID].as_ref().map(|p| p.pid), Some(812));
    }

    #[test]
    fn refract_folder_is_read_from_the_environment() {
        let raw = b"HOME=/home/dev\0REFRACT_FOLDER=/home/dev/projects/tidepool-api\0TERM=xterm\0";
        assert_eq!(
            environ_value(raw, FOLDER_VAR).as_deref(),
            Some("/home/dev/projects/tidepool-api")
        );
        assert_eq!(
            environ_value(b"REFRACT_FOLDER=/x", FOLDER_VAR).as_deref(),
            Some("/x")
        );
        assert_eq!(environ_value(b"REFRACT_FOLDER=\0", FOLDER_VAR), None);
        assert_eq!(environ_value(b"XREFRACT_FOLDER=/x\0", FOLDER_VAR), None);
        assert_eq!(environ_value(b"REFRACT_FOLDERS=/x\0", FOLDER_VAR), None);
        assert_eq!(environ_value(b"", FOLDER_VAR), None);
    }

    #[test]
    fn only_the_window_on_exactly_this_repo_counts() {
        let process = |folder: Option<&str>| LiveProcess {
            pid: 812,
            started: None,
            refract_folder: folder.map(str::to_owned),
        };
        assert!(process(Some(REPO)).in_refract_window(REPO));
        assert!(process(Some(&format!("{REPO}/"))).in_refract_window(REPO));
        assert!(!process(Some("/home/dev/projects")).in_refract_window(REPO));
        assert!(!process(Some(&format!("{REPO}/src"))).in_refract_window(REPO));
        assert!(!process(None).in_refract_window(REPO));
        let mnt = process(Some("/mnt/c/Users/dev/Tidepool"));
        assert!(mnt.in_refract_window("/mnt/c/users/dev/tidepool/"));
    }

    #[test]
    fn same_process_needs_a_known_unchanged_start() {
        assert!(same_process(at(3), at(3)));
        assert!(!same_process(at(3), at(4)));
        assert!(!same_process(at(3), None));
        assert!(!same_process(None, None));
    }

    #[test]
    fn stat_ppid_parses_after_the_command_name() {
        assert_eq!(parse_stat_ppid("812 (claude) S 640 812 640 0"), Some(640));
        assert_eq!(parse_stat_ppid("812 (my (odd) name) R 7 1 1"), Some(7));
        assert_eq!(parse_stat_ppid("812 (claude) S"), None);
        assert_eq!(parse_stat_ppid("garbage"), None);
    }

    #[test]
    fn ancestors_stop_before_init_at_loops_and_at_the_cap() {
        let parents: HashMap<u32, u32> = [(812, 640), (640, 600), (600, 1)].into();
        let parent = |pid| parents.get(&pid).copied();
        assert_eq!(ancestors(812, parent), [812, 640, 600]);
        // An unknown parent ends the chain.
        assert_eq!(ancestors(900, parent), [900]);
        assert!(ancestors(1, parent).is_empty());
        let looped: HashMap<u32, u32> = [(5, 6), (6, 5)].into();
        assert_eq!(ancestors(5, |pid| looped.get(&pid).copied()), [5, 6]);
        let long = ancestors(1000, |pid| Some(pid + 1));
        assert_eq!(long.len(), MAX_ANCESTORS);
        assert_eq!(long[0], 1000);
    }

    #[test]
    fn this_process_has_ancestors_in_proc() {
        let me = std::process::id();
        assert_eq!(proc_ancestors(me).first(), Some(&me));
        assert!(!is_claude(me));
        assert!(process_started(me).is_some());
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
