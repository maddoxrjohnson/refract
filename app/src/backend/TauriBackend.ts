import { invoke } from "@tauri-apps/api/core";
import { listen, type EventCallback, type UnlistenFn } from "@tauri-apps/api/event";
import type { Backend, Unsubscribe } from "./Backend";
import type {
  AgentEvent,
  AgentStatus,
  Credentials,
  GpuUsage,
  HookInfo,
  LaunchMode,
  LaunchStep,
  LaunchTicket,
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
} from "./types";

/** Event names the Rust side emits. */
export const EVENTS = {
  visibility: "refract://visibility",
  agentStatus: "agent://status",
  agentEvent: "agent://event",
  launchProgress: "launch://progress",
  gpu: "system://gpu",
  stats: "system://stats",
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
}
