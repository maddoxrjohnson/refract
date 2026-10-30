import { describe, expect, it } from "vitest";
import type { Preselect, RepoSnapshot, SessionInfo } from "../backend/types";
import { DIRTY_REPO_PROMPT } from "../repoStatus";
import {
  initialNavState,
  matchingPaths,
  navigate,
  newAgentMode,
  pickerOptions,
  resumableSession,
  type NavContext,
  type NavInput,
  type NavResult,
  type NavState,
} from "./navigation";

function session(id: string, hoursAgo: number, kind: "interrupted" | "finished" = "interrupted"): SessionInfo {
  return {
    id,
    state: kind === "interrupted" ? { kind, detail: "apiError" } : { kind, detail: "completed" },
    lastTimestamp: new Date(Date.UTC(2026, 9, 30, 12) - hoursAgo * 3_600_000).toISOString(),
    lastAssistantText: `text of ${id}`,
    byteSize: 1000,
  };
}

function repo(
  name: string,
  extra: Partial<Pick<RepoSnapshot, "changedFiles" | "sessions" | "preselect" | "mass">> = {},
): RepoSnapshot {
  return {
    path: `/p/${name}`,
    name,
    originUrl: null,
    remote: null,
    branch: "main",
    ahead: null,
    behind: null,
    changedFiles: 0,
    dirtyPaths: [],
    lastCommit: null,
    language: null,
    trackedBytes: 0,
    commits30d: 0,
    mass: 0.5,
    sessions: [],
    preselect: { kind: "noPicker" },
    ...extra,
  };
}

// A plus shape around the centre, world y up:
//        north
//  west  centre  east
//        south
const repos = [
  repo("centre", { mass: 1 }),
  repo("north"),
  repo("south"),
  repo("east"),
  repo("west"),
  repo("eastfar"),
];
const ctx: NavContext = {
  repos: new Map(repos.map((r) => [r.path, r])),
  positions: new Map([
    ["/p/centre", { x: 0, y: 0 }],
    ["/p/north", { x: 0.2, y: 3 }],
    ["/p/south", { x: -0.2, y: -3 }],
    ["/p/east", { x: 3, y: 0.5 }],
    ["/p/west", { x: -3, y: 0 }],
    ["/p/eastfar", { x: 6, y: 0 }],
  ]),
};

const key = (k: string, mods: { ctrl?: boolean; meta?: boolean; alt?: boolean } = {}): NavInput => ({
  type: "key",
  key: { key: k, ...mods },
});

/** Applies inputs in order; returns the final result with every effect collected. */
function run(inputs: NavInput[], from: NavState = initialNavState, context = ctx): NavResult {
  let result: NavResult = { state: from, effects: [], handled: false };
  const effects = [];
  for (const input of inputs) {
    result = navigate(result.state, input, context);
    effects.push(...result.effects);
  }
  return { ...result, effects };
}

const typed = (text: string) => [...text].map((c) => key(c));

describe("pointer and clicks", () => {
  it("magnet focus sets and clears the focused gem", () => {
    expect(run([{ type: "pointerFocus", path: "/p/north" }]).state.focused).toBe("/p/north");
    expect(
      run([
        { type: "pointerFocus", path: "/p/north" },
        { type: "pointerFocus", path: null },
      ]).state.focused,
    ).toBeNull();
  });

  it("clicking a gem zooms to it", () => {
    const { state } = run([{ type: "select", path: "/p/east" }]);
    expect(state).toMatchObject({ focused: "/p/east", zoomed: "/p/east" });
  });

  it("a background click closes the picker first, then zooms out", () => {
    const dirty = repo("dirty", { changedFiles: 2, preselect: { kind: "newAgent" } });
    const c = { ...ctx, repos: new Map([[dirty.path, dirty]]) };
    let r = run([{ type: "select", path: dirty.path }, key("Enter")], initialNavState, c);
    expect(r.state.picker).not.toBeNull();
    r = run([{ type: "backgroundClick" }], r.state, c);
    expect(r.state.picker).toBeNull();
    expect(r.state.zoomed).toBe(dirty.path);
    r = run([{ type: "backgroundClick" }], r.state, c);
    expect(r.state.zoomed).toBeNull();
  });
});

