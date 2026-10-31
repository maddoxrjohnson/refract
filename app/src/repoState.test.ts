import { describe, expect, it } from "vitest";
import type { AgentEvent, LiveState, Preselect, RepoSnapshot, SessionInfo } from "./backend/types";
import { initialRepoState, repoReducer, type RepoAction, type RepoState } from "./repoState";

function repo(path: string, branch = "main"): RepoSnapshot {
  return {
    path,
    name: path.split("/").pop() ?? path,
    originUrl: null,
    remote: null,
    branch,
    ahead: null,
    behind: null,
    changedFiles: 0,
    dirtyPaths: [],
    lastCommit: null,
    language: null,
    trackedBytes: 0,
    commits30d: 0,
    mass: 0,
    sessions: [],
    preselect: { kind: "noPicker" },
  };
}

const event = (e: AgentEvent): RepoAction => ({ type: "event", event: e });
const run = (actions: RepoAction[], from: RepoState = initialRepoState) =>
  actions.reduce(repoReducer, from);
const session = (id: string, detail: LiveState): SessionInfo => ({
  id,
  state: { kind: "live", detail },
  lastTimestamp: null,
  lastAssistantText: null,
  byteSize: 0,
  focusable: true,
});
const changed = (repoPath: string, s: SessionInfo, preselect: Preselect) =>
  event({ event: "sessionChanged", data: { repoPath, session: s, preselect } });
const branches = (state: RepoState) =>
  Object.fromEntries([...state.repos].map(([path, r]) => [path, r.branch]));

