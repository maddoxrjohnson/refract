import { afterEach, describe, expect, it, vi } from "vitest";
import { canDelete } from "../migrations";
import {
  DEMO_WINDOWS_REPO,
  DEMO_WINDOWS_WORKTREE,
  DEMO_WORKTREE_MAIN,
  DemoBackend,
  demoHookDiff,
  demoRepos,
} from "./DemoBackend";
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
    expect(diff).toContain(`"PreToolUse": [\n+      { "matcher": "AskUserQuestion|ExitPlanMode", "hooks": [`);
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

describe("DemoBackend's refactors", () => {
  const DOTFILES = "/home/demo/projects/dotfiles";
  const SWEEP = "/home/demo/projects/sweep";

  it("seeds one ready refactor and gives every repo a short sha", () => {
    const repos = demoRepos();
    expect(repos.filter((r) => r.refactor).map((r) => [r.name, r.refactor!.state])).toEqual([["sweep", "ready"]]);
    for (const r of repos) expect(r.lastCommit?.sha).toMatch(/^[0-9a-f]{7}$/);
  });

  it("runs start, plan, work and ready, then merges", async () => {
    vi.useFakeTimers();
    const backend = new DemoBackend();
    const states: string[] = [];
    backend.onAgentEvent((e) => {
      if (e.event === "repoChanged" && e.data.path === DOTFILES) states.push(e.data.refactor?.state ?? "none");
    });
    const launches: string[] = [];
    backend.onDemoLaunch((l) => launches.push(`${l.mode.kind}:${l.mode.kind === "new" ? l.mode.planMode : ""}`));
    const ticket = await settle(backend.startRefactor(DOTFILES, "the aliases"));
    expect(ticket.folderUri).toContain("/home/demo/projects/.refract-worktrees/dotfiles-");
    expect(launches).toEqual(["new:true"]);
    await expect(settle(backend.startRefactor(DOTFILES))).rejects.toMatchObject({ message: "This repo already has a refactor." });
    const notices: string[] = [];
    backend.onRefactorNotice((n) => notices.push(`${n.repoName}:${n.alert}`));
    await vi.advanceTimersByTimeAsync(20_000);
    expect(states).toEqual(["starting", "working", "needsYou", "working", "ready"]);
    expect(notices).toEqual(["dotfiles:planReady", "dotfiles:finished"]);
    const repo = await backend.getRepo(DOTFILES);
    expect(repo.refactor).toMatchObject({ live: false, filesChanged: 7, summary: expect.any(String) });
    expect(repo.refactor!.checks!.map((c) => c.command)).toEqual(["shellcheck *.sh"]);

    expect(await settle(backend.mergeRefactor(DOTFILES))).toEqual({ outcome: "merged", commits: 3 });
    const merged = await backend.getRepo(DOTFILES);
    expect(merged.refactor).toBeNull();
    expect(merged.lastCommit?.summary).toBe("Give the request handlers clearer names");
    expect(merged.ahead).toBe(3);
  });

  it("refuses to discard while live, discards the seeded one, and blocks a merge over a clashing file", async () => {
    vi.useFakeTimers();
    const backend = new DemoBackend();
    const notices: unknown[] = [];
    backend.onRefactorNotice((n) => notices.push(n));
    await backend.setZoomed("/home/demo/projects/ledger-sync");
    await settle(backend.startRefactor("/home/demo/projects/ledger-sync"));
    await vi.advanceTimersByTimeAsync(1500);
    await expect(settle(backend.discardRefactor("/home/demo/projects/ledger-sync"))).rejects.toMatchObject({ kind: "refactor" });
    await vi.advanceTimersByTimeAsync(20_000);
    expect(await settle(backend.mergeRefactor("/home/demo/projects/ledger-sync"))).toEqual({
      outcome: "blocked",
      reason: "dirtyOverlap",
      files: ["src/file-1.txt"],
    });
    expect((await backend.getRepo("/home/demo/projects/ledger-sync")).refactor?.state).toBe("mergeBlocked");
    // The zoomed repo's panel shows its own changes.
    expect(notices).toEqual([]);

    await settle(backend.discardRefactor(SWEEP));
    expect((await backend.getRepo(SWEEP)).refactor).toBeNull();
  });

  it("resumes back to ready and opens the refactor's window as the pretend terminal", async () => {
    vi.useFakeTimers();
    const backend = new DemoBackend();
    const modes: string[] = [];
    backend.onDemoLaunch((l) => modes.push(l.mode.kind));
    await backend.openRefactor(SWEEP);
    await settle(backend.resumeRefactor(SWEEP, false));
    expect((await backend.getRepo(SWEEP)).refactor).toMatchObject({ state: "working", live: true });
    await vi.advanceTimersByTimeAsync(5000);
    expect((await backend.getRepo(SWEEP)).refactor?.state).toBe("ready");
    expect(modes).toEqual(["focus", "resume"]);
  });
});

