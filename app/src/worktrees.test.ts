import { describe, expect, it } from "vitest";
import type { LiveState, MissingWorktreeStatus, WorktreeInfo } from "./backend/types";
import { repoStatus } from "./repoStatus";
import { testRefactor, testRepo, testSession, testWorktree } from "./ui/testRepo";
import { gitStatusLabel, ignoredList, missingWorktreeLabel, worktreeAgent, worktreeView } from "./worktrees";

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
      [{ kind: "onlyHereUnknown" }, "Can't tell if its commits are pushed"],
      [{ kind: "holdsRepo" }, "Holds another repo"],
      [{ kind: "tooBigToCheck" }, "Too many ignored files to check"],
      [{ kind: "release", tag: "v1.4.0" }, "In v1.4.0"],
      [{ kind: "merged", branch: "main" }, "Merged into main"],
      [{ kind: "pushed" }, "Pushed, not merged"],
    ];
    for (const [status, label] of rows) expect(gitStatusLabel(status)).toBe(label);
  });

  it("offer Remove when the helper allows it and no agent is open", () => {
    const view = worktreeView(testWorktree("cards", { status: { kind: "release", tag: "v1.4.0" } }));
    expect(view).toEqual({ label: "In v1.4.0", tone: "done", removable: true, agentOpen: false, checkable: false });
    const dirty = worktreeView(testWorktree("search", { status: { kind: "changes", count: 3 }, removable: false }));
    expect(dirty).toEqual({ label: "3 uncommitted changes", tone: "keep", removable: false, agentOpen: false, checkable: false });
  });

  it("hold back Remove from a worktree with a refactor, and say why", () => {
    const refactored = testWorktree("cards", { status: { kind: "pushed" } }, { refactor: testRefactor() });
    expect(worktreeView(refactored)).toEqual({ label: "Has a refactor", tone: "keep", removable: false, agentOpen: false, checkable: false });
    // Something that could be lost still says so.
    const dirty = testWorktree("search", { status: { kind: "changes", count: 3 }, removable: false }, { refactor: testRefactor() });
    expect(worktreeView(dirty)?.label).toBe("3 uncommitted changes");
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
    expect(worktreeView(idle)).toEqual({ label: "Merged into main", tone: "done", removable: false, agentOpen: true, checkable: false });
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

  it("offer Check… on a Windows drive when only the unread files could keep it", () => {
    const unchecked = { untrackedChecked: false, status: { kind: "unchecked" }, removable: false, checkable: true } as const;
    const wt = testWorktree("tablet", unchecked);
    expect(worktreeView(wt)).toEqual({
      label: "Changes not checked (Windows drive)",
      tone: "keep",
      removable: false,
      agentOpen: false,
      checkable: true,
    });
    // Older helpers send no `checkable`.
    expect(worktreeView(testWorktree("old", { ...unchecked, checkable: undefined }))?.checkable).toBe(false);
    // An agent or a refactor holds it back, as it does Remove.
    const idle = testWorktree("idle", unchecked, { sessions: [live("i", "idle")] });
    expect(worktreeView(idle)).toMatchObject({ checkable: false, agentOpen: true });
    const working = testWorktree("busy", unchecked, { sessions: [live("w", "working")] });
    expect(worktreeView(working)).toMatchObject({ label: "Agent running", checkable: false });
    const refactored = testWorktree("ref", unchecked, { refactor: testRefactor() });
    expect(worktreeView(refactored)).toMatchObject({ label: "Has a refactor", checkable: false });
  });

  it("let a check's answer stand in for the git status", () => {
    const wt = testWorktree("tablet", { status: { kind: "unchecked" }, removable: false, checkable: true });
    const pushed = { status: { kind: "pushed" }, removable: true, ignored: ["node_modules/"] } as const;
    expect(worktreeView(wt, { ...pushed, ignored: [...pushed.ignored] })).toEqual({
      label: "Pushed, not merged",
      tone: "done",
      removable: true,
      agentOpen: false,
      checkable: true,
    });
    // Check again stays, whatever the answer.
    const found = worktreeView(wt, { status: { kind: "changes", count: 2 }, removable: false, ignored: [] });
    expect(found).toMatchObject({ label: "2 uncommitted changes", tone: "keep", removable: false, checkable: true });
  });

  it("say each missing folder's status in the spec's words", () => {
    const rows: [MissingWorktreeStatus, string][] = [
      [{ kind: "locked" }, "Folder is gone · Locked"],
      [{ kind: "driveNotAttached" }, "Folder is gone · Drive not attached"],
      [{ kind: "moved", to: "/home/dev/projects/tidepool-moved" }, "Moved to /home/dev/projects/tidepool-moved"],
      [{ kind: "ownRefs" }, "Folder is gone · Has its own refs"],
      [{ kind: "commitsOnlyHere" }, "Folder is gone · Its commits would be lost"],
      [{ kind: "gone" }, "Folder is gone"],
    ];
    for (const [status, label] of rows) expect(missingWorktreeLabel(status)).toBe(label);
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
