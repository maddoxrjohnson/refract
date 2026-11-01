import { useEffect, useRef, useState, type ReactNode } from "react";
import type { MergeOutcome, RefactorInfo, RepoSnapshot } from "../backend/types";
import { blockText, CHECK_LABELS, discardQuestion, refactorBlocker } from "../refactors";
import { describeError } from "./StatusToast";
import "./ui.css";

export interface RefactorSectionProps {
  repo: RepoSnapshot;
  /** Opens the Refactor dialog. */
  onStart(): void;
  /** Opens or raises the worktree's VS Code window: Go to the refactor, Review in VS Code. */
  onOpen(): void;
  /** Resume refactor, or with `fix` Fix with Claude after a conflict. Runs as a launch. */
  onResume(fix: boolean): void;
  /** Rejects with the helper's reason. */
  onMerge(): Promise<MergeOutcome>;
  onDiscard(): Promise<void>;
  /** A launch or clone is under way, so Refactor and Resume would wait behind it. */
  launching?: boolean;
}

function plural(n: number, one: string, many: string): string {
  return `${n} ${n === 1 ? one : many}`;
}

/** Why Fix with Claude and Discard wait while the agent runs. */
const LIVE_TITLE = "Claude is still open in the refactor window. Close it first.";

/** A merge or discard that outlasted the wait; the helper carries on. */
function isPending(err: unknown): boolean {
  return typeof err === "object" && err !== null && (err as { kind?: unknown }).kind === "pending";
}

const ALT_R = (
  <kbd className="rf-kbd" aria-hidden="true">
    Alt+R
  </kbd>
);

/** The commits, the diffstat, the checks and the summary of a refactor with results. */
function Results({ info }: { info: RefactorInfo }) {
  return (
    <>
      {info.commits.length > 0 && (
        <ol className="rf-refactor__commits" aria-label="Commits">
          {info.commits.map((c) => (
            <li key={c.sha}>
              <span className="rf-mono rf-muted">{c.sha}</span> {c.summary}
            </li>
          ))}
        </ol>
      )}
      {info.commits.length > 0 && (
        <p className="rf-refactor__line rf-mono">
          {plural(info.filesChanged, "file", "files")} changed,{" "}
          <span aria-label={`${info.insertions} lines added`}>+{info.insertions}</span>{" "}
          <span aria-label={`${info.deletions} lines removed`}>−{info.deletions}</span>
        </p>
      )}
      {info.checks === null ? (
        <p className="rf-refactor__line rf-muted">No check results reported.</p>
      ) : (
        info.checks.length > 0 && (
          <table className="rf-refactor__checks">
            <caption className="rf-visually-hidden">Checks before and after</caption>
            <thead>
              <tr>
                <th scope="col">Check</th>
                <th scope="col">Before</th>
                <th scope="col">After</th>
              </tr>
            </thead>
            <tbody>
              {info.checks.map((c) => (
                <tr key={c.command}>
                  <th scope="row" className="rf-mono">
                    {c.command}
                  </th>
                  <td className={`rf-check-${c.before}`}>{CHECK_LABELS[c.before]}</td>
                  <td className={`rf-check-${c.after}`}>{CHECK_LABELS[c.after]}</td>
                </tr>
              ))}
            </tbody>
          </table>
        )
      )}
      {info.summary && <p className="rf-quote rf-refactor__summary">{info.summary}</p>}
    </>
  );
}

