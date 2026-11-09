import { useId, useState, type ReactNode } from "react";
import type {
  CiStatus,
  CodexReadiness,
  LaunchMode,
  RemoteKind,
  RemoteRepo,
  RepoSnapshot,
  SessionInfo,
} from "../backend/types";
import type { RemoteDetailsState } from "../hooks/useRemoteDetails";
import { hostLabel, remoteUrl, repoStatus, unfinishedSessions } from "../repoStatus";
import { StatusLine } from "./FocusLabel";
import { firstLine, relativeTime, sessionStateLabel } from "./format";
import { agentName, focusableSession, newAgentMode, resumableSession, resumeMode } from "./navigation";
import "./ui.css";

export interface DetailPanelProps {
  repo: RepoSnapshot;
  /** Current time in milliseconds, for relative times. */
  now: number;
  /** What Enter does: dispatch navigation's `{ type: "launch" }`. */
  onPrimary(): void;
  /** Start directly in a given mode (the New agent and Resume buttons). */
  onLaunch(mode: LaunchMode): void;
  onOpenFolder(): void;
  onOpenUrl(url: string): void;
  onClose(): void;
  /** Hides the gem; settings can bring it back. */
  onHide(): void;
  /** The repo on GitHub or GitLab, when it is listed there. */
  remote?: RemoteRepo | null;
  /** Not cloned yet: the primary action clones and launches (spec §4.4). */
  remoteOnly?: boolean;
  /** Pull or merge requests and CI; `null` when the repo has no remote to ask. */
  details?: RemoteDetailsState | null;
  /** Fetches the details again, past the cache. */
  onRefreshDetails?(): void;
  /** Move to WSL, for a cloned repo on a Windows drive (spec §2.1). */
  move?: ReactNode;
  /** The Refactor section of a cloned repo (refactor spec §4). */
  refactor?: ReactNode;
  /** Linked worktrees folded into the gem: their agents count in the status line. */
  worktrees?: readonly RepoSnapshot[];
  /** The Worktrees section, after Working tree (worktrees spec §1). */
  worktreeSection?: ReactNode;
  /** Make Codex-ready: launches an agent that fixes the instructions (codex spec §9). */
  onMakeCodexReady?(): void;
  /** A launch is under way, so Make Codex-ready waits. */
  launching?: boolean;
}

const CI_LABELS: Record<CiStatus, string> = {
  success: "Passing",
  failure: "Failing",
  pending: "Running",
  none: "No CI",
};

const KIND_LABELS: Record<RemoteKind, string> = { github: "GitHub", gitlab: "GitLab" };

function plural(n: number, one: string, many: string): string {
  return `${n} ${n === 1 ? one : many}`;
}

/** Open pull or merge requests and CI on the default branch (spec §3.4). */
function RemoteSection({
  kind,
  state,
  now,
  onOpenUrl,
  onRefresh,
}: {
  kind: RemoteKind;
  state: RemoteDetailsState;
  now: number;
  onOpenUrl(url: string): void;
  onRefresh?(): void;
}) {
  const requests =
    kind === "github" ? ["pull request", "pull requests"] : ["merge request", "merge requests"];
  const title = kind === "github" ? "Pull requests and CI" : "Merge requests and CI";
  const d = state.details;
  return (
    <section className="rf-section" aria-label={title} aria-busy={state.status === "loading"}>
      <h3 className="rf-section__label">{title}</h3>
      {d ? (
        <>
          <dl className="rf-facts">
            <dt>Open</dt>
            <dd>
              {d.openPullRequests === null
                ? "Unknown"
                : d.openPullRequests === 0
                  ? `No open ${requests[1]}`
                  : plural(d.openPullRequests, requests[0], requests[1])}
            </dd>
            <dt>CI</dt>
            <dd className="rf-ci">
              <span className={`rf-ci__status rf-ci__status--${d.ci ?? "none"}`}>
                {d.ci ? CI_LABELS[d.ci] : "Unknown"}
              </span>
              {d.ciUrl && (
                <button type="button" className="rf-link" onClick={() => onOpenUrl(d.ciUrl!)}>
                  View run
                </button>
              )}
            </dd>
          </dl>
          <div className="rf-updated rf-muted">
            <span>
              {state.status === "loading"
                ? "Updating…"
                : `Updated ${relativeTime(Date.parse(d.fetchedAt), now)}`}
            </span>
            {onRefresh && state.status !== "loading" && (
              <button type="button" className="rf-link" onClick={onRefresh}>
                Refresh
              </button>
            )}
          </div>
        </>
      ) : (
        state.status === "loading" && <div className="rf-muted">Loading…</div>
      )}
      {state.status === "error" && (
        <p className="rf-error" role="alert">
          {state.message}
          {onRefresh && (
            <>
              {" "}
              <button type="button" className="rf-link" onClick={onRefresh}>
                Try again
              </button>
            </>
          )}
        </p>
      )}
    </section>
  );
}

