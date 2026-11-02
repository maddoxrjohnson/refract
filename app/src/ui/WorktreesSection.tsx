import { type RefObject, useEffect, useLayoutEffect, useRef, useState } from "react";
import type { MissingWorktree, RepoSnapshot, SessionInfo, WorktreeCheck, WorktreeInfo, WorktreeJobDone } from "../backend/types";
import { ignoredList, missingWorktreeLabel, worktreeView } from "../worktrees";
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
  /** `ignored` is the list the confirm showed. Rejects with the helper's reason. */
  onRemove(path: string, ignored: string[]): Promise<void>;
  /** Reads a Windows-drive worktree's files; rejects with the helper's reason. */
  onCheck(path: string): Promise<WorktreeCheck>;
  /** The main repo's worktree entries whose folders are gone, listed after the live ones. */
  missing?: readonly MissingWorktree[];
  /** Drops a missing entry (its listed path); rejects with the helper's reason. */
  onPrune(path: string): Promise<void>;
  /** Hears each remove or prune job end (`worktreeJobDone`), also after its request timed out. */
  onJobDone?(cb: (done: WorktreeJobDone) => void): () => void;
  /** Hears the helper stop or restart, which ends its jobs without a `worktreeJobDone`. */
  onHelperGone?(cb: () => void): () => void;
}

const HELPER_RESTARTED = "The helper restarted; check the worktree again.";

/** What a remove or prune job left to say: still running past the wait, or why it failed. */
interface Notice {
  text: string;
  pending: boolean;
}

/** A removal that outlasted the wait; the helper carries on. */
function isPending(err: unknown): boolean {
  return typeof err === "object" && err !== null && (err as { kind?: unknown }).kind === "pending";
}

/** Whether focus went with something that unmounted, or is still in `el`, so moving it takes nothing from elsewhere. */
function focusLost(el: HTMLElement | null): boolean {
  const active = document.activeElement;
  return !active || active === document.body || (el?.contains(active) ?? false);
}

/** A notice and, once the job is over, Dismiss. */
function NoticeLine({ notice, name, onDismiss }: { notice: Notice; name?: string; onDismiss(): void }) {
  const text = name ? (
    <>
      <span className="rf-mono">{name}</span>: {notice.text}
    </>
  ) : (
    notice.text
  );
  if (notice.pending) {
    return (
      <p className="rf-hint" role="status">
        {text}
      </p>
    );
  }
  return (
    <div className="rf-worktree__notice">
      <p className="rf-error" role="alert">
        {text}
      </p>
      <button type="button" className="rf-link" aria-label={`Dismiss the notice about ${name ?? "this worktree"}`} onClick={onDismiss}>
        Dismiss
      </button>
    </div>
  );
}

/**
 * Tells the section when a row unmounts holding focus (or while its job ran, as a
 * disabled button may drop focus first), so the section can take it.
 */
