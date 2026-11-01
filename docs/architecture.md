# Architecture

Refract has four parts. They are split by where they have to run, not by layer.

```
Windows                                              WSL (Ubuntu)
┌──────────────── Refract (Tauri 2) ───────────────┐                  ┌──────────── refract-agent ────────────┐
│ Interface: React, TypeScript, React Three Fiber  │                  │ Repo scanning and git status          │
│ Tray, global hotkey, start with Windows          │  JSON lines over │ Session file reading and classifying  │
│ GPU and system counters                          │ ◄──────────────► │ inotify watches: repos, sessions,     │
│ GitHub / GitLab APIs, tokens in Credential Mgr   │  stdin / stdout  │   hook status files, usage transcripts│
│ Starts VS Code, installs the helper into WSL     │  (wsl.exe)       │ Running-process checks (/proc)        │
└──────────────────────────────────────────────────┘                  │ Cloning, launch requests              │
                                                                       │ Status hook installer, token usage    │
                                                                       └───────────────────────────────────────┘
        ┌──────── VS Code window (WSL remote, "Refract" profile) ────────┐
        │ Refract extension: reads the launch request, hides the editor  │
        │ chrome, opens a terminal and runs `claude`                      │
        └─────────────────────────────────────────────────────────────────┘
```

Contents: [why the split](#why-the-work-is-split-this-way) · [layout](#repository-layout) · [protocol](#protocol-between-the-app-and-the-helper) · [client](#the-apps-client) · [session state](#session-state) · [interface](#the-interface) · [scene](#the-scene) · [remote repos](#remote-repos) · [status hook](#the-status-hook) · [refactor](#refactor) · [usage and system stats](#usage-and-system-stats) · [resource use](#resource-use-while-hidden) · [build and release](#build-ci-and-release) · [privacy](#security-and-privacy). Choices that were weighed are recorded in [decisions](decisions/README.md).

## Why the work is split this way

**The helper runs inside WSL.** Claude Code, git and most repositories live in WSL's Linux filesystem. Reading thousands of small files across the Windows–WSL boundary (`\\wsl$`) is slow. Running git there from Windows means starting a process per call. A single long-lived Linux process can use inotify, so it waits for changes instead of polling, and idles at effectively zero CPU.

**Logic lives in a library with no I/O.** `refract-core` holds the decisions that are easy to get wrong:
- Is this session interrupted?
- Which sessions are worth resuming, and which option should the picker preselect?
- How heavy is this repo?
- How many tokens were used in the last five hours?
- What does the status hook add to `settings.json`, and how is it taken out again?

It takes parsed data in and returns values, so every rule is covered by fast unit tests over recorded fixtures.

**The interface does not know where its data comes from.** The React app talks to a `Backend` interface (`app/src/backend/Backend.ts`):
- `TauriBackend` forwards calls to the Rust side.
- `DemoBackend` serves invented repos and simulated launches.

The same interface code runs in the desktop app and in the public web demo.

**VS Code does the terminal work.** Embedding a terminal in Refract would mean shipping a terminal emulator and PTY handling, and keeping each agent's process alive inside Refract. Instead Refract asks VS Code to open the folder. A small extension in that window then:
- arranges a terminal-only layout,
- runs `claude`,
- watches for later requests aimed at the same folder.

When VS Code is given a folder that is already open, it focuses that window. That covers the "focus the existing window" behaviour with no extra work. [launch.md](launch.md) has the details.

**The network is only reached from Rust.** The interface's content security policy allows no outgoing connections except to the Rust side, so GitHub and GitLab are asked from the Windows process, which also holds the tokens.

## Repository layout

| Path | Contents |
|---|---|
| `crates/refract-core` | Session parsing and classification, mass score, picker rules, usage totals, hook settings edits, Move to WSL rules, protocol types, log line splitting and stamping |
| `crates/refract-agent` | The WSL helper binary, including the status hook script (`src/status-hook.sh`) |
| `crates/refract-client` | The app's side of the protocol: starts the helper, matches responses to requests, restarts it |
| `crates/refract-launch` | `refract-launch`, a development tool that runs the launch path from WSL |
| `app/src-tauri` | The Windows app's Rust side |
| `app/src` | The interface |
| `app/scripts` | Build helpers (`ensure-helper.mjs`, `ensure-vsix.mjs`) and the demo recording scripts |
| `app/e2e` | Playwright tests of the web demo |
| `vscode-extension` | The VS Code extension |
| `.github/workflows` | CI, GitHub Pages and release builds |
| `scripts` | `dev-app.sh` and a PowerShell helper for closing test windows |
| `docs` | This document, [launch.md](launch.md), [decision records](decisions/README.md), the scene mockups (current: `mockups/gem-studies.html`) |

The root Cargo workspace holds `crates/*`. `app/src-tauri` is a separate Cargo project with its own lockfile, built for Windows.

## Protocol between the app and the helper

The two sides exchange newline-delimited JSON over the helper's standard input and output.
- **Requests** carry an `id` and receive exactly one response with the same `id`.
- **Events** have no `id` and arrive when something changes, such as a repo becoming dirty or a session starting or ending.

Types live in `crates/refract-core/src/protocol.rs`; `PROTOCOL_VERSION` is 1.

### Methods

| Method | Params | Result | Purpose |
|---|---|---|---|
| `ping` | none | `{protocolVersion, agentVersion}` | Hello |
| `listRepos` | none | `{repos, loading}` | Every local repo; starts the first load if needed |
| `getRepo` | `{path}` | one repo snapshot | One repo's snapshot |
| `prepareLaunch` | `{path, mode}` | `{requestId, sessionId, folderUri}` | Writes the request file the VS Code extension claims ([launch.md](launch.md)) |
| `setupServer` | `{vsix, commit?}` | `{installed, applicationScoped, skipped?}` | Installs the Refract extension in the WSL VS Code server ([launch.md](launch.md)) |
| `cloneRepo` | `{url, name}` | the clone's path | Clones an `https://` URL into `~/projects/<name>` |
| `addRepo` | `{path}` | `null` | Reports the repo that holds `path` from now on |
| `usage` | none | token totals, and `limits` when there is a reading | Tokens used today, in 7 days and in 5 hours, by repo and by model; plan-limit percentages from the status line ([plan limits](#plan-limits)) |
| `hookInfo` | none | `{installed, outdated, settingsPath, diff}` | Whether the status hook is installed, whether an older install needs updating, and what installing would change |
| `installHook` | none | the same | Installs the status hook |
| `uninstallHook` | none | the same | Removes it and restores `settings.json` |
| `shutdown` | none | `null` | Answers, then the helper exits |
| `migrate` | `{path, dest, snapshot}` | `{id}` | Moves a repo from a Windows drive into WSL in the background ([Move to WSL](#move-to-wsl)); a preflight refusal is error `6` with one message per line |
| `migrateRecheck` | `{id}` | `{changed, removed, unreadable, copyChanged, copyRemoved, copyUnreadable, copyMissing, busyAgents, sourceMissing}` | Check 4 again, and the placed copy against its last check: paths modified or added in the original since the last check (a folder whose time alone changed is not listed), paths gone from it, paths of it that could not be read (`.` for the folder), paths of the copy that changed since its post-install check (`["."]` when the copy is gone; while deleting, only those whose original is still there), of those the ones deleted in the copy (`copyRemoved`) and the ones that could not be read there (`copyUnreadable`, `.` for the folder), whether the copy folder is gone (`copyMissing`, so an unreadable copy is never taken for a gone one), a sentence per `claude` process running in the original, and whether the original folder itself is gone. Error `6` unless the report allows deleting or the migration is deleting; it still answers while a `migrateFinish delete` is finishing |
| `migrateBeginDelete` | `{id, sourceGone?, force?}` | `{ok, recheck}` | Sent right before the app recycles or deletes anything: the recheck, and when `changed`, `unreadable`, `copyChanged` and `busyAgents` are all empty (removed paths and a missing original are fine: the copy holds them) the migration enters the *deleting* state, saved in its state file, and `ok` is true. `sourceGone` is what Windows sees: when the helper can't see the original but `sourceGone` is false (the default), its drive is hidden from WSL, so `ok` is false with `sourceMissing` and the deleting state is not entered. In the deleting state, and when the original is gone on both sides, a change to the copy only blocks for a path the original still has (an added file, or one whose original is already gone, doesn't); `force`, only in the deleting state, is `{changed, removed}`: those paths the user saw and agreed to delete the rest anyway for, edited in the copy (`changed`) or deleted there (`copyRemoved`, in `removed`), and skips exactly those while each still changed that way (the copy keeps its version; for one in `removed` the original's version is then gone for good): any other copy change still blocks, as does a path seen edited that has since been deleted in the copy (or the other way round), and it never skips `changed`, `unreadable`, `busyAgents`, `copyUnreadable`, a hidden original or a copy that is gone. Idempotent; in the deleting state it tests again, so a partly deleted original passes, and it still answers while a `migrateFinish delete` is finishing (its config write failed), so the app's retry (this, then `migrateFinish delete`) works. Error `6` unless the report allows deleting or the migration is deleting |
| `migrateResync` | `{id}` | `null` | Copies what changed into the placed copy and checks again; `migrateProgress` and a fresh `migrateReady` follow. Only paths verifiably gone from the original leave the copy. Error `6` while deleting, when the original folder is gone, when the copy changed since its last check, or while the report's `recoveredChanges` is not empty |
| `migrateFinish` | `{id, action}` | `null` | Ends a migration whose report is ready: `delete` (only in the deleting state, once the original is gone, and refused when the copy is gone: then only `discard` ends it), `keep` (refused while deleting, when the copy was never placed, or is gone) or `discard` (refused while deleting, when the original folder is gone, the copy changed since its last check, or the report's `recoveredChanges` is not empty; with the copy gone it only forgets the migration, in any state, also while a `migrateFinish delete` is finishing; it then deletes the transcript copies only while their originals are there, and never once `delete` began removing the old transcripts, which it saves in the state file before it does). The state file goes only once the config is written; if that fails, the same `migrateFinish` again finishes it |
| `migrateCancel` | `{id}` | `{ended}` | Stops a running migration. Before the copy is placed it deletes the temporary copy and the migration is gone (`ended: true`); once placed (from the first `installing` progress on), or during a re-sync, the copy stays, the step stops and a `migrateReady` saying so follows (`ended: false`). Error `6` once the report is out, or while deleting |
| `migratePending` | none | `[{id, report, deleting, copyMissing}]` | Reports waiting for `migrateFinish`, including those from before a restart; `deleting` when only `migrateFinish delete` (or `discard`, once the copy is gone) can end it; `copyMissing` when the placed copy is gone |
| `stopAgent` | `{path, sessionId}` | `null` | Sends SIGTERM to a live session's `claude` process, unless it is working (error `7`) |
| `startRefactor` | `{path, focus?}` | a launch ticket for the worktree | Makes a worktree and branch from the repo's last commit and launches a plan-mode agent there ([Refactor](#refactor)). Error `8` on a detached HEAD, a repo with no commits, or when the repo already has a refactor |
| `resumeRefactor` | `{path, fix?}` | a launch ticket | Resumes the refactor's agent; with `fix`, asks it to rebase the branch onto the target after a conflict. Error `8` while the agent runs |
| `mergeRefactor` | `{path}` | `{outcome: "merged", commits, kept?}` or `{outcome: "blocked", reason, files}` | Rebases the branch onto its target if needed, fast-forwards the target, removes the worktree and branch. A refusal that leaves everything as it was is `blocked`, not an error. Nothing is pushed |
| `discardRefactor` | `{path}` | `null` | Deletes the refactor's worktree and branch. Error `8` while the agent runs |

### Events

| Event | Data | Sent when |
|---|---|---|
| `repoChanged` | full repo snapshot | A repo appeared or something about it changed, including its sessions and their live state |
| `repoRemoved` | `{path}` | A repo is gone or no longer under a configured root |
| `sessionChanged` | `{repoPath, session, preselect}` | One session of a repo changed state or content |
| `loadComplete` | none | The first load finished and every repo has been reported |
| `migrateProgress` | `{id, source, dest, step, done, total, placed}` | A migration moved on; `source` and `dest` are the report's paths, so a window that missed the start can place it; `step` is `copying` (bytes), `normalising`, `hashing`, `checkingGit`, `rechecking`, `installing` or `movingHistory`. An `installing` event (sent even with nothing to install) means the copy is in place, so a cancel keeps it; `placed` says so on every event (during a re-sync too), so a reloaded window knows |
| `migrateReady` | `{id, report}` | A migration finished its checks, passed or failed, and waits for `migrateFinish`; sent again after a helper restart while one is pending. Each of the report's `checks` carries its `title`, and its `advice` when it failed; `installChanges` lists what the install created, changed or removed outside the build folders; `recoveredChanges` what changed in the copy while an install was cut short (the install's doing or the user's own edits, which can't be told apart: backed up and allowed like `installChanges`, but `discard` and `migrateResync` are refused while it is not empty); `placed` says the copy is at its destination (so `keep` is possible even when a later check failed); an install step stopped by a cancel or a restart is `cancelled` |

Usage totals are not pushed. The app asks for them with `usage`.

### Errors

Errors follow JSON-RPC where a code fits: `-32700` bad JSON, `-32600` not a request, `-32601` unknown method, `-32602` bad params, `-32603` internal. Refract adds `1` repo not found (or a launch path that is not a directory), `2` server setup failed, `3` clone failed, `4` status hook failed, `5` the agent a focus launch names is no longer running in the Refract window on that folder (or, for `stopAgent`, the session is not running or its process is not certain), `6` a migration could not start or the migration named is unknown or not at that step, `7` `stopAgent` named a session that is working, `8` a refactor request was refused or failed (see [Refactor](#refactor)).

A repo snapshot (`listRepos`, `getRepo`, `repoChanged`) carries two fields added with the refactor, both optional on the wire so older peers still parse it: `refactor`, the repo's [`RefactorInfo`](#refactor) while it has one, and `lastCommit.sha`, the abbreviated id of its newest commit, which the Refactor dialog shows as "Starts from a1b2c3d". `LaunchMode` gained `planMode` on `new` and `prompt` on `resume`. None of this changed `PROTOCOL_VERSION`.

### Rules of the line protocol

An `id` may be any JSON number or string and is echoed back unchanged. A line whose `id` cannot be read (bad JSON, invalid UTF-8 with no readable id, a line over 1 MB, or an `id` that is not a number or string) is answered with `"id": null` and an error. Methods without parameters also accept `"params": {}` or `null`.

The first `listRepos` starts loading in the background and answers immediately with `{"repos": [...], "loading": true}`, listing what has loaded so far. Linux repos load first, together, and arrive as `repoChanged` events at once. Neither the answer nor they wait on `/mnt/`: roots there are searched while git reads the Linux repos, and the repos found there are watched once the Linux repos are reported, since both are slow on Windows drives. Repos under `/mnt/` load after them, four at a time, and each one arrives as soon as git has read it. A repo's arrival can change other repos' masses, which then arrive again as `repoChanged`. `{"event": "loadComplete"}` follows once, after everything is reported. Until then a `listRepos` lists the repos loaded so far with `loading: true`; after it, `loading` is `false`. A `getRepo` for a repo that has not loaded yet is answered once it has.

Git on `/mnt/` drives is slow because every `lstat` crosses to Windows (several milliseconds each), and git checks tracked files one after another. A repo of 500 tracked files takes seconds. When Git for Windows wrote the index, Linux git by default also takes every file for touched and reads it in full, because that index holds no inode numbers. On Windows drives under `/mnt/` (drvfs mounts, not Linux disks attached under `/mnt/wsl/`) the helper therefore skips untracked files, compares files by size and modification time only (`core.checkStat=minimal`), and reads a repo's status and its HEAD facts at the same time. The working-tree watches are kept there: edits made from Linux, such as by Claude Code running in WSL, produce inotify events, while edits made from Windows produce none on any `/mnt/` watch.

Repos that appear later are found by watching every directory the search walks under the roots. A folder with a `.git` that git cannot read is not reported. It might be a `git init` or a copy still in progress, or a repo git refuses (owned by another user, or made by a newer git). Git is never allowed to look above the folder for a repository enclosing it. The helper watches the folder and its git directory. With nothing prompting it, the helper tries the folder again 2, 10 and 60 seconds after each failure, then stops. A change in the folder or its git directory prompts a try 2 seconds after the last change, so a copy is read soon after it ends. While changes keep coming, the helper still tries at least once a minute. After three failed tries prompted by changes, it tries at most once a minute. A repo that stays unreadable therefore costs a try a minute while something writes to it, and nothing while nothing does. A repo replaced at the same path, for example moved away with another copied or cloned in its place, is read and watched again. The helper notices a replacement by the inode numbers of the folder and its `.git`. On file systems where those are not stable (overlayfs with `xino=off`, sshfs without `use_ino`, CIFS with `noserverino`), a rescan can mistake a repo for replaced and read it again, which is harmless. `/mnt/` drives keep them stable.

`prepareLaunch` leaves a request file for the VS Code extension and answers with the folder URI to open. `setupServer` runs on a separate thread inside the helper, so other requests are still answered while it runs, and its response can arrive after theirs.

`cloneRepo` and `addRepo` are described under [Cloning and adding repos](#cloning-and-adding-repos).

### Move to WSL

The `migrate` methods and events are specified in `.plans/wsl-migration-spec.md`; the types are in `protocol.rs`. Before sending `migrate`, the app writes the source snapshot (the file manifest S0 and the git snapshot G0 it took with `git.exe`) as JSON and passes the file's Linux path as `snapshot`. The exact format, and a Linux-git builder of it that the tests use, are in `crates/refract-agent/src/migrate/source.rs`. Each repo's G0 includes `indexFlags` (`git ls-files -v`: skip-worktree and assume-unchanged), and each repo records the `autocrlf` its blob ids were computed with: what `git config core.autocrlf` says for `git.exe`, global and system config included (`false` when unset). The helper normalises the copy and re-reads changed source files with that value; a snapshot without it means the repo's local value, else `true`. Both sides run git with `core.quotepath=false`, `GIT_OPTIONAL_LOCKS=0` and `LC_ALL=C`, hash files with `core.safecrlf=false`, and take a symlink's blob from its target as stored. The rules that decide what to skip, what to compare and what to refuse are in `crates/refract-core/src/migrate` (module docs there); the helper's steps are in `crates/refract-agent/src/migrate` (module docs there).

Each migration runs on a thread of its own, so requests are answered while it copies. The helper refuses what it can check from Linux (destination, free space, worktrees, a git operation in progress, links leading out, Git LFS), copies into `.refract-migrate-<name>-<id>` next to the destination, normalises line endings and git config, and checks the copy against the snapshot. Then it stats the source again and re-copies what changed, up to three rounds. Only when checks 1 to 4 pass does it rename the copy into place, run the install and copy the session history. A failed report deletes the temporary copy at once, so only `discard` is accepted for it. A step that fails, or panics, ends in a `migrateReady` whose checks failed or did not run, never in silence; a re-sync that fails the same way sends a report too. Once the copy is placed nothing but `discard` (with its guards) deletes it: a cancel or a panic during the install or history ends in a report, a cancelled re-sync too. Each step after placement that writes (install, history, re-sync) is saved as running in the state file before it starts, the transcript copies about to be written and each batch of re-synced paths with it; a helper that restarts mid-step reports it cut short: what changed in the copy since it was placed is listed apart as `recoveredChanges` (it may include the user's edits made while the helper was down) and backed up as an install's, and blocks `discard` and re-sync, the transcript copies already written are found (so `discard` deletes them) and the history check fails, and a re-sync fails check 4 until one runs to its end. Only what a re-sync wrote (its paths, each repo's index and config, new loose objects) counts as its doing in the copy's stat, so an edit made in the copy meanwhile still blocks `discard`. State files are flushed to disk (file and folder) before a request that changed them is answered. If the helper itself stops mid-copy, the interface drops the running entry when it sees the helper stop, and the copy is deleted when the helper starts again. A placed migration is kept in `~/.local/share/refract/migrations/state/` until `migrateFinish`: after a restart the helper sends its `migrateReady` again and lists it in `migratePending`. At startup the helper deletes temporary copies left by a helper that stopped mid-copy.

Deleting the original goes: `migrateBeginDelete` with `sourceGone` from Windows (if `ok` is false, show the recheck's lists and offer `migrateResync`, or **Keep both** when the copy changed, or, while deleting, **Delete the rest anyway** with `force` set to the copy changes listed, in the two lists shown, carried to **Yes, delete permanently** if the bin can't take the rest), the app recycles the original (or skips that when it is gone), then `migrateFinish` with `delete`. A copy deleted by hand while deleting stops it for good (the rest of the original stays; the deleted part is in the Recycle Bin): `migrateFinish delete` is refused, the retry's recheck says `copyMissing`, and `discard` then ends the migration. After a helper restart the app forgets the reports the helper no longer lists in `migratePending` (a failed report never placed, say), in its own state and in the interface. The deleting state is saved before `ok` is answered, so after a crash or restart the migration comes back with `deleting: true`, and the only way on is `migrateBeginDelete` again, the rest of the recycle and `migrateFinish delete`: a partly deleted original then shows only removals, which never send a re-sync deleting the copy's files. The helper then deletes the old transcripts that are unchanged since they were copied (only if the history check passed; one that changed is copied again when its copy is untouched, else kept and named in the saved report's warnings) and points `extra` and `hidden` entries in `agent.toml` at the new path. `keep` adds the old path to `hidden`. Both add the new path to `extra` when no root covers it.

What the helper does after the report is guarded by what is on disk. It records the placed copy's stat after the install (each repo's index by its entries, so a `git status` that only refreshes stat data is no change), and the recheck, `migrateBeginDelete`, re-sync and `discard` compare the copy with it: a copy that changed may hold your work, so it is neither re-synced over nor deleted, and a resumed report whose copy is gone no longer allows deleting. A path of the original that cannot be read is reported as unreadable, never as removed, and a re-sync removes a path from the copy only when the original folder is there and the path is not. When the original folder is gone, `migrateResync` and `discard` are refused, since the copy is then the only one (unless the copy is gone too, when `discard` only forgets the migration). Tracked files are rewritten as Linux git checks them out only when their bytes are exactly what Windows git checks their clean blob out as (`checkout-index --temp` from a scratch index, with the Windows settings); any other tracked file (a clean filter that drops content, mixed line endings) is copied byte for byte, and check 1 fails on it unless Linux git stores the same blob. When a repo's index changed in the original during or after the copy, the whole repo is normalised again, and files it no longer tracks are copied again as they are. The original's version of each file the install changed or removed is backed up to `~/.local/share/refract/migrations/<timestamp>-<name>.backup/` (the report's `installBackup`); a file that can't be backed up fails check 3. Transcripts move by their own `cwd`, so a sibling repo whose project folder name collides keeps its sessions. State files are checked before a restarted helper acts on them (UUID id, plain absolute paths, transcripts under Claude Code's projects folder), the snapshot's repo paths must be relative, a submodule whose git folder resolves outside the copy stops the migration, the destination is resolved through symlinks before the `/mnt` and 9p checks, and two migrations whose folders contain one another never run together.

The app's side is in `app/src-tauri/src/migrate` (module docs there list its commands and events). It reads the source natively on NTFS: every file is hashed on up to eight threads, alternate data streams are listed, links that leave the repo and files that can't be opened refuse the move, and G0 and the clean blob ids come from `git.exe` (Linux git through `wsl.exe` when `git.exe` is missing). **Delete original** asks the helper to recheck the source, refuses while a VS Code window has the folder open, sends it to the Recycle Bin with `IFileOperation` (no UI; a folder the bin can't hold is reported, so the interface can ask before deleting it for good), leaves a `.lnk` pointing at the copy and then sends `migrateFinish delete`. A helper that does not know `migrateRecheck` makes it refuse, so the original is never deleted without the recheck. Helper error codes map to the app's error kinds: `6` is `notReady` (`refused` for `migrate` itself), `7` `busy`, `5` `notReady`, `1` `invalid`. The preflight's Linux-side script also reports, per install step, whether its tool is missing in WSL (`command -v` in a login shell, as the install looks for it), and how many transcripts the source's project folder holds, which the dialog shows.

Writes to standard output block if the app stops reading. The helper then stops reading its input. Pending file-system events are held in the kernel's queue, which overflows if it fills, and an overflow triggers a full reload once output resumes. Memory therefore stays bounded and no change is lost.

The helper never writes anything else to standard output; logs go to standard error.

## The app's client

The app talks to the helper through `refract-client`, which starts the helper, numbers each request and hands each response back to the caller waiting for it. Events go straight to the interface. Each line of the helper's standard error is forwarded to the app's own standard error, and the last few lines are kept to explain a crash.

### The log file

The app's standard error shows in the terminal when the app is started from one. A release build has no console, so at startup, when the process has no standard error handle, `app/src-tauri/src/logfile.rs` points it at a pipe. Rust's standard library looks the handle up on every write, so every `eprintln!` in the app and its crates, the helper's forwarded lines included, goes into the pipe (an unreadable line from the helper is logged only by its first 200 bytes, since it may hold reply text or paths); so does the standard error of any child process that inherits it. One thread reads the pipe, splits it into lines of at most 16 KiB and stamps each with the UTC time to the millisecond. A failed read is tried again after 50 ms; after 20 failures in a row the thread stops and empties the standard error handle, so later writes are dropped rather than left waiting on a pipe nobody empties. Another appends them to `refract.log` in the app's log folder (`%LOCALAPPDATA%\io.github.maddoxrjohnson.refract\logs`), so a slow disk never holds up a writer: if that thread falls 1024 lines behind, new lines are dropped and the count is written once the queue empties. Lines the file cannot take are dropped too, and the file is opened again for the next one.

Each start writes a line with the app's version. A log over 1 MiB at startup, or one about to pass 2 MiB while running, is renamed to `refract.old.log`, replacing the previous one. If the rename fails (another program holds the log open without allowing it), the log may grow to twice the limit before one more try; if that fails too, the log is emptied instead. On exit, and after a panic's message, the app waits up to 200 ms for the lines already written to reach the file: it writes a marker line that the reading thread swallows, and waits for it and every line before it to be written. The settings panel's "Open log folder" opens the folder in Explorer.

If the helper exits, every request still waiting for an answer fails at once as disconnected, and the client starts the helper again after a delay. The delay starts at one second and doubles with each failure in a row, up to 30 seconds. A helper that ran for at least a minute before exiting counts as healthy, so the next delay is back to one second. Failing to start the helper at all, for example when `wsl.exe` is missing, counts as a failure too. While the helper is down, requests fail straight away instead of queueing, and the interface shows that it is restarting, when it will try again and why it stopped. A request that times out is forgotten, so an answer that arrives later is dropped. Shutting down sends `shutdown`, closes the helper's input and kills it if it has not exited after a grace period. Nothing is restarted after that.

The interface does not get a free line to the helper. Its `agent_request` command accepts only `ping`, `listRepos` and `getRepo`. Everything else goes through a command that checks what it sends: `launch`, `clone_repo`, `add_repo`, `hook_info`, `install_hook`, `uninstall_hook`, `usage` and the `migrate_*` commands.

## Session state

Claude Code writes one JSON-lines file per conversation under `~/.claude/projects/<encoded working directory>/<session id>.jsonl`. For each repo, Refract lists the sessions whose working directory is that repo, then classifies each one:

| State | How it is known |
|---|---|
| Live | A `claude` process is running with that session id, or in that working directory (read from `/proc`) |
| Working / waiting on you / idle | Live, plus the [status hook's](#the-status-hook) most recent report. Without the hook a live session has no detail |
| Focusable | Live, the process running it is known for certain, and its environment (`/proc/<pid>/environ`) has `REFRACT_FOLDER` equal to the repo path, so it runs in a terminal of the Refract window on exactly this folder, which the picker can go to. Sent as `focusable` on each session. The process is known when it names the session id, or when it is the only process in the repo naming none; a session matched to one of several such processes is live but not focusable |
| Finished | Not live, and the last entry is a complete assistant reply (or a user interrupt) |
| Interrupted | Not live, and the last entry is an unanswered user message, a tool call without a result, tool results the assistant never answered, or an API error |

Only the start and the end of each file are read, and results are cached until inotify reports a change.

A session that isn't running can be worth resuming even when it finished. Each session is sent with a `resumeReason`, and the first that applies wins:

| Reason | Which sessions | When |
|---|---|---|
| `interrupted` | any | It is interrupted |
| `askedQuestion` | the last conversation | The transcript ends on a reply that asks something: with code removed, one of its last 8 non-empty lines has a `?` (or `？`) that ends a sentence |
| `uncommittedChanges` | the last conversation | The repo has uncommitted changes and the session was active after HEAD's committer time, or there are no commits |
| `unpushedCommits` | the last conversation | The branch is ahead of its upstream, or has no upstream but an origin and commits no `origin/*` branch has, and the session was active after HEAD's committer time. A gone upstream or a detached HEAD has nothing to push |

The last conversation is the session with the newest transcript timestamp, skipping transcripts with no conversation in them. When it is still running, no session gets the last three reasons: the running agent's own edits aren't another session's unfinished work. Running and finished sessions get `null`. The chipped corner still means only `interrupted`.

## The interface

`app/src/App.tsx` holds the state; the pieces are small and take plain props.

### Where the data comes from

- **Local repos** come from `listRepos` plus `repoChanged`, `repoRemoved` and `loadComplete` events, combined by a reducer (`repoState.ts`). Snapshots carry no version, so an event that arrives while a `listRepos` request is in flight is taken as newer than its answer. The list is fetched again whenever the helper enters the running state, and on every show of the window.
- **Remote repos** come from `listRemotes` and are merged with the local ones (`projects.ts`). A remote repo whose host, owner and name match a local repo's `origin` (ignoring case) is the same gem. One with no local clone becomes a stippled stone keyed `remote:<host>/<owner>/<name>`, with no sessions and a mass capped at 0.45 so uncloned repos sit towards the edges. Forks and archived repos that are not cloned stay out unless settings turn them on; a cloned fork always shows. Hidden keys apply to every gem.
- **Stone appearance** comes from `repoStatus.ts`: uncommitted changes (red ring on the sheet), an interrupted session (chipped corner), a live session working (rosette turns) or waiting (red line-work, a knock and red rings). Light or dark comes from the `theme` setting through `theme.ts`, which follows the Windows app mode for System, writes `data-theme` on `<html>` for an override, remembers the choice in local storage so it shows before the settings load, and matches the native title bar.

### Navigation

Finding and choosing a project is a pure state machine in `ui/navigation.ts`. Input events go in; a new state and a list of effects (launch, clone) come out, and `App.tsx` runs the effects. The state is `focused` (the gem with a label), `zoomed` (the gem the camera is on, with the detail panel open), `filter`, `picker` and a pending `follow` after a clone.

| Input | Result |
|---|---|
| Pointer near a gem | That gem is focused (the scene works out the nearest gem within about 130 px) |
| Click on a gem | Focus and zoom it, closing the picker |
| Click elsewhere | Closes the picker, otherwise zooms out |
| Printable key | Added to the filter. Gems whose name does not contain it dim; focus moves to the best match (names that start with it, then the heaviest). A zoom on a gem that no longer matches ends |
| Backspace | Removes the last filter character |
| Arrow keys | Move focus (and the zoom, when zoomed) to the nearest matching gem in that direction; sideways distance counts double |
| Enter | Not zoomed: zoom the focused gem. Zoomed: launch |
| Escape | Closes the picker; otherwise zooms out; otherwise clears the filter |

Launching a gem depends on what it is:
- Not cloned: the `clone` effect, which clones and then launches.
- The helper's preselect is `noPicker` (clean repo, no session with a resume reason, no focusable session): launch a new agent at once.
- Otherwise open the picker.

A new agent in a repo with uncommitted changes starts with a short opening prompt asking Claude to review `git diff`. The picker lists "Go to the running agent" for each focusable live session, then the sessions with a resume reason, each newest first, then New agent. A resume option names its reason: "Resume: stopped mid-task", "Resume: asked you a question", "Resume: uncommitted changes" or "Resume: unpushed commits". It highlights the option the helper preselected (`focus` or `resume`, each with a `sessionId`), or else New agent. The helper preselects resume when the newest session that isn't running has a reason and ended less than 2 days ago; its size doesn't matter. Going to a running agent launches with mode `{"kind": "focus", "sessionId": …}`, which starts nothing and shows that agent's terminal ([launch.md](launch.md)). Keys with Ctrl, Meta or Alt are ignored, a held Enter does not repeat, and the settings dialog takes all keys while it is open.

### Panels and widgets

- **Detail panel.** Branch, upstream, changed files, last commit, last agent session, and for repos with a GitHub or GitLab remote the open pull or merge requests and CI state. Actions: launch (or Clone and launch), Go to agent (when a session is focusable), Resume, New agent, Open folder, Open on the host, Hide. Remote details are fetched when a gem is zoomed, again 15 minutes after they were fetched while the same gem stays zoomed and the window is shown, and on Refresh.
- **Picker, filter bar, focus label.** Presentation for the navigation state above.
- **Settings.** GitLab address and token, GitHub token (the GitHub CLI login is used otherwise), show forks, show archived, refresh remotes, the hidden list with Unhide, adding a folder, the cost estimate (on or off, and its price table with Add row, remove, Reset to defaults and Save prices), the status hook's install and remove, and Open log folder. Changes are saved one after another; if saving fails the old values come back. Price table edits stay a draft until Save prices, which checks every row first and marks the bad cells (`aria-invalid`, described by the message); the Windows side checks them again (a match of 1 to 100 characters, prices from 0 to 10,000, at most 50 rows) and refuses the save otherwise. Edits made while a save is under way are kept. A row that an earlier, shorter match always wins gets a warning under the table. A price table in `settings.json` that is the wrong shape or fails those checks is replaced by the defaults when the file is read, leaving every other setting as it was.
- **Status hook offer.** Once the repos are in, a small card offers to install the hook, with "Show what changes" revealing the diff of `settings.json`. "Not now" is remembered.
- **Widgets.** System load at the top right, Claude token use at the bottom right, with the optional cost estimate (hovering or focusing lists the last 7 days by repo) and, when Claude Code's status line relays them, the 5-hour and weekly plan limits ([plan limits](#plan-limits)). Both fade while the detail panel covers their corner.
- **Launch toast.** Shows cloning, setup and opening steps, and failures until dismissed. One launch or clone runs at a time.

### Where state is kept

| State | Where |
|---|---|
| Graphics setting (`refract.quality`), hook offer dismissed (`refract.hookOfferDismissed`) | The webview's `localStorage`; the interface works without it |
| GitLab address, show forks, show archived, hidden gems, theme (`theme`: system, light or dark; anything else reads as system), cost estimate and its prices (`costEstimate`; a file without it gets the estimate off and the shipped prices) | `settings.json` in the app's config directory on Windows |
| GitHub and GitLab tokens | Windows Credential Manager |
| Extra folders, roots, hidden local paths, where the plan limits file is | `~/.config/refract/agent.toml` in WSL (optional; `addRepo` appends to `extra`). Keys: `roots`, `extra`, `hidden`, `depth`, `limits_file`; an unknown key makes the helper ignore the file and say so on stderr |

### The web demo

`DemoBackend` implements the whole `Backend` with invented data: 13 local repos covering every gem state, remote repos (some matching, some not cloned, a fork and an archived one), pull request and CI numbers, a pretend hook diff with the real installer's commands and matchers, and usage and system numbers that drift slowly, with usage split across invented shares of Opus, Sonnet and Haiku so the cost estimate, on in the demo, has something to price, and invented plan limits (about 37% of the 5-hour window and 58% of the week, resetting a couple of hours and a few days after the page loads). Settings, hook state and clones live in memory and reset on reload. A launch walks through the progress steps and then tells `DemoTerminal`, which types out the `claude` command and an invented reply in a fake terminal. GitHub links open this project's page rather than the invented owner's. The demo has no GPU counters, so its automatic quality uses frame times only.

`main.tsx` picks the demo backend when built with `--mode demo` and whenever the page is not inside Tauri, which is what `npm run dev` shows in a plain browser. `npm run build:demo` builds it into `app/dist-demo` for GitHub Pages. `app/e2e/demo.spec.ts` runs ten browser tests against that build (see [Build, CI and release](#build-ci-and-release)).

## The scene

The scene is React Three Fiber code in `app/src/scene`. Its look, Banknote, is described in the spec (§3.2) and [decision 0017](decisions/0017-banknote-scene.md).

### Layout and camera

Stones lie on a paper sheet (the y = 0 plane). `layout.ts` places them on a golden-angle spiral from the centre outwards, heaviest first, then relaxes the spiral: rounds of pull towards the centre and pairwise push-apart until no two stones' discs (1.22 times their size) overlap. Repos are sorted by mass and then path first, so a stone keeps its place whatever order repos arrive in. Stones shrink once there are more than about 14. The cut (one of six, in `cuts.ts`) is picked from a hash of the path, so a repo also keeps its shape.

The camera looks down from 56° above the sheet with a 28° field of view, at a distance that fits the layout in both directions. The pointer tilts the view a few degrees. Zooming flies in and tilts down to about 35°, placing the stone left of centre so the panel fits on the right and, in windows less than about 1300 px tall, raising it so the launch picker fits below it. The focus label and the marks are moved by the frame loop with DOM transforms, so React does not re-render per frame.

### Drawing the engraving

`banknote.ts` holds the palettes and the shaders, ported from `docs/mockups/gem-studies.html`. Each stone is a convex mesh with flat normals and a barycentric attribute (`prepare()` in `cuts.ts`) that masks the edges between coplanar triangles, so only real facet edges are drawn. The stone shader draws wavy lines in screen space, one direction per facet, their weight set by how much the facet faces away from the light; in dark mode the lines mark the lit facets instead, like scratchboard. Line spacing is in CSS pixels, so the engraving keeps its weight at every resolution. A remote-only stone swaps lines for stipple; a waiting stone draws its lines and facet edges in red, with bold edges and a band of extra weight running up it, knocks (a hop and a wobble every 1.6 s) and sends red rings across the sheet. A stopped session's stone has a corner cut off its geometry. The rosette, the red ring and the waiting rings are transparent decals on the sheet, and the splinters from the opening are one instanced mesh.

All materials share one set of palette uniforms, so switching between light and dark changes their values without rebuilding anything or replaying the opening. Colours are sRGB values converted to linear by three.js and back by an `OutputPass`, so the screen shows them exactly; an e2e test checks the paper and rim colours in both modes.

The scene renders through three's own `EffectComposer` with only a `RenderPass` and an `OutputPass`, into a half-float target multisampled on Medium and High. There is no post-processing beyond that.

### Drawing only on change

The canvas uses R3F's `demand` frame loop: a frame is drawn only when something asks for one. Frames are requested by a change of props (repos, filter, focus, zoom, level), a resize, or pointer movement. After each drawn frame the scene asks `ticker.ts` for the next one only if something is still moving: ambient motion on, the camera or a gem easing towards its goal, or the pointer parallax settling. When everything has settled the loop simply stops and costs nothing.

The ticker (`createTicker`) invalidates the canvas `1000 / fps` ms after the previous frame started, measured from the frame's start and a few milliseconds early so the animation frame that follows lands on the right display refresh. That is what holds the tier's frame cap, and pointer movement goes through it too.

### Quality levels

| | Low | Medium | High |
|---|---|---|---|
| Frame cap | 30 fps | 30 fps | 60 fps |
| Largest pixel ratio | 1× | 1.5× | 2× |
| Multisampling | off | 4× | 4× |
| Splinters | 40 | 90 | 90 |
| Stones turn, rosettes spin, waiting stones knock and send out rings | no | yes | yes |

The look is the same at every level. Low has no ambient motion, so it draws only when something changes. The tiers are in `quality/tiers.ts`. The graphics control in the corner offers Auto, Low, Medium and High; a fixed choice bypasses the controller.

### The adaptive controller

`quality/controller.ts` picks the level in Auto. It is a pure object that is handed the time with every sample, so it is tested without a clock. It starts at Medium, where the first GPU readings can drop it to Low at once, and has two inputs.

- **Frame intervals.** The scene reports the time between two consecutive drawn frames, but only when the earlier frame asked for the later one, so idle gaps never count. Frame CPU time says nothing about the GPU in a WebView, because GL calls queue in another process. Late frames mean the level cannot hold its cap.
- **GPU counters.** Every 2 seconds while the window is visible, the Windows side reports the busiest GPU engine's use and Refract's own share of it. "Others" is the difference.

| Move | When |
|---|---|
| Straight to Low | Others use more than 60% of the GPU, a game or a render |
| Down one level | Over 3 seconds, the median frame interval is more than 1.25 times the level's budget. That level then stays out of reach for 30 seconds |
| Up one level | At least 10 seconds at this level; the median interval is within 1.1 times the budget over frames that cover at least half the window; at least three GPU readings in that window show others under 60% and Refract itself under 35%; and the GPU has not been busy for the last 10 seconds |
| Low to Medium on GPU readings alone | Low draws only on change, so after a game forces it there may be no frames to judge. If Low was forced by the GPU, once others stay under 45% for a wait that starts at 10 seconds, the level returns to Medium |

The gaps between the thresholds are the hysteresis: step down at 1.25 times the budget but up only at 1.1, drop at 60% but recover at 45%. If the GPU turns busy again within a minute of a recovery, the wait before the next one doubles, up to two minutes, so a game hovering near the threshold does not rebuild the effects every few seconds. Calm for a minute resets the wait. Samples live in fixed-size ring buffers, so a frame allocates nothing.

### Loading and failure

The scene, which is mostly three.js, is a separate chunk loaded with `React.lazy`, so the overlays and data requests start without waiting for it. A chunk that fails to load is asked for once more. Until it arrives the user sees the empty page in the paper colour. If it still fails or throws, `SceneBoundary` shows a message and the overlays stay usable. If WebGL cannot start, the canvas shows its own message. The scene also follows the system's reduced-motion setting: no opening burst, no turning or spinning, waiting stones keep their red line-work but neither knock nor send out rings, and camera moves still ease but quickly.

## Remote repos

The Windows side lists repos on GitHub and GitLab and fetches pull request and CI state (`app/src-tauri/src/remote`). The interface asks through the `remote_list` and `remote_details` commands.

| | GitHub | GitLab |
|---|---|---|
| Asked | When a stored token or a GitHub CLI login exists | When an address is set in settings and a token is stored |
| Listing | `GET /user/repos`: repos you own, collaborate on or reach through an organization; 100 a page, up to 50 pages | `GET /api/v4/projects?membership=true`; 100 a page, up to 50 pages |
| Details | One GraphQL query: the open pull request count and the check state of the default branch's newest commit | Open merge request count and the newest pipeline on the default branch |
| CI result | Success, failure, pending, or none | Same |

### Tokens

| Provider | Order tried | Credential Manager target |
|---|---|---|
| GitHub | A token stored by Refract, then `gh.exe auth token` on Windows, then `gh auth token` inside the helper's WSL distro (through a login shell, so `gh` is found where the user's profile puts it) | `refract:github` |
| GitLab | The stored token only | `refract:gitlab` |

Stored tokens are generic credentials under the Windows user, written with `CredWriteW`. A token typed into settings is trimmed and checked (no spaces or control characters, at most 1,024 bytes), goes straight to Rust and is never held in React state. The interface learns only where a token comes from (`ghCli`, `stored`, `missing`), never the token. A token from `gh` is accepted only if it has one of GitHub's token prefixes. Saving or removing a token drops that provider's cached answers.

### Requests

All requests go through one small blocking layer (`remote/http.rs`): HTTPS only, the system's TLS roots, a 15 second limit from connect to the end of the body, a 16 MiB cap on a body, a bearer token in the `Authorization` header. **Redirects are not followed**, so a token can never be sent on to another host; a 3xx comes back as an answer, and GitLab's is reported as "the address moved to … update it in settings". The `Link` header's next page is followed only if it is on `api.github.com`. Failures are worded for a person: a refused token, a rate limit with the wait if known, a timeout, an address that does not answer. The GitLab address is stored as `https://host[:port]` with no user name, query or fragment.

### Caching

Every answer is kept for 15 minutes, per source for lists and per repo for details. A failed fetch, or a source with no token, is tried again after 60 seconds, and a failure keeps the last good data and reports its error next to it. A refresh from the interface skips the cache. Each cache slot has its own lock, so a slow repo holds up only requests for that same repo, and they then share its answer. Lists for a GitLab address no longer in settings are dropped.

## Cloning and adding repos

Cloning happens in the helper (`crates/refract-agent/src/repos.rs`), where the repos live.

`cloneRepo {url, name}`:
- The request is refused unless the URL is `https://` with no user name or token in it, and the name is a plain folder name.
- The destination is `~/projects/<name>`. If that folder is already a repo with a commit whose `origin` is the same URL, the helper answers with its path and clones nothing, so retrying after a failed launch is safe. Anything else in the way is an error.
- git clones into a hidden sibling folder, and the result is renamed into place, so a failed or killed clone never leaves a half-written folder.
- git never prompts (`GIT_TERMINAL_PROMPT=0`), and Refract does not hand it a token. A private repo clones only if git in WSL already has credentials for it.
- A clone gets ten minutes. After that git and the processes it started are sent SIGTERM and then SIGKILL as a group, and what was written is removed.
- Clones run one at a time on their own thread, so the helper keeps answering other requests.

`addRepo {path}` takes any folder inside a working tree. Unless a configured root already covers the repo, it is added to `extra` in `agent.toml` (the rest of the file, comments included, is left as it was). If the user had hidden it there, that is undone. The repo is then reported with `repoChanged`: before the response once the first load is done, otherwise by the load. A finished clone is added the same way.

In the interface, Clone and launch shows a "cloning" step, then moves the zoom and focus to the clone once it appears as a local repo (its key changes from `remote:…` to a path), then launches a new agent.

## The status hook

The hook is optional. Without it Refract still shows interrupted sessions and which sessions have a running process; with it, it also knows when an agent is working and when it is waiting for you.

### Events and statuses

Refract adds one small script to Claude Code's hook settings. The script writes a one-line status file for a session when one of these events fires:

| Claude Code event | Only for | Status written | What the stone shows |
|---|---|---|---|
| `SessionStart` | `startup`, `resume`, `clear` | finished | Nothing special (idle) |
| `UserPromptSubmit` | | working | Rosette turns |
| `PostToolUse` | any tool | working | Rosette turns (marks the turn working again after a permission prompt or a question is answered) |
| `Notification` | `permission_prompt`, `elicitation_dialog` | waitingOnUser | Red line-work, knocking, red rings |
| `PreToolUse` | `AskUserQuestion`, `ExitPlanMode` | waitingOnUser | Red line-work, knocking, red rings |
| `Stop` | | finished | Nothing special (idle) |
| `SessionEnd` | | ended | Nothing special |

`Notification` is limited to permission prompts and input requests because its `idle_prompt`, sent after a minute at the prompt, would wrongly say the agent is blocked. A multiple-choice question (`AskUserQuestion`) and a plan waiting for approval (`ExitPlanMode`) send no `Notification`, so the `PreToolUse` of those two tools marks the session waiting; its `PostToolUse`, once you answer, marks it working again. `SessionStart` leaves out `compact`, which happens mid-turn.

A status counts only for a session whose `claude` process is running, and the process decides liveness: a session saying `ended` that is still running is live with no detail. Pressing Esc or denying a permission ends a turn with no `Stop` event, so a live session whose transcript ends in a user interrupt written after a working or waiting status is shown as idle. Statuses are ignored altogether unless `settings.json` holds Refract's entries. Claude Code reads its hook settings when a session starts, so a session already running when the hook is installed may not report.

### The command and why it always exits 0

Each entry is this command, with the paths filled in:

```
[ -f '<script>' ] && sh '<script>' <status> '<status dir>' || cat >/dev/null; exit 0
```

Claude Code treats exit code 2 as "block this prompt or stop", and `sh` exits 2 when the script file is missing. A hook that could block a session would be worse than no hook, so the command checks the script exists, drains its input if not, and exits 0 either way. The script itself reads at most 1 MiB of its input and drains the rest (Claude never writes into a closed pipe), prints nothing, ignores anything it does not understand and ends with `exit 0`. It writes the file through a temporary file and a rename, so a reader never sees half of one. It never talks to a model, so it uses no tokens.

### Installing and restoring

The edits are pure functions over the bytes of `settings.json` in `refract-core`, applied on disk by the helper (`crates/refract-agent/src/hook.rs`).

- **Install** keeps every other key and every other hook, in order, and adds one group per event. Refract's entries are recognised by the script's path in the command. An install that is already complete changes nothing; a partial or outdated one is replaced. A `hooks` value of the wrong shape is refused rather than overwritten.
- **Updating.** When a newer Refract registers a different event set (or command form, status directory or script), an existing install is no longer complete. `hookInfo` then says `installed: false, outdated: true`, and Settings shows the hook as "Needs update" with Update and Remove buttons and the diff. Nothing changes until Update is pressed; Update runs the same install, which drops the old entries and adds the current ones. Like any hook change, it reaches only Claude Code sessions started after it.
- **Backup.** Before the first change the helper saves the file's bytes (or a marker if there was no file) in the state directory. A second install, including an update, never overwrites the backup, so uninstalling after an update still restores the bytes from before the first install.
- **Safe writes.** The new file is written next to the old one and renamed over it, keeping its mode. A symlinked `settings.json` has its target edited. If something else (Claude Code) changes the file while Refract is working, the edit is redone once on the new bytes and a second change is an error. Nothing is written if the file is not valid JSON.
- **Uninstall** removes Refract's entries, and any group or list left empty by that. If what is left equals the backup as JSON (empty event lists aside), the backup's bytes are written back verbatim, so install then uninstall is byte-for-byte identical, or the file is deleted if there was none before. If anything else was changed in the meantime, only Refract's entries are removed, everything else is kept, and the file is written in Claude Code's format (two-space indent, trailing newline). The backup, the script and the status files are then deleted.

The diff that "Show what changes" displays is computed by the helper with a small unified-diff function, from the same install transform, so it shows exactly what installing would write.

### Where the files are

| File | Default location | Overridden by |
|---|---|---|
| Claude Code settings | `~/.claude/settings.json` | `CLAUDE_CONFIG_DIR` |
| The script | `~/.local/share/refract/status-hook.sh` | `XDG_DATA_HOME` |
| Status files | `~/.local/state/refract/sessions/<session id>.json` | `XDG_STATE_HOME` |
| Backup | `~/.local/state/refract/settings.backup.json` (or `settings.backup.missing`) | `XDG_STATE_HOME` |

A relative `XDG_*` value is ignored. The helper watches the status directory with inotify and updates the affected session when a file is written or removed. Status files do not pile up: at startup, then hourly, the helper deletes `ended` files and the script's leftover temporary files older than 10 minutes. An ended session classifies the same as one with no report, so that changes nothing the interface shows. Full reads of the directory (startup, reload, install) also delete any status file untouched for seven days; a live session that quiet then shows as unknown until its next event.

## Refactor

**Refactor** on a zoomed repo (or Alt+R) has an agent make up to three behaviour-preserving refactors in a separate copy of the repo, and leaves the result for you to review, merge or discard. The helper does the work (`crates/refract-agent/src/refactor.rs`); the prompt, the result file's format and the naming rules are pure code in `crates/refract-core/src/refactor.rs`, so the helper and its tests share one wording. [Decision 0018](decisions/0018-refactor-in-a-worktree.md) records why it works this way.

### The flow

1. **Start.** The dialog shows the commit the refactor starts from and how many uncommitted files are left out, takes an optional "Focus on…" line (one line, at most 500 characters) and says alerts need the status hook when it is not installed. `startRefactor` refuses a detached HEAD ("Check out a branch first."), a repo with no commits, and a repo that already has a refactor. Remote-only repos have no button: the panel says to clone first.
2. **Worktree.** The helper runs `git worktree add -b refactor/<yyyymmdd-hhmm> ~/projects/.refract-worktrees/<repo>-<yyyymmdd-hhmm> <HEAD>` (local time; `-2`, `-3` if either name is taken). The folder is inside `~/projects`, which VS Code already trusts, and the repo search skips dot directories, so the worktree never becomes a stone. The repo may be busy: a running agent and uncommitted files are fine, since the worktree starts from the last commit and touches nothing else. A failed start removes the worktree and branch it just made.
3. **Agent.** A launch request opens the worktree in its own VS Code window and runs `claude --session-id <uuid> --permission-mode plan -- "<prompt>"` ([launch.md](launch.md)). Claude's default model is used. The prompt tells the agent to set up the project, run the repo's test, lint and type-check commands for a baseline, propose a plan of up to three refactors and wait for approval, then make each one as its own commit, re-run the checks, revert a refactor that breaks a check that passed in the baseline, never push or switch branches, and write the result file.
4. **You approve the plan** in the terminal. With the hook installed, the stone turns red while the plan waits (`ExitPlanMode` marks the session waiting, see [Events and statuses](#events-and-statuses)).
5. **Result.** When the agent is done the panel lists the commits, files changed and lines added and removed, the checks before and after, and the agent's summary. **Review in VS Code** raises the worktree's window; there is no diff viewer in Refract. **Merge into `<branch>`** and **Discard** end the refactor.

One refactor per repo. While one exists the button reads **Go to the refactor**.

### States

A refactor's state is worked out by the helper from its record, the agent's process, transcript and hook status, the branch's commits and the result file, and sent in the repo's snapshot as `refactor` (`RefactorInfo`: `state`, `worktree`, `folderUri`, `branch`, `targetBranch`, `baseCommit`, `sessionId`, `createdAt`, `live`, `interrupted`, `commits`, `filesChanged`, `insertions`, `deletions`, `checks`, `summary`, `blocked`). The first row that applies wins.

| State | How it is known | Mark on the stone | Panel actions |
|---|---|---|---|
| `working` | The agent's process runs and its turn is not over (hook `working`, or no hook status) | Turning | Go to the refactor |
| `needsYou` | The agent runs and the hook says `waitingOnUser` (plan approval or a question); or it is open and idle with no commits and no finished result | Red | Go to the refactor |
| `mergeBlocked` | The last merge stopped and nothing has moved since | Red | Merge again, Fix with Claude (conflict only), Go to the refactor (when the agent is open, or the worktree is busy or dirty), Discard |
| `ready` | At least one commit on the branch that the target lacks, and the agent is stopped or idle | A "ready" seal | Merge into `<branch>`, Review in VS Code, Resume refactor (when `interrupted`), Discard |
| `starting` | No agent, no transcript, launched under 2 minutes ago | Turning | none ("Opening VS Code…") |
| `nothingToDo` | No commits and a valid result file with no refactors; or merged, but the worktree could not be removed | Plain | Remove (and Go to the refactor while the agent is open) |
| `interrupted` | No agent and nothing committed: a transcript that did not finish, no result file, or no transcript after the 2 minutes | Red | Resume refactor, Discard |

Where the code differs from the first spec:
- An agent that is open but idle, its turn over, counts as stopped. With the hook, idle means the hook said the turn finished. Without it, the transcript ends with a finished turn and has been quiet for 20 seconds. The state is then `ready` (or `nothingToDo`) while `live` stays true and the terminal is still open, and the panel adds that Claude is still open. A merge goes ahead with an open idle agent (and blocks if one started working while it ran), but Discard, Resume refactor and Fix with Claude need the terminal closed.
- Any other `claude` process in the worktree counts as busy.
- A merged refactor whose worktree git refused to remove stays as `nothingToDo` with the summary "Merged; the worktree could not be removed: …"; only Remove is offered, and the helper does not adopt it again.
- The result file is optional. Without a valid one the panel shows the commit list and diffstat from git and says "No check results reported".

### Merge and the safety rules

Refract never writes to the working copy except in the last step of a merge. Every step either leaves the repo, the worktree and the branch as they were, or is the one write. `mergeRefactor` runs on a worker thread, because git can be slow, and takes these steps in order. Each check that fails ends the merge as `blocked` with a `reason` and the `files` concerned (paths relative to the repo unless noted), and nothing changes.

| Check | `reason` |
|---|---|
| The refactor's agent is working or waiting on you (an open idle one is fine) | `live` |
| The target branch is gone | `targetMissing` |
| The target no longer contains the commit the refactor started from (it was rewound or force-pushed over) | `targetRewound` |
| The worktree is not on its branch, or a rebase, merge, cherry-pick or revert is under way there | `worktreeBusy` |
| The worktree has uncommitted changes to tracked files (`files`: those) | `worktreeDirty` |
| Another worktree has the target checked out, or is rebasing it (`files`: those worktrees' absolute paths) | `targetBusy` |
| The target is checked out in the repo and an uncommitted file there is one the refactor changed, renames counted (`files`: the clashing files) | `dirtyOverlap` |
| Rebasing the branch onto the target hits a conflict (`files`: the conflicted files) | `conflict` |

1. **Rebase** (only when the target moved on): `git rebase <target>` in the worktree, with the user's rebase settings overridden (no autostash, autosquash, fork point or `updateRefs`, merges flattened). On conflict the rebase is aborted, so the worktree is as it was, and the panel offers **Fix with Claude**: `resumeRefactor {fix: true}` resumes the agent with a prompt to rebase onto the target and resolve the conflicts keeping both sides' intent, re-run the checks, keep one commit per refactor and update the result file.
2. **Move the target.** Target checked out in the repo: `git merge --ff-only <branch>`, with `merge.autoStash` and `merge.ff` overridden. Git itself also refuses if an uncommitted file would be overwritten. Target not checked out: `git fetch . <branch>:<target>`, which updates the ref only if it is a fast-forward and no worktree has the target. The helper then checks that the target is at the branch's last commit; if not, it keeps everything and reports an error.
3. **Clean up.** `git worktree remove` without `--force`, then `git branch -d`; when `-d` refuses because the target is not the current branch, the helper deletes the branch with `update-ref -d` only if it is merged into the target and still where it was merged. Whatever git refuses stays, and the reply lists it in `kept`.

Other rules:
- **Discard** (after "Discard N commits?" in the panel) runs `git worktree remove --force` and `git branch -D`, and only for a branch starting with `refactor/`. It is refused while any agent runs in the worktree, idle or not.
- **Path check before anything is deleted.** The worktree's real path must lie under `~/projects/.refract-worktrees/` (not be that folder) and be a linked worktree in that repo's `git worktree list`, or the helper refuses. A record that points elsewhere cannot make it delete a folder.
- **Nothing is pushed**, and the prompt tells the agent not to.
- **One action at a time per repo.** A start, merge or discard marks the repo busy; another request for it is refused with "busy with another action" until it ends.
- The git commands work on git 2.25 and later; newer settings are passed as `-c` overrides, which older git ignores.

A merged refactor disappears: record, worktree and branch go, and the panel says "Merged N commits" until it closes.

### Records and reconciling

The helper keeps one JSON record per refactor in `~/.local/state/refract/refactors/<uuid>.json` (`XDG_STATE_HOME` applies), written through a temporary file and a rename. It holds the repo path, worktree, branch, base commit, target branch (the branch checked out when Refactor was pressed), session id, launch request id, focus line, creation and last launch times, and the merge block, with the branch commit it applies to. A new commit on the branch clears the block.

After the first load, `reconcile` brings records in line with git, and deletes nothing:
- A record whose repo is gone, or whose worktree is not in `git worktree list` or not on disk, is dropped.
- A linked worktree of a known repo that is on a `refactor/` branch under the worktrees folder and has no record is adopted: the target is the repo's current branch, the base is the merge base, the session is the newest transcript for that folder (none leaves it `interrupted`).

`resumeRefactor` without `fix` resumes the recorded session with "Continue the refactor from where it stopped." If that session never wrote a transcript, it starts a new plan-mode session with the original prompt and records the new session id, after deleting the old launch request if nothing had picked it up.

### Polling and refreshing

The refactor's state belongs to the parent repo's snapshot, so the stone's mark and the panel follow `repoChanged`; the helper sends it only when the `RefactorInfo` changed. A refactor's session is never attached to a stone: its working directory is the worktree, which is not a repo. The state is worked out again:
- when the repo is refreshed and when a status-hook file for one of the refactor's live sessions changes;
- every 3 seconds while a refactor is starting (within 2 minutes of its launch), working, or `needsYou` by the hook;
- every 30 seconds for all of them otherwise, to catch a closed terminal or a commit.

Tests shorten the two waits with `REFRACT_REFACTOR_GRACE_SECS` (the 2 minutes) and `REFRACT_REFACTOR_QUIET_SECS` (the 20 seconds).

### The result file

The agent writes `refract-refactor.json` into the worktree's own git directory (`git rev-parse --absolute-git-dir`, whose path the prompt gives in full), so it is outside the working tree and never committed:

```json
{
  "checks": [
    {"command": "cargo test", "before": "pass", "after": "pass"},
    {"command": "cargo clippy -- -D warnings", "before": "fail", "after": "fail"}
  ],
  "refactors": [{"title": "Split parse.rs into lexer and parser", "commit": "a1b2c3d"}],
  "summary": "Split the parser and removed two unused helpers; all checks as before."
}
```

`before` and `after` are `pass`, `fail` or `notRun`. `checks` and `refactors` are required; `summary` and each `commit` are optional; unknown fields are ignored. A file over 256 KiB, not JSON, missing a required key or with another outcome string is invalid and treated as absent. `refactors: []` with no commits means the agent found nothing worth doing (`nothingToDo`). The commit list shown in the panel comes from git, not from this file.

### Alerts

There are two: the plan is ready (the refactor enters `needsYou` for the first time) and the refactor is ready (it enters `ready`). Both need the status hook to know when the agent waits. The Windows app decides each from the helper's `repoChanged` events (`app/src-tauri/src/alerts.rs`), so a hidden window, whose WebView2 is suspended, still gets them. How it is shown follows the window:
- window hidden, minimized or not focused (behind VS Code, say): a Windows toast;
- window in front (shown and focused) and the repo not zoomed: a notice in the interface (the `refactor://notice` event);
- window in front and the repo zoomed: nothing, since the panel shows it.

Each alert comes once per refactor, which the app tells apart by worktree and creation time (a snapshot that briefly lacks the refactor, as in the helper's first load, forgets nothing). A refactor first seen by the app is not announced (it may have been there before the app started). One first seen in any state but `starting` counts as past its plan, so a later `needsYou` is taken for a question and not announced. The "ready" alert is re-armed when the refactor is interrupted and when it is resumed from Refract.

### In the interface

The panel's Refactor section shows the button or the state's text and actions from the table above; `RefactorDialog` takes the focus line. Alt+R opens the dialog on the zoomed repo, or the refactor's window when it has one. The mark is a second engraved ring on the sheet: turning while the agent works, red for the three states that need you, a still seal when there is something to merge, plain when there was nothing. The web demo simulates the whole run (start, plan, approval, commits, merge, discard).

## Usage and system stats

### Usage totals

The usage widget adds up the tokens in Claude Code's own session files. Nothing is sent anywhere, and no model is called. `refract-core/src/usage.rs` does the counting and `refract-agent/src/usage.rs` keeps it up to date.

**What counts.** Every `assistant` entry with a `message.usage` and a timestamp, in the main transcripts and in subagent transcripts, which are real cost. Input, output, cache read and cache write tokens are kept apart and the widget shows their sum. Entries whose counts are all zero (synthetic error messages, model `<synthetic>`) are dropped.

**By model.** Each record keeps its `message.model` (such as `claude-opus-5-5`) and how much of its cache write had the 1-hour lifetime, from `usage.cache_creation.ephemeral_1h_input_tokens`; the rest counts as 5-minute, the default, including records that do not split their writes. The two are priced differently. A record with no model is kept under the model id `""`, so the split always adds up to the window's total; the app prices it as unknown. Records share one copy of each model id, so the extra memory is a pointer and a number per record.

**Windows.** Today (since local midnight in WSL's time zone), the last 7 days and the last 5 hours, both rolling and ending now. The 7 day total is also split by repo: a transcript belongs to the deepest known repo that contains the directory the session started in, else to that directory itself.

**The result.** `today`, `last7Days` and `last5Hours` are token totals `{input, output, cacheRead, cacheWrite}`. `byRepo` lists `{path, name, last7Days}`, heaviest first. `byModel` holds the three windows again, `{today, last7Days, last5Hours}`, each a list of `{model, input, output, cacheRead, cacheWrite, cacheWrite1h}` heaviest first, where `cacheWrite1h` is the 1-hour part of `cacheWrite`. `byModel` was added after the first release: it is a separate field rather than a list inside each window, so the window totals keep their shape, and an app reading a helper without it treats it as absent. Then come `liveAgents`, `limits` (see [plan limits](#plan-limits); left out when there is no reading), `computedAt` and `ready`.

**De-duplication.** Claude Code writes one response as several lines that repeat the same `message.id`, `requestId` and usage. Records are keyed by `message.id` (else `requestId`) and counted once, within a file and across files, because a session and its fork can both hold the same message.

**Incremental reads.** A separate thread with its own inotify instance watches the `projects` directory, every project directory and the deeper directories that hold recent transcripts. For each transcript written in the last 7 days it keeps the byte offset it has read up to and the records found. A write reads only the new bytes, up to the last complete line; a half-written last line waits for its newline. A file that shrank or was replaced is read from the start. Lines that do not mention `"usage"` are skipped without parsing, once the session's directory is known. Records older than 7 days are dropped, so answering `usage` is only adding up what is in memory.

**Never waiting.** The first scan runs on that thread, and until it finishes the answer is zeros marked `ready: false`, which the widget shows as counting. While the thread is busy reading, `usage` answers the last summary it computed rather than waiting for it.

The widget asks every 2 seconds, one request after the last has answered, only while the window is visible and the helper is running.

**Cost estimate.** Off by default; settings turn it on. Each window's row then shows `≈ $X` between its label and its token count, worked out by `app/src/usage/cost.ts` from that window's `byModel` list and the price table in settings, and a line under the rows says it is an estimate at API list prices. A price row is `{match, input, output, cacheWrite5m, cacheWrite1h, cacheRead}` in US dollars per million tokens; the first row whose `match` is in the model id, ignoring case, prices it. Tokens of models no row matches are left out of the dollars, and the line then says how many there are (over the 7 days), with the model ids in its tooltip and in text a screen reader reads. With the estimate on and a helper too old to send `byModel`, the line says "Cost needs a newer helper" instead. The shipped table, `app/src/usage/defaultPrices.json`, was taken from Anthropic's pricing page on 2026-10-30 (its `takenOn`) and is read by both the interface (Reset to defaults) and the Windows side (settings saved before the estimate existed). It lists longer ids before their prefixes (`opus-5-5` before `opus-5`), which a test checks. **Known limitation:** Claude Haiku 5.5 costs 5 times as much for prompts over 100,000 tokens, and cached tokens read count toward a prompt's size, so Claude Code's Haiku subagents often cross that line. The table holds only the lower rate, which can make Haiku's share several times too low. Pricing each record by its own prompt size would fix it and is not done yet. Regional (`inference_geo`), batch and fast-mode prices are not modelled either.

### Plan limits

The usage widget can show how much of the plan's 5-hour and weekly limits is used, but only as Claude Code itself reports it. Refract never works a percentage out from token counts: with no reading, the rows are not shown.

**Where the numbers come from.** Claude Code passes a JSON object on standard input to the user's status line command each time it refreshes the status line. That object can hold `rate_limits.five_hour` and `rate_limits.seven_day`, each with `used_percentage` and `resets_at`. The helper does not run inside the status line, so a status line script relays them into a small file. By default that is `usage-state.json` in Claude Code's config directory: `$CLAUDE_CONFIG_DIR` when it is set, else `~/.claude`, the same directory whose `projects` the helper reads. Refract only reads that file and never writes it. Another path can be set with `limits_file` in `agent.toml` (`~` is expanded).

**The file.** Any status line can write it; write a temporary file and rename it over the old one so the helper never reads half a file.

```json
{"updated": 1793361600.5,
 "five_hour": {"used": 37.0, "resets_at": 1793370000},
 "seven_day": {"used": 58.0, "resets_at": 1793800000}}
```

| Key | Meaning |
|---|---|
| `updated` | When the status line wrote the file, in Unix seconds (a fraction is allowed). Required |
| `five_hour`, `seven_day` | A window, or `null` or left out when Claude Code gave none |
| `used` | `rate_limits.<window>.used_percentage`: a percentage from 0 to 1000. A window with `null` or no `used` is left out |
| `resets_at` | `rate_limits.<window>.resets_at`: when the window resets, in Unix seconds (milliseconds are refused). A window with `null` or no `resets_at` is left out |

Unknown keys are ignored. Anything else, such as text that is not JSON, a missing `updated`, or a percentage that is negative, not a number or over 1000, counts as no reading. That is never an error. `refract-core/src/limits.rs` parses the file. On each `usage` request the helper opens the file without blocking and checks it through that handle: anything but a regular file of at most 64 KiB gives no reading, so a FIFO in its place is never waited on and a large file is never read. It reads and parses the file again only when its size, modification time or inode has changed (`refract-agent/src/limits.rs`), and then reads at most 64 KiB.

**In the answer.** `limits` is `{fiveHour?, sevenDay?, updatedAt}`, where each window is `{usedPercent, resetsAt}` and times are RFC 3339 strings like `computedAt`. It is left out when there is no reading. It was added after the first release, so an app reading an older helper finds no `limits` and shows no rows, and an older app ignores it.

**In the widget.** Under the token rows (and the cost note, when the estimate is on) are two compact rows, "5-hour limit 37% · resets 15:30" and "Weekly limit 58% · resets Mon 03:00", each over a thin meter (`role="meter"`, with the percentage and reset time as its text). Times are local and in 24-hour form, with the weekday whenever the date is not today's ("Sat 00:30"), and the date when it is more than 6 days away. The bar turns red when the rounded percentage is 90 or more, and a percentage that rounds above 999 shows as ">999%". The rows' tooltip, also read by screen readers, names the source ("Claude Code's status line, via the helper's limits file"; the app does not know the path). `app/src/usage/limits.ts` decides what to show, given the time now:
- A window whose `resetsAt` has passed on Windows' clock shows "reset since last reading" and no percentage, because its real value is unknown until the next reading.
- A reading more than 15 minutes old keeps its values and adds "Limits as of 14:02". The status line runs only while a Claude Code session is open (with `"refreshInterval": 60` in its settings, about once a minute), so readings stop when no session is. `updatedAt` was taken on WSL's clock, which can drift from Windows' (WSL 2 does after sleep), so the age is measured on the helper's clock: the answer's `computedAt` less `updatedAt`, plus the time since that answer arrived. An `updatedAt` more than a minute ahead of `computedAt` counts as stale.
- With neither window, the rows are not shown.

These rules also run on a timer, not only when an answer arrives, so the rows change at a reset, when the reading turns stale, and at local midnight even if the helper has stopped answering. The widget sets one timeout for the earliest of those and clears it when it unmounts.

When the rows are shown, the 7-day breakdown's list gets less room on short windows, so the widget stays clear of the system widget.

### System stats

The system widget shows CPU, memory and GPU use, with Refract's own share drawn inside each bar. It is sampled on the Windows side (`app/src-tauri/src/gpu.rs` and `stats.rs`), once every 2 seconds, **only while the window is visible**. A thread starts when the window is shown and ends when it is hidden.

- **GPU.** The performance counter `\GPU Engine(*)\Utilization Percentage` has one instance per process and engine. Instances are added up per adapter and engine type, and the busiest stands for the whole GPU, as in Task Manager. The same reading is filtered to the processes that are Refract's to give its own share. The first reading only primes the counters. Without the counter the stats still go out, with no GPU figure.
- **CPU and memory.** From `sysinfo`: whole-machine CPU and memory, and for Refract's own processes their CPU (as a share of all CPUs) and memory. A reading is two events: `system://gpu` for the quality controller and `system://stats` for the widget.
- **What is "Refract".** The app process plus the WebView2 processes and the `wsl.exe` that carries the helper, found by walking children of the app. VS Code, `cmd.exe`, `gh.exe` and the like are not counted. Work the helper does inside the WSL virtual machine is not attributed, and memory is summed per process, so pages shared between WebView2 processes can be counted more than once.

## Resource use while hidden

Hiding the window (the hotkey, the tray, or the close button, which only hides) stops everything that costs anything:

| What | Behaviour while hidden |
|---|---|
| The scene | The canvas frame loop is `never` and the ticker is cancelled, so nothing is drawn and no timer redraws |
| GPU and system sampling | The sampler thread ends |
| Usage polling | Stops (no requests to the helper) |
| Remote details refresh | The 15 minute timer is not kept |
| WebView2 | Hidden, then suspended with a low memory target; resumed before it is shown |
| The helper | Keeps running and keeps its watches; it wakes only for file changes, and for a liveness check while some session is live or recent |

When the window is shown again the interface fetches the repo list, settings, remote list and hook state again, because events can be held back or lost while WebView2 is suspended.

## Build, CI and release

Everything builds from WSL. The Windows app is cross-compiled with `cargo-xwin`; CI builds it natively on Windows. [README.md](../README.md) has the commands.

### Bundled files

`tauri.conf.json` bundles two resources into the installer: the VS Code extension package (`refract.vsix`) and the Linux helper (`refract-agent`). Neither is committed. `app/scripts/ensure-vsix.mjs` packages the extension if it is missing. `app/scripts/ensure-helper.mjs` writes an empty placeholder helper if there is none, which lets the app build anywhere; the app then uses a helper already installed in WSL. Both run through `npm run resources`, which the Tauri scripts call first.

### Installing the helper into WSL

On every start of the helper the app (`app/src-tauri/src/agent.rs`):
1. Picks the distro (`REFRACT_WSL_DISTRO`, or from `wsl.exe --list --verbose` as [launch.md](launch.md) describes).
2. Unless `REFRACT_AGENT` names a helper (development), runs `refract-agent --version` from `~/.local/share/refract/bin/` in that distro.
3. If that does not print `refract-agent <the app's version>`, it copies the bundled binary there through a temporary file and a rename, so a running helper keeps its old file and a failed copy leaves the old one in place.
4. Starts the helper with `wsl.exe --exec`.

The check allows 30 seconds and the copy 60. Any failure ends up in the helper status the interface shows. An empty bundled file means "use what is installed".

### CI

`.github/workflows/ci.yml` runs on every push and pull request:

| Job | Runs on | Checks |
|---|---|---|
| Rust (Linux workspace) | Linux | `cargo fmt --check`, `clippy -D warnings`, `cargo test` for `crates/*` |
| Web | Linux | `tsc`, Vitest, `vite build`, then Playwright against the web demo in Chromium with software WebGL |
| VS Code extension | Linux | Typecheck and Vitest |
| Rust (Windows app) | Windows | `fmt`, `clippy -D warnings` and `cargo test` for `app/src-tauri` |
| gitleaks | Linux | A secret scan of the whole history, with the tool's download pinned by checksum |

The browser tests set the graphics level to Low first: Medium animates constantly, and software WebGL on a two-core runner starves the demo's timers.

`pages.yml` builds `npm run build:demo` and deploys it to GitHub Pages, only after CI has passed on a push to `main`, building the commit CI tested. The repository's Pages source has to be set to GitHub Actions once.

### Release

`release.yml` runs on a `v*` tag, or by hand. It has three jobs:
1. **Helper.** Checks that the app's `tauri.conf.json` version, the app crate's version and the helper's version agree (and match the tag), builds a static musl binary, and checks that `--version` prints the expected string. The version match matters because the app reinstalls the helper whenever the two differ.
2. **Installer.** Puts that binary where the bundle expects it, packages the extension, and builds the NSIS installer. It is not code-signed, so Windows SmartScreen warns.
3. **Publish.** On a tag only, attaches the installer to a GitHub Release. This is the only job with permission to write to the repository, and it runs nothing from the build.

Third-party actions are pinned to commit hashes, and checkouts do not keep credentials.

## Security and privacy

- The interface cannot reach the network itself: its content security policy allows only the connection to the Rust side. The window has only the default core permissions.
- GitHub and GitLab tokens are stored in Windows Credential Manager and never reach files, logs or the interface. GitHub can reuse the GitHub CLI's existing login.
- API requests use HTTPS only, do not follow redirects, and send the token only to the host that was configured.
- `open_folder` opens directories only, `open_log_folder` opens only the app's own log folder, and `open_url` opens `https://` links only and refuses addresses with a user name in them.
- The status hook installer backs up `~/.claude/settings.json` before editing it, only touches the entries it added, and its hook command cannot block Claude Code.
- Refract never calls a model. Usage numbers are summed from files already on your machine.
- Nothing is sent anywhere except the GitHub and GitLab API calls needed to list repos, pull requests and CI status, and `git clone` from the host you chose to clone from.
