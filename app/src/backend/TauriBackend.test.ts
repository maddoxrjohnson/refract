import { emit } from "@tauri-apps/api/event";
import { clearMocks, mockIPC } from "@tauri-apps/api/mocks";
import { afterEach, describe, expect, it } from "vitest";
import { EVENTS, TauriBackend } from "./TauriBackend";
import type { AgentStatus } from "./types";

afterEach(() => clearMocks());

/** Lets pending promise callbacks run. */
const settle = () => new Promise((r) => setTimeout(r, 0));

describe("TauriBackend", () => {
  it("sends requests as {method, params} through agent_request", async () => {
    const calls: Array<{ cmd: string; args: unknown }> = [];
    mockIPC((cmd, args) => {
      calls.push({ cmd, args });
      if (cmd === "agent_request") return { repos: [], loading: true };
      if (cmd === "is_visible") return true;
    });
    const backend = new TauriBackend();
    expect(await backend.listRepos()).toEqual({ repos: [], loading: true });
    await backend.getRepo("/home/dev/projects/tidepool-api");
    expect(await backend.isVisible()).toBe(true);
    expect(calls).toEqual([
      { cmd: "agent_request", args: { request: { method: "listRepos" } } },
      {
        cmd: "agent_request",
        args: { request: { method: "getRepo", params: { path: "/home/dev/projects/tidepool-api" } } },
      },
      { cmd: "is_visible", args: {} },
    ]);
  });

  it("launches through the launch command and reports its progress", async () => {
    const calls: Array<{ cmd: string; args: unknown }> = [];
    const ticket = {
      requestId: "6f1c",
      sessionId: "0b7e",
      folderUri: "vscode-remote://wsl+Ubuntu/home/dev/projects/tidepool-api",
    };
    mockIPC(
      (cmd, args) => {
        calls.push({ cmd, args });
        if (cmd === "launch") return ticket;
      },
      { shouldMockEvents: true },
    );
    const backend = new TauriBackend();
    const steps: string[] = [];
    backend.onLaunchProgress((step) => steps.push(step));
    await settle();
    const path = "/home/dev/projects/tidepool-api";
    expect(await backend.launch(path, { kind: "new" })).toEqual(ticket);
    await backend.launch(path, { kind: "resume", sessionId: "0b7e" });
    await emit(EVENTS.launchProgress, { step: "opening" });
    expect(steps).toEqual(["opening"]);
    expect(calls.filter((c) => c.cmd === "launch")).toEqual([
      { cmd: "launch", args: { path, mode: { kind: "new" } } },
      { cmd: "launch", args: { path, mode: { kind: "resume", sessionId: "0b7e" } } },
    ]);
  });

  it("passes launch errors through", async () => {
    const error = { kind: "vscodeNotFound", message: "VS Code not found." };
    mockIPC(() => {
      throw error;
    });
    await expect(new TauriBackend().launch("/x", { kind: "new" })).rejects.toEqual(error);
  });

  it("delivers events until unsubscribed", async () => {
    mockIPC(() => undefined, { shouldMockEvents: true });
    const backend = new TauriBackend();
    const seen: boolean[] = [];
    const statuses: AgentStatus[] = [];
    const stop = backend.onVisibility((v) => seen.push(v));
    backend.onAgentStatus((s) => statuses.push(s));
    await settle();
    await emit(EVENTS.visibility, { visible: true });
    await emit(EVENTS.agentStatus, { state: "running" });
    stop();
    await settle();
    await emit(EVENTS.visibility, { visible: false });
    expect(seen).toEqual([true]);
    expect(statuses).toEqual([{ state: "running" }]);
  });
});
