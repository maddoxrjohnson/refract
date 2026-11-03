# Hook pids: which session a `claude` process runs

Status: built 2026-11-02, revised after review round 1 the same day (see §9 for where it differs).

## 1. Problem

A process started as `claude --resume X` (or `--session-id X`) keeps `X` on its command line after the user runs `/clear` or `/resume Y`, but it now runs Y. `procs::live_sessions` gave the process to X, the id it names, and never let it claim Y, so Y was not live. Y's transcript is mid-task, so the app showed "Session didn't finish" (interrupted) and no spinner while the agent was working.

Seen: a process running `claude --resume 6fb8…`; the status file of the new session `8ffe…` says `working`; no status file for `6fb8…` (`SessionEnd` wrote `ended`, then the hourly sweep removed it).

## 2. Decisions

| Question | Decision |
|---|---|
| How to learn which process ran a hook | The status script records its ancestors' pids and start times |
| How many | Up to 3, from `$PPID` up, stopping at pid <= 1 or an unreadable `/proc/<pid>/stat`. Claude Code is the second (`sh -c '<command>'`, then the script); a third leaves room for one more shell. A `claude` run from another session's Bash tool has that session's `claude` fourth, so it is never reached |
| Where Claude Code is in the chain | Not relied on: the first recorded ancestor that is a running `claude` process counts |
| Pid reuse | Each pid comes with its start in clock ticks (field 22 of the stat line); a process matches only with the very same pid and ticks. Ticks do not move when WSL's wall clock steps after a sleep, unlike a comparison of `at` with btime-based start times |
| Ended reports | Never claim a process: a nested `claude`'s last report, or a `SessionEnd` landing after the next `SessionStart`, would otherwise move a process to a finished session |
| Old installs | Files without `pids` still parse; the changed script bytes make the install show "Needs update"; a fallback covers them meanwhile, only on positive evidence from the old script |

## 3. Status file

`{"status":"working","event":"PostToolUse","at":1793661784,"pids":[8123,8090,7311],"starts":[912345,912340,880021]}`. Two parallel arrays rather than pairs, so the `pids` written by the first version of this change still parse. Both are empty when nothing was readable. Only digits reach the JSON: each pid and start is checked with a `case` before it is added. Fields are taken after the last `)` of the stat line, since the command name may hold spaces and parentheses; a shell function splits them, so the script's own `$1`/`$2` stay. The script stays POSIX sh, uses only builtins for the walk (`read`, parameter expansion, a function), prints nothing and exits 0.

## 4. The rule (`procs::live_sessions`)

Inputs: all running `claude` processes, the directory, its sessions (id, last written, first timestamp), and `HookRuns`, what every hook report says about the processes, worked out once per `/proc` scan or report change.

1. **Hook claims.** A report's process is the first of its recorded ancestors that is a running `claude` process (any directory) with the same pid and start ticks. A report saying `ended` or without `at` leads nowhere (`at` orders the reports). Each process runs the session of the latest report leading to it, across all reports, the greater session id at the same second. If that session is in this directory's sessions, it is live with that process (certain). Other reports leading to the process make nothing live through it. The process no longer uses its command-line id and is skipped by steps 2-4.
2. **Named processes** not placed in step 1: the named id is live, with that process (certain); of two naming one id the first is kept.
3. **Unnamed processes** in the directory not placed in step 1: as before (most recent unclaimed session written at or after the process started, later-started first; certain only when it is the only one).
4. **Fallback** (a hook from before pids): a named, unplaced process P in the directory whose named session X is on disk moves to the newest Y that is unclaimed after steps 1-3, has a report without pids that does not say `ended`, whose first transcript timestamp is at or after P's start, and whose last write is later than X's. It needs P's start time and no unplaced unnamed process in the directory. Y is then live with P (certain); X stays live only if another unplaced, unmoved process names it. No report for Y, or one whose ancestors lead to no running process, moves nothing: a session another process ran and closed looks the same.

## 5. Other readers of `ClaudeProcess::session_id`

- `state.rs` `live_set` (snapshot, recheck, `live_process` for focus launches and `stopAgent`) passes the agent's `HookRuns`, rebuilt on every scan and in `set_hooks`.
- `refactor.rs` `look`: `named`, `others`, `live_ids` and the background `LiveProcess` lookup use the session a process runs: the one `live_sessions` tied to it (so step 4 agrees), else the one its hook last reported, else the one it names. A refactor whose agent ran `/clear` counts as another agent busy in the worktree until it exits.
- `migrate/worker.rs` `busy_agents` only words a message ("An agent (pid …, session …) is running"); any process in the source blocks the migration whatever it runs, so it is unchanged.

## 6. Tests

- `procs.rs`: `/clear` in a `--resume X` process (Y live with the pid, X not); an `ended` report claims nothing; a report without `at` claims nothing; a pid now held by another process (other ticks), pids without starts, unknown ticks; ticks match whatever the clock says; two reports for one process (only the newest, greater id at the same second); a nested `claude` with its parent fourth does not claim the parent; a newer report from another directory takes the process; a placed process is not matched again; the fallback moves a named process (newest Y, X kept by another process naming it); fallback refused with an unnamed process, Y started before P, Y older than X, unknown start, Y's old report `ended`; fallback never takes a Y run by an exited process (no report, report for another session only, report whose pids lead nowhere).
- `refract-core` `hook.rs`: status files with and without `pids`/`starts` parse; the report keeps three; starts not one per pid are dropped.
- `refract-agent` `hook.rs`: reports keep the first three ancestors with their starts.
- `tests/hook.rs`: the script run as Claude Code runs it writes digits-only `pids` and `starts`, the first being the shell that ran the command (its field 22) and the second the test; again with a shell named `a) b (c`; every event in the end-to-end run writes both.
- `state.rs`: `set_hooks` with a report leading to a `--resume X` process makes Y live and working, X not live.
- `refactor.rs`: `look` for a refactor agent that ran `/clear` (live, busy, the new session in `live_ids`, the recorded one not live).

## 7. Docs

`docs/architecture.md` (live state, "Which process runs a session", status file format, the seven-day prune), ADR 0011 amendment.

## 9. As built

- `HookReport` stays `Copy`: `pids: [u32; 3]` and `starts: [u64; 3]` with 0 for none, `HookReport::new(status, at)`, `has_pids()` and `ancestors()` (pairs with both set). `StatusFile::report()` builds it.
- `ClaudeProcess` gains `start_ticks`, read with `started` from the same stat line.
- `procs::HookRuns` holds the pid-to-session map and the sessions with an old-script report that is not `ended`; the agent rebuilds it on every scan of `/proc` and in `set_hooks`, so it is worked out once rather than per repo.
- `set_hooks` works liveness out again for every repo from the processes last scanned, since a new report can move a process; the background count is redone only when the live set changed.
- `at` still matters: it picks the latest report for a process, so a report without it leads nowhere. The script always writes it.
- The first round's 2 s `at` slack and its same-second preference for non-`ended` reports are gone: ticks match exactly and `ended` reports never claim.
- Not done: rescanning `/proc` in `set_hooks` when a report leads nowhere. A just-started `claude` is seen at the next scan; with the three-ancestor cap its reports can no longer land on a parent meanwhile.
- `earliest_unnamed_start` (background work since for an uncertain session) still counts unnamed processes the hook placed; that can only make the start earlier, which counts more background work, not less.
- A process idle on Y for more than seven days loses Y's report to the stale prune and goes back to its command-line X until its next event (documented in architecture.md).