function useFocusOnUnmount(el: RefObject<HTMLElement | null>, busy: boolean, onFocusLost: () => void) {
  const latest = useRef({ busy, onFocusLost });
  latest.current = { busy, onFocusLost };
  // A layout cleanup runs while the row is still in the document.
  useLayoutEffect(() => {
    const node = el.current;
    return () => {
      const active = document.activeElement;
      if ((node?.contains(active) ?? false) || (latest.current.busy && active === document.body)) latest.current.onFocusLost();
    };
  }, [el]);
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

/** Each row's part of the section's jobs: whether one runs, what it left to say, and its callbacks. */
interface JobProps {
  busy: boolean;
  notice: Notice | null;
  onDismiss(): void;
  onFocusLost(): void;
}

type RowProps = { repo: RepoSnapshot; onRemove(ignored: string[]): void } & JobProps &
  Pick<WorktreesSectionProps, "now" | "onOpen" | "onCheck">;

function WorktreeRow({ repo, now, onOpen, onRemove, onCheck, busy, notice, onDismiss, onFocusLost }: RowProps) {
  const [confirming, setConfirming] = useState(false);
  const [checking, setChecking] = useState(false);
  // A check's answer holds for the snapshot it was asked about; the next one has its own facts.
  const [checked, setChecked] = useState<{ info: WorktreeInfo; result: WorktreeCheck } | null>(null);
  const infoRef = useRef(repo.worktree);
  infoRef.current = repo.worktree;
  // The check's own failure; the remove's goes in the section's notice, which outlives the row.
  const [checkError, setCheckError] = useState<Notice | null>(null);
  const liRef = useRef<HTMLLIElement>(null);
  useFocusOnUnmount(liRef, busy, onFocusLost);
  // Opening or closing the confirm unmounts the focused button, so focus moves to Keep it
  // on open and back to Remove… on Keep it. When a check or the confirm's own going leaves
  // focus nowhere, it goes to Check again, or else Open.
  const keepRef = useRef<HTMLButtonElement>(null);
  const removeRef = useRef<HTMLButtonElement>(null);
  const checkRef = useRef<HTMLButtonElement>(null);
  const openRef = useRef<HTMLButtonElement>(null);
  const focusNext = useRef<"keep" | "remove" | "restore" | null>(null);
  const check = checked && checked.info === repo.worktree ? checked.result : null;
  const view = worktreeView(repo, check);
  const said = lastMessage(repo);
  const line = said ? firstLine(said.lastAssistantText) : null;
  // What the confirm lists, and what Remove sends for the helper to compare.
  const ignoredNow = check ? check.ignored : (repo.worktree?.ignored ?? []);
  const ignored = ignoredList(ignoredNow);
  const removable = view?.removable ?? false;
  // The confirm stays while its Remove runs; otherwise it goes when Remove is no longer offered.
  const showConfirm = confirming && (removable || busy);

  // A refused Remove says the facts moved; the check's answer may be what moved.
  useEffect(() => {
    if (notice && !notice.pending) setChecked(null);
  }, [notice]);
  useEffect(() => {
    if (confirming && !showConfirm) {
      focusNext.current = "restore";
      setConfirming(false);
    }
  }, [confirming, showConfirm]);
  useEffect(() => {
    const to = focusNext.current;
    if (!to) return;
    focusNext.current = null;
    if (to === "keep") keepRef.current?.focus();
    else if (to === "remove") removeRef.current?.focus();
    else if (focusLost(liRef.current)) (checkRef.current ?? openRef.current)?.focus();
  });

  const runCheck = () => {
    const info = repo.worktree;
    if (!info) return;
    setChecking(true);
    setCheckError(null);
    onDismiss();
    onCheck(repo.path).then(
      (result) => {
        setChecking(false);
        setChecked({ info, result });
        // Straight to the confirm when nothing can be lost, unless a new snapshot came meanwhile.
        if (result.removable && infoRef.current === info) {
          focusNext.current = "keep";
          setConfirming(true);
        } else {
          focusNext.current = "restore";
        }
      },
      (err: unknown) => {
        setChecking(false);
        focusNext.current = "restore";
        setCheckError({ text: describeError(err), pending: isPending(err) });
      },
    );
  };

  return (
    <li ref={liRef} className="rf-worktree" aria-label={repo.name}>
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
      {view?.agentOpen && (
        <p className="rf-hint">
          {repo.worktree?.checkable && !check ? "Close the agent here to check its files" : "Close the agent here to remove"}
        </p>
      )}
      {checking && (
        <p className="rf-hint" role="status">
          Checking files on the Windows drive…
        </p>
      )}
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
      {showConfirm ? (
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
            <button
              type="button"
              className="rf-button rf-button--danger"
              disabled={busy}
              onClick={() => {
                setCheckError(null);
                onRemove([...ignoredNow]);
              }}
            >
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
                onDismiss();
              }}
            >
              Keep it
            </button>
          </div>
        </div>
      ) : (
        <div className="rf-row">
          <button
            ref={openRef}
            type="button"
            className="rf-button"
            aria-label={`Open ${repo.name}`}
            onClick={() => onOpen(repo.path)}
          >
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
                setCheckError(null);
                onDismiss();
                setConfirming(true);
              }}
            >
              Remove…
            </button>
          )}
          {view?.checkable && (
            <button
              ref={checkRef}
              type="button"
              className="rf-button"
              aria-label={check ? `Check ${repo.name} again` : `Check ${repo.name}…`}
              disabled={checking}
              onClick={runCheck}
            >
              {check ? "Check again" : "Check…"}
            </button>
          )}
        </div>
      )}
      {checkError &&
        (checkError.pending ? (
          <p className="rf-hint" role="status">
            {checkError.text}
          </p>
        ) : (
          <p className="rf-error" role="alert">
            {checkError.text}
          </p>
        ))}
      {notice && (
        <NoticeLine
          notice={notice}
          onDismiss={() => {
            focusNext.current = "restore";
            onDismiss();
          }}
        />
      )}
    </li>
  );
}


function folderName(path: string): string {
  return path.replace(/\/+$/, "").split("/").pop() || path;
}

