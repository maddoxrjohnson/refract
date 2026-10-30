import { useCallback, useEffect, useMemo, useReducer, useRef } from "react";
import type { Backend } from "../backend/Backend";
import type { DeleteOutcome, FinishAction, MigrateError } from "../backend/types";
import { initialMigrations, migrationsReducer, type MigrationsState } from "../migrations";

/** Move to WSL's state and commands, shared by the gem marks, the panel and the dialog. */
export interface MigrationControls {
  /** By source path. */
  state: MigrationsState;
  /** Takes the snapshot and starts the copy; rejects with the backend's `MigrateError`. */
  start(source: string, dest: string): Promise<void>;
  /** Stops a snapshot or a running copy of `source`; the original is untouched. */
  cancel(source: string): Promise<void>;
  /** Shown as running from the moment it is asked for; back to the report if it fails. */
  resync(id: string): Promise<void>;
  /**
   * Delete original; the entry is dropped once it says `deleted`, and marked deleting once
   * it says the delete has begun.
   */
  remove(id: string, permanent: boolean): Promise<DeleteOutcome>;
  /** Keep both or discard the copy; the entry is dropped once it answers. */
  finish(id: string, action: FinishAction): Promise<void>;
}

/**
 * Follows migrations through the helper's events, and asks for pending reports whenever
 * `refresh` changes while `enabled` (the helper is running): after a restart, and on each
 * show, since WebView2 is suspended while hidden and events can be lost then. While the
 * helper is not running, no copy runs, so none is shown as running.
 */
export function useMigrations(backend: Backend, enabled: boolean, refresh: number): MigrationControls {
  const [state, dispatch] = useReducer(migrationsReducer, initialMigrations);
  const stateRef = useRef(state);
  stateRef.current = state;

  useEffect(
    () =>
      backend.onAgentEvent((event) => {
        if (event.event === "migrateProgress") dispatch({ type: "progress", progress: event.data });
        else if (event.event === "migrateReady") dispatch({ type: "ready", ...event.data });
      }),
    [backend],
  );
  useEffect(() => backend.onMigrateSnapshot((progress) => dispatch({ type: "snapshot", progress })), [backend]);

  useEffect(() => {
    // A stopped helper takes its running copies with it; the pending list brings back
    // the reports it kept once it runs again.
    if (!enabled) {
      dispatch({ type: "helperStopped" });
      return;
    }
    let live = true;
    backend.migratePending().then(
      (list) => live && dispatch({ type: "pending", list }),
      (err: unknown) => console.warn("migratePending failed", err),
    );
    return () => {
      live = false;
    };
  }, [backend, enabled, refresh]);

  const start = useCallback(
    async (source: string, dest: string) => {
      dispatch({ type: "starting", source, dest });
      try {
        const { id } = await backend.migrateStart(source, dest);
        dispatch({ type: "started", source, id });
      } catch (err) {
        dispatch({ type: "gone", source });
        throw err;
      }
    },
    [backend],
  );

  const cancel = useCallback(
    async (source: string) => {
      const m = stateRef.current.get(source);
      if (!m) return;
      if (m.id === null) {
        // migrateStart then fails with `cancelled` and drops the entry.
        await backend.migrateCancelSnapshot(source);
        return;
      }
      await backend.migrateCancel(m.id);
      dispatch({ type: "gone", source });
    },
    [backend],
  );

  const resync = useCallback(
    async (id: string) => {
      const before = [...stateRef.current.values()].find((m) => m.id === id)?.phase;
      dispatch({ type: "resyncing", id });
      try {
        await backend.migrateResync(id);
      } catch (err) {
        if (before?.kind === "ready") dispatch({ type: "ready", id, report: before.report });
        throw err;
      }
    },
    [backend],
  );

  const sourceOf = (id: string) => [...stateRef.current.values()].find((m) => m.id === id)?.source;

  const remove = useCallback(
    async (id: string, permanent: boolean) => {
      const outcome = await backend.migrateDelete(id, permanent);
      const source = sourceOf(id);
      if (outcome.kind === "deleted") {
        if (source) dispatch({ type: "gone", source });
      } else if (outcome.kind === "unfinished" || outcome.deleting) {
        dispatch({ type: "deleting", id });
      }
      return outcome;
    },
    [backend],
  );

  const finish = useCallback(
    async (id: string, action: FinishAction) => {
      await backend.migrateFinish(id, action);
      const source = sourceOf(id);
      if (source) dispatch({ type: "gone", source });
    },
    [backend],
  );

  return useMemo(
    () => ({ state, start, cancel, resync, remove, finish }),
    [state, start, cancel, resync, remove, finish],
  );
}

/** Whether a backend error is the `cancelled` a stopped preflight or snapshot fails with. */
export function isCancelled(err: unknown): boolean {
  return (err as Partial<MigrateError> | null)?.kind === "cancelled";
}
