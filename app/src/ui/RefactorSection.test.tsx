import { act, cleanup, fireEvent, render, screen } from "@testing-library/react";
import { afterEach, describe, expect, it, vi } from "vitest";
import type { MergeOutcome, RefactorInfo, RepoSnapshot } from "../backend/types";
import { RefactorSection, type RefactorSectionProps } from "./RefactorSection";
import { testRefactor, testRepo } from "./testRepo";

afterEach(cleanup);

function setup(repo: RepoSnapshot, extra: Partial<RefactorSectionProps> = {}) {
  const props: RefactorSectionProps = {
    repo,
    onStart: vi.fn(),
    onOpen: vi.fn(),
    onResume: vi.fn(),
    onMerge: vi.fn(() => Promise.resolve<MergeOutcome>({ outcome: "merged", commits: 2 })),
    onDiscard: vi.fn(() => Promise.resolve()),
    ...extra,
  };
  const view = render(<RefactorSection {...props} />);
  const section = () => screen.getByRole("region", { name: "Refactor" });
  return { props, view, section };
}

const withRefactor = (extra: Partial<RefactorInfo>) => testRepo({ refactor: testRefactor(extra) });
const button = (name: string | RegExp) => screen.getByRole("button", { name });
const flush = () => act(async () => {});

describe("RefactorSection", () => {
  it("offers Refactor with Alt+R on a repo without one", () => {
    const { props } = setup(testRepo());
    fireEvent.click(button(/Refactor…/));
    expect(props.onStart).toHaveBeenCalled();
    expect(button(/Refactor…/).textContent).toContain("Alt+R");
  });

  it("disables Refactor on a detached HEAD", () => {
    const { section } = setup(testRepo({ branch: null }));
    expect(button(/Refactor…/)).toHaveProperty("disabled", true);
    expect(section().textContent).toContain("Check out a branch first.");
  });

  it("shows a spinner while starting, with nothing to press", () => {
    const { section } = setup(withRefactor({ state: "starting", live: false, commits: [] }));
    expect(section().textContent).toContain("Opening VS Code…");
    expect(screen.queryAllByRole("button")).toEqual([]);
  });

  it("goes to the refactor while Claude works or needs you", () => {
    for (const state of ["working", "needsYou"] as const) {
      const { props, section, view } = setup(withRefactor({ state, live: true, commits: [] }));
      if (state === "needsYou") expect(section().textContent).toContain("approve its plan");
      fireEvent.click(button(/Go to the refactor/));
      expect(props.onOpen).toHaveBeenCalled();
      expect(screen.queryByRole("button", { name: /Discard/ })).toBeNull();
      view.unmount();
    }
  });

  it("resumes or discards an interrupted refactor", () => {
    const { props } = setup(withRefactor({ state: "interrupted", commits: [] }));
    fireEvent.click(button("Resume refactor"));
    expect(props.onResume).toHaveBeenCalledWith(false);
    fireEvent.click(button("Discard"));
    expect(screen.getByText("Remove the refactor's worktree and branch?")).toBeTruthy();
  });

  it("shows a ready refactor's commits, diffstat, checks and summary", () => {
    const { section, props } = setup(withRefactor({ state: "ready" }));
    const text = section().textContent ?? "";
    expect(text).toContain("Extract the retry policy");
    expect(text).toContain("5 files changed, +80 −120");
    expect(text).toContain("npm run lint");
    expect(text).toContain("failed");
    expect(text).toContain("not run");
    expect(text).toContain("Two refactors; the tests still pass.");
    fireEvent.click(button(/Review in VS Code/));
    expect(props.onOpen).toHaveBeenCalled();
    expect(screen.queryByRole("button", { name: "Resume refactor" })).toBeNull();
  });

  it("says when no checks were reported, and offers Resume after an interruption", () => {
    const { section } = setup(withRefactor({ state: "ready", checks: null, interrupted: true }));
    expect(section().textContent).toContain("No check results reported.");
    expect(button("Resume refactor")).toBeTruthy();
  });

  it("merges, then says how many commits went in until the panel closes", async () => {
    const outcome: MergeOutcome = { outcome: "merged", commits: 2, kept: ["The branch refactor/x was kept: not fully merged."] };
    const { props, view, section } = setup(withRefactor({ state: "ready" }), {
      onMerge: vi.fn(() => Promise.resolve(outcome)),
    });
    fireEvent.click(button("Merge into main"));
    await flush();
    expect(props.onMerge).toHaveBeenCalled();
    // The refactor is gone from the snapshot; the line stays.
    view.rerender(<RefactorSection {...props} repo={testRepo({ refactor: null })} />);
    expect(section().textContent).toContain("Merged 2 commits into main.");
    expect(section().textContent).toContain("not fully merged");
    expect(button(/Refactor…/)).toBeTruthy();
  });

  it("shows a merge error from the helper inline", async () => {
    const { section } = setup(withRefactor({ state: "ready" }), {
      onMerge: vi.fn(() => Promise.reject({ kind: "refactor", message: "Cannot merge the refactor: busy" })),
    });
    fireEvent.click(button("Merge into main"));
    await flush();
    expect(section().querySelector('[role="alert"]')?.textContent).toBe("Cannot merge the refactor: busy");
  });

  it("says a merge stopped while the agent runs", async () => {
    const { section } = setup(withRefactor({ state: "ready", live: true }), {
      onMerge: vi.fn(() => Promise.resolve<MergeOutcome>({ outcome: "blocked", reason: "live", files: [] })),
    });
    fireEvent.click(button("Merge into main"));
    await flush();
    expect(section().textContent).toContain("still running");
  });

  it("asks before discarding, and not while the agent is live", async () => {
    const live = setup(withRefactor({ state: "ready", live: true }));
    expect(button("Discard")).toHaveProperty("disabled", true);
    live.view.unmount();

    const { props } = setup(withRefactor({ state: "ready" }));
    fireEvent.click(button("Discard"));
    expect(screen.getByText("Discard 2 commits?")).toBeTruthy();
    fireEvent.click(button("Keep it"));
    expect(props.onDiscard).not.toHaveBeenCalled();
    fireEvent.click(button("Discard"));
    const confirm = screen.getByRole("group", { name: "Confirm discard" });
    fireEvent.click(confirm.querySelector("button")!);
    await flush();
    expect(props.onDiscard).toHaveBeenCalled();
  });

  it("moves focus into the discard confirm, and back to Discard on Keep it", () => {
    setup(withRefactor({ state: "ready" }));
    button("Discard").focus();
    fireEvent.click(button("Discard"));
    expect(document.activeElement).toBe(button("Keep it"));
    fireEvent.click(button("Keep it"));
    expect(document.activeElement).toBe(button("Discard"));
  });

  it("shows a blocked merge's files, Merge again, and Fix with Claude only for conflicts", () => {
    const blocked = (reason: "conflict" | "dirtyOverlap" | "worktreeDirty") =>
      withRefactor({ state: "mergeBlocked", blocked: { reason, files: ["src/sync.ts"] } });
    const conflict = setup(blocked("conflict"));
    expect(conflict.section().textContent).toContain("src/sync.ts");
    expect(conflict.section().textContent).toContain("rebasing it hit conflicts");
    fireEvent.click(button("Fix with Claude"));
    expect(conflict.props.onResume).toHaveBeenCalledWith(true);
    expect(button("Merge again")).toBeTruthy();
    conflict.view.unmount();

    const dirty = setup(blocked("dirtyOverlap"));
    expect(screen.queryByRole("button", { name: "Fix with Claude" })).toBeNull();
    expect(screen.queryByRole("button", { name: /Go to the refactor/ })).toBeNull();
    expect(button("Discard")).toBeTruthy();
    dirty.view.unmount();

    setup(blocked("worktreeDirty"));
    expect(button(/Go to the refactor/)).toBeTruthy();
  });

  it("offers Remove when there was nothing to do", () => {
    const { section } = setup(withRefactor({ state: "nothingToDo", commits: [], summary: "The code is already tidy." }));
    expect(section().textContent).toContain("Claude found nothing worth refactoring.");
    expect(section().textContent).toContain("The code is already tidy.");
    fireEvent.click(button("Remove"));
    expect(screen.getByText("Remove the refactor's worktree and branch?")).toBeTruthy();
  });
  it("with Claude still open after a conflict: Go to the refactor, and why Fix and Discard wait", () => {
    const { section } = setup(
      withRefactor({ state: "mergeBlocked", live: true, blocked: { reason: "conflict", files: ["src/sync.ts"] } }),
    );
    expect(section().textContent).toContain("Claude is still open in the refactor window; close it to fix or discard.");
    expect(button(/Go to the refactor/)).toBeTruthy();
    for (const name of ["Fix with Claude", "Discard"]) {
      expect(button(name)).toHaveProperty("disabled", true);
      expect(button(name).getAttribute("title")).toContain("Close it first");
    }
    expect(button("Merge again")).toHaveProperty("disabled", false);
  });

  it("with Claude still open and nothing to do: Go to the refactor, Remove waits", () => {
    const { section } = setup(withRefactor({ state: "nothingToDo", live: true, commits: [] }));
    expect(section().textContent).toContain("close it to fix or discard");
    expect(button(/Go to the refactor/)).toBeTruthy();
    expect(button("Remove")).toHaveProperty("disabled", true);
  });

  it("keeps the merged line over a leftover copy, and drops it for a new refactor", async () => {
    const { props, view, section } = setup(withRefactor({ state: "ready" }));
    fireEvent.click(button("Merge into main"));
    await flush();
    const leftover = testRepo({
      refactor: testRefactor({ state: "nothingToDo", commits: [], summary: "Merged; the worktree could not be removed: in use" }),
    });
    view.rerender(<RefactorSection {...props} repo={leftover} />);
    expect(section().textContent).toContain("Merged 2 commits into main.");
    expect(section().textContent).toContain("leftover copy");
    expect(section().textContent).not.toContain("found nothing");
    expect(button("Remove")).toBeTruthy();
    const next = testRepo({ refactor: testRefactor({ state: "starting", worktree: "/home/demo/projects/.refract-worktrees/tidepool-20261031-1610" }) });
    view.rerender(<RefactorSection {...props} repo={next} />);
    expect(section().textContent).not.toContain("Merged 2 commits");
  });

  const STILL = { kind: "pending", message: "Still running; the panel will update when it finishes." };

  it("keeps a merge that outlasted the wait busy, then says it merged without a count", async () => {
    const { props, view, section } = setup(withRefactor({ state: "ready" }), {
      onMerge: vi.fn(() => Promise.reject(STILL)),
    });
    fireEvent.click(button("Merge into main"));
    await flush();
    expect(section().querySelector('[role="alert"]')).toBeNull();
    expect(section().textContent).toContain("Still running; the panel will update when it finishes.");
    expect(button("Merging…")).toHaveProperty("disabled", true);
    expect(button("Discard")).toHaveProperty("disabled", true);
    // A snapshot with the same state changes nothing.
    view.rerender(<RefactorSection {...props} repo={withRefactor({ state: "ready" })} />);
    expect(button("Merging…")).toHaveProperty("disabled", true);
    view.rerender(<RefactorSection {...props} repo={testRepo({ refactor: null })} />);
    expect(section().textContent).toContain("Merged into main.");
    expect(section().textContent).not.toContain("Still running");
  });

  it("frees the buttons when a pending merge ends blocked", async () => {
    const { props, view, section } = setup(withRefactor({ state: "ready" }), {
      onMerge: vi.fn(() => Promise.reject(STILL)),
    });
    fireEvent.click(button("Merge into main"));
    await flush();
    view.rerender(
      <RefactorSection {...props} repo={withRefactor({ state: "mergeBlocked", blocked: { reason: "conflict", files: [] } })} />,
    );
    expect(button("Merge again")).toHaveProperty("disabled", false);
    expect(section().textContent).not.toContain("Still running");
  });

  it("shows nothing merged after a pending discard ends", async () => {
    const { props, view, section } = setup(withRefactor({ state: "ready" }), {
      onDiscard: vi.fn(() => Promise.reject(STILL)),
    });
    fireEvent.click(button("Discard"));
    fireEvent.click(screen.getByRole("group", { name: "Confirm discard" }).querySelector("button")!);
    await flush();
    expect(button("Discarding…")).toHaveProperty("disabled", true);
    view.rerender(<RefactorSection {...props} repo={testRepo({ refactor: null })} />);
    expect(section().textContent).not.toContain("Merged");
    expect(section().textContent).not.toContain("Still running");
    expect(button(/Refactor…/)).toHaveProperty("disabled", false);
  });

  it("waits with Refactor and Resume while another launch runs", () => {
    const first = setup(testRepo(), { launching: true });
    expect(button(/Refactor…/)).toHaveProperty("disabled", true);
    first.view.unmount();
    setup(withRefactor({ state: "interrupted", commits: [] }), { launching: true });
    expect(button("Resume refactor")).toHaveProperty("disabled", true);
  });
});
