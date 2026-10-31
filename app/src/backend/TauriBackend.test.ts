import { emit } from "@tauri-apps/api/event";
import { clearMocks, mockIPC, mockWindows } from "@tauri-apps/api/mocks";
import { afterEach, describe, expect, it } from "vitest";
import { EVENTS, TauriBackend } from "./TauriBackend";
import type {
  AgentStatus,
  DeleteOutcome,
  GpuUsage,
  HookInfo,
  MigrateSnapshotProgress,
  RemoteRepo,
  Settings,
  SystemStats,
  UsageSummary,
} from "./types";

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

  it("opens folders and links through their commands", async () => {
    const calls: Array<{ cmd: string; args: unknown }> = [];
    const refused = { kind: "invalid", message: "Only https links can be opened: http://x" };
    mockIPC((cmd, args) => {
      calls.push({ cmd, args });
      if (cmd === "open_url") throw refused;
    });
    const backend = new TauriBackend();
    await backend.openFolder("/home/dev/projects/tidepool-api");
    await expect(backend.openUrl("http://x")).rejects.toEqual(refused);
    await backend.openLogFolder();
    expect(calls).toEqual([
      { cmd: "open_folder", args: { path: "/home/dev/projects/tidepool-api" } },
      { cmd: "open_url", args: { url: "http://x" } },
      { cmd: "open_log_folder", args: {} },
    ]);
  });

  it("delivers GPU usage until unsubscribed", async () => {
    mockIPC(() => undefined, { shouldMockEvents: true });
    const usages: GpuUsage[] = [];
    const stop = new TauriBackend().onGpuUsage((u) => usages.push(u));
    await settle();
    await emit(EVENTS.gpu, { totalPercent: 42.5, ownPercent: 7 });
    stop();
    await settle();
    await emit(EVENTS.gpu, { totalPercent: 10, ownPercent: 0 });
    expect(usages).toEqual([{ totalPercent: 42.5, ownPercent: 7 }]);
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

  it("lists remotes, fetches details and reads and saves settings through their commands", async () => {
    const calls: Array<{ cmd: string; args: unknown }> = [];
    const settings: Settings = {
      gitlabUrl: "https://gitlab.example.com",
      showForks: false,
      showArchived: true,
      hidden: ["remote:github.com/demo-user/orbit-cli"],
      costEstimate: {
        enabled: true,
        prices: [{ match: "sonnet", input: 2, output: 10, cacheWrite5m: 2.5, cacheWrite1h: 4, cacheRead: 0.1 }],
      },
      theme: "dark",
    };
    mockIPC((cmd, args) => {
      calls.push({ cmd, args });
      if (cmd === "remote_list") return { repos: [], sources: [], fetchedAt: "2026-10-30T12:00:00Z" };
      if (cmd === "settings_get") return settings;
      if (cmd === "credentials") return { github: "ghCli", gitlab: "missing" };
    });
    const backend = new TauriBackend();
    const ref = { host: "gitlab.example.com", owner: "example-group/sub", name: "app" };
    await backend.listRemotes();
    await backend.listRemotes(true);
    await backend.remoteDetails(ref);
    await backend.remoteDetails(ref, true);
    expect(await backend.getSettings()).toEqual(settings);
    await backend.setSettings(settings);
    expect(await backend.credentials()).toEqual({ github: "ghCli", gitlab: "missing" });
    expect(calls).toEqual([
      { cmd: "remote_list", args: { refresh: false } },
      { cmd: "remote_list", args: { refresh: true } },
      { cmd: "remote_details", args: { repo: ref, refresh: false } },
      { cmd: "remote_details", args: { repo: ref, refresh: true } },
      { cmd: "settings_get", args: {} },
      { cmd: "settings_set", args: { settings } },
      { cmd: "credentials", args: {} },
    ]);
  });

  it("sets the window's theme, with system as no theme", async () => {
    mockWindows("main");
    const calls: Array<{ cmd: string; args: unknown }> = [];
    mockIPC((cmd, args) => {
      calls.push({ cmd, args });
    });
    const backend = new TauriBackend();
    await backend.setWindowTheme("dark");
    await backend.setWindowTheme("system");
    expect(calls).toEqual([
      { cmd: "plugin:window|set_theme", args: { label: "main", value: "dark" } },
      { cmd: "plugin:window|set_theme", args: { label: "main", value: null } },
    ]);
  });

  it("stores tokens, clones and adds folders through their commands", async () => {
    const calls: Array<{ cmd: string; args: unknown }> = [];
    const exists = { kind: "exists", message: "~/projects/orbit-cli already exists." };
    mockIPC((cmd, args) => {
      calls.push({ cmd, args });
      if (cmd === "clone_repo") return "/home/dev/projects/orbit-cli";
      if (cmd === "add_repo") throw exists;
    });
    const backend = new TauriBackend();
    const repo: RemoteRepo = {
      kind: "github",
      host: "github.com",
      owner: "demo-user",
      name: "orbit-cli",
      webUrl: "https://github.com/demo-user/orbit-cli",
      cloneUrl: "https://github.com/demo-user/orbit-cli.git",
      defaultBranch: "main",
      language: "Rust",
      sizeKb: 120,
      pushedAt: null,
      fork: false,
      archived: false,
      private: true,
      description: null,
    };
    await backend.setToken("gitlab", "glpat-example");
    await backend.setToken("github", null);
    expect(await backend.cloneRepo(repo)).toBe("/home/dev/projects/orbit-cli");
    await expect(backend.addRepo("/home/dev/code/orbit-cli")).rejects.toEqual(exists);
    expect(calls).toEqual([
      { cmd: "set_token", args: { kind: "gitlab", token: "glpat-example" } },
      { cmd: "set_token", args: { kind: "github", token: null } },
      { cmd: "clone_repo", args: { repo } },
      { cmd: "add_repo", args: { path: "/home/dev/code/orbit-cli" } },
    ]);
  });

  it("asks about the hook and usage through their commands", async () => {
    const calls: Array<{ cmd: string; args: unknown }> = [];
    const info: HookInfo = {
      installed: false,
      settingsPath: "/home/dev/.claude/settings.json",
      diff: "--- a/settings.json\n+++ b/settings.json\n",
    };
    const totals = { input: 1, output: 2, cacheRead: 3, cacheWrite: 4 };
    const summary: UsageSummary = {
      today: totals,
      last7Days: totals,
      last5Hours: totals,
      liveAgents: 2,
      byRepo: [{ path: "/home/dev/projects/tidepool-api", name: "tidepool-api", last7Days: totals }],
      computedAt: "2026-10-30T12:00:00Z",
      ready: true,
    };
    const notRunning = { kind: "notRunning" };
    mockIPC((cmd, args) => {
      calls.push({ cmd, args });
      if (cmd === "hook_info") return info;
      if (cmd === "install_hook") return { ...info, installed: true, diff: null };
      if (cmd === "uninstall_hook") throw notRunning;
      if (cmd === "usage") return summary;
    });
    const backend = new TauriBackend();
    expect(await backend.hookInfo()).toEqual(info);
    expect(await backend.installHook()).toEqual({ ...info, installed: true, diff: null });
    await expect(backend.uninstallHook()).rejects.toEqual(notRunning);
    expect(await backend.usage()).toEqual(summary);
    expect(calls).toEqual([
      { cmd: "hook_info", args: {} },
      { cmd: "install_hook", args: {} },
      { cmd: "uninstall_hook", args: {} },
      { cmd: "usage", args: {} },
    ]);
  });

  it("delivers system stats until unsubscribed", async () => {
    mockIPC(() => undefined, { shouldMockEvents: true });
    const stats: SystemStats = {
      cpuPercent: 12.5,
      memUsedBytes: 8e9,
      memTotalBytes: 32e9,
      gpuPercent: 30,
      own: { cpuPercent: 0.4, memBytes: 1.2e8, gpuPercent: 1.5 },
    };
    const seen: SystemStats[] = [];
    const stop = new TauriBackend().onSystemStats((s) => seen.push(s));
    await settle();
    await emit(EVENTS.stats, stats);
    stop();
    await settle();
    await emit(EVENTS.stats, { ...stats, cpuPercent: 90 });
    expect(seen).toEqual([stats]);
  });

  it("moves a repo to WSL through the migrate commands", async () => {
    const calls: Array<{ cmd: string; args: unknown }> = [];
    const deleted: DeleteOutcome = { kind: "wontFit", reason: "too large", deleting: false };
    mockIPC(
      (cmd, args) => {
        calls.push({ cmd, args });
        if (cmd === "migrate_start") return { id: "m-1" };
        if (cmd === "migrate_delete") return deleted;
        if (cmd === "migrate_windows" || cmd === "migrate_pending") return [];
        if (cmd === "migrate_cancel_snapshot" || cmd === "migrate_close_window") return true;
      },
      { shouldMockEvents: true },
    );
    const backend = new TauriBackend();
    const seen: MigrateSnapshotProgress[] = [];
    const stop = backend.onMigrateSnapshot((p) => seen.push(p));
    await settle();
    const path = "/mnt/c/Users/dev/Desktop/aurora-notes";
    const dest = "/home/dev/projects/aurora-notes";
    await backend.migratePreflight(path);
    await backend.migratePreflight(path, dest);
    expect(await backend.migrateStart(path, dest)).toEqual({ id: "m-1" });
    expect(await backend.migrateCancelSnapshot(path)).toBe(true);
    expect(await backend.migrateWindows(path)).toEqual([]);
    expect(await backend.migrateCloseWindow(path, 42)).toBe(true);
    await backend.migrateStopAgent(path, "0b7e");
    await backend.migrateResync("m-1");
    expect(await backend.migrateDelete("m-1", false)).toEqual(deleted);
    await backend.migrateFinish("m-1", "keep");
    await backend.migrateCancel("m-1");
    expect(await backend.migratePending()).toEqual([]);
    const progress: MigrateSnapshotProgress = {
      path,
      phase: "hashing",
      filesDone: 1,
      filesTotal: 2,
      bytesDone: 3,
      bytesTotal: 4,
    };
    await emit(EVENTS.migrateSnapshot, progress);
    stop();
    await settle();
    await emit(EVENTS.migrateSnapshot, { ...progress, filesDone: 2 });
    expect(seen).toEqual([progress]);
    expect(calls).toEqual([
      { cmd: "migrate_preflight", args: { path, dest: null } },
      { cmd: "migrate_preflight", args: { path, dest } },
      { cmd: "migrate_start", args: { path, dest } },
      { cmd: "migrate_cancel_snapshot", args: { path } },
      { cmd: "migrate_windows", args: { path } },
      { cmd: "migrate_close_window", args: { path, id: 42 } },
      { cmd: "migrate_stop_agent", args: { path, sessionId: "0b7e" } },
      { cmd: "migrate_resync", args: { id: "m-1" } },
      { cmd: "migrate_delete", args: { id: "m-1", permanent: false, force: { changed: [], removed: [] } } },
      { cmd: "migrate_finish", args: { id: "m-1", action: "keep" } },
      { cmd: "migrate_cancel", args: { id: "m-1" } },
      { cmd: "migrate_pending", args: {} },
    ]);
  });
});
