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
  install: [{ folder: "node_modules", tool: "npm", command: "npm ci", installHint: "install node", missing: false }],
  droppedStreams: 0,
  historySessions: 2,
  vscodeWindows: [],
  linuxGit: false,
  cloudSynced: false,
};

const RECHECK = { changed: [], removed: [], unreadable: [], copyChanged: [], busyAgents: [], sourceMissing: false };

function fakeBackend(preflight: Partial<MigratePreflight> = {}, pending: MigrationReport[] = [], deleting = false) {
  const listeners = new Set<(e: AgentEvent) => void>();
  return {
    onAgentEvent: vi.fn((cb: (e: AgentEvent) => void) => {
      listeners.add(cb);
      return () => listeners.delete(cb);
    }),
    onMigrateSnapshot: vi.fn(() => () => {}),
    migratePending: vi.fn().mockResolvedValue(pending.map((report) => ({ id: report.id, report, deleting }))),
    migrateSourceExists: vi.fn().mockResolvedValue(true),
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

  it("says which install tool is missing and how many sessions move", async () => {
    const backend = fakeBackend({ install: [{ ...PREFLIGHT.install[0], missing: true }], historySessions: 2 });
    const { dialog } = await setup(backend);
    expect(dialog.textContent).toContain("npm isn't installed in WSL");
    expect(dialog.textContent).toContain("install node");
    expect(dialog.textContent).toContain("2 Claude sessions will move with it.");
  });

  it("shows the report's own check titles", async () => {
    const report = testReport();
    report.checks[0] = { ...report.checks[0], title: "Every file matches, byte for byte" };
    const { dialog } = await setup(fakeBackend({}, [report]));
    expect(dialog.textContent).toContain("Every file matches, byte for byte");
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
    backend.migrateDelete.mockResolvedValueOnce({ kind: "wontFit", reason: "The bin is full.", deleting: false });
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
    await act(async () =>
      answer({ kind: "deleted", permanent: false, alreadyGone: false, shortcut: "C:\\x\\aurora-notes.lnk" }),
    );
    expect(dialog.textContent).toContain("The original is in the Recycle Bin.");
    expect(dialog.textContent).toContain("C:\\x\\aurora-notes.lnk");
    fireEvent.keyDown(dialog, { key: "Escape" });
    expect(props.onClose).toHaveBeenCalled();
  });

  it("offers Re-sync when the original changed, and Close for open windows", async () => {
    const backend = fakeBackend({}, [testReport()]);
    backend.migrateDelete
      .mockResolvedValueOnce({
        kind: "windowsOpen",
        windows: [{ id: 3, title: "aurora-notes - VS Code" }],
        deleting: false,
      })
      .mockResolvedValueOnce({
        ...RECHECK,
        kind: "changed",
        changed: ["src/main.ts"],
        busyAgents: ["An agent is still working in aurora-notes."],
        deleting: false,
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
    // Focus follows the new view, so Escape still reaches the dialog.
    expect(document.activeElement).toBe(button("Cancel the move"));
  });

  it("goes back to the report when Re-sync is refused", async () => {
    const backend = fakeBackend({}, [testReport()]);
    backend.migrateDelete.mockResolvedValueOnce({ ...RECHECK, kind: "changed", changed: ["a.txt"], deleting: false });
    backend.migrateResync.mockRejectedValueOnce({ kind: "agent", message: "Cannot re-sync: the helper said no" });
    const { dialog } = await setup(backend);
    fireEvent.click(button("Delete original"));
    await act(async () => {});
    fireEvent.click(button("Re-sync"));
    await act(async () => {});
    expect(screen.queryByRole("button", { name: "Cancel the move" })).toBeNull();
    expect(dialog.textContent).toContain("the helper said no");
    expect(button("Delete original")).toBeTruthy();
  });

  it("offers Stop for an idle agent and Try again, not Re-sync, when only agents block", async () => {
    const backend = fakeBackend({}, [testReport()]);
    backend.migrateDelete.mockResolvedValueOnce({
      ...RECHECK,
      kind: "changed",
      busyAgents: ["An agent (pid 7) is running in aurora-notes."],
      deleting: false,
    });
    const repo = testRepo({ sessions: [testSession("idle", { kind: "live", detail: "waitingOnUser" }, 1)] });
    await setup(backend, repo);
    fireEvent.click(button("Delete original"));
    await act(async () => {});
    expect(screen.queryByRole("button", { name: "Re-sync" })).toBeNull();
    fireEvent.click(button("Stop"));
    fireEvent.click(button("Stop agent"));
    await act(async () => {});
    expect(backend.migrateStopAgent).toHaveBeenCalledWith(TEST_SOURCE, repo.sessions[0].id);
    expect(button("Try again")).toBeTruthy();
    expect(button("Keep both")).toBeTruthy();
  });

  it("offers only Keep both when the copy changed after the check", async () => {
    const backend = fakeBackend({}, [testReport()]);
    backend.migrateDelete.mockResolvedValueOnce({
      ...RECHECK,
      kind: "changed",
      changed: ["a.txt"],
      copyChanged: ["notes.md"],
      deleting: false,
    });
    const { dialog } = await setup(backend);
    fireEvent.click(button("Delete original"));
    await act(async () => {});
    expect(dialog.textContent).toContain("The copy in WSL changed");
    expect(screen.queryByRole("button", { name: "Re-sync" })).toBeNull();
    expect(screen.queryByRole("button", { name: "Discard the copy" })).toBeNull();
    expect(document.activeElement).toBe(button("Keep both"));
  });

  it("offers only ending the move when the copy is gone", async () => {
    const backend = fakeBackend({}, [testReport()]);
    backend.migrateDelete.mockResolvedValueOnce({ ...RECHECK, kind: "changed", copyChanged: ["."], deleting: false });
    const { dialog } = await setup(backend);
    fireEvent.click(button("Delete original"));
    await act(async () => {});
    expect(dialog.textContent).toContain("The copy in WSL is gone");
    expect(screen.queryByRole("button", { name: "Keep both" })).toBeNull();
    fireEvent.click(button("End the move"));
    await act(async () => {});
    expect(backend.migrateFinish).toHaveBeenCalledWith(expect.any(String), "discard");
  });

  it("refuses when WSL can't see the original's drive, and lists unreadable and removed paths", async () => {
    const backend = fakeBackend({}, [testReport()]);
    backend.migrateDelete
      .mockResolvedValueOnce({ kind: "driveHidden", deleting: false })
      .mockResolvedValueOnce({ ...RECHECK, kind: "changed", unreadable: ["locked.db"], removed: ["old.txt"], deleting: false });
    const { dialog } = await setup(backend);
    fireEvent.click(button("Delete original"));
    await act(async () => {});
    expect(dialog.textContent).toContain("WSL can't see the drive");
    fireEvent.click(button("Try again"));
    await act(async () => {});
    expect(dialog.textContent).toContain("locked.db");
    expect(dialog.textContent).toContain("can't tell whether they changed");
    expect(dialog.textContent).toContain("old.txt");
    expect(dialog.textContent).toContain("the copy still has them");
  });

  it("after a partial delete offers only Finish deleting, which finishes", async () => {
    const backend = fakeBackend({}, [testReport()]);
    backend.migrateDelete
      .mockResolvedValueOnce({ kind: "unfinished", reason: "Cannot delete the original: a file is in use" })
      .mockResolvedValueOnce({ kind: "deleted", permanent: false, alreadyGone: false });
    const { dialog } = await setup(backend);
    fireEvent.click(button("Delete original"));
    await act(async () => {});
    expect(dialog.textContent).toContain("Deleting the original didn't finish.");
    expect(dialog.textContent).toContain("a file is in use");
    expect(dialog.textContent).not.toContain("nothing was deleted");
    for (const name of ["Re-sync", "Keep both", "Discard the copy", "Delete original"]) {
      expect(screen.queryByRole("button", { name })).toBeNull();
    }
    expect(document.activeElement).toBe(button("Finish deleting"));
    fireEvent.click(button("Finish deleting"));
    await act(async () => {});
    expect(backend.migrateDelete).toHaveBeenLastCalledWith("m-1", false);
    expect(dialog.textContent).toContain("The original is in the Recycle Bin.");
  });

  it("comes back as Finish deleting after a restart", async () => {
    const backend = fakeBackend({}, [testReport()], true);
    const { dialog } = await setup(backend);
    expect(dialog.textContent).toContain("Deleting the original didn't finish.");
    expect(button("Finish deleting")).toBeTruthy();
    expect(screen.queryByRole("button", { name: "Keep both" })).toBeNull();
    backend.migrateDelete.mockResolvedValueOnce({ ...RECHECK, kind: "changed", changed: ["new.txt"], deleting: true });
    fireEvent.click(button("Finish deleting"));
    await act(async () => {});
    expect(dialog.textContent).toContain("new.txt");
    expect(screen.queryByRole("button", { name: "Re-sync" })).toBeNull();
    expect(button("Finish deleting")).toBeTruthy();
  });

  it("offers to finish when the original was deleted by hand", async () => {
    const backend = fakeBackend({}, [testReport()]);
    backend.migrateSourceExists.mockResolvedValue(false);
    backend.migrateDelete.mockResolvedValueOnce({ kind: "deleted", permanent: false, alreadyGone: true });
    const { dialog } = await setup(backend);
    expect(dialog.textContent).toContain("The original folder is already gone.");
    expect(screen.queryByRole("button", { name: "Discard the copy" })).toBeNull();
    fireEvent.click(button("Finish the move"));
    await act(async () => {});
    expect(dialog.textContent).toContain("was already gone");
  });

  it("ignores a held Enter", async () => {
    const backend = fakeBackend({}, [testReport()]);
    const { dialog } = await setup(backend);
    fireEvent.keyDown(dialog, { key: "Enter", repeat: true });
    expect(backend.migrateDelete).not.toHaveBeenCalled();
    const ev = new KeyboardEvent("keydown", { key: "Enter", repeat: true, bubbles: true, cancelable: true });
    button("Delete original").dispatchEvent(ev);
    expect(ev.defaultPrevented).toBe(true);
  });
});