/** The detail panel's Refactor section (refactor spec §1, §4): the button, or the refactor's state and actions. */
export function RefactorSection({ repo, onStart, onOpen, onResume, onMerge, onDiscard, launching = false }: RefactorSectionProps) {
  const info = repo.refactor ?? null;
  const [busy, setBusy] = useState<"merge" | "discard" | null>(null);
  const [confirming, setConfirming] = useState(false);
  // Opening or closing the confirm unmounts the focused button, so focus moves to Keep it
  // on open and back to the Discard (or Remove) button on Keep it.
  const keepRef = useRef<HTMLButtonElement>(null);
  const discardRef = useRef<HTMLButtonElement>(null);
  const focusNext = useRef<"keep" | "discard" | null>(null);
  useEffect(() => {
    const target = focusNext.current === "keep" ? keepRef.current : focusNext.current === "discard" ? discardRef.current : null;
    focusNext.current = null;
    target?.focus();
  }, [confirming]);
  const [error, setError] = useState<{ text: string; pending: boolean } | null>(null);
  // Kept after the refactor is gone, until the panel closes or moves to another repo, or
  // another refactor appears.
  // `commits` is null when the merge outlasted its wait, so its outcome never came.
  const [merged, setMerged] = useState<{
    commits: number | null;
    target: string;
    kept: string[];
    worktree: string;
  } | null>(null);
  // A merge or discard still running in the helper after the wait: the buttons stay busy
  // until the refactor's state or worktree moves on.
  const [pending, setPending] = useState<{
    kind: "merge" | "discard";
    state: RefactorInfo["state"];
    worktree: string;
    target: string;
  } | null>(null);
  const worktree = info?.worktree ?? null;
  const state = info?.state ?? null;
  useEffect(() => {
    if (worktree !== null) setMerged((m) => (m && m.worktree !== worktree ? null : m));
  }, [worktree]);
  useEffect(() => {
    if (!pending || (pending.state === state && pending.worktree === worktree)) return;
    // Gone after a merge that outlasted its wait: it went in, with an unknown count.
    if (worktree === null && pending.kind === "merge") {
      setMerged({ commits: null, target: pending.target, kept: [], worktree: pending.worktree });
    }
    setPending(null);
    setBusy(null);
    setError(null);
  }, [pending, state, worktree]);
  /** `kind`'s request failed, or is still running when the error says so. */
  const fail = (kind: "merge" | "discard") => (err: unknown) => {
    const still = isPending(err);
    setError({ text: describeError(err), pending: still });
    if (still && info) {
      setPending({ kind, state: info.state, worktree: info.worktree, target: info.targetBranch });
    } else {
      setBusy(null);
    }
  };
  // A blocked merge whose reason the state doesn't show (the agent was still running).
  const [stopped, setStopped] = useState<string | null>(null);

  const merge = () => {
    if (!info) return;
    const target = info.targetBranch;
    setBusy("merge");
    setError(null);
    setStopped(null);
    onMerge()
      .then((outcome) => {
        if (outcome.outcome === "merged") {
          setMerged({ commits: outcome.commits, target, kept: outcome.kept ?? [], worktree: info.worktree });
        } else if (outcome.reason === "live") {
          setStopped(blockText(outcome, target));
        }
        setBusy(null);
      })
      .catch(fail("merge"));
  };

  const discard = () => {
    setBusy("discard");
    setError(null);
    onDiscard()
      .then(() => {
        setConfirming(false);
        setBusy(null);
      })
      .catch(fail("discard"));
  };

  const errorLine =
    error &&
    (error.pending ? (
      <p className="rf-hint" role="status">
        {error.text}
      </p>
    ) : (
      <p className="rf-error" role="alert">
        {error.text}
      </p>
    ));
  const mergedBanner = merged && (
    <div className="rf-refactor__merged" role="status">
      <p className="rf-ok">
        Merged {merged.commits === null ? "" : `${plural(merged.commits, "commit", "commits")} `}into{" "}
        <span className="rf-mono">{merged.target}</span>.
      </p>
      {merged.kept.map((line) => (
        <p key={line} className="rf-hint">
          {line}
        </p>
      ))}
    </div>
  );

  if (!info) {
    const blocker = refactorBlocker(repo);
    return (
      <section className="rf-section" aria-label="Refactor">
        <h3 className="rf-section__label">Refactor</h3>
        {mergedBanner}
        {!merged && (
          <p className="rf-hint">
            {blocker ?? "Claude proposes up to 3 refactors in a copy of the repo, then commits each one for you to review."}
          </p>
        )}
        <button
          type="button"
          className="rf-button"
          aria-haspopup="dialog"
          disabled={blocker !== null || launching}
          onClick={onStart}
        >
          <span>Refactor…</span>
          {ALT_R}
        </button>
        {errorLine}
      </section>
    );
  }

  const live = info.live;
  const commits = info.commits.length;
  const goTo = (
    <button type="button" className="rf-button" onClick={onOpen}>
      <span>Go to the refactor</span>
      {ALT_R}
    </button>
  );
  const resume = (fix: boolean, label: string) => (
    <button
      type="button"
      className={`rf-button${fix ? " rf-button--accent" : ""}`}
      disabled={live || launching || busy !== null}
      title={live ? LIVE_TITLE : undefined}
      onClick={() => onResume(fix)}
    >
      {label}
    </button>
  );
  const discardButton = (label: string) => (
    <button
      ref={discardRef}
      type="button"
      className="rf-button rf-button--danger"
      disabled={live || busy !== null}
      title={live ? LIVE_TITLE : undefined}
      onClick={() => {
        focusNext.current = "keep";
        setError(null);
        setConfirming(true);
      }}
    >
      {label}
    </button>
  );
  const mergeButton = (label: string) => (
    <button type="button" className="rf-button rf-button--primary" disabled={busy !== null} onClick={merge}>
      {busy === "merge" ? "Merging…" : label}
    </button>
  );

  // An idle agent left open is the normal end of a run; say what it holds up.
  const stillOpen = live && (
    <p className="rf-refactor__line rf-muted">Claude is still open in the refactor window; close it to fix or discard.</p>
  );
  let body: ReactNode;
  let actions: ReactNode;
  switch (info.state) {
    case "starting":
      body = (
        <p className="rf-refactor__line rf-refactor__busy" role="status">
          <span className="rf-spinner" aria-hidden="true" /> Opening VS Code…
        </p>
      );
      break;
    case "working":
      body = <p className="rf-refactor__line">Claude is planning or working in its own copy.</p>;
      actions = goTo;
      break;
    case "needsYou":
      body = (
        <p className="rf-refactor__line rf-refactor__attention">
          Claude needs you: approve its plan or answer its question.
        </p>
      );
      actions = goTo;
      break;
    case "interrupted":
      body = (
        <p className="rf-refactor__line rf-refactor__attention">
          The refactor stopped before it finished{commits ? ` with ${plural(commits, "commit", "commits")}` : ""}.
        </p>
      );
      actions = (
        <>
          {resume(false, "Resume refactor")}
          {discardButton("Discard")}
        </>
      );
      break;
    case "ready":
      body = (
        <>
          <p className="rf-refactor__line">
            Ready on <span className="rf-mono">{info.branch}</span>
            {live ? " · the agent is still open" : ""}
          </p>
          <Results info={info} />
        </>
      );
      actions = (
        <>
          {mergeButton(`Merge into ${info.targetBranch}`)}
          <button type="button" className="rf-button" onClick={onOpen}>
            <span>Review in VS Code</span>
            {ALT_R}
          </button>
          {info.interrupted && resume(false, "Resume refactor")}
          {discardButton("Discard")}
        </>
      );
      break;
    case "nothingToDo":
      body = (
        <>
          <p className="rf-refactor__line">
            {merged ? "The refactor's leftover copy is still here; Remove deletes it." : "Claude found nothing worth refactoring."}
          </p>
          {info.summary && <p className="rf-quote rf-refactor__summary">{info.summary}</p>}
          {stillOpen}
        </>
      );
      actions = (
        <>
          {live && goTo}
          {discardButton("Remove")}
        </>
      );
      break;
    case "mergeBlocked": {
      const block = info.blocked;
      body = (
        <>
          <p className="rf-refactor__line rf-refactor__attention" role="alert">
            {block ? blockText(block, info.targetBranch) : "The last merge stopped."}
          </p>
          {block && block.files.length > 0 && (
            <ul className="rf-refactor__files rf-mono" aria-label="Files">
              {block.files.map((f) => (
                <li key={f}>{f}</li>
              ))}
            </ul>
          )}
          <Results info={info} />
          {stillOpen}
        </>
      );
      actions = (
        <>
          {mergeButton("Merge again")}
          {block?.reason === "conflict" && resume(true, "Fix with Claude")}
          {(live || block?.reason === "worktreeDirty" || block?.reason === "worktreeBusy") && goTo}
          {discardButton("Discard")}
        </>
      );
      break;
    }
  }

  return (
    <section className="rf-section rf-refactor" aria-label="Refactor" aria-busy={busy !== null}>
      <h3 className="rf-section__label">Refactor</h3>
      {mergedBanner}
      {body}
      {stopped && <p className="rf-refactor__line rf-refactor__attention">{stopped}</p>}
      {confirming ? (
        <div className="rf-refactor__confirm" role="group" aria-label="Confirm discard">
          <p className="rf-refactor__question">{discardQuestion(commits)}</p>
          <p className="rf-hint">
            Deletes the worktree and the <span className="rf-mono">{info.branch}</span> branch. Your repo is not touched.
          </p>
          <div className="rf-row">
            <button type="button" className="rf-button rf-button--danger" disabled={live || busy !== null} onClick={discard}>
              {busy === "discard" ? "Discarding…" : commits > 0 ? "Discard" : "Remove"}
            </button>
            <button
              ref={keepRef}
              type="button"
              className="rf-button"
              disabled={busy !== null}
              onClick={() => {
                focusNext.current = "discard";
                setConfirming(false);
              }}
            >
              Keep it
            </button>
          </div>
        </div>
      ) : (
        actions && <div className="rf-row">{actions}</div>
      )}
      {errorLine}
    </section>
  );
}
