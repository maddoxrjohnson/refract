import type { LaunchMode, RepoSnapshot, SessionInfo } from "../backend/types";
import { isRemoteKey, remoteKey } from "../projects";
import { refactorBlocker } from "../refactors";
import { DIRTY_REPO_PROMPT, focusableSessions, unfinishedSessions } from "../repoStatus";

// Finding and choosing a project (spec §3.3, §4.3) as a pure state machine. Inputs are
// pointer, click and key events; the result says what changed and which effects to run.

/** One row of the launch picker. */
export type PickerOption =
  /** Go to the VS Code terminal already running a live session. */
  | { kind: "focus"; session: SessionInfo }
  | { kind: "resume"; session: SessionInfo }
  | { kind: "new"; prompt?: string };

export interface PickerState {
  /** The repo being launched. */
  path: string;
  options: PickerOption[];
  /** The highlighted option; Enter accepts it. */
  index: number;
}

export interface NavState {
  /** Magnet or keyboard focus: the gem that shows a label. */
  focused: string | null;
  /** The gem the camera is on, with the detail panel open. */
  zoomed: string | null;
  /** Typed text; gems whose name contains it (any case) match. */
  filter: string;
  picker: PickerState | null;
  /** A cloned gem's new key, waiting for its repo to appear; focus and zoom then move to it. */
  follow: { from: string; to: string } | null;
}

export const initialNavState: NavState = {
  focused: null,
  zoomed: null,
  filter: "",
  picker: null,
  follow: null,
};

/** A key press, reduced to what navigation needs. */
export interface NavKey {
  key: string;
  /** The physical key (`KeyboardEvent.code`), for shortcuts on any layout. */
  code?: string;
  ctrl?: boolean;
  meta?: boolean;
  alt?: boolean;
}

export type NavInput =
  /** Nearest gem to the pointer, or none. */
  | { type: "pointerFocus"; path: string | null }
  /** A click on a gem. */
  | { type: "select"; path: string }
  | { type: "backgroundClick" }
  | { type: "key"; key: NavKey }
  /**
   * What Enter does on a zoomed gem: launch, or open the picker. `path` defaults to the
   * zoomed gem; it may also be a worktree folded into a gem (its Open button).
   */
  | { type: "launch"; path?: string }
  /** Highlight a picker option (pointer hover). */
  | { type: "pickerMove"; index: number }
  /** Accept a picker option; `index` defaults to the highlighted one. */
  | { type: "pickerAccept"; index?: number }
  | { type: "pickerClose" }
  | { type: "clearFilter" }
  /** Repos changed: forget paths that are gone. */
  | { type: "sync" }
  /** A remote-only gem was cloned to `to`: focus and zoom follow it once `to` is a repo. */
  | { type: "rekey"; from: string; to: string };

export type NavEffect =
  | { kind: "launch"; path: string; mode: LaunchMode }
  /** Alt+R on a repo without a refactor: open the Refactor dialog. */
  | { kind: "refactorDialog"; path: string }
  /** Alt+R on a repo with a refactor: open or raise its worktree's VS Code window. */
  | { kind: "openRefactor"; path: string }
  /** Clone a remote-only repo, then launch a new agent in it. `key` is its gem path. */
  | { kind: "clone"; key: string };

export interface NavContext {
  repos: ReadonlyMap<string, RepoSnapshot>;
  /** Gem centres in world units, y up. */
  positions: ReadonlyMap<string, { x: number; y: number }>;
  /** Paths of gems that are not cloned yet; launching one clones it first. */
  remoteOnly?: ReadonlySet<string>;
  /**
   * Linked worktrees folded into a gem, by the gem's path. Search matches their names and
   * branches (and finds the gem); they can be launched, but are not gems themselves.
   */
  worktrees?: ReadonlyMap<string, readonly RepoSnapshot[]>;
}

export interface NavResult {
  state: NavState;
  effects: NavEffect[];
  /** The input meant something here; for keys, the caller should preventDefault. */
  handled: boolean;
}

/**
 * Paths whose name contains `filter`, ignoring case, or that hold a folded worktree whose
 * name or branch does; `null` when there is no filter.
 */
