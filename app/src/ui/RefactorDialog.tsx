import { useEffect, useId, useRef, useState, type FormEvent, type KeyboardEvent } from "react";
import type { RepoSnapshot } from "../backend/types";
import type { HookControls } from "../hooks/useHook";
import { busyNote } from "../refactors";
import { describeError } from "./StatusToast";
import "./ui.css";

export interface RefactorDialogProps {
  repo: RepoSnapshot;
  /** The status hook, for the note that alerts need it. */
  hook: HookControls;
  /** Starts the refactor with the "Focus on…" line, empty for none. */
  onConfirm(focus: string): void;
  /** Another launch or clone is under way: Start waits for it. */
  busy?: boolean;
  onClose(): void;
}

/** Longest "Focus on…" line; the prompt is meant to stay short. */
export const FOCUS_MAX = 200;

/**
 * Confirms a refactor (refactor spec §1, §2): what it leaves alone, an optional focus,
 * and, without the status hook, that its alerts need the hook. Enter starts, Escape
 * cancels; window keys stay with the dialog while it is open.
 */
export function RefactorDialog({ repo, hook, onConfirm, onClose, busy = false }: RefactorDialogProps) {
  const base = useId();
  const titleId = `${base}-title`;
  const noteId = `${base}-note`;
  const [focus, setFocus] = useState("");
  const [installing, setInstalling] = useState(false);
  const [installError, setInstallError] = useState<string | null>(null);
  const input = useRef<HTMLInputElement>(null);
  const note = busyNote(repo);
  const needsHook = hook.info !== null && !hook.info.installed;

  useEffect(() => input.current?.focus(), []);

  const submit = (ev: FormEvent) => {
    ev.preventDefault();
    if (!busy) onConfirm(focus.trim());
  };

  const onKeyDown = (ev: KeyboardEvent<HTMLFormElement>) => {
    ev.stopPropagation();
    if (ev.key === "Escape") {
      ev.preventDefault();
      onClose();
    }
  };

  const install = () => {
    setInstalling(true);
    setInstallError(null);
    hook
      .install()
      .catch((err: unknown) => setInstallError(describeError(err)))
      .finally(() => setInstalling(false));
  };

  return (
    <form
      className="rf-glass rf-refactor-dialog"
      role="dialog"
      aria-labelledby={titleId}
      aria-describedby={note ? noteId : undefined}
      onSubmit={submit}
      onKeyDown={onKeyDown}
    >
      <h2 id={titleId} className="rf-picker__title">
        Refactor <span className="rf-mono">{repo.name}</span>
      </h2>
      <p className="rf-refactor-dialog__text">
        Claude works in a new worktree on its own branch, proposes up to 3 behaviour-preserving refactors for you to
        approve, and commits each one. Nothing is pushed.
      </p>
      {note && (
        <p id={noteId} className="rf-hint">
          {note}
        </p>
      )}
      <label className="rf-field">
        <span>Focus on… (optional)</span>
        <input
          ref={input}
          className="rf-input"
          type="text"
          value={focus}
          maxLength={FOCUS_MAX}
          placeholder="the parser, the sync worker…"
          onChange={(ev) => setFocus(ev.target.value)}
        />
      </label>
      {needsHook && (
        <div className="rf-refactor-dialog__hook">
          <p className="rf-hint">
            Alerts for the plan and the finished refactor need the status hook.
          </p>
          <button type="button" className="rf-button" disabled={installing} onClick={install}>
            {installing ? "Installing…" : hook.info?.outdated ? "Update the hook" : "Install the hook"}
          </button>
          {installError && (
            <p className="rf-error" role="alert">
              {installError}
            </p>
          )}
        </div>
      )}
      <div className="rf-picker__footer">
        <button type="button" className="rf-button rf-button--ghost" onClick={onClose}>
          Cancel <kbd className="rf-kbd">Esc</kbd>
        </button>
        <button
          type="submit"
          className="rf-button rf-button--primary"
          disabled={busy}
          title={busy ? "Another launch is under way." : undefined}
        >
          <span>Start refactor</span>
          <kbd className="rf-kbd" aria-hidden="true">
            Enter
          </kbd>
        </button>
      </div>
    </form>
  );
}
