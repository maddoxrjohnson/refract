import type {
  CheckOutcome,
  MergeBlock,
  RefactorInfo,
  RefactorNotice,
  RepoSnapshot,
} from "./backend/types";

// The refactor button (.plans/refactor-spec.md): what the stone's mark, the panel and the
// toasts make of a repo's `refactor`.

/**
 * The refactor mark on the stone: turning while Claude starts or works, red while it
 * needs you (or stopped, or a merge did), the still "ready" seal once there is something
 * to merge, plain when it found nothing to do.
 */
export type RefactorMark = "turning" | "alert" | "ready" | "plain";

export function refactorMark(info: RefactorInfo | null | undefined): RefactorMark | null {
  if (!info) return null;
  switch (info.state) {
    case "starting":
    case "working":
      return "turning";
    case "needsYou":
    case "interrupted":
    case "mergeBlocked":
      return "alert";
    case "ready":
      return "ready";
    case "nothingToDo":
      return "plain";
  }
}

/** Why Refactor can't start here, or `null` when it can. Remote-only repos have no button. */
export function refactorBlocker(repo: RepoSnapshot): string | null {
  if (repo.branch === null) return "Check out a branch first.";
  if (!repo.lastCommit) return "Commit something first.";
  return null;
}

function plural(n: number, one: string, many: string): string {
  return `${n} ${n === 1 ? one : many}`;
}

/**
 * The dialog's one-line note about what the refactor leaves alone (spec §2 "Busy repo"),
 * or `null` when the repo is clean and no agent runs in it.
 */
export function busyNote(repo: RepoSnapshot): string | null {
  const dirty = repo.changedFiles > 0;
  const running = repo.sessions.some((s) => s.state.kind === "live");
  if (!dirty && !running) return null;
  const sha = repo.lastCommit?.sha;
  const parts = [sha ? `Starts from ${sha}.` : "Starts from the last commit."];
  if (dirty) parts.push(`Your ${plural(repo.changedFiles, "uncommitted file", "uncommitted files")} aren't included.`);
  if (running) parts.push("The agent running here carries on undisturbed.");
  return parts.join(" ");
}

export const CHECK_LABELS: Record<CheckOutcome, string> = {
  pass: "passed",
  fail: "failed",
  notRun: "not run",
};

/** Why the last merge stopped, as the panel says it. */
export function blockText(block: Pick<MergeBlock, "reason">, target: string): string {
  switch (block.reason) {
    case "dirtyOverlap":
      return "Your uncommitted changes touch files the refactor also changed. Commit or stash them, then merge again.";
    case "conflict":
      return `The refactor no longer applies cleanly to ${target}: rebasing it hit conflicts, so nothing was changed.`;
    case "targetMissing":
      return `The branch ${target} no longer exists.`;
    case "live":
      return "The refactor agent is still running. Let it finish, then merge again.";
    case "targetBusy":
      return `${target} is checked out or being rebased in another worktree. Finish there, then merge again.`;
    case "worktreeBusy":
      return "The refactor's copy is in the middle of a rebase or merge, or not on its branch (after a crash, say). Finish or abort it in the refactor window (git rebase --abort), or Discard.";
    case "worktreeDirty":
      return "The refactor's copy has uncommitted changes, which can include new files Claude didn't add. Commit or remove them in the refactor window, then merge again.";
    case "targetRewound":
      return `${target} no longer contains the commit the refactor started from, so it can't be merged.`;
  }
}

/** "Discard 3 commits?", or for a refactor with none, what Remove deletes. */
export function discardQuestion(commits: number): string {
  return commits > 0
    ? `Discard ${plural(commits, "commit", "commits")}?`
    : "Remove the refactor's worktree and branch?";
}

// The toast's text for an alert the Rust side sent (app/src-tauri/src/alerts.rs decides them).
export const NOTICE_TEXT: Record<RefactorNotice["alert"], string> = {
  planReady: "refactor plan is ready for you",
  finished: "refactor is ready to review",
};
