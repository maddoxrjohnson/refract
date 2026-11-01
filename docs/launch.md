# Launch path

How a click in Refract becomes a VS Code window with a terminal running Claude Code.

```
Refract (Windows)                 refract-agent (WSL)                VS Code window (WSL remote)
  │ prepareLaunch {path, mode} ──►  writes a request file
  │ ◄── {requestId, sessionId,      ~/.local/state/refract/launch/
  │      folderUri}                 <requestId>.json
  │
  │ code --profile Refract --folder-uri <folderUri>
  └───────────────────────────────────────────────────────────────►  opens or focuses the window
                                                                      Refract extension claims the
                                                                      request, opens a terminal, runs
                                                                      `claude --session-id <id>`
```

## Why a request file

`code` can only open or focus a folder; it cannot pass a message to the window it opens. The helper therefore leaves a request file in WSL. The extension runs in the WSL extension host, so it reads that file with ordinary Node file access and watches the directory for later requests.

VS Code focuses an existing window when it is given a folder that is already open. A later request for the same folder therefore reaches the window that is already running, which opens a new terminal tab. A focus request (see below) reaches it the same way, and shows a terminal that is already there instead.

## Request file

Directory: `$XDG_STATE_HOME/refract/launch/`, defaulting to `~/.local/state/refract/launch/`. A relative `XDG_STATE_HOME` is ignored. The helper creates the directory with mode 0700, since requests hold prompts.

`<requestId>.json`, written to a temporary file in the same directory and then renamed into place, so readers never see a partial file:

```json
{
  "version": 1,
  "requestId": "6f1c…",
  "folder": "/home/me/projects/example",
  "argv": ["claude", "--session-id", "0b7e…"],
  "title": "New agent",
  "createdAt": "2026-10-29T17:54:44Z"
}
```

- `folder` is the repo's absolute Linux path, with no trailing slash.
- `argv` is the command to run in the terminal. For a new agent it is `claude --session-id <uuid>`, with `--permission-mode plan` after it in plan mode (refactors use it), followed by `--` and an opening prompt if there is one. The `--` ends `claude`'s options, so a prompt that starts with `-` is not read as a flag. For a resume it is `claude --resume <session-id>`, followed by `--` and a prompt if one is given.
- `title` names the terminal tab.
- `focus`, present only on a focus request, names a running agent whose terminal to show. `argv` is then empty and nothing is started:

```json
{
  "version": 1,
  "requestId": "6f1c…",
  "folder": "/home/me/projects/example",
  "argv": [],
  "title": "Running agent",
  "createdAt": "2026-10-29T17:54:44Z",
  "focus": {"sessionId": "0b7e…", "pids": [8123, 8090, 7311]}
}
```

  `pids` is the session's `claude` process followed by its ancestors, nearest first, read from field 4 (`ppid`) of `/proc/<pid>/stat`: the `claude` process, the shell that started it, and so on. The chain stops before pid 1 and holds at most 16 entries. The extension accepts 1 to 32 positive integers. A VS Code terminal's `processId` is its shell's pid, so the terminal running the agent is the one whose `processId` is in the chain, however many processes (such as `npx` or a wrapper script) sit between the shell and `claude`.

A window **claims** a request by renaming `<requestId>.json` to `<requestId>.claimed`. If the rename fails because the file has gone, another window took it. After starting the terminal, the window deletes the `.claimed` file.

A window ignores requests for other folders, requests with an unknown `version` or that cannot be parsed, requests older than one hour, and requests dated more than a minute in the future. A request between two minutes and one hour old is claimed but not started on its own; see "In the window". A focus request of that age is deleted without being claimed. The helper deletes request, claimed and leftover temporary files older than one hour each time it writes a new one.

## Protocol

`prepareLaunch` request params:

```json
{"path": "/home/me/projects/example", "mode": {"kind": "new", "prompt": "optional", "planMode": false}}
{"path": "/home/me/projects/example", "mode": {"kind": "resume", "sessionId": "0b7e…", "prompt": "optional"}}
{"path": "/home/me/projects/example", "mode": {"kind": "focus", "sessionId": "0b7e…"}}
```

Result:

```json
{"requestId": "6f1c…", "sessionId": "0b7e…", "folderUri": "vscode-remote://wsl+Ubuntu/home/me/projects/example"}
```

