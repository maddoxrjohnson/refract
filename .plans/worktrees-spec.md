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