describe("filter", () => {
  it("typing filters by case-insensitive substring and focuses the best match", () => {
    const { state, handled } = run(typed("EAs"));
    expect(handled).toBe(true);
    expect(state.filter).toBe("EAs");
    expect([...matchingPaths(repos, state.filter)!].sort()).toEqual(["/p/east", "/p/eastfar"]);
    expect(state.focused).toBe("/p/east");
  });

  it("prefers names that start with the filter", () => {
    expect(run(typed("th")).state.focused).toBe("/p/north");
    expect(run(typed("so")).state.focused).toBe("/p/south");
    expect(run(typed("far")).state.focused).toBe("/p/eastfar");
  });

  it("keeps the focused gem when it still matches", () => {
    const { state } = run([{ type: "pointerFocus", path: "/p/eastfar" }, ...typed("east")]);
    expect(state.focused).toBe("/p/eastfar");
  });

  it("clears focus when nothing matches", () => {
    const { state } = run([{ type: "pointerFocus", path: "/p/east" }, ...typed("zzz")]);
    expect(state.focused).toBeNull();
    expect(matchingPaths(repos, "zzz")!.size).toBe(0);
  });

  it("no filter means no matching set", () => {
    expect(matchingPaths(repos, "")).toBeNull();
  });

  it("Backspace removes the last character and is unhandled on an empty filter", () => {
    expect(run([...typed("ea"), key("Backspace")]).state.filter).toBe("e");
    expect(run([key("Backspace")]).handled).toBe(false);
  });

  it("ignores keys held with Ctrl, Meta or Alt, and space", () => {
    for (const mods of [{ ctrl: true }, { meta: true }, { alt: true }]) {
      const r = run([key("a", mods)]);
      expect(r.handled).toBe(false);
      expect(r.state.filter).toBe("");
    }
    expect(run([key(" ")]).handled).toBe(false);
  });

  it("zooms out when the zoomed gem stops matching", () => {
    const { state } = run([{ type: "select", path: "/p/west" }, ...typed("no")]);
    expect(state.zoomed).toBeNull();
    expect(state.focused).toBe("/p/north");
  });

  it("clearFilter empties it", () => {
    expect(run([...typed("ea"), { type: "clearFilter" }]).state.filter).toBe("");
  });
});

describe("Escape", () => {
  it("zooms out, keeping focus on the gem, before clearing the filter", () => {
    let r = run([...typed("e"), { type: "select", path: "/p/east" }, key("Escape")]);
    expect(r.state).toMatchObject({ zoomed: null, focused: "/p/east", filter: "e" });
    r = run([key("Escape")], r.state);
    expect(r.state.filter).toBe("");
    expect(r.handled).toBe(true);
    expect(run([key("Escape")], r.state).handled).toBe(false);
  });
});