describe("DemoBackend's worktrees", () => {
  it("seeds three linked worktrees of one repo, one ready to remove and one to check", () => {
    const linked = demoRepos().filter((r) => r.worktreeOf);
    expect(linked.map((r) => r.worktreeOf)).toEqual([DEMO_WORKTREE_MAIN, DEMO_WORKTREE_MAIN, DEMO_WORKTREE_MAIN]);
    const [cards, search, tablet] = linked;
    expect(tablet.path).toBe(DEMO_WINDOWS_WORKTREE);
    expect(tablet.worktree).toMatchObject({ status: { kind: "unchecked" }, removable: false, checkable: true });
    expect(cards.worktree).toMatchObject({ status: { kind: "release", tag: "v1.4.0" }, removable: true });
    expect(cards.sessions[0].lastAssistantText).toBe("Should I tag v1.4.0?");
    expect(search.worktree).toMatchObject({ status: { kind: "changes", count: 3 }, removable: false });
  });

  it("removes the ready one as the helper does, and refuses the dirty one", async () => {
    vi.useFakeTimers();
    const backend = new DemoBackend();
    const events: AgentEvent[] = [];
    backend.onAgentEvent((e) => events.push(e));
    const [cards, search] = (await backend.listRepos()).repos.filter((r) => r.worktreeOf);
    await expect(settle(backend.removeWorktree(search.path))).rejects.toEqual({
      kind: "worktree",
      message: "Cannot remove the worktree: This worktree has uncommitted changes.",
    });
    await expect(settle(backend.removeWorktree(DEMO_WORKTREE_MAIN))).rejects.toMatchObject({ kind: "worktree" });
    // Each job ends with `worktreeJobDone`, a refused one with the reason.
    expect(events).toEqual([
      {
        event: "worktreeJobDone",
        data: { path: search.path, error: "Cannot remove the worktree: This worktree has uncommitted changes." },
      },
      { event: "worktreeJobDone", data: { path: DEMO_WORKTREE_MAIN, error: "Cannot remove the worktree: Not a linked worktree." } },
    ]);
    events.length = 0;
    await settle(backend.removeWorktree(cards.path, cards.worktree!.ignored));
    expect(events.map((e) => e.event)).toEqual(["repoRemoved", "repoChanged", "worktreeJobDone"]);
    expect(events[0]).toEqual({ event: "repoRemoved", data: { path: cards.path } });
    expect(events[1]).toMatchObject({ data: { path: DEMO_WORKTREE_MAIN } });
    expect(events[2]).toEqual({ event: "worktreeJobDone", data: { path: cards.path } });
    expect((await backend.listRepos()).repos.some((r) => r.path === cards.path)).toBe(false);
  });

  it("checks the Windows-drive worktree slowly, refuses a Remove that missed a new ignored file, then removes it", async () => {
    vi.useFakeTimers();
    const backend = new DemoBackend();
    const events: AgentEvent[] = [];
    backend.onAgentEvent((e) => events.push(e));
    let done = false;
    const check = backend.checkWorktree(DEMO_WINDOWS_WORKTREE).finally(() => (done = true));
    await vi.advanceTimersByTimeAsync(500);
    expect(done).toBe(false);
    await expect(settle(check)).resolves.toEqual({ status: { kind: "pushed" }, removable: true, ignored: ["node_modules/"] });
    await expect(settle(backend.checkWorktree(DEMO_WORKTREE_MAIN))).rejects.toMatchObject({ kind: "worktree" });
    expect(events).toEqual([]);
    // A program on Windows wrote `.env` after the check, so the confirm's list misses it.
    const refusal = "Cannot remove the worktree: New ignored files appeared since you confirmed (.env). Check again.";
    await expect(settle(backend.removeWorktree(DEMO_WINDOWS_WORKTREE, ["node_modules/"]))).rejects.toEqual({
      kind: "worktree",
      message: refusal,
    });
    expect(events).toEqual([{ event: "worktreeJobDone", data: { path: DEMO_WINDOWS_WORKTREE, error: refusal } }]);
    events.length = 0;
    const again = await settle(backend.checkWorktree(DEMO_WINDOWS_WORKTREE));
    expect(again.ignored).toEqual(["node_modules/", ".env"]);
    await settle(backend.removeWorktree(DEMO_WINDOWS_WORKTREE, again.ignored));
    expect(events.map((e) => e.event)).toEqual(["repoRemoved", "repoChanged", "worktreeJobDone"]);
    expect(events[0]).toEqual({ event: "repoRemoved", data: { path: DEMO_WINDOWS_WORKTREE } });
  });

  it("lists a missing worktree under its repo and cleans it up, keeping the others", async () => {
    vi.useFakeTimers();
    const backend = new DemoBackend();
    const events: AgentEvent[] = [];
    backend.onAgentEvent((e) => events.push(e));
    const main = await backend.getRepo(DEMO_WORKTREE_MAIN);
    expect(main.missingWorktrees).toEqual([
      { path: "/home/demo/projects/harbor-android-offline", branch: "offline-maps", status: { kind: "gone" }, cleanable: true },
    ]);
    expect(demoRepos().filter((r) => r.missingWorktrees).map((r) => r.path)).toEqual([DEMO_WORKTREE_MAIN]);
    await expect(settle(backend.pruneWorktree(DEMO_WORKTREE_MAIN, "/home/demo/projects/elsewhere"))).rejects.toMatchObject({
      kind: "worktree",
    });
    expect(events).toEqual([
      {
        event: "worktreeJobDone",
        data: { path: "/home/demo/projects/elsewhere", error: "Cannot clean up the worktree: Git does not list it as missing." },
      },
    ]);
    events.length = 0;
    await settle(backend.pruneWorktree(DEMO_WORKTREE_MAIN, "/home/demo/projects/harbor-android-offline"));
    expect(events.map((e) => e.event)).toEqual(["repoChanged", "worktreeJobDone"]);
    expect(events[1]).toEqual({ event: "worktreeJobDone", data: { path: "/home/demo/projects/harbor-android-offline" } });
    const next = (events[0] as Extract<AgentEvent, { event: "repoChanged" }>).data;
    expect(next.path).toBe(DEMO_WORKTREE_MAIN);
    expect(next.missingWorktrees).toBeUndefined();
    expect((await backend.getRepo(DEMO_WORKTREE_MAIN)).missingWorktrees).toBeUndefined();
    expect((await backend.listRepos()).repos.filter((r) => r.worktreeOf)).toHaveLength(3);
  });
});
