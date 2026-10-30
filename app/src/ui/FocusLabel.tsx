import type { RepoSnapshot } from "../backend/types";
import { hostLabel, repoStatus } from "../repoStatus";
import { WINDOWS_DRIVE_TEXT } from "./GemMark";
import "./ui.css";

/** The one-line state under a gem's name, and how loudly to show it. */
export function stateLine(repo: RepoSnapshot): { text: string; tone: "attention" | "dirty" | "calm" } {
  const s = repoStatus(repo);
  const parts: string[] = [];
  if (s.waiting) parts.push("Waiting for you");
  if (s.working) parts.push("Agent working");
  if (s.interrupted) parts.push("Session didn't finish");
  if (s.dirty) parts.push(`${repo.changedFiles} changed ${repo.changedFiles === 1 ? "file" : "files"}`);
  if (parts.length === 0) parts.push("Clean");
  const tone = s.waiting || s.interrupted ? "attention" : s.dirty || s.working ? "dirty" : "calm";
  return { text: parts.join(" · "), tone };
}

export interface FocusLabelProps {
  repo: RepoSnapshot;
  /** The repo exists only on GitHub or GitLab. */
  notCloned?: boolean;
  /** The repo is on a Windows drive: says what the gem's marker means. */
  windowsDrive?: boolean;
  /** A Move to WSL's step, while one runs or waits for its report's answer. */
  move?: string | null;
}

/** The small label beside the focused gem: name, host, language and state (spec §3.3). */
export function FocusLabel({ repo, notCloned = false, windowsDrive = false, move = null }: FocusLabelProps) {
  const state = notCloned ? { text: "Not cloned", tone: "calm" } : stateLine(repo);
  return (
    <div className="rf-glass rf-focus-label" role="status" aria-live="polite">
      <div className="rf-focus-label__name">{repo.name}</div>
      <div className="rf-focus-label__meta">
        {hostLabel(repo)} · {repo.language ?? "Unknown language"}
      </div>
      <div className={`rf-state rf-state--${state.tone}`}>{state.text}</div>
      {move && <div className="rf-state rf-state--move">Moving to WSL · {move}</div>}
      {windowsDrive && !move && <div className="rf-focus-label__drive">{WINDOWS_DRIVE_TEXT}</div>}
    </div>
  );
}
