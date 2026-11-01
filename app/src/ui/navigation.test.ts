import { describe, expect, it } from "vitest";
import type { LiveState, Preselect, RepoSnapshot, SessionInfo } from "../backend/types";
import { DIRTY_REPO_PROMPT, repoStatus } from "../repoStatus";
import {
  initialNavState,
  matchingPaths,
  navigate,
  newAgentMode,
  optionKey,
  optionMode,
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
    focusable: false,
    resumeReason: kind === "interrupted" ? "interrupted" : null,
  };
}

function running(id: string, hoursAgo: number, detail: LiveState, focusable = true): SessionInfo {
  return { ...session(id, hoursAgo), state: { kind: "live", detail }, focusable, resumeReason: null };
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

  it("offers finished sessions that have a resume reason, without chipping the stone", () => {
    const asked = { ...session("asked", 0.5, "finished"), resumeReason: "askedQuestion" as const };
    const r = repo("asked", { sessions: [old, asked, done], preselect: { kind: "resume", sessionId: "asked" } });
    expect(pickerOptions(r)).toEqual([
      { kind: "resume", session: asked },
      { kind: "resume", session: old },
      { kind: "new" },
    ]);
    expect(resumableSession(r)).toBe(asked);
    expect(run([{ type: "launch", path: r.path }], initialNavState, ctxFor(r)).state.picker?.index).toBe(0);
    expect(repoStatus(repo("x", { sessions: [asked] })).interrupted).toBe(false);
    expect(repoStatus(repo("x", { sessions: [old] })).interrupted).toBe(true);
  });
});

