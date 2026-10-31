import { afterEach, describe, expect, it, vi } from "vitest";
import { canDelete } from "../migrations";
import { DEMO_WINDOWS_REPO, DemoBackend, demoHookDiff, demoRepos } from "./DemoBackend";
import type { AgentEvent, MigrateError, MigrateSnapshotProgress } from "./types";

afterEach(() => vi.useRealTimers());

/** Runs `promise` to its end on fake timers. */
async function settle<T>(promise: Promise<T>): Promise<T> {
  let result: { ok: true; value: T } | { ok: false; error: unknown } | null = null;
  void promise.then(
    (value) => (result = { ok: true, value }),
    (error: unknown) => (result = { ok: false, error }),
  );
  while (!result) await vi.advanceTimersByTimeAsync(100);
  const r = result as { ok: true; value: T } | { ok: false; error: unknown };
  if (r.ok) return r.value;
  throw r.error;
}

describe("demo repos", () => {
  it("show every resume reason, and preselect only sessions that have one", () => {
    const repos = demoRepos();
    const reasons = new Set(repos.flatMap((r) => r.sessions.map((s) => s.resumeReason)));
    expect(reasons).toEqual(new Set([null, "interrupted", "askedQuestion", "uncommittedChanges", "unpushedCommits"]));
    for (const r of repos) {
      const pre = r.preselect;
      if (pre.kind !== "resume") continue;
      expect(r.sessions.find((s) => s.id === pre.sessionId)?.resumeReason, r.name).toBeTruthy();
    }
  });
});

describe("DemoBackend's Move to WSL", () => {
  it("runs a migration through every step to a passing report and the Recycle Bin", async () => {
    vi.useFakeTimers();
    const backend = new DemoBackend();
    const events: AgentEvent[] = [];
    const snapshots: MigrateSnapshotProgress[] = [];
    backend.onAgentEvent((e) => events.push(e));
    backend.onMigrateSnapshot((p) => snapshots.push(p));

    const preflight = await settle(backend.migratePreflight(DEMO_WINDOWS_REPO));
    expect(preflight).toMatchObject({
      name: "marigold-site",
      dest: "/home/demo/projects/marigold-site",
      refusals: [],
      install: [{ command: "npm ci" }],
    });
    expect(preflight.skipped.map((s) => s.path)).toEqual(["node_modules", ".next"]);
    expect(snapshots.map((p) => p.phase)).toContain("probing");

    // A VS Code window has it open, so Start is refused until it closes.
    expect(preflight.vscodeWindows).toHaveLength(1);
    await expect(settle(backend.migrateStart(DEMO_WINDOWS_REPO, preflight.dest!))).rejects.toMatchObject({
      kind: "busy",
    });
    expect(await settle(backend.migrateCloseWindow(DEMO_WINDOWS_REPO, preflight.vscodeWindows[0].id))).toBe(true);
    expect(await backend.migrateWindows(DEMO_WINDOWS_REPO)).toEqual([]);

    const { id } = await settle(backend.migrateStart(DEMO_WINDOWS_REPO, preflight.dest!));
    while (!events.some((e) => e.event === "migrateReady")) await vi.advanceTimersByTimeAsync(100);
    const steps = events.flatMap((e) => (e.event === "migrateProgress" ? [e.data.step] : []));
    expect([...new Set(steps)]).toEqual([
      "copying",
      "normalising",
      "hashing",
      "checkingGit",
      "rechecking",
      "installing",
      "movingHistory",
    ]);
    const ready = events.find((e) => e.event === "migrateReady");
    expect(ready?.event === "migrateReady" && canDelete(ready.data.report)).toBe(true);
    expect(await backend.migratePending()).toEqual([{ id, report: ready?.event === "migrateReady" && ready.data.report }]);

    const outcome = await settle(backend.migrateDelete(id, false));
    expect(outcome).toEqual({
      kind: "deleted",
      permanent: false,
      alreadyGone: false,
      shortcut: "C:\\Users\\demo\\Desktop\\marigold-site.lnk",
    });
    expect(events).toContainEqual({ event: "repoRemoved", data: { path: DEMO_WINDOWS_REPO } });
    const repos = (await backend.listRepos()).repos.map((r) => r.path);
    expect(repos).toContain("/home/demo/projects/marigold-site");
    expect(repos).not.toContain(DEMO_WINDOWS_REPO);
    expect(await backend.migratePending()).toEqual([]);
  });

  it("can ask to delete permanently, and stops a snapshot when cancelled", async () => {
    vi.useFakeTimers();
    const backend = new DemoBackend({ moveOutcome: "wontFit" });
    const windows = await backend.migrateWindows(DEMO_WINDOWS_REPO);
    await settle(backend.migrateCloseWindow(DEMO_WINDOWS_REPO, windows[0].id));

    const cancelled = backend.migratePreflight(DEMO_WINDOWS_REPO).catch((e: MigrateError) => e);
    expect(await backend.migrateCancelSnapshot(DEMO_WINDOWS_REPO)).toBe(true);
    expect(await settle(cancelled)).toMatchObject({ kind: "cancelled" });

    const dest = "/home/demo/projects/marigold-site";
    const { id } = await settle(backend.migrateStart(DEMO_WINDOWS_REPO, dest));
    await expect(backend.migrateDelete(id, false)).rejects.toMatchObject({ kind: "notReady" });
    while ((await backend.migratePending()).length === 0) await vi.advanceTimersByTimeAsync(100);
    expect(await settle(backend.migrateDelete(id, false))).toMatchObject({ kind: "wontFit" });
    expect(await settle(backend.migrateDelete(id, true))).toMatchObject({ kind: "deleted", permanent: true });
  });

  it("refuses a destination that exists or isn't on Linux", async () => {
    vi.useFakeTimers();
    const backend = new DemoBackend();
    const taken = await settle(backend.migratePreflight(DEMO_WINDOWS_REPO, "/home/demo/projects/atlas-api"));
    expect(taken.refusals.map((r) => r.kind)).toEqual(["destExists"]);
    const windows = await settle(backend.migratePreflight(DEMO_WINDOWS_REPO, "/mnt/d/elsewhere"));
    expect(windows.refusals.map((r) => r.kind)).toEqual(["destNotLinux"]);
  });
});

describe("demoHookDiff", () => {
  it("shows the command and matchers the real installer writes", () => {
    const diff = demoHookDiff();
    expect(diff).toContain(
      `"command": "[ -f '/home/demo/.local/share/refract/status-hook.sh' ] && sh '/home/demo/.local/share/refract/status-hook.sh' waitingOnUser '/home/demo/.local/state/refract/sessions' || cat >/dev/null; exit 0"`,
    );
    expect(diff).toContain(`{ "matcher": "permission_prompt|elicitation_dialog", "hooks": [`);
    expect(diff).toContain(`{ "matcher": "startup|resume|clear", "hooks": [`);
    expect(diff).toContain(`"PostToolUse": [\n+      { "matcher": "*", "hooks": [`);
    expect(diff).toContain(`"Stop": [\n+      { "hooks": [`);
  });

  it("counts its lines in the hunk header", () => {
    const lines = demoHookDiff().trimEnd().split("\n").slice(3);
    const [, oldCount, newCount] = /^@@ -1,(\d+) \+1,(\d+) @@$/.exec(demoHookDiff().split("\n")[2])!;
    expect(lines.filter((l) => !l.startsWith("+")).length).toBe(Number(oldCount));
    expect(lines.filter((l) => !l.startsWith("-")).length).toBe(Number(newCount));
  });
});
