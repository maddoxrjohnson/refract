import { describe, expect, it } from "vitest";
import {
  blockText,
  busyNote,
  discardQuestion,
  refactorBlocker,
  refactorMark,
} from "./refactors";
import { testRefactor, testRepo, testSession } from "./ui/testRepo";


describe("refactorMark", () => {
  it("turns while starting or working, is red when it needs you, a seal when ready, plain when done", () => {
    const marks = (["starting", "working", "needsYou", "interrupted", "mergeBlocked", "ready", "nothingToDo"] as const).map(
      (state) => refactorMark(testRefactor({ state })),
    );
    expect(marks).toEqual(["turning", "turning", "alert", "alert", "alert", "ready", "plain"]);
    expect(refactorMark(null)).toBeNull();
    expect(refactorMark(undefined)).toBeNull();
  });
});

describe("the dialog's note and the button's blockers", () => {
  it("says nothing for a clean repo with no agent", () => {
    expect(busyNote(testRepo())).toBeNull();
  });

  it("names the commit and the uncommitted files left out", () => {
    const repo = testRepo({ changedFiles: 3, lastCommit: { summary: "x", time: 0, sha: "a1b2c3d" } });
    expect(busyNote(repo)).toBe("Starts from a1b2c3d. Your 3 uncommitted files aren't included.");
    expect(busyNote({ ...repo, changedFiles: 1, lastCommit: { summary: "x", time: 0 } })).toBe(
      "Starts from the last commit. Your 1 uncommitted file aren't included.",
    );
  });

  it("mentions a running agent", () => {
    const repo = testRepo({ sessions: [testSession("s", { kind: "live", detail: "working" }, 0)] });
    expect(busyNote(repo)).toContain("The agent running here carries on undisturbed.");
  });

  it("needs a branch and a commit", () => {
    expect(refactorBlocker(testRepo())).toBeNull();
    expect(refactorBlocker(testRepo({ branch: null }))).toBe("Check out a branch first.");
    expect(refactorBlocker(testRepo({ lastCommit: null }))).toBe("Commit something first.");
  });

  it("words merge blocks and the discard question", () => {
    expect(blockText({ reason: "targetMissing" }, "main")).toBe("The branch main no longer exists.");
    for (const reason of ["dirtyOverlap", "conflict", "live", "targetBusy", "worktreeBusy", "worktreeDirty", "targetRewound"] as const) {
      expect(blockText({ reason }, "main").length).toBeGreaterThan(10);
    }
    expect(discardQuestion(3)).toBe("Discard 3 commits?");
    expect(discardQuestion(1)).toBe("Discard 1 commit?");
    expect(discardQuestion(0)).toBe("Remove the refactor's worktree and branch?");
  });
});

