import { invoke } from "@tauri-apps/api/core";
import { listen, type EventCallback, type UnlistenFn } from "@tauri-apps/api/event";
import { getCurrentWindow } from "@tauri-apps/api/window";
import type { ThemeSetting } from "../theme";
import type { Backend, Unsubscribe } from "./Backend";
import type {
  AgentEvent,
  AgentStatus,
  Credentials,
  DeleteOutcome,
  Forced,
  FinishAction,
  GpuUsage,
  HookInfo,
  LaunchMode,
  LaunchStep,
  LaunchTicket,
  MergeOutcome,
  MigratePreflight,
  RefactorNotice,
  MigrateSnapshotProgress,
  PendingMigration,
  CancelOutcome,
  RemoteDetails,
  RemoteKind,
  RemoteList,
  RemoteRef,
  RemoteRepo,
  RepoList,
  RepoSnapshot,
  Settings,
  SystemStats,
  UsageSummary,
  VsCodeWindow,
} from "./types";
import { NO_FORCE } from "./types";

/** Event names the Rust side emits. */
export const EVENTS = {
  visibility: "refract://visibility",
  agentStatus: "agent://status",
  agentEvent: "agent://event",
  launchProgress: "launch://progress",
  gpu: "system://gpu",
  stats: "system://stats",
  migrateSnapshot: "migrate://snapshot",
  refactorNotice: "refactor://notice",
} as const;

/**
 * Subscribes synchronously from the caller's point of view: `listen` resolves later,
 * so an unsubscribe that arrives first is applied once it does.
 */
function subscribe<T>(event: string, handler: EventCallback<T>): Unsubscribe {
  let unlisten: UnlistenFn | undefined;
  let stopped = false;
  listen<T>(event, handler).then(
    (fn) => {
      if (stopped) fn();
      else unlisten = fn;
    },
    (err: unknown) => console.error(`listen ${event} failed`, err),
  );
  return () => {
    stopped = true;
    unlisten?.();
  };
}

export class TauriBackend implements Backend {
  agentStatus(): Promise<AgentStatus> {
    return invoke<AgentStatus>("agent_status");
  }

  onAgentStatus(cb: (status: AgentStatus) => void): Unsubscribe {
    return subscribe<AgentStatus>(EVENTS.agentStatus, (e) => cb(e.payload));
  }

  listRepos(): Promise<RepoList> {
    return invoke<RepoList>("agent_request", { request: { method: "listRepos" } });
  }

  getRepo(path: string): Promise<RepoSnapshot> {
    return invoke<RepoSnapshot>("agent_request", {
      request: { method: "getRepo", params: { path } },
    });
  }

  onAgentEvent(cb: (event: AgentEvent) => void): Unsubscribe {
    return subscribe<AgentEvent>(EVENTS.agentEvent, (e) => cb(e.payload));
  }

  launch(path: string, mode: LaunchMode): Promise<LaunchTicket> {
    return invoke<LaunchTicket>("launch", { path, mode });
  }

  onLaunchProgress(cb: (step: LaunchStep) => void): Unsubscribe {
    return subscribe<{ step: LaunchStep }>(EVENTS.launchProgress, (e) => cb(e.payload.step));
  }

  isVisible(): Promise<boolean> {
    return invoke<boolean>("is_visible");
  }

  onVisibility(cb: (visible: boolean) => void): Unsubscribe {
    return subscribe<{ visible: boolean }>(EVENTS.visibility, (e) => cb(e.payload.visible));
  }

  /** Rejects with `{kind, message}`, `kind` one of invalid, noDistro, failed, internal. */
  openFolder(path: string): Promise<void> {
    return invoke<void>("open_folder", { path });
  }

  /** Rejects with `{kind, message}` as `openFolder` does. */
  openUrl(url: string): Promise<void> {
    return invoke<void>("open_url", { url });
  }

  /** Rejects with `{kind, message}` as `openFolder` does. */
  openLogFolder(): Promise<void> {
    return invoke<void>("open_log_folder");
  }

  onGpuUsage(cb: (usage: GpuUsage) => void): Unsubscribe {
    return subscribe<GpuUsage>(EVENTS.gpu, (e) => cb(e.payload));
  }

  // Remote repos, settings and cloning reject with `CommandError` (`{kind, message}`).

  listRemotes(refresh = false): Promise<RemoteList> {
    return invoke<RemoteList>("remote_list", { refresh });
  }

  remoteDetails(repo: RemoteRef, refresh = false): Promise<RemoteDetails> {
    return invoke<RemoteDetails>("remote_details", { repo, refresh });
  }

  getSettings(): Promise<Settings> {
    return invoke<Settings>("settings_get");
  }

