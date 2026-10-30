# Move to WSL: spec

Status: specified 2026-10-30, not started. Linked from `refract-spec.md` §12.

## 1. What it is

Repos on a Windows drive (seen in WSL as `/mnt/c/...`) work badly with agents: git is slow over drvfs, inotify misses edits made from Windows, file modes are wrong and line endings are CRLF. **Move to WSL** copies such a repo onto the WSL Linux filesystem (for example `C:\Users\dev\Desktop\aurora-notes` → `~/projects/aurora-notes`). It checks the copy exhaustively, and only after you confirm does it send the Windows original to the Recycle Bin.

What it promises: deleting the original never loses anything except the regenerable build folders it listed. If any check fails, nothing is deleted.

## 2. User experience

### 2.1 Entry point
- Gems for repos on a Windows drive get a small marker. Its tooltip says "On a Windows drive: git and agents are slower here."
- The detail panel shows **Move to WSL…** for those repos, one repo at a time.

### 2.2 Dialog (preflight)
The dialog opens after a quick preflight (§4.1) and shows:
- **Destination:** defaults to the first Linux scan root plus the folder name (`~/projects/aurora-notes`) and can be edited. If the folder exists, it suggests `aurora-notes-2`. It never merges into an existing folder.
- **Size:** what will be copied, and the build folders that will be skipped, with their sizes (§3.2).
- **Install:** the command that will run afterwards (for example `npm ci`, `uv sync`), or a warning that the tool is missing in WSL.
- **History:** "N Claude sessions will move with it."
- **Warnings:** the repo looks Windows-tool-specific (`.sln`, `.vcxproj`, a Unity `ProjectSettings/`), so Windows tools on `\\wsl.localhost` will be slow or broken.
- **Busy:** each VS Code window that has the folder open gets a **Close** button (any unsaved-file prompt still appears). Each agent on the repo that is finished or waiting on you gets a **Stop** button, after a confirmation. An agent that is mid-turn can't be stopped: its button is disabled with "agent is working". **Start** stays disabled until nothing is busy.
- **Refusals** (§4.1): shown with their reasons instead of **Start**.

### 2.3 While it runs
- The work runs in the background. The gem shows a progress ring, and the panel shows the current step ("Copying 1.2 / 3.4 GB", "Hashing", "Checking git", "Installing").
- You can hide Refract to the tray. A Windows notification appears when the migration is ready for your OK, or when it fails.
- **Cancel** is available until the report: it deletes the temporary copy and leaves the original untouched.

### 2.4 Report and final OK
The report lists every check (§5) with its result, the skipped build folders, the install result and log, and the history that moved. It offers three actions:
- **Delete original** (default): §6.
- **Keep both:** the original stays on Windows and Refract hides its gem.
- **Discard the copy:** the WSL copy is deleted and everything stays as it was.

If a check fails, the report says which check, on which files, and what to do. Only **Discard the copy** and **Retry** are offered.

Reports are saved to `~/.local/share/refract/migrations/<timestamp>-<name>.json` (with the install log next to it).

## 3. What gets copied

### 3.1 Everything, byte for byte, except build folders
- All files and folders, including `.git` (byte for byte; §3.3), untracked files and ignored files such as `.env`.
- mtimes are preserved. Files get mode 644 and folders 755. Tracked files get their executable bit from the git index during re-checkout (§3.3).
- Alternate data streams (for example `Zone.Identifier`) are not copied. Each one dropped is listed in the report. They never block the migration.

### 3.2 Skipped build folders
A folder is skipped only when **git ignores it** (tracked content is never skipped) **and** it matches a rule:
- `node_modules`; `.venv`, `venv`, `env` (only if it contains `pyvenv.cfg`); `__pycache__`, `.pytest_cache`, `.mypy_cache`, `.ruff_cache`, `.tox`
- `target` (only if it has `CACHEDIR.TAG` or a `Cargo.toml` next to it); `.next`, `.nuxt`, `.turbo`, `.parcel-cache`, `.svelte-kit`, `.gradle`
- `dist`, `build`, `out` (only when ignored, as above)

The rules live in `refract-core` with tests. Skipped folders are listed in the dialog and the report with their sizes, and they go to the Recycle Bin with the original.