describe("going to a running agent", () => {
  const waiting = running("waiting", 1, "waitingOnUser");
  const working = running("working", 0.5, "working");
  const outside = running("outside", 0.2, "working", false);
  const stopped = session("stopped", 3);
  const live = (preselect: Preselect) =>
    repo("live", { sessions: [stopped, waiting, outside, working], preselect });
  const ctxFor = (r: RepoSnapshot): NavContext => ({ repos: new Map([[r.path, r]]), positions: new Map() });

  it("lists focusable running agents newest first, then resumes, then New agent", () => {
    expect(pickerOptions(live({ kind: "newAgent" }))).toEqual([
      { kind: "focus", session: working },
      { kind: "focus", session: waiting },
      { kind: "resume", session: stopped },
      { kind: "new" },
    ]);
  });

  it("preselects the named running agent and goes to it on Enter", () => {
    const r = live({ kind: "focus", sessionId: "waiting" });
    const c = ctxFor(r);
    let res = run([{ type: "launch", path: r.path }], initialNavState, c);
    expect(res.state.picker?.index).toBe(1);
    res = run([key("Enter")], res.state, c);
    expect(res.effects).toEqual([
      { kind: "launch", path: r.path, mode: { kind: "focus", sessionId: "waiting" } },
    ]);
  });

  it("falls back to New agent when the named agent is not listed", () => {
    const r = live({ kind: "focus", sessionId: "outside" });
    expect(run([{ type: "launch", path: r.path }], initialNavState, ctxFor(r)).state.picker?.index).toBe(3);
  });

  it("maps each option to its mode, with keys unique across kinds", () => {
    const same = { ...stopped, id: "same" };
    const options = [
      { kind: "focus" as const, session: { ...waiting, id: "same" } },
      { kind: "resume" as const, session: same },
      { kind: "new" as const },
    ];
    expect(options.map(optionMode)).toEqual([
      { kind: "focus", sessionId: "same" },
      { kind: "resume", sessionId: "same" },
      { kind: "new" },
    ]);
    expect(new Set(options.map(optionKey)).size).toBe(3);
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

describe("Alt+R", () => {
  const committed = (name: string, extra: Partial<RepoSnapshot> = {}): RepoSnapshot => ({
    ...repo(name),
    lastCommit: { summary: "Initial commit", time: 0, sha: "a1b2c3d" },
    ...extra,
  });
  const info = { state: "working", worktree: "/w", folderUri: "vscode-remote://wsl+Demo/w" } as RepoSnapshot["refactor"];
  const remoteKey = "remote:github.com/demo-user/orbit-cli";
  const local = [
    committed("plain"),
    committed("busy", { refactor: info }),
    committed("detached", { branch: null }),
    repo("empty"),
  ];
  const c: NavContext = {
    repos: new Map([...local, { ...committed("orbit-cli"), path: remoteKey }].map((r) => [r.path, r])),
    positions: new Map(),
    remoteOnly: new Set([remoteKey]),
  };
  const altR = (path: string, k = "r") =>
    run([{ type: "select", path }, key(k, { alt: true })], initialNavState, c);

  it("opens the Refactor dialog on the zoomed repo, with either case", () => {
    for (const k of ["r", "R"]) {
      const r = altR("/p/plain", k);
      expect(r.handled).toBe(true);
      expect(r.effects).toEqual([{ kind: "refactorDialog", path: "/p/plain" }]);
    }
  });

  it("matches the physical R key on other layouts", () => {
    const r = run([{ type: "select", path: "/p/plain" }, { type: "key", key: { key: "к", code: "KeyR", alt: true } }], initialNavState, c);
    expect(r.effects).toEqual([{ kind: "refactorDialog", path: "/p/plain" }]);
    const t = run([{ type: "select", path: "/p/plain" }, { type: "key", key: { key: "r", code: "KeyT", alt: true } }], initialNavState, c);
    expect(t.handled).toBe(false);
  });

  it("goes to the refactor once the repo has one", () => {
    expect(altR("/p/busy").effects).toEqual([{ kind: "openRefactor", path: "/p/busy" }]);
  });

  it("does nothing unzoomed, on remote-only repos, detached HEADs, repos without commits or in the picker", () => {
    for (const path of [remoteKey, "/p/detached", "/p/empty"]) {
      const r = altR(path);
      expect(r.handled, path).toBe(false);
      expect(r.effects).toEqual([]);
    }
    expect(run([{ type: "pointerFocus", path: "/p/plain" }, key("r", { alt: true })], initialNavState, c).handled).toBe(
      false,
    );
    const picker = { path: "/p/plain", options: [{ kind: "new" as const }], index: 0 };
    const r = navigate({ ...initialNavState, zoomed: "/p/plain", picker }, key("r", { alt: true }), c);
    expect(r.handled).toBe(false);
  });

  it("leaves Ctrl+Alt+R and other Alt keys alone", () => {
    expect(run([{ type: "select", path: "/p/plain" }, key("r", { alt: true, ctrl: true })], initialNavState, c).effects).toEqual([]);
    expect(run([{ type: "select", path: "/p/plain" }, key("t", { alt: true })], initialNavState, c).handled).toBe(false);
  });
});

describe("sync", () => {
  it("drops focus, zoom and picker for repos that went away", () => {
    const state: NavState = {
      focused: "/p/gone",
      zoomed: "/p/gone",
      filter: "",
      picker: { path: "/p/gone", options: [{ kind: "new" }], index: 0 },
      follow: null,
    };
    expect(run([{ type: "sync" }], state).state).toEqual(initialNavState);
  });

  it("returns the same state object when nothing changed", () => {
    const state: NavState = { ...initialNavState, focused: "/p/east" };
    expect(navigate(state, { type: "sync" }, ctx).state).toBe(state);
  });
});

describe("following a clone", () => {
  const remote = { host: "github.com", owner: "demo-user", name: "kelp-cli" };
  const remoteGem = { ...repo("kelp-cli"), path: "remote:github.com/demo-user/kelp-cli", remote };
  const cloned = { ...repo("kelp-cli"), path: "/home/demo/projects/kelp-cli", remote };
  const context = (gems: RepoSnapshot[], remoteOnly: string[] = []): NavContext => ({
    repos: new Map(gems.map((r) => [r.path, r])),
    positions: new Map(gems.map((r, i) => [r.path, { x: i, y: 0 }])),
    remoteOnly: new Set(remoteOnly),
  });
  const before = context([...repos, remoteGem], [remoteGem.path]);
  const after = context([...repos, cloned]);
  const zoomed: NavState = { ...initialNavState, focused: remoteGem.path, zoomed: remoteGem.path };

  it("moves focus and zoom to the clone when its repo is already listed", () => {
    const r = navigate(zoomed, { type: "rekey", from: remoteGem.path, to: cloned.path }, after);
    expect(r.state).toEqual({ ...zoomed, focused: cloned.path, zoomed: cloned.path });
  });

  it("waits for the clone's repo when it is not listed yet, then follows on sync", () => {
    const both = context([...repos, remoteGem, { ...cloned, remote: null }], [remoteGem.path]);
    let r = navigate(zoomed, { type: "rekey", from: remoteGem.path, to: cloned.path }, before);
    expect(r.state.zoomed).toBe(remoteGem.path);
    r = navigate(r.state, { type: "sync" }, before);
    expect(r.state.zoomed).toBe(remoteGem.path);
    // Even a clone whose origin did not match the listing (so both gems show) is followed.
    r = navigate(r.state, { type: "sync" }, both);
    expect(r.state).toEqual({ ...zoomed, focused: cloned.path, zoomed: cloned.path });
  });

  it("follows a clone that shows up before the clone call answers", () => {
    const r = navigate(zoomed, { type: "sync" }, after);
    expect(r.state).toEqual({ ...zoomed, focused: cloned.path, zoomed: cloned.path });
    const late = navigate(r.state, { type: "rekey", from: remoteGem.path, to: cloned.path }, after);
    expect(late.state).toEqual(r.state);
  });

  it("does nothing once the user has moved on", () => {
    const elsewhere: NavState = { ...initialNavState, focused: "/p/east", zoomed: "/p/east" };
    const r = navigate(elsewhere, { type: "rekey", from: remoteGem.path, to: cloned.path }, before);
    expect(r.handled).toBe(false);
    expect(navigate(r.state, { type: "sync" }, after).state).toBe(r.state);
  });

  it("drops a waiting follow when the remote gem goes away without a clone", () => {
    let r = navigate(zoomed, { type: "rekey", from: remoteGem.path, to: cloned.path }, before);
    r = navigate(r.state, { type: "sync" }, ctx);
    expect(r.state).toEqual(initialNavState);
  });
});
