import { describe, expect, it } from "vitest";
import type { AgentEvent, RepoSnapshot } from "./backend/types";
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
