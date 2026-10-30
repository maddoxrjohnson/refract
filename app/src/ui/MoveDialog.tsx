import { useCallback, useEffect, useId, useRef, useState, type KeyboardEvent, type ReactNode } from "react";
import type { Backend } from "../backend/Backend";
import type {
  DeleteOutcome,
  MigrateError,
  MigratePreflight,
  MigrateSnapshotProgress,
  MigrationReport,
  RefusalView,
  RepoSnapshot,
  VsCodeWindow,
} from "../backend/types";
import { isCancelled, type MigrationControls } from "../hooks/useMigrations";
import {
  busyAgents,
  type BusyAgent,
  canDelete,
  installText,
  overallProgress,
  snapshotText,
  stepText,
  windowsPath,
  type Migration,
} from "../migrations";
import { formatBytes, sessionStateLabel } from "./format";
import { describeError } from "./StatusToast";
import "./ui.css";

/** How long the destination waits after typing before it is checked again. */
export const DEST_DEBOUNCE_MS = 600;
/** How often the VS Code windows are asked for again while some block Start. */
const WINDOWS_POLL_MS = 2500;
/** Problems listed per failed check; the rest are counted. */
const MAX_PROBLEMS = 6;

export interface MoveDialogProps {
  backend: Backend;
  /** The source's WSL path (`/mnt/c/...`). */
  source: string;
  name: string;
  /** The repo as last listed, for its agents; gone once the move is done. */
  repo?: RepoSnapshot;
  /** The source's migration, once started. */
  migration?: Migration;
  moves: MigrationControls;
  onClose(): void;
  /** Keep both, or No to deleting permanently: the original stays, and its gem is hidden. */
  onKeep(source: string): void;
  /** Delete original went through: the gem moves to `dest`. */
  onMoved(source: string, dest: string): void;
}

/** The selector for what Tab moves between inside the dialog. */
const FOCUSABLE = 'button:not(:disabled), input:not(:disabled), [href], [tabindex]:not([tabindex="-1"])';
/** The control that takes focus when a view appears; the heading when it is disabled. */
const PRIMARY = "[data-primary]:not(:disabled)";

/**
 * Move to WSL (spec §2.2–2.4): the preflight and Start, the progress, the report with its
 * final OK, and the result of Delete original. Docked where the detail panel is, so the
 * zoomed gem and its ring stay in view. Modal: it keeps the keyboard, and Escape closes
 * it except while an answer is being sent.
 */
export function MoveDialog(props: MoveDialogProps) {
  const { source, name, migration, moves, onClose } = props;
  const titleId = useId();
  const root = useRef<HTMLDivElement>(null);
  const heading = useRef<HTMLHeadingElement>(null);
  // Escape is ignored while an answer to the report is in flight.
  const [locked, setLocked] = useState(false);
  const [done, setDone] = useState<{ outcome: Extract<DeleteOutcome, { kind: "deleted" }>; dest: string } | null>(
    null,
  );
  // Why the last Start failed, shown over the fresh preflight.
  const [notice, setNotice] = useState<{ message: string; refusals: RefusalView[] } | null>(null);
  // Why the last Re-sync failed, shown on the report it went back to.
  const [resyncError, setResyncError] = useState<string | null>(null);

  // Each new view (preflight, progress, report, done) moves focus to its main control, or
  // to the heading, so the keyboard (and Escape) never ends up on the page behind.
  const view = done ? "done" : migration?.phase.kind === "ready" ? `report-${migration.id}` : migration ? "progress" : "preflight";
  useEffect(() => {
    const primary = root.current?.querySelector<HTMLElement>(PRIMARY);
    (primary ?? heading.current ?? root.current)?.focus();
  }, [view]);

  // Escape still closes if focus was lost to the page.
  const lockedRef = useRef(locked);
  lockedRef.current = locked;
  useEffect(() => {
    const onWindowKey = (ev: globalThis.KeyboardEvent) => {
      if (ev.key !== "Escape" || root.current?.contains(document.activeElement)) return;
      ev.preventDefault();
      if (!lockedRef.current) onClose();
    };
    window.addEventListener("keydown", onWindowKey);
    return () => window.removeEventListener("keydown", onWindowKey);
  }, [onClose]);

  // A held Enter must not press the button that takes focus next.
  const onKeyDownCapture = (ev: KeyboardEvent<HTMLDivElement>) => {
    if (ev.key === "Enter" && ev.repeat) {
      ev.preventDefault();
      ev.stopPropagation();
    }
  };

  const onKeyDown = (ev: KeyboardEvent<HTMLDivElement>) => {
    ev.stopPropagation();
    if (ev.key === "Escape") {
      ev.preventDefault();
      if (!locked) onClose();
      return;
    }
    if (ev.key !== "Tab" || !root.current) return;
    const items = [...root.current.querySelectorAll<HTMLElement>(FOCUSABLE)];
    if (items.length === 0) return;
    const first = items[0];
    const last = items[items.length - 1];
    const active = document.activeElement;
    if (ev.shiftKey && (active === first || active === root.current)) {
      ev.preventDefault();
      last.focus();
    } else if (!ev.shiftKey && active === last) {
      ev.preventDefault();
      first.focus();
    }
  };

  const start = (dest: string) => {
    setNotice(null);
    moves.start(source, dest).catch((err: unknown) => {
      if (isCancelled(err)) return;
      const e = err as Partial<MigrateError>;
      setNotice({ message: describeError(err), refusals: e.refusals ?? [] });
    });
  };

  let title = `Move ${name} to WSL`;
  let body: ReactNode;
  if (done) {
    title = `Moved ${name} to WSL`;
    body = <DoneView outcome={done.outcome} dest={done.dest} onClose={onClose} />;
  } else if (migration?.phase.kind === "ready" && migration.id) {
    body = (
      <ReportView
        key={migration.phase.report.finishedAt}
        backend={props.backend}
        id={migration.id}
        report={migration.phase.report}
        deleting={migration.phase.deleting}
        copyMissing={migration.phase.copyMissing}
        agents={busyAgents(props.repo)}
        resyncError={resyncError}
        onResyncError={setResyncError}
        onClose={onClose}
        moves={moves}
        setLocked={setLocked}
        onKeep={() => {
          props.onKeep(source);
          onClose();
        }}
        onDiscarded={onClose}
        onDeleted={(outcome) => {
          setDone({ outcome, dest: migration.dest });
          props.onMoved(source, migration.dest);
        }}
      />
    );
  } else if (migration) {
    body = <ProgressView migration={migration} moves={moves} onClose={onClose} />;
  } else {
    body = (
      <PreflightView
        backend={props.backend}
        source={source}
        name={name}
        repo={props.repo}
        notice={notice}
        onStart={start}
        onClose={onClose}
      />
    );
  }

  return (
    <>
      {/* Takes clicks meant for the scene while the dialog is open, without dimming it. */}
      <div className="rf-move-shield" onMouseDown={(ev) => ev.preventDefault()} />
      <div
        ref={root}
        className="rf-glass rf-move"
        role="dialog"
        aria-modal="true"
        aria-labelledby={titleId}
        tabIndex={-1}
        onKeyDown={onKeyDown}
        onKeyDownCapture={onKeyDownCapture}
      >
        <header className="rf-move__header">
          <h2 id={titleId} ref={heading} tabIndex={-1} className="rf-move__title">
            {title}
          </h2>
          <button
            type="button"
            className="rf-button rf-button--ghost rf-icon-button"
            aria-label="Close (Escape)"
            onClick={onClose}
            disabled={locked}
          >
            ×
          </button>
        </header>
        {body}
      </div>
    </>
  );
}

