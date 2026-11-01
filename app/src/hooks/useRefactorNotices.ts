import { useCallback, useEffect, useRef, useState } from "react";
import type { RepoSnapshot } from "../backend/types";
import { trackRefactors, type RefactorNotice, type RefactorSeen } from "../refactors";

/**
 * How long after the window shows changes stay quiet. The app toasted them from Windows
 * while the window was hidden (WebView2 is suspended then), and the refetch on show would
 * otherwise report them again.
 */
export const QUIET_AFTER_SHOW_MS = 5000;
/** How long a notice stays. */
export const NOTICE_MS = 8000;

/**
 * The in-app toast for refactors (refactor spec §4): the plan ready for the first time,
 * or the refactor ready, in a repo other than the zoomed one, while the window shows.
 * Each transition is told once; the zoomed repo's panel shows its own.
 */
export function useRefactorNotices(
  repos: ReadonlyMap<string, RepoSnapshot>,
  visible: boolean,
  zoomed: string | null,
) {
  const seen = useRef(new Map<string, RefactorSeen>());
  const shownAt = useRef<number | null>(null);
  const zoomedRef = useRef(zoomed);
  zoomedRef.current = zoomed;
  const [notice, setNotice] = useState<RefactorNotice | null>(null);

  // Declared before the tracking effect, so a show and a change in one render count the change as early.
  useEffect(() => {
    shownAt.current = visible ? Date.now() : null;
  }, [visible]);

  useEffect(() => {
    const alerts = trackRefactors(seen.current, repos.values());
    const at = shownAt.current;
    if (at === null || Date.now() - at < QUIET_AFTER_SHOW_MS) return;
    const told = alerts.filter((a) => a.path !== zoomedRef.current);
    if (told.length > 0) setNotice(told[told.length - 1]);
  }, [repos]);

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
