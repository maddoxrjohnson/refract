import { act, cleanup, fireEvent, render, screen, within } from "@testing-library/react";
import { afterEach, describe, expect, it, vi } from "vitest";
import type { MissingWorktree, RepoSnapshot, WorktreeCheck, WorktreeJobDone } from "../backend/types";
import { NOW, testRefactor, testSession, testWorktree } from "./testRepo";
import { WorktreesSection, type WorktreesSectionProps } from "./WorktreesSection";

afterEach(cleanup);

function setup(worktrees: RepoSnapshot[], extra: Partial<WorktreesSectionProps> = {}) {
  const props: WorktreesSectionProps = {
    worktrees,
    now: NOW,
    onOpen: vi.fn(),
    onRemove: vi.fn(() => Promise.resolve()),
    onCheck: vi.fn(() => Promise.resolve<WorktreeCheck>({ status: { kind: "pushed" }, removable: true, ignored: [] })),
    onPrune: vi.fn(() => Promise.resolve()),
    ...extra,
  };
  const view = render(<WorktreesSection {...props} />);
  return { props, view };
}

const row = (name: string) => screen.getByRole("listitem", { name });
const flush = () => act(async () => {});

const released = testWorktree(
  "wide-cards",
  { release: "v1.4.0", merged: "main", status: { kind: "release", tag: "v1.4.0" } },
  {
    sessions: [
      testSession("s1", { kind: "finished", detail: "completed" }, 26, "Should I tag v1.4.0?\nThe notes are ready.", 1, "askedQuestion"),
    ],
  },
);
const dirty = testWorktree("search", { status: { kind: "changes", count: 3 }, removable: false }, { changedFiles: 3 });

