import { describe, expect, it } from "vitest";
import type { RefactorState, RepoSnapshot } from "./backend/types";
import {
  blockText,
  busyNote,
  discardQuestion,
  refactorBlocker,
  refactorMark,
  stepRefactor,
  trackRefactors,
  type RefactorAlert,
  type RefactorSeen,
} from "./refactors";
import { testRefactor, testRepo, testSession } from "./ui/testRepo";

const A = "/home/demo/projects/.refract-worktrees/tidepool-20261031-1405";
const B = "/home/demo/projects/.refract-worktrees/tidepool-20261031-1610";

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

/** Steps `states` (null = no refactor) from a repo seen without one, collecting toasts. */
function run(states: ([string, RefactorState] | null)[]): (RefactorAlert | null)[] {
  let [seen] = stepRefactor(undefined, null);
  return states.map((s) => {
    const [next, alert] = stepRefactor(seen, s && testRefactor({ worktree: s[0], state: s[1] }));
    seen = next;
    return alert;
  });
}

describe("toast once per transition", () => {
  it("tells the plan once and the finish once", () => {
    expect(
      run([[A, "starting"], [A, "working"], [A, "needsYou"], [A, "needsYou"], [A, "working"], [A, "needsYou"], [A, "ready"], [A, "ready"], null]),
    ).toEqual([null, null, "planReady", null, null, null, "finished", null, null]);
  });

  it("tells the next refactor again, and not a merge that stopped and cleared", () => {
    expect(run([[A, "needsYou"], [A, "ready"], null, [B, "needsYou"], [B, "ready"]])).toEqual([
      "planReady",
      "finished",
      null,
      "planReady",
      "finished",
    ]);
    expect(run([[A, "ready"], [A, "mergeBlocked"], [A, "ready"]])).toEqual([null, null, null]);
  });

  it("never toasts a first sighting, and one seen past planning won't call a question the plan", () => {
    const [seen, alert] = stepRefactor(undefined, testRefactor({ worktree: A, state: "ready" }));
    expect(alert).toBeNull();
    expect(stepRefactor(seen, testRefactor({ worktree: A, state: "needsYou" }))[1]).toBeNull();
    const [planning] = stepRefactor(undefined, testRefactor({ worktree: A, state: "working" }));
    expect(stepRefactor(planning, testRefactor({ worktree: A, state: "needsYou" }))[1]).toBe("planReady");
  });

  it("tracks every repo and forgets those that are gone", () => {
    const seen = new Map<string, RefactorSeen>();
    const repo = (state: RefactorState | null): RepoSnapshot =>
      testRepo({ refactor: state ? testRefactor({ worktree: A, state }) : null });
    expect(trackRefactors(seen, [repo("working")])).toEqual([]);
    expect(trackRefactors(seen, [repo("needsYou")])).toEqual([
      { path: "/home/demo/projects/tidepool", repoName: "tidepool", alert: "planReady" },
    ]);
    trackRefactors(seen, []);
    expect(seen.size).toBe(0);
    // Back as a first sighting: quiet.
    expect(trackRefactors(seen, [repo("ready")])).toEqual([]);
  });
});

