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

/**
 * Fetches details whenever `ref` names a different repo; `refresh` bypasses the backend's
 * cache. Answers for a repo that is no longer asked about are dropped.
 */
export function useRemoteDetails(backend: Backend, ref: RemoteRef | null) {
  const key = ref ? remoteKey(ref) : null;
  const [entry, setEntry] = useState<{ key: string; state: RemoteDetailsState } | null>(null);
  const latest = useRef(ref);
  latest.current = ref;
  const seq = useRef(0);

  const load = useCallback(
    (refresh: boolean) => {
      const r = latest.current;
      if (!r) return;
      const k = remoteKey(r);
      const id = ++seq.current;
      const kept = (prev: typeof entry) => (prev?.key === k ? prev.state.details : null);
      setEntry((prev) => ({ key: k, state: { status: "loading", details: kept(prev) } }));
      backend.remoteDetails({ host: r.host, owner: r.owner, name: r.name }, refresh).then(
        (details) => {
          if (id === seq.current) setEntry({ key: k, state: { status: "ready", details } });
        },
        (err: unknown) => {
          if (id !== seq.current) return;
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
    };
  }, [key, load]);

  const refresh = useCallback(() => load(true), [load]);
  const state: RemoteDetailsState | null = !key ? null : entry?.key === key ? entry.state : LOADING;
  return { state, refresh };
}