export function matchingPaths(
  repos: Iterable<RepoSnapshot>,
  filter: string,
  worktrees?: ReadonlyMap<string, readonly RepoSnapshot[]>,
): Set<string> | null {
  if (!filter) return null;
  const needle = filter.toLowerCase();
  const has = (text: string | null) => !!text && text.toLowerCase().includes(needle);
  const out = new Set<string>();
  for (const r of repos) {
    if (has(r.name) || worktrees?.get(r.path)?.some((w) => has(w.name) || has(w.branch))) out.add(r.path);
  }
  return out;
}

/** The repo at `path`: a gem, or a worktree folded into one. */
export function findRepo(ctx: NavContext, path: string): RepoSnapshot | undefined {
  const gem = ctx.repos.get(path);
  if (gem || !ctx.worktrees) return gem;
  for (const list of ctx.worktrees.values()) {
    const w = list.find((r) => r.path === path);
    if (w) return w;
  }
  return undefined;
}

/** How a new agent starts in this repo: with the review prompt when it has changes. */
export function newAgentMode(repo: RepoSnapshot): LaunchMode {
  return repo.changedFiles > 0 ? { kind: "new", prompt: DIRTY_REPO_PROMPT } : { kind: "new" };
}

/** The session a Resume button resumes: the preselected one, else the newest unfinished. */
export function resumableSession(repo: RepoSnapshot): SessionInfo | null {
  const unfinished = unfinishedSessions(repo);
  const pre = repo.preselect;
  if (pre.kind === "resume") {
    const s = unfinished.find((u) => u.id === pre.sessionId);
    if (s) return s;
  }
  return unfinished[0] ?? null;
}

/** The running agent a Go to agent button shows: the preselected one, else the newest. */
export function focusableSession(repo: RepoSnapshot): SessionInfo | null {
  const running = focusableSessions(repo);
  const pre = repo.preselect;
  if (pre.kind === "focus") {
    const s = running.find((r) => r.id === pre.sessionId);
    if (s) return s;
  }
  return running[0] ?? null;
}

/** Running agents to go to, then unfinished sessions, each newest first, then New agent. */
export function pickerOptions(repo: RepoSnapshot): PickerOption[] {
  const fresh: PickerOption =
    repo.changedFiles > 0 ? { kind: "new", prompt: DIRTY_REPO_PROMPT } : { kind: "new" };
  return [
    ...focusableSessions(repo).map((session): PickerOption => ({ kind: "focus", session })),
    ...unfinishedSessions(repo).map((session): PickerOption => ({ kind: "resume", session })),
    fresh,
  ];
}

/** The launch an option starts. */
export function optionMode(option: PickerOption): LaunchMode {
  if (option.kind === "focus") return { kind: "focus", sessionId: option.session.id };
  if (option.kind === "resume") return { kind: "resume", sessionId: option.session.id };
  return option.prompt ? { kind: "new", prompt: option.prompt } : { kind: "new" };
}

/** A stable React key for an option, unique within one picker. */
export function optionKey(option: PickerOption): string {
  return option.kind === "new" ? "new" : `${option.kind}:${option.session.id}`;
}

function openPicker(repo: RepoSnapshot): PickerState {
  const options = pickerOptions(repo);
  const pre = repo.preselect;
  let index = options.length - 1;
  if (pre.kind === "resume" || pre.kind === "focus") {
    const i = options.findIndex(
      (o) => o.kind !== "new" && o.kind === pre.kind && o.session.id === pre.sessionId,
    );
    if (i >= 0) index = i;
  }
  return { path: repo.path, options, index };
}

const result = (state: NavState, effects: NavEffect[] = [], handled = true): NavResult => ({
  state,
  effects,
  handled,
});
const unhandled = (state: NavState): NavResult => result(state, [], false);

/** The best gem for a filter: names that start with it, then the heaviest, then by name. */
function bestMatch(ctx: NavContext, matches: Set<string>, filter: string): string | null {
  const needle = filter.toLowerCase();
  let best: RepoSnapshot | null = null;
  const rank = (r: RepoSnapshot) => (r.name.toLowerCase().startsWith(needle) ? 1 : 0);
  for (const path of matches) {
    const r = ctx.repos.get(path);
    if (!r) continue;
    if (
      !best ||
      rank(r) > rank(best) ||
      (rank(r) === rank(best) &&
        (r.mass > best.mass || (r.mass === best.mass && r.name.localeCompare(best.name) < 0)))
    ) {
      best = r;
    }
  }
  return best?.path ?? null;
}