/** The Codex line: "Codex: Ready", "Codex: 2 warnings", "Codex: Not ready" or "Codex: Busy (Claude working)". */
export function codexLine(codex: CodexReadiness | undefined): string {
  if (!codex) return "Codex not installed";
  switch (codex.state) {
    case "ready":
      return "Codex: Ready";
    case "warnings":
      return `Codex: ${plural(codex.findings.length, "warning", "warnings")}`;
    case "notReady":
      return "Codex: Not ready";
    case "busy": {
      const gate = codex.gates.codex;
      if (gate?.kind !== "busy") return "Codex: Busy";
      const doing = gate.state === "waitingOnUser" ? "waiting on you" : "working";
      return `Codex: Busy (${agentName(gate.agent)} ${doing})`;
    }
  }
}

/** Whether the repo is ready for Codex, its findings and Make Codex-ready (codex spec §10). */
function CodexSection({
  codex,
  launching,
  onMakeReady,
}: {
  codex: CodexReadiness | undefined;
  launching: boolean;
  onMakeReady?(): void;
}) {
  const listId = useId();
  const [open, setOpen] = useState(false);
  const findings = codex?.findings ?? [];
  const fixable = codex?.state === "notReady" || codex?.state === "warnings";
  return (
    <section className="rf-section rf-codex" aria-label="Codex">
      <div className={codex ? undefined : "rf-muted"}>{codexLine(codex)}</div>
      {findings.length > 0 && (
        <>
          <button
            type="button"
            className="rf-link"
            aria-expanded={open}
            aria-controls={listId}
            onClick={() => setOpen((o) => !o)}
          >
            {open ? "Hide findings" : `Show ${plural(findings.length, "finding", "findings")}`}
          </button>
          {open && (
            <ul id={listId} className="rf-findings">
              {findings.map((f, i) => (
                <li key={i}>
                  <span className="rf-mono">
                    {f.file}
                    {f.line !== null ? `:${f.line}` : ""}
                  </span>{" "}
                  {f.text}
                </li>
              ))}
            </ul>
          )}
        </>
      )}
      {fixable && onMakeReady && (
        <div className="rf-row">
          <button type="button" className="rf-button" disabled={launching} onClick={onMakeReady}>
            Make Codex-ready
          </button>
        </div>
      )}
    </section>
  );
}

function upstream(repo: RepoSnapshot): string {
  if (repo.ahead === null || repo.behind === null) return "No upstream";
  if (repo.ahead === 0 && repo.behind === 0) return "Up to date";
  const parts = [];
  if (repo.ahead) parts.push(`${repo.ahead} ahead`);
  if (repo.behind) parts.push(`${repo.behind} behind`);
  return parts.join(", ");
}