describe("arrows", () => {
  const from = (path: string): NavState => ({ ...initialNavState, focused: path });

  it("move focus to the nearest gem in that direction", () => {
    expect(run([key("ArrowUp")], from("/p/centre")).state.focused).toBe("/p/north");
    expect(run([key("ArrowDown")], from("/p/centre")).state.focused).toBe("/p/south");
    expect(run([key("ArrowRight")], from("/p/centre")).state.focused).toBe("/p/east");
    expect(run([key("ArrowLeft")], from("/p/centre")).state.focused).toBe("/p/west");
    expect(run([key("ArrowRight")], from("/p/east")).state.focused).toBe("/p/eastfar");
  });

  it("stay put when nothing lies that way", () => {
    const r = run([key("ArrowRight")], from("/p/eastfar"));
    expect(r.state.focused).toBe("/p/eastfar");
    expect(r.handled).toBe(true);
  });

  it("start from the gem nearest the centre when nothing is focused", () => {
    expect(run([key("ArrowLeft")]).state.focused).toBe("/p/centre");
  });

  it("skip gems the filter hides", () => {
    // "s" matches south, east, west, eastfar; not north or centre.
    const r = run([...typed("s"), { type: "pointerFocus", path: "/p/south" }, key("ArrowUp")]);
    expect(r.state.focused).not.toBe("/p/north");
    expect(r.state.focused).not.toBe("/p/centre");
    expect(r.state.focused).not.toBe("/p/south");
  });

  it("move the zoom along when zoomed", () => {
    const r = run([{ type: "select", path: "/p/centre" }, key("ArrowRight")]);
    expect(r.state).toMatchObject({ focused: "/p/east", zoomed: "/p/east" });
  });

  it("start from the zoomed gem when zoomed, whatever is focused", () => {
    const zoomedOn = (zoomed: string, focused: string | null): NavState => ({
      ...initialNavState,
      zoomed,
      focused,
    });
    expect(run([key("ArrowRight")], zoomedOn("/p/east", null)).state).toMatchObject({
      focused: "/p/eastfar",
      zoomed: "/p/eastfar",
    });
    expect(run([key("ArrowUp")], zoomedOn("/p/centre", "/p/west")).state).toMatchObject({
      focused: "/p/north",
      zoomed: "/p/north",
    });
    expect(run([key("ArrowRight")], zoomedOn("/p/eastfar", "/p/centre")).state).toMatchObject({
      focused: "/p/eastfar",
      zoomed: "/p/eastfar",
    });
  });
});

describe("Enter and launching", () => {
  it("zooms the focused gem, then launches a clean repo straight away", () => {
    let r = run([{ type: "pointerFocus", path: "/p/north" }, key("Enter")]);
    expect(r.state.zoomed).toBe("/p/north");
    expect(r.effects).toEqual([]);
    r = run([key("Enter")], r.state);
    expect(r.effects).toEqual([{ kind: "launch", path: "/p/north", mode: { kind: "new" } }]);
    expect(r.state.picker).toBeNull();
  });

  it("is unhandled with nothing focused", () => {
    expect(run([key("Enter")]).handled).toBe(false);
  });

  const old = session("old", 30);
  const recent = session("recent", 2);
  const done = session("done", 1, "finished");
  const busy = (preselect: Preselect) =>
    repo("busy", { changedFiles: 3, sessions: [old, done, recent], preselect });
  const ctxFor = (r: RepoSnapshot): NavContext => ({ repos: new Map([[r.path, r]]), positions: new Map() });

  it("opens the picker with unfinished sessions newest first, then New agent", () => {
    const b = busy({ kind: "resume", sessionId: "recent" });
    const r = run([{ type: "select", path: b.path }, key("Enter")], initialNavState, ctxFor(b));
    expect(r.effects).toEqual([]);
    expect(r.state.picker).toEqual({
      path: b.path,
      options: [
        { kind: "resume", session: recent },
        { kind: "resume", session: old },
        { kind: "new", prompt: DIRTY_REPO_PROMPT },
      ],
      index: 0,
    });
  });

  it("preselects New agent when asked, and resumes the named session", () => {
    const b = busy({ kind: "newAgent" });
    expect(run([{ type: "launch", path: b.path }], initialNavState, ctxFor(b)).state.picker?.index).toBe(2);
    const c = busy({ kind: "resume", sessionId: "old" });
    expect(run([{ type: "launch", path: c.path }], initialNavState, ctxFor(c)).state.picker?.index).toBe(1);
  });

  it("moves with Up/Down within bounds and accepts with Enter", () => {
    const b = busy({ kind: "resume", sessionId: "recent" });
    const c = ctxFor(b);
    let r = run([{ type: "launch", path: b.path }, key("ArrowUp")], initialNavState, c);
    expect(r.state.picker?.index).toBe(0);
    r = run([key("ArrowDown"), key("ArrowDown"), key("ArrowDown")], r.state, c);
    expect(r.state.picker?.index).toBe(2);
    r = run([key("Enter")], r.state, c);
    expect(r.effects).toEqual([
      { kind: "launch", path: b.path, mode: { kind: "new", prompt: DIRTY_REPO_PROMPT } },
    ]);
    expect(r.state.picker).toBeNull();
  });

  it("accepts a resume option by index from the mouse", () => {
    const b = busy({ kind: "newAgent" });
    const c = ctxFor(b);
    const r = run(
      [{ type: "launch", path: b.path }, { type: "pickerMove", index: 1 }, { type: "pickerAccept" }],
      initialNavState,
      c,
    );
    expect(r.effects).toEqual([{ kind: "launch", path: b.path, mode: { kind: "resume", sessionId: "old" } }]);
  });

  it("closes the picker on Escape without zooming out, and ignores typing while open", () => {
    const b = busy({ kind: "newAgent" });
    const c = ctxFor(b);
    let r = run([{ type: "select", path: b.path }, key("Enter"), key("x")], initialNavState, c);
    expect(r.state.filter).toBe("");
    r = run([key("Escape")], r.state, c);
    expect(r.state.picker).toBeNull();
    expect(r.state.zoomed).toBe(b.path);
  });

  it("helpers give the direct New agent and Resume modes", () => {
    const b = busy({ kind: "newAgent" });
    expect(newAgentMode(b)).toEqual({ kind: "new", prompt: DIRTY_REPO_PROMPT });
    expect(newAgentMode(repo("clean"))).toEqual({ kind: "new" });
    expect(resumableSession(b)).toBe(recent);
    expect(resumableSession(busy({ kind: "resume", sessionId: "old" }))).toBe(old);
    expect(resumableSession(repo("clean"))).toBeNull();
    expect(pickerOptions(repo("clean"))).toEqual([{ kind: "new" }]);
  });
});

