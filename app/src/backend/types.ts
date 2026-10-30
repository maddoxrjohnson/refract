// Mirrors of the Rust/serde shapes the interface reads. Field names follow the wire
// format (camelCase); see crates/refract-core/src/protocol.rs and crates/refract-client.

/** State of the WSL helper process, from refract-client's `AgentStatus`. */
export type AgentStatus =
  | { state: "starting" }
  | { state: "running" }
  | { state: "restarting"; attempt: number; retryInMs: number; lastError: string | null }
  | { state: "stopped" };

/** Why `agent_request` failed, from refract-client's `RequestError`. */
export type AgentRequestError =
  | { kind: "agent"; code: number; message: string }
  | { kind: "timeout" }
  | { kind: "disconnected" }
  | { kind: "notRunning" };

export type LiveState = "working" | "waitingOnUser" | "idle" | "unknown";
export type InterruptReason =
  | "unansweredPrompt"
  | "toolCallWithoutResult"
  | "toolResultWithoutReply"
  | "apiError";
export type FinishKind = "completed" | "userInterrupted";

export type SessionState =
  | { kind: "live"; detail: LiveState }
  | { kind: "interrupted"; detail: InterruptReason }
  | { kind: "finished"; detail: FinishKind };

export type Preselect =
  | { kind: "noPicker" }
  | { kind: "resume"; sessionId: string }
  | { kind: "newAgent" };

export interface SessionInfo {
  id: string;
  state: SessionState;
  /** RFC 3339. */
  lastTimestamp: string | null;
  lastAssistantText: string | null;
  byteSize: number;
}

export interface RemoteId {
  host: string;
  owner: string;
  name: string;
}

export interface LastCommit {
  summary: string;
  /** Unix seconds. */
  time: number;
}

export interface RepoSnapshot {
  path: string;
  name: string;
  originUrl: string | null;
  remote: RemoteId | null;
  /** `null` when HEAD is detached. */
  branch: string | null;
  ahead: number | null;
  behind: number | null;
  changedFiles: number;
  dirtyPaths: string[];
  lastCommit: LastCommit | null;
  language: string | null;
  trackedBytes: number;
  commits30d: number;
  mass: number;
  sessions: SessionInfo[];
  preselect: Preselect;
}

export interface RepoList {
  repos: RepoSnapshot[];
  /** True while the first load runs; the rest arrive as `repoChanged` events. */
  loading: boolean;
}

/** An unsolicited message from the helper. */
export type AgentEvent =
  | { event: "repoChanged"; data: RepoSnapshot }
  | { event: "repoRemoved"; data: { path: string } }
  | {
      event: "sessionChanged";
      data: { repoPath: string; session: SessionInfo; preselect: Preselect };
    }
  | { event: "loadComplete" };

/** How to start the agent, from refract-core's `LaunchMode`. */
export type LaunchMode =
  | { kind: "new"; prompt?: string }
  | { kind: "resume"; sessionId: string };

/** What a launch returns, from refract-core's `LaunchTicket`. */
export interface LaunchTicket {
  requestId: string;
  sessionId: string;
  folderUri: string;
}

/** What a launch is doing, sent as `launch://progress` events. */
export type LaunchStep = "setup" | "server" | "prepare" | "opening" | "done";

/** Why the `launch` command failed; `message` is meant for a person. */
export interface LaunchError {
  kind: "vscodeNotFound" | "setup" | "agent" | "vscode" | "internal";
  message: string;
}

/** GPU use from Windows' performance counters, sent as `system://gpu` events. */
export interface GpuUsage {
  /** Busiest engine across all processes, 0 to 100. */
  totalPercent: number;
  /** Refract's own share of that engine, 0 to 100. */
  ownPercent: number;
}

// Remote repos (M5). Fetched on the Windows side from GitHub and GitLab; see
// app/src-tauri/src/remote.

export type RemoteKind = "github" | "gitlab";

/** A repo on GitHub or GitLab that the user owns, collaborates on or is a member of. */
export interface RemoteRepo {
  kind: RemoteKind;
  /** "github.com" or the GitLab host, lower case. */
  host: string;
  /** Owner login, or the GitLab namespace path (may contain `/`). */
  owner: string;
  name: string;
  webUrl: string;
  /** HTTPS clone URL. */
  cloneUrl: string;
  defaultBranch: string | null;
  language: string | null;
  sizeKb: number;
  /** RFC 3339. */
  pushedAt: string | null;
  fork: boolean;
  archived: boolean;
  private: boolean;
  description: string | null;
}

export interface RemoteSource {
  kind: RemoteKind;
  host: string;
  /** False when there is no URL or token for it, so it was not asked. */
  configured: boolean;
  /** Why listing failed, for a person; `null` when it worked or was not tried. */
  error: string | null;
}

export interface RemoteList {
  repos: RemoteRepo[];
  sources: RemoteSource[];
  /** RFC 3339. */
  fetchedAt: string;
}

export type CiStatus = "success" | "failure" | "pending" | "none";

/** Pull or merge requests and CI on the default branch; cached about 15 minutes. */
export interface RemoteDetails {
  openPullRequests: number | null;
  ci: CiStatus | null;
  ciUrl: string | null;
  /** RFC 3339. */
  fetchedAt: string;
}

/** Identifies a remote repo in requests. */
export interface RemoteRef {
  host: string;
  owner: string;
  name: string;
}

/** App settings, stored as JSON on the Windows side. Tokens are never in here. */
export interface Settings {
  /** The GitLab instance, e.g. "https://gitlab.example.com"; `null` turns GitLab off. */
  gitlabUrl: string | null;
  showForks: boolean;
  showArchived: boolean;
  /** Project keys (see `projectKey` in src/projects.ts) the user hid. */
  hidden: string[];
}

/** Where each token comes from. Tokens themselves never reach the interface. */
export interface Credentials {
  github: "ghCli" | "stored" | "missing";
  gitlab: "stored" | "missing";
}

/** Why a remote, settings or clone command failed; `message` is meant for a person. */
export interface CommandError {
  kind: string;
  message: string;
}