### 3.3 Line endings and git config
Windows git usually checks files out with CRLF (`core.autocrlf=true`) and `core.filemode=false`. The copy is made to look like a Linux checkout:
1. Copy `.git` byte for byte.
2. For each tracked file, compute the blob Windows git would store for it (the "clean" form, using the Windows settings) and write it into the copy's object store. These are new loose objects; nothing existing changes.
3. Rewrite the working-tree file as Linux git checks that blob out (`git cat-file --filters`, using the repo's `.gitattributes`, `core.autocrlf` unset). That gives LF unless `.gitattributes` asks for otherwise.
4. In the copy's local config, remove the Windows-only keys (`core.autocrlf`, `core.filemode=false`, `core.symlinks=false`, `core.ignorecase=true`) and set `core.filemode=true`. Then run `git update-index --refresh`.

Uncommitted edits carry over as the same diffs, because they're compared as blobs (§5, check 1). Staged changes survive because the index is copied, not rebuilt.

### 3.4 Session history
Claude Code keeps transcripts in `~/.claude/projects/<encoded cwd>/`. They're copied to the folder for the new path, and every `cwd` field equal to the old path, or under it, is rewritten to the new path. The rewrite goes line by line through a JSON parse, and no other field changes. To verify: the same number of lines, every line parses, and each line differs only in `cwd`. The old transcripts are deleted after **Delete original**, because the old path is gone. The usage widget already dedupes messages across files, so the copies are never counted twice.

Refract never edits `~/.claude.json`, so Claude Code asks for folder trust once at the new path.

### 3.5 Install
The install runs after the checks pass, inside the destination, through a login shell (`bash -lc`) so nvm and similar tools are found. It only runs for skipped folders that need it:
- `node_modules` → `npm ci` / `pnpm install --frozen-lockfile` / `yarn install --immutable` (Berry) or `--frozen-lockfile` (v1) / `bun install`, picked by the lockfile
- `.venv` → `uv sync` (`uv.lock`) / `poetry install` (`poetry.lock`) / `python3 -m venv .venv && .venv/bin/pip install -r requirements.txt`
- If the tool is missing in WSL, the report gives the command to install it. If the install fails or runs past 15 minutes, the log is shown, but the migration doesn't fail.

## 4. Steps

### 4.1 Preflight (refuse without changing anything)
- The source isn't a git repo on a Windows drive.
- The source is a linked worktree, or has linked worktrees (`.git/worktrees/*`).
- A merge, rebase, cherry-pick, revert or bisect is in progress. The dialog asks you to finish or abort it first.
- A symlink, junction or other reparse point points outside the repo.
- A file can't be opened for reading (locked by another program).
- The repo uses Git LFS and WSL has no `git-lfs`. The dialog gives the install command.
- The destination isn't on the Linux filesystem, or free space there is less than 1.2× the copy size plus 1 GB.

Busy windows and agents don't refuse. They block **Start** (§2.2).

### 4.2 Snapshot (Windows side, native NTFS)
The app (Windows side) builds the source manifest **S0**: for every file and folder outside skipped folders, including `.git`, it records the relative path, type, size, mtime, attributes and SHA-256. For each tracked file it also records the clean blob id, from `git.exe hash-object --stdin-paths`.

Git snapshot **G0**, also from `git.exe`:
- `rev-parse HEAD`
- `for-each-ref` (every ref, including `refs/stash`), the stash list, remotes and upstreams
- `config --local -l`
- `status --porcelain=v2 -z --untracked-files=all --ignored=matching`
- `submodule status --recursive`
- `fsck --full` result

If `git.exe` is missing, the helper's Linux git is used on the `/mnt` path with `-c core.autocrlf=true -c core.filemode=false`, and the report says so.

### 4.3 Copy and normalise (helper, WSL)
The helper copies into `<dest parent>/.refract-migrate-<name>-<id>` on the same filesystem as the destination, then normalises per §3.3. Submodules are handled recursively (`.git/modules`, nested working trees). LFS objects in `.git/lfs` are copied. Hashing the destination runs in the helper while the app hashes the source, so each side reads its own disk natively.

### 4.4 Verify (§5), then put it in place
The temporary folder is renamed to the destination only after every check passes. The rename is atomic because both are on the same filesystem. Install (§3.5) and history (§3.4) follow, then the report (§2.4).

### 4.5 Interruption
If the app, the helper or WSL stops mid-migration, the helper deletes any leftover `.refract-migrate-*` folders at startup, and the repo is simply still on Windows. A migration that was already put in place but not yet confirmed is kept in a state file, so its report reappears on the next start.

## 5. Checks (all required before Delete original)

1. **Every file matches.** Every S0 path exists in the copy with the same type. Untracked and ignored files must match in SHA-256 and size. Tracked files must match by blob id: Linux `git hash-object --path` of the copy equals the Windows clean blob id in S0. File counts and total sizes match. The copy has no extra files except the new loose objects from §3.3, which must all pass fsck.
2. **Git state is identical.** Every `.git` file is byte-identical except `index` (whose `ls-files -s --stage` output must be identical), `config` (only the expected key changes from §3.3) and the new loose objects. HEAD, all refs and the stash match G0. `git status --porcelain=v2` in the copy equals G0's (the ignored set excludes the skipped folders). `fsck --full` reports no new problems compared with G0. Submodules are checked recursively. With LFS, `git lfs fsck` must pass.
3. **Nothing skipped without you knowing.** Any copy error fails this check, and so does any path that couldn't be represented. The only things allowed to be missing are the listed skipped folders and the listed alternate data streams.
4. **Nothing touched it during the copy.** After verifying, the source is snapshotted again (stat of every file, plus refs and the index hashed). Changed files are re-copied and checked again, up to 3 rounds; after that the check fails with "the folder keeps changing". No VS Code window or agent may have the folder open at the end.
5. **History copied** (§3.4 rules). This doesn't block deletion of the repo, but if it fails, the old transcripts are not deleted.

## 6. Delete original
1. Run check 4 again right then, because time has passed while the report waited for you: stat every file against the final snapshot and confirm nothing has the folder open. If anything changed, refuse and offer **Re-sync**, which re-copies and re-checks the changes.
2. Send the folder to the Recycle Bin with `IFileOperation` (`FOFX_RECYCLEONDELETE`, no UI). Beforehand, check that the drive's Recycle Bin can hold it (bin size limits from `SHQueryRecycleBin` and the drive's quota). If it can't, ask "This won't fit in the Recycle Bin. Delete permanently?" with **No** as the default. If you choose No, the result is the same as **Keep both**.
3. Create `<name>.lnk` at the old location, pointing to `\\wsl.localhost\<distro>\<dest path>`.
4. Delete the old transcripts (§3.4). Update the settings and `agent.toml` entries (hidden list, `extra`) that point at the old path.
5. The gem moves to the new path, keeping its remote match by origin.

