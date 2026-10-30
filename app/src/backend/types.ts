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
