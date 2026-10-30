import { act, cleanup, fireEvent, render, screen } from "@testing-library/react";
import { afterEach, describe, expect, it, vi } from "vitest";
import type { Backend } from "../backend/Backend";
import type { AgentEvent, DeleteOutcome, MigratePreflight, MigrationReport, RepoSnapshot } from "../backend/types";
import { useMigrations } from "../hooks/useMigrations";
import { MoveDialog } from "./MoveDialog";
import { TEST_DEST, TEST_SOURCE, testReport, testRepo, testSession } from "./testRepo";

afterEach(cleanup);

const PREFLIGHT: MigratePreflight = {
  source: TEST_SOURCE,
  windowsPath: "C:\\Users\\dev\\Desktop\\aurora-notes",
  name: "aurora-notes",
  dest: TEST_DEST,
  refusals: [],
  warnings: [],
  files: 120,
  dirs: 14,
  bytes: 3 * 1024 ** 3,
  skipped: [{ path: "node_modules", rule: "nodeModules", bytes: 400 * 1024 ** 2, files: 30_000 }],
  install: [{ folder: "node_modules", tool: "npm", command: "npm ci", installHint: "install node" }],
  droppedStreams: 0,
  vscodeWindows: [],
  linuxGit: false,
};

function fakeBackend(preflight: Partial<MigratePreflight> = {}, pending: MigrationReport[] = []) {
  const listeners = new Set<(e: AgentEvent) => void>();
  return {
    onAgentEvent: vi.fn((cb: (e: AgentEvent) => void) => {
      listeners.add(cb);
      return () => listeners.delete(cb);
    }),
    onMigrateSnapshot: vi.fn(() => () => {}),
    migratePending: vi.fn().mockResolvedValue(pending.map((report) => ({ id: report.id, report }))),
    migratePreflight: vi.fn().mockResolvedValue({ ...PREFLIGHT, ...preflight }),
    migrateCancelSnapshot: vi.fn().mockResolvedValue(false),
    migrateStart: vi.fn().mockResolvedValue({ id: "m-1" }),
    migrateWindows: vi.fn().mockResolvedValue([]),
    migrateCloseWindow: vi.fn().mockResolvedValue(true),
    migrateStopAgent: vi.fn().mockResolvedValue(undefined),
    migrateResync: vi.fn().mockResolvedValue(undefined),
    migrateDelete: vi.fn<(id: string, permanent: boolean) => Promise<DeleteOutcome>>(),
    migrateFinish: vi.fn().mockResolvedValue(undefined),
    migrateCancel: vi.fn().mockResolvedValue(undefined),
    send: (e: AgentEvent) => listeners.forEach((cb) => cb(e)),
  };
}

function Harness(props: { backend: Backend; repo?: RepoSnapshot; onClose(): void; onKeep(s: string): void }) {
  const moves = useMigrations(props.backend, true, 1);
  return (
    <MoveDialog
      backend={props.backend}
      source={TEST_SOURCE}
      name="aurora-notes"
      repo={props.repo}
      migration={moves.state.get(TEST_SOURCE)}
      moves={moves}
      onClose={props.onClose}
      onKeep={props.onKeep}
      onMoved={() => {}}
    />
  );
}

async function setup(backend: ReturnType<typeof fakeBackend>, repo?: RepoSnapshot) {
  const props = { onClose: vi.fn(), onKeep: vi.fn() };
  render(<Harness backend={backend as unknown as Backend} repo={repo} {...props} />);
  await act(async () => {});
  return { props, dialog: screen.getByRole("dialog") };
}

const button = (name: string | RegExp) => screen.getByRole("button", { name });

describe("MoveDialog preflight", () => {
  it("shows what will happen and keeps Start off while a window or agent has the folder", async () => {
    const backend = fakeBackend({ vscodeWindows: [{ id: 7, title: "aurora-notes - Visual Studio Code" }] });
    const repo = testRepo({
      path: TEST_SOURCE,
      sessions: [
        testSession("busy", { kind: "live", detail: "working" }, 1),
        testSession("idle", { kind: "live", detail: "waitingOnUser" }, 1),
      ],
    });
    const { dialog } = await setup(backend, repo);
    expect(screen.getByLabelText("Destination in WSL")).toHaveProperty("value", TEST_DEST);
    expect(dialog.textContent).toContain("3 GB in 120 files");
    expect(dialog.textContent).toContain("node_modules");
    expect(dialog.textContent).toContain("npm ci");
    expect(button("Start")).toHaveProperty("disabled", true);
    expect(button("Agent is working")).toHaveProperty("disabled", true);

    fireEvent.click(button(/Close VS Code window/));
    await act(async () => {});
    expect(backend.migrateCloseWindow).toHaveBeenCalledWith(TEST_SOURCE, 7);
    expect(screen.queryByRole("button", { name: /Close VS Code window/ })).toBeNull();

    fireEvent.click(button("Stop"));
    fireEvent.click(button("Stop agent"));
    await act(async () => {});
    expect(backend.migrateStopAgent).toHaveBeenCalledWith(TEST_SOURCE, "idle");
    // The working agent still blocks Start.
    expect(button("Start")).toHaveProperty("disabled", true);
  });

  it("starts once nothing blocks, and checks a new destination again", async () => {
    vi.useFakeTimers();
    try {
      const backend = fakeBackend();
      await setup(backend);
      fireEvent.change(screen.getByLabelText("Destination in WSL"), { target: { value: "/home/dev/elsewhere" } });
      expect(button("Start")).toHaveProperty("disabled", true);
      await act(async () => {
        await vi.advanceTimersByTimeAsync(700);
      });
      expect(backend.migratePreflight).toHaveBeenLastCalledWith(TEST_SOURCE, "/home/dev/elsewhere");
      fireEvent.click(button("Start"));
      await act(async () => {});
      expect(backend.migrateStart).toHaveBeenCalledWith(TEST_SOURCE, "/home/dev/elsewhere");
      expect(screen.getByRole("button", { name: "Cancel the move" })).toBeTruthy();
    } finally {
      vi.useRealTimers();
    }
  });

  it("shows refusals instead of Start", async () => {
    const backend = fakeBackend({
      refusals: [{ kind: "operationInProgress", operation: "rebase", message: "A rebase is in progress." }],
    });
    const { dialog } = await setup(backend);
    expect(dialog.textContent).toContain("A rebase is in progress.");
    expect(screen.queryByRole("button", { name: "Start" })).toBeNull();
  });
});

