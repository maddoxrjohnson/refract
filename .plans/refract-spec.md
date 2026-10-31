# Refract: product and technical spec

Status: approved for build, 2026-10-29.
Interactive mockup of the chosen scene: `docs/mockups/gem-studies.html` (open it in a browser; it opens on Banknote). The first scene's mockup is `docs/mockups/frozen-shatter.html`.

## 1. What Refract is

Refract is a small desktop launcher for people who run coding agents across many repositories. It shows every project you work on as an engraved cut stone on a sheet of paper, in 3D. A stone's size, colour and marks tell you about the repo at a glance. Choose a stone and press Enter, and Refract opens a Claude Code session in that repo, inside a VS Code window that contains nothing but the terminal.

If a repo has uncommitted work, or an earlier agent session stopped before it finished, Refract offers to resume that conversation instead of starting a new one.

Refract never calls a language model itself, so it uses no tokens. It is designed to stay out of the way: it lives in the system tray, opens with a hotkey, and draws nothing while hidden.

## 2. Goals and non-goals

Goals

- One keystroke from "which project was I on?" to a working agent in the right folder.
- Never lose an interrupted agent conversation.
- Negligible cost when idle: 0% GPU, under 1% CPU and under 100 MB of memory in use (Task Manager's Memory column) while in the tray. The hidden WebView2 is suspended, so it keeps some memory reserved but paged out (about 140 MB in all in M3); that is acceptable as long as it stays within reason.
- A codebase that is pleasant to read: clear module boundaries, tests for the logic that matters, and documentation written for people.

Non-goals for version 1

- Running on macOS or native Linux. Version 1 targets Windows 11 with WSL2. OS-specific code is kept behind interfaces so other platforms can follow.
- Editing code, viewing diffs or reviewing pull requests inside Refract. It launches the tools you already use.
- Talking to Claude directly or managing API keys for it.

## 3. User experience

### 3.1 Opening Refract

- Refract starts with a tray icon. Starting with Windows is optional and off by default; it is toggled from the tray menu.
- Win+Alt+Space shows or hides the window from anywhere. (Win+Shift+Space was the first choice, but Windows reserves it for switching input languages.) Clicking the tray icon does the same.
- When hidden, Refract stops rendering completely.

### 3.2 The scene: Banknote

Every project is a cut stone drawn like the security engraving on a banknote: fine wavy line-work on each facet, a guilloche rosette printed on the sheet under it, and a pale wash of colour. The stones lie on a sheet of paper seen from about 56° above, heaviest near the centre. When Refract opens, the stones and a scatter of splinters burst from the centre and settle on the sheet; then each stone turns slowly. The camera always fits every stone in view, and moving the cursor tilts the view slightly. (The first scene, Frozen shatter, used refracting glass; see decision 0017.)

How a stone encodes its repo:

| Property | Meaning |
|---|---|
| Size | Mass: a blend of repository size and commits in the last 30 days, log-scaled and normalised across your repos |
| Colour wash and stone | The repo's main language (TypeScript is sapphire, Python citrine, Rust mandarin garnet, and so on) |
| Cut | Fixed per repo, so a repo is recognisable by shape |
| Red ring on the sheet | Uncommitted changes |
| Chipped corner | An agent session stopped before it finished |
| Rosette turns, stone turns faster | An agent is working in this repo right now (requires the status hook, §6) |
| Slow red pulse | An agent is waiting for your input (requires the status hook) |
| Stippled instead of engraved | The repo exists on GitHub or GitLab but is not cloned locally |

Red always means that something needs you. Light mode is green ink on pale paper; dark mode is gold ink on deep green. Refract follows the Windows app mode, and settings can fix it to Light or Dark. The panels use the same paper and ink.

### 3.3 Finding and choosing a project

- **Magnet focus.** The gem nearest the cursor (within about 130 px) is focused: it lifts, grows slightly and shows a small label with its name, host, language and state. You never need to aim exactly.
- **Click to zoom.** Clicking a gem zooms the camera to it and opens the detail panel.
- **Enter to launch.** With a gem zoomed, Enter launches (§4). Escape zooms back out.
- **Keyboard.** Typing filters gems by name. The arrow keys move focus between gems. Enter on a focused gem zooms, and Enter again launches.

### 3.4 Detail panel

Shown next to the zoomed gem:

- Branch, commits ahead and behind its upstream, and the number of changed files
- Last commit message and when it was made
- Last agent session: when it ran, whether it finished, and the first line of its final message
- Open pull requests (GitHub) or merge requests (GitLab), and CI status on the default branch. These are fetched from the network, cached, and refreshed about every 15 minutes or when stale.
- Actions: **New agent** or **Resume**, **Open folder**, **Open on GitHub/GitLab**, **Hide**

### 3.5 Widgets

There are two small paper widgets in the corners of the scene. Both update about every 2 seconds, and only while the window is visible.

- **System:** CPU, memory and GPU use, plus Refract's own share, so you can see that Refract is cheap to run.
- **Claude usage:** tokens used today, in the last 7 days and in a rolling 5-hour window, and how many agents are running now. Hovering shows a per-repo breakdown. The numbers are added up from Claude Code's local session files, so they cost nothing to collect. An optional cost estimate uses a pricing table you can edit. Plan-limit percentages are shown only if Claude Code exposes them through an official local source. Refract never guesses them.

### 3.6 Graphics quality

There are three levels. Refract chooses one automatically, and you can override it in settings. The look is the same at every level.

| Level | Resolution | Anti-aliasing | Splinters | Frame cap |
|---|---|---|---|---|
| Low | 1× | Off | 40 | 30 fps, and stones do not turn |
| Medium | up to 1.5× | 4× multisampling | 90 | 30 fps |
| High | up to 2× | 4× multisampling | 90 | 60 fps |

Automatic choice:
- Refract reads system-wide GPU use from Windows' performance counters every 2 seconds while visible. If other programs are using more than about 60% of the GPU, as a game or a render would, Refract drops to Low.
- If frames take longer than the level's budget for 3 seconds, it steps down a level.
- If there is plenty of headroom for 10 seconds, it steps up.

On every level, the scene only redraws while something is changing.

## 4. Launching an agent

### 4.1 What opens

A new VS Code window opens using a dedicated VS Code profile called "Refract", connected to WSL, for the repo's folder. The window shows only a terminal in the main editor area, with the sidebar, panel and activity bar hidden. The terminal runs Claude Code:

- New agent: `claude --session-id <new-uuid>`. Refract chooses the ID, so it always knows which conversation belongs to which launch.
- Resume: `claude --resume <session-id>`.

The profile keeps this bare layout away from your normal VS Code setup.

### 4.2 If the repo is already open

If a VS Code window already has that folder open, Refract brings it to the front instead of opening another. If you asked for a new agent, a new terminal tab opens in that window. If you chose "Go to the running agent", no terminal opens: the window comes to the front and shows the terminal tab that agent is running in.

### 4.3 Choosing between resuming and starting fresh

A launch goes straight to a new agent when the repo is clean, has no unfinished sessions and has no agent running in its Refract VS Code window. Otherwise a small picker lists:

- "Go to the running agent" for each agent still running in a terminal of the Refract window on this repo (what it is doing: waiting on you, working, idle or just running; when it last wrote; the first line of its last reply),
- each unfinished session (when it ran, the first line of its last message, how large it is), and
- New agent.

One option is preselected so that Enter accepts it:

- **Go to the running agent** when the most recent such agent is waiting on you. Nothing new starts; the existing window comes to the front with that agent's terminal shown.
- Otherwise **Resume** the most recent session when it ended less than about 2 days ago, edited the files that are currently uncommitted, and its transcript is below a size limit. Resuming a very long conversation reloads all of it, which costs more than starting fresh.
- Otherwise **New agent**, started with a short opening prompt: "This repo has uncommitted changes. Review `git diff` and help me finish or commit them."

### 4.4 Repos that aren't cloned yet

A stippled (not cloned) stone's detail panel shows **Clone and launch**. It clones into `~/projects/<repo-name>` in WSL, then launches a new agent.

## 5. Which projects appear

- **GitHub:** all repos you own or collaborate on, through the GitHub CLI's existing login.
- **School GitLab** (a school GitLab instance, URL set in settings): projects you are a member of, through a personal access token you paste once in settings.
- **Local folders:** repositories under configurable roots, by default `~/projects`, `~/code` and your Windows Desktop. They are matched to remote repos by their `origin` URL.
- Forks and archived repos are hidden by default.
- You can hide any repo, add any local folder, and unhide things from settings.

Tokens are stored in Windows Credential Manager, never in files.

## 6. Knowing what an agent is doing

### 6.1 Without the status hook (always available)

Refract reads the end of each session file in `~/.claude/projects/` and checks for running `claude` processes. A session counts as **interrupted** when it is not running and its last entry is one of these:
- a message from you with no reply,
- a tool call with no result,
- an API error.

### 6.2 With the status hook (optional, recommended)

Claude Code can run a command when something happens in a session. On first run, Refract offers to add one small script to your Claude Code settings, and shows exactly what will change. The script writes a one-line status file for each session when:

| Event | Meaning |
|---|---|
| Session starts | An agent is open in this repo |
| You send a prompt | The agent is working |
| Claude stops and waits | The turn finished |
| Claude needs permission or input | The agent is blocked on you |
| Session ends | Closed, and why |

The script prints nothing, so Claude never sees it and it costs no tokens. It takes a few milliseconds per event. Refract can remove it at any time, restoring your settings exactly. This is what makes the *working* and *waiting on you* states possible.

## 7. Architecture

See `docs/architecture.md` for the full description. In summary:

- **Refract app (Windows):** Tauri 2. The Rust side handles the tray, hotkey, GPU counters, GitHub and GitLab API calls, and launching VS Code. The interface is React with TypeScript and React Three Fiber.
- **refract-agent (WSL):** a small Rust program that Refract starts through `wsl.exe` and talks to over standard input and output. It scans repos, runs git, reads session files, watches for changes with inotify, and checks running processes. Doing this inside WSL avoids slow cross-filesystem access.
- **refract-core:** a Rust library with the logic that matters most: session parsing and classification, mass scoring, picker preselection and usage totals. It has no I/O, so it is fully unit-tested.
- **Refract VS Code extension:** runs in the WSL window, receives the launch request, sets up the bare terminal layout, and starts Claude Code.
- **Status hook:** a small script plus an installer that edits `~/.claude/settings.json` safely, keeping a backup.

The interface talks to the backend through one `Backend` interface. A `DemoBackend` implementation with invented data powers the web demo.

## 8. Public demo and repository

- A web build of the interface runs on GitHub Pages with invented repos. Launching shows a simulated VS Code terminal instead of starting anything.
- The README leads with a short recording of the demo.
- CI runs formatting, lints, Rust and TypeScript tests, browser tests of the demo, and a secret scan.
- Tagged releases build a Windows installer. It is unsigned at first, so Windows SmartScreen will warn on install.
- No private repository names appear in the demo, screenshots, test fixtures or recordings.
- Agent configuration, assistant notes and secrets are git-ignored.

## 9. Testing and acceptance

| Area | How it is tested |
|---|---|
| Session classification | Unit tests over recorded transcript fixtures: clean finish, crash mid-tool, API error, waiting on user, live session |
| Usage totals | Unit tests against fixtures whose totals were summed by hand |
| Git and scanning | Integration tests against temporary repos: clean, dirty, ahead, behind, no remote |
| Hook installer | Install then uninstall leaves `settings.json` byte-for-byte identical |
| Interface | Component tests (Vitest); browser tests of the demo build (Playwright) |
| End to end | Hotkey opens Refract. Launching a clean repo opens a bare VS Code terminal running a new agent. Launching again focuses the same window. Killing a session mid-tool shows amber edges and preselects Resume. |
| Resources | In the tray: 0% GPU, under 1% CPU, under 100 MB in use (reserved memory may be higher; see §2). Under heavy GPU load from another app, Refract drops to Low. |

## 10. Build order

1. Core logic and the WSL helper
2. The launch path: VS Code extension and profile, proven end to end before any 3D work
3. The Tauri shell: tray, hotkey, show and hide
4. The scene: gems, focus, zoom, detail panel, picker, quality levels
5. Remote repos: GitHub, GitLab, cloning, PR and CI status
6. The status hook and the two widgets
7. The demo site, README, CI and release builds

## 11. Open questions

- Whether Claude Code exposes plan-usage limits through an official local source (affects the usage widget).
- Code signing: whether to pay for a certificate (for example Azure Trusted Signing) so Windows doesn't warn on install.

## 12. Move to WSL

Repos on a Windows drive can be moved onto the WSL Linux filesystem from their detail panel: Refract copies the repo (skipping regenerable build folders), re-checks it out with Linux line endings, verifies every file and the full git state, moves Claude session history, runs the install, and sends the original to the Recycle Bin only after you confirm. Full spec: `.plans/wsl-migration-spec.md`.