/** A worktree git still lists whose folder is gone (spec §10.2), with Clean up when that is safe. */
function MissingRow({
  entry,
  onPrune,
  busy,
  notice,
  onDismiss,
  onFocusLost,
}: { entry: MissingWorktree; onPrune(): void } & JobProps) {
  const [confirming, setConfirming] = useState(false);
  const liRef = useRef<HTMLLIElement>(null);
  useFocusOnUnmount(liRef, busy, onFocusLost);
  // Focus moves as in a worktree row: to Keep it on open, back to Clean up… on Keep it.
  const keepRef = useRef<HTMLButtonElement>(null);
  const cleanRef = useRef<HTMLButtonElement>(null);
  const focusNext = useRef<"keep" | "clean" | "restore" | null>(null);
  const name = folderName(entry.path);
  // As in a worktree row, the confirm stays while Clean up runs.
  const showConfirm = confirming && (entry.cleanable || busy);

  useEffect(() => {
    if (confirming && !showConfirm) {
      focusNext.current = "restore";
      setConfirming(false);
    }
  }, [confirming, showConfirm]);
  useEffect(() => {
    const to = focusNext.current;
    if (!to) return;
    focusNext.current = null;
    if (to === "keep") keepRef.current?.focus();
    else if (to === "clean") cleanRef.current?.focus();
    else if (focusLost(liRef.current)) {
      if (cleanRef.current) cleanRef.current.focus();
      else onFocusLost();
    }
  });

  return (
    <li ref={liRef} className="rf-worktree" aria-label={name}>
      <div className="rf-worktree__head">
        <span className="rf-mono rf-muted rf-worktree__name">{entry.path}</span>
        <span className="rf-mono rf-muted rf-worktree__branch">{entry.branch ?? "(detached)"}</span>
      </div>
      <p className={`rf-worktree__status rf-worktree__status--${entry.cleanable ? "done" : "keep"}`}>
        {missingWorktreeLabel(entry.status)}
      </p>
      {entry.status.kind === "moved" && (
        <p className="rf-hint">
          Run <span className="rf-mono">git worktree repair</span> there
        </p>
      )}
      {showConfirm ? (
        <div className="rf-refactor__confirm" role="group" aria-label={`Confirm cleaning up ${name}`}>
          <p className="rf-refactor__question">
            Forget this worktree? Only git&apos;s record of it goes
            {entry.branch ? (
              <>
                ; branch <span className="rf-mono">{entry.branch}</span> stays.
              </>
            ) : (
              "."
            )}
          </p>
          <div className="rf-row">
            <button type="button" className="rf-button rf-button--danger" disabled={busy} onClick={onPrune}>
              {busy ? "Cleaning up…" : "Clean up"}
            </button>
            <button
              ref={keepRef}
              type="button"
              className="rf-button"
              disabled={busy}
              onClick={() => {
                focusNext.current = "clean";
                setConfirming(false);
                onDismiss();
              }}
            >
              Keep it
            </button>
          </div>
        </div>
      ) : (
        entry.cleanable && (
          <div className="rf-row">
            <button
              ref={cleanRef}
              type="button"
              className="rf-button"
              aria-label={`Clean up ${name}…`}
              onClick={() => {
                focusNext.current = "keep";
                onDismiss();
                setConfirming(true);
              }}
            >
              Clean up…
            </button>
          </div>
        )
      )}
      {notice && (
        <NoticeLine
          notice={notice}
          onDismiss={() => {
            focusNext.current = "restore";
            onDismiss();
          }}
        />
      )}
    </li>
  );
}

/**
 * The detail panel's Worktrees section (worktrees spec §1): one row per linked worktree
 * folded into the gem, with its status, the last thing its agent said, Open and, when
 * nothing can be lost, Remove (or Check… first on a Windows drive). Then the entries
 * whose folders are gone, with Clean up (§10.2).
 *
 * The section, not the row, holds each remove and prune job, keyed by path: a half-removed
 * worktree's row goes with its `repoRemoved` before its refusal can be read, and a job that
 * outlasts its request ends only with `worktreeJobDone`, or with the helper going. A job
 * that worked stays busy until its row goes, so its confirm is not offered again meanwhile.
 * A notice whose row is gone shows under the list, and keeps the section up until it is
 * dismissed.
 */
