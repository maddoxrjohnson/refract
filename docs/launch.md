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

VS Code focuses an existing window when it is given a folder that is already open. A later request for the same folder therefore reaches the window that is already running, which opens a new terminal tab.

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
- `argv` is the command to run in the terminal. For a new agent it is `claude --session-id <uuid>`, followed by `--` and an opening prompt if there is one. The `--` ends `claude`'s options, so a prompt that starts with `-` is not read as a flag. For a resume it is `claude --resume <session-id>`.
- `title` names the terminal tab.

A window **claims** a request by renaming `<requestId>.json` to `<requestId>.claimed`. If the rename fails because the file has gone, another window took it. After starting the terminal, the window deletes the `.claimed` file.

A window ignores requests for other folders, requests with an unknown `version`, and requests older than two minutes. The helper deletes request, claimed and leftover temporary files older than one hour each time it writes a new one.

## Protocol

`prepareLaunch` request params:

```json
{"path": "/home/me/projects/example", "mode": {"kind": "new", "prompt": "optional"}}
{"path": "/home/me/projects/example", "mode": {"kind": "resume", "sessionId": "0b7e…"}}
```

Result:

```json
{"requestId": "6f1c…", "sessionId": "0b7e…", "folderUri": "vscode-remote://wsl+Ubuntu/home/me/projects/example"}
```

- For a new agent, the helper generates `sessionId` (a random UUID v4). For a resume, it echoes the given one back. A resume `sessionId` that is not a hyphenated UUID is rejected with `INVALID_PARAMS`.
- `folderUri` uses the distro from `WSL_DISTRO_NAME`. Path segments are percent-encoded.
- The path must be an existing directory. Otherwise the error code is `REPO_NOT_FOUND`.

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

Extensions in the WSL server belong to one profile, like extensions on Windows. The exception is an entry marked application-scoped ("Apply Extension to all Profiles"), which every profile gets. Refract marks its own entry this way, so the extension reaches Refract windows.

## First-run setup

A Refract window needs four things before the first launch. Each step checks first and does nothing when it is already done, so Refract runs them before every launch, in this order, and opens the folder only after them.

1. **The Refract profile.** Until the profile exists, `code --profile Refract --list-extensions` and `--install-extension` fail with "Profile 'Refract' not found." If `storage.json` has no Refract profile, Refract runs `code --profile Refract --new-window`, which creates the profile and opens one empty local window in it. It then reads `storage.json` every 500 ms, for up to 20 seconds, until the profile appears.
2. **The WSL extension in the Refract profile.** The profile starts empty, and without `ms-vscode-remote.remote-wsl` its windows cannot connect to WSL. Refract runs `code --profile Refract --list-extensions` and, if the extension is missing, `code --profile Refract --install-extension ms-vscode-remote.remote-wsl`. These must reach the Windows VS Code. On Windows that is `code.cmd`. From WSL it is `cmd.exe /c "code …"` run from a directory under `/mnt/c`, because VS Code's `bin/code` run from WSL sends extension commands to the WSL server instead. If step 1 opened an empty window, Refract now closes it: the window whose title contains the profile name and not `[WSL`.
3. **The Refract extension in the WSL server.** Refract reads the version from the vsix's `extension/package.json` and the installed `refract.refract` version from `~/.vscode-server/extensions/extensions.json`. Only when they differ, or the extension is not installed, it runs `~/.vscode-server/bin/<commit>/bin/code-server --install-extension refract.vsix --force`. A rebuilt vsix therefore needs a new version to be installed. `<commit>` is the Windows VS Code's commit, the second line of `code --version`; if that directory is missing, Refract uses the most recently modified one. The server directory only exists after VS Code has connected to WSL once, so on a fresh machine Refract says so and skips this step.
4. **The extension in every profile.** In `~/.vscode-server/extensions/extensions.json`, Refract sets `metadata.isApplicationScoped` to `true` on the `refract.refract` entry. It changes only that field, keeps every other entry as it was, and writes through a temporary file.

## Workspace trust

A terminal opened in a folder VS Code does not trust waits for VS Code's trust dialog before its shell starts. Refract keeps that protection rather than setting `terminal.integrated.allowInUntrustedWorkspace`. Instead, first-run setup asks you to trust your projects folder once: run **Workspaces: Manage Workspace Trust** and add `~/projects`. Every repo under it is then trusted.

## In the window

On activation the extension scans the launch directory for requests matching its single workspace folder. It also watches that directory, and scans again whenever the window gains focus. For each claimed request it:

1. Opens a terminal in the editor area, named after `title`, with the folder as its working directory.
2. Types the command into it. Arguments that are plain tokens, such as `claude`, flags and UUIDs, are typed as they are. Any other argument, such as an opening prompt, is passed in the terminal's environment as `REFRACT_ARG_<n>` and typed as `"$REFRACT_ARG_<n>"`. The typed line therefore stays short, and tabs, quotes or a long prompt cannot be misread by the shell. The shell stays open after `claude` exits.
3. On a request found during activation (that is, the window was opened for it), sets up the bare layout. It closes the sidebar, panel and secondary sidebar, then closes every tab that is not a terminal and has no unsaved changes. Requests that arrive later only add a terminal tab, so an existing window's layout is left alone.
