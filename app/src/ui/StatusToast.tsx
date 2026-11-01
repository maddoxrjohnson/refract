import type { AgentRequestError, AgentStatus, LaunchError, LaunchStep } from "../backend/types";
import "./ui.css";

// Wording carried over from the shell milestone's App.tsx.

export function describeAgentStatus(status: AgentStatus | null): string {
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

export const LAUNCH_STEP_LABELS: Record<LaunchStep, string> = {
  setup: "setting up the VS Code profile…",
  server: "installing the extension in WSL…",
  prepare: "preparing the launch…",
  opening: "opening VS Code…",
  done: "opened",
};

/** A person-readable message for a rejected backend call. */
export function describeError(err: unknown): string {
  if (typeof err === "string") return err;
  const e = err as Partial<AgentRequestError> | Partial<LaunchError> | null;
  if (e && e.kind === "agent" && "code" in e) return `${e.message} (${e.code})`;
  if (e && "message" in e && typeof e.message === "string") return e.message;
  if (e && e.kind) return e.kind;
  return String(err);
}

/** Where a launch is: cloning first, just sent, a progress step, or failed with a message. */
export type LaunchProgress =
  | { repoName: string; step: "cloning" | "starting" | LaunchStep }
  | { repoName: string; step: "failed"; message: string };

/** A refactor that moved on in a repo other than the zoomed one (refactor spec §4). */
export interface ToastNotice {
  repoName: string;
  text: string;
  /** Zooms to the repo; shows a Show button when given. */
  onShow?(): void;
}

export interface StatusToastProps {
  /** Helper state; shown only while it is not running. */
  agent: AgentStatus | null;
  launch: LaunchProgress | null;
  /** Shows a dismiss button when given. */
  onDismiss?(): void;
  notice?: ToastNotice | null;
}

function launchText(launch: LaunchProgress): string {
  if (launch.step === "failed") return launch.message;
  if (launch.step === "starting") return "starting…";
  if (launch.step === "cloning") return "cloning…";
  return LAUNCH_STEP_LABELS[launch.step];
}

/** Helper connection and launch progress, bottom left. Renders nothing when all is well. */
export function StatusToast({ agent, launch, onDismiss, notice = null }: StatusToastProps) {
  const showAgent = agent?.state !== "running";
  if (!showAgent && !launch && !notice) return null;
  const failed = launch?.step === "failed";
  // A finished launch fades out on its own; App removes it once the fade is over.
  const fading = !showAgent && !notice && launch?.step === "done";
  return (
    <div className={`rf-glass rf-toast${fading ? " rf-toast--done" : ""}`} role={failed ? "alert" : "status"} aria-live={failed ? "assertive" : "polite"}>
      {showAgent && (
        <p className={`rf-toast__line${agent?.state === "stopped" ? " rf-toast__line--error" : ""}`}>
          <span className="rf-muted">Agent:</span> {describeAgentStatus(agent)}
        </p>
      )}
      {launch && (
        <p
          className={`rf-toast__line${failed ? " rf-toast__line--error" : launch.step === "done" ? " rf-toast__line--done" : ""}`}
        >
          <span className="rf-toast__repo">{launch.repoName}</span> {launchText(launch)}
        </p>
      )}
      {notice && (
        <p className="rf-toast__line rf-toast__notice">
          <span className="rf-toast__repo">{notice.repoName}</span> {notice.text}
          {notice.onShow && (
            <button type="button" className="rf-link" onClick={notice.onShow}>
              Show
            </button>
          )}
        </p>
      )}
      {onDismiss && (
        <button type="button" className="rf-button rf-button--ghost" onClick={onDismiss}>
          Dismiss
        </button>
      )}
    </div>
  );
}
