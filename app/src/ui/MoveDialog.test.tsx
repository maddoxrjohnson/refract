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

function fakeBackend(
  preflight: Partial<MigratePreflight> = {},
  pending: MigrationReport[] = [],
  deleting = false,
  copyMissing = false,
) {
  const listeners = new Set<(e: AgentEvent) => void>();
  return {
    onAgentEvent: vi.fn((cb: (e: AgentEvent) => void) => {
      listeners.add(cb);
      return () => listeners.delete(cb);
    }),
    onMigrateSnapshot: vi.fn(() => () => {}),
    migratePending: vi.fn().mockResolvedValue(pending.map((report) => ({ id: report.id, report, deleting, copyMissing }))),
    migrateSourceExists: vi.fn().mockResolvedValue(true),
    migratePreflight: vi.fn().mockResolvedValue({ ...PREFLIGHT, ...preflight }),
    migrateCancelSnapshot: vi.fn().mockResolvedValue(false),
    migrateStart: vi.fn().mockResolvedValue({ id: "m-1" }),
    migrateWindows: vi.fn().mockResolvedValue([]),
    migrateCloseWindow: vi.fn().mockResolvedValue(true),
    migrateStopAgent: vi.fn().mockResolvedValue(undefined),
    migrateResync: vi.fn().mockResolvedValue(undefined),
    migrateDelete: vi.fn<(id: string, permanent: boolean, force?: string[]) => Promise<DeleteOutcome>>(),
    migrateFinish: vi.fn().mockResolvedValue(undefined),
    migrateCancel: vi.fn().mockResolvedValue({ ended: true }),
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
    // The copy is in place, so stopping the re-sync keeps it.
    expect(button("Stop and keep the copy")).toBeTruthy();
    expect(screen.getByText(/The copy is already in place, so stopping keeps it/)).toBeTruthy();
    // Focus follows the new view, so Escape still reaches the dialog.
    expect(document.activeElement).toBe(button("Stop and keep the copy"));
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
    expect(screen.queryByRole("button", { name: "Stop and keep the copy" })).toBeNull();
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

describe("MoveDialog states with a way forward", () => {
  const failedPlaced = () =>
    testReport({
      placed: true,
      checks: testReport().checks.map((c) =>
        c.check === "unchanged" ? { ...c, status: "failed", problems: [{ message: "The re-sync didn't finish." }] } : c,
      ),
    });

  it("stopping once the copy is placed keeps it and waits for the report", async () => {
    const backend = fakeBackend();
    backend.migrateCancel.mockResolvedValueOnce({ ended: false });
    await setup(backend);
    fireEvent.click(button("Start"));
    await act(async () => {});
    expect(button("Cancel the move")).toBeTruthy();
    expect(screen.getByText(/Cancelling deletes the partial copy/)).toBeTruthy();
    await act(async () =>
      backend.send({
        event: "migrateProgress",
        data: { id: "m-1", source: TEST_SOURCE, dest: TEST_DEST, step: "installing", done: 0, total: 1 },
      }),
    );
    expect(screen.queryByText(/deletes the partial copy/)).toBeNull();
    fireEvent.click(button("Stop and keep the copy"));
    await act(async () => {});
    expect(backend.migrateCancel).toHaveBeenCalledWith("m-1");
    // Still shown: the report follows.
    expect(button("Stop and keep the copy")).toBeTruthy();
    await act(async () => backend.send({ event: "migrateReady", data: { id: "m-1", report: testReport({ placed: true }) } }));
    expect(button("Delete original")).toBeTruthy();
  });

  it("offers Try again and Keep both when WSL can't see the drive", async () => {
    const backend = fakeBackend({}, [testReport()]);
    backend.migrateDelete.mockResolvedValueOnce({ kind: "driveHidden", deleting: false });
    const { props } = await setup(backend);
    fireEvent.click(button("Delete original"));
    await act(async () => {});
    expect(button("Try again")).toBeTruthy();
    fireEvent.click(button("Keep both"));
    await act(async () => {});
    expect(backend.migrateFinish).toHaveBeenCalledWith("m-1", "keep");
    expect(props.onKeep).toHaveBeenCalled();
  });

  it("while deleting, lists copy changes and offers Delete the rest anyway", async () => {
    const backend = fakeBackend({}, [testReport()], true);
    backend.migrateDelete
      .mockResolvedValueOnce({ ...RECHECK, kind: "changed", copyChanged: ["README.md"], deleting: true })
      .mockResolvedValueOnce({ kind: "deleted", permanent: false, alreadyGone: false });
    const { dialog } = await setup(backend);
    fireEvent.click(button("Finish deleting"));
    await act(async () => {});
    expect(dialog.textContent).toContain("The copy in WSL differs from what's left of the original");
    expect(dialog.textContent).not.toContain("The original changed");
    expect(dialog.textContent).toContain("README.md");
    expect(dialog.textContent).toContain("Changed in the copy (the copy keeps its version):");
    for (const name of ["Keep both", "Re-sync", "Discard the copy"]) {
      expect(screen.queryByRole("button", { name })).toBeNull();
    }
    fireEvent.click(button("Delete the rest anyway"));
    await act(async () => {});
    expect(backend.migrateDelete).toHaveBeenLastCalledWith("m-1", false, { changed: ["README.md"], removed: [] });
    expect(dialog.textContent).toContain("The original is in the Recycle Bin.");
  });

  it("review 3 B: Yes, delete permanently after Delete the rest anyway keeps the same paths, so it can't loop", async () => {
    const backend = fakeBackend({}, [testReport()], true);
    backend.migrateDelete
      .mockResolvedValueOnce({ ...RECHECK, kind: "changed", copyChanged: ["README.md"], deleting: true })
      .mockResolvedValueOnce({ kind: "wontFit", reason: "The bin is full.", deleting: true })
      .mockResolvedValueOnce({ kind: "deleted", permanent: true, alreadyGone: false });
    const { dialog } = await setup(backend);
    fireEvent.click(button("Finish deleting"));
    await act(async () => {});
    fireEvent.click(button("Delete the rest anyway"));
    await act(async () => {});
    expect(backend.migrateDelete).toHaveBeenLastCalledWith("m-1", false, { changed: ["README.md"], removed: [] });
    fireEvent.click(button("Yes, delete permanently"));
    await act(async () => {});
    expect(backend.migrateDelete).toHaveBeenLastCalledWith("m-1", true, { changed: ["README.md"], removed: [] });
    expect(dialog.textContent).not.toContain("differs from what's left");
  });

  it("review 3 B: lists what was deleted in the copy apart, as gone for good, and forces every listed path", async () => {
    const backend = fakeBackend({}, [testReport()], true);
    backend.migrateDelete.mockResolvedValueOnce({
      ...RECHECK,
      kind: "changed",
      copyChanged: ["README.md", "notes.txt"],
      copyRemoved: ["notes.txt"],
      copyUnreadable: [],
      copyMissing: false,
      deleting: true,
    });
    const { dialog } = await setup(backend);
    fireEvent.click(button("Finish deleting"));
    await act(async () => {});
    expect(dialog.textContent).toContain("Changed in the copy (the copy keeps its version):README.md");
    expect(dialog.textContent).toContain("Deleted in the copy. Deleting the original removes these for good:notes.txt");
    fireEvent.click(button("Delete the rest anyway"));
    await act(async () => {});
    expect(backend.migrateDelete).toHaveBeenLastCalledWith("m-1", false, { changed: ["README.md"], removed: ["notes.txt"] });
  });

  it("review 4: sends the edited and the deleted copy paths apart, also on to Yes, delete permanently", async () => {
    const backend = fakeBackend({}, [testReport()], true);
    backend.migrateDelete
      .mockResolvedValueOnce({
        ...RECHECK,
        kind: "changed",
        copyChanged: ["README.md", "notes.txt"],
        copyRemoved: ["notes.txt"],
        copyUnreadable: [],
        copyMissing: false,
        deleting: true,
      })
      .mockResolvedValueOnce({ kind: "wontFit", reason: "The bin is full.", deleting: true })
      .mockResolvedValueOnce({ kind: "deleted", permanent: true, alreadyGone: false });
    await setup(backend);
    fireEvent.click(button("Finish deleting"));
    await act(async () => {});
    fireEvent.click(button("Delete the rest anyway"));
    await act(async () => {});
    fireEvent.click(button("Yes, delete permanently"));
    await act(async () => {});
    expect(backend.migrateDelete).toHaveBeenLastCalledWith("m-1", true, {
      changed: ["README.md"],
      removed: ["notes.txt"],
    });
  });

  it("review 3 F: a copy that can't be read is explained, not taken for gone, and Keep both is offered", async () => {
    const backend = fakeBackend({}, [testReport()]);
    backend.migrateDelete.mockResolvedValueOnce({
      ...RECHECK,
      kind: "changed",
      copyChanged: ["."],
      copyUnreadable: ["."],
      copyMissing: false,
      deleting: false,
    });
    const { dialog } = await setup(backend);
    fireEvent.click(button("Delete original"));
    await act(async () => {});
    expect(dialog.textContent).toContain("couldn't be read");
    expect(dialog.textContent).not.toContain("is gone");
    expect(button("Keep both")).toBeTruthy();
    expect(screen.queryByRole("button", { name: "End the move" })).toBeNull();
  });

  it("review 3 F: while deleting, an unreadable copy path is never forced", async () => {
    const backend = fakeBackend({}, [testReport()], true);
    backend.migrateDelete.mockResolvedValueOnce({
      ...RECHECK,
      kind: "changed",
      copyChanged: ["README.md", "src"],
      copyUnreadable: ["src"],
      copyMissing: false,
      deleting: true,
    });
    await setup(backend);
    fireEvent.click(button("Finish deleting"));
    await act(async () => {});
    expect(screen.queryByRole("button", { name: "Delete the rest anyway" })).toBeNull();
    expect(button("Finish deleting")).toBeTruthy();
  });

  it("review 3 A: lists changes found after an interrupted install and offers neither Discard nor Re-sync", async () => {
    const recoveredChanges = { created: ["my-notes.md"], changed: ["README.md"], removed: [] };
    const backend = fakeBackend({}, [testReport({ recoveredChanges, installBackup: "/home/dev/backup" })]);
    const { dialog } = await setup(backend);
    expect(dialog.textContent).toContain("Changed while the install was interrupted — may include your own edits");
    expect(dialog.textContent).toContain("created my-notes.md");
    expect(button("Delete original")).toBeTruthy();
    expect(button("Keep both")).toBeTruthy();
    expect(screen.queryByRole("button", { name: "Discard the copy" })).toBeNull();
    cleanup();
    const failed = testReport({ recoveredChanges, placed: true });
    failed.checks[3] = { ...failed.checks[3], status: "failed", problems: [{ message: "changed" }] };
    await setup(fakeBackend({}, [failed]));
    expect(button("Keep both")).toBeTruthy();
    for (const name of ["Re-sync", "Discard the copy"]) {
      expect(screen.queryByRole("button", { name })).toBeNull();
    }
  });

  it("review 3 D: a finish refused because the copy went leads, on Finish deleting, to the copy-gone view", async () => {
    const backend = fakeBackend({}, [testReport()], true);
    backend.migrateDelete
      .mockResolvedValueOnce({ kind: "unfinished", reason: "The original is gone, but the helper could not finish." })
      .mockResolvedValueOnce({
        ...RECHECK,
        kind: "changed",
        copyChanged: ["."],
        copyUnreadable: [],
        copyMissing: true,
        sourceMissing: true,
        deleting: true,
      });
    const { dialog } = await setup(backend);
    fireEvent.click(button("Finish deleting"));
    await act(async () => {});
    expect(dialog.textContent).toContain("could not finish");
    fireEvent.click(button("Finish deleting"));
    await act(async () => {});
    expect(dialog.textContent).toContain("The copy in WSL is gone, so nothing more of the original will be deleted.");
    expect(dialog.textContent).toContain("Recycle Bin");
    expect(button("End the move")).toBeTruthy();
  });

  it("review 3 F: doesn't offer Re-sync when only a failed install backup blocks, and offers what works", async () => {
    const report = testReport({ installChanges: { created: [], changed: ["package.json"], removed: [] }, placed: true });
    report.checks[2] = {
      ...report.checks[2],
      status: "failed",
      problems: [{ path: "package.json", message: "Its original couldn't be backed up." }],
    };
    await setup(fakeBackend({}, [report]));
    expect(screen.queryByRole("button", { name: "Re-sync" })).toBeNull();
    expect(button("Keep both")).toBeTruthy();
    expect(button("Discard the copy")).toBeTruthy();
  });

  it("while deleting, never offers Delete the rest anyway when the original changed", async () => {
    const backend = fakeBackend({}, [testReport()], true);
    backend.migrateDelete.mockResolvedValueOnce({
      ...RECHECK,
      kind: "changed",
      changed: ["notes.txt"],
      copyChanged: ["README.md"],
      deleting: true,
    });
    const { dialog } = await setup(backend);
    fireEvent.click(button("Finish deleting"));
    await act(async () => {});
    expect(dialog.textContent).toContain("The original changed while it was being deleted");
    expect(screen.queryByRole("button", { name: "Delete the rest anyway" })).toBeNull();
    expect(button("Finish deleting")).toBeTruthy();
  });

  it("while deleting with the copy gone, points to the Recycle Bin and ends the move after asking", async () => {
    const backend = fakeBackend({}, [testReport()], true, true);
    const { dialog, props } = await setup(backend);
    expect(dialog.textContent).toContain("The copy in WSL is gone, so nothing more of the original will be deleted.");
    expect(dialog.textContent).toContain("Recycle Bin");
    expect(screen.queryByRole("button", { name: "Finish deleting" })).toBeNull();
    fireEvent.click(button("End the move"));
    expect(backend.migrateFinish).not.toHaveBeenCalled();
    expect(document.activeElement).toBe(button("Back"));
    fireEvent.click(button("Yes, end the move"));
    await act(async () => {});
    expect(backend.migrateFinish).toHaveBeenCalledWith("m-1", "discard");
    expect(props.onClose).toHaveBeenCalled();
  });

  it("with the copy gone, only ends the move, even with the original gone too", async () => {
    const backend = fakeBackend({}, [testReport({ placed: true })], false, true);
    backend.migrateSourceExists.mockResolvedValue(false);
    const { dialog } = await setup(backend);
    expect(dialog.textContent).toContain("The copy in WSL is gone");
    expect(dialog.textContent).toContain("The original folder is gone too");
    for (const name of ["Keep both", "Delete original", "Finish the move", "Discard the copy"]) {
      expect(screen.queryByRole("button", { name })).toBeNull();
    }
    fireEvent.click(button("End the move"));
    await act(async () => {});
    expect(backend.migrateFinish).toHaveBeenCalledWith("m-1", "discard");
  });

  it("offers Re-sync, Keep both and Discard for a placed copy whose check failed", async () => {
    const backend = fakeBackend({}, [failedPlaced()]);
    await setup(backend);
    expect(screen.queryByRole("button", { name: "Delete original" })).toBeNull();
    expect(screen.queryByRole("button", { name: "Retry" })).toBeNull();
    expect(button("Keep both")).toBeTruthy();
    expect(button("Discard the copy")).toBeTruthy();
    fireEvent.click(button("Re-sync"));
    await act(async () => {});
    expect(backend.migrateResync).toHaveBeenCalledWith("m-1");
  });

  it("offers only keeping a placed copy whose check failed once the original is gone", async () => {
    const backend = fakeBackend({}, [failedPlaced()]);
    backend.migrateSourceExists.mockResolvedValue(false);
    await setup(backend);
    for (const name of ["Re-sync", "Discard the copy", "Retry"]) {
      expect(screen.queryByRole("button", { name })).toBeNull();
    }
    fireEvent.click(button("Keep the copy"));
    await act(async () => {});
    expect(backend.migrateFinish).toHaveBeenCalledWith("m-1", "keep");
  });

  it("asks for the pending reports again when an answer is refused", async () => {
    const backend = fakeBackend({}, [testReport()]);
    backend.migrateFinish.mockRejectedValueOnce({ kind: "notReady", message: "The copy is gone." });
    await setup(backend);
    expect(backend.migratePending).toHaveBeenCalledTimes(1);
    backend.migratePending.mockResolvedValueOnce([{ id: "m-1", report: testReport(), copyMissing: true }]);
    fireEvent.click(button("Discard the copy"));
    await act(async () => {});
    expect(backend.migratePending).toHaveBeenCalledTimes(2);
    expect(button("End the move")).toBeTruthy();
  });
});
