import { useEffect, useId, useRef } from "react";
import { firstLine, formatSize, relativeTime } from "./format";
import type { AgentKind, ResumeReason, SessionInfo } from "../backend/types";
import { agentName, optionKey, type PickerOption } from "./navigation";
import "./ui.css";

export interface PickerProps {
  repoName: string;
  options: PickerOption[];
  /** The highlighted option, from navigation's picker state. */
  index: number;
  /** Current time in milliseconds, for relative times. */
  now: number;
  /** Pointer hover: dispatch `{ type: "pickerMove", index }`. */
  onMove(index: number): void;
  /** Click: dispatch `{ type: "pickerAccept", index }`. */
  onAccept(index: number): void;
  onClose(): void;
}

/** What a running agent is doing, for its "Go to the running agent" row. */
export function liveStatus(session: SessionInfo): string {
  if (session.state.kind !== "live") return "running";
  switch (session.state.detail) {
    case "waitingOnUser":
      return "waiting on you";
    case "working":
      return "working";
    case "idle":
      return "idle";
    default:
      return "running";
  }
}

const RESUME_LABELS: Record<ResumeReason, string> = {
  interrupted: "Resume: stopped mid-task",
  cutOff: "Resume: cut off by a restart",
  askedQuestion: "Resume: asked you a question",
  uncommittedChanges: "Resume: uncommitted changes",
  unpushedCommits: "Resume: unpushed commits",
};

/** The label of a session's Resume row, naming why it is worth resuming. */
export function resumeLabel(session: SessionInfo): string {
  return session.resumeReason ? RESUME_LABELS[session.resumeReason] : "Resume";
}

/** A New row's label: "New agent" without Codex, else the agent, briefed or after stopping the other. */
export function newLabel(option: Extract<PickerOption, { kind: "new" }>): string {
  if (!option.agent) return "New agent";
  const name = agentName(option.agent);
  if (option.stop) return `Stop the idle ${agentName(option.agent === "codex" ? "claude" : "codex")} agent and start ${name}`;
  return `New ${name} agent${option.brief ? ", briefed" : ""}`;
}

/** A New row's second line: why it can't start, else why it is preselected and what it does. */
export function newText(option: Extract<PickerOption, { kind: "new" }>): string {
  if (option.blocked) return option.blocked;
  const does = option.brief
    ? `Picks up where ${agentName(option.agent === "codex" ? "claude" : "codex")} left off`
    : option.prompt
      ? "Starts by reviewing the uncommitted changes"
      : "A fresh session";
  return option.reason ? `${option.reason} · ${does}` : does;
}

/** `[Claude]` or `[Codex]` before a session row's label, with Codex installed. */
function Tag({ agent }: { agent?: AgentKind }) {
  if (!agent) return null;
  return <span className="rf-option__tag rf-mono">[{agentName(agent)}] </span>;
}

/**
 * Go to a running agent, resume a session or start fresh (spec §4.3). Keys are handled by the window-level
 * navigation handler; the list takes focus so screen readers follow the highlight.
 */
export function Picker({ repoName, options, index, now, onMove, onAccept, onClose }: PickerProps) {
  const base = useId();
  const titleId = `${base}-title`;
  const optionId = (i: number) => `${base}-option-${i}`;
  const list = useRef<HTMLUListElement>(null);

  useEffect(() => list.current?.focus(), []);
  // A long list scrolls; keep the highlighted option in sight.
  useEffect(() => {
    list.current?.querySelector('[aria-selected="true"]')?.scrollIntoView?.({ block: "nearest" });
  }, [index]);

  return (
    <div className="rf-glass rf-picker" role="dialog" aria-labelledby={titleId}>
      <h2 id={titleId} className="rf-picker__title">
        Start an agent in <span className="rf-mono">{repoName}</span>
      </h2>
      <ul
        ref={list}
        className="rf-picker__list"
        role="listbox"
        tabIndex={0}
        aria-labelledby={titleId}
        aria-activedescendant={options[index] ? optionId(index) : undefined}
      >
        {options.map((option, i) => (
          <li
            key={optionKey(option)}
            id={optionId(i)}
            className={`rf-option${option.kind === "new" && option.blocked ? " rf-option--dimmed" : ""}`}
            role="option"
            aria-selected={i === index}
            aria-disabled={option.kind === "new" && option.blocked ? true : undefined}
            onMouseEnter={() => onMove(i)}
            onClick={() => onAccept(i)}
          >
            {option.kind === "focus" ? (
              <>
                <div className="rf-option__head">
                  <span>
                    <Tag agent={option.agent} />
                    Go to the running agent
                  </span>
                  <span className="rf-option__meta rf-mono">
                    {liveStatus(option.session)}
                    {option.session.lastTimestamp
                      ? ` · ${relativeTime(Date.parse(option.session.lastTimestamp), now)}`
                      : ""}
                  </span>
                </div>
                <div className="rf-option__text">
                  {firstLine(option.session.lastAssistantText) ?? "No reply yet"}
                </div>
              </>
            ) : option.kind === "resume" ? (
              <>
                <div className="rf-option__head">
                  <span>
                    <Tag agent={option.agent} />
                    {resumeLabel(option.session)}
                  </span>
                  <span className="rf-option__meta rf-mono">
                    {option.session.lastTimestamp
                      ? `${relativeTime(Date.parse(option.session.lastTimestamp), now)} · `
                      : ""}
                    {formatSize(option.session.byteSize)}
                  </span>
                </div>
                <div className="rf-option__text">
                  {firstLine(option.session.lastAssistantText) ?? "No reply yet"}
                </div>
              </>
            ) : (
              <>
                <div className="rf-option__head">
                  <span>{newLabel(option)}</span>
                </div>
                <div className="rf-option__text">{newText(option)}</div>
              </>
            )}
          </li>
        ))}
      </ul>
      <div className="rf-picker__footer">
        <span>
          <kbd className="rf-kbd">↑</kbd> <kbd className="rf-kbd">↓</kbd> choose ·{" "}
          <kbd className="rf-kbd">Enter</kbd> start
        </span>
        <button type="button" className="rf-button rf-button--ghost" onClick={onClose}>
          Cancel <kbd className="rf-kbd">Esc</kbd>
        </button>
      </div>
    </div>
  );
}
