# Worktrees spec

## 1. What it does

A linked git worktree (a folder whose `.git` is a file pointing at `<main>/.git/worktrees/<name>`) no
longer shows as a second gem. It folds into its main repo's gem. The gem gets a small mark with the
number of extra checkouts. The detail panel gets a **Worktrees** section with one row per worktree:
folder, branch, a status that answers "am I done with this?", the last thing the agent said there, an
**Open** button (same as Enter on that folder) and, when nothing can be lost, **Remove**.

Trigger (2026-11-01): `~/code/studens-wide-lesson` showed as a second studens gem. Working out whether it
was finished took reading git state and an old transcript by hand (the session ended asking "release
0.13.3?" and was never answered). Refract should show that at a glance.

## 2. Decisions (Maddox, 2026-11-01)

- Fold worktrees into the main repo's gem, with a small checkout-count mark. The list lives in the detail panel.
- Clean-up: "Remove worktree" runs `git worktree remove` (never `--force`) and keeps the branch. Enabled only
  when nothing can be lost: clean (untracked files too), every commit on a remote or merged, no agent running.
  No branch delete.

## 3. Decisions made by Claude

- **The helper keeps each worktree as a repo.** It still has its own `RepoState`: status, watch, sessions,
  status hook, `prepareLaunch`. Its snapshot gains `worktreeOf` (the main work tree's path) and `worktree`
  (the facts in section 4). The **app** folds it into the main gem. This keeps launch, resume, focus and
  `sessionChanged` working unchanged.
- **The app folds a worktree only when its main repo is shown.** If the main repo is outside the roots,
  hidden, or a bare repo, the worktree stays a gem of its own, as today.
- **Worktrees outside the roots are found too.** When a known repo has linked worktrees (`<git dir>/worktrees/*`),
  the helper adds each one whose folder exists as a tracked repo. Worktrees nested inside the main folder,
  which the walk never enters, are found this way as well. A tracked worktree is dropped when it is removed
  or its folder disappears.
- **Refactor worktrees are excluded.** Anything under `~/projects/.refract-worktrees` belongs to the Refactor
  flow and its own panel section. It is never listed, counted or removable here.
- **Sessions go to the deepest repo.** A session whose cwd is inside a nested worktree belongs to the
  worktree, not to the main repo.
- **The stone shows worktree agents.** The main gem's agent state is the most urgent of the main repo's and
  its worktrees' (waiting > working > idle). A waiting agent in a worktree turns the stone red.
- **Search matches worktrees.** Search also matches worktree folder and branch names and focuses the main gem.
- **Merged means a plain ancestor.** "Merged" means HEAD is an ancestor of the default branch:
  `refs/remotes/origin/HEAD`'s target, else local `main`, else `master`. Squash and rebase merges are not
  detected and fall to "Pushed, not merged", which still allows Remove.
- **Only ignored files can go.** Ignored files are deleted by `git worktree remove`. The confirm dialog lists
  the ignored top-level entries (up to 8 plus "and N more"), so a stray `.env` isn't lost silently.
- **No Remove on Windows drives.** Worktrees on `/mnt/*` are status only in v1: no Remove, and no
  ignored-file listing (slow on drvfs; Windows-git paths in worktree metadata).
- **Tags are watched.** The helper also watches `refs/tags` in the common dir, so "In v0.13.3" appears after
  a fetch without waiting for another change.

## 4. States

Facts per worktree (helper, computed on that repo's refresh; all via git, no network):

| Fact | How |
|---|---|
| `locked` | `<worktree git dir>/locked` exists |
| `operation` | rebase-merge/, rebase-apply/, MERGE_HEAD, CHERRY_PICK_HEAD, REVERT_HEAD, BISECT_LOG in the worktree git dir |
| `changes` | tracked + untracked (non-ignored) paths. Untracked are always counted for worktrees, except on /mnt (unknown) |
| `onlyHere` | `rev-list --count --max-count=100 HEAD --not --remotes` (any remote, not just origin) |
| `merged` | default branch name when HEAD is its ancestor |
| `release` | oldest tag containing HEAD (`tag --contains HEAD --sort=creatordate`, first) |
| `detached` | HEAD not on a branch |
| `ignored` | top-level ignored entries (`status --ignored --porcelain --directory`), up to 9, not on /mnt |

Agent state comes from the worktree's own sessions and live processes (already in its snapshot).

Status: the first matching row wins.

| # | Condition | Label (row) | Remove |
|---|---|---|---|
| 1 | an agent is waiting | Agent waiting for you | no |
| 2 | an agent is working, or a live claude process runs in the folder | Agent running | no |
| 3 | `locked` | Locked | no |
| 4 | `operation` | Mid-rebase / Mid-merge / Mid-cherry-pick / Mid-revert / Bisecting | no |
| 5 | `changes` > 0 | N uncommitted changes | no |
| 6 | on /mnt with unknown untracked | Changes not checked (Windows drive) | no |
| 7 | `onlyHere` > 0 and not `merged` | N commits only on this computer (100+ shown as "100+") | no |
| 8 | `release` | In `<tag>` | yes |
| 9 | `merged` | Merged into `<branch>` | yes |
| 10 | otherwise (all on a remote) | Pushed, not merged | yes |

Rows 8–10 also show "Ready to remove" unless the worktree is on /mnt (no Remove in v1). Under the status,
the row shows the last agent message in that folder (one line, full text on hover/expand) and how long ago.
A detached HEAD adds "(detached)" after the branch.

Gem mark: shown when the repo has ≥1 folded worktree. It shows the count of extra checkouts and gets an
accent dot when any of them is ready to remove.

## 5. Data-safety rules

`removeWorktree { path }`: the helper re-checks everything at execution time and ignores the app's view.

1. `path` canonicalises to a non-main entry of `git -C <main> worktree list --porcelain`, where `<main>` is
   found from the path's own common dir. Otherwise refused.
2. Not under `~/projects/.refract-worktrees`, and no refactor record names it.
3. Not on `/mnt/*` (v1).
4. Not locked, not prunable, no `operation`.
5. `git -C <path> status --porcelain --untracked-files=all` prints nothing.
6. `onlyHere == 0`, or HEAD is an ancestor of the default branch.
7. No session in working/waiting, and no live claude process whose cwd is in the folder.
8. Runs `git -C <main> worktree remove <path>`. Never `--force`, never `rm`. Git's own clean check is a second guard.
9. Never deletes, moves or resets the branch.
10. One mutating git job per repo at a time (shares the refactor job serialisation).
11. On refusal, the reply names which rule failed, in words for the app. Nothing is changed.

## 6. Pieces

- **core:** `WorktreeFacts` + `derive_worktree_status(facts, agent) -> (Status, removable)`, a pure
  function with the table above.
- **helper:**
  - facts in `git.rs` (only for linked worktrees)
  - `worktreeOf` + `worktree` on `RepoSnapshot`
  - discovery of linked worktrees outside the walk (and exclusion of refactor ones)
  - deepest-repo session attach
  - `refs/tags` watch
  - `RemoveWorktree` method + handler
- **app:**
  - fold in `projects.ts` (and navigation search)
  - combined agent state for the stone
  - GemMark checkout mark
  - DetailPanel "Worktrees" section with Open/Remove
  - confirm dialog with the ignored list
  - `removeWorktree` in Backend / TauriBackend / DemoBackend
- **demo:** one demo repo with two worktrees, one "In v1.4.0" (ready) and one with uncommitted changes, and
  a working Remove in the demo backend.
- **docs:** architecture "Worktrees", ADR 0019, a README line.

## 7. Tests

- **core:** every row of the status table, plus precedence (e.g. waiting + dirty = waiting; merged +
  onlyHere = merged).
- **helper integration (real git, temp dirs, bare remote):**
  - facts for pushed, merged, tagged, dirty, untracked, ignored-only, locked, mid-rebase, detached and only-here
  - a refactor worktree is excluded
  - a worktree outside the roots is found
  - a nested worktree is found and gets the sessions
  - `removeWorktree` refuses for each rule in section 5 and succeeds on a clean pushed worktree (the branch
    still exists)
- **app (Vitest):**
  - folding, and the fallback when the main repo is absent
  - search
  - combined stone state
  - Worktrees section labels and Remove enabled/disabled
  - confirm lists ignored entries
- **e2e (demo):** the gem shows the mark; the panel lists both worktrees; Remove on the ready one removes it
  and the mark updates.

## 8. Out of scope (v1)

- Remove on /mnt worktrees.
- Pruning missing-folder worktree entries (`git worktree prune`).
- Branch delete.
- Squash-merge detection.
- Creating worktrees.
- Satellite stones.
- Blocking Remove for non-claude processes (e.g. an editor) open on the folder.

## 9. As built (2026-11-01)

- Rule 7 is stricter: any live session (idle too) or claude process in the folder blocks Remove; the row says to
  close the agent.
- Extra non-removable statuses: Holds another repo (a nested worktree, repo, submodule or .git in an ignored dir),
  Too many ignored files to check, and Can't tell if its commits are pushed. Also "Has a refactor".
- The helper also refuses skip-worktree/assume-unchanged edits, and runs git with `-c status.showUntrackedFiles=normal`.
- Build/dependency folders (node_modules, target, dist, build, .venv, venv, __pycache__, .next, .gradle) aren't
  searched for nested repos (budget); nested worktrees there are still caught via the worktree list.
- Refactor adoption only goes into the main checkout. Windows-git worktrees (C:/ gitdir) are skipped in discovery.

## 10. v1.1: Remove on Windows drives, clean up missing folders (2026-11-01)

Two items from §8 come in: Remove on `/mnt` worktrees, and cleaning up worktree entries whose folder is
gone. Branch delete, squash-merge detection and the rest of §8 stay out.

### 10.1 Remove on Windows drives (`/mnt/*` drvfs)

Why it was out: listing untracked and ignored files on drvfs is slow (seconds to minutes), so the
snapshot never reads them there (row 6, "Changes not checked"). v1.1 reads them **on demand**, once,
when the user asks.

- **core:** `derive_worktree_status` also returns `checkable`: on a Windows drive, untracked files not
  listed, not made by Git for Windows (the snapshot reads the same `.git`/admin `gitdir` test as rule 3,
  on Windows drives only), and the status the same facts would get with `untracked_checked = true` is
  row 8-10 (holds_repo / too_many_ignored are unknown there and left false). `removable` stays false on
  a Windows drive. With untracked files not listed, a blocker already known from the refs (Can't tell if
  its commits are pushed, N only here) comes before row 6 "Changes not checked", so the row names what
  really keeps it; `checkable` stays false then. Locked, operation and changes still come first.
- **protocol:** `WorktreeInfo.checkable: bool` (serde default false). New method
  `checkWorktree { path }` → `WorktreeCheck { status: WorktreeStatus, removable: bool, ignored: string[] }`.
  It reads the facts the way a Linux-side worktree gets them (untracked listed, ignored entries up to
  `MAX_IGNORED`, nested-repo search) on any drive, and derives with `windows_drive = false`. Edits git
  hides (assume-unchanged, skip-worktree) count as changes, and `core.fsmonitor=false` is passed to its
  status. Read-only. Refused with `WORKTREE_REFUSED` as the removal would be: a non-linked path, one made
  by Git for Windows, a refactor worktree, a prunable one. Tauri command `check_worktree`, same 5-minute
  timeout as `remove_worktree`.
- **threads:** each `checkWorktree` runs on a thread of its own, so a minutes-long check on a Windows
  drive holds up no other repo's jobs; the per-main-repo busy set still keeps it from overlapping another
  job of the same main repo. `removeWorktree` and `pruneWorktree` stay on the shared job worker: the busy
  set is keyed per main repo, but refactor jobs are keyed by the repo they were started from (which may
  be a linked worktree of the same main repo), and a removal's "has a refactor" rule reads the refactor
  records when it is asked for, so a start still in flight is only kept apart from it by the single
  worker.
- **removeWorktree { path, ignored?: string[] }.** `ignored` is the list of top-level ignored entries the
  confirm showed (the snapshot's or the check's). When it has fewer than `MAX_IGNORED` (9) entries, the
  helper refuses if the top-level ignored set it reads at execution has an entry not in it: "New ignored
  files appeared since you confirmed (<up to 3, then "and more">). Check again." on a Windows drive, and
  "… Review the list and remove again." elsewhere. With 9 or more entries (the confirm said "and more") or
  when absent (an older app), nothing is compared. Tauri `remove_worktree(path, ignored?)` passes it
  through. The snapshot keeps its ignored list while the status and HEAD are unchanged, so after this
  refusal off a Windows drive the helper reads the worktree again with its ignored entries re-read and
  sends its `repoChanged` (after the answer, before `worktreeJobDone`), and the next confirm lists them.
- **Order in removeWorktree.** Rules 3, 1, 2, prunable and "has a refactor" first; then the cheap rules
  (locked, operation, rule 6, rule 7) with HEAD recorded (`rev-parse HEAD`), so a worktree in use is
  refused at once; then the slow ones (hidden edits, the untracked status, then nested repos and the
  `ignored` comparison last, so ignored files made while the status runs are still seen); then the cheap
  rules again right before `git worktree remove`, with HEAD unchanged
  ("This worktree's HEAD moved while it was being checked. Check again."). Rule 6 uses the recorded
  commit. Rule 3 is checked before the request is queued too, so a worktree whose `.git` holds a Windows
  path gets the Git for Windows message, not "Not a linked worktree". The rule-5 status, the
  ignored listing and `git worktree remove` run with `-c core.fsmonitor=false` (an fsmonitor misses Windows writes on drvfs).
- **removeWorktree rule 3 becomes:** refuse when the worktree's `.git` file or its admin `gitdir` file
  holds a Windows path (`X:/` or `X:\`), i.e. Git for Windows made or repaired it: "This worktree was made
  by Git for Windows; remove it from Windows." Otherwise a Windows-drive worktree goes through every other
  rule unchanged (the full untracked status runs, slowly, at execution time).
- **Half-removed on Windows.** A Windows program holding a file open makes `git worktree remove` fail
  partway; git still drops the admin dir. After any failed `worktree remove` the helper re-reads the
  worktree list. If the entry is gone, the worktree is dropped (`repoRemoved`, main repo `repoChanged`; its
  watches go too) and on a Windows drive the refusal says: "Git removed the worktree, but a Windows program
  had files open, so some are still in <path>. Close it and delete the folder." Elsewhere: "Git removed the
  worktree but could not delete all of its files, so some are still in <path> (<git's first stderr line>).
  Delete the folder." Nothing is retried or deleted by Refract.
- **Event `worktreeJobDone`.** Every `removeWorktree` and `pruneWorktree` job ends with
  `{"event":"worktreeJobDone","data":{"path":"…","error":"…"}}`, `error` left out on success and set to the
  refusal's message when refused or half-removed. `path` is the path the app sent (for prune, the entry's
  path). It is sent after the answer and the answer's other events (half-removed ones included), so an app
  whose request timed out still learns the outcome. Requests refused before a job is queued (busy, not a
  linked worktree, not a known main checkout) are answered at once and get no event.
- **app:** a row with `checkable` (and no agent, no refactor) shows **Check…** where Remove… would be.
  Click → "Checking files on the Windows drive…" (button disabled) → the check's result replaces the row's
  label until that worktree's next snapshot. If `removable`, the usual confirm opens with the check's
  `ignored` list; Remove then calls `removeWorktree` with that list. A refused check shows its message like a
  refused Remove. As built: "Check again" stays offered while a result shows; a refused Remove clears the
  result. Remove/prune refusals (half-removed included) are notices kept by the Worktrees section, keyed by
  path, so they outlive the row; a timed-out request stays pending until `worktreeJobDone`. Notices live in
  the zoomed repo's panel, so zooming out drops them.

### 10.2 Missing-folder entries

Today a worktree whose folder is gone simply vanishes from the panel, while git keeps its entry (and the
branch stays checked out there, so `git switch <branch>` elsewhere fails). v1.1 lists those entries under
their main repo and offers **Clean up**, which drops only git's bookkeeping.

Facts (main repo, read with its ref facts and whenever a tracked worktree is dropped): each non-main entry of
`git worktree list --porcelain` marked `prunable` (or locked) whose path gives `NotFound` on
`symlink_metadata` (any other error, such as EACCES on a parent or a dead network mount, is not "gone" and
the entry is not listed), minus refactor worktrees (§3). The m5 ref walk is skipped when m1, m2 or m4
already holds, and its answer is kept per commit until the refs move. m3 compares only with known linked
worktrees. Per entry: `path`,
`branch` (short name, or none when detached), `locked`, and the derived status.

Status, first match wins:

| # | Condition | Label | Clean up |
|---|---|---|---|
| m1 | `locked` | Folder is gone · Locked | no |
| m2 | path under `/mnt/`, `/media/` or `/run/media/`, and no mount holds it other than `/` and the bare parent dirs `/mnt`, `/mnt/wsl`, `/media`, `/run`, `/run/media`, `/run/media/<user>` (by `/proc/mounts`; unreadable → counts as not attached) | Folder is gone · Drive not attached | no |
| m3 | a repo Refract knows has this entry's admin dir as its git dir (the folder was moved) | Moved to `<path>` (hint: run `git worktree repair` there) | no |
| m4 | the entry has its own refs (anything under `<admin>/refs/`, e.g. bisect or `refs/worktree/*`) | Folder is gone · Has its own refs | no |
| m5 | detached, and no branch, tag or remote ref contains its HEAD (`for-each-ref --contains <head> --count=1` prints nothing) | Folder is gone · Its commits would be lost | no |
| m6 | otherwise | Folder is gone | yes |

- **protocol:** `RepoSnapshot.missingWorktrees: MissingWorktree[]` (serde default empty, skipped when
  empty), `MissingWorktree { path, branch?: string, status: MissingWorktreeStatus, cleanable: bool }`,
  `MissingWorktreeStatus` tagged by `kind`: `locked | driveNotAttached | moved { to } | ownRefs |
  commitsOnlyHere | gone`. New method `pruneWorktree { repo, path }` (`repo` = main repo's snapshot path,
  `path` = the entry's path as listed) → `null`. Tauri command `prune_worktree`.
- **pruneWorktree data-safety rules** (re-checked at execution, app's view ignored; refusals use
  `WORKTREE_REFUSED` with the rule in words; nothing changed on refusal):
  1. `repo` is a known non-bare main repo and `path` is a non-main, `prunable` entry of its
     `git worktree list --porcelain`, matched by the listed path.
  2. Not a refactor worktree (refactor dir or a refactor record).
  3. `path` does not exist: `symlink_metadata` fails with `NotFound`. If it reappeared: "The folder is
     back." Any other error: "Can't tell whether the folder is there: <error>."
  4. m1-m5 all false.
  5. Checks rule 3 again immediately before, then runs `git -c status.showUntrackedFiles=normal -c
     core.fsmonitor=false -C <repo> worktree remove <path>` (no `--force`; on a missing folder git only
     deletes the admin dir, checked on git 2.43). Never `worktree prune` (it drops every entry at once),
     never `rm`. Ends with `worktreeJobDone` (§10.1).
  6. Never deletes, moves or resets the branch. One job per repo, shared with the other git jobs.
  On success the main repo's snapshot is sent again without the entry.
- **app:** the Worktrees section appears when a repo has folded worktrees or missing entries. Missing rows
  come after the live ones: folder (muted), branch, label, and **Clean up…** when `cleanable`. Its inline
  confirm reads "Forget this worktree? Only git's record of it goes; branch `<b>` stays." with Clean up /
  Keep it. The gem mark is unchanged (it counts checkouts that exist).
- **demo:** the demo repo with worktrees gains a `/mnt/c/...` worktree (checkable; its check comes back
  `Pushed` with ignored `node_modules/`) and one missing entry (cleanable, branch kept), both working in
  DemoBackend.

### 10.3 Tests

- core: `checkable` for each row (true only for Windows-drive + otherwise rows 8-10); the missing-entry
  derivation for m1-m6 and precedence; the mount rule from `/proc/mounts` text.
- helper (real git, `TEST_SLOW_PREFIX` stands in for drvfs): snapshot of a slow-prefix worktree is
  `unchecked` + `checkable`; `checkWorktree` finds untracked, ignored and a nested repo; `removeWorktree`
  succeeds on a clean slow-prefix worktree and refuses one with a Windows `gitdir`; missing entries listed
  for a deleted folder; `pruneWorktree` refuses locked, moved, own refs, detached-only, folder back, refactor,
  and succeeds keeping the branch.
- v1.1 review fixes: core, a known blocker before "Changes not checked" and Git-for-Windows never
  checkable; the m2 rule for `/media` and `/run/media`; helper, an unreadable parent (mode 000) is not
  gone in the listing or in prune, HEAD moved between the checks refuses (unit, real git), the `ignored`
  comparison (unit and helper), `worktreeJobDone` after success and refusal of both jobs and none for a
  request-time refusal, `checkWorktree` counting a skip-worktree edit and refusing a Windows-`gitdir`
  worktree whose snapshot is not checkable; protocol shapes of the event and the `ignored` param.
- app (Vitest): Check… flow (busy, result label, confirm with check's ignored, refusal); missing rows,
  labels, Clean up confirm and call.
- e2e (demo): Check → Remove on the `/mnt` worktree; Clean up on the missing entry.
