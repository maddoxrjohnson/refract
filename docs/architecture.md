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

Contents: [why the split](#why-the-work-is-split-this-way) · [layout](#repository-layout) · [protocol](#protocol-between-the-app-and-the-helper) · [client](#the-apps-client) · [session state](#session-state) · [interface](#the-interface) · [scene](#the-scene) · [remote repos](#remote-repos) · [status hook](#the-status-hook) · [usage and system stats](#usage-and-system-stats) · [resource use](#resource-use-while-hidden) · [build and release](#build-ci-and-release) · [privacy](#security-and-privacy). Choices that were weighed are recorded in [decisions](decisions/README.md).

## Why the work is split this way

**The helper runs inside WSL.** Claude Code, git and most repositories live in WSL's Linux filesystem. Reading thousands of small files across the Windows–WSL boundary (`\\wsl$`) is slow. Running git there from Windows means starting a process per call. A single long-lived Linux process can use inotify, so it waits for changes instead of polling, and idles at effectively zero CPU.

**Logic lives in a library with no I/O.** `refract-core` holds the decisions that are easy to get wrong:
- Is this session interrupted?
- Which option should the picker preselect?
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
| `crates/refract-core` | Session parsing and classification, mass score, picker rules, usage totals, hook settings edits, protocol types, log line splitting and stamping |
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
| `docs` | This document, [launch.md](launch.md), [decision records](decisions/README.md), the scene mockup |

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
| `usage` | none | token totals | Tokens used today, in 7 days and in 5 hours, by repo |
| `hookInfo` | none | `{installed, settingsPath, diff}` | Whether the status hook is installed, and what installing would change |
| `installHook` | none | the same | Installs the status hook |
| `uninstallHook` | none | the same | Removes it and restores `settings.json` |
| `shutdown` | none | `null` | Answers, then the helper exits |

### Events

| Event | Data | Sent when |
|---|---|---|
| `repoChanged` | full repo snapshot | A repo appeared or something about it changed, including its sessions and their live state |
| `repoRemoved` | `{path}` | A repo is gone or no longer under a configured root |
| `sessionChanged` | `{repoPath, session, preselect}` | One session of a repo changed state or content |
| `loadComplete` | none | The first load finished and every repo has been reported |

Usage totals are not pushed. The app asks for them with `usage`.

### Errors

Errors follow JSON-RPC where a code fits: `-32700` bad JSON, `-32600` not a request, `-32601` unknown method, `-32602` bad params, `-32603` internal. Refract adds `1` repo not found (or a launch path that is not a directory), `2` server setup failed, `3` clone failed, `4` status hook failed.

### Rules of the line protocol

An `id` may be any JSON number or string and is echoed back unchanged. A line whose `id` cannot be read (bad JSON, invalid UTF-8 with no readable id, a line over 1 MB, or an `id` that is not a number or string) is answered with `"id": null` and an error. Methods without parameters also accept `"params": {}` or `null`.

The first `listRepos` starts loading in the background and answers immediately with `{"repos": [...], "loading": true}`, listing what has loaded so far. Linux repos load first and repos under `/mnt/` after them. Repos arrive as `repoChanged` events as each phase finishes, and `{"event": "loadComplete"}` follows once everything is reported. After that, `loading` is `false`. A `getRepo` for a repo that has not loaded yet is answered once it has.

Repos that appear later are found by watching every directory the search walks under the roots. A folder with a `.git` that git cannot read is not reported. It might be a `git init` or a copy still in progress, or a repo git refuses (owned by another user, or made by a newer git). Git is never allowed to look above the folder for a repository enclosing it. The helper watches the folder and its git directory. With nothing prompting it, the helper tries the folder again 2, 10 and 60 seconds after each failure, then stops. A change in the folder or its git directory prompts a try 2 seconds after the last change, so a copy is read soon after it ends. While changes keep coming, the helper still tries at least once a minute. After three failed tries prompted by changes, it tries at most once a minute. A repo that stays unreadable therefore costs a try a minute while something writes to it, and nothing while nothing does. A repo replaced at the same path, for example moved away with another copied or cloned in its place, is read and watched again. The helper notices a replacement by the inode numbers of the folder and its `.git`. On file systems where those are not stable (overlayfs with `xino=off`, sshfs without `use_ino`, CIFS with `noserverino`), a rescan can mistake a repo for replaced and read it again, which is harmless. `/mnt/` drives keep them stable.

`prepareLaunch` leaves a request file for the VS Code extension and answers with the folder URI to open. `setupServer` runs on a separate thread inside the helper, so other requests are still answered while it runs, and its response can arrive after theirs.

`cloneRepo` and `addRepo` are described under [Cloning and adding repos](#cloning-and-adding-repos).

Writes to standard output block if the app stops reading. The helper then stops reading its input. Pending file-system events are held in the kernel's queue, which overflows if it fills, and an overflow triggers a full reload once output resumes. Memory therefore stays bounded and no change is lost.

The helper never writes anything else to standard output; logs go to standard error.

## The app's client

The app talks to the helper through `refract-client`, which starts the helper, numbers each request and hands each response back to the caller waiting for it. Events go straight to the interface. Each line of the helper's standard error is forwarded to the app's own standard error, and the last few lines are kept to explain a crash.

### The log file

The app's standard error shows in the terminal when the app is started from one. A release build has no console, so at startup, when the process has no standard error handle, `app/src-tauri/src/logfile.rs` points it at a pipe. Rust's standard library looks the handle up on every write, so every `eprintln!` in the app and its crates, the helper's forwarded lines included, goes into the pipe (an unreadable line from the helper is logged only by its first 200 bytes, since it may hold reply text or paths); so does the standard error of any child process that inherits it. One thread reads the pipe, splits it into lines of at most 16 KiB and stamps each with the UTC time to the millisecond. A failed read is tried again after 50 ms; after 20 failures in a row the thread stops and empties the standard error handle, so later writes are dropped rather than left waiting on a pipe nobody empties. Another appends them to `refract.log` in the app's log folder (`%LOCALAPPDATA%\io.github.maddoxrjohnson.refract\logs`), so a slow disk never holds up a writer: if that thread falls 1024 lines behind, new lines are dropped and the count is written once the queue empties. Lines the file cannot take are dropped too, and the file is opened again for the next one.

Each start writes a line with the app's version. A log over 1 MiB at startup, or one about to pass 2 MiB while running, is renamed to `refract.old.log`, replacing the previous one. If the rename fails (another program holds the log open without allowing it), the log may grow to twice the limit before one more try; if that fails too, the log is emptied instead. On exit, and after a panic's message, the app waits up to 200 ms for the lines already written to reach the file: it writes a marker line that the reading thread swallows, and waits for it and every line before it to be written. The settings panel's "Open log folder" opens the folder in Explorer.

If the helper exits, every request still waiting for an answer fails at once as disconnected, and the client starts the helper again after a delay. The delay starts at one second and doubles with each failure in a row, up to 30 seconds. A helper that ran for at least a minute before exiting counts as healthy, so the next delay is back to one second. Failing to start the helper at all, for example when `wsl.exe` is missing, counts as a failure too. While the helper is down, requests fail straight away instead of queueing, and the interface shows that it is restarting, when it will try again and why it stopped. A request that times out is forgotten, so an answer that arrives later is dropped. Shutting down sends `shutdown`, closes the helper's input and kills it if it has not exited after a grace period. Nothing is restarted after that.

The interface does not get a free line to the helper. Its `agent_request` command accepts only `ping`, `listRepos` and `getRepo`. Everything else goes through a command that checks what it sends: `launch`, `clone_repo`, `add_repo`, `hook_info`, `install_hook`, `uninstall_hook` and `usage`.

## Session state

Claude Code writes one JSON-lines file per conversation under `~/.claude/projects/<encoded working directory>/<session id>.jsonl`. For each repo, Refract lists the sessions whose working directory is that repo, then classifies each one:

| State | How it is known |
|---|---|
| Live | A `claude` process is running with that session id, or in that working directory (read from `/proc`) |
| Working / waiting on you / idle | Live, plus the [status hook's](#the-status-hook) most recent report. Without the hook a live session has no detail |
| Finished | Not live, and the last entry is a complete assistant reply (or a user interrupt) |
| Interrupted | Not live, and the last entry is an unanswered user message, a tool call without a result, tool results the assistant never answered, or an API error |

Only the start and the end of each file are read, and results are cached until inotify reports a change.

## The interface

`app/src/App.tsx` holds the state; the pieces are small and take plain props.

### Where the data comes from

- **Local repos** come from `listRepos` plus `repoChanged`, `repoRemoved` and `loadComplete` events, combined by a reducer (`repoState.ts`). Snapshots carry no version, so an event that arrives while a `listRepos` request is in flight is taken as newer than its answer. The list is fetched again whenever the helper enters the running state, and on every show of the window.
- **Remote repos** come from `listRemotes` and are merged with the local ones (`projects.ts`). A remote repo whose host, owner and name match a local repo's `origin` (ignoring case) is the same gem. One with no local clone becomes a dimmed gem keyed `remote:<host>/<owner>/<name>`, with no sessions and a mass capped at 0.45 so uncloned repos sit towards the edges. Forks and archived repos that are not cloned stay out unless settings turn them on; a cloned fork always shows. Hidden keys apply to every gem.
- **Gem appearance** comes from `repoStatus.ts`: uncommitted changes (blue halo), an interrupted session (amber edges), a live session working (shimmer) or waiting (slow amber pulse).

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
- The helper's preselect is `noPicker` (clean repo, nothing unfinished): launch a new agent at once.
- Otherwise open the picker.

A new agent in a repo with uncommitted changes starts with a short opening prompt asking Claude to review `git diff`. The picker lists unfinished sessions newest first, then New agent, and highlights the session the helper preselected or else New agent. Keys with Ctrl, Meta or Alt are ignored, a held Enter does not repeat, and the settings dialog takes all keys while it is open.

### Panels and widgets

- **Detail panel.** Branch, upstream, changed files, last commit, last agent session, and for repos with a GitHub or GitLab remote the open pull or merge requests and CI state. Actions: launch (or Clone and launch), Resume, New agent, Open folder, Open on the host, Hide. Remote details are fetched when a gem is zoomed, again 15 minutes after they were fetched while the same gem stays zoomed and the window is shown, and on Refresh.
- **Picker, filter bar, focus label.** Presentation for the navigation state above.
- **Settings.** GitLab address and token, GitHub token (the GitHub CLI login is used otherwise), show forks, show archived, refresh remotes, the hidden list with Unhide, adding a folder, the status hook's install and remove, and Open log folder. Changes are saved one after another; if saving fails the old values come back.
- **Status hook offer.** Once the repos are in, a small card offers to install the hook, with "Show what changes" revealing the diff of `settings.json`. "Not now" is remembered.
- **Widgets.** System load at the top right, Claude token use at the bottom right (hovering or focusing lists the last 7 days by repo). Both fade while the detail panel covers their corner.
- **Launch toast.** Shows cloning, setup and opening steps, and failures until dismissed. One launch or clone runs at a time.

### Where state is kept

| State | Where |
|---|---|
| Graphics setting (`refract.quality`), hook offer dismissed (`refract.hookOfferDismissed`) | The webview's `localStorage`; the interface works without it |
| GitLab address, show forks, show archived, hidden gems | `settings.json` in the app's config directory on Windows |
| GitHub and GitLab tokens | Windows Credential Manager |
| Extra folders, roots, hidden local paths | `~/.config/refract/agent.toml` in WSL (optional; `addRepo` appends to `extra`) |

### The web demo

`DemoBackend` implements the whole `Backend` with invented data: 13 local repos covering every gem state, remote repos (some matching, some not cloned, a fork and an archived one), pull request and CI numbers, a pretend hook diff with the real installer's commands and matchers, and usage and system numbers that drift slowly. Settings, hook state and clones live in memory and reset on reload. A launch walks through the progress steps and then tells `DemoTerminal`, which types out the `claude` command and an invented reply in a fake terminal. GitHub links open this project's page rather than the invented owner's. The demo has no GPU counters, so its automatic quality uses frame times only.

`main.tsx` picks the demo backend when built with `--mode demo` and whenever the page is not inside Tauri, which is what `npm run dev` shows in a plain browser. `npm run build:demo` builds it into `app/dist-demo` for GitHub Pages. `app/e2e/demo.spec.ts` runs seven browser tests against that build (see [Build, CI and release](#build-ci-and-release)).

## The scene

The scene is React Three Fiber code in `app/src/scene`.

### Layout and camera

Gems sit on a golden-angle spiral from the centre outwards, heaviest first (`layout.ts`). Depth and every random choice are seeded from a hash of the repo's path, so a gem keeps its place whatever order repos arrive in. Gems shrink once there are more than about 14. The camera fits every gem, shifts a little with the pointer, and when a gem is zoomed flies to it, placing it left of centre so the panel fits on the right, and in windows less than about 1300 px tall raising it so the launch picker fits below it. The focus label is moved by the frame loop with a DOM transform, so React does not re-render per frame.

### Two-pass refraction

Glass needs something behind it to bend. Each drawn frame therefore renders twice:

1. **Back pass.** Everything except the glass is drawn into a half-float texture, sized at the tier's share of the canvas. This is the painted backdrop, the dust and each gem's lit core. The glass sits on a separate layer that this pass skips, so a shell never samples itself.
2. **Main pass.** The whole scene goes through the post-processing chain (bloom on Medium and High, depth of field on High, ACES tone mapping). Each shell's shader looks up the back texture at its own screen position, shifted by the view ray refracted through the facet, once per colour channel with slightly different indices. That is the rainbow edge. Reflections come from a procedural set of soft lights, and glints from fixed highlight directions.

The number of samples per channel is compiled into the shader, so a change of level rebuilds the shell materials and the effect chain. The blue halo and the amber glow are additive shells around the gem; the amber edges are line segments.

### Drawing only on change

The canvas uses R3F's `demand` frame loop: a frame is drawn only when something asks for one. Frames are requested by a change of props (repos, filter, focus, zoom, level), a resize, or pointer movement. After each drawn frame the scene asks `ticker.ts` for the next one only if something is still moving: ambient motion on, the camera or a gem easing towards its goal, or the pointer parallax settling. When everything has settled the loop simply stops and costs nothing.

The ticker (`createTicker`) invalidates the canvas `1000 / fps` ms after the previous frame started, measured from the frame's start and a few milliseconds early so the animation frame that follows lands on the right display refresh. That is what holds the tier's frame cap, and pointer movement goes through it too.

### Quality levels

| | Low | Medium | High |
|---|---|---|---|
| Frame cap | 30 fps | 30 fps | 60 fps |
| Largest pixel ratio | 1× | 1.5× | 2× |
| Refraction samples | 1 | 3 | 12 |
| Back texture size | half | three quarters | full |
| Rainbow split, glints | none | partial | full |
| Bloom | off | on | on |
| Depth of field | off | off | on |
| Gems spin, scene breathes | no | yes | yes |
| Facets per gem | 18 | 28 | 40 |

Low has no ambient motion, so it draws only when something changes. The tiers are in `quality/tiers.ts`. The graphics control in the corner offers Auto, Low, Medium and High; a fixed choice bypasses the controller.

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

The scene, which is mostly three.js, is a separate chunk loaded with `React.lazy`, so the overlays and data requests start without waiting for it. A chunk that fails to load is asked for once more. Until it arrives the user sees an empty dark backdrop. If it still fails or throws, `SceneBoundary` shows a message and the overlays stay usable. If WebGL cannot start, the canvas shows its own message. The scene also follows the system's reduced-motion setting: no spin or breathing, and camera moves still ease but quickly.

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

| Claude Code event | Only for | Status written | What the gem shows |
|---|---|---|---|
| `SessionStart` | `startup`, `resume`, `clear` | finished | Nothing special (idle) |
| `UserPromptSubmit` | | working | Shimmer |
| `PostToolUse` | any tool | working | Shimmer (marks the turn working again after a permission prompt is answered) |
| `Notification` | `permission_prompt`, `elicitation_dialog` | waitingOnUser | Slow amber pulse |
| `Stop` | | finished | Nothing special (idle) |
| `SessionEnd` | | ended | Nothing special |

`Notification` is limited to permission prompts and input requests because its `idle_prompt`, sent after a minute at the prompt, would wrongly say the agent is blocked. `SessionStart` leaves out `compact`, which happens mid-turn.

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
- **Backup.** Before the first change the helper saves the file's bytes (or a marker if there was no file) in the state directory. A second install never overwrites the backup.
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

A relative `XDG_*` value is ignored. The helper watches the status directory with inotify and updates the affected session when a file is written or removed. Status files untouched for seven days are deleted when the directory is read in full.

## Usage and system stats

### Usage totals

The usage widget adds up the tokens in Claude Code's own session files. Nothing is sent anywhere, and no model is called. `refract-core/src/usage.rs` does the counting and `refract-agent/src/usage.rs` keeps it up to date.

**What counts.** Every `assistant` entry with a `message.usage` and a timestamp, in the main transcripts and in subagent transcripts, which are real cost. Input, output, cache read and cache write tokens are kept apart and the widget shows their sum. Entries whose counts are all zero (synthetic error messages) are dropped.

**Windows.** Today (since local midnight in WSL's time zone), the last 7 days and the last 5 hours, both rolling and ending now. The 7 day total is also split by repo: a transcript belongs to the deepest known repo that contains the directory the session started in, else to that directory itself.

**De-duplication.** Claude Code writes one response as several lines that repeat the same `message.id`, `requestId` and usage. Records are keyed by `message.id` (else `requestId`) and counted once, within a file and across files, because a session and its fork can both hold the same message.

**Incremental reads.** A separate thread with its own inotify instance watches the `projects` directory, every project directory and the deeper directories that hold recent transcripts. For each transcript written in the last 7 days it keeps the byte offset it has read up to and the records found. A write reads only the new bytes, up to the last complete line; a half-written last line waits for its newline. A file that shrank or was replaced is read from the start. Lines that do not mention `"usage"` are skipped without parsing, once the session's directory is known. Records older than 7 days are dropped, so answering `usage` is only adding up what is in memory.

**Never waiting.** The first scan runs on that thread, and until it finishes the answer is zeros marked `ready: false`, which the widget shows as counting. While the thread is busy reading, `usage` answers the last summary it computed rather than waiting for it.

The widget asks every 2 seconds, one request after the last has answered, only while the window is visible and the helper is running. It shows token counts only: there is no cost estimate, and no plan-limit percentage, which Refract will not guess.

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