describe("remote-only gems", () => {
  const key = "remote:github.com/demo-user/orbit-cli";
  const remote = { ...repo("orbit-cli"), path: key };
  const c: NavContext = {
    repos: new Map([[key, remote], ...ctx.repos]),
    positions: new Map([[key, { x: 0, y: -6 }], ...ctx.positions]),
    remoteOnly: new Set([key]),
  };

  it("Enter on a zoomed remote-only gem clones instead of launching", () => {
    const r = run([{ type: "select", path: key }, { type: "key", key: { key: "Enter" } }], initialNavState, c);
    expect(r.effects).toEqual([{ kind: "clone", key }]);
    expect(r.state.picker).toBeNull();
  });

  it("the primary action clones too, and local gems still launch", () => {
    expect(run([{ type: "select", path: key }, { type: "launch" }], initialNavState, c).effects).toEqual([
      { kind: "clone", key },
    ]);
    expect(run([{ type: "launch", path: "/p/east" }], initialNavState, c).effects).toEqual([
      { kind: "launch", path: "/p/east", mode: { kind: "new" } },
    ]);
  });

  it("remote-only gems are focusable with the arrows", () => {
    const r = run([{ type: "pointerFocus", path: "/p/south" }, { type: "key", key: { key: "ArrowDown" } }], initialNavState, c);
    expect(r.state.focused).toBe(key);
  });
});

describe("sync", () => {
  it("drops focus, zoom and picker for repos that went away", () => {
    const state: NavState = {
      focused: "/p/gone",
      zoomed: "/p/gone",
      filter: "",
      picker: { path: "/p/gone", options: [{ kind: "new" }], index: 0 },
    };
    expect(run([{ type: "sync" }], state).state).toEqual(initialNavState);
  });

  it("returns the same state object when nothing changed", () => {
    const state: NavState = { ...initialNavState, focused: "/p/east" };
    expect(navigate(state, { type: "sync" }, ctx).state).toBe(state);
  });
});