function plural(n: number, one: string, many: string): string {
  return `${n.toLocaleString("en-US")} ${n === 1 ? one : many}`;
}

/** The skipped build folders, with their sizes. */
function SkippedList({ skipped }: { skipped: { path: string; bytes: number }[] }) {
  return (
    <ul className="rf-move-list">
      {skipped.map((s) => (
        <li key={s.path}>
          <span className="rf-mono rf-move-list__name">{s.path}</span>
          <span className="rf-muted rf-mono">{formatBytes(s.bytes)}</span>
        </li>
      ))}
    </ul>
  );
}

function Refusals({ refusals }: { refusals: RefusalView[] }) {
  return (
    <ul className="rf-move-refusals" role="list">
      {refusals.map((r, i) => (
        <li key={`${r.kind}-${i}`}>{r.message}</li>
      ))}
    </ul>
  );
}

/** A thin bar with the step above it. */
function ProgressLine({ text, fraction }: { text: string; fraction: number | null }) {
  return (
    <div className="rf-move-progress">
      <div className="rf-move-progress__text" aria-live="polite">
        {text}
      </div>
      <div
        className="rf-move-bar"
        role="progressbar"
        aria-label={text}
        aria-valuemin={0}
        aria-valuemax={100}
        aria-valuenow={fraction === null ? undefined : Math.round(fraction * 100)}
      >
        <div
          className={`rf-move-bar__fill${fraction === null ? " rf-move-bar__fill--unknown" : ""}`}
          style={{ width: `${((fraction ?? 0.25) * 100).toFixed(1)}%` }}
        />
      </div>
    </div>
  );
}

function snapshotFraction(p: MigrateSnapshotProgress | null): number | null {
  if (!p) return null;
  if (p.bytesTotal > 0) return Math.min(1, p.bytesDone / p.bytesTotal);
  if (p.filesTotal > 0) return Math.min(1, p.filesDone / p.filesTotal);
  return null;
}

function isLinuxPath(dest: string): boolean {
  return dest.startsWith("/") && !/^\/mnt\/[a-z](\/|$)/i.test(dest);
}

/** The repo's agents with **Stop** for those finished or waiting; a working one can't be stopped. */
function AgentRows({
  backend,
  source,
  agents,
  onError,
}: {
  backend: Backend;
  source: string;
  agents: BusyAgent[];
  onError(message: string | null): void;
}) {
  const [confirmStop, setConfirmStop] = useState<string | null>(null);
  const [stopping, setStopping] = useState<string | null>(null);
  const stopAgent = (sessionId: string) => {
    setConfirmStop(null);
    setStopping(sessionId);
    onError(null);
    backend
      .migrateStopAgent(source, sessionId)
      .catch((err: unknown) => onError(describeError(err)))
      .finally(() => setStopping(null));
  };
  return (
    <>
      {agents.map(({ session, working }) => (
        <li key={`a-${session.id}`}>
          <span className="rf-move-busy__name">Agent · {sessionStateLabel(session.state)}</span>
          {working ? (
            <button type="button" className="rf-button" disabled>
              Agent is working
            </button>
          ) : confirmStop === session.id ? (
            <span className="rf-move-confirm">
              <button type="button" className="rf-button" onClick={() => stopAgent(session.id)}>
                Stop agent
              </button>
              <button type="button" className="rf-button rf-button--ghost" onClick={() => setConfirmStop(null)}>
                Keep
              </button>
            </span>
          ) : (
            <button
              type="button"
              className="rf-button"
              disabled={stopping === session.id}
              onClick={() => setConfirmStop(session.id)}
            >
              {stopping === session.id ? "Stopping…" : "Stop"}
            </button>
          )}
        </li>
      ))}
    </>
  );
}

interface PreflightViewProps {
  backend: Backend;
  source: string;
  name: string;
  repo?: RepoSnapshot;
  notice: { message: string; refusals: RefusalView[] } | null;
  onStart(dest: string): void;
  onClose(): void;
}

