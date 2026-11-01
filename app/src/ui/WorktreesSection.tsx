import { useEffect, useRef, useState } from "react";
import type { RepoSnapshot, SessionInfo } from "../backend/types";
import { ignoredList, worktreeView } from "../worktrees";
import { firstLine, relativeTime } from "./format";
import { describeError } from "./StatusToast";
import "./ui.css";

export interface WorktreesSectionProps {
  /** The linked worktrees folded into the zoomed gem. */
  worktrees: readonly RepoSnapshot[];
  /** Current time in milliseconds, for relative times. */
  now: number;
  /** What Enter does on a repo at this path: launch, or open the picker. */
  onOpen(path: string): void;
  /** Rejects with the helper's reason. */
  onRemove(path: string): Promise<void>;
}

/** A removal that outlasted the wait; the helper carries on. */
function isPending(err: unknown): boolean {
  return typeof err === "object" && err !== null && (err as { kind?: unknown }).kind === "pending";
}

/** The newest session in the folder that said something. */
function lastMessage(repo: RepoSnapshot): SessionInfo | null {
  let best: SessionInfo | null = null;
  for (const s of repo.sessions) {
    if (!s.lastAssistantText?.trim()) continue;
    if (!best || (s.lastTimestamp ?? "") > (best.lastTimestamp ?? "")) best = s;
  }
  return best;
}

function WorktreeRow({ repo, now, onOpen, onRemove }: { repo: RepoSnapshot } & Omit<WorktreesSectionProps, "worktrees">) {
  const [confirming, setConfirming] = useState(false);
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState<{ text: string; pending: boolean } | null>(null);
  // Opening or closing the confirm unmounts the focused button, so focus moves to Keep it
  // on open and back to Remove… on Keep it.
  const keepRef = useRef<HTMLButtonElement>(null);
  const removeRef = useRef<HTMLButtonElement>(null);
  const focusNext = useRef<"keep" | "remove" | null>(null);
  useEffect(() => {
    const target = focusNext.current === "keep" ? keepRef.current : focusNext.current === "remove" ? removeRef.current : null;
    focusNext.current = null;
    target?.focus();
  }, [confirming]);
  const view = worktreeView(repo);
  const said = lastMessage(repo);
  const line = said ? firstLine(said.lastAssistantText) : null;
  const ignored = ignoredList(repo.worktree?.ignored ?? []);
  const removable = view?.removable ?? false;

  const remove = () => {
    setBusy(true);
    setError(null);
    onRemove(repo.path).then(
      // On success the row goes away with the worktree's `repoRemoved`.
      () => {},
      (err: unknown) => {
        const pending = isPending(err);
        setError({ text: describeError(err), pending });
        if (!pending) setBusy(false);
      },
    );
  };

  return (
    <li className="rf-worktree" aria-label={repo.name}>
      <div className="rf-worktree__head">
        <span className="rf-worktree__name">{repo.name}</span>
        <span className="rf-mono rf-muted rf-worktree__branch">{repo.branch ?? "(detached)"}</span>
      </div>
      {view && (
        <p className={`rf-worktree__status rf-worktree__status--${view.tone}`}>
          {view.label}
          {view.removable && <span className="rf-worktree__ready"> · Ready to remove</span>}
        </p>
      )}
      {view?.agentOpen && <p className="rf-hint">Close the agent here to remove</p>}
      {line && said && (
        <p className="rf-worktree__said">
          <span className="rf-worktree__quote" title={said.lastAssistantText ?? undefined}>
            {line}
          </span>
          {said.lastTimestamp && (
            <span className="rf-muted">· {relativeTime(Date.parse(said.lastTimestamp), now)}</span>
          )}
        </p>
      )}
      {confirming && removable ? (
        <div className="rf-refactor__confirm" role="group" aria-label={`Confirm removing ${repo.name}`}>
          <p className="rf-refactor__question">
            Remove the folder <span className="rf-mono">{repo.path}</span>?
            {repo.branch && (
              <>
                {" "}
                The branch <span className="rf-mono">{repo.branch}</span> stays.
              </>
            )}
          </p>
          {ignored.shown.length > 0 && (
            <>
              <p className="rf-hint">Also deletes ignored files:</p>
              <ul className="rf-refactor__files rf-mono" aria-label="Ignored files">
                {ignored.shown.map((f) => (
                  <li key={f}>{f}</li>
                ))}
                {ignored.more && <li className="rf-muted">and more</li>}
              </ul>
            </>
          )}
          <div className="rf-row">
            <button type="button" className="rf-button rf-button--danger" disabled={busy} onClick={remove}>
              {busy ? "Removing…" : "Remove"}
            </button>
            <button
              ref={keepRef}
              type="button"
              className="rf-button"
              disabled={busy}
              onClick={() => {
                focusNext.current = "remove";
                setConfirming(false);
                setError(null);
              }}
            >
              Keep it
            </button>
          </div>
        </div>
      ) : (
        <div className="rf-row">
          <button type="button" className="rf-button" aria-label={`Open ${repo.name}`} onClick={() => onOpen(repo.path)}>
            Open
          </button>
          {removable && (
            <button
              ref={removeRef}
              type="button"
              className="rf-button"
              aria-label={`Remove ${repo.name}…`}
              onClick={() => {
                focusNext.current = "keep";
                setError(null);
                setConfirming(true);
              }}
            >
              Remove…
            </button>
          )}
        </div>
      )}
      {error &&
        (error.pending ? (
          <p className="rf-hint" role="status">
            {error.text}
          </p>
        ) : (
          <p className="rf-error" role="alert">
            {error.text}
          </p>
        ))}
    </li>
  );
}

/**
 * The detail panel's Worktrees section (worktrees spec §1): one row per linked worktree
 * folded into the gem, with its status, the last thing its agent said, Open and, when
 * nothing can be lost, Remove.
 */
export function WorktreesSection({ worktrees, now, onOpen, onRemove }: WorktreesSectionProps) {
  if (worktrees.length === 0) return null;
  return (
    <section className="rf-section rf-worktrees" aria-label="Worktrees">
      <h3 className="rf-section__label">Worktrees</h3>
      <ul className="rf-worktrees__list">
        {worktrees.map((w) => (
          <WorktreeRow key={w.path} repo={w} now={now} onOpen={onOpen} onRemove={onRemove} />
        ))}
      </ul>
    </section>
  );
}
