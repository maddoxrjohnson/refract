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
- mtimes are preserved. Folders get mode 755. drvfs can't report the Windows side's executable bit reliably, so files get 644 except: tracked files take their mode from the git index during re-checkout (§3.3); hooks (files directly in a git folder's `hooks/` not ending in `.sample`) and untracked or ignored files starting with `#!` get 755.
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
3. Rewrite the working-tree file as Linux git checks that blob out (`git cat-file --filters`, using the repo's `.gitattributes`, `core.autocrlf` unset). That gives LF unless `.gitattributes` asks for otherwise. A file is rewritten only when its bytes are exactly what Windows git checks the blob out as (`git checkout-index --temp` from a scratch index, with the Windows settings), so nothing the blob doesn't hold is lost. Any other tracked file (a clean filter that drops content, mixed line endings) is copied byte for byte; check 1 then fails on it, with advice, unless Linux git stores the same blob.
4. In the copy's local config, remove the Windows-only keys (`core.autocrlf`, `core.filemode=false`, `core.symlinks=false`, `core.ignorecase=true`) and set `core.filemode=true`. Then run `git update-index --refresh`. Rewritten entries keep their skip-worktree and assume-unchanged flags, and intent-to-add entries stay intent-to-add.

Uncommitted edits carry over as the same diffs, because they're compared as blobs (§5, check 1). Staged changes survive because the index is copied, not rebuilt.

### 3.4 Session history
Claude Code keeps transcripts in `~/.claude/projects/<encoded cwd>/`. They're copied to the folder for the new path, and every `cwd` field equal to the old path, or under it, is rewritten to the new path. Folder names are lossy (`/mnt/c/dev/app/web` and `/mnt/c/dev/app-web` share one), so each transcript moves only when its own first `cwd` lies in the old path; files in a session's folder follow the session, and other files of a project folder move only when all its sessions do. Each copy records the original's size and SHA-256. The rewrite goes line by line through a JSON parse, and no other field changes. To verify: the same number of lines, every line parses, and each line differs only in `cwd`. The old transcripts are deleted after **Delete original**, because the old path is gone: only those unchanged since they were copied. One that changed (the session went on in the old path) is copied again when nobody wrote to its copy, and kept otherwise. **Discard the copy** deletes only copies nobody wrote to since. The usage widget already dedupes messages across files, so the copies are never counted twice.

Refract never edits `~/.claude.json`, so Claude Code asks for folder trust once at the new path.

### 3.5 Install
The install runs after the checks pass, inside the destination, through a login shell (`bash -lc`) so nvm and similar tools are found. It only runs for skipped folders that need it:
- `node_modules` → `npm ci` / `pnpm install --frozen-lockfile` / `yarn install --immutable` (Berry) or `--frozen-lockfile` (v1) / `bun install`, picked by the lockfile
- `.venv` → `uv sync` (`uv.lock`) / `poetry install` (`poetry.lock`) / `python3 -m venv .venv && .venv/bin/pip install -r requirements.txt`
- If the tool is missing in WSL, the report gives the command to install it. If the install fails or runs past 15 minutes, the log is shown, but the migration doesn't fail.
- The copy is statted before and after the install (skipped folders left out). Files it created, and files from the original it changed or removed (for example `package-lock.json` written, `package.json` rewritten), are listed in the report as changed by install. Later re-syncs allow exactly those differences, until the original's own version is copied over them again.
- The original's version of each file the install changed or removed is backed up to `~/.local/share/refract/migrations/<timestamp>-<name>.backup/`, at the same relative path, checked against the snapshot's SHA-256; the report gives the folder. A file that can't be backed up fails check 3, so deleting the original never loses it.
- After the install the copy is statted again (each repo's index by its entries), and that is the copy "as checked" that §6 compares against.

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
The app (Windows side) builds the whole snapshot before it sends `migrate`; nothing is hashed in parallel with the copy. It builds the source manifest **S0**: for every file and folder outside skipped folders, including `.git`, it records the relative path, type, size, mtime, attributes and SHA-256. For each tracked file it also records the clean blob id, from `git.exe hash-object --stdin-paths`.

Git snapshot **G0**, also from `git.exe`:
- `rev-parse HEAD`
- `for-each-ref` (every ref, including `refs/stash`), the stash list, remotes and upstreams
- `config --local -l`
- `status --porcelain=v2 -z --untracked-files=all --ignored=matching`
- `ls-files -s -z` (index entries) and `ls-files -v -z` (skip-worktree and assume-unchanged flags)
- `submodule status --recursive`
- `fsck --full` result

If `git.exe` is missing, the helper's Linux git is used on the `/mnt` path with `-c core.autocrlf=true -c core.filemode=false`, and the report says so.

### 4.3 Copy and normalise (helper, WSL)
The helper copies into `<dest parent>/.refract-migrate-<name>-<id>` on the same filesystem as the destination, then normalises per §3.3. Submodules are handled recursively (`.git/modules`, nested working trees). LFS objects in `.git/lfs` are copied. The helper hashes only the copy, after normalising; the source was hashed by the app before `migrate` (§4.2).

### 4.4 Verify (§5), then put it in place
The temporary folder is renamed to the destination only after every check passes. The rename is atomic because both are on the same filesystem. Install (§3.5) and history (§3.4) follow, then the report (§2.4).

### 4.5 Interruption
If the app, the helper or WSL stops mid-migration, the helper deletes any leftover `.refract-migrate-*` folders at startup, and the repo is simply still on Windows. A migration that was already put in place but not yet confirmed is kept in a state file, so its report reappears on the next start. A step after placement (install, history, re-sync) is saved as running before it starts, so one cut short is reported as such (§6.1); a cancel or a panic after placement never deletes the copy either.

## 5. Checks (all required before Delete original)

1. **Every file matches.** Every S0 path exists in the copy with the same type. Untracked and ignored files must match in SHA-256 and size. Tracked files must match by blob id: Linux `git hash-object --path` of the copy equals the Windows clean blob id in S0. File counts and total sizes match. The copy has no extra files except the new loose objects from §3.3, which must all pass fsck.
2. **Git state is identical.** Every `.git` file is byte-identical except `index` (whose `ls-files -s` output and `ls-files -v` flags must be identical), `config` (only the expected key changes from §3.3) and the new loose objects. HEAD, all refs and the stash match G0. `git status --porcelain=v2` in the copy equals G0's (the ignored set excludes the skipped folders). `fsck --full` reports no new problems compared with G0. Submodules are checked recursively. With LFS, `git lfs fsck` must pass.
3. **Nothing skipped without you knowing.** Any copy error fails this check, and so does any path that couldn't be represented, and any file the install changed whose original version couldn't be backed up (§3.5). The only things allowed to be missing are the listed skipped folders and the listed alternate data streams. A submodule whose git folder lies outside the copy stops the copy here, before git touches it.
4. **Nothing touched it during the copy.** After verifying, the source is snapshotted again (stat of every file, plus refs and the index hashed). Changed files are re-copied and checked again, up to 3 rounds; after that the check fails with "the folder keeps changing". A path that can't be read (a folder that can't be listed, a ref that can't be opened) fails this check: it is never taken for removed, so nothing leaves the copy because of it. When a repo's index changed, the whole repo is normalised again and files it no longer tracks are copied again as they are. No VS Code window or agent may have the folder open at the end.
5. **History copied** (§3.4 rules). This doesn't block deletion of the repo, but if it fails, the old transcripts are not deleted.

## 6. Delete original
1. Run check 4 again right then, because time has passed while the report waited for you (`migrateBeginDelete`): stat every file against the final snapshot, confirm nothing has the folder open, and stat the placed copy against its state after the install. Files modified or added in the original, paths of it that can't be read, any change to the copy (or the copy gone) and running agents refuse. Files removed from the original don't: the copy still has them. If the original changed, offer **Re-sync**, which re-copies and re-checks the changes and removes from the copy only paths verifiably gone from the original. If the copy changed, it may hold your work, so it is neither re-synced over nor discarded: only **Keep both**. The app sends what Windows sees: when WSL can't see the original but Windows can, its drive is hidden from WSL, nothing of it was checked, and the deleting state is not entered (the dialog offers **Try again** and **Keep both**). When the check passes, the migration is marked *deleting* in its state file before anything is deleted. From then on, re-sync, keep and cancel are refused, and discard unless the copy is gone; only finishing the delete ends it, after a crash or restart too. Running the check again then passes for a partly deleted original (it shows only removals), so the rest can go. While deleting (and when the original is gone on both sides), a change to the copy only blocks for a path the original still has: a file added to the copy, or one whose original is already gone, loses nothing when the rest goes. For the paths that do block, **Delete the rest anyway** lists them in two groups, changed in the copy (the copy keeps its version) and deleted in the copy (deleting the original removes these for good), and goes on for exactly those paths (the original's version goes to the Recycle Bin, or for good if the user then says **Yes, delete permanently**, which carries the same paths); any other copy change still blocks, and it never skips changes in the original, unreadable paths (in either), running agents, a hidden drive or a copy that is gone. A copy that can't be read is told apart from one that is gone: the dialog explains it and offers **Keep both** (or, deleting, **Finish deleting** once it can be read). A copy deleted by hand while deleting stops the delete for good (finishing the delete is refused too): the dialog says the deleted part of the original is in the Recycle Bin and the rest still on disk, and **End the move** (after asking) only forgets it. If the original folder is already gone, the Recycle Bin step is skipped, and re-sync and discard are refused, since the copy is the only one left (with the copy gone too, discard only forgets the move). Check whether the folder fits in the Recycle Bin, and ask about deleting permanently, before this step, since **No** (Keep both) is refused once deleting.
2. Send the folder to the Recycle Bin with `IFileOperation` (`FOFX_RECYCLEONDELETE`, no UI). Beforehand, check that the drive's Recycle Bin can hold it (bin size limits from `SHQueryRecycleBin` and the drive's quota). If it can't, ask "This won't fit in the Recycle Bin. Delete permanently?" with **No** as the default. If you choose No, the result is the same as **Keep both**.
3. Create `<name>.lnk` at the old location, pointing to `\\wsl.localhost\<distro>\<dest path>`.
4. Delete the old transcripts that are unchanged since they were copied (§3.4); a changed one is copied again first, or kept and named in the saved report. Update the settings and `agent.toml` entries (hidden list, `extra`) that point at the old path.
5. The gem moves to the new path, keeping its remote match by origin.

### 6.1 Every state has a way forward
What the dialog offers in each state (original as Windows and WSL see it, copy as the helper sees it). Each row has at least one action that is safe and that the helper accepts; actions the helper would refuse are not offered, and a refused answer brings the pending state back from the helper.

| State | Original | Copy | Offered |
|---|---|---|---|
| Copying, before placement | any | temporary | **Cancel the move**: deletes the temporary copy |
| Install, history or re-sync running (copy placed) | any | placed | **Stop and keep the copy**: the step stops, a report says so |
| Report failed, never placed | there | none | **Discard the copy** (forgets), **Retry** |
| Report failed, placed (re-sync failed or stopped) | there | as checked | **Re-sync**, **Keep both**, **Discard the copy** |
| Report failed, placed, only an install backup failed | there | as checked | **Keep both**, **Discard the copy** (a re-sync can't clear it) |
| Report, after an install cut short with changes in the copy | there | changed while the install was interrupted (listed) | **Delete original** when the report passed, **Keep both** (discard and re-sync are refused while those changes are there; the original's versions are backed up) |
| Report failed, placed | there | changed | **Keep both** (re-sync and discard are refused, with why) |
| Report failed, placed | gone | there | **Keep the copy** |
| Report passed | there, unchanged | as checked | **Delete original**, **Keep both**, **Discard the copy** |
| Delete original refused | changed | as checked | **Re-sync**, **Keep both** |
| Delete original refused | unreadable, or agents running | as checked | **Try again** (and **Stop** for idle agents), **Keep both** |
| Delete original refused | any | changed | **Keep both** |
| Delete original refused | any | can't be read (not gone) | **Keep both**, or **Try again** once it can be read |
| Delete original refused | VS Code windows open | any | **Close** each, **Try again**, **Back** |
| Delete original refused | won't fit the Recycle Bin | any | **No** (Keep both), **Yes, delete permanently** |
| Delete original refused | there on Windows, hidden from WSL | any | **Try again** once the drive is visible, **Keep both** |
| Report passed | gone on both sides | there (changes ignored) | **Finish the move** |
| Not deleting | any | gone | **End the move**: only forgets it |
| Deleting | partly there, only removals | as checked, or only additions and changes to paths already gone | **Finish deleting** |
| Deleting | changed, unreadable, or agents running | any | **Finish deleting** once fixed (move the files out, close programs, **Stop** agents) |
| Deleting | partly there | changed or deleted for paths the original still has | **Delete the rest anyway** (lists them in two groups; acts on exactly those), or **Finish deleting** after undoing them |
| Deleting | partly there | a path can't be read | **Finish deleting** once it can be read |
| Deleting | there on Windows, hidden from WSL | any | **Finish deleting** once the drive is visible, or after deleting the rest by hand |
| Deleting | VS Code windows open, or won't fit | any | **Close** and **Finish deleting**; **Not now** or **Yes, delete permanently** |
| Deleting | gone | there | **Finish deleting** (finishes the move; after a config write that failed, the same button retries) |
| Deleting | any | gone | **End the move**, after asking; the deleted part is in the Recycle Bin (also when the copy went just before the finish: **Finish deleting** leads here) |

After a restart: a migration cut short during the install comes back as a report with the install stopped, what changed in the copy listed apart as changed while the install was interrupted (it may include your own edits) and backed up; during the history, with the history check failed and the copies already written found, so discarding deletes them; during a re-sync, with check 4 failed (**Re-sync**, **Keep both**, **Discard the copy**); deleting, still deleting; finished but with the config not yet written, as its report again, to answer again. A report the restarted helper no longer has (a failed one never placed) is dropped from the dialog and the app.

## 7. Where the code goes
- `refract-core`: build-folder rules, manifest diffing, the `cwd` rewrite, refusal rules, report types (unit tests with fixtures).
- Helper: preflight (Linux side), copy, normalise, hash, git checks, install, history, temp cleanup, state file. Protocol (details in `docs/architecture.md`):
  - `migrate {path, dest, snapshot}` → `{id}`, then `migrateProgress` events and `migrateReady {id, report}`
  - `migrateRecheck {id}` → `{changed, removed, unreadable, copyChanged, copyRemoved, copyUnreadable, copyMissing, busyAgents, sourceMissing}` (§6 step 1 without changing anything); refused unless the report passes or the migration is deleting
  - `migrateBeginDelete {id, sourceGone, force}` → `{ok, recheck}`: the same check; when `changed`, `unreadable`, `copyChanged` and `busyAgents` are empty, the migration is marked deleting (saved) and `ok` is true. Never `ok` when WSL can't see the original and `sourceGone` is false. While deleting, `copyChanged` lists only paths the original still has, and `force` (the list of those the user saw) skips exactly those, never unreadable ones. Idempotent, and still answered while a finish delete is finishing
  - `migrateResync {id}` → `null`, then progress and a fresh `migrateReady`; refused while deleting, when the original folder is gone, when the copy changed, or while the report's `recoveredChanges` is not empty
  - `migrateFinish {id, action: delete|keep|discard}`: `delete` only while deleting, refused when the copy is gone; `keep` refused while deleting or when the copy is gone; `discard` refused while deleting, when the original folder is gone, the copy changed or `recoveredChanges` is not empty, except that with the copy gone it only forgets the move. The state file stays until the config is written. `migrateCancel {id}` → `{ended}` while running: before placement it deletes the temporary copy (`ended`); once placed, and during a re-sync, the step stops and a report follows
  - `migratePending` → `[{id, report, deleting, copyMissing}]` (reports waiting, including after a restart)
  - `stopAgent {path, sessionId}` → `null` (SIGTERM an idle session)
  - Errors: `6` a migration could not start, or is unknown or not at that step; `7` `stopAgent` named a working session.
- App (Windows): S0/G0 snapshot with `git.exe`, reparse-point and lock checks, Recycle Bin, `.lnk`, closing VS Code windows, stopping idle agents, notifications, dialog, gem ring and report UI. DemoBackend simulates a migration for the demo site.

## 8. Testing
- Unit tests: the build-folder rules, manifest diffs, `cwd` rewrite edge cases, refusal rules.
- Helper integration tests on temporary repos with CRLF + autocrlf, staged and unstaged edits, untracked and ignored files, stashes, submodules and an executable script.
- Fault injection: edit the source mid-copy, corrupt a copied byte, drop a ref, make a file unreadable, add an external junction, a clean filter that drops content, staging during and after the copy, an unreadable folder after the report, the copy edited or deleted after the report, the original deleted by hand, a partly deleted original, a submodule pointing outside the copy, a destination reaching `/mnt` through a link, a project-folder name collision and a session that goes on after the copy. Each must block deletion (or keep the data) with the right message.
- Windows tests (CI windows runner): the Recycle Bin path, including the case where it doesn't fit, and `.lnk` creation.
- Live: an invented scratch repo on the Windows Desktop, migrated end to end, then restored from the Recycle Bin to confirm it's intact.

## 9. Open questions
- Should the migration also offer to update the `origin` of other local clones? No for now.
- Should it move back to Windows? No; out of scope.