describe("WorktreesSection", () => {
  it("lists each worktree with its branch, status and last agent message", () => {
    setup([released, dirty]);
    const cards = row("tidepool-wide-cards");
    expect(cards.textContent).toContain("wide-cards");
    expect(cards.textContent).toContain("In v1.4.0 · Ready to remove");
    const said = within(cards).getByText("Should I tag v1.4.0?");
    expect(said.getAttribute("title")).toBe("Should I tag v1.4.0?\nThe notes are ready.");
    expect(cards.textContent).toContain("yesterday");
    const search = row("tidepool-search");
    expect(search.textContent).toContain("3 uncommitted changes");
    expect(search.textContent).not.toContain("Ready to remove");
  });

  it("says (detached) for a detached HEAD", () => {
    setup([testWorktree("probe", {}, { branch: null })]);
    expect(row("tidepool-probe").textContent).toContain("(detached)");
  });

  it("shows nothing without worktrees", () => {
    const { view } = setup([]);
    expect(view.container.textContent).toBe("");
  });

  it("offers Remove only when nothing can be lost and no agent is open", () => {
    const idle = testWorktree("idle", { status: { kind: "pushed" } }, {
      sessions: [testSession("i", { kind: "live", detail: "idle" }, 0.1)],
    });
    const waiting = testWorktree("asks", {}, {
      sessions: [testSession("w", { kind: "live", detail: "waitingOnUser" }, 0.1)],
    });
    setup([released, dirty, idle, waiting]);
    expect(within(row("tidepool-wide-cards")).queryByRole("button", { name: /Remove/ })).not.toBeNull();
    expect(within(row("tidepool-search")).queryByRole("button", { name: /Remove/ })).toBeNull();
    const open = row("tidepool-idle");
    expect(within(open).queryByRole("button", { name: /Remove/ })).toBeNull();
    expect(open.textContent).toContain("Pushed, not merged");
    expect(open.textContent).toContain("Close the agent here to remove");
    const asks = row("tidepool-asks");
    expect(asks.textContent).toContain("Agent waiting for you");
    expect(within(asks).queryByRole("button", { name: /Remove/ })).toBeNull();
  });

  it("offers no Remove for a worktree with a refactor, and says why", () => {
    setup([testWorktree("cards", { status: { kind: "pushed" } }, { refactor: testRefactor() })]);
    const cards = row("tidepool-cards");
    expect(cards.textContent).toContain("Has a refactor");
    expect(within(cards).queryByRole("button", { name: /Remove/ })).toBeNull();
  });

  it("moves focus into the confirm, and back to Remove… on Keep it", () => {
    setup([released]);
    const remove = screen.getByRole("button", { name: "Remove tidepool-wide-cards…" });
    remove.focus();
    fireEvent.click(remove);
    expect(document.activeElement).toBe(within(screen.getByRole("group")).getByRole("button", { name: "Keep it" }));
    fireEvent.click(document.activeElement!);
    expect(document.activeElement).toBe(screen.getByRole("button", { name: "Remove tidepool-wide-cards…" }));
  });

  it("opens a worktree as Enter does on its folder", () => {
    const { props } = setup([released, dirty]);
    fireEvent.click(screen.getByRole("button", { name: "Open tidepool-search" }));
    expect(props.onOpen).toHaveBeenCalledWith(dirty.path);
  });

  it("confirms Remove, naming the folder, the branch that stays and the ignored files", async () => {
    const ignored = Array.from({ length: 9 }, (_, i) => (i === 0 ? ".env" : `cache-${i}/`));
    const wt = testWorktree("wide-cards", { ignored, status: { kind: "merged", branch: "main" } });
    const { props } = setup([wt]);
    fireEvent.click(screen.getByRole("button", { name: "Remove tidepool-wide-cards…" }));
    const confirm = screen.getByRole("group", { name: "Confirm removing tidepool-wide-cards" });
    expect(confirm.textContent).toContain(`Remove the folder ${wt.path}? The branch wide-cards stays.`);
    expect(confirm.textContent).toContain("Also deletes ignored files:");
    const files = within(confirm).getByRole("list", { name: "Ignored files" });
    const items = within(files).getAllByRole("listitem").map((li) => li.textContent);
    expect(items).toEqual([...ignored.slice(0, 8), "and more"]);
    // Keep it backs out without removing.
    fireEvent.click(within(confirm).getByRole("button", { name: "Keep it" }));
    expect(props.onRemove).not.toHaveBeenCalled();
    fireEvent.click(screen.getByRole("button", { name: "Remove tidepool-wide-cards…" }));
    fireEvent.click(within(screen.getByRole("group")).getByRole("button", { name: "Remove" }));
    // With the whole list the confirm was built from, for the helper to compare.
    expect(props.onRemove).toHaveBeenCalledWith(wt.path, ignored);
    expect(within(screen.getByRole("group")).getByRole("button", { name: "Removing…" })).toHaveProperty("disabled", true);
    await flush();
  });

  it("leaves out the ignored list when there is none", () => {
    setup([released]);
    fireEvent.click(screen.getByRole("button", { name: /Remove/ }));
    expect(screen.getByRole("group").textContent).not.toContain("ignored");
  });

  it("shows the helper's refusal and lets Remove be tried again", async () => {
    const onRemove = vi.fn(() =>
      Promise.reject({ kind: "worktree", message: "Cannot remove the worktree: This worktree has uncommitted changes." }),
    );
    setup([released], { onRemove });
    fireEvent.click(screen.getByRole("button", { name: /Remove/ }));
    fireEvent.click(within(screen.getByRole("group")).getByRole("button", { name: "Remove" }));
    await flush();
    expect(screen.getByRole("alert").textContent).toBe(
      "Cannot remove the worktree: This worktree has uncommitted changes.",
    );
    expect(within(screen.getByRole("group")).getByRole("button", { name: "Remove" })).toHaveProperty("disabled", false);
  });
});

/** A promise and the functions that settle it. */
function deferred<T>() {
  let resolve!: (value: T) => void;
  let reject!: (err: unknown) => void;
  const promise = new Promise<T>((res, rej) => {
    resolve = res;
    reject = rej;
  });
  return { promise, resolve, reject };
}

const tablet = testWorktree(
  "tablet",
  { untrackedChecked: false, status: { kind: "unchecked" }, removable: false, checkable: true },
  { path: "/mnt/c/Users/dev/source/tidepool-tablet" },
);

