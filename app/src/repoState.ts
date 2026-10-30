import type { AgentEvent, RepoList, RepoSnapshot } from "./backend/types";

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
};

export function repoReducer(state: RepoState, action: RepoAction): RepoState {
  switch (action.type) {
    case "fetchStarted":
      return { ...state, loadComplete: false, touched: new Map() };
    case "listed": {
      const repos = new Map(action.list.repos.map((r) => [r.path, r]));
      for (const [path, snapshot] of state.touched) {
        if (snapshot) repos.set(path, snapshot);
        else repos.delete(path);
      }
      return { ...state, repos, loading: action.list.loading && !state.loadComplete };
    }
    case "event": {
      const { event } = action;
      switch (event.event) {
        case "repoChanged":
          return {
            ...state,
            repos: new Map(state.repos).set(event.data.path, event.data),
            touched: new Map(state.touched).set(event.data.path, event.data),
          };
        case "repoRemoved": {
          const repos = new Map(state.repos);
          repos.delete(event.data.path);
          return {
            ...state,
            repos,
            touched: new Map(state.touched).set(event.data.path, null),
          };
        }
        case "loadComplete":
          return { ...state, loading: false, loadComplete: true };
        case "sessionChanged":
          return state;
      }
    }
  }
}
