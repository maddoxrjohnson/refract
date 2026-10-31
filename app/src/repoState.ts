import type { AgentEvent, Preselect, RepoList, RepoSnapshot, SessionInfo } from "./backend/types";

/**
 * The repos the interface shows, built from `listRepos` answers and helper events.
 * Snapshots carry no version, so an event that arrives while a `listRepos` request is
 * in flight is taken as newer than that answer.
 */
export interface RepoState {
  repos: Map<string, RepoSnapshot>;
  loading: boolean;
  /** `loadComplete` arrived since the current fetch started. */
  loadComplete: boolean;
  /** What events did to each path since the current fetch started; `null` is removed. */
  touched: Map<string, RepoSnapshot | null>;
  /**
   * `sessionChanged` events since the current fetch started, for paths not in `touched`.
   * Only the sessions and preselect are newer than the answer; the rest of the repo is
   * taken from it.
   */
  sessionPatches: Map<string, SessionPatch>;
}

export interface SessionPatch {
  sessions: Map<string, SessionInfo>;
  preselect: Preselect;
}

export type RepoAction =
  /** A `listRepos` request was sent. */
  | { type: "fetchStarted" }
  /** Its answer arrived. */
  | { type: "listed"; list: RepoList }
  | { type: "event"; event: AgentEvent };

export const initialRepoState: RepoState = {
  repos: new Map(),
  loading: false,
  loadComplete: false,
  touched: new Map(),
  sessionPatches: new Map(),
};

/** `repo` with the patch's sessions swapped in by id, or `repo` itself when none is there. */
function patched(repo: RepoSnapshot, patch: SessionPatch): RepoSnapshot {
  if (!repo.sessions.some((s) => patch.sessions.has(s.id))) return repo;
  return {
    ...repo,
    sessions: repo.sessions.map((s) => patch.sessions.get(s.id) ?? s),
    preselect: patch.preselect,
  };
}

export function repoReducer(state: RepoState, action: RepoAction): RepoState {
  switch (action.type) {
    case "fetchStarted":
      return { ...state, loadComplete: false, touched: new Map(), sessionPatches: new Map() };
    case "listed": {
      const repos = new Map(action.list.repos.map((r) => [r.path, r]));
      for (const [path, snapshot] of state.touched) {
        if (snapshot) repos.set(path, snapshot);
        else repos.delete(path);
      }
      for (const [path, patch] of state.sessionPatches) {
        const repo = repos.get(path);
        if (repo) repos.set(path, patched(repo, patch));
      }
      return { ...state, repos, loading: action.list.loading && !state.loadComplete };
    }
    case "event": {
      const { event } = action;
      switch (event.event) {
        case "repoChanged": {
          const sessionPatches = new Map(state.sessionPatches);
          sessionPatches.delete(event.data.path);
          return {
            ...state,
            repos: new Map(state.repos).set(event.data.path, event.data),
            touched: new Map(state.touched).set(event.data.path, event.data),
            sessionPatches,
          };
        }
        case "repoRemoved": {
          const repos = new Map(state.repos);
          repos.delete(event.data.path);
          const sessionPatches = new Map(state.sessionPatches);
          sessionPatches.delete(event.data.path);
          return {
            ...state,
            repos,
            touched: new Map(state.touched).set(event.data.path, null),
            sessionPatches,
          };
        }
        case "loadComplete":
          return { ...state, loading: false, loadComplete: true };
        case "sessionChanged": {
          // Sent instead of `repoChanged` when only sessions' states moved (the status
          // hook's working and waiting among them); the session list itself is unchanged.
          const { repoPath, session, preselect } = event.data;
          const one: SessionPatch = { sessions: new Map([[session.id, session]]), preselect };
          const repos = new Map(state.repos);
          const shown = repos.get(repoPath);
          if (shown) repos.set(repoPath, patched(shown, one));
          // A repo snapshot from this fetch's events is newer than the answer, so the
          // change goes onto it; otherwise it is kept to go onto the answer.
          const touched = state.touched.get(repoPath);
          if (touched !== undefined) {
            if (touched === null) return { ...state, repos };
            return {
              ...state,
              repos,
              touched: new Map(state.touched).set(repoPath, patched(touched, one)),
            };
          }
          const earlier = state.sessionPatches.get(repoPath);
          const patch: SessionPatch = {
            sessions: new Map(earlier?.sessions).set(session.id, session),
            preselect,
          };
          return {
            ...state,
            repos,
            sessionPatches: new Map(state.sessionPatches).set(repoPath, patch),
          };
        }
        case "migrateProgress":
        case "migrateReady":
          return state;
      }
    }
  }
}