export function WorktreesSection({
  worktrees,
  now,
  onOpen,
  onRemove,
  onCheck,
  missing = [],
  onPrune,
  onJobDone,
  onHelperGone,
}: WorktreesSectionProps) {
  const [busy, setBusy] = useState<ReadonlySet<string>>(() => new Set());
  // Read by the job's callbacks, which outlive the render that started them.
  const busyRef = useRef(busy);
  // Jobs past their request's wait, which only `worktreeJobDone` or the helper going ends.
  const waiting = useRef(new Set<string>());
  // Jobs that worked, kept busy until their row goes.
  const finished = useRef(new Set<string>());
  // The paths with a row, as last rendered.
  const rowsRef = useRef<ReadonlySet<string>>(new Set());
  const [notices, setNotices] = useState<ReadonlyMap<string, Notice>>(() => new Map());
  const headingRef = useRef<HTMLHeadingElement>(null);
  // A row that held focus went, or its notice was dismissed: the heading takes focus.
  const refocus = useRef(false);
  useEffect(() => {
    if (!refocus.current) return;
    refocus.current = false;
    if (focusLost(null)) headingRef.current?.focus();
  });

  const setRunning = (path: string, on: boolean) => {
    const next = new Set(busyRef.current);
    if (on) next.add(path);
    else next.delete(path);
    busyRef.current = next;
    setBusy(next);
  };
  const setNotice = (path: string, notice: Notice | null) =>
    setNotices((prev) => {
      if (!notice && !prev.has(path)) return prev;
      const next = new Map(prev);
      if (notice) next.set(path, notice);
      else next.delete(path);
      return next;
    });

  useEffect(
    () =>
      onJobDone?.((done) => {
        // A job this section did not start, or whose reply already came.
        if (!busyRef.current.has(done.path) || finished.current.has(done.path)) return;
        waiting.current.delete(done.path);
        setNotice(done.path, done.error ? { text: done.error, pending: false } : null);
        // Gone with its `repoRemoved` (or the main repo's `repoChanged`), unless already.
        if (done.error || !rowsRef.current.has(done.path)) setRunning(done.path, false);
        else finished.current.add(done.path);
      }),
    [onJobDone],
  );
  useEffect(
    () =>
      onHelperGone?.(() => {
        for (const path of waiting.current) {
          setRunning(path, false);
          setNotice(path, { text: HELPER_RESTARTED, pending: false });
        }
        waiting.current.clear();
      }),
    [onHelperGone],
  );

  const run = (path: string, job: () => Promise<void>) => {
    finished.current.delete(path);
    setRunning(path, true);
    setNotice(path, null);
    job().then(
      // On success the row goes with the worktree's `repoRemoved` (or the main repo's
      // `repoChanged`), and the job with its `worktreeJobDone`.
      () => {},
      (err: unknown) => {
        const pending = isPending(err);
        // Past the wait, unless its `worktreeJobDone` came first.
        if (pending && (!busyRef.current.has(path) || finished.current.has(path))) return;
        setNotice(path, { text: describeError(err), pending });
        if (pending) waiting.current.add(path);
        else setRunning(path, false);
      },
    );
  };

  const shown = new Set([...worktrees.map((w) => w.path), ...missing.map((m) => m.path)]);
  rowsRef.current = shown;
  const shownKey = [...shown].join("\n");
  // A job that worked ends once its row is gone.
  useEffect(() => {
    const rows = new Set(shownKey.split("\n"));
    for (const path of finished.current) {
      if (rows.has(path)) continue;
      finished.current.delete(path);
      setRunning(path, false);
    }
  }, [shownKey]);
  const orphans = [...notices].filter(([path]) => !shown.has(path));
  if (worktrees.length === 0 && missing.length === 0 && orphans.length === 0) return null;
  const jobProps = (path: string): JobProps => ({
    busy: busy.has(path),
    notice: notices.get(path) ?? null,
    onDismiss: () => setNotice(path, null),
    onFocusLost: () => {
      refocus.current = true;
    },
  });
  return (
    <section className="rf-section rf-worktrees" aria-label="Worktrees">
      <h3 ref={headingRef} tabIndex={-1} className="rf-section__label">
        Worktrees
      </h3>
      <ul className="rf-worktrees__list">
        {worktrees.map((w) => (
          <WorktreeRow
            key={w.path}
            repo={w}
            now={now}
            onOpen={onOpen}
            onRemove={(ignored) => run(w.path, () => onRemove(w.path, ignored))}
            onCheck={onCheck}
            {...jobProps(w.path)}
          />
        ))}
        {missing.map((m) => (
          <MissingRow key={m.path} entry={m} onPrune={() => run(m.path, () => onPrune(m.path))} {...jobProps(m.path)} />
        ))}
      </ul>
      {orphans.map(([path, notice]) => (
        <NoticeLine
          key={path}
          notice={notice}
          name={folderName(path)}
          onDismiss={() => {
            refocus.current = true;
            setNotice(path, null);
          }}
        />
      ))}
    </section>
  );
}