/** What will happen, what blocks Start, and Start (spec §2.2). */
function PreflightView({ backend, source, name, repo, notice, onStart, onClose }: PreflightViewProps) {
  const destId = useId();
  const [preflight, setPreflight] = useState<MigratePreflight | null>(null);
  const [checking, setChecking] = useState(true);
  const [progress, setProgress] = useState<MigrateSnapshotProgress | null>(null);
  const [error, setError] = useState<string | null>(null);
  const [refusals, setRefusals] = useState<RefusalView[]>([]);
  const [windows, setWindows] = useState<VsCodeWindow[]>([]);
  // The destination as typed; `null` until the first preflight suggests one.
  const [dest, setDest] = useState<string | null>(null);
  // The destination the current preflight checked.
  const [checked, setChecked] = useState<string | null>(null);
  const requested = useRef<string | null>(null);
  const seq = useRef(0);
  const inFlight = useRef<Promise<unknown> | null>(null);
  const [note, setNote] = useState<string | null>(null);

  /** Runs the preflight, after stopping one still running: only one may run per folder. */
  const run = useCallback(
    async (target?: string) => {
      const id = ++seq.current;
      requested.current = target ?? null;
      setChecking(true);
      setError(null);
      const previous = inFlight.current;
      if (previous) {
        await backend.migrateCancelSnapshot(source).catch(() => false);
        await previous.catch(() => {});
      }
      if (id !== seq.current) return;
      const request = backend.migratePreflight(source, target);
      inFlight.current = request;
      try {
        const answer = await request;
        if (id !== seq.current) return;
        // What was typed stays the key, even if the answer spells it differently.
        requested.current = target ?? answer.dest;
        setPreflight(answer);
        setRefusals(answer.refusals);
        setWindows(answer.vscodeWindows);
        setChecked(target ?? answer.dest);
        setDest((current) => (current === null || target === undefined ? (answer.dest ?? "") : current));
      } catch (err) {
        if (id !== seq.current || isCancelled(err)) return;
        const e = err as Partial<MigrateError>;
        if (e.kind === "refused" && e.refusals) setRefusals(e.refusals);
        else setError(describeError(err));
        setDest((current) => current ?? "");
      } finally {
        if (inFlight.current === request) inFlight.current = null;
        if (id === seq.current) {
          setChecking(false);
          setProgress(null);
        }
      }
    },
    [backend, source],
  );

  useEffect(() => {
    void run();
    return () => {
      seq.current++;
      if (inFlight.current) void backend.migrateCancelSnapshot(source).catch(() => false);
    };
  }, [backend, source, run]);

  useEffect(
    () =>
      backend.onMigrateSnapshot((p) => {
        if (p.path === source) setProgress(p);
      }),
    [backend, source],
  );

  // A changed destination is checked again once typing stops.
  const draft = dest?.trim() ?? "";
  const destOk = isLinuxPath(draft);
  useEffect(() => {
    if (dest === null || !destOk || draft === requested.current) return;
    const timer = setTimeout(() => void run(draft), DEST_DEBOUNCE_MS);
    return () => clearTimeout(timer);
  }, [dest, draft, destOk, run]);

  // Closed windows drop off the list; it is asked for again while any block Start.
  const refreshWindows = useCallback(() => {
    backend.migrateWindows(source).then(setWindows, (err: unknown) => setNote(describeError(err)));
  }, [backend, source]);
  const hasWindows = windows.length > 0;
  useEffect(() => {
    if (!hasWindows) return;
    const timer = setInterval(refreshWindows, WINDOWS_POLL_MS);
    return () => clearInterval(timer);
  }, [hasWindows, refreshWindows]);

  const closeWindow = (w: VsCodeWindow) => {
    setNote(null);
    backend.migrateCloseWindow(source, w.id).then(
      () => refreshWindows(),
      (err: unknown) => setNote(describeError(err)),
    );
  };

  const agents = busyAgents(repo);
  const busy = windows.length > 0 || agents.length > 0;
  const allRefusals = checking ? [] : refusals;
  const canStart =
    !!preflight && !checking && !error && allRefusals.length === 0 && !busy && destOk && draft === checked;

  return (
    <>
      <div className="rf-move__body">
        {notice && (
          <div className="rf-move-notice" role="alert">
            <p className="rf-error">{notice.message}</p>
            {notice.refusals.length > 0 && <Refusals refusals={notice.refusals} />}
          </div>
        )}
        <section className="rf-section" aria-label="Where it goes">
          <div className="rf-field">
            <label htmlFor={destId}>Destination in WSL</label>
            <input
              id={destId}
              className="rf-input rf-mono rf-move__dest"
              type="text"
              spellCheck={false}
              autoComplete="off"
              value={dest ?? ""}
              placeholder={dest === null ? "Finding a folder…" : `/home/you/projects/${name}`}
              disabled={dest === null}
              aria-invalid={dest !== null && !destOk}
              onChange={(ev) => setDest(ev.target.value)}
            />
          </div>
          {dest !== null && !destOk && (
            <p className="rf-error">Use a folder on the Linux side, such as /home/you/projects/{name}.</p>
          )}
          <dl className="rf-facts rf-move-facts">
            <dt>From</dt>
            <dd className="rf-mono">{preflight?.windowsPath ?? windowsPath(source)}</dd>
            {preflight && (
              <>
                <dt>Size</dt>
                <dd>
                  {formatBytes(preflight.bytes)} in {plural(preflight.files, "file", "files")}
                </dd>
                <dt>History</dt>
                <dd>
                  {preflight.historySessions > 0
                    ? `${plural(preflight.historySessions, "Claude session", "Claude sessions")} will move with it.`
                    : "No Claude sessions to move."}
                </dd>
              </>
            )}
          </dl>
        </section>

        {checking && (
          <section className="rf-section" aria-label="Checking">
            <ProgressLine
              text={progress ? snapshotText(progress) : "Checking the folder"}
              fraction={snapshotFraction(progress)}
            />
          </section>
        )}
        {error && (
          <p className="rf-error" role="alert">
            {error}{" "}
            <button type="button" className="rf-link" onClick={() => void run(destOk ? draft : undefined)}>
              Try again
            </button>
          </p>
        )}

        {preflight && preflight.skipped.length > 0 && (
          <section className="rf-section" aria-label="Build folders left out">
            <h3 className="rf-section__label">Build folders left out</h3>
            <SkippedList skipped={preflight.skipped} />
            <p className="rf-move-note rf-muted">They go to the Recycle Bin with the original.</p>
          </section>
        )}

        {preflight && preflight.install.length > 0 && (
          <section className="rf-section" aria-label="Install">
            <h3 className="rf-section__label">Afterwards it runs</h3>
            <ul className="rf-move-list">
              {preflight.install.map((step) => (
                <li key={`${step.folder}-${step.command}`}>
                  <code className="rf-mono">{step.command}</code>
                  {step.missing && (
                    <p className="rf-move-note rf-muted">
                      {step.tool} isn't installed in WSL, so this step will be skipped. Install it with{" "}
                      <code className="rf-mono">{step.installHint}</code>.
                    </p>
                  )}
                </li>
              ))}
            </ul>
          </section>
        )}

        {preflight?.cloudSynced && (
          <p className="rf-move-notice rf-move-warn" role="note">
            This folder is synced by OneDrive. Deleting the original after the move also deletes it from OneDrive and
            your other devices.
          </p>
        )}

        {preflight &&
          (preflight.warnings.length > 0 || preflight.linuxGit || preflight.droppedStreams > 0) && (
            <section className="rf-section" aria-label="Warnings">
              <h3 className="rf-section__label">Worth knowing</h3>
              <ul className="rf-move-warnings">
                {preflight.warnings.map((w) => (
                  <li key={w}>{w}</li>
                ))}
                {preflight.linuxGit && (
                  <li>Git for Windows wasn't found, so Linux git reads the folder. That is slower.</li>
                )}
                {preflight.droppedStreams > 0 && (
                  <li>
                    {plural(preflight.droppedStreams, "alternate data stream", "alternate data streams")} (such as
                    Zone.Identifier) won't be copied. The report lists them.
                  </li>
                )}
              </ul>
            </section>
          )}

        {busy && (
          <section className="rf-section" aria-label="Close these first">
            <h3 className="rf-section__label">Close these first</h3>
            <ul className="rf-move-busy">
              {windows.map((w) => (
                <li key={`w-${w.id}`}>
                  <span className="rf-move-busy__name">{w.title}</span>
                  <button
                    type="button"
                    className="rf-button"
                    aria-label={`Close VS Code window ${w.title}`}
                    onClick={() => closeWindow(w)}
                  >
                    Close
                  </button>
                </li>
              ))}
              <AgentRows backend={backend} source={source} agents={agents} onError={setNote} />
            </ul>
            <p className="rf-move-note rf-muted">A window with unsaved files still asks before it closes.</p>
          </section>
        )}
        {note && (
          <p className="rf-error" role="alert">
            {note}
          </p>
        )}
        {allRefusals.length > 0 && (
          <section className="rf-section" aria-label="Can't move this folder">
            <h3 className="rf-section__label">Can't move this folder</h3>
            <Refusals refusals={allRefusals} />
          </section>
        )}
      </div>
      <footer className="rf-move__actions">
        {allRefusals.length === 0 && (
          <button
            type="button"
            className="rf-button rf-button--accent"
            data-primary
            disabled={!canStart}
            onClick={() => onStart(draft)}
          >
            Start
          </button>
        )}
        <button type="button" className="rf-button" onClick={onClose}>
          Cancel
        </button>
        <p className="rf-hint rf-move__hint">
          {allRefusals.length > 0
            ? "Nothing was changed."
            : busy
              ? "Start is available once nothing has the folder open."
              : "The original stays until you confirm the report."}
        </p>
      </footer>
    </>
  );
}