describe("MoveDialog report", () => {
  it("offers only Discard and Retry when a check failed, with its advice", async () => {
    const report = testReport({
      checks: testReport().checks.map((c) =>
        c.check === "gitState"
          ? { ...c, status: "failed", problems: [{ message: "refs/stash differs" }], advice: "Run git fsck." }
          : c,
      ),
    });
    const { dialog } = await setup(fakeBackend({}, [report]));
    expect(dialog.textContent).toContain("refs/stash differs");
    expect(dialog.textContent).toContain("Run git fsck.");
    expect(screen.queryByRole("button", { name: "Delete original" })).toBeNull();
    expect(button("Discard the copy")).toBeTruthy();
    expect(button("Retry")).toBeTruthy();
  });

  it("asks before deleting permanently, with No as the default", async () => {
    const backend = fakeBackend({}, [testReport()]);
    backend.migrateDelete.mockResolvedValueOnce({ kind: "wontFit", reason: "The bin is full." });
    const { props } = await setup(backend);
    expect(document.activeElement).toBe(button("Delete original"));
    fireEvent.click(button("Delete original"));
    await act(async () => {});
    expect(screen.getByText("This won't fit in the Recycle Bin. Delete permanently?")).toBeTruthy();
    expect(document.activeElement).toBe(button("No"));
    fireEvent.click(button("No"));
    await act(async () => {});
    expect(backend.migrateFinish).toHaveBeenCalledWith("m-1", "keep");
    expect(props.onKeep).toHaveBeenCalledWith(TEST_SOURCE);
  });

  it("ignores Escape while deleting, then shows where it went", async () => {
    const backend = fakeBackend({}, [testReport()]);
    let answer: (o: DeleteOutcome) => void = () => {};
    backend.migrateDelete.mockReturnValueOnce(new Promise((r) => (answer = r)));
    const { props, dialog } = await setup(backend);
    fireEvent.click(button("Delete original"));
    fireEvent.keyDown(dialog, { key: "Escape" });
    expect(props.onClose).not.toHaveBeenCalled();
    await act(async () => answer({ kind: "deleted", permanent: false, shortcut: "C:\\x\\aurora-notes.lnk" }));
    expect(dialog.textContent).toContain("The original is in the Recycle Bin.");
    expect(dialog.textContent).toContain("C:\\x\\aurora-notes.lnk");
    fireEvent.keyDown(dialog, { key: "Escape" });
    expect(props.onClose).toHaveBeenCalled();
  });

  it("offers Re-sync when the original changed, and Close for open windows", async () => {
    const backend = fakeBackend({}, [testReport()]);
    backend.migrateDelete
      .mockResolvedValueOnce({ kind: "windowsOpen", windows: [{ id: 3, title: "aurora-notes - VS Code" }] })
      .mockResolvedValueOnce({
        kind: "changed",
        changed: ["src/main.ts"],
        busyAgents: ["An agent is still working in aurora-notes."],
      });
    const { dialog } = await setup(backend);
    fireEvent.click(button("Delete original"));
    await act(async () => {});
    fireEvent.click(button(/Close VS Code window/));
    await act(async () => {});
    expect(backend.migrateCloseWindow).toHaveBeenCalledWith(TEST_SOURCE, 3);
    fireEvent.click(button("Try again"));
    await act(async () => {});
    expect(dialog.textContent).toContain("src/main.ts");
    expect(dialog.textContent).toContain("An agent is still working in aurora-notes.");
    fireEvent.click(button("Re-sync"));
    await act(async () => {});
    expect(backend.migrateResync).toHaveBeenCalledWith("m-1");
    expect(button("Cancel the move")).toBeTruthy();
  });
});
