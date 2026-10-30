import { invoke } from "@tauri-apps/api/core";
import { listen, type EventCallback, type UnlistenFn } from "@tauri-apps/api/event";
import type { Backend, Unsubscribe } from "./Backend";
import type {
  AgentEvent,
  AgentStatus,
  LaunchMode,
  LaunchStep,
  LaunchTicket,
  RepoList,
  RepoSnapshot,
} from "./types";

/** Event names the Rust side emits. */
export const EVENTS = {
  visibility: "refract://visibility",
  agentStatus: "agent://status",
  agentEvent: "agent://event",
  launchProgress: "launch://progress",
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
}