/** The snapshot and the copy, until the report (spec §2.3). */
function ProgressView({
  migration,
  moves,
  onClose,
}: {
  migration: Migration;
  moves: MigrationControls;
  onClose(): void;
}) {
  const [cancelling, setCancelling] = useState(false);
  const [error, setError] = useState<string | null>(null);
  // Once the copy is in place (install, history, or a re-sync), stopping keeps it.
  const placed = migration.phase.kind === "running" && migration.phase.placed;
  const cancel = () => {
    setCancelling(true);
    setError(null);
    moves
      .cancel(migration.source)
      .catch((err: unknown) => setError(describeError(err)))
      .finally(() => setCancelling(false));
  };
  return (
    <>
      <div className="rf-move__body">
        <section className="rf-section" aria-label="Progress">
          <dl className="rf-facts rf-move-facts">
            <dt>From</dt>
            <dd className="rf-mono">{windowsPath(migration.source)}</dd>
            <dt>To</dt>
            <dd className="rf-mono">{migration.dest}</dd>
          </dl>
          <ProgressLine text={stepText(migration.phase)} fraction={overallProgress(migration.phase)} />
          <p className="rf-move-note rf-muted">
            You can hide Refract. A notification appears when the copy is ready for your OK, or if it fails.
          </p>
        </section>
        {error && (
          <p className="rf-error" role="alert">
            {error}
          </p>
        )}
      </div>
      <footer className="rf-move__actions">
        <button type="button" className="rf-button" data-primary onClick={cancel} disabled={cancelling}>
          {placed ? (cancelling ? "Stopping…" : "Stop and keep the copy") : cancelling ? "Cancelling…" : "Cancel the move"}
        </button>
        <button type="button" className="rf-button" onClick={onClose}>
          Close
        </button>
        <p className="rf-hint rf-move__hint">
          {placed
            ? "The copy is already in place, so stopping keeps it: the rest of this step is skipped, and the report shows so you can choose what to keep. The original is untouched."
            : "Cancelling deletes the partial copy. The original is untouched."}
        </p>
      </footer>
    </>
  );
}

/** What Delete original said, short of `deleted`. */
type DeleteState = Exclude<DeleteOutcome, { kind: "deleted" }> | null;

