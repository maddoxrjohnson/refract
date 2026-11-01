import { describe, expect, it } from "vitest";
import type { LiveState, WorktreeInfo } from "./backend/types";
import { repoStatus } from "./repoStatus";
import { testRepo, testSession, testWorktree } from "./ui/testRepo";
import { gitStatusLabel, ignoredList, worktreeAgent, worktreeView } from "./worktrees";

const live = (id: string, detail: LiveState) => testSession(id, { kind: "live", detail }, 0.1);

describe("worktree rows", () => {
  it("say each git status in the spec's words", () => {
    const rows: [WorktreeInfo["status"], string][] = [
      [{ kind: "locked" }, "Locked"],
      [{ kind: "operation", op: "rebase" }, "Mid-rebase"],
      [{ kind: "operation", op: "merge" }, "Mid-merge"],
      [{ kind: "operation", op: "cherryPick" }, "Mid-cherry-pick"],
      [{ kind: "operation", op: "revert" }, "Mid-revert"],
      [{ kind: "operation", op: "bisect" }, "Bisecting"],
      [{ kind: "changes", count: 3 }, "3 uncommitted changes"],
      [{ kind: "changes", count: 1 }, "1 uncommitted change"],
      [{ kind: "unchecked" }, "Changes not checked (Windows drive)"],
      [{ kind: "onlyHere", count: 2 }, "2 commits only on this computer"],
      [{ kind: "onlyHere", count: 1 }, "1 commit only on this computer"],
      [{ kind: "onlyHere", count: 100 }, "100+ commits only on this computer"],
      [{ kind: "release", tag: "v1.4.0" }, "In v1.4.0"],
      [{ kind: "merged", branch: "main" }, "Merged into main"],
      [{ kind: "pushed" }, "Pushed, not merged"],
    ];
    for (const [status, label] of rows) expect(gitStatusLabel(status)).toBe(label);
  });

  it("offer Remove when the helper allows it and no agent is open", () => {
    const view = worktreeView(testWorktree("cards", { status: { kind: "release", tag: "v1.4.0" } }));
    expect(view).toEqual({ label: "In v1.4.0", tone: "done", removable: true, agentOpen: false });
    const dirty = worktreeView(testWorktree("search", { status: { kind: "changes", count: 3 }, removable: false }));
    expect(dirty).toEqual({ label: "3 uncommitted changes", tone: "keep", removable: false, agentOpen: false });
  });

  it("put the agent rows first", () => {
    const dirty = { status: { kind: "changes", count: 2 }, removable: false } as const;
    const waiting = testWorktree("a", dirty, { sessions: [live("w", "working"), live("x", "waitingOnUser")] });
    expect(worktreeAgent(waiting)).toBe("waiting");
    expect(worktreeView(waiting)).toMatchObject({ label: "Agent waiting for you", tone: "attention", removable: false });
    const working = testWorktree("b", {}, { sessions: [live("w", "working"), live("i", "idle")] });
    expect(worktreeView(working)).toMatchObject({ label: "Agent running", tone: "working", removable: false });
  });

  it("keep the git status for an idle agent, but hold Remove until it closes", () => {
    const idle = testWorktree("c", { status: { kind: "merged", branch: "main" } }, { sessions: [live("i", "idle")] });
    expect(worktreeView(idle)).toEqual({ label: "Merged into main", tone: "done", removable: false, agentOpen: true });
    // Nothing to hold when git already keeps it.
    const kept = testWorktree("d", { status: { kind: "locked" }, removable: false }, { sessions: [live("i", "idle")] });
    expect(worktreeView(kept)?.agentOpen).toBe(false);
  });

  it("finished and interrupted sessions do not count as agents", () => {
    const wt = testWorktree("e", {}, {
      sessions: [
        testSession("f", { kind: "finished", detail: "completed" }, 1),
        testSession("g", { kind: "interrupted", detail: "apiError" }, 2),
      ],
    });
    expect(worktreeAgent(wt)).toBeNull();
    expect(worktreeView(wt)?.removable).toBe(true);
  });

  it("have no view without the helper's facts", () => {
    expect(worktreeView(testRepo())).toBeNull();
  });

  it("list up to 8 ignored entries and say when there are more", () => {
    expect(ignoredList([".env", "build/"])).toEqual({ shown: [".env", "build/"], more: false });
    const nine = Array.from({ length: 9 }, (_, i) => `f${i}`);
    expect(ignoredList(nine)).toEqual({ shown: nine.slice(0, 8), more: true });
  });
});

describe("the main stone", () => {
  it("shows the most urgent agent of the repo and its worktrees", () => {
    const main = testRepo();
    expect(repoStatus(main)).toMatchObject({ working: false, waiting: false });
    const working = testWorktree("a", {}, { sessions: [live("w", "working")] });
    const waiting = testWorktree("b", {}, { sessions: [live("x", "waitingOnUser")] });
    expect(repoStatus(main, [working])).toMatchObject({ working: true, waiting: false });
    expect(repoStatus(main, [working, waiting])).toMatchObject({ working: true, waiting: true });
  });

  it("keeps the worktrees' changes and stopped sessions to their own rows", () => {
    const wt = testWorktree("a", {}, {
      changedFiles: 4,
      sessions: [testSession("g", { kind: "interrupted", detail: "apiError" }, 2)],
    });
    expect(repoStatus(testRepo(), [wt])).toEqual({ dirty: false, interrupted: false, working: false, waiting: false });
  });
});
