import { useId } from "react";
import type { LaunchMode, RepoSnapshot, SessionInfo } from "../backend/types";
import { hostLabel, remoteUrl } from "../repoStatus";
import { firstLine, relativeTime, sessionStateLabel } from "./format";
import { newAgentMode, resumableSession } from "./navigation";
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

/** The panel docked on the right while a gem is zoomed (spec §3.4, without PRs/CI and Hide). */
export function DetailPanel({
  repo,
  now,
  onPrimary,
  onLaunch,
  onOpenFolder,
  onOpenUrl,
  onClose,
}: DetailPanelProps) {
  const hintId = useId();
  const direct = repo.preselect.kind === "noPicker";
  const resume = resumableSession(repo);
  const session = lastSession(repo);
  const sessionLine = session ? firstLine(session.lastAssistantText) : null;
  const url = remoteUrl(repo);
  const host = hostLabel(repo);

  return (
    <aside className="rf-glass rf-detail" aria-label={`${repo.name} details`}>
      <header className="rf-detail__header">
        <div className="rf-detail__title">
          <h2 className="rf-detail__name">{repo.name}</h2>
          <div className="rf-muted">
            {host} · {repo.language ?? "Unknown language"}
          </div>
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
            </>
          ) : (
            <div className="rf-muted">No agent sessions yet</div>
          )}
        </section>
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
        <p id={hintId} className="rf-hint">
          {direct
            ? "Enter starts a new agent in VS Code."
            : "Enter opens a picker: resume a session or start fresh."}
        </p>
        {!direct && (
          <div className="rf-row">
            {resume && (
              <button
                type="button"
                className="rf-button"
                onClick={() => onLaunch({ kind: "resume", sessionId: resume.id })}
              >
                Resume
              </button>
            )}
            <button type="button" className="rf-button" onClick={() => onLaunch(newAgentMode(repo))}>
              New agent
            </button>
          </div>
        )}
        <div className="rf-row">
          <button type="button" className="rf-button" onClick={onOpenFolder}>
            Open folder
          </button>
          {url && (
            <button type="button" className="rf-button" onClick={() => onOpenUrl(url)}>
              Open on {host}
            </button>
          )}
        </div>
      </div>
    </aside>
  );
}