interface ReportViewProps {
  backend: Backend;
  id: string;
  report: MigrationReport;
  /** Delete original has begun: only finishing it is offered (or ending the move once the copy is gone). */
  deleting: boolean;
  /** The helper says the placed copy is gone. */
  copyMissing: boolean;
  /** The repo's live agents, for Stop when the recheck finds them. */
  agents: BusyAgent[];
  /** Why the last Re-sync failed; the report shows again when it does. */
  resyncError: string | null;
  onResyncError(message: string | null): void;
  moves: MigrationControls;
  setLocked(locked: boolean): void;
  onClose(): void;
  onKeep(): void;
  onDiscarded(): void;
  onDeleted(outcome: Extract<DeleteOutcome, { kind: "deleted" }>): void;
}

const STATUS_LABEL = { passed: "Passed", failed: "Failed", notRun: "Not run" } as const;

/** Paths, the first few of them, with how many more. */
function PathList({ paths }: { paths: string[] }) {
  return (
    <div className="rf-mono rf-muted rf-move-paths">
      {paths.slice(0, MAX_PROBLEMS).join("\n")}
      {paths.length > MAX_PROBLEMS && `\nand ${paths.length - MAX_PROBLEMS} more`}
    </div>
  );
}

/** The report and the final OK (spec §2.4, §6). */
function ReportView(props: ReportViewProps) {
  const { backend, id, report, moves, setLocked, onKeep, onDiscarded, onDeleted } = props;
  const ok = canDelete(report);
  const changes = report.installChanges;
  const installChanges: [string, string][] = changes
    ? [
        ...changes.created.map((p): [string, string] => ["created", p]),
        ...changes.changed.map((p): [string, string] => ["changed", p]),
        ...changes.removed.map((p): [string, string] => ["removed", p]),
      ]
    : [];
  const [pending, setPending] = useState<string | null>(null);
  const [outcome, setOutcome] = useState<DeleteState>(null);
  const [error, setError] = useState<string | null>(props.resyncError);
  const [note, setNote] = useState<string | null>(null);
  // Asked before ending a move whose copy went while deleting.
  const [confirmEnd, setConfirmEnd] = useState(false);
  // The original folder is gone (deleted by hand): the move only needs finishing.
  const [sourceGone, setSourceGone] = useState(false);
  const deleting = props.deleting || outcome?.kind === "unfinished" || (outcome !== null && "deleting" in outcome && outcome.deleting);
  // An older report has no `placed`: one that passed was placed.
  const placed = report.placed ?? ok;
  const copyGone =
    props.copyMissing ||
    (outcome?.kind === "changed" && outcome.copyChanged.length === 1 && outcome.copyChanged[0] === ".");
  useEffect(() => {
    if (!placed || props.deleting) return;
    let live = true;
    backend.migrateSourceExists(id).then(
      (exists) => live && setSourceGone(!exists),
      () => {},
    );
    return () => {
      live = false;
    };
  }, [backend, id, placed, props.deleting]);
  // The default answer (Delete original, or No, …) takes focus when it appears, and again
  // when an answer in flight disabled the focused button.
  const defaultAction = useRef<HTMLButtonElement>(null);
  const outcomeKind = `${outcome?.kind ?? "none"}-${deleting}-${sourceGone}-${copyGone}-${confirmEnd}`;
  useEffect(() => {
    if (pending) return;
    const active = document.activeElement as HTMLButtonElement | null;
    if (!active || active === document.body || active.disabled || active.classList.contains("rf-move")) {
      defaultAction.current?.focus();
    }
  }, [pending, outcomeKind]);
  const lastKind = useRef(outcomeKind);
  useEffect(() => {
    if (lastKind.current !== outcomeKind) defaultAction.current?.focus();
    lastKind.current = outcomeKind;
  }, [outcomeKind]);

  /** Runs one answer at a time; Escape and the other buttons wait for it. */
  const act = async (label: string, work: () => Promise<void>) => {
    setPending(label);
    setLocked(true);
    setError(null);
    try {
      await work();
    } catch (err) {
      setError(describeError(err));
    } finally {
      setPending(null);
      setLocked(false);
    }
  };

  const deleteOriginal = (permanent: boolean, force = false) =>
    act(
      permanent
        ? "Deleting permanently…"
        : force
          ? "Deleting the rest…"
          : deleting
            ? "Finishing the delete…"
            : "Deleting the original…",
      async () => {
        const answer = await moves.remove(id, permanent, force);
        if (answer.kind === "deleted") onDeleted(answer);
        else setOutcome(answer);
      },
    );
  const keep = () => act("Keeping both…", () => moves.finish(id, "keep").then(onKeep));
  const discard = () => act("Discarding the copy…", () => moves.finish(id, "discard").then(onDiscarded));
  // With the copy gone, discarding only forgets the move.
  const endMove = () => act("Ending the move…", () => moves.finish(id, "discard").then(onDiscarded));
  // The dialog shows a fresh preflight once the entry is gone.
  const retry = () => act("Discarding the copy…", () => moves.finish(id, "discard"));
  // The report goes away while it runs, so a refusal is shown by the report that returns.
  const resync = () => {
    props.onResyncError(null);
    void act("Re-syncing…", () =>
      moves.resync(id).catch((err: unknown) => {
        props.onResyncError(describeError(err));
        throw err;
      }),
    );
  };
  const closeWindow = (w: VsCodeWindow) =>
    backend.migrateCloseWindow(report.source, w.id).then(
      () =>
        setOutcome((o) =>
          o?.kind === "windowsOpen" ? { ...o, windows: o.windows.filter((x) => x.id !== w.id) } : o,
        ),
      (err: unknown) => setError(describeError(err)),
    );

  const disabled = pending !== null;
  const primary = (label: string, onClick: () => void) => (
    <button type="button" className="rf-button rf-button--accent" ref={defaultAction} data-primary disabled={disabled} onClick={onClick}>
      {label}
    </button>
  );
  const secondary = (label: string, onClick: () => void) => (
    <button type="button" className="rf-button" disabled={disabled} onClick={onClick}>
      {label}
    </button>
  );
  const finishButton = primary("Finish deleting", () => void deleteOriginal(false));
  const keepLabel = sourceGone ? "Keep the copy" : "Keep both";
  let actions: ReactNode;
  if (copyGone && deleting) {
    // The rest of the original must never go now: the copy that held it is gone.
    actions = (
      <div className="rf-move-ask" role="alert">
        <p className="rf-move-ask__question">The copy in WSL is gone, so nothing more of the original will be deleted.</p>
        <p className="rf-move-note">
          Deleting the original had already begun. What was deleted of it is in the Recycle Bin (unless you chose to
          delete it permanently), and the rest is still at <span className="rf-mono">{windowsPath(report.source)}</span>.
          Restore the deleted part from the Recycle Bin to put the folder back together.
        </p>
        {report.installBackup && (
          <p className="rf-move-note rf-muted">
            The original's version of each file the install changed is saved in{" "}
            <span className="rf-mono">{report.installBackup}</span>.
          </p>
        )}
        {confirmEnd ? (
          <>
            <p className="rf-move-note">
              End the move? Refract stops tracking it and deletes nothing. Get the original back from the Recycle Bin
              first if you need it.
            </p>
            <div className="rf-row">
              {primary("Back", () => setConfirmEnd(false))}
              <button type="button" className="rf-button rf-button--danger" disabled={disabled} onClick={endMove}>
                Yes, end the move
              </button>
            </div>
          </>
        ) : (
          <div className="rf-row">
            {primary("End the move", () => setConfirmEnd(true))}
            {secondary("Not now", props.onClose)}
          </div>
        )}
      </div>
    );
  } else if (copyGone) {
    actions = (
      <div className="rf-move-ask" role="alert">
        <p className="rf-move-ask__question">The copy in WSL is gone, so there's nothing to keep.</p>
        <p className="rf-move-note rf-muted">
          {sourceGone
            ? "The original folder is gone too. Ending the move only forgets it; nothing is deleted."
            : "The original is as it was. Ending the move only forgets it; nothing is deleted."}
        </p>
        <div className="rf-row">{primary("End the move", endMove)}</div>
      </div>
    );
  } else if (outcome?.kind === "wontFit") {
    actions = (
      <div className="rf-move-ask" role="alertdialog" aria-label="Delete permanently?">
        <p className="rf-move-ask__question">
          {deleting
            ? "What's left won't fit in the Recycle Bin. Delete it permanently?"
            : "This won't fit in the Recycle Bin. Delete permanently?"}
        </p>
        {outcome.reason && <p className="rf-move-note rf-muted">{outcome.reason}</p>}
        <p className="rf-move-note rf-muted">
          {deleting ? "Not now leaves it as it is; the copy is safe." : "No keeps both copies."}
        </p>
        <div className="rf-row">
          {primary(deleting ? "Not now" : "No", deleting ? props.onClose : keep)}
          <button type="button" className="rf-button rf-button--danger" disabled={disabled} onClick={() => void deleteOriginal(true)}>
            Yes, delete permanently
          </button>
        </div>
      </div>
    );
  } else if (outcome?.kind === "driveHidden") {
    actions = (
      <div className="rf-move-ask" role="alert">
        <p className="rf-move-ask__question">WSL can't see the drive the original is on, so nothing was deleted.</p>
        <p className="rf-move-note rf-muted">
          The folder is still there on Windows, but not at {report.source}, so Refract couldn't check it for changes.
          Make the drive visible in WSL (for example, restart WSL or mount it), then{" "}
          {deleting
            ? "finish deleting. Or delete what's left of it yourself on Windows; finishing then moves its history and settings over."
            : "try again, or keep both."}
        </p>
        <div className="rf-row">
          {deleting ? (
            finishButton
          ) : (
            <>
              {primary("Try again", () => void deleteOriginal(false))}
              {secondary("Keep both", keep)}
            </>
          )}
        </div>
      </div>
    );
  } else if (outcome?.kind === "changed") {
    const originalChanged = outcome.changed.length > 0 || outcome.unreadable.length > 0;
    const copyChanged = outcome.copyChanged.length > 0;
    const resyncable = outcome.changed.length > 0 && !copyChanged;
    // While deleting, a changed copy may be left behind on purpose: the copy keeps its version either way.
    const canForce = deleting && copyChanged && !originalChanged && outcome.busyAgents.length === 0;
    const question = deleting
      ? originalChanged
        ? "The original changed while it was being deleted, so nothing more was deleted."
        : copyChanged
          ? "The copy in WSL differs from what's left of the original, so nothing more was deleted."
          : "Something is still using the original, so nothing more was deleted."
      : originalChanged
        ? "The original changed after the check, so nothing was deleted."
        : copyChanged
          ? "The copy in WSL changed after the check, so nothing was deleted."
          : "Something is still using the original, so nothing was deleted.";
    actions = (
      <div className="rf-move-ask" role="alert">
        <p className="rf-move-ask__question">{question}</p>
        {outcome.changed.length > 0 && (
          <>
            <p className="rf-move-note">Changed or added in the original:</p>
            <PathList paths={outcome.changed} />
          </>
        )}
        {copyChanged &&
          (deleting ? (
            <>
              <p className="rf-move-note">
                These were changed or removed in the copy, and the original still has its own version of them:
              </p>
              <PathList paths={outcome.copyChanged} />
              <p className="rf-move-note rf-muted">
                Your copy keeps its version either way. Delete the rest anyway sends the original's version of these,
                and everything else left of the original at {windowsPath(report.source)}, to the Recycle Bin. Or undo
                those changes in the copy, then finish deleting.
              </p>
            </>
          ) : (
            <>
              <p className="rf-move-note">
                The copy in WSL changed after it was checked, so it may hold work the original doesn't. Refract won't
                re-sync over it or delete it; you can keep both:
              </p>
              <PathList paths={outcome.copyChanged} />
            </>
          ))}
        {outcome.unreadable.length > 0 && (
          <>
            <p className="rf-move-note">
              These couldn't be read, so Refract can't tell whether they changed. Close any program using them, then
              try again:
            </p>
            <PathList paths={outcome.unreadable} />
          </>
        )}
        {outcome.removed.length > 0 && (
          <>
            <p className="rf-move-note rf-muted">
              Gone from the original since the check. That's fine: the copy still has them.
            </p>
            <PathList paths={outcome.removed} />
          </>
        )}
        {outcome.busyAgents.map((sentence) => (
          <p key={sentence} className="rf-move-note">
            {sentence}
          </p>
        ))}
        {outcome.busyAgents.length > 0 && props.agents.length > 0 && (
          <ul className="rf-move-busy">
            <AgentRows backend={backend} source={report.source} agents={props.agents} onError={setNote} />
          </ul>
        )}
        {deleting && originalChanged && (
          <p className="rf-move-note rf-muted">
            Undo the change, or move those files out of the folder, then finish deleting.
          </p>
        )}
        <div className="rf-row">
          {deleting ? (
            <>
              {finishButton}
              {canForce && (
                <button
                  type="button"
                  className="rf-button rf-button--danger"
                  disabled={disabled}
                  onClick={() => void deleteOriginal(false, true)}
                >
                  Delete the rest anyway
                </button>
              )}
            </>
          ) : resyncable ? (
            primary("Re-sync", resync)
          ) : !copyChanged ? (
            primary("Try again", () => void deleteOriginal(false))
          ) : null}
          {!deleting && (
            <button
              type="button"
              className={`rf-button${copyChanged ? " rf-button--accent" : ""}`}
              ref={copyChanged ? defaultAction : undefined}
              data-primary={copyChanged || undefined}
              disabled={disabled}
              onClick={keep}
            >
              Keep both
            </button>
          )}
        </div>
      </div>
    );
  } else if (outcome?.kind === "windowsOpen") {
    actions = (
      <div className="rf-move-ask" role="alert">
        <p className="rf-move-ask__question">Close these VS Code windows first.</p>
        <ul className="rf-move-busy">
          {outcome.windows.map((w) => (
            <li key={w.id}>
              <span className="rf-move-busy__name">{w.title}</span>
              <button
                type="button"
                className="rf-button"
                aria-label={`Close VS Code window ${w.title}`}
                onClick={() => void closeWindow(w)}
              >
                Close
              </button>
            </li>
          ))}
        </ul>
        <p className="rf-move-note rf-muted">
          A WSL window on the new copy has the same name, so it's listed too.
        </p>
        <div className="rf-row">
          {primary(deleting ? "Finish deleting" : "Try again", () => void deleteOriginal(false))}
          {secondary("Back", () => setOutcome(null))}
        </div>
      </div>
    );
  } else if (deleting) {
    actions = (
      <div className="rf-move-ask" role="alert">
        <p className="rf-move-ask__question">Deleting the original didn't finish.</p>
        {outcome?.kind === "unfinished" && <p className="rf-move-note">{outcome.reason}</p>}
        <p className="rf-move-note rf-muted">
          The copy in WSL is complete. Part of the original may already be in the Recycle Bin; finishing deletes the
          rest.
        </p>
        <div className="rf-row">{finishButton}</div>
      </div>
    );
  } else if (ok && sourceGone) {
    actions = (
      <div className="rf-move-ask" role="alert">
        <p className="rf-move-ask__question">The original folder is already gone.</p>
        <p className="rf-move-note rf-muted">The copy in WSL is complete. Finishing moves its history and settings over.</p>
        <div className="rf-row">{primary("Finish the move", () => void deleteOriginal(false))}</div>
      </div>
    );
  } else if (ok) {
    actions = (
      <div className="rf-row">
        {primary("Delete original", () => void deleteOriginal(false))}
        {secondary("Keep both", keep)}
        {secondary("Discard the copy", discard)}
      </div>
    );
  } else if (placed && sourceGone) {
    actions = (
      <div className="rf-move-ask" role="alert">
        <p className="rf-move-ask__question">The original folder is gone, so the copy is the only one left.</p>
        <p className="rf-move-note rf-muted">
          A check failed, so look over the copy at {report.dest}. Keeping it makes it the repo Refract shows.
        </p>
        <div className="rf-row">{primary(keepLabel, keep)}</div>
      </div>
    );
  } else if (placed) {
    actions = (
      <div className="rf-move-ask" role="alert">
        <p className="rf-move-note rf-muted">
          The copy is in place at {report.dest}. Re-sync copies what changed in the original again and checks it again;
          or keep both, or discard the copy.
        </p>
        <div className="rf-row">
          {primary("Re-sync", resync)}
          {secondary("Keep both", keep)}
          {secondary("Discard the copy", discard)}
        </div>
      </div>
    );
  } else {
    actions = (
      <div className="rf-row">
        {primary("Discard the copy", discard)}
        {secondary("Retry", retry)}
      </div>
    );
  }

  return (
    <>
      <div className="rf-move__body">
        <section className="rf-section" aria-label="Summary">
          <p className={`rf-move-verdict rf-move-verdict--${ok ? "ok" : "failed"}`}>
            {!ok
              ? "A check failed, so the original can't be deleted. Nothing was changed on Windows."
              : deleting
                ? "The copy passed every check. Deleting the original has begun."
                : "The copy passed every check. Nothing has been deleted yet."}
          </p>
          <dl className="rf-facts rf-move-facts">
            <dt>From</dt>
            <dd className="rf-mono">{windowsPath(report.source)}</dd>
            <dt>To</dt>
            <dd className="rf-mono">{report.dest}</dd>
            <dt>Copied</dt>
            <dd>
              {formatBytes(report.copyTotals.bytes)} in {plural(report.copyTotals.files, "file", "files")}
            </dd>
          </dl>
        </section>

        <section className="rf-section" aria-label="Checks">
          <h3 className="rf-section__label">Checks</h3>
          <ul className="rf-move-checks">
            {report.checks.map((c) => (
              <li key={c.check} className={`rf-move-check rf-move-check--${c.status}`}>
                <span className="rf-move-check__icon" aria-hidden="true">
                  {c.status === "passed" ? "✓" : c.status === "failed" ? "✕" : "–"}
                </span>
                <div className="rf-move-check__body">
                  <div>
                    {c.title}
                    <span className="rf-visually-hidden">: {STATUS_LABEL[c.status]}</span>
                    {c.check === "history" && c.status === "failed" && (
                      <span className="rf-muted"> · doesn't block deleting</span>
                    )}
                  </div>
                  {c.problems.length > 0 && (
                    <div className="rf-mono rf-muted rf-move-paths">
                      {c.problems
                        .slice(0, MAX_PROBLEMS)
                        .map((p) => (p.path ? `${p.path}: ${p.message}` : p.message))
                        .join("\n")}
                      {c.problems.length > MAX_PROBLEMS && `\nand ${c.problems.length - MAX_PROBLEMS} more`}
                    </div>
                  )}
                  {c.status === "failed" && c.advice && <div className="rf-move-check__advice">{c.advice}</div>}
                </div>
              </li>
            ))}
          </ul>
        </section>

        {report.skipped.length > 0 && (
          <section className="rf-section" aria-label="Build folders left out">
            <h3 className="rf-section__label">Build folders left out</h3>
            <SkippedList skipped={report.skipped} />
          </section>
        )}

        {report.install.length > 0 && (
          <section className="rf-section" aria-label="Install">
            <h3 className="rf-section__label">Install</h3>
            <ul className="rf-move-list rf-move-list--stacked">
              {report.install.map((r) => (
                <li key={`${r.step.folder}-${r.step.command}`}>
                  <code className="rf-mono">{r.step.command}</code>
                  <span className={r.status === "succeeded" ? "rf-muted" : "rf-move-warn"}>{installText(r)}</span>
                  {r.status === "toolMissing" && (
                    <span className="rf-muted">
                      Install it with <code className="rf-mono">{r.step.installHint}</code>, then run the command in the
                      new folder.
                    </span>
                  )}
                  {r.logPath && r.status !== "succeeded" && (
                    <span className="rf-muted">
                      Log: <span className="rf-mono">{r.logPath}</span>
                    </span>
                  )}
                </li>
              ))}
            </ul>
            {installChanges.length > 0 && (
              <>
                <p className="rf-move-note rf-muted">
                  Changed by install (this doesn't block deleting the original):
                </p>
                <div className="rf-mono rf-muted rf-move-paths">
                  {installChanges
                    .slice(0, MAX_PROBLEMS)
                    .map(([what, path]) => `${what} ${path}`)
                    .join("\n")}
                  {installChanges.length > MAX_PROBLEMS && `\nand ${installChanges.length - MAX_PROBLEMS} more`}
                </div>
              </>
            )}
            {report.installBackup && (
              <p className="rf-move-note rf-muted">
                The original's version of each file the install changed or removed is saved in{" "}
                <span className="rf-mono">{report.installBackup}</span>.
              </p>
            )}
          </section>
        )}

        <section className="rf-section" aria-label="History">
          <h3 className="rf-section__label">History</h3>
          <p className="rf-move-note">
            {report.history.sessions > 0
              ? `${plural(report.history.sessions, "Claude session", "Claude sessions")} moved with it.`
              : "No Claude sessions to move."}
          </p>
        </section>

        {(report.warnings.length > 0 || report.linuxGit || report.droppedStreams.length > 0) && (
          <section className="rf-section" aria-label="Warnings">
            <h3 className="rf-section__label">Worth knowing</h3>
            <ul className="rf-move-warnings">
              {report.warnings.map((w) => (
                <li key={w}>{w}</li>
              ))}
              {report.linuxGit && <li>Git for Windows wasn't found, so Linux git read the original.</li>}
              {report.droppedStreams.length > 0 && (
                <li>
                  Not copied: {plural(report.droppedStreams.length, "alternate data stream", "alternate data streams")}
                  , such as {report.droppedStreams[0].path}:{report.droppedStreams[0].stream}.
                </li>
              )}
            </ul>
          </section>
        )}
      </div>
      <footer className="rf-move__actions rf-move__actions--column">
        {actions}
        {pending && (
          <p className="rf-hint" role="status">
            {pending}
          </p>
        )}
        {(error ?? note) && (
          <p className="rf-error" role="alert">
            {error ?? note}
          </p>
        )}
      </footer>
    </>
  );
}

/** Delete original went through (spec §6). */
function DoneView({
  outcome,
  dest,
  onClose,
}: {
  outcome: Extract<DeleteOutcome, { kind: "deleted" }>;
  dest: string;
  onClose(): void;
}) {
  return (
    <>
      <div className="rf-move__body">
        <section className="rf-section" aria-label="Done">
          <p className="rf-move-verdict rf-move-verdict--ok">
            {outcome.alreadyGone
              ? "The original folder was already gone. The move is finished."
              : outcome.permanent
                ? "The original was deleted permanently."
                : "The original is in the Recycle Bin."}
          </p>
          <dl className="rf-facts rf-move-facts">
            <dt>Now at</dt>
            <dd className="rf-mono">{dest}</dd>
            {outcome.shortcut && (
              <>
                <dt>Shortcut</dt>
                <dd className="rf-mono">{outcome.shortcut}</dd>
              </>
            )}
          </dl>
          {outcome.shortcutError && (
            <p className="rf-move-note rf-move-warn">No shortcut was made at the old place: {outcome.shortcutError}</p>
          )}
        </section>
      </div>
      <footer className="rf-move__actions">
        <button type="button" className="rf-button rf-button--accent" data-primary onClick={onClose}>
          Done
        </button>
      </footer>
    </>
  );
}
