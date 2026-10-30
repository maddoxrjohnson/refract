import { describe, expect, it } from "vitest";
import {
  busyAgents,
  byteRange,
  canDelete,
  initialMigrations,
  isWindowsDrive,
  migrationsReducer,
  overallProgress,
  stepText,
  windowsPath,
  type MigrationAction,
  type MigrationsState,
} from "./migrations";
import { TEST_DEST, TEST_SOURCE, testReport, testRepo, testSession } from "./ui/testRepo";

const SOURCE = TEST_SOURCE;
const DEST = TEST_DEST;
const GB = 1024 ** 3;

const run = (actions: MigrationAction[], state: MigrationsState = initialMigrations) =>
  actions.reduce(migrationsReducer, state);

describe("migrationsReducer", () => {
  it("follows a migration from Start to its report", () => {
    let s = run([{ type: "starting", source: SOURCE, dest: DEST }]);
    expect(s.get(SOURCE)).toMatchObject({ id: null, phase: { kind: "snapshot", progress: null } });
    const progress = {
      path: SOURCE,
      phase: "hashing" as const,
      filesDone: 1,
      filesTotal: 2,
      bytesDone: 3,
      bytesTotal: 4,
    };
    s = run([{ type: "snapshot", progress }], s);
    expect(s.get(SOURCE)?.phase).toEqual({ kind: "snapshot", progress });
    s = run(
      [
        { type: "started", source: SOURCE, id: "m-1" },
        {
          type: "progress",
          progress: { id: "m-1", source: SOURCE, dest: DEST, step: "copying", done: GB, total: 3 * GB },
        },
      ],
      s,
    );
    expect(s.get(SOURCE)).toMatchObject({ id: "m-1", phase: { kind: "running", step: "copying" } });
    const report = testReport();
    s = run([{ type: "ready", id: "m-1", report }], s);
    expect(s.get(SOURCE)?.phase).toEqual({ kind: "ready", report, deleting: false });
    s = run([{ type: "gone", source: SOURCE }], s);
    expect(s.size).toBe(0);
  });

  it("keeps a begun delete through new reports, and brings it back from the pending list", () => {
    const report = testReport();
    let s = run([
      { type: "ready", id: "m-1", report },
      { type: "deleting", id: "m-1" },
      { type: "ready", id: "m-1", report },
      { type: "pending", list: [{ id: "m-1", report }] },
    ]);
    expect(s.get(SOURCE)?.phase).toEqual({ kind: "ready", report, deleting: true });
    s = run([{ type: "pending", list: [{ id: "m-1", report, deleting: true }] }]);
    expect(s.get(SOURCE)?.phase).toEqual({ kind: "ready", report, deleting: true });
    // Only a report can be marked; a running copy is left alone.
    expect(run([{ type: "deleting", id: "m-9" }])).toBe(initialMigrations);
  });

  it("places progress of a migration this window didn't start by its source", () => {
    const hashing = { id: "m-3", source: SOURCE, dest: DEST, step: "hashing" as const, done: 1, total: 2 };
    const s = run([{ type: "progress", progress: hashing }]);
    expect(s.get(SOURCE)).toEqual({
      source: SOURCE,
      dest: DEST,
      id: "m-3",
      phase: { kind: "running", step: "hashing", done: 1, total: 2 },
    });
    // Started here: the progress takes the entry, and the late answer changes nothing.
    const t = run([
      { type: "starting", source: SOURCE, dest: DEST },
      { type: "progress", progress: { ...hashing, id: "m-4", step: "copying" } },
      { type: "started", source: SOURCE, id: "m-4" },
    ]);
    expect(t.get(SOURCE)).toMatchObject({ id: "m-4", phase: { kind: "running", step: "copying" } });
  });

  it("drops running copies when the helper stops, and keeps reports", () => {
    const other = testReport({ id: "m-5", source: "/mnt/d/other", dest: "/home/dev/other" });
    const s = run([
      { type: "progress", progress: { id: "m-3", source: SOURCE, dest: DEST, step: "hashing", done: 1, total: 2 } },
      { type: "ready", id: "m-5", report: other },
      { type: "starting", source: "/mnt/c/third", dest: "/home/dev/third" },
      { type: "helperStopped" },
    ]);
    expect([...s.keys()].sort()).toEqual(["/mnt/c/third", "/mnt/d/other"]);
    expect(run([{ type: "helperStopped" }])).toBe(initialMigrations);
  });

  it("ignores progress it can't place and snapshots of folders not starting", () => {
    const s = run([
      { type: "progress", progress: { id: "elsewhere", source: "", dest: "", step: "hashing", done: 1, total: 2 } },
      {
        type: "snapshot",
        progress: { path: SOURCE, phase: "walking", filesDone: 1, filesTotal: 2, bytesDone: 0, bytesTotal: 0 },
      },
    ]);
    expect(s).toBe(initialMigrations);
  });

  it("keeps a report that arrives before migrateStart answers", () => {
    const report = testReport();
    const s = run([
      { type: "starting", source: SOURCE, dest: DEST },
      { type: "ready", id: "m-1", report },
      { type: "started", source: SOURCE, id: "m-1" },
    ]);
    expect(s.get(SOURCE)).toMatchObject({ id: "m-1", phase: { kind: "ready" } });
  });

  it("takes pending reports, keyed by their source, and re-syncs them", () => {
    let s = run([{ type: "pending", list: [{ id: "m-7", report: testReport({ id: "m-7" }) }] }]);
    expect(s.get(SOURCE)).toMatchObject({ id: "m-7", dest: DEST, phase: { kind: "ready" } });
    s = run([{ type: "resyncing", id: "m-7" }], s);
    expect(s.get(SOURCE)?.phase).toEqual({ kind: "running", step: "rechecking", done: 0, total: 0 });
  });
});