describe("WorktreesSection on a Windows drive", () => {
  it("offers Check… instead of Remove…, and runs it with the button held", async () => {
    const pending = deferred<WorktreeCheck>();
    const onCheck = vi.fn(() => pending.promise);
    setup([tablet], { onCheck });
    const r = row("tidepool-tablet");
    expect(r.textContent).toContain("Changes not checked (Windows drive)");
    expect(within(r).queryByRole("button", { name: /Remove/ })).toBeNull();
    fireEvent.click(within(r).getByRole("button", { name: "Check tidepool-tablet…" }));
    expect(onCheck).toHaveBeenCalledWith(tablet.path);
    expect(within(r).getByRole("button", { name: "Check tidepool-tablet…" })).toHaveProperty("disabled", true);
    expect(within(r).getByRole("status").textContent).toBe("Checking files on the Windows drive…");
    pending.resolve({ status: { kind: "changes", count: 2 }, removable: false, ignored: [] });
    await flush();
    // The answer replaces the label; nothing to remove, so no confirm, but Check again
    // stays, as edits from Windows bring no new snapshot. Focus waits there.
    expect(r.textContent).toContain("2 uncommitted changes");
    expect(r.textContent).not.toContain("Changes not checked");
    expect(within(r).queryByRole("group")).toBeNull();
    expect(within(r).queryByRole("button", { name: /Remove/ })).toBeNull();
    const again = within(r).getByRole("button", { name: "Check tidepool-tablet again" });
    expect(again.textContent).toBe("Check again");
    expect(document.activeElement).toBe(again);
    expect(within(r).queryByRole("status")).toBeNull();
    fireEvent.click(again);
    expect(onCheck).toHaveBeenCalledTimes(2);
  });

  it("opens the confirm with the check's ignored files, then removes as before", async () => {
    const onCheck = vi.fn(() =>
      Promise.resolve<WorktreeCheck>({ status: { kind: "merged", branch: "main" }, removable: true, ignored: ["node_modules/"] }),
    );
    const { props } = setup([tablet], { onCheck });
    fireEvent.click(screen.getByRole("button", { name: "Check tidepool-tablet…" }));
    await flush();
    const r = row("tidepool-tablet");
    expect(r.textContent).toContain("Merged into main · Ready to remove");
    const confirm = within(r).getByRole("group", { name: "Confirm removing tidepool-tablet" });
    expect(document.activeElement).toBe(within(confirm).getByRole("button", { name: "Keep it" }));
    expect(within(confirm).getByRole("list", { name: "Ignored files" }).textContent).toBe("node_modules/");
    // Keep it leaves Remove… for later.
    fireEvent.click(within(confirm).getByRole("button", { name: "Keep it" }));
    expect(document.activeElement).toBe(screen.getByRole("button", { name: "Remove tidepool-tablet…" }));
    fireEvent.click(screen.getByRole("button", { name: "Remove tidepool-tablet…" }));
    fireEvent.click(within(screen.getByRole("group")).getByRole("button", { name: "Remove" }));
    expect(props.onRemove).toHaveBeenCalledWith(tablet.path, ["node_modules/"]);
    await flush();
  });

  it("goes back to the snapshot's status when the next snapshot comes", async () => {
    const { props, view } = setup([tablet]);
    fireEvent.click(screen.getByRole("button", { name: "Check tidepool-tablet…" }));
    await flush();
    expect(row("tidepool-tablet").textContent).toContain("Pushed, not merged");
    // A session update keeps the snapshot's facts, and so the answer.
    view.rerender(<WorktreesSection {...props} worktrees={[{ ...tablet, sessions: [] }]} />);
    expect(row("tidepool-tablet").textContent).toContain("Pushed, not merged");
    view.rerender(<WorktreesSection {...props} worktrees={[{ ...tablet, worktree: { ...tablet.worktree! } }]} />);
    expect(row("tidepool-tablet").textContent).toContain("Changes not checked (Windows drive)");
    expect(screen.queryByRole("group")).toBeNull();
    expect(screen.getByRole("button", { name: "Check tidepool-tablet…" })).toHaveProperty("disabled", false);
  });

  it("shows a refused check as a refused Remove, and lets it be tried again", async () => {
    const onCheck = vi.fn(() => Promise.reject({ kind: "worktree", message: "Cannot check the worktree: Not a linked worktree." }));
    setup([tablet], { onCheck });
    fireEvent.click(screen.getByRole("button", { name: "Check tidepool-tablet…" }));
    await flush();
    expect(screen.getByRole("alert").textContent).toBe("Cannot check the worktree: Not a linked worktree.");
    expect(screen.getByRole("button", { name: "Check tidepool-tablet…" })).toHaveProperty("disabled", false);
    expect(row("tidepool-tablet").textContent).toContain("Changes not checked (Windows drive)");
  });

  it("holds Check… back while an agent is open there, and says so", () => {
    setup([{ ...tablet, sessions: [testSession("i", { kind: "live", detail: "idle" }, 0.1)] }]);
    const r = row("tidepool-tablet");
    expect(within(r).queryByRole("button", { name: /Check/ })).toBeNull();
    expect(r.textContent).toContain("Close the agent here to check its files");
  });
});

