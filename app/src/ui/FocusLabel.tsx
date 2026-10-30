import type { RepoSnapshot } from "../backend/types";
import { hostLabel, repoStatus } from "../repoStatus";
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
}

/** The small label beside the focused gem: name, host, language and state (spec §3.3). */
export function FocusLabel({ repo }: FocusLabelProps) {
  const state = stateLine(repo);
  return (
    <div className="rf-glass rf-focus-label" role="status" aria-live="polite">
      <div className="rf-focus-label__name">{repo.name}</div>
      <div className="rf-focus-label__meta">
        {hostLabel(repo)} · {repo.language ?? "Unknown language"}
      </div>
      <div className={`rf-state rf-state--${state.tone}`}>{state.text}</div>
    </div>
  );
}
