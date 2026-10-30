# Architecture

Refract has four parts. They are split by where they have to run, not by layer.

```
Windows                                              WSL (Ubuntu)
┌──────────────── Refract (Tauri 2) ───────────────┐                  ┌──────────── refract-agent ────────────┐
│ Interface: React, TypeScript, React Three Fiber  │                  │ Repo scanning and git status          │
│ Tray, global hotkey, start with Windows          │  JSON lines over │ Session file reading and classifying  │
│ GPU and system counters, quality controller      │ ◄──────────────► │ inotify watches: repos, sessions,     │
│ GitHub / GitLab APIs, tokens in Credential Mgr   │  stdin / stdout  │   hook status files                   │
│ Starts VS Code                                   │  (wsl.exe)       │ Running-process checks (/proc)        │
└──────────────────────────────────────────────────┘                  │ Cloning, launch requests              │
                                                                       └───────────────────────────────────────┘
        ┌──────── VS Code window (WSL remote, "Refract" profile) ────────┐
        │ Refract extension: reads the launch request, hides the editor  │
        │ chrome, opens a terminal and runs `claude`                      │
        └─────────────────────────────────────────────────────────────────┘
```

## Why the work is split this way

**The helper runs inside WSL.** Claude Code, git and most repositories live in WSL's Linux filesystem. Reading thousands of small files across the Windows–WSL boundary (`\\wsl$`) is slow. Running git there from Windows means starting a process per call. A single long-lived Linux process can use inotify, so it waits for changes instead of polling, and idles at effectively zero CPU.

**Logic lives in a library with no I/O.** `refract-core` holds the decisions that are easy to get wrong:
- Is this session interrupted?
- Which option should the picker preselect?
- How heavy is this repo?
- How many tokens were used in the last five hours?

It takes parsed data in and returns values, so every rule is covered by fast unit tests over recorded fixtures.

**The interface does not know where its data comes from.** The React app talks to a `Backend` interface:
- `TauriBackend` forwards calls to the Rust side.
- `DemoBackend` serves invented repos and simulated launches.

The same interface code runs in the desktop app and in the public web demo.

**VS Code does the terminal work.** Embedding a terminal in Refract would mean shipping a terminal emulator and PTY handling, and keeping each agent's process alive inside Refract. Instead Refract asks VS Code to open the folder. A small extension in that window then:
- arranges a terminal-only layout,
- runs `claude`,
- watches for later requests aimed at the same folder.

When VS Code is given a folder that is already open, it focuses that window. That covers the "focus the existing window" behaviour with no extra work.

## Repository layout

| Path | Contents |
|---|---|
| `crates/refract-core` | Session parsing and classification, mass score, picker rules, usage totals, protocol types |
| `crates/refract-agent` | The WSL helper binary |
| `app/src-tauri` | The Windows app's Rust side |
| `app/src` | The interface |
| `vscode-extension` | The VS Code extension |
| `hook` | Status hook script and installer |
| `docs` | This document, decision records, the scene mockup |

## Protocol between the app and the helper

The two sides exchange newline-delimited JSON over the helper's standard input and output.
- **Requests** carry an `id` and receive exactly one response with the same `id`.
- **Events** have no `id` and arrive when something changes, such as a repo becoming dirty, a session starting or ending, or usage totals moving.

An `id` may be any JSON number or string and is echoed back unchanged. A line whose `id` cannot be read (bad JSON, invalid UTF-8 with no readable id, a line over 1 MB, or an `id` that is not a number or string) is answered with `"id": null` and an error. Methods without parameters also accept `"params": {}` or `null`.

The first `listRepos` starts loading in the background and answers immediately with `{"repos": [...], "loading": true}`, listing what has loaded so far. Linux repos load first and repos under `/mnt/` after them. Repos arrive as `repoChanged` events as each phase finishes, and `{"event": "loadComplete"}` follows once everything is reported. After that, `loading` is `false`. A `getRepo` for a repo that has not loaded yet is answered once it has.

`prepareLaunch` leaves a request file for the VS Code extension and answers with the folder URI to open. `setupServer` installs the Refract extension in the WSL VS Code server. It runs on a separate thread inside the helper, so other requests are still answered while it runs, and its response can arrive after theirs. [launch.md](launch.md) describes both.

`cloneRepo {url, name}` clones an `https://` URL into `~/projects/<name>` and answers with the clone's path; when that folder is already a clone of the same URL it answers with the path without cloning, and any other folder in the way is an error. Clones run one at a time on their own thread with a ten-minute limit and never prompt for credentials. `addRepo {path}` takes any folder inside a working tree. Either way the repo is added to `extra` in `agent.toml` unless a root already covers it, and is reported with `repoChanged`: before the response once the first load is done, otherwise by the load.

Writes to standard output block if the app stops reading. The helper then stops reading its input. Pending file-system events are held in the kernel's queue, which overflows if it fills, and an overflow triggers a full reload once output resumes. Memory therefore stays bounded and no change is lost.

The helper never writes anything else to standard output; logs go to standard error.

## The app's client

The app talks to the helper through `refract-client`, which starts the helper, numbers each request and hands each response back to the caller waiting for it. Events go straight to the interface. Each line of the helper's standard error is forwarded to the app's own standard error, which shows when the app is run from a terminal (a log file comes later), and the last few lines are kept to explain a crash.

If the helper exits, every request still waiting for an answer fails at once as disconnected, and the client starts the helper again after a delay. The delay starts at one second and doubles with each failure in a row, up to 30 seconds. A helper that ran for at least a minute before exiting counts as healthy, so the next delay is back to one second. Failing to start the helper at all, for example when `wsl.exe` is missing, counts as a failure too. While the helper is down, requests fail straight away instead of queueing, and the interface shows that it is restarting, when it will try again and why it stopped. A request that times out is forgotten, so an answer that arrives later is dropped. Shutting down sends `shutdown`, closes the helper's input and kills it if it has not exited after a grace period. Nothing is restarted after that.

## Session state

Claude Code writes one JSON-lines file per conversation under `~/.claude/projects/<encoded working directory>/<session id>.jsonl`. For each repo, Refract lists the sessions whose working directory is that repo, then classifies each one:

| State | How it is known |
|---|---|
| Live | A `claude` process is running with that session or working directory |
| Working / waiting on you | The optional status hook's most recent event |
| Finished | The last entry is a complete assistant reply |
| Interrupted | Not live, and the last entry is an unanswered user message, a tool call without a result, or an API error |

Only the end of each file is read, and results are cached until inotify reports a change.

## Rendering and resource use

- The scene draws only when something changes, or when the quality level allows ambient motion. It stops entirely while the window is hidden.
- Glass refraction works in two passes:
  1. The scene without the glass shells is drawn into a texture.
  2. Each shell's shader bends its lookup into that texture, by slightly different amounts for red, green and blue.
- The quality controller picks one of three levels from system GPU use and measured frame time. The levels change sample counts, resolution and post-processing, so the same scene scales from integrated graphics to a desktop GPU.

## Security and privacy

- GitHub and GitLab tokens are stored in Windows Credential Manager. GitHub can reuse the GitHub CLI's existing login.
- The status hook installer backs up `~/.claude/settings.json` before editing it, and only touches the entries it added.
- Nothing is sent anywhere except the GitHub and GitLab API calls needed to list repos, pull requests and CI status.
