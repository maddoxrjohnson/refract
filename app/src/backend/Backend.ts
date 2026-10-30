import type {
  AgentEvent,
  DeleteOutcome,
  Forced,
  FinishAction,
  HookInfo,
  MigratePreflight,
  MigrateSnapshotProgress,
  PendingMigration,
  CancelOutcome,
  VsCodeWindow,
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
  /** Opens the folder holding the app's log file, `refract.log`, in the file manager. */
  openLogFolder(): Promise<void>;
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

  // Move to WSL (app/src-tauri/src/migrate/mod.rs documents the flow). Paths are the
  // source's WSL path (`/mnt/c/...`). Each rejects with a `MigrateError`.

  /** Walks the source and says what the dialog shows; sends `onMigrateSnapshot` progress. */
  migratePreflight(path: string, dest?: string): Promise<MigratePreflight>;
  /** Takes the source snapshot, then starts the copy; resolves with the migration's id. */
  migrateStart(path: string, dest: string): Promise<{ id: string }>;
  /** Stops a running preflight or snapshot of `path`; false when none runs. */
  migrateCancelSnapshot(path: string): Promise<boolean>;
  /**
   * Stops a running migration. Before the copy is placed the partial copy is deleted (`ended`); once it is placed,
   * or during a re-sync, the step stops, the copy stays and a report follows.
   */
  migrateCancel(id: string): Promise<CancelOutcome>;
  /** VS Code windows that have the folder open. */
  migrateWindows(path: string): Promise<VsCodeWindow[]>;
  /** Asks one to close (its unsaved-file prompt still shows); false when it is gone. */
  migrateCloseWindow(path: string, id: number): Promise<boolean>;
  /** Stops an agent on the repo that is finished or waiting on the user. */
  migrateStopAgent(path: string, sessionId: string): Promise<void>;
  /** Re-copies what changed since the report; progress and a new `migrateReady` follow. */
  migrateResync(id: string): Promise<void>;
  /**
   * Delete original; `permanent` only after the user agreed to skip the Recycle Bin. `force`: the `copyChanged` paths
   * the user saw and answered Delete the rest anyway for, while deleting, as edited or deleted in the copy; the helper
   * skips only those, and each only while it still changed that way.
   */
  migrateDelete(id: string, permanent: boolean, force?: Forced): Promise<DeleteOutcome>;
  /** Keep both, or discard the copy. */
  migrateFinish(id: string, action: FinishAction): Promise<void>;
  /** Reports still waiting for an answer, after a restart. */
  migratePending(): Promise<PendingMigration[]>;
  /** False once the original folder is gone (deleted by hand, or partly by Delete original). */
  migrateSourceExists(id: string): Promise<boolean>;
  onMigrateSnapshot(cb: (progress: MigrateSnapshotProgress) => void): Unsubscribe;

  /**
   * Demo only: launches that were simulated instead of started, for the pretend terminal.
   * `TauriBackend` leaves this out, so the desktop app never shows it.
   */
  onDemoLaunch?(cb: (launch: DemoLaunch) => void): Unsubscribe;
}

/** A launch the web demo pretends to have made (spec §8). */
export interface DemoLaunch {
  repoName: string;
  path: string;
  mode: LaunchMode;
  sessionId: string;
}