/** Applies a new filter: focus follows the best match, a zoom on a hidden gem ends. */
function withFilter(state: NavState, filter: string, ctx: NavContext): NavState {
  const matches = matchingPaths(ctx.repos.values(), filter, ctx.worktrees);
  if (!matches) return { ...state, filter };
  const zoomed = state.zoomed && matches.has(state.zoomed) ? state.zoomed : null;
  const focused =
    state.focused && matches.has(state.focused) ? state.focused : bestMatch(ctx, matches, filter);
  return { ...state, filter, zoomed, focused };
}

const DIRECTIONS: Record<string, { x: number; y: number }> = {
  ArrowUp: { x: 0, y: 1 },
  ArrowDown: { x: 0, y: -1 },
  ArrowLeft: { x: -1, y: 0 },
  ArrowRight: { x: 1, y: 0 },
};

/**
 * Nearest matching gem in a direction, weighing sideways distance double. Starts from the
 * zoomed gem when zoomed (the pointer may have focused another), else the focused one.
 */
function step(state: NavState, dir: { x: number; y: number }, ctx: NavContext): string | null {
  const matches = matchingPaths(ctx.repos.values(), state.filter, ctx.worktrees);
  const candidates = [...ctx.positions].filter(
    ([path]) => ctx.repos.has(path) && (!matches || matches.has(path)),
  );
  const origin = state.zoomed ?? state.focused;
  const from = origin ? ctx.positions.get(origin) : undefined;
  if (!from) {
    // Nothing focused: start at the gem nearest the centre.
    let best: string | null = null;
    let bestD = Infinity;
    for (const [path, p] of candidates) {
      const d = Math.hypot(p.x, p.y);
      if (d < bestD) [best, bestD] = [path, d];
    }
    return best;
  }
  let best = origin;
  let bestScore = Infinity;
  for (const [path, p] of candidates) {
    if (path === origin) continue;
    const dx = p.x - from.x;
    const dy = p.y - from.y;
    const along = dx * dir.x + dy * dir.y;
    if (along <= 1e-6) continue;
    const across = Math.abs(dx * dir.y - dy * dir.x);
    const score = along + 2 * across;
    if (score < bestScore) [best, bestScore] = [path, score];
  }
  return best;
}

function launch(state: NavState, path: string | null, ctx: NavContext): NavResult {
  const repo = path ? findRepo(ctx, path) : undefined;
  if (!repo) return unhandled(state);
  if (ctx.remoteOnly?.has(repo.path)) {
    return result({ ...state, picker: null }, [{ kind: "clone", key: repo.path }]);
  }
  if (repo.preselect.kind === "noPicker") {
    return result({ ...state, picker: null }, [
      { kind: "launch", path: repo.path, mode: newAgentMode(repo) },
    ]);
  }
  return result({ ...state, picker: openPicker(repo) });
}

function accept(state: NavState, picker: PickerState, index: number): NavResult {
  const option = picker.options[index];
  if (!option) return unhandled(state);
  return result({ ...state, picker: null }, [
    { kind: "launch", path: picker.path, mode: optionMode(option) },
  ]);
}

function pickerKey(state: NavState, picker: PickerState, key: string): NavResult {
  const last = picker.options.length - 1;
  const move = (index: number) =>
    result({ ...state, picker: { ...picker, index: Math.max(0, Math.min(last, index)) } });
  switch (key) {
    case "ArrowUp":
      return move(picker.index - 1);
    case "ArrowDown":
      return move(picker.index + 1);
    case "Home":
      return move(0);
    case "End":
      return move(last);
    case "Enter":
      return accept(state, picker, picker.index);
    case "Escape":
      return result({ ...state, picker: null });
    default:
      return unhandled(state);
  }
}

/**
 * Alt+R on the zoomed repo: Refactor, or Go to the refactor once it has one. Not on
 * remote-only repos, nor while Refactor can't start (detached HEAD, no commits).
 */
function refactorKey(state: NavState, ctx: NavContext): NavResult {
  const repo = state.zoomed ? ctx.repos.get(state.zoomed) : undefined;
  if (!repo || state.picker || ctx.remoteOnly?.has(repo.path)) return unhandled(state);
  if (repo.refactor) return result(state, [{ kind: "openRefactor", path: repo.path }]);
  if (refactorBlocker(repo)) return unhandled(state);
  return result(state, [{ kind: "refactorDialog", path: repo.path }]);
}

