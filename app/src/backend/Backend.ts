import type {
  AgentEvent,
  AgentStatus,
  LaunchMode,
  LaunchStep,
  LaunchTicket,
  RepoList,
  RepoSnapshot,
} from "./types";

/** Stops a subscription. */
export type Unsubscribe = () => void;

/**
 * Everything the interface asks of the outside world. `TauriBackend` forwards to the
 * Rust side; a demo implementation can serve invented data instead.
 */
export interface Backend {
  agentStatus(): Promise<AgentStatus>;
  onAgentStatus(cb: (status: AgentStatus) => void): Unsubscribe;
  listRepos(): Promise<RepoList>;
  getRepo(path: string): Promise<RepoSnapshot>;
  onAgentEvent(cb: (event: AgentEvent) => void): Unsubscribe;
  /** Opens `path` in VS Code with an agent. Rejects with a `LaunchError`. */
  launch(path: string, mode: LaunchMode): Promise<LaunchTicket>;
  onLaunchProgress(cb: (step: LaunchStep) => void): Unsubscribe;
  /** Whether the window is shown right now. */
  isVisible(): Promise<boolean>;
  onVisibility(cb: (visible: boolean) => void): Unsubscribe;
}
