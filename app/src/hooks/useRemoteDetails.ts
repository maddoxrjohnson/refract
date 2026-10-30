import { useCallback, useEffect, useRef, useState } from "react";
import type { Backend } from "../backend/Backend";
import type { RemoteDetails, RemoteRef } from "../backend/types";
import { remoteKey } from "../projects";
import { describeError } from "../ui/StatusToast";

/** Pull or merge requests and CI for the zoomed gem; earlier details stay while reloading. */
export type RemoteDetailsState =
  | { status: "loading"; details: RemoteDetails | null }
  | { status: "ready"; details: RemoteDetails }
  | { status: "error"; message: string; details: RemoteDetails | null };

const LOADING: RemoteDetailsState = { status: "loading", details: null };

/** How often details are fetched again while the same gem stays zoomed and the window is shown. */
export const DETAILS_REFRESH_MS = 15 * 60_000;
/** The timed fetch never comes sooner than this after the timer is set, however old the data. */
export const DETAILS_MIN_DELAY_MS = 1_000;

/**
 * Fetches details whenever `ref` names a different repo; `refresh` bypasses the backend's
 * cache. Answers for a repo that is no longer asked about are dropped. While the same repo
 * stays asked about and the window is `visible`, details are fetched again 15 minutes
 * after the shown ones were fetched, quietly: no loading state, and a failure keeps what
 * is shown. A timed fetch never starts while another request is still out. A window shown
 * again after that long fetches at once.
 */
export function useRemoteDetails(backend: Backend, ref: RemoteRef | null, visible = true) {
  const key = ref ? remoteKey(ref) : null;
  const [entry, setEntry] = useState<{ key: string; state: RemoteDetailsState } | null>(null);
  const latest = useRef(ref);
  latest.current = ref;
  const seq = useRef(0);
  /** The request whose answer will be used, or `null` when none is out. */
  const pending = useRef<number | null>(null);
  /** When the last fetch started (Date.now()). */
  const startedAt = useRef(0);

  const load = useCallback(
    (refresh: boolean, quiet = false) => {
      const r = latest.current;
      if (!r) return;
      // A timed fetch must not take the place of a request the person is waiting on.
      if (quiet && pending.current !== null) return;
      const k = remoteKey(r);
      const id = ++seq.current;
      pending.current = id;
      startedAt.current = Date.now();
      const kept = (prev: typeof entry) => (prev?.key === k ? prev.state.details : null);
      if (!quiet) setEntry((prev) => ({ key: k, state: { status: "loading", details: kept(prev) } }));
      backend.remoteDetails({ host: r.host, owner: r.owner, name: r.name }, refresh).then(
        (details) => {
          if (id !== seq.current) return;
          pending.current = null;
          setEntry({ key: k, state: { status: "ready", details } });
        },
        (err: unknown) => {
          if (id !== seq.current) return;
          pending.current = null;
          if (quiet) return;
          setEntry((prev) => ({
            key: k,
            state: { status: "error", message: describeError(err), details: kept(prev) },
          }));
        },
      );
    },
    [backend],
  );

  useEffect(() => {
    if (key) load(false);
    return () => {
      seq.current++;
      pending.current = null;
    };
  }, [key, load]);

  const refresh = useCallback(() => load(true), [load]);
  const state: RemoteDetailsState | null = !key ? null : entry?.key === key ? entry.state : LOADING;

  // The timer follows the age of the details shown (a manual Refresh or an answer cached
  // for up to 15 minutes included); before there are any, the start of the fetch.
  const shownAt = state?.details ? Date.parse(state.details.fetchedAt) : NaN;
  // The backend caches for 15 minutes too, so the timed fetch skips its cache.
  useEffect(() => {
    if (!key || !visible) return;
    let timer: ReturnType<typeof setTimeout>;
    const schedule = (ms: number) => {
      timer = setTimeout(() => {
        load(true, true);
        schedule(DETAILS_REFRESH_MS);
      }, ms);
    };
    const from = Number.isFinite(shownAt) ? shownAt : startedAt.current;
    const due = from + DETAILS_REFRESH_MS - Date.now();
    schedule(Math.min(DETAILS_REFRESH_MS, Math.max(DETAILS_MIN_DELAY_MS, due)));
    return () => clearTimeout(timer);
  }, [key, visible, load, shownAt]);

  return { state, refresh };
}