- For a new agent, the helper generates `sessionId` (a random UUID v4). For a resume or a focus, it echoes the given one back. A resume or focus `sessionId` that is not a hyphenated UUID is rejected with `INVALID_PARAMS`.
- A focus names a live session of the repo whose process runs in a terminal of the Refract window on exactly that folder: its environment has `REFRACT_FOLDER` equal to the repo path, ignoring a trailing slash (and ASCII case under `/mnt/`). The snapshot marks such sessions `focusable`. The helper takes the process from its last liveness check, which records a pid only when certain: the process names the session id, or it is the only process in the repo naming none. Before reading the ancestors it confirms the pid still runs `claude` and that its start time in `/proc/<pid>/stat` is the one recorded, so a pid reused by a later process is not followed. When the session is not live, its process is unknown or has exited, or it runs anywhere else, the error code is `NOT_RUNNING` (5) with a message such as "that agent is no longer running" or "that agent is not running in a Refract VS Code window on this folder", which the app shows as it is.
- `folderUri` uses the distro from `WSL_DISTRO_NAME`. Path segments are percent-encoded.
- The path must be an existing directory. Otherwise the error code is `REPO_NOT_FOUND`.

`setupServer` runs steps 3 and 4 of the first-run setup (see below) inside WSL, where the server's files are. Params:

```json
{"vsix": "C:\\Users\\me\\AppData\\Local\\Refract\\refract.vsix", "commit": "1a2b3c…"}
```

