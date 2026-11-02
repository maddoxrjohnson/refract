# Hook pids: which session a `claude` process runs

Status: built 2026-11-02 (see §9 for where it differs).

## 1. Problem

A process started as `claude --resume X` (or `--session-id X`) keeps `X` on its command line after the user runs `/clear` or `/resume Y`, but it now runs Y. `procs::live_sessions` gave the process to X, the id it names, and never let it claim Y, so Y was not live. Y's transcript is mid-task, so the app showed "Session didn't finish" (interrupted) and no spinner while the agent was working.

Seen: a process running `claude --resume 6fb8…`; the status file of the new session `8ffe…` says `working`; no status file for `6fb8…` (`SessionEnd` wrote `ended`, then the hourly sweep removed it).

## 2. Decisions

| Question | Decision |
|---|---|
| How to learn which process ran a hook | The status script records the pids of its ancestors |
| How many | Up to 4, from `$PPID` up, stopping at pid <= 1 or an unreadable `/proc/<pid>/stat` |
| Where Claude Code is in the chain | Not relied on; it is probably the grandparent (`sh -c '<command>'` then `sh <script>`), so the first pid that is a running `claude` process counts |
| Old installs | Files without `pids` still parse; the changed script bytes make the install show "Needs update"; a fallback rule covers them meanwhile |
| Pid reuse | A pid counts only if its process started no later than 2 s after the report |

## 3. Status file

`{"status":"working","event":"PostToolUse","at":1793661784,"pids":[123,122,100,1]}`. `pids` is empty when nothing was readable. Only digits reach the JSON: each pid is checked with a `case` before it is added. The ppid is field 4 of `/proc/<pid>/stat`, taken after the last `)` since the command name may hold spaces and parentheses. The script stays POSIX sh, uses only builtins for the walk (`read`, parameter expansion), prints nothing and exits 0.

## 4. The rule (`procs::live_sessions`)

Inputs: all running `claude` processes, the directory, its sessions (id, last written, first timestamp), and every hook report by session id.

1. **Hook claims.** A report's process is the first of its pids that is a running `claude` process (any directory) whose start is unknown or no later than `at` + 2 s. Each such process runs the session of the latest report leading to it, across all reports; at the same second a report that is not `ended` wins, then the greater session id. If that session is in this directory's sessions, it is live with that process (certain). Other reports leading to the process make nothing live through it. The process no longer uses its command-line id and is skipped by steps 2-4.
2. **Named processes** not placed in step 1: the named id is live, with that process (certain); of two naming one id the first is kept.
3. **Unnamed processes** in the directory not placed in step 1: as before (most recent unclaimed session written at or after the process started, later-started first; certain only when it is the only one).
4. **Fallback** (no hook pids): a named, unplaced process P in the directory whose named session X is on disk moves to the newest Y that is unclaimed after steps 1-3, whose first transcript timestamp is at or after P's start, whose last write is later than X's, and whose hook report, if any, leads to no other running process. It needs P's start time and no unplaced unnamed process in the directory. Y is then live with P (certain); X stays live only if another unplaced, unmoved process names it.

## 5. Other readers of `ClaudeProcess::session_id`

- `state.rs` `live_set` (snapshot, recheck, `live_process` for focus launches and `stopAgent`) passes the hook reports.
- `refactor.rs` `look`: `named`, `others`, `live_ids` and the background `LiveProcess` lookup use the session a process runs (hook first, else its command line).
- `migrate/worker.rs` `busy_agents` only words a message ("An agent (pid …, session …) is running"); any process in the source blocks the migration whatever it runs, so it is unchanged.

## 6. Tests

- `procs.rs`: `/clear` in a `--resume X` process with hook pids (Y live with the pid, X not); same-second `ended` vs new session; a pid whose process started after the report is ignored (and the 2 s slack); two reports for one process (only the newest); a newer report from another directory takes the process; a placed process is not matched again; the fallback moves a named process (newest Y, X kept by another process naming it); fallback refused with an unnamed process, Y started before P, Y older than X, Y's hook leading elsewhere, unknown start.
- `refract-core` `hook.rs`: status files with and without `pids` parse.
- `refract-agent` `hook.rs`: reports keep the first four pids.
- `tests/hook.rs`: the script run as Claude Code runs it writes digits-only pids, the first being the shell that ran the command and the second the test; every event in the end-to-end run writes pids.
- `state.rs`: `set_hooks` with a report leading to a `--resume X` process makes Y live and working, X not live.

## 7. Docs

`docs/architecture.md` (live state, "Which process runs a session", status file format), ADR 0011 amendment.

## 9. As built

- `HookReport` stays `Copy`: `pids: [u32; 4]` with 0 for none, `HookReport::new(status, at)` and `pids()`.
- `set_hooks` now works liveness out again for every repo from the processes last scanned, since a new report can move a process; the background count is redone only when the live set changed.
- Ties at the same second prefer a report that is not `ended`, because `/clear` writes `ended` for X and `SessionStart` (`finished`) for Y within the same second and the session id alone would pick at random.
- A report without `at` leads to no process (the pid-reuse guard needs it); the script always writes `at`.
- `procs::hook_sessions` (pid to session id) is public for `refactor.rs`.
- `earliest_unnamed_start` (background work since for an uncertain session) still counts unnamed processes the hook placed; that can only make the start earlier, which counts more background work, not less.
