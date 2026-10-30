import { useCallback, useEffect, useState } from "react";
import type { Backend } from "../backend/Backend";
import type { HookInfo } from "../backend/types";
import { describeError } from "../ui/StatusToast";

/** The status hook's state, shared by the first-run offer and the settings section. */
export interface HookControls {
  /** `null` until the helper answers. */
  info: HookInfo | null;
  /** Why asking the helper failed, or `null`. */
  error: string | null;
  /** Rejects with the backend's error; `info` updates when it works. */
  install(): Promise<void>;
  remove(): Promise<void>;
}

/**
 * Asks the helper about the hook while `enabled` (the helper is running), again
 * whenever `refresh` changes (each show), since the file can change outside Refract.
 */
export function useHook(backend: Backend, enabled: boolean, refresh: number): HookControls {
  const [info, setInfo] = useState<HookInfo | null>(null);
  const [error, setError] = useState<string | null>(null);

  useEffect(() => {
    if (!enabled) return;
    let live = true;
    backend.hookInfo().then(
      (next) => {
        if (!live) return;
        setInfo(next);
        setError(null);
      },
      (err: unknown) => live && setError(describeError(err)),
    );
    return () => {
      live = false;
    };
  }, [backend, enabled, refresh]);

  const install = useCallback(async () => {
    setInfo(await backend.installHook());
    setError(null);
  }, [backend]);
  const remove = useCallback(async () => {
    setInfo(await backend.uninstallHook());
    setError(null);
  }, [backend]);

  return { info, error, install, remove };
}