describe("repoReducer", () => {
  it("takes the list when no events raced it", () => {
    const state = run([
      { type: "fetchStarted" },
      { type: "listed", list: { repos: [repo("/a"), repo("/b")], loading: true } },
    ]);
    expect(branches(state)).toEqual({ "/a": "main", "/b": "main" });
    expect(state.loading).toBe(true);
  });

  it("keeps an event that arrived before the list answered", () => {
    const state = run([
      { type: "fetchStarted" },
      event({ event: "repoChanged", data: repo("/a", "feature") }),
      event({ event: "repoChanged", data: repo("/c", "main") }),
      event({ event: "repoRemoved", data: { path: "/b" } }),
      { type: "listed", list: { repos: [repo("/a"), repo("/b")], loading: true } },
    ]);
    expect(branches(state)).toEqual({ "/a": "feature", "/c": "main" });
  });

  it("ignores loading in a list that answered after loadComplete", () => {
    const state = run([
      { type: "fetchStarted" },
      event({ event: "loadComplete" }),
      { type: "listed", list: { repos: [repo("/a")], loading: true } },
    ]);
    expect(state.loading).toBe(false);
  });

  it("starts over on a new fetch, as after a helper restart", () => {
    const before = run([
      { type: "fetchStarted" },
      event({ event: "repoChanged", data: repo("/old") }),
      event({ event: "loadComplete" }),
      { type: "listed", list: { repos: [repo("/old")], loading: false } },
    ]);
    const after = run(
      [
        { type: "fetchStarted" },
        { type: "listed", list: { repos: [repo("/new")], loading: true } },
      ],
      before,
    );
    expect(branches(after)).toEqual({ "/new": "main" });
    expect(after.loading).toBe(true);
  });

  it("applies a session's new state and the repo's preselect", () => {
    const a = { ...repo("/a"), sessions: [session("s1", "working"), session("s2", "idle")] };
    const state = run([
      { type: "fetchStarted" },
      { type: "listed", list: { repos: [a, repo("/b")], loading: false } },
      event({
        event: "sessionChanged",
        data: {
          repoPath: "/a",
          session: session("s1", "waitingOnUser"),
          preselect: { kind: "focus", sessionId: "s1" },
        },
      }),
      event({
        event: "sessionChanged",
        data: {
          repoPath: "/gone",
          session: session("s9", "working"),
          preselect: { kind: "newAgent" },
        },
      }),
    ]);
    const updated = state.repos.get("/a");
    expect(updated?.sessions.map((s) => s.state.detail)).toEqual(["waitingOnUser", "idle"]);
    expect(updated?.preselect).toEqual({ kind: "focus", sessionId: "s1" });
    expect([...state.repos.keys()]).toEqual(["/a", "/b"]);
  });

  it("puts a session change that raced the list onto the answer, not onto old state", () => {
    const old = { ...repo("/a"), sessions: [session("s1", "working")] };
    const fresh = {
      ...repo("/a", "feat"),
      sessions: [session("s1", "working"), session("s3", "idle")],
    };
    const state = run([
      { type: "listed", list: { repos: [old], loading: false } },
      { type: "fetchStarted" },
      changed("/a", session("s1", "waitingOnUser"), { kind: "focus", sessionId: "s1" }),
      { type: "listed", list: { repos: [fresh], loading: false } },
    ]);
    const a = state.repos.get("/a");
    expect(a?.branch).toBe("feat");
    expect(a?.sessions.map((s) => [s.id, s.state.detail])).toEqual([
      ["s1", "waitingOnUser"],
      ["s3", "idle"],
    ]);
    expect(a?.preselect).toEqual({ kind: "focus", sessionId: "s1" });
  });

  it("keeps a session change for a repo the first list has not brought yet", () => {
    const a = { ...repo("/a"), sessions: [session("s1", "working"), session("s2", "working")] };
    const state = run([
      { type: "fetchStarted" },
      changed("/a", session("s1", "waitingOnUser"), { kind: "newAgent" }),
      changed("/a", session("s2", "idle"), { kind: "focus", sessionId: "s1" }),
      { type: "listed", list: { repos: [a], loading: true } },
    ]);
    const shown = state.repos.get("/a");
    expect(shown?.sessions.map((s) => s.state.detail)).toEqual(["waitingOnUser", "idle"]);
    expect(shown?.preselect).toEqual({ kind: "focus", sessionId: "s1" });
  });

  it("puts a session change onto a snapshot an earlier event of the same fetch brought", () => {
    const listed = { ...repo("/a"), sessions: [session("s1", "working")] };
    const state = run([
      { type: "fetchStarted" },
      event({
        event: "repoChanged",
        data: { ...repo("/a", "dev"), sessions: [session("s1", "working")] },
      }),
      changed("/a", session("s1", "waitingOnUser"), { kind: "focus", sessionId: "s1" }),
      { type: "listed", list: { repos: [listed], loading: false } },
    ]);
    expect(state.repos.get("/a")?.branch).toBe("dev");
    expect(state.repos.get("/a")?.sessions[0]?.state.detail).toBe("waitingOnUser");
  });

  it("lets a later repo snapshot replace an earlier session change", () => {
    const listed = { ...repo("/a"), sessions: [session("s1", "working")] };
    const state = run([
      { type: "fetchStarted" },
      changed("/a", session("s1", "waitingOnUser"), { kind: "focus", sessionId: "s1" }),
      event({ event: "repoChanged", data: { ...repo("/a"), sessions: [session("s1", "idle")] } }),
      { type: "listed", list: { repos: [listed], loading: false } },
    ]);
    expect(state.repos.get("/a")?.sessions[0]?.state.detail).toBe("idle");
  });

  it("ignores a change to a session the repo does not have", () => {
    const a = { ...repo("/a"), sessions: [session("s1", "working")] };
    const state = run([
      { type: "fetchStarted" },
      { type: "listed", list: { repos: [a], loading: false } },
      changed("/a", session("s9", "waitingOnUser"), { kind: "focus", sessionId: "s9" }),
    ]);
    expect(state.repos.get("/a")).toEqual(a);
  });

  it("applies events after the list", () => {
    const state = run([
      { type: "fetchStarted" },
      { type: "listed", list: { repos: [repo("/a"), repo("/b")], loading: true } },
      event({ event: "repoChanged", data: repo("/a", "dev") }),
      event({ event: "repoRemoved", data: { path: "/b" } }),
      event({ event: "loadComplete" }),
    ]);
    expect(branches(state)).toEqual({ "/a": "dev" });
    expect(state.loading).toBe(false);
  });
});
