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
use refract_core::classify::{HookReport, HookStatus};
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
    /// The same, as field 22 of `/proc/<pid>/stat`: clock ticks after boot. Exact, and
    /// unmoved when the wall clock steps, so it is what status hook reports match.
    pub start_ticks: Option<u64>,
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
            let start_ticks = fs::read_to_string(dir.join("stat"))
                .ok()
                .and_then(|stat| parse_stat_start_ticks(&stat));
            let started = start_ticks
                .zip(btime)
                .and_then(|(ticks, btime)| start_time(btime, ticks));
            Some(ClaudeProcess {
                pid,
                cwd: cwd.to_string_lossy().into_owned(),
                session_id: command.session_id,
                started,
                start_ticks,
                refract_folder: refract_folder(pid),
            })
        })
        .collect()
}

/// What [`live_sessions`] needs to know of one session's transcript.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SessionTimes<'a> {
    pub id: &'a str,
    /// When its transcript was last written.
    pub modified: Option<DateTime<Utc>>,
    /// Its transcript's first timestamp.
    pub first: Option<DateTime<Utc>>,
}

/// What the status hook says about the running `claude` processes: worked out once per
/// scan of `/proc` or change of reports, and used for every directory.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct HookRuns {
    /// The session each process runs by its hook, by pid.
    runs: HashMap<u32, String>,
    /// Sessions whose report comes from a script too old to record pids and does not
    /// say `ended`: the only ones step 4 of [`live_sessions`] moves a process to.
    unplaced: HashSet<String>,
}

impl HookRuns {
    /// Which session each of `procs` runs by the reports in `hooks` (by session id,
    /// from any directory). A report leads to the first of its ancestors that is one of
    /// `procs` with the very same start ticks, so a pid reused since does not count; a
    /// report saying `ended`, or without a time, leads nowhere. A process runs the
    /// session of the latest report leading to it, the greater session id at the same
    /// second.
    pub fn new(procs: &[ClaudeProcess], hooks: &HashMap<String, HookReport>) -> HookRuns {
        let mut best: HashMap<u32, (i64, &str)> = HashMap::new();
        for (id, report) in hooks {
            let Some(at) = report.at else {
                continue;
            };
            if report.status == HookStatus::Ended {
                continue;
            }
            let process = report.ancestors().find_map(|(pid, start)| {
                procs
                    .iter()
                    .find(|p| p.pid == pid && p.start_ticks == Some(start))
            });
            if let Some(process) = process {
                let key = (at, id.as_str());
                best.entry(process.pid)
                    .and_modify(|b| *b = (*b).max(key))
                    .or_insert(key);
            }
        }
        HookRuns {
            runs: best
                .into_iter()
                .map(|(pid, (_, id))| (pid, id.to_owned()))
                .collect(),
            unplaced: hooks
                .iter()
                .filter(|(_, r)| !r.has_pids() && r.status != HookStatus::Ended)
                .map(|(id, _)| id.clone())
                .collect(),
        }
    }

    /// The session process `pid` runs by its hook, if a report leads to it.
    pub fn session(&self, pid: u32) -> Option<&str> {
        self.runs.get(&pid).map(String::as_str)
    }
}

