import type { RepoSnapshot, SessionInfo } from "./backend/types";

/** What a gem shows about its repo (spec §3.2). */
export interface RepoStatus {
  /** Uncommitted changes: the blue halo. */
  dirty: boolean;
  /** An agent session stopped before it finished: pulsing amber edges. */
  interrupted: boolean;
  /** An agent is working right now (status hook): gentle shimmer. */
  working: boolean;
  /** An agent is waiting for input (status hook): slow amber pulse. */
  waiting: boolean;
}

export function repoStatus(repo: RepoSnapshot): RepoStatus {
  let interrupted = false;
  let working = false;
  let waiting = false;
  for (const s of repo.sessions) {
    if (s.state.kind === "interrupted") interrupted = true;
    else if (s.state.kind === "live") {
      if (s.state.detail === "working") working = true;
      else if (s.state.detail === "waitingOnUser") waiting = true;
    }
  }
  return { dirty: repo.changedFiles > 0, interrupted, working, waiting };
}

/** Sessions the picker offers to resume, newest first. */
export function unfinishedSessions(repo: RepoSnapshot): SessionInfo[] {
  return repo.sessions
    .filter((s) => s.state.kind === "interrupted")
    .sort((a, b) => (b.lastTimestamp ?? "").localeCompare(a.lastTimestamp ?? ""));
}

/** "GitHub", "GitLab", another host name, or "Local" without a recognised remote. */
export function hostLabel(repo: RepoSnapshot): string {
  const host = repo.remote?.host;
  if (!host) return "Local";
  if (host === "github.com") return "GitHub";
  if (host.startsWith("gitlab.") || host.includes(".gitlab.")) return "GitLab";
  return host;
}

/** The repo's web page, when its remote is known. */
export function remoteUrl(repo: RepoSnapshot): string | null {
  const r = repo.remote;
  return r ? `https://${r.host}/${r.owner}/${r.name}` : null;
}

/** The opening prompt for a new agent in a repo with uncommitted changes (spec §4.3). */
export const DIRTY_REPO_PROMPT =
  "This repo has uncommitted changes. Review `git diff` and help me finish or commit them.";