- `vsix` is the extension package. It may be a Linux path (`/home/me/refract.vsix`, where `\` is part of a file name rather than a separator), a Windows drive path (`C:\Users\me\refract.vsix`, `C:/Users/me/refract.vsix` or the verbatim `\\?\C:\Users\me\refract.vsix`, read as `/mnt/c/Users/me/refract.vsix`), or a WSL path (`\\wsl.localhost\<distro>\home\me\refract.vsix`, `\\wsl$\<distro>\…` or the verbatim `\\?\UNC\wsl.localhost\<distro>\…` and `\\?\UNC\wsl$\<distro>\…`, read as `/home/me/refract.vsix`). Verbatim paths are accepted because the app's resource directory comes from `canonicalize`. The helper converts the path itself and does not check the distro name. A relative path, another share, another verbatim form such as `\\?\Volume{…}\…`, or anything else is rejected with `INVALID_PARAMS`.
- `commit` is the Windows VS Code's commit, the second line of `code --version`. It may be left out or `null`, and then the most recently modified server is used.

Result:

```json
{"installed": true, "applicationScoped": true}
{"installed": false, "applicationScoped": false, "skipped": "no vsix at /home/me/refract.vsix"}
```

- `installed` is true when the vsix was installed by this request. `applicationScoped` is true when this request marked the extension for every profile. Both are false when everything was already in place.
- `skipped`, when present, says why nothing was done: there is no file at `vsix`, or there is no VS Code server yet. The launch can still go ahead.
- Any other failure, such as `code-server` failing or `extensions.json` not being readable, is an error with code `SETUP_FAILED` (2).
- Installing takes several seconds. The helper runs it on a separate thread, so it answers other requests in the meantime, and a later request's response can arrive first. `setupServer` requests run one at a time, in the order they arrive.

## The Refract profile

Refract opens every window with `--profile Refract`. First-run setup creates the profile (see below), and VS Code records it in `%APPDATA%\Code\User\globalStorage\storage.json` under `userDataProfiles` (`{"location": "<dir>", "name": "Refract"}`). Its settings live in `%APPDATA%\Code\User\profiles\<dir>\settings.json`.

After a launch, Refract finds the profile there and adds any of these settings that are missing. It never changes a value that is already set:

| Setting | Value |
|---|---|
| `workbench.activityBar.location` | `"hidden"` |
| `workbench.statusBar.visible` | `false` |
| `workbench.startupEditor` | `"none"` |
| `workbench.layoutControl.enabled` | `false` |
| `workbench.secondarySideBar.defaultVisibility` | `"hidden"` |
| `workbench.tips.enabled` | `false` |
| `window.commandCenter` | `false` |
| `chat.commandCenter.enabled` | `false` |
| `terminal.integrated.defaultLocation` | `"editor"` |

VS Code watches the settings file, so the layout applies even to the window that was just opened.

Folders remember the profile they were last opened with (`profileAssociations` in `storage.json`). Opening a folder with `--profile Refract` moves it to the Refract profile.

## The extension

`vscode-extension/` has `extensionKind: ["workspace"]`, so it runs in the WSL extension host. Without a request file it does nothing, so it is harmless in ordinary windows.

It declares `capabilities.untrustedWorkspaces.supported: true`, so it also runs in Restricted Mode, before the folder is trusted.

In a window with a single folder it sets `REFRACT_FOLDER` to that folder's path through its environment variable collection, so every terminal the window starts carries it: the ones the extension opens and ones opened by hand. The helper reads it from a `claude` process's environment to tell which window the process runs in. Windows with no folder, a multi-root workspace or a parent folder of the repo get no value, or a different one, so their agents are not offered. Neither are agents in other editors (VS Code Insiders, Cursor, VSCodium), whose WSL servers do not have the extension. A terminal only gets the variable when it starts, so agents started before extension 0.1.3 was active in the window are not offered until they are restarted in a new terminal.

Extensions in the WSL server belong to one profile, like extensions on Windows. The exception is an entry marked application-scoped ("Apply Extension to all Profiles"), which every profile gets. Refract marks its own entry this way, so the extension reaches Refract windows.

## First-run setup

A Refract window needs four things before the first launch. Each step checks first and does nothing when it is already done, so Refract runs them before every launch, in this order, and opens the folder only after them. Steps 1 and 2 run on Windows. Steps 3 and 4 change files in WSL, so the app asks the helper to do them with `setupServer`.

1. **The Refract profile.** Until the profile exists, `code --profile Refract --list-extensions` and `--install-extension` fail with "Profile 'Refract' not found." If `storage.json` has no Refract profile, Refract runs `code --profile Refract --new-window`, which creates the profile and opens one empty local window in it. It then reads `storage.json` every 500 ms, for up to 20 seconds, until the profile appears.
2. **The WSL extension in the Refract profile.** The profile starts empty, and without `ms-vscode-remote.remote-wsl` its windows cannot connect to WSL. Refract runs `code --profile Refract --list-extensions` and, if the extension is missing, `code --profile Refract --install-extension ms-vscode-remote.remote-wsl`. These must reach the Windows VS Code. On Windows that is `code.cmd`. From WSL it is `cmd.exe /c "code …"` run from a directory under `/mnt/c`, because VS Code's `bin/code` run from WSL sends extension commands to the WSL server instead. If step 1 opened an empty window, Refract now closes it: the window whose title contains the profile name and not `[WSL`.
3. **The Refract extension in the WSL server.** The helper reads the version from the vsix's `extension/package.json` and the installed `refract.refract` version from `~/.vscode-server/extensions/extensions.json`. Only when they differ, or the extension is not installed, does it run `~/.vscode-server/bin/<commit>/bin/code-server --install-extension refract.vsix --force`. A rebuilt vsix therefore needs a new version to be installed. `<commit>` is the Windows VS Code's commit, the second line of `code --version`; if that directory is missing, or the app sends no commit, the helper uses the most recently modified one. The server directory only exists after VS Code has connected to WSL once, so on a fresh machine the helper says so in `skipped` and skips this step and the next.
4. **The extension in every profile.** In `~/.vscode-server/extensions/extensions.json`, the helper sets `metadata.isApplicationScoped` to `true` on the `refract.refract` entry. It changes only that field, keeps every other entry as it was, and writes through a temporary file.

## From the Windows app

The app runs every step above from Windows, one launch at a time, and reports each step to the interface as a `launch://progress` event (`setup`, `server`, `prepare`, `opening`, `done`). It finds VS Code's `code.cmd` in the user install under `%LOCALAPPDATA%\Programs\Microsoft VS Code\bin`, or else on `PATH`, and runs it directly. Rust's standard library starts `.cmd` files through `cmd.exe` and quotes their arguments for it. Folder URIs are percent-encoded, so they pass through unchanged. Every process the app starts, `wsl.exe`, `code.cmd` and `powershell.exe` alike, gets `CREATE_NO_WINDOW`, so no console window flashes up. The extension package comes from `REFRACT_VSIX` when it is set, which the development script points at the one built in the repo, and otherwise from the copy bundled with the app. The helper that answers `prepareLaunch` and `setupServer` is bundled and installed into WSL by the app too; [architecture.md](architecture.md#installing-the-helper-into-wsl) describes how.

The app starts `wsl.exe` with `WSL_UTF8=1`, so its own messages arrive as UTF-8 rather than UTF-16, and runs it from `%USERPROFILE%` with `--cd ~`, because `wsl.exe` cannot translate a `\\wsl.localhost` working directory. It names the distro with `-d` when it has one: `REFRACT_WSL_DISTRO` if set, otherwise one chosen from `wsl.exe --list --verbose`, read when the helper is first started (off the main thread, with a 10 second limit). The app takes the default distro, unless the default belongs to a container tool (`docker-desktop`, `rancher-desktop` and their `-data` distros); then it takes the first other distro, preferring one whose name starts with `Ubuntu`. Only rows shaped like the table count, so a message such as "no installed distributions" yields none. When the list cannot be read (`wsl.exe` fails or times out) or offers no usable distro, the app leaves out `-d` so `wsl.exe` uses its default, and logs why. A list that was read is kept for later restarts of the helper; one that failed is read again at the next start.

Commands the app runs on Windows have time limits, after which they are killed and the launch fails with an error naming the command: 90 seconds for `code` commands that open a window (a cold start can be slow), 60 seconds for `code --install-extension`, and 30 seconds for anything else, such as `code --version` and the PowerShell window closer. The bundled extension's path comes from `canonicalize` and is verbatim (`\\?\C:\…`); the app sends it to the helper in the plain form (`C:\…`, or `\\server\…` for `\\?\UNC\server\…`).

The `agent_request` command, through which the interface talks to the helper, accepts only `ping`, `listRepos` and `getRepo`; other methods are refused with `-32601`. Launching and setup go through the app's own `launch` command.

While its window is hidden, the app suspends WebView2 (`TrySuspend`, with the memory usage target set to low) so it holds as little memory as it can, and resumes it before showing the window. The interface fetches the repo list again on each show, in case events were held back while it was suspended.

## Workspace trust

A terminal opened in a folder VS Code does not trust waits for VS Code's trust dialog before its shell starts. Refract keeps that protection rather than setting `terminal.integrated.allowInUntrustedWorkspace`. Instead, first-run setup asks you to trust your projects folder once: run **Workspaces: Manage Workspace Trust** and add `~/projects`. Every repo under it is then trusted.

## In the window

On activation the extension scans the launch directory for requests matching its single workspace folder. It also watches that directory, and scans again whenever the window gains focus. For each claimed request it:

1. Opens a terminal in the editor area, named after `title`, with the folder as its working directory.
2. Types the command into it. Arguments that are plain tokens, such as `claude`, flags and UUIDs, are typed as they are. Any other argument, such as an opening prompt, is passed in the terminal's environment as `REFRACT_ARG_<n>` and typed as `"$REFRACT_ARG_<n>"`. The typed line therefore stays short, and tabs, quotes or a long prompt cannot be misread by the shell. The shell stays open after `claude` exits.
3. On a request found during activation (that is, the window was opened for it), sets up the bare layout. It closes the sidebar, panel and secondary sidebar, then closes every tab that is not a terminal and has no unsaved changes. Requests that arrive later only add a terminal tab, so an existing window's layout is left alone.

A focus request is handled differently. The window reads each open terminal's `processId` in turn and calls `show()` on the first one whose pid is in `focus.pids`. It never creates a terminal and never sets up the bare layout. When no terminal matches, for example because the agent runs in another window or was started in a terminal that has since been closed, it logs that to the Refract output channel and does nothing else. The window itself is brought to the front by the same `code --folder-uri` call as any launch.

A request is fresh for two minutes after `createdAt`. A window can take longer than that to connect, for example while VS Code's server downloads after an update, so a request between two minutes and one hour old is not dropped. The window claims it, then shows a message: `Refract asked to start "<title>" here <N> minutes ago, but the window took too long to open. Start it now?` with **Start** and **Dismiss**.

- **Start** runs the request exactly as a fresh one, including the bare layout if it was found during activation.
- **Dismiss**, or closing the message, deletes the claimed file and starts nothing.
- If several such requests are found at once, such as after a held key, only the newest gets a message. The older ones are deleted.
- The message is not modal, so a fresh request that arrives while it is open still starts at once.
- A focus request that old is deleted without a message: showing a terminal minutes after it was asked for would only surprise.

The extension reads every terminal's `processId` at once, each with a one-second limit, so a terminal that never answers cannot hold up the queue. One that times out or has no process id counts as no match.

A window that is already open with extension 0.1.2 or older ignores focus requests, because their `argv` is empty, until it is reloaded. Until then, going to a running agent brings the window forward without switching tabs.