const gone: MissingWorktree = {
  path: "/home/dev/projects/tidepool-offline",
  branch: "offline",
  status: { kind: "gone" },
  cleanable: true,
};

describe("WorktreesSection's missing folders", () => {
  it("lists them after the live rows, with folder, branch and status, and Clean up… only when safe", () => {
    const missing: MissingWorktree[] = [
      gone,
      { path: "/home/dev/projects/tidepool-locked", branch: "locked", status: { kind: "locked" }, cleanable: false },
      { path: "/mnt/d/work/tidepool-usb", branch: "usb", status: { kind: "driveNotAttached" }, cleanable: false },
      {
        path: "/home/dev/projects/tidepool-old",
        branch: "moved",
        status: { kind: "moved", to: "/home/dev/projects/tidepool-new" },
        cleanable: false,
      },
      { path: "/home/dev/projects/tidepool-bisect", branch: "refs", status: { kind: "ownRefs" }, cleanable: false },
      { path: "/home/dev/projects/tidepool-probe", status: { kind: "commitsOnlyHere" }, cleanable: false },
    ];
    setup([released], { missing });
    const items = within(screen.getByRole("region", { name: "Worktrees" })).getAllByRole("listitem");
    expect(items.map((li) => li.getAttribute("aria-label"))).toEqual([
      "tidepool-wide-cards",
      "tidepool-offline",
      "tidepool-locked",
      "tidepool-usb",
      "tidepool-old",
      "tidepool-bisect",
      "tidepool-probe",
    ]);
    const offline = row("tidepool-offline");
    expect(offline.textContent).toContain("/home/dev/projects/tidepool-offline");
    expect(offline.textContent).toContain("offline");
    expect(offline.textContent).toContain("Folder is gone");
    expect(within(offline).getByRole("button", { name: "Clean up tidepool-offline…" })).toBeTruthy();
    expect(row("tidepool-locked").textContent).toContain("Folder is gone · Locked");
    expect(row("tidepool-usb").textContent).toContain("Folder is gone · Drive not attached");
    expect(row("tidepool-old").textContent).toContain("Moved to /home/dev/projects/tidepool-new");
    expect(row("tidepool-old").textContent).toContain("Run git worktree repair there");
    expect(row("tidepool-bisect").textContent).toContain("Folder is gone · Has its own refs");
    expect(row("tidepool-probe").textContent).toContain("Folder is gone · Its commits would be lost");
    expect(row("tidepool-probe").textContent).toContain("(detached)");
    for (const name of ["tidepool-locked", "tidepool-usb", "tidepool-old", "tidepool-bisect", "tidepool-probe"]) {
      expect(within(row(name)).queryByRole("button")).toBeNull();
    }
  });

  it("shows the section for missing folders alone", () => {
    setup([], { missing: [gone] });
    expect(screen.getByRole("region", { name: "Worktrees" })).toBeTruthy();
  });

  it("confirms Clean up, naming the branch that stays, and calls it with the listed path", async () => {
    const { props } = setup([], { missing: [gone] });
    const button = screen.getByRole("button", { name: "Clean up tidepool-offline…" });
    button.focus();
    fireEvent.click(button);
    const confirm = screen.getByRole("group", { name: "Confirm cleaning up tidepool-offline" });
    expect(confirm.textContent).toContain("Forget this worktree? Only git's record of it goes; branch offline stays.");
    expect(document.activeElement).toBe(within(confirm).getByRole("button", { name: "Keep it" }));
    fireEvent.click(document.activeElement!);
    expect(props.onPrune).not.toHaveBeenCalled();
    expect(document.activeElement).toBe(screen.getByRole("button", { name: "Clean up tidepool-offline…" }));
    fireEvent.click(screen.getByRole("button", { name: "Clean up tidepool-offline…" }));
    fireEvent.click(within(screen.getByRole("group")).getByRole("button", { name: "Clean up" }));
    expect(props.onPrune).toHaveBeenCalledWith(gone.path);
    expect(within(screen.getByRole("group")).getByRole("button", { name: "Cleaning up…" })).toHaveProperty("disabled", true);
    await flush();
  });

  it("words the confirm without a branch when detached", () => {
    setup([], { missing: [{ ...gone, branch: undefined }] });
    fireEvent.click(screen.getByRole("button", { name: /Clean up/ }));
    expect(screen.getByRole("group").textContent).toBe("Forget this worktree? Only git's record of it goes.Clean upKeep it");
  });

  it("shows a refused Clean up and lets it be tried again", async () => {
    const onPrune = vi.fn(() => Promise.reject({ kind: "worktree", message: "Cannot clean up the worktree: The folder is back." }));
    setup([], { missing: [gone], onPrune });
    fireEvent.click(screen.getByRole("button", { name: /Clean up/ }));
    fireEvent.click(within(screen.getByRole("group")).getByRole("button", { name: "Clean up" }));
    await flush();
    expect(screen.getByRole("alert").textContent).toBe("Cannot clean up the worktree: The folder is back.");
    expect(within(screen.getByRole("group")).getByRole("button", { name: "Clean up" })).toHaveProperty("disabled", false);
  });
});

