# Refactor a repo with one button (spec)

Status: agreed with Maddox in an interview on 2026-10-31. Not started.
Prerequisite: the status hook marks `AskUserQuestion` / `ExitPlanMode` as waiting (branch
`tidy-statuses-spin`), so plan approval turns the stone red.

## 1. What it does

On a zoomed repo, **Refactor** (panel button, or **Alt+R**) opens a small dialog. Confirming it:

1. creates a git worktree of the repo on a new branch, starting from the repo's last commit;
2. opens that worktree in its own VS Code window (Refract profile) and starts Claude there in
   **plan mode** with a built-in refactor prompt;
3. Claude surveys the repo, sets up dependencies, runs the checks for a baseline, and proposes a
   plan of **up to 3** behaviour-preserving refactors; Maddox approves (or edits) the plan in the terminal;
4. Claude makes one commit per refactor, re-running the checks after each and reverting any refactor
   that breaks a check that passed in the baseline;
5. the repo's panel shows the result (commits, files changed, checks before/after) with
   **Review in VS Code**, **Merge into `<branch>`** and **Discard**.

Nothing is pushed. One refactor per repo at a time.

## 2. Decisions (Maddox, 2026-10-31)

| Topic | Decision |
|---|---|
| Autonomy | Visible terminal, plan mode first; Maddox approves the plan. |
| Where changes go | New worktree + branch; Maddox's working copy and running agents untouched. |
| Scope | Agent picks the highest-value refactors, up to 3 per run. |
| Busy repo | Allowed with a running agent and with uncommitted changes; the dialog shows a one-line note ("Starts from a1b2c3d. Your 3 uncommitted files aren't included."). |
| Remote-only repos | No Refactor button; the panel says "Clone it first". |
| Worktree visibility | Not its own stone. Parent stone gets a refactor mark; its panel shows the refactor. |
| Finish line | Commits on the branch; Maddox reviews; Merge or Discard from the panel. |
| Checks | Find the repo's test/lint commands; baseline first; after each refactor; revert a refactor that breaks a passing check. |
| Prompt | Built-in prompt plus an optional "Focus on…" line in the dialog. |
| Window | Its own VS Code window on the worktree. |
| Merge when the branch moved on | Rebase the refactor branch onto it, then fast-forward. On conflict: abort, offer **Fix with Claude**. |
| Merge with uncommitted changes | Allowed unless an uncommitted file is one the refactor changed; then list the clashing files. |
| Concurrency | One refactor per repo; while one exists the button reads **Go to the refactor**. |
| Dependencies | The agent installs them in the worktree (prompt step); Refract copies or links nothing. |
| Model | Maddox's Claude default (no `--model`). |
| Alerts | Refactor mark on the stone (red when it needs Maddox, "ready" when done) + Windows toast when the plan is ready and when the refactor finishes, also while hidden. |
| Discard | Confirm "Discard N commits?", then delete worktree and branch. Disabled while the agent is live. |
| Review | Panel summary + **Review in VS Code** (raises the worktree window). No diff viewer in Refract. |
| Hook | Not required. Without it the dialog notes that alerts need the hook and offers to install it. |
| Extras in v1 | Demo-site simulation, Alt+R, **Resume refactor** after an interruption. |

## 3. Decisions made by Claude (tell Maddox; he may change them)

- **Worktree location:** `~/projects/.refract-worktrees/<repo>-<yyyymmdd-hhmm>`. It sits inside the
  `~/projects` folder VS Code already trusts, so no trust prompt; the helper's scan already skips
  dot-directories, so it never becomes a stone. Repos outside `~/projects` (e.g. `/mnt/...`) use the
  same folder.
- **Branch name:** `refactor/<yyyymmdd-hhmm>`; if taken, append `-2`, `-3`.
- **Target branch:** the branch the repo was on when Refactor was pressed (recorded). Detached HEAD:
  Refactor disabled with "Check out a branch first".
- **Result file:** the agent writes `refract-refactor.json` into the worktree's own git dir
  (`git rev-parse --git-dir`, never committed): `{checks: [{command, before, after}], refactors:
  [{title, commit}], summary}`. The panel reads it; if it's missing or malformed the panel falls back
  to git (commit list + diffstat) and says "No check results reported".
- **Record:** the helper keeps one JSON record per refactor in `~/.local/state/refract/refactors/`
  (repo path, worktree path, branch, base commit, target branch, session id, created). On start it
  reconciles records with `git worktree list` (record without worktree → dropped; refract worktree
  without record → adopted with what git knows).
- **Fix with Claude:** needs `LaunchMode::Resume` to accept an optional prompt (serde default, no
  protocol bump), mirroring `New { prompt }`.

## 4. States (each has a way forward)

Shown on the parent repo's panel and as the stone's refactor mark.