function lastSession(repo: RepoSnapshot): SessionInfo | null {
  let best: SessionInfo | null = null;
  for (const s of repo.sessions) {
    if (!best || (s.lastTimestamp ?? "") > (best.lastTimestamp ?? "")) best = s;
  }
  return best;
}

/** The panel docked on the right while a gem is zoomed (spec §3.4, §4.4). */
export function DetailPanel({
  repo,
  now,
  onPrimary,
  onLaunch,
  onOpenFolder,
  onOpenUrl,
  onClose,
  onHide,
  remote = null,
  remoteOnly = false,
  details = null,
  onRefreshDetails,
  move = null,
  refactor = null,
  worktrees,
  worktreeSection = null,
  onMakeCodexReady,
  launching = false,
}: DetailPanelProps) {
  const hintId = useId();
  const direct = repo.preselect.kind === "noPicker";
  const resume = resumableSession(repo);
  const running = focusableSession(repo);
  const session = lastSession(repo);
  const sessionLine = session ? firstLine(session.lastAssistantText) : null;
  // Sessions the picker offers to resume besides the one shown here.
  const moreToResume = unfinishedSessions(repo).filter((s) => s.id !== session?.id).length;
  const url = remote?.webUrl ?? remoteUrl(repo);
  const host = remote ? KIND_LABELS[remote.kind] : hostLabel(repo);
  const kind: RemoteKind = remote?.kind ?? (repo.remote?.host === "github.com" ? "github" : "gitlab");
  const remoteSection = details && (
    <RemoteSection
      kind={kind}
      state={details}
      now={now}
      onOpenUrl={onOpenUrl}
      onRefresh={onRefreshDetails}
    />
  );
  const secondary = (
    <div className="rf-row">
      {!remoteOnly && (
        <button type="button" className="rf-button" onClick={onOpenFolder}>
          Open folder
        </button>
      )}
      {url && (
        <button type="button" className="rf-button" onClick={() => onOpenUrl(url)}>
          Open on {host}
        </button>
      )}
      <button type="button" className="rf-button" onClick={onHide}>
        Hide
      </button>
    </div>
  );

  const header = (
    <header className="rf-detail__header">
      <div className="rf-detail__title">
        <h2 className="rf-detail__name">{repo.name}</h2>
        <div className="rf-muted">
          {host} · {repo.language ?? "Unknown language"}
        </div>
        <StatusLine repo={repo} notCloned={remoteOnly} worktrees={worktrees} />
      </div>
      <button
        type="button"
        className="rf-button rf-button--ghost rf-icon-button"
        aria-label="Close details (Escape)"
        onClick={onClose}
      >
        ×
      </button>
    </header>
  );

  if (remoteOnly) {
    return (
      <aside className="rf-glass rf-detail" aria-label={`${repo.name} details`}>
        {header}
        <div className="rf-detail__body">
          <section className="rf-section" aria-label="Remote repository">
            <p className="rf-badges">
              <span className="rf-badge">Not cloned</span>
              {remote?.private && <span className="rf-badge">Private</span>}
              {remote?.fork && <span className="rf-badge">Fork</span>}
              {remote?.archived && <span className="rf-badge">Archived</span>}
            </p>
            {remote?.description && <p className="rf-description">{remote.description}</p>}
            <dl className="rf-facts">
              <dt>Owner</dt>
              <dd className="rf-mono">{repo.remote?.owner}</dd>
              <dt>Default branch</dt>
              <dd className="rf-mono">{repo.branch ?? "None"}</dd>
              <dt>Last push</dt>
              <dd>{remote?.pushedAt ? relativeTime(Date.parse(remote.pushedAt), now) : "Never"}</dd>
            </dl>
          </section>
          {remoteSection}
          <section className="rf-section" aria-label="Refactor">
            <h3 className="rf-section__label">Refactor</h3>
            <p className="rf-hint">Clone it first. Refactors work on a local copy.</p>
          </section>
        </div>
        <div className="rf-detail__actions">
          <button
            type="button"
            className="rf-button rf-button--primary"
            aria-describedby={hintId}
            onClick={onPrimary}
          >
            <span>Clone and launch</span>
            <kbd className="rf-kbd" aria-hidden="true">
              Enter
            </kbd>
          </button>
          <p id={hintId} className="rf-hint">
            Enter clones into <span className="rf-mono">~/projects/{repo.name}</span> in WSL and starts a
            new agent.
          </p>
          {secondary}
        </div>
      </aside>
    );
  }

  return (
    <aside className="rf-glass rf-detail" aria-label={`${repo.name} details`}>
      {header}

      <div className="rf-detail__body">
        <section className="rf-section" aria-label="Working tree">
          <dl className="rf-facts">
            <dt>Branch</dt>
            <dd className="rf-mono">{repo.branch ?? "detached HEAD"}</dd>
            <dt>Upstream</dt>
            <dd className="rf-mono">{upstream(repo)}</dd>
            <dt>Changes</dt>
            <dd className="rf-mono">
              {repo.changedFiles === 0
                ? "Clean"
                : `${repo.changedFiles} changed ${repo.changedFiles === 1 ? "file" : "files"}`}
            </dd>
          </dl>
        </section>

        {worktreeSection}

        <CodexSection codex={repo.codex} launching={launching} onMakeReady={onMakeCodexReady} />

        {move}

        {refactor}

        <section className="rf-section">
          <h3 className="rf-section__label">Last commit</h3>
          {repo.lastCommit ? (
            <>
              <div>{repo.lastCommit.summary}</div>
              <div className="rf-muted">{relativeTime(repo.lastCommit.time * 1000, now)}</div>
            </>
          ) : (
            <div className="rf-muted">No commits yet</div>
          )}
        </section>

        <section className="rf-section">
          <h3 className="rf-section__label">Last agent session</h3>
          {session ? (
            <>
              <div>
                {sessionStateLabel(session.state)}
                {session.lastTimestamp && (
                  <span className="rf-muted">
                    {" · "}
                    {relativeTime(Date.parse(session.lastTimestamp), now)}
                  </span>
                )}
              </div>
              {sessionLine && <p className="rf-quote">{sessionLine}</p>}
              {moreToResume > 0 && (
                <div className="rf-muted">
                  {moreToResume === 1 ? "1 more to resume" : `${moreToResume} more to resume`}
                </div>
              )}
            </>
          ) : (
            <div className="rf-muted">No agent sessions yet</div>
          )}
        </section>

        {remoteSection}
      </div>

      <div className="rf-detail__actions">
        <button
          type="button"
          className="rf-button rf-button--primary"
          aria-describedby={hintId}
          onClick={onPrimary}
        >
          <span>{direct ? "New agent" : "Launch…"}</span>
          <kbd className="rf-kbd" aria-hidden="true">
            Enter
          </kbd>
        </button>
        <p
          id={hintId}
          className={`rf-hint${running && repoStatus(repo).waiting ? " rf-hint--attention" : ""}`}
        >
          {direct
            ? "Enter starts a new agent in VS Code."
            : running
              ? "Enter opens a picker: go to the running agent, resume a session or start fresh."
              : "Enter opens a picker: resume a session or start fresh."}
        </p>
        {!direct && (
          <div className="rf-row">
            {running && (
              <button
                type="button"
                className="rf-button"
                onClick={() => onLaunch({ kind: "focus", sessionId: running.id })}
              >
                Go to agent
              </button>
            )}
            {resume && (
              <button
                type="button"
                className="rf-button"
                onClick={() => onLaunch(resumeMode(resume))}
              >
                Resume
              </button>
            )}
            <button type="button" className="rf-button" onClick={() => onLaunch(newAgentMode(repo))}>
              New agent
            </button>
          </div>
        )}
        {secondary}
      </div>
    </aside>
  );
}