/// Decides which of a working directory's sessions are live, and which process runs
/// each when that is certain. `procs` are all running `claude` processes, `sessions`
/// the transcripts in `cwd`, and `hooks` what the status hook says about `procs`.
///
/// A process keeps the id on its command line (`--resume X`, `--session-id X`) after
/// `/clear` or `/resume Y` switches it to another session, so the status hook comes
/// first. In order:
///
/// 1. Hook reports. A report's process is the first of its (at most three) recorded
///    ancestors that is a running `claude` process, in any directory, with the very
///    same start ticks (a pid reused since does not count). A report saying `ended` or
///    without a time leads to no process. A process runs the session of the latest
///    report leading to it, across all directories (the greater session id at the same
///    second). When that session is in `sessions`, it is live with that process.
///    Earlier reports leading to the same process make nothing live through it, and the
///    process is done with: its command line no longer counts, and the steps below
///    skip it.
/// 2. A process naming an id in `sessions` makes it live with that process (of two
///    naming the same id, the first listed is kept). A fresh `--session-id` not on disk
///    yet marks nothing.
/// 3. Each process in `cwd` that names no id is matched to the most recent session not
///    yet claimed that was written at or after the process started, so a fresh session
///    with no transcript yet claims nothing. Later-started processes choose first,
///    since fewer sessions fit them, and one whose start time is unknown may take any
///    session. The process is certain only when it is the only such process in `cwd`;
///    a session matched to one of several is live with no process: the match decides
///    liveness well enough, but not which terminal it runs in.
/// 4. For a hook installed before pids were recorded: a process P in `cwd` named
///    session X in `sessions` moves to the newest session Y that is still unclaimed,
///    has a report from such a hook that does not say `ended` (so some process ran a
///    hook for it and has not ended it), starts at or after P started and was written
///    after X. Then Y is live with P, and X stays live only if another process names
///    it. This needs P's start time, and no process in `cwd` naming no id left after
///    step 1, since that one could be running Y. Without such a report nothing moves:
///    a session another process ran and closed looks the same.
pub fn live_sessions<'a>(
    procs: &'a [ClaudeProcess],
    cwd: &str,
    sessions: &[SessionTimes],
    hooks: &HookRuns,
) -> HashMap<String, Option<LiveProcess>> {
    let known: HashSet<&str> = sessions.iter().map(|s| s.id).collect();
    let mut live: HashMap<String, Option<LiveProcess>> = HashMap::new();
    // Step 1.
    for process in procs {
        if let Some(id) = hooks.session(process.pid).filter(|id| known.contains(id)) {
            live.insert(id.to_owned(), Some(live_process(process)));
        }
    }
    let open: Vec<(usize, &ClaudeProcess)> = procs
        .iter()
        .enumerate()
        .filter(|(_, p)| hooks.session(p.pid).is_none())
        .collect();
    let named_in = |p: &'a ClaudeProcess| p.session_id.as_deref().filter(|id| known.contains(id));
    // Step 2.
    for (_, process) in &open {
        if let Some(id) = named_in(process) {
            live.entry(id.to_owned())
                .or_insert_with(|| Some(live_process(process)));
        }
    }
    // Step 3.
    let mut unnamed: Vec<&ClaudeProcess> = open
        .iter()
        .map(|(_, p)| *p)
        .filter(|p| p.session_id.is_none() && paths_match(&p.cwd, cwd))
        .collect();
    let certain = unnamed.len() == 1;
    // `None` sorts first, so reversed the unknown start times come last.
    unnamed.sort_by_key(|p| std::cmp::Reverse(p.started));
    let mut recent: Vec<&SessionTimes> = sessions.iter().collect();
    recent.sort_by_key(|s| std::cmp::Reverse(s.modified));
    for process in &unnamed {
        let claim = recent.iter().find(|s| {
            !live.contains_key(s.id)
                && process
                    .started
                    .is_none_or(|started| s.modified.is_some_and(|w| w >= started))
        });
        if let Some(s) = claim {
            live.insert(s.id.to_owned(), certain.then(|| live_process(process)));
        }
    }
    // Step 4.
    if !unnamed.is_empty() || hooks.unplaced.is_empty() {
        return live;
    }
    let mut moved: Vec<(usize, &str)> = Vec::new();
    for &(i, process) in &open {
        let (Some(x), Some(started)) = (named_in(process), process.started) else {
            continue;
        };
        if !paths_match(&process.cwd, cwd) {
            continue;
        }
        let x_written = sessions.iter().find(|s| s.id == x).and_then(|s| s.modified);
        let next = recent.iter().find(|y| {
            !live.contains_key(y.id)
                && hooks.unplaced.contains(y.id)
                && y.first.is_some_and(|first| first >= started)
                && y.modified.is_some_and(|w| Some(w) > x_written)
        });
        if let Some(y) = next {
            live.insert(y.id.to_owned(), Some(live_process(process)));
            moved.push((i, x));
        }
    }
    let gone: HashSet<usize> = moved.iter().map(|(i, _)| *i).collect();
    for (_, x) in moved {
        let stays = open
            .iter()
            .find(|(j, p)| !gone.contains(j) && named_in(p) == Some(x));
        match stays {
            Some((_, p)) => live.insert(x.to_owned(), Some(live_process(p))),
            None => live.remove(x),
        };
    }
    live
}

