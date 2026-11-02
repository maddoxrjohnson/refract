import type { GitOperation, MissingWorktreeStatus, RepoSnapshot, WorktreeCheck, WorktreeStatus } from "./backend/types";

// Linked worktrees folded into their main repo's gem (.plans/worktrees-spec.md §4): what
// a worktree's row says, and whether it can be removed. The helper decides the git rows
// (3-10); the agent rows (1-2) go on top here, from the worktree's own sessions, which
// `sessionChanged` keeps current without a new snapshot.

/** The worktree's agent, most urgent first: waiting, working, open but idle, or none. */
export type WorktreeAgent = "waiting" | "working" | "open" | null;

export function worktreeAgent(repo: RepoSnapshot): WorktreeAgent {
  let agent: WorktreeAgent = null;
  for (const s of repo.sessions) {
    if (s.state.kind !== "live") continue;
    if (s.state.detail === "waitingOnUser") return "waiting";
    if (s.state.detail === "working") agent = "working";
    else if (!agent) agent = "open";
  }
  return agent;
}

const OPERATION_LABELS: Record<GitOperation, string> = {
  rebase: "Mid-rebase",
  merge: "Mid-merge",
  cherryPick: "Mid-cherry-pick",
  revert: "Mid-revert",
  bisect: "Bisecting",
};

function plural(n: number, one: string, many: string): string {
  return `${n} ${n === 1 ? one : many}`;
}

/** The row's words for a git status (spec §4 rows 3-10). */
export function gitStatusLabel(status: WorktreeStatus): string {
  switch (status.kind) {
    case "locked":
      return "Locked";
    case "operation":
      return OPERATION_LABELS[status.op];
    case "changes":
      return plural(status.count, "uncommitted change", "uncommitted changes");
    case "unchecked":
      return "Changes not checked (Windows drive)";
    case "onlyHere":
      return status.count >= 100
        ? "100+ commits only on this computer"
        : plural(status.count, "commit only on this computer", "commits only on this computer");
    case "onlyHereUnknown":
      return "Can't tell if its commits are pushed";
    case "holdsRepo":
      return "Holds another repo";
    case "tooBigToCheck":
      return "Too many ignored files to check";
    case "release":
      return `In ${status.tag}`;
    case "merged":
      return `Merged into ${status.branch}`;
    case "pushed":
      return "Pushed, not merged";
  }
}

/** How a row reads: an agent needs you, one is busy, something could be lost, or it is done. */
export type WorktreeTone = "attention" | "working" | "keep" | "done";

export interface WorktreeView {
  /** The status (spec §4), agent rows first. */
  label: string;
  tone: WorktreeTone;
  /** Remove is offered: the helper says nothing can be lost, no agent is open there and it has no refactor. */
  removable: boolean;
  /** Nothing can be lost, but an agent is still open in the folder, which blocks Remove. */
  agentOpen: boolean;
  /**
   * Check… is offered: on a Windows drive, only the unread files could keep it (spec §10.1).
   * It stays after an answer, as Check again: edits from Windows bring no new snapshot.
   */
  checkable: boolean;
}

/**
 * What a folded worktree's row shows; `null` without the helper's facts. `check` is a
 * `checkWorktree` answer for the current snapshot, which stands in for its git status.
 */
export function worktreeView(repo: RepoSnapshot, check?: WorktreeCheck | null): WorktreeView | null {
  const info = repo.worktree;
  if (!info) return null;
  const agent = worktreeAgent(repo);
  const none = { removable: false, agentOpen: false, checkable: false };
  if (agent === "waiting") return { label: "Agent waiting for you", tone: "attention", ...none };
  if (agent === "working") return { label: "Agent running", tone: "working", ...none };
  const status = check ? check.status : info.status;
  // The helper refuses Remove while any session is live, idle ones too.
  const done = check ? check.removable : info.removable;
  // A check could make it removable, so the same rules hold Check… back.
  const checkable = info.checkable ?? false;
  // Nor does it remove the repo a refactor was started from; say why Remove is missing.
  if ((done || checkable) && repo.refactor) return { label: "Has a refactor", tone: "keep", ...none };
  return {
    label: gitStatusLabel(status),
    tone: done ? "done" : "keep",
    removable: done && agent === null,
    agentOpen: (done || checkable) && agent === "open",
    checkable: checkable && agent === null,
  };
}

/** A missing-folder entry's words (spec §10.2, m1-m6). */
export function missingWorktreeLabel(status: MissingWorktreeStatus): string {
  switch (status.kind) {
    case "locked":
      return "Folder is gone · Locked";
    case "driveNotAttached":
      return "Folder is gone · Drive not attached";
    case "moved":
      return `Moved to ${status.to}`;
    case "ownRefs":
      return "Folder is gone · Has its own refs";
    case "commitsOnlyHere":
      return "Folder is gone · Its commits would be lost";
    case "gone":
      return "Folder is gone";
  }
}

/**
 * The ignored entries Remove deletes, as the confirm lists them: up to 8, and whether
 * there are more. The helper sends at most 9, so how many more is not known.
 */
export function ignoredList(ignored: readonly string[]): { shown: string[]; more: boolean } {
  return { shown: ignored.slice(0, 8), more: ignored.length > 8 };
}
