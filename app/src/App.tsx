import { useEffect, useReducer, useRef, useState } from "react";
import type { Backend } from "./backend/Backend";
import type {
  AgentRequestError,
  AgentStatus,
  LaunchError,
  LaunchMode,
  LaunchStep,
  RepoSnapshot,
} from "./backend/types";
import { useVisibility } from "./hooks/useVisibility";
import { initialRepoState, repoReducer } from "./repoState";

// Placeholder interface for the shell milestone: the 3D scene replaces it later.

function describeStatus(status: AgentStatus | null): string {
  if (!status) return "connecting…";
  switch (status.state) {
    case "starting":
      return "starting";
    case "running":
      return "running";
    case "restarting":
      return (
        `restarting (attempt ${status.attempt}, retry in ${Math.round(status.retryInMs / 1000)} s)` +
        (status.lastError ? `: ${status.lastError}` : "")
      );
    case "stopped":
      return "stopped";
  }
}

const STEP_LABELS: Record<LaunchStep, string> = {
  setup: "setting up the VS Code profile…",
  server: "installing the extension in WSL…",
  prepare: "preparing the launch…",
  opening: "opening VS Code…",
  done: "opened",
};

function describeError(err: unknown): string {
  if (typeof err === "string") return err;
  const e = err as Partial<AgentRequestError> | Partial<LaunchError> | null;
  if (e && e.kind === "agent" && "code" in e) return `${e.message} (${e.code})`;
  if (e && "message" in e && typeof e.message === "string") return e.message;
  if (e && e.kind) return e.kind;
  return String(err);
}

function sortRepos(repos: Iterable<RepoSnapshot>): RepoSnapshot[] {
  return [...repos].sort((a, b) => a.name.localeCompare(b.name));
}

/** Counts animation frames while visible, to show that drawing stops when hidden. */
function FrameCounter({ visible }: { visible: boolean }) {
  const frames = useRef(0);
  const label = useRef<HTMLSpanElement>(null);
  useEffect(() => {
    if (!visible) return;
    let handle = 0;
    const tick = () => {
      frames.current += 1;
      if (label.current) label.current.textContent = String(frames.current);
      handle = requestAnimationFrame(tick);
    };
    handle = requestAnimationFrame(tick);
    return () => cancelAnimationFrame(handle);
  }, [visible]);
  return (
    <p className="muted">
      frames drawn: <span ref={label}>{frames.current}</span>
    </p>
  );
}

export function App({ backend }: { backend: Backend }) {
  const visible = useVisibility(backend);
  const [status, setStatus] = useState<AgentStatus | null>(null);
  const [{ repos, loading }, dispatch] = useReducer(repoReducer, initialRepoState);
  const [error, setError] = useState<string | null>(null);
  const [launchStatus, setLaunchStatus] = useState<string | null>(null);

  useEffect(
    () => backend.onLaunchProgress((step) => setLaunchStatus(STEP_LABELS[step])),
    [backend],
  );

  const launch = (repo: RepoSnapshot, mode: LaunchMode) => {
    setLaunchStatus(`${repo.name}: starting…`);
    backend.launch(repo.path, mode).then(
      () => setLaunchStatus(`${repo.name}: opened`),
      (err: unknown) => setLaunchStatus(`${repo.name}: ${describeError(err)}`),
    );
  };

  useEffect(() => {
    let live = true;
    const stop = backend.onAgentStatus((s) => live && setStatus(s));
    backend.agentStatus().then(
      (s) => live && setStatus((current) => current ?? s),
      (err: unknown) => live && setError(describeError(err)),
    );
    return () => {
      live = false;
      stop();
    };
  }, [backend]);

  // Counts shows. WebView2 is suspended while hidden, so events can be held back or
  // lost; each show refetches.
  const [shown, setShown] = useState(0);
  useEffect(() => {
    if (visible) setShown((n) => n + 1);
  }, [visible]);

  // Refetch whenever the helper (re)enters running: a restarted helper starts empty.
  const running = status?.state === "running";
  useEffect(() => {
    if (!running) return;
    let live = true;
    dispatch({ type: "fetchStarted" });
    backend.listRepos().then(
      (list) => {
        if (!live) return;
        dispatch({ type: "listed", list });
        setError(null);
      },
      (err: unknown) => live && setError(describeError(err)),
    );
    return () => {
      live = false;
    };
  }, [backend, running, shown]);

  useEffect(() => backend.onAgentEvent((event) => dispatch({ type: "event", event })), [backend]);

  return (
    <main>
      <h1>Refract</h1>
      <p>
        Agent: <strong>{describeStatus(status)}</strong>
      </p>
      {error && <p className="error">{error}</p>}
      {launchStatus && <p>Launch: {launchStatus}</p>}
      <FrameCounter visible={visible} />
      <h2>
        Repos ({repos.size}
        {loading ? ", loading…" : ""})
      </h2>
      <ul className="repos">
        {sortRepos(repos.values()).map((repo) => {
          // The session the picker would preselect, when it would resume one.
          const resume = repo.preselect.kind === "resume" ? repo.preselect.sessionId : null;
          return (
            <li key={repo.path}>
              <span className="name">{repo.name}</span>
              <span className="muted">{repo.branch ?? "detached"}</span>
              {repo.changedFiles > 0 && <span className="dirty">{repo.changedFiles} changed</span>}
              <span className="path">{repo.path}</span>
              <button type="button" onClick={() => launch(repo, { kind: "new" })}>
                New agent
              </button>
              {resume && (
                <button
                  type="button"
                  onClick={() => launch(repo, { kind: "resume", sessionId: resume })}
                >
                  Resume
                </button>
              )}
            </li>
          );
        })}
      </ul>
    </main>
  );
}