/// The earliest start of the processes in `cwd` that name no session, for a session
/// matched to one of several of them (see [`live_sessions`]): whichever runs it started
/// no earlier. `None` when there is none or a start time is unknown.
pub fn earliest_unnamed_start(procs: &[ClaudeProcess], cwd: &str) -> Option<DateTime<Utc>> {
    let starts: Vec<Option<DateTime<Utc>>> = procs
        .iter()
        .filter(|p| p.session_id.is_none() && paths_match(&p.cwd, cwd))
        .map(|p| p.started)
        .collect();
    starts
        .iter()
        .copied()
        .collect::<Option<Vec<_>>>()?
        .into_iter()
        .min()
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
    use refract_core::classify::HOOK_PIDS;

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
            start_ticks: None,
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

    /// Sessions from (id, last written) pairs, with no first timestamp.
    fn times<'a>(pairs: &[(&'a str, Option<DateTime<Utc>>)]) -> Vec<SessionTimes<'a>> {
        pairs
            .iter()
            .map(|&(id, modified)| SessionTimes {
                id,
                modified,
                first: None,
            })
            .collect()
    }

    fn without_hooks(
        procs: &[ClaudeProcess],
        cwd: &str,
        pairs: &[(&str, Option<DateTime<Utc>>)],
    ) -> HashMap<String, Option<LiveProcess>> {
        live_sessions(procs, cwd, &times(pairs), &HookRuns::default())
    }

    /// The start ticks test processes are given: any value tied to the pid will do.
    fn ticks(pid: u32) -> u64 {
        u64::from(pid) * 100 + 7
    }

    /// A status hook report written at `hour` by a hook whose ancestors are `pids`, each
    /// with the start ticks of [`ticks`].
    fn report(status: HookStatus, hour: u32, pids: &[u32]) -> HookReport {
        let mut report = HookReport::new(status, at(hour).map(|t| t.timestamp()));
        report.pids[..pids.len()].copy_from_slice(pids);
        for (slot, &pid) in report.starts.iter_mut().zip(pids) {
            *slot = ticks(pid);
        }
        report
    }

    fn hooks(procs: &[ClaudeProcess], reports: &[(&str, HookReport)]) -> HookRuns {
        let reports: HashMap<String, HookReport> = reports
            .iter()
            .map(|(id, r)| ((*id).to_owned(), *r))
            .collect();
        HookRuns::new(procs, &reports)
    }

    fn pid_of(live: &HashMap<String, Option<LiveProcess>>, id: &str) -> Option<u32> {
        live.get(id)?.as_ref().map(|p| p.pid)
    }

    /// `claude --resume <id>` as pid `pid`, started at `hour`, in `cwd`.
    fn resumed(cwd: &str, pid: u32, id: &str, hour: u32) -> ClaudeProcess {
        ClaudeProcess {
            pid,
            session_id: Some(id.into()),
            start_ticks: Some(ticks(pid)),
            ..started_at(cwd, hour)
        }
    }

    /// `claude` naming no session as pid `pid`, started at `hour`, in `cwd`.
    fn plain(cwd: &str, pid: u32, hour: u32) -> ClaudeProcess {
        ClaudeProcess {
            pid,
            start_ticks: Some(ticks(pid)),
            ..started_at(cwd, hour)
        }
    }

    const ELSEWHERE: &str = "/home/dev/projects/aurora-notes";
    const ID3: &str = "3f6c1a52-0000-4000-8000-000000000003";

    #[test]
    fn clear_in_a_resumed_process_moves_it_to_the_session_its_hook_names() {
        // `claude --resume ID`, then /clear: ID ends and ID2 starts in the same process,
        // whose shell (900, 901) ran the hooks.
        let sessions = times(&[(ID, at(1)), (ID2, at(5))]);
        let procs = [resumed(REPO, 812, ID, 0)];
        let reports = hooks(
            &procs,
            &[
                (ID, report(HookStatus::Working, 1, &[899, 812, 640])),
                (ID2, report(HookStatus::Working, 3, &[901, 812, 640])),
            ],
        );
        let live = live_sessions(&procs, REPO, &sessions, &reports);
        assert_eq!(sorted(live.clone()), [ID2]);
        assert_eq!(pid_of(&live, ID2), Some(812));
    }

    #[test]
    fn an_ended_report_claims_no_process() {
        // SessionEnd for ID lands after SessionStart for ID2; ID2 still runs.
        let sessions = times(&[(ID, at(1)), (ID2, at(5))]);
        let procs = [resumed(REPO, 812, ID2, 0)];
        let reports = hooks(
            &procs,
            &[
                (ID, report(HookStatus::Ended, 4, &[900, 812])),
                (ID2, report(HookStatus::Finished, 3, &[901, 812])),
            ],
        );
        let live = live_sessions(&procs, REPO, &sessions, &reports);
        assert_eq!(sorted(live.clone()), [ID2]);
        assert_eq!(pid_of(&live, ID2), Some(812));
        // Alone, it leaves the process to its command line.
        let procs = [resumed(REPO, 812, ID, 0)];
        let reports = hooks(&procs, &[(ID2, report(HookStatus::Ended, 4, &[900, 812]))]);
        assert_eq!(
            sorted(live_sessions(&procs, REPO, &sessions, &reports)),
            [ID]
        );
    }

    #[test]
    fn a_report_without_a_time_claims_no_process() {
        let sessions = times(&[(ID, at(1)), (ID2, at(5))]);
        let procs = [resumed(REPO, 812, ID, 0)];
        let mut untimed = report(HookStatus::Working, 3, &[901, 812]);
        untimed.at = None;
        let reports = hooks(&procs, &[(ID2, untimed)]);
        assert_eq!(
            sorted(live_sessions(&procs, REPO, &sessions, &reports)),
            [ID]
        );
    }

    #[test]
    fn a_hook_pid_now_held_by_another_process_is_ignored() {
        // Pid 812 is now a process with other start ticks than the one that ran the hook.
        let sessions = times(&[(ID, at(1)), (ID2, at(4))]);
        let procs = [resumed(REPO, 812, ID, 5)];
        let mut reused = report(HookStatus::Working, 3, &[901, 812]);
        reused.starts[1] += 1;
        let live = live_sessions(&procs, REPO, &sessions, &hooks(&procs, &[(ID2, reused)]));
        assert_eq!(sorted(live.clone()), [ID]);
        assert_eq!(pid_of(&live, ID), Some(812));
        // Pids without starts (or a process whose start is unknown) match nothing.
        let mut bare = report(HookStatus::Working, 3, &[901, 812]);
        bare.starts = [0; HOOK_PIDS];
        let live = live_sessions(&procs, REPO, &sessions, &hooks(&procs, &[(ID2, bare)]));
        assert_eq!(sorted(live), [ID]);
        let unknown = [ClaudeProcess {
            start_ticks: None,
            ..procs[0].clone()
        }];
        let fresh = report(HookStatus::Working, 3, &[901, 812]);
        let reports = hooks(&unknown, &[(ID2, fresh)]);
        assert_eq!(
            sorted(live_sessions(&unknown, REPO, &sessions, &reports)),
            [ID]
        );
        // The start is matched exactly, whatever the wall clock says: a report from
        // before the process's start time as the clock now places it still counts.
        let reports = hooks(
            &procs,
            &[(ID2, report(HookStatus::Working, 3, &[901, 812]))],
        );
        let live = live_sessions(&procs, REPO, &sessions, &reports);
        assert_eq!(sorted(live.clone()), [ID2]);
        assert_eq!(pid_of(&live, ID2), Some(812));
    }

    #[test]
    fn of_two_reports_leading_to_one_process_only_the_newest_is_live() {
        let sessions = times(&[(ID, at(1)), (ID2, at(5))]);
        let procs = [plain(REPO, 812, 0)];
        let reports = hooks(
            &procs,
            &[
                (ID, report(HookStatus::Working, 4, &[900, 812])),
                (ID2, report(HookStatus::Finished, 3, &[901, 812])),
            ],
        );
        let live = live_sessions(&procs, REPO, &sessions, &reports);
        // Without the hook the unnamed process would have taken ID2, the most recent.
        assert_eq!(sorted(live.clone()), [ID]);
        assert_eq!(pid_of(&live, ID), Some(812));
        // At the same second the greater session id wins, whatever the order.
        let reports = hooks(
            &procs,
            &[
                (ID2, report(HookStatus::Working, 4, &[900, 812])),
                (ID, report(HookStatus::Working, 4, &[901, 812])),
            ],
        );
        let live = live_sessions(&procs, REPO, &sessions, &reports);
        assert_eq!(sorted(live), [ID2]);
    }

    #[test]
    fn a_nested_claude_does_not_claim_its_parent() {
        // `claude -p` run from a session's Bash tool: the hook's ancestors are its
        // shell, the child claude (since exited), the Bash tool's shell, then the
        // parent claude, fourth, which a report does not keep.
        let sessions = times(&[(ID, at(1)), (ID3, at(5))]);
        let procs = [resumed(REPO, 812, ID, 0)];
        let file = refract_core::hook::parse_status_file(
            format!(
                r#"{{"status":"working","at":{},"pids":[950,940,930,812],"starts":[1,2,3,{}]}}"#,
                at(4).unwrap().timestamp(),
                ticks(812)
            )
            .as_bytes(),
        )
        .unwrap();
        let reports = hooks(&procs, &[(ID3, file.report())]);
        let live = live_sessions(&procs, REPO, &sessions, &reports);
        assert_eq!(sorted(live.clone()), [ID]);
        assert_eq!(pid_of(&live, ID), Some(812));
    }

    #[test]
    fn a_newer_report_from_another_directory_takes_the_process() {
        // The process runs in another directory; its newest report is for a session
        // there, so this directory's session is not live through it.
        let sessions = times(&[(ID, at(1))]);
        let procs = [plain(ELSEWHERE, 700, 0)];
        let older = (ID, report(HookStatus::Working, 2, &[900, 700]));
        let newer = (ID3, report(HookStatus::Working, 4, &[901, 700]));
        let live = live_sessions(&procs, REPO, &sessions, &hooks(&procs, &[older, newer]));
        assert!(live.is_empty());
        // Alone, the report makes the session live with that process, wherever it runs.
        let live = live_sessions(&procs, REPO, &sessions, &hooks(&procs, &[older]));
        assert_eq!(pid_of(&live, ID), Some(700));
    }

    #[test]
    fn a_process_the_hook_placed_is_not_matched_again() {
        // The process named ID2 by its hook neither keeps its command-line id nor takes
        // a session as an unnamed process; the other unnamed process is then certain.
        let sessions = times(&[(ID, at(1)), (ID2, at(5)), (ID3, at(6))]);
        let procs = [resumed(REPO, 812, ID, 0), plain(REPO, 907, 0)];
        let reports = hooks(&procs, &[(ID2, report(HookStatus::Working, 5, &[812]))]);
        let live = live_sessions(&procs, REPO, &sessions, &reports);
        assert_eq!(sorted(live.clone()), [ID2, ID3]);
        assert_eq!(pid_of(&live, ID2), Some(812));
        assert_eq!(pid_of(&live, ID3), Some(907));
    }

    /// A session last written at `written` whose transcript starts at `first`.
    fn timed(id: &str, first: u32, written: u32) -> SessionTimes<'_> {
        SessionTimes {
            id,
            modified: at(written),
            first: at(first),
        }
    }

    /// Reports from a hook installed before pids were recorded.
    fn old_hook(procs: &[ClaudeProcess], ids: &[&str]) -> HookRuns {
        let reports: Vec<(&str, HookReport)> = ids
            .iter()
            .map(|id| (*id, report(HookStatus::Working, 5, &[])))
            .collect();
        hooks(procs, &reports)
    }

    #[test]
    fn with_an_old_hook_a_named_process_moves_to_a_newer_session_it_started() {
        // `claude --resume ID` started at 2, then /clear started ID2 at 4.
        let sessions = [timed(ID, 0, 3), timed(ID2, 4, 5)];
        let procs = [resumed(REPO, 812, ID, 2)];
        let live = live_sessions(&procs, REPO, &sessions, &old_hook(&procs, &[ID2]));
        assert_eq!(sorted(live.clone()), [ID2]);
        assert_eq!(pid_of(&live, ID2), Some(812));
        // The newest such session is taken.
        let sessions = [timed(ID, 0, 3), timed(ID2, 4, 5), timed(ID3, 4, 6)];
        let reports = old_hook(&procs, &[ID2, ID3]);
        let live = live_sessions(&procs, REPO, &sessions, &reports);
        assert_eq!(sorted(live.clone()), [ID3]);
        // ID stays live while another process names it.
        let both = [resumed(REPO, 812, ID, 2), resumed(REPO, 813, ID, 1)];
        let live = live_sessions(&both, REPO, &sessions, &old_hook(&both, &[ID2, ID3]));
        assert_eq!(pid_of(&live, ID3), Some(812));
        assert_eq!(pid_of(&live, ID2), Some(813));
        assert!(!live.contains_key(ID));
        let other = [resumed(REPO, 812, ID, 2), resumed(ELSEWHERE, 813, ID, 1)];
        let live = live_sessions(&other, REPO, &sessions, &old_hook(&other, &[ID2, ID3]));
        assert_eq!(pid_of(&live, ID3), Some(812));
        assert_eq!(pid_of(&live, ID), Some(813));
    }

    #[test]
    fn the_fallback_is_refused_when_it_could_be_wrong() {
        let procs = [resumed(REPO, 812, ID, 2)];
        let check = |procs: &[ClaudeProcess], sessions: &[SessionTimes], reports: HookRuns| {
            let live = live_sessions(procs, REPO, sessions, &reports);
            assert!(!live.contains_key(ID2), "{live:?}");
            assert_eq!(pid_of(&live, ID), Some(812));
        };
        let sessions = [timed(ID, 0, 3), timed(ID2, 4, 5)];
        // An unnamed process in the directory could be running ID2.
        let with_unnamed = [procs[0].clone(), plain(REPO, 907, 6)];
        check(&with_unnamed, &sessions, old_hook(&with_unnamed, &[ID2]));
        // ID2 started before the process did.
        let early = [timed(ID, 0, 3), timed(ID2, 1, 5)];
        check(&procs, &early, old_hook(&procs, &[ID2]));
        // ID2 was last written before ID.
        let older = [timed(ID, 0, 6), timed(ID2, 4, 5)];
        check(&procs, &older, old_hook(&procs, &[ID2]));
        // The process's start time is not known.
        let unknown = [ClaudeProcess {
            started: None,
            ..procs[0].clone()
        }];
        check(&unknown, &sessions, old_hook(&unknown, &[ID2]));
        // ID2's old-hook report says it ended.
        let ended = hooks(&procs, &[(ID2, report(HookStatus::Ended, 5, &[]))]);
        check(&procs, &sessions, ended);
    }

    #[test]
    fn the_fallback_never_takes_a_session_run_by_a_process_since_exited() {
        // ID2 was run by another `claude --session-id ID2` in the repo, now closed,
        // while the process on ID waits on a long tool call.
        let procs = [resumed(REPO, 812, ID, 2)];
        let sessions = [timed(ID, 0, 3), timed(ID2, 4, 5)];
        // No hook at all, or no report for ID2.
        let live = live_sessions(&procs, REPO, &sessions, &HookRuns::default());
        assert_eq!(sorted(live.clone()), [ID]);
        assert_eq!(pid_of(&live, ID), Some(812));
        let live = live_sessions(&procs, REPO, &sessions, &old_hook(&procs, &[ID3]));
        assert_eq!(sorted(live), [ID]);
        // A report whose pids lead to no running process.
        let dead = hooks(
            &procs,
            &[(ID2, report(HookStatus::Finished, 5, &[960, 950]))],
        );
        let live = live_sessions(&procs, REPO, &sessions, &dead);
        assert_eq!(sorted(live.clone()), [ID]);
        assert_eq!(pid_of(&live, ID), Some(812));
    }

    #[test]
    fn named_process_marks_its_session_only() {
        let sessions = [(ID, at(1)), (ID2, at(5))];
        let live = without_hooks(&[proc(REPO, Some(ID))], REPO, &sessions);
        assert_eq!(sorted(live), [ID]);
    }

    #[test]
    fn unnamed_process_marks_most_recent_session_in_its_cwd() {
        let sessions = [(ID, at(1)), (ID2, at(5))];
        assert_eq!(
            sorted(without_hooks(&[proc(REPO, None)], REPO, &sessions)),
            [ID2]
        );
        let elsewhere = proc("/home/dev/projects/aurora-notes", None);
        assert!(without_hooks(&[elsewhere], REPO, &sessions).is_empty());
    }

    #[test]
    fn unnamed_processes_skip_sessions_already_claimed() {
        let sessions = [(ID, at(1)), (ID2, at(5))];
        let procs = [proc(REPO, Some(ID2)), proc(REPO, None)];
        assert_eq!(sorted(without_hooks(&procs, REPO, &sessions)), [ID, ID2]);
    }

    #[test]
    fn new_session_not_on_disk_yet_marks_nothing() {
        let sessions = [(ID, at(1))];
        let fresh = "3f6c1a52-0000-4000-8000-0000000000ff";
        assert!(without_hooks(&[proc(REPO, Some(fresh))], REPO, &sessions).is_empty());
    }

    #[test]
    fn no_processes_means_nothing_live() {
        assert!(without_hooks(&[], REPO, &[(ID, at(1))]).is_empty());
    }

    #[test]
    fn unnamed_process_ignores_sessions_written_before_it_started() {
        // A fresh session with no transcript yet must not claim an old one.
        let sessions = [(ID, at(1)), (ID2, at(5))];
        assert!(without_hooks(&[started_at(REPO, 6)], REPO, &sessions).is_empty());
        assert_eq!(
            sorted(without_hooks(&[started_at(REPO, 4)], REPO, &sessions)),
            [ID2]
        );
        // Written at the very second it started still counts.
        assert_eq!(
            sorted(without_hooks(&[started_at(REPO, 5)], REPO, &sessions)),
            [ID2]
        );
    }

    #[test]
    fn later_started_processes_choose_first() {
        // The process started at 4 can only own ID2; the one started at 0 takes ID.
        let sessions = [(ID, at(1)), (ID2, at(5))];
        let procs = [started_at(REPO, 0), started_at(REPO, 4)];
        assert_eq!(sorted(without_hooks(&procs, REPO, &sessions)), [ID, ID2]);
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
        let live = without_hooks(&[named, unnamed], REPO, &sessions);
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
        let live = without_hooks(&procs, REPO, &sessions);
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
        let live = without_hooks(&procs, REPO, &sessions);
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
        let live = without_hooks(&procs, REPO, &sessions);
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

    #[test]
    fn the_earliest_unnamed_start_in_a_directory() {
        const REPO: &str = "/home/dev/projects/tidepool-api";
        let named = ClaudeProcess {
            started: at(7),
            ..proc(REPO, Some("5e2a7c10-0000-4000-8000-0000000000a1"))
        };
        let procs = [
            started_at(REPO, 10),
            started_at(REPO, 9),
            started_at("/home/dev/projects/kelp-cli", 8),
            named,
        ];
        assert_eq!(earliest_unnamed_start(&procs, REPO), at(9));
        assert_eq!(earliest_unnamed_start(&procs, "/home/dev/other"), None);
        let unknown = [started_at(REPO, 9), proc(REPO, None)];
        assert_eq!(earliest_unnamed_start(&unknown, REPO), None);
    }
}
