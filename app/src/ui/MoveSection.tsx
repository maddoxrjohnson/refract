import { useState } from "react";
import { canDelete, overallProgress, stepText, type Migration } from "../migrations";
import { WINDOWS_DRIVE_TEXT } from "./GemMark";
import { describeError } from "./StatusToast";
import "./ui.css";

export interface MoveSectionProps {
  /** The repo's migration, while one runs or its report waits. */
  migration?: Migration;
  /** Opens the dialog: the preflight, the progress or the report. */
  onOpen(): void;
  /** Stops the snapshot or the copy; the original is untouched. */
  onCancel(): Promise<void>;
}

/** The detail panel's Move to WSL section (spec §2.1, §2.3). */
export function MoveSection({ migration, onOpen, onCancel }: MoveSectionProps) {
  const [cancelling, setCancelling] = useState(false);
  const [error, setError] = useState<string | null>(null);
  const phase = migration?.phase;

  const cancel = () => {
    setCancelling(true);
    setError(null);
    onCancel()
      .catch((err: unknown) => setError(describeError(err)))
      .finally(() => setCancelling(false));
  };

  return (
    <section className="rf-section rf-move-section" aria-label="Move to WSL">
      <h3 className="rf-section__label">Move to WSL</h3>
      {!phase ? (
        <>
          <p className="rf-move-section__line rf-muted">{WINDOWS_DRIVE_TEXT}</p>
          <button type="button" className="rf-button" aria-haspopup="dialog" onClick={onOpen}>
            Move to WSL…
          </button>
        </>
      ) : phase.kind === "ready" ? (
        <>
          <p className={`rf-move-section__line${canDelete(phase.report) ? "" : " rf-move-section__line--failed"}`}>
            {canDelete(phase.report)
              ? "The copy passed every check and waits for your OK."
              : "A check failed. Nothing was deleted."}
          </p>
          <button type="button" className="rf-button rf-button--accent" aria-haspopup="dialog" onClick={onOpen}>
            Review…
          </button>
        </>
      ) : (
        <>
          <div className="rf-move-section__step" aria-live="polite">
            {stepText(phase)}
          </div>
          <div
            className="rf-move-bar"
            role="progressbar"
            aria-label="Move to WSL"
            aria-valuemin={0}
            aria-valuemax={100}
            aria-valuenow={Math.round(overallProgress(phase) * 100)}
          >
            <div className="rf-move-bar__fill" style={{ width: `${(overallProgress(phase) * 100).toFixed(1)}%` }} />
          </div>
          <div className="rf-row">
            <button type="button" className="rf-button" aria-haspopup="dialog" onClick={onOpen}>
              Details
            </button>
            <button type="button" className="rf-button" onClick={cancel} disabled={cancelling}>
              {cancelling ? "Cancelling…" : "Cancel"}
            </button>
          </div>
        </>
      )}
      {error && (
        <p className="rf-error" role="alert">
          {error}
        </p>
      )}
    </section>
  );
}
