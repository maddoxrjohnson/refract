import type {
  AgentEvent,
  AgentStatus,
  GpuUsage,
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
  /** Opens a repo's folder (a WSL path) in the file manager. */
  openFolder(path: string): Promise<void>;
  /** Opens an `https://` URL in the default browser; other schemes are refused. */
  openUrl(url: string): Promise<void>;
  /** System GPU use, sampled about every 2 s while the window is visible. */
  onGpuUsage(cb: (usage: GpuUsage) => void): Unsubscribe;
}
