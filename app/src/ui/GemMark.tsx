import { canDelete, overallProgress, type Migration } from "../migrations";
import "./ui.css";

/** The tooltip line for a repo on a Windows drive (spec §2.1). */
export const WINDOWS_DRIVE_TEXT = "On a Windows drive: git and agents are slower here.";

export interface GemMarkProps {
  /** The repo is on a Windows drive: a small marker at the gem's upper left. */
  windows: boolean;
  /** A Move to WSL of the repo: a ring around the gem, full once its report waits. */
  migration?: Migration;
  /**
   * Linked worktrees folded into the gem: their count at the gem's lower right, with a
   * dot when one of them can be removed.
   */
  checkouts?: { count: number; removable: boolean };
}

/**
 * What the scene draws over a gem (its `marks`), sized from `--r`, the gem's radius in
 * pixels. Static: it changes only when the migration moves on or the worktrees change. The
 * focus label and the detail panel say the same in words, so it is hidden from assistive
 * technology.
 */
export function GemMark({ windows, migration, checkouts }: GemMarkProps) {
  const progress = migration ? overallProgress(migration.phase) : 0;
  const tone =
    migration?.phase.kind === "ready" ? (canDelete(migration.phase.report) ? "ready" : "failed") : "running";
  return (
    <div className="rf-gem-mark" aria-hidden="true">
      {migration && (
        <svg className={`rf-move-ring rf-move-ring--${tone}`} viewBox="0 0 100 100" data-progress={progress.toFixed(2)}>
          <circle className="rf-move-ring__track" cx="50" cy="50" r="48" />
          <circle
            className="rf-move-ring__fill"
            cx="50"
            cy="50"
            r="48"
            pathLength="100"
            strokeDasharray={`${(Math.max(0.02, progress) * 100).toFixed(1)} 100`}
          />
        </svg>
      )}
      {windows && (
        <svg className="rf-drive-mark" viewBox="0 0 12 12" width="12" height="12">
          <rect x="1" y="1" width="4.4" height="4.4" rx="0.8" />
          <rect x="6.6" y="1" width="4.4" height="4.4" rx="0.8" />
          <rect x="1" y="6.6" width="4.4" height="4.4" rx="0.8" />
          <rect x="6.6" y="6.6" width="4.4" height="4.4" rx="0.8" />
        </svg>
      )}
      {checkouts && checkouts.count > 0 && (
        <span
          className={`rf-checkout-mark${checkouts.removable ? " rf-checkout-mark--removable" : ""}`}
          data-count={checkouts.count}
        >
          <svg viewBox="0 0 12 12" width="10" height="10">
            <circle cx="3" cy="2.6" r="1.4" />
            <circle cx="3" cy="9.4" r="1.4" />
            <circle cx="9" cy="4.2" r="1.4" />
            <path d="M3 4v4M9 5.6c0 2-2.4 2.2-5.4 3" />
          </svg>
          {checkouts.count}
        </span>
      )}
    </div>
  );
}
