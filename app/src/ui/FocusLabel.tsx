import type { RepoSnapshot } from "../backend/types";
import { hostLabel, repoStatus } from "../repoStatus";
import { stoneFor } from "../stones";
import { WINDOWS_DRIVE_TEXT } from "./GemMark";
import "./ui.css";

/**
 * The one-line state under a gem's name, and how loudly to show it. Agents in the folded
 * `worktrees` count, as they do on the stone.
 */
export function stateLine(
  repo: RepoSnapshot,
  worktrees: readonly RepoSnapshot[] = [],
): { text: string; tone: "attention" | "dirty" | "calm" } {
  const s = repoStatus(repo, worktrees);
  const parts: string[] = [];
  if (s.waiting) parts.push("Waiting for you");
  if (s.working) parts.push("Agent working");
  if (s.interrupted) parts.push("Session didn't finish");
  if (s.dirty) parts.push(`${repo.changedFiles} changed ${repo.changedFiles === 1 ? "file" : "files"}`);
  if (parts.length === 0) parts.push("Clean");
  const tone = s.waiting || s.interrupted ? "attention" : s.dirty || s.working ? "dirty" : "calm";
  return { text: parts.join(" · "), tone };
}

/** Which swatch a status line shows (banknote spec §6). */
export type StatusSwatch = "waiting" | "working" | "interrupted" | "dirty" | "remote" | "clean";

/** The one swatch for a repo: waiting, then working, interrupted, dirty, not cloned, clean. */
export function statusSwatch(
  repo: RepoSnapshot,
  notCloned = false,
  worktrees: readonly RepoSnapshot[] = [],
): StatusSwatch {
  const s = repoStatus(repo, worktrees);
  if (s.waiting) return "waiting";
  if (s.working) return "working";
  if (s.interrupted) return "interrupted";
  if (s.dirty) return "dirty";
  return notCloned ? "remote" : "clean";
}

/** A repo's state as a swatch and a line of text, red when it needs you. */
export function StatusLine({
  repo,
  notCloned = false,
  worktrees,
}: {
  repo: RepoSnapshot;
  notCloned?: boolean;
  /** Linked worktrees folded into the gem. */
  worktrees?: readonly RepoSnapshot[];
}) {
  const state = notCloned ? { text: "Not cloned", tone: "calm" } : stateLine(repo, worktrees);
  const swatch = statusSwatch(repo, notCloned, worktrees);
  return (
    <div className={`rf-state rf-state--${state.tone}`} data-swatch={swatch}>
      <span className={`rf-sw rf-sw--${swatch}`} aria-hidden="true" />
      {state.text}
    </div>
  );
}

export interface FocusLabelProps {
  repo: RepoSnapshot;
  /** The repo exists only on GitHub or GitLab. */
  notCloned?: boolean;
  /** The repo is on a Windows drive: says what the gem's marker means. */
  windowsDrive?: boolean;
  /** A Move to WSL's step, while one runs or waits for its report's answer. */
  move?: string | null;
  /** Linked worktrees folded into the gem. */
  worktrees?: readonly RepoSnapshot[];
}

/** The small label beside the focused gem: name, host, language, stone and state (spec §3.3). */
export function FocusLabel({ repo, notCloned = false, windowsDrive = false, move = null, worktrees }: FocusLabelProps) {
  return (
    <div className="rf-glass rf-focus-label" role="status" aria-live="polite">
      <div className="rf-focus-label__name">{repo.name}</div>
      <div className="rf-focus-label__meta">
        {hostLabel(repo)} · {repo.language ?? "Unknown language"} · {stoneFor(repo.language).name}
      </div>
      <StatusLine repo={repo} notCloned={notCloned} worktrees={worktrees} />
      {move && <div className="rf-state rf-state--move">Moving to WSL · {move}</div>}
      {windowsDrive && !move && <div className="rf-focus-label__drive">{WINDOWS_DRIVE_TEXT}</div>}
    </div>
  );
}
