import type {
  AgentEvent,
  HookInfo,
  SystemStats,
  UsageSummary,
  Credentials,
  RemoteDetails,
  RemoteKind,
  RemoteList,
  RemoteRef,
  RemoteRepo,
  Settings,
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

  /** Repos on GitHub and GitLab, cached about 15 minutes unless `refresh`. */
  listRemotes(refresh?: boolean): Promise<RemoteList>;
  /** Open pull/merge requests and CI for one repo, cached about 15 minutes unless `refresh`. */
  remoteDetails(repo: RemoteRef, refresh?: boolean): Promise<RemoteDetails>;
  getSettings(): Promise<Settings>;
  /** Saves settings; the next `listRemotes` uses them. */
  setSettings(settings: Settings): Promise<void>;
  credentials(): Promise<Credentials>;
  /** Stores a token in Windows Credential Manager; `null` removes it. */
  setToken(kind: RemoteKind, token: string | null): Promise<void>;
  /** Clones into `~/projects/<name>` in WSL and resolves with the local path. */
  cloneRepo(repo: RemoteRepo): Promise<string>;
  /** Adds a local folder (a WSL path) to the repos the helper reports. */
  addRepo(path: string): Promise<void>;

  /** Whether the status hook is installed, and what installing would change. */
  hookInfo(): Promise<HookInfo>;
  installHook(): Promise<HookInfo>;
  /** Removes the hook, restoring settings.json byte for byte when nothing else changed. */
  uninstallHook(): Promise<HookInfo>;
  /** Token totals; cheap, so the widget asks about every 2 s while visible. */
  usage(): Promise<UsageSummary>;
  /** Machine load about every 2 s while the window is visible. */
  onSystemStats(cb: (stats: SystemStats) => void): Unsubscribe;
}
