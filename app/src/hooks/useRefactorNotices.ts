import { useCallback, useEffect, useRef, useState } from "react";
import type { Backend } from "../backend/Backend";
import type { RefactorNotice } from "../backend/types";

/** How long a notice stays. */
export const NOTICE_MS = 8000;

/**
 * The in-app toast for refactors (refactor spec §4). The Rust side decides each alert
 * once and sends it here while the window shows and the repo isn't zoomed, so this only
 * tells it the zoomed repo and shows what arrives.
 */
export function useRefactorNotices(backend: Backend, zoomed: string | null) {
  const [notice, setNotice] = useState<RefactorNotice | null>(null);

  // The zoomed repo's panel shows its own changes, even if a notice was already on its way.
  const zoomedRef = useRef(zoomed);
  zoomedRef.current = zoomed;
  useEffect(
    () =>
      backend.onRefactorNotice((n) => {
        if (n.path !== zoomedRef.current) setNotice(n);
      }),
    [backend],
  );

  useEffect(() => {
    backend.setZoomed(zoomed).catch((err: unknown) => console.warn("setZoomed failed", err));
  }, [backend, zoomed]);

  // Zooming to the repo shows it in the panel instead.
  useEffect(() => {
    if (notice && notice.path === zoomed) setNotice(null);
  }, [notice, zoomed]);

  useEffect(() => {
    if (!notice) return;
    const timer = setTimeout(() => setNotice(null), NOTICE_MS);
    return () => clearTimeout(timer);
  }, [notice]);

  const dismiss = useCallback(() => setNotice(null), []);
  return { notice, dismiss };
}
