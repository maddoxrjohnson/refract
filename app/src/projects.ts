import type { RemoteList, RemoteRef, RemoteRepo, RepoSnapshot, Settings } from "./backend/types";

// Which projects appear (spec §5): local repos from the helper merged with remote repos
// from GitHub and GitLab. A remote repo with a local clone shows as the local gem; one
// without shows as a dimmed gem whose path is its project key, so the scene, layout and
// navigation, which all key gems by path, work unchanged.

/** The key of a remote repo: `remote:<host>/<owner>/<name>`, lower case. */
export function remoteKey(ref: RemoteRef): string {
  return `remote:${ref.host}/${ref.owner}/${ref.name}`.toLowerCase();
}

/** Stable key for hiding: a local repo's path, or a remote repo's `remoteKey`. */
export function projectKey(project: RepoSnapshot | RemoteRepo): string {
  return "path" in project ? project.path : remoteKey(project);
}

/** Whether a key names a remote-only project rather than a local path. */
export const isRemoteKey = (key: string) => key.startsWith("remote:");

/** The key Hide stores for a gem: its remote's key when it has a remote, else its path. */
export function hideKey(repo: RepoSnapshot): string {
  return repo.remote ? remoteKey(repo.remote) : repo.path;
}

/** Largest mass a remote-only gem gets, so clones-to-be stay towards the edges. */
export const REMOTE_MAX_MASS = 0.45;

/** Log-scaled repository size, 0 to `REMOTE_MAX_MASS`; about 1 GB reaches the top. */
export function remoteMass(sizeKb: number): number {
  const kb = Number.isFinite(sizeKb) ? Math.max(0, sizeKb) : 0;
  return REMOTE_MAX_MASS * Math.min(1, Math.log10(1 + kb) / 6);
}

/** A remote-only repo as a gem: no sessions, no changes, launches straight to cloning. */
export function remoteSnapshot(remote: RemoteRepo): RepoSnapshot {
  return {
    path: remoteKey(remote),
    name: remote.name,
    originUrl: remote.cloneUrl,
    remote: { host: remote.host, owner: remote.owner, name: remote.name },
    branch: remote.defaultBranch,
    ahead: null,
    behind: null,
    changedFiles: 0,
    dirtyPaths: [],
    lastCommit: null,
    language: remote.language,
    trackedBytes: Math.round(remote.sizeKb * 1024),
    commits30d: 0,
    mass: remoteMass(remote.sizeKb),
    sessions: [],
    preselect: { kind: "noPicker" },
  };
}

export interface Projects {
  /** Every gem, local first, then remote-only. */
  repos: RepoSnapshot[];
  /** Remote info by gem path, for local repos with a listed remote and remote-only gems. */
  remotes: Map<string, RemoteRepo>;
  /** Paths (project keys) of the gems that are not cloned. */
  remoteOnly: Set<string>;
}

/**
 * Merges local repos with the remote listing. Local and remote match when host, owner and
 * name agree, ignoring case. Forks and archived repos are left out only when remote-only;
 * hidden keys apply to every gem (a local repo by its path or its remote's key).
 */
export function mergeProjects(
  local: RepoSnapshot[],
  remote: RemoteList | null,
  settings: Settings,
): Projects {
  const hidden = new Set(settings.hidden.map((k) => (isRemoteKey(k) ? k.toLowerCase() : k)));
  const byKey = new Map<string, RemoteRepo>();
  for (const r of remote?.repos ?? []) {
    const key = remoteKey(r);
    if (!byKey.has(key)) byKey.set(key, r);
  }

  const repos: RepoSnapshot[] = [];
  const remotes = new Map<string, RemoteRepo>();
  const cloned = new Set<string>();
  for (const repo of local) {
    const key = repo.remote ? remoteKey(repo.remote) : null;
    if (key) cloned.add(key);
    if (hidden.has(repo.path) || (key && hidden.has(key))) continue;
    repos.push(repo);
    const match = key ? byKey.get(key) : undefined;
    if (match) remotes.set(repo.path, match);
  }

  const remoteOnly = new Set<string>();
  for (const [key, r] of byKey) {
    if (cloned.has(key) || hidden.has(key)) continue;
    if (r.fork && !settings.showForks) continue;
    if (r.archived && !settings.showArchived) continue;
    repos.push(remoteSnapshot(r));
    remotes.set(key, r);
    remoteOnly.add(key);
  }
  return { repos, remotes, remoteOnly };
}

/** A hidden key for a person: `owner/name · host` for remote keys, the path otherwise. */
export function describeKey(key: string): string {
  if (!isRemoteKey(key)) return key;
  const rest = key.slice("remote:".length);
  const slash = rest.indexOf("/");
  return slash < 0 ? rest : `${rest.slice(slash + 1)} · ${rest.slice(0, slash)}`;
}