| State | How it's known | Mark | Panel actions |
|---|---|---|---|
| Starting | record exists, no session yet (launch pending) | turning | — (spinner, "Opening VS Code…") |
| Planning / working | refactor session live, hook `working` (or Unknown without hook) | turning | Go to the refactor |
| Needs you | refactor session live, hook `waitingOnUser` (plan approval or question) | red | Go to the refactor |
| Interrupted | session not live; transcript classifies as interrupted, or idle with 0 commits and no result file | red outline | Resume refactor, Discard |
| Ready | session not live (or idle) and ≥1 commit ahead of base | "ready" | Review in VS Code, Merge into `<branch>`, Discard |
| Nothing to do | session ended, 0 commits, result file says no refactors | plain | Discard ("Remove") |
| Merge blocked | Merge pressed; clashing uncommitted files, or rebase conflict | red | (list of files) Merge again / Fix with Claude / Discard |
| Merged | fast-forward done | — | record + worktree + branch removed; toast-free; panel line "Merged 3 commits" until the panel closes |

Toasts: entering **Needs you** the first time (plan ready) and entering **Ready**. Only when the
window is hidden or the repo isn't zoomed; one toast per transition.

## 5. Data-safety rules

1. Refract never writes to Maddox's working copy except the final fast-forward of the target branch.
2. Merge = in the worktree `git rebase <target>`; on conflict `git rebase --abort` (worktree back as it
   was) and show **Fix with Claude**. Then in the main repo:
   - target checked out there: `git merge --ff-only refactor/…` (git itself refuses if an uncommitted
     file would be overwritten; Refract checks the overlap first to name the files);
   - target not checked out: `git fetch . refactor/…:<target>` (ff-only ref update, no working-copy change).
3. After a successful merge: `git worktree remove` (no `--force`), then `git branch -d` (not `-D`).
   If either refuses, keep it and show why.
4. Discard: confirm, then `git worktree remove --force` and `git branch -D`. Only allowed when the
   refactor session isn't live.
5. The worktree path must be under `~/projects/.refract-worktrees/` and registered in `git worktree
   list` of that repo before anything deletes it.
6. Nothing is ever pushed.

## 6. The prompt (draft)

Launched as `claude --permission-mode plan -- "<prompt>"` in the worktree.

> You are refactoring this repository in a git worktree on branch `refactor/…` (your own copy;
> the user's working copy is elsewhere). Goal: up to 3 behaviour-preserving refactors with the
> best payoff (duplication, oversized files or functions, dead code, unclear names, tangled modules).
> No new features, no public API or behaviour changes, no dependency upgrades.
> 1. Set up the project so its checks can run (install dependencies as the README/lockfiles say).
> 2. Find the test, lint and type-check commands and run them for a baseline.
> 3. Propose the plan: each refactor, why, files touched, risk. Wait for approval.
> 4. For each refactor: make it, re-run the checks, commit it alone with a clear message. If a check
>    that passed in the baseline now fails and you can't fix it within the refactor, revert it.
> 5. Write `<git-dir>/refract-refactor.json` (format: …) and finish with a short summary.
> [Focus on: <note>]

Exact wording, the JSON format and examples live in one file shared by the helper and its tests.

## 7. Pieces

- **Helper (refract-agent):** `refactor.rs`: start (worktree add, record, launch request with prompt +
  plan mode), state (record + sessions in the worktree path + git ahead count + result file), merge,
  discard, reconcile on start. Sessions whose cwd is a refract worktree map to the parent repo's
  refactor, not to a stone. Protocol: `startRefactor`, `mergeRefactor`, `discardRefactor`;
  `RepoInfo.refactor: Option<RefactorInfo>` (serde default); `repoChanged` carries changes.
- **Core:** launch argv for plan mode; `Resume { prompt }`; result-file parse; worktree path rules.
- **Extension:** none expected (launch requests already open a folder and run argv).
- **App:** panel section (states above), Refactor dialog (note, focus line, hook nudge), Alt+R in
  navigation (Alt currently unhandled), refactor mark on the stone (Banknote style: an engraved
  second ring/tick; red = needs you), toasts via the existing `tauri-plugin-notification` path.
- **Demo:** DemoBackend simulates start → planning → needs you → working → ready → merge/discard.
- **Docs:** architecture (refactor section, protocol table), ADR 0018, README feature line.

## 8. Tests

- Core: argv with plan mode and prompt after `--`; Resume with prompt; result-file parse (good, bad, huge).
- Helper integration (temp repos): start creates worktree/branch/record; busy repo allowed; one-per-repo;
  detached HEAD refused; state transitions from fake sessions/hook statuses; merge ff, merge after
  target moved (clean rebase), rebase conflict → aborted + worktree unchanged, clashing dirty file
  blocks with names, non-overlapping dirty file merges, target not checked out; discard; reconcile
  (record without worktree, worktree without record); path-safety refusals.
- App: dialog, panel per state, Alt+R, toast-once logic, demo flow e2e.
- Live: Maddox runs one real refactor on an invented scratch repo.

## 9. Out of scope (v1)

Pushing / PRs, several refactors per repo, a diff viewer, model choice, refactoring remote-only repos,
scheduling refactors.