/** A stand-in for the helper's `worktreeJobDone` events. */
function jobEvents() {
  let listener: ((done: WorktreeJobDone) => void) | undefined;
  return {
    onJobDone: (cb: (done: WorktreeJobDone) => void) => {
      listener = cb;
      return () => {
        listener = undefined;
      };
    },
    send: (done: WorktreeJobDone) => act(() => listener?.(done)),
  };
}

const HALF = "Cannot remove the worktree: Removed part of the folder; a program on Windows holds the rest.";
const STILL = { kind: "pending", message: "Still running; the panel will update when it finishes." };

describe("WorktreesSection's jobs", () => {
  it("keeps a half-removal's message after its row goes, until dismissed", async () => {
    const { props, view } = setup([released, dirty], { onRemove: vi.fn(() => Promise.reject({ kind: "worktree", message: HALF })) });
    fireEvent.click(screen.getByRole("button", { name: "Remove tidepool-wide-cards…" }));
    fireEvent.click(within(screen.getByRole("group")).getByRole("button", { name: "Remove" }));
    await flush();
    expect(within(row("tidepool-wide-cards")).getByRole("alert").textContent).toBe(HALF);
    // The helper drops the half-removed worktree: `repoRemoved` takes its row.
    view.rerender(<WorktreesSection {...props} worktrees={[dirty]} />);
    const section = screen.getByRole("region", { name: "Worktrees" });
    expect(within(section).getByRole("alert").textContent).toBe(`tidepool-wide-cards: ${HALF}`);
    // The section stays for the notice when it has no rows left.
    view.rerender(<WorktreesSection {...props} worktrees={[]} />);
    expect(screen.getByRole("alert").textContent).toBe(`tidepool-wide-cards: ${HALF}`);
    fireEvent.click(screen.getByRole("button", { name: "Dismiss the notice about tidepool-wide-cards" }));
    expect(view.container.textContent).toBe("");
  });

  it("holds a removal that outlasted its wait until its job ends, then shows how", async () => {
    const jobs = jobEvents();
    const onRemove = vi.fn(() => Promise.reject(STILL));
    setup([released], { onRemove, onJobDone: jobs.onJobDone });
    fireEvent.click(screen.getByRole("button", { name: /Remove/ }));
    fireEvent.click(within(screen.getByRole("group")).getByRole("button", { name: "Remove" }));
    await flush();
    const r = row("tidepool-wide-cards");
    expect(within(r).getByRole("status").textContent).toBe(STILL.message);
    expect(within(r).getByRole("button", { name: "Removing…" })).toHaveProperty("disabled", true);
    // Another path's job leaves it be.
    jobs.send({ path: dirty.path });
    expect(within(r).getByRole("button", { name: "Removing…" })).toHaveProperty("disabled", true);
    jobs.send({ path: released.path, error: "Cannot remove the worktree: This worktree is locked." });
    expect(within(r).queryByRole("status")).toBeNull();
    expect(within(r).getByRole("alert").textContent).toBe("Cannot remove the worktree: This worktree is locked.");
    expect(within(r).getByRole("button", { name: "Remove" })).toHaveProperty("disabled", false);
  });

  it("clears the wait's notice when the job that outlasted it succeeds", async () => {
    const jobs = jobEvents();
    const { props, view } = setup([released, dirty], { onRemove: vi.fn(() => Promise.reject(STILL)), onJobDone: jobs.onJobDone });
    fireEvent.click(screen.getByRole("button", { name: "Remove tidepool-wide-cards…" }));
    fireEvent.click(within(screen.getByRole("group")).getByRole("button", { name: "Remove" }));
    await flush();
    view.rerender(<WorktreesSection {...props} worktrees={[dirty]} />);
    expect(screen.getByRole("status").textContent).toBe(`tidepool-wide-cards: ${STILL.message}`);
    jobs.send({ path: released.path });
    expect(screen.queryByRole("status")).toBeNull();
    expect(screen.queryByRole("alert")).toBeNull();
  });

  it("drops a Windows-drive check's answer when Remove is refused, and offers Check again", async () => {
    const refusal = "Cannot remove the worktree: New ignored files appeared since you confirmed (.env). Check again.";
    const onCheck = vi.fn(() =>
      Promise.resolve<WorktreeCheck>({ status: { kind: "pushed" }, removable: true, ignored: ["node_modules/"] }),
    );
    const onRemove = vi.fn(() => Promise.reject({ kind: "worktree", message: refusal }));
    setup([tablet], { onCheck, onRemove });
    fireEvent.click(screen.getByRole("button", { name: "Check tidepool-tablet…" }));
    await flush();
    const r = row("tidepool-tablet");
    expect(r.textContent).toContain("Ready to remove");
    fireEvent.click(within(screen.getByRole("group")).getByRole("button", { name: "Remove" }));
    expect(onRemove).toHaveBeenCalledWith(tablet.path, ["node_modules/"]);
    await flush();
    expect(within(r).getByRole("alert").textContent).toBe(refusal);
    expect(r.textContent).toContain("Changes not checked (Windows drive)");
    expect(r.textContent).not.toContain("Ready to remove");
    expect(within(r).queryByRole("group")).toBeNull();
    // Focus went with the confirm; it lands on Check….
    const check = within(r).getByRole("button", { name: "Check tidepool-tablet…" });
    expect(document.activeElement).toBe(check);
    fireEvent.click(check);
    expect(onCheck).toHaveBeenCalledTimes(2);
    expect(within(r).queryByRole("alert")).toBeNull();
  });

  it("closes the confirm for good when Remove stops being offered", () => {
    const { props, view } = setup([released]);
    fireEvent.click(screen.getByRole("button", { name: /Remove/ }));
    expect(document.activeElement).toBe(within(screen.getByRole("group")).getByRole("button", { name: "Keep it" }));
    const busy = { ...released, sessions: [testSession("i", { kind: "live", detail: "idle" }, 0.1)] };
    view.rerender(<WorktreesSection {...props} worktrees={[busy]} />);
    expect(screen.queryByRole("group")).toBeNull();
    expect(document.activeElement).toBe(screen.getByRole("button", { name: "Open tidepool-wide-cards" }));
    // The agent closing again does not bring the confirm back.
    view.rerender(<WorktreesSection {...props} worktrees={[released]} />);
    expect(screen.queryByRole("group")).toBeNull();
    expect(screen.getByRole("button", { name: "Remove tidepool-wide-cards…" })).toBeTruthy();
  });

  it("moves focus to the heading when the row it was in is cleaned up", async () => {
    const { props, view } = setup([released], { missing: [gone] });
    fireEvent.click(screen.getByRole("button", { name: "Clean up tidepool-offline…" }));
    const clean = within(screen.getByRole("group")).getByRole("button", { name: "Clean up" });
    clean.focus();
    fireEvent.click(clean);
    await flush();
    view.rerender(<WorktreesSection {...props} missing={[]} />);
    expect(document.activeElement).toBe(screen.getByRole("heading", { name: "Worktrees" }));
  });

  it("keeps a removal that worked busy until its row goes", async () => {
    const jobs = jobEvents();
    const { props, view } = setup([released, dirty], { onJobDone: jobs.onJobDone });
    fireEvent.click(screen.getByRole("button", { name: "Remove tidepool-wide-cards…" }));
    fireEvent.click(within(screen.getByRole("group")).getByRole("button", { name: "Remove" }));
    await flush();
    // The job's end comes before the `repoRemoved` that takes the row.
    jobs.send({ path: released.path });
    const r = row("tidepool-wide-cards");
    expect(within(r).getByRole("button", { name: "Removing…" })).toHaveProperty("disabled", true);
    expect(within(r).queryByRole("alert")).toBeNull();
    // A snapshot meanwhile does not offer the confirm again.
    view.rerender(<WorktreesSection {...props} worktrees={[{ ...released }, dirty]} />);
    expect(within(row("tidepool-wide-cards")).getByRole("button", { name: "Removing…" })).toHaveProperty("disabled", true);
    view.rerender(<WorktreesSection {...props} worktrees={[dirty]} />);
    expect(screen.queryByRole("listitem", { name: "tidepool-wide-cards" })).toBeNull();
    // Back (say, made again), it is no longer busy.
    view.rerender(<WorktreesSection {...props} worktrees={[released, dirty]} />);
    expect(within(row("tidepool-wide-cards")).getByRole("button", { name: "Remove tidepool-wide-cards…" })).toHaveProperty(
      "disabled",
      false,
    );
  });

  it("ends a removal that outlasted its wait when the helper restarts", async () => {
    const jobs = jobEvents();
    let gone: (() => void) | undefined;
    const onHelperGone = (cb: () => void) => {
      gone = cb;
      return () => {
        gone = undefined;
      };
    };
    setup([released, dirty], { onRemove: vi.fn(() => Promise.reject(STILL)), onJobDone: jobs.onJobDone, onHelperGone });
    fireEvent.click(screen.getByRole("button", { name: "Remove tidepool-wide-cards…" }));
    fireEvent.click(within(screen.getByRole("group")).getByRole("button", { name: "Remove" }));
    await flush();
    const r = row("tidepool-wide-cards");
    expect(within(r).getByRole("button", { name: "Removing…" })).toHaveProperty("disabled", true);
    act(() => gone?.());
    expect(within(r).queryByRole("status")).toBeNull();
    expect(within(r).getByRole("alert").textContent).toBe("The helper restarted; check the worktree again.");
    expect(within(r).getByRole("button", { name: "Remove" })).toHaveProperty("disabled", false);
    // A late event from the old helper changes nothing.
    jobs.send({ path: released.path, error: "Cannot remove the worktree: This worktree is locked." });
    expect(within(r).getByRole("alert").textContent).toBe("The helper restarted; check the worktree again.");
  });

  it("lists the ignored entries a refusal's new snapshot brings, and sends them", async () => {
    const refusal =
      "Cannot remove the worktree: New ignored files appeared since you confirmed (node_modules/). Review the list and remove again.";
    const built = testWorktree("built", { ignored: ["target/"] });
    const onRemove = vi.fn<WorktreesSectionProps["onRemove"]>(() => Promise.reject({ kind: "worktree", message: refusal }));
    const { props, view } = setup([built], { onRemove });
    fireEvent.click(screen.getByRole("button", { name: "Remove tidepool-built…" }));
    fireEvent.click(within(screen.getByRole("group")).getByRole("button", { name: "Remove" }));
    expect(onRemove).toHaveBeenLastCalledWith(built.path, ["target/"]);
    await flush();
    // The helper sends the worktree again, its ignored entries re-read.
    const after = testWorktree("built", { ignored: ["node_modules/", "target/"] });
    view.rerender(<WorktreesSection {...props} worktrees={[after]} />);
    const confirm = screen.getByRole("group", { name: "Confirm removing tidepool-built" });
    const files = within(confirm).getByRole("list", { name: "Ignored files" });
    expect(within(files).getAllByRole("listitem").map((li) => li.textContent)).toEqual(["node_modules/", "target/"]);
    fireEvent.click(within(confirm).getByRole("button", { name: "Remove" }));
    expect(onRemove).toHaveBeenLastCalledWith(built.path, ["node_modules/", "target/"]);
    await flush();
  });
});
