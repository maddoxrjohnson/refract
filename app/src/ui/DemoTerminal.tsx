import { useEffect, useRef, useState } from "react";
import type { Backend, DemoLaunch } from "../backend/Backend";
import "./ui.css";

/** Milliseconds per typed character of the command. */
const TYPE_MS = 28;
/** Pause between the command and each line of the reply. */
const LINE_MS = 260;

/**
 * The command the VS Code extension would run, quoted the way a shell would show it.
 * Going to a running agent runs nothing, so its command is empty.
 */
export function demoCommand(launch: DemoLaunch): string {
  if (launch.mode.kind === "focus") return "";
  if (launch.mode.kind === "resume") return `claude --resume ${launch.sessionId}`;
  const prompt = launch.mode.prompt ? ` '${launch.mode.prompt.replace(/'/g, "'\\''")}'` : "";
  return `claude --session-id ${launch.sessionId}${prompt}`;
}

/** An invented reply; nothing here comes from a real agent. */
function demoReply(launch: DemoLaunch): string[] {
  const header = ["✻ Claude Code (demo)", `  cwd: ~/projects/${launch.repoName}`, ""];
  if (launch.mode.kind === "focus") {
    return [
      ...header,
      `● Switched to the agent already running in ${launch.repoName}.`,
      "  Nothing new was started; it is where you left it.",
    ];
  }
  if (launch.mode.kind === "resume") {
    return [
      ...header,
      `● Picking up the session that stopped in ${launch.repoName}.`,
      "  I'll check where it got to before running anything again.",
    ];
  }
  if (launch.mode.prompt) {
    return [
      ...header,
      `> ${launch.mode.prompt}`,
      "",
      "● Reading git diff to see what was left unfinished…",
    ];
  }
  return [...header, `● Ready in ${launch.repoName}. What should we work on?`];
}

/**
 * Demo only (spec §8): stands in for the VS Code window a launch would open, typing the
 * `claude` command and an invented greeting. Shown only by backends with `onDemoLaunch`.
 */
export function DemoTerminal({ backend }: { backend: Backend }) {
  const [launch, setLaunch] = useState<DemoLaunch | null>(null);
  const [typed, setTyped] = useState(0);
  const [lines, setLines] = useState(0);
  const closeButton = useRef<HTMLButtonElement>(null);

  useEffect(() => {
    if (!backend.onDemoLaunch) return;
    return backend.onDemoLaunch((next) => {
      setLaunch(next);
      setTyped(0);
      setLines(0);
    });
  }, [backend]);

  const command = launch ? demoCommand(launch) : "";
  const reply = launch ? demoReply(launch) : [];

  // Type the command a character at a time, then reveal the reply a line at a time.
  useEffect(() => {
    if (!launch) return;
    const timer =
      typed < command.length
        ? setTimeout(() => setTyped((n) => n + 1), TYPE_MS)
        : lines < reply.length
          ? setTimeout(() => setLines((n) => n + 1), lines === 0 ? LINE_MS * 2 : LINE_MS / 3)
          : undefined;
    return () => clearTimeout(timer);
  }, [launch, typed, lines, command.length, reply.length]);

  // While open, the terminal owns the keyboard: Escape closes it and nothing reaches the
  // scene's filter or navigation (a capture listener on window runs before theirs).
  useEffect(() => {
    if (!launch) return;
    closeButton.current?.focus();
    const onKey = (ev: KeyboardEvent) => {
      if (ev.key === "Tab") return;
      ev.stopImmediatePropagation();
      if (ev.key === "Escape") {
        ev.preventDefault();
        setLaunch(null);
      }
    };
    window.addEventListener("keydown", onKey, true);
    return () => window.removeEventListener("keydown", onKey, true);
  }, [launch]);

  if (!launch) return null;
  const done = typed >= command.length;
  return (
    <div className="rf-term-backdrop" onClick={() => setLaunch(null)}>
      <section
        className="rf-glass rf-term"
        role="dialog"
        aria-label={`Simulated terminal for ${launch.repoName}`}
        onClick={(ev) => ev.stopPropagation()}
      >
        <header className="rf-term__bar">
          <span className="rf-term__title">
            {launch.repoName} [WSL: Demo] · <span className="rf-muted">bash</span>
          </span>
          <button
            ref={closeButton}
            type="button"
            className="rf-button rf-button--ghost rf-icon-button"
            aria-label="Close terminal (Escape)"
            onClick={() => setLaunch(null)}
          >
            <svg viewBox="0 0 16 16" width="14" height="14" aria-hidden="true">
              <path d="M4 4l8 8M12 4l-8 8" stroke="currentColor" strokeWidth="1.6" strokeLinecap="round" />
            </svg>
          </button>
        </header>
        <pre className="rf-term__screen rf-mono" data-testid="demo-terminal">
          {command && (
            <>
              <span className="rf-term__prompt">demo@refract:~/projects/{launch.repoName}$ </span>
              {command.slice(0, typed)}
              {!done && <span className="rf-term__cursor" aria-hidden="true" />}
              {"\n"}
            </>
          )}
          {reply.slice(0, lines).join("\n")}
          {done && lines >= reply.length && (
            <>
              {"\n\n"}
              <span className="rf-term__prompt">{"> "}</span>
              <span className="rf-term__cursor" aria-hidden="true" />
            </>
          )}
        </pre>
        <footer className="rf-term__note">Demo: nothing is launched. The desktop app opens VS Code here.</footer>
      </section>
    </div>
  );
}

/** A small corner note that the repos are invented, for the web demo only. */
export function DemoBadge() {
  return (
    <a
      className="rf-glass rf-demo-badge"
      href="https://github.com/maddoxrjohnson/refract"
      target="_blank"
      rel="noopener noreferrer"
    >
      Demo with invented repos
    </a>
  );
}
