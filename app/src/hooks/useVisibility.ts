import { useEffect, useState } from "react";
import type { Backend } from "../backend/Backend";

/**
 * Whether the window is shown. Starts `false` until the backend answers, so nothing
 * draws while hidden; the scene uses this to switch its frame loop off.
 */
export function useVisibility(backend: Backend): boolean {
  const [visible, setVisible] = useState(false);
  useEffect(() => {
    let live = true;
    // An event that arrives before the initial query answers is newer, so it wins.
    let heard = false;
    const stop = backend.onVisibility((v) => {
      heard = true;
      if (live) setVisible(v);
    });
    backend.isVisible().then(
      (v) => {
        if (live && !heard) setVisible(v);
      },
      (err: unknown) => console.error("isVisible failed", err),
    );
    return () => {
      live = false;
      stop();
    };
  }, [backend]);
  return visible;
}
