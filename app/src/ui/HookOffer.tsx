import { useId, useState } from "react";
import type { HookControls } from "../hooks/useHook";
import { describeError } from "./StatusToast";
import "./ui.css";

const DISMISSED_KEY = "refract.hookOfferDismissed";

/** Whether the user said "Not now" to the offer; false when storage is unavailable. */
export function loadHookOfferDismissed(): boolean {
  try {
    return localStorage.getItem(DISMISSED_KEY) === "1";
  } catch {
    return false;
  }
}

export function saveHookOfferDismissed(): void {
  try {
    localStorage.setItem(DISMISSED_KEY, "1");
  } catch {
    // Storage blocked: the offer comes back after a reload.
  }
}

/** Runs install or remove, tracking whether one is under way and why the last failed. */
function useAction() {
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState<string | null>(null);
  const start = async (run: () => Promise<void>): Promise<boolean> => {
    setBusy(true);
    setError(null);
    try {
      await run();
      return true;
    } catch (err) {
      setError(describeError(err));
      return false;
    } finally {
      setBusy(false);
    }
  };
  return { busy, error, start };
}

/** "Show what changes" and the diff it reveals. */
function DiffToggle({ diff, settingsPath }: { diff: string | null; settingsPath: string }) {
  const [shown, setShown] = useState(false);
  const id = useId();
  if (diff === null) return null;
  // Settings already hold the entries; only the script itself is missing or outdated.
  if (diff === "") return <p className="rf-hint">Your settings already have the hook; updating restores its script.</p>;
  return (
    <>
      <button type="button" className="rf-link" aria-expanded={shown} aria-controls={id} onClick={() => setShown(!shown)}>
        {shown ? "Hide the changes" : "Show what changes"}
      </button>
      {shown && (
        <div id={id} className="rf-diff">
          <p className="rf-diff__caption rf-muted">
            Changes to <span className="rf-mono">{settingsPath}</span>
          </p>
          <pre className="rf-diff__body rf-mono" tabIndex={0} aria-label="What installing changes">
            {diff.split("\n").map((line, i) => (
              <span
                key={i}
                className={line.startsWith("+") && !line.startsWith("+++") ? "rf-diff__add" : line.startsWith("-") && !line.startsWith("---") ? "rf-diff__del" : undefined}
              >
                {line}
                {"\n"}
              </span>
            ))}
          </pre>
        </div>
      )}
    </>
  );
}

const WHAT_IT_DOES =
  "Claude Code can run a command when something happens in a session. Refract can add one small script to your Claude Code settings. It writes a one-line status file when a session starts, when you send a prompt, when Claude stops, needs your permission or asks you something, and when the session ends. That is what lets the gems show when an agent is working and when it is waiting on you.";
const WHY_UPDATE =
  "Refract's hook has changed since it was installed, so some states may not show. Updating replaces its entries in your settings; Remove still puts them back as they were. It applies to Claude Code sessions started after the update.";
const WHY_ITS_SAFE =
  "The script prints nothing, so Claude never sees it and it costs no tokens. You can remove it at any time in Settings, which puts your settings back as they were. It applies to Claude Code sessions started after you install it.";

export interface HookOfferProps {
  hook: HookControls;
  /** "Not now", or done after installing; the offer is not shown again. */
  onDismiss(): void;
}

/** The one-time first-run offer to install the status hook (spec §6.2). */
export function HookOffer({ hook, onDismiss }: HookOfferProps) {
  const titleId = useId();
  const action = useAction();
  const info = hook.info;
  if (!info) return null;
  return (
    <section className="rf-glass rf-hook-offer" aria-labelledby={titleId}>
      <h2 id={titleId} className="rf-hook-offer__title">
        See when agents are working
      </h2>
      <div className="rf-hook-offer__body">
        <p>{WHAT_IT_DOES}</p>
        <p className="rf-muted">{WHY_ITS_SAFE}</p>
        <DiffToggle diff={info.diff} settingsPath={info.settingsPath} />
        {action.error && (
          <p className="rf-error" role="alert">
            {action.error}
          </p>
        )}
      </div>
      <div className="rf-hook-offer__actions">
        <button
          type="button"
          className="rf-button rf-button--accent"
          disabled={action.busy}
          onClick={() => void action.start(hook.install).then((ok) => ok && onDismiss())}
        >
          {info.outdated ? (action.busy ? "Updating…" : "Update") : action.busy ? "Installing…" : "Install"}
        </button>
        <button type="button" className="rf-button rf-button--ghost" disabled={action.busy} onClick={onDismiss}>
          Not now
        </button>
      </div>
    </section>
  );
}

/** The settings dialog's section: the hook's state, with Install, Update or Remove. */
export function HookSection({ hook, labelId }: { hook: HookControls; labelId: string }) {
  const action = useAction();
  const [done, setDone] = useState<string | null>(null);
  const info = hook.info;
  const error = action.error ?? hook.error;
  // The button under way shows its progress label.
  const [running, setRunning] = useState<string | null>(null);
  const run = (fn: () => Promise<void>, label: string, message: string) => {
    setDone(null);
    setRunning(label);
    void action.start(fn).then((ok) => ok && setDone(message));
  };
  const button = (label: string, busyLabel: string, fn: () => Promise<void>, message: string) => (
    <button type="button" className="rf-button" disabled={action.busy} onClick={() => run(fn, label, message)}>
      {action.busy && running === label ? busyLabel : label}
    </button>
  );
  return (
    <section className="rf-section" aria-labelledby={labelId}>
      <h3 id={labelId} className="rf-section__label">
        Status hook
      </h3>
      <p className="rf-settings__line">
        <span className="rf-muted">State:</span>{" "}
        {info ? (info.installed ? "Installed" : info.outdated ? "Needs update" : "Not installed") : hook.error ? "Unknown" : "…"}
      </p>
      <p className="rf-hint rf-settings__line">
        Lets the gems show when an agent is working or waiting on you. It prints nothing and costs no tokens.
      </p>
      {info && (
        <p className="rf-hint rf-settings__line">
          In <span className="rf-mono">{info.settingsPath}</span>
        </p>
      )}
      {info?.outdated && <p className="rf-hint rf-settings__line">{WHY_UPDATE}</p>}
      {info && !info.installed && <DiffToggle diff={info.diff} settingsPath={info.settingsPath} />}
      {info && (
        <div className="rf-row rf-hook-buttons">
          {info.outdated && button("Update", "Updating…", hook.install, "Updated.")}
          {info.installed || info.outdated
            ? button("Remove", "Removing…", hook.remove, "Removed.")
            : button("Install", "Installing…", hook.install, "Installed.")}
        </div>
      )}
      {error && (
        <p className="rf-error" role="alert">
          {error}
        </p>
      )}
      {done && !error && (
        <p className="rf-ok" role="status">
          {done}
        </p>
      )}
    </section>
  );
}