function onKey(state: NavState, k: NavKey, ctx: NavContext): NavResult {
  // By physical key, so Alt+R works on non-Latin layouts too.
  const isR = k.code ? k.code === "KeyR" : k.key.toLowerCase() === "r";
  if (k.alt && !k.ctrl && !k.meta && isR) return refactorKey(state, ctx);
  if (k.ctrl || k.meta || k.alt) return unhandled(state);
  if (state.picker) return pickerKey(state, state.picker, k.key);

  const dir = DIRECTIONS[k.key];
  if (dir) {
    const next = step(state, dir, ctx);
    return result({ ...state, focused: next, zoomed: state.zoomed ? next : null });
  }
  switch (k.key) {
    case "Enter":
      if (state.zoomed) return launch(state, state.zoomed, ctx);
      if (state.focused) return result({ ...state, zoomed: state.focused });
      return unhandled(state);
    case "Escape":
      if (state.zoomed) return result({ ...state, zoomed: null, focused: state.zoomed });
      if (state.filter) return result({ ...state, filter: "" });
      return unhandled(state);
    case "Backspace":
      if (!state.filter) return unhandled(state);
      return result(withFilter(state, state.filter.slice(0, -1), ctx));
  }
  // Printable characters other than space type into the filter.
  if ([...k.key].length === 1 && k.key.trim() !== "") {
    return result(withFilter(state, state.filter + k.key, ctx));
  }
  return unhandled(state);
}

/** The local repo cloned from a remote key's repo, if one is listed. */
function localClone(key: string, ctx: NavContext): string | null {
  for (const r of ctx.repos.values()) {
    if (r.remote && remoteKey(r.remote) === key && !ctx.remoteOnly?.has(r.path)) return r.path;
  }
  return null;
}

/**
 * Forgets focus, zoom and picker for repos that are gone; same object when nothing is.
 * A remote-only gem that went away because its clone appeared hands them to the clone.
 */
function sync(state: NavState, ctx: NavContext): NavState {
  const follow = state.follow && ctx.repos.has(state.follow.to) ? state.follow : null;
  const keep = (p: string | null) => {
    if (follow && p === follow.from) return follow.to;
    if (!p || ctx.repos.has(p)) return p;
    return isRemoteKey(p) ? localClone(p, ctx) : null;
  };
  const focused = keep(state.focused);
  const zoomed = keep(state.zoomed);
  const picker = state.picker && findRepo(ctx, state.picker.path) ? state.picker : null;
  // A pending follow lasts while its gem is still focused or zoomed and has not arrived.
  const waiting =
    !follow && state.follow && (focused === state.follow.from || zoomed === state.follow.from)
      ? state.follow
      : null;
  if (
    focused === state.focused &&
    zoomed === state.zoomed &&
    picker === state.picker &&
    waiting === state.follow
  ) {
    return state;
  }
  return { ...state, focused, zoomed, picker, follow: waiting };
}

export function navigate(current: NavState, input: NavInput, ctx: NavContext): NavResult {
  const state = sync(current, ctx);
  switch (input.type) {
    case "sync":
      return result(state);
    case "rekey":
      if (state.focused !== input.from && state.zoomed !== input.from) return unhandled(state);
      return result(sync({ ...state, follow: { from: input.from, to: input.to } }, ctx));
    case "pointerFocus":
      return result(state.focused === input.path ? state : { ...state, focused: input.path });
    case "select":
      if (!ctx.repos.has(input.path)) return unhandled(state);
      return result({ ...state, focused: input.path, zoomed: input.path, picker: null });
    case "backgroundClick":
      if (state.picker) return result({ ...state, picker: null });
      if (state.zoomed) return result({ ...state, zoomed: null });
      return unhandled(state);
    case "key":
      return onKey(state, input.key, ctx);
    case "launch":
      return launch(state, input.path ?? state.zoomed, ctx);
    case "pickerMove":
      if (!state.picker || !state.picker.options[input.index]) return unhandled(state);
      return result({ ...state, picker: { ...state.picker, index: input.index } });
    case "pickerAccept":
      if (!state.picker) return unhandled(state);
      return accept(state, state.picker, input.index ?? state.picker.index);
    case "pickerClose":
      return state.picker ? result({ ...state, picker: null }) : unhandled(state);
    case "clearFilter":
      return result(state.filter ? { ...state, filter: "" } : state);
  }
}