## 7. Where the code goes
- `refract-core`: build-folder rules, manifest diffing, the `cwd` rewrite, refusal rules, report types (unit tests with fixtures).
- Helper: preflight (Linux side), copy, normalise, hash, git checks, install, history, temp cleanup, state file. Protocol: `migrate {path, dest}` → `migrateProgress` events → `migrateReady {report}`; `migrateFinish {id, action: delete|keep|discard}`; `migrateCancel {id}`.
- App (Windows): S0/G0 snapshot with `git.exe`, reparse-point and lock checks, Recycle Bin, `.lnk`, closing VS Code windows, stopping idle agents, notifications, dialog, gem ring and report UI. DemoBackend simulates a migration for the demo site.

## 8. Testing
- Unit tests: the build-folder rules, manifest diffs, `cwd` rewrite edge cases, refusal rules.
- Helper integration tests on temporary repos with CRLF + autocrlf, staged and unstaged edits, untracked and ignored files, stashes, submodules and an executable script.
- Fault injection: edit the source mid-copy, corrupt a copied byte, drop a ref, make a file unreadable, add an external junction. Each must block deletion with the right message.
- Windows tests (CI windows runner): the Recycle Bin path, including the case where it doesn't fit, and `.lnk` creation.
- Live: an invented scratch repo on the Windows Desktop, migrated end to end, then restored from the Recycle Bin to confirm it's intact.

## 9. Open questions
- Should the migration also offer to update the `origin` of other local clones? No for now.
- Should it move back to Windows? No; out of scope.
