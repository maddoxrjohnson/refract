import type {
  CheckOutcome,
  MergeBlock,
  RefactorInfo,
  RefactorState,
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
      return "The refactor's worktree is off its branch or mid-rebase. Sort it out there, then merge again.";
    case "worktreeDirty":
      return "The refactor's worktree has uncommitted changes. Commit or drop them there, then merge again.";
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

// Toasts (spec §4): entering needsYou the first time (the plan) and entering ready, once
// per transition. The same rules as app/src-tauri/src/alerts.rs, which toasts while the
// window is hidden.

/** What was last seen of one repo's refactor. */
export interface RefactorSeen {
  /** Tells one refactor from the next; `null` while the repo has none. */
  worktree: string | null;
  state: RefactorState | null;
  /** The plan toast was shown for this refactor, or it was first seen past planning. */
  planTold: boolean;
}

export type RefactorAlert = "planReady" | "finished";

/**
 * What `next` is after `prev` (`undefined` the first time the repo is seen), and the toast
 * the change calls for. A first sighting never toasts.
 */
export function stepRefactor(
  prev: RefactorSeen | undefined,
  next: RefactorInfo | null | undefined,
): [RefactorSeen, RefactorAlert | null] {
  const worktree = next?.worktree ?? null;
  const state = next?.state ?? null;
  if (!prev) {
    const planTold = state !== null && state !== "starting" && state !== "working";
    return [{ worktree, state, planTold }, null];
  }
  const same = prev.worktree !== null && prev.worktree === worktree;
  const before = same ? prev.state : null;
  let planTold = same && prev.planTold;
  let alert: RefactorAlert | null = null;
  if (state === "needsYou" && before !== state && !planTold) {
    planTold = true;
    alert = "planReady";
  } else if (state === "ready" && before !== null && before !== "ready" && before !== "mergeBlocked") {
    alert = "finished";
  }
  return [{ worktree, state, planTold }, alert];
}

export interface RefactorNotice {
  path: string;
  repoName: string;
  alert: RefactorAlert;
}

/**
 * Steps every repo in `repos` from `seen` (updated in place; repos that are gone are
 * forgotten) and returns the toasts the changes call for.
 */
export function trackRefactors(
  seen: Map<string, RefactorSeen>,
  repos: Iterable<RepoSnapshot>,
): RefactorNotice[] {
  const out: RefactorNotice[] = [];
  const present = new Set<string>();
  for (const repo of repos) {
    present.add(repo.path);
    const [next, alert] = stepRefactor(seen.get(repo.path), repo.refactor);
    seen.set(repo.path, next);
    if (alert) out.push({ path: repo.path, repoName: repo.name, alert });
  }
  for (const path of [...seen.keys()]) if (!present.has(path)) seen.delete(path);
  return out;
}

export const NOTICE_TEXT: Record<RefactorAlert, string> = {
  planReady: "refactor plan is ready for you",
  finished: "refactor is ready to review",
};