  setSettings(settings: Settings): Promise<void> {
    return invoke<void>("settings_set", { settings });
  }

  setWindowTheme(theme: ThemeSetting): Promise<void> {
    return getCurrentWindow().setTheme(theme === "system" ? null : theme);
  }

  credentials(): Promise<Credentials> {
    return invoke<Credentials>("credentials");
  }

  setToken(kind: RemoteKind, token: string | null): Promise<void> {
    return invoke<void>("set_token", { kind, token });
  }

  cloneRepo(repo: RemoteRepo): Promise<string> {
    return invoke<string>("clone_repo", { repo });
  }

  addRepo(path: string): Promise<void> {
    return invoke<void>("add_repo", { path });
  }

  // Starting and resuming a refactor are launches; merge and discard reject with `CommandError`.

  startRefactor(path: string, focus?: string): Promise<LaunchTicket> {
    return invoke<LaunchTicket>("start_refactor", { path, focus: focus ?? null });
  }

  resumeRefactor(path: string, fix: boolean): Promise<LaunchTicket> {
    return invoke<LaunchTicket>("resume_refactor", { path, fix });
  }

  mergeRefactor(path: string): Promise<MergeOutcome> {
    return invoke<MergeOutcome>("merge_refactor", { path });
  }

  discardRefactor(path: string): Promise<void> {
    return invoke<void>("discard_refactor", { path });
  }

  openRefactor(path: string): Promise<void> {
    return invoke<void>("open_refactor", { path });
  }

  onRefactorNotice(cb: (notice: RefactorNotice) => void): Unsubscribe {
    return subscribe<RefactorNotice>(EVENTS.refactorNotice, (e) => cb(e.payload));
  }

  setZoomed(path: string | null): Promise<void> {
    return invoke<void>("set_zoomed", { path });
  }

  /** Rejects with `CommandError`, as merge and discard do. */
  removeWorktree(path: string): Promise<void> {
    return invoke<void>("remove_worktree", { path });
  }

  // The hook and usage commands reject with `AgentRequestError`, as `agent_request` does.

  hookInfo(): Promise<HookInfo> {
    return invoke<HookInfo>("hook_info");
  }

  installHook(): Promise<HookInfo> {
    return invoke<HookInfo>("install_hook");
  }

  uninstallHook(): Promise<HookInfo> {
    return invoke<HookInfo>("uninstall_hook");
  }

  usage(): Promise<UsageSummary> {
    return invoke<UsageSummary>("usage");
  }

  onSystemStats(cb: (stats: SystemStats) => void): Unsubscribe {
    return subscribe<SystemStats>(EVENTS.stats, (e) => cb(e.payload));
  }

  // Move to WSL: each rejects with `MigrateError` (`{kind, message, refusals?}`).

  migratePreflight(path: string, dest?: string): Promise<MigratePreflight> {
    return invoke<MigratePreflight>("migrate_preflight", { path, dest: dest ?? null });
  }

  migrateStart(path: string, dest: string): Promise<{ id: string }> {
    return invoke<{ id: string }>("migrate_start", { path, dest });
  }

  migrateCancelSnapshot(path: string): Promise<boolean> {
    return invoke<boolean>("migrate_cancel_snapshot", { path });
  }

  migrateCancel(id: string): Promise<CancelOutcome> {
    return invoke<CancelOutcome>("migrate_cancel", { id });
  }

  migrateWindows(path: string): Promise<VsCodeWindow[]> {
    return invoke<VsCodeWindow[]>("migrate_windows", { path });
  }

  migrateCloseWindow(path: string, id: number): Promise<boolean> {
    return invoke<boolean>("migrate_close_window", { path, id });
  }

  migrateStopAgent(path: string, sessionId: string): Promise<void> {
    return invoke<void>("migrate_stop_agent", { path, sessionId });
  }

  migrateResync(id: string): Promise<void> {
    return invoke<void>("migrate_resync", { id });
  }

  migrateDelete(id: string, permanent: boolean, force: Forced = NO_FORCE): Promise<DeleteOutcome> {
    return invoke<DeleteOutcome>("migrate_delete", { id, permanent, force });
  }

  migrateFinish(id: string, action: FinishAction): Promise<void> {
    return invoke<void>("migrate_finish", { id, action });
  }

  migratePending(): Promise<PendingMigration[]> {
    return invoke<PendingMigration[]>("migrate_pending");
  }

  migrateSourceExists(id: string): Promise<boolean> {
    return invoke<boolean>("migrate_source_exists", { id });
  }

  onMigrateSnapshot(cb: (progress: MigrateSnapshotProgress) => void): Unsubscribe {
    return subscribe<MigrateSnapshotProgress>(EVENTS.migrateSnapshot, (e) => cb(e.payload));
  }
}
