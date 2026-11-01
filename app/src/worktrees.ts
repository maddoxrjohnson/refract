import type { GitOperation, RepoSnapshot, WorktreeStatus } from "./backend/types";

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
}

/** What a folded worktree's row shows; `null` without the helper's facts. */
export function worktreeView(repo: RepoSnapshot): WorktreeView | null {
  const info = repo.worktree;
  if (!info) return null;
  const agent = worktreeAgent(repo);
  if (agent === "waiting") return { label: "Agent waiting for you", tone: "attention", removable: false, agentOpen: false };
  if (agent === "working") return { label: "Agent running", tone: "working", removable: false, agentOpen: false };
  // The helper refuses Remove while any session is live, idle ones too.
  const done = info.removable;
  // Nor does it remove the repo a refactor was started from; say why Remove is missing.
  if (done && repo.refactor) return { label: "Has a refactor", tone: "keep", removable: false, agentOpen: false };
  return {
    label: gitStatusLabel(info.status),
    tone: done ? "done" : "keep",
    removable: done && agent === null,
    agentOpen: done && agent === "open",
  };
}

/**
 * The ignored entries Remove deletes, as the confirm lists them: up to 8, and whether
 * there are more. The helper sends at most 9, so how many more is not known.
 */
export function ignoredList(ignored: readonly string[]): { shown: string[]; more: boolean } {
  return { shown: ignored.slice(0, 8), more: ignored.length > 8 };
}