describe("the step line", () => {
  it("reads like the spec", () => {
    expect(stepText({ kind: "running", step: "copying", done: 1.2 * GB, total: 3.4 * GB })).toBe(
      "Copying 1.2 / 3.4 GB",
    );
    expect(stepText({ kind: "running", step: "copying", done: 50 * 1024 ** 2, total: 200 * 1024 ** 2 })).toBe(
      "Copying 50 / 200 MB",
    );
    expect(stepText({ kind: "running", step: "hashing", done: 1, total: 2 })).toBe("Hashing");
    expect(stepText({ kind: "running", step: "checkingGit", done: 1, total: 2 })).toBe("Checking git");
    expect(stepText({ kind: "running", step: "installing", done: 0, total: 1 })).toBe("Installing");
    expect(stepText({ kind: "running", step: null, done: 0, total: 0 })).toBe("Starting");
    expect(stepText({ kind: "snapshot", progress: null })).toBe("Reading the folder");
    expect(
      stepText({
        kind: "snapshot",
        progress: { path: SOURCE, phase: "walking", filesDone: 1200, filesTotal: 3400, bytesDone: 0, bytesTotal: 0 },
      }),
    ).toBe("Listing files 1,200 / 3,400");
    expect(byteRange(0, 10)).toBe("0 / 1 MB");
  });

  it("says whether the report waits for an OK", () => {
    expect(stepText({ kind: "ready", report: testReport(), deleting: false })).toBe("Ready for your OK");
    const failed = testReport({
      checks: testReport().checks.map((c) => (c.check === "files" ? { ...c, status: "failed" } : c)),
    });
    expect(stepText({ kind: "ready", report: failed, deleting: false })).toBe("A check failed");
    expect(stepText({ kind: "ready", report: testReport(), deleting: true })).toBe("Deleting didn't finish");
  });

  it("only moves forward across the steps", () => {
    const steps = ["copying", "normalising", "hashing", "checkingGit", "rechecking", "movingHistory", "installing"] as const;
    let last = overallProgress({
      kind: "snapshot",
      progress: { path: SOURCE, phase: "hashing", filesDone: 9, filesTotal: 10, bytesDone: 9, bytesTotal: 10 },
    });
    for (const step of steps) {
      for (const done of [0, 5, 10]) {
        const p = overallProgress({ kind: "running", step, done, total: 10 });
        expect(p).toBeGreaterThanOrEqual(last);
        last = p;
      }
    }
    expect(last).toBeLessThanOrEqual(1);
    expect(overallProgress({ kind: "ready", report: testReport(), deleting: false })).toBe(1);
  });
});

describe("canDelete", () => {
  it("needs the four guarding checks passed; history doesn't count", () => {
    expect(canDelete(testReport())).toBe(true);
    const historyFailed = testReport({
      checks: testReport().checks.map((c) => (c.check === "history" ? { ...c, status: "failed" } : c)),
    });
    expect(canDelete(historyFailed)).toBe(true);
    const notRun = testReport({
      checks: testReport().checks.map((c) => (c.check === "unchanged" ? { ...c, status: "notRun" } : c)),
    });
    expect(canDelete(notRun)).toBe(false);
    expect(canDelete(testReport({ checks: [] }))).toBe(false);
  });
});

describe("paths", () => {
  it("knows a Windows drive and its Windows path", () => {
    expect(isWindowsDrive(SOURCE)).toBe(true);
    expect(isWindowsDrive("/mnt/D/work/x")).toBe(true);
    expect(isWindowsDrive("/mnt/wsl/x")).toBe(false);
    expect(isWindowsDrive("/home/dev/projects/x")).toBe(false);
    expect(isWindowsDrive("remote:github.com/demo-user/x")).toBe(false);
    expect(windowsPath(SOURCE)).toBe("C:\\Users\\dev\\Desktop\\aurora-notes");
    expect(windowsPath("/mnt/d")).toBe("D:\\");
  });
});

describe("busyAgents", () => {
  it("lists live agents; a working one can't be stopped", () => {
    const repo = testRepo({
      sessions: [
        testSession("a", { kind: "live", detail: "working" }, 1),
        testSession("b", { kind: "live", detail: "waitingOnUser" }, 1),
        testSession("c", { kind: "finished", detail: "completed" }, 1),
        testSession("d", { kind: "interrupted", detail: "apiError" }, 1),
      ],
    });
    expect(busyAgents(repo).map((a) => [a.session.id, a.working])).toEqual([
      ["a", true],
      ["b", false],
    ]);
    expect(busyAgents(undefined)).toEqual([]);
  });
});
