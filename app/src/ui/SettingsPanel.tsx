import { useCallback, useEffect, useId, useRef, useState, type FormEvent } from "react";
import type { Backend } from "../backend/Backend";
import type { Credentials, RemoteKind, RemoteSource, Settings } from "../backend/types";
import { describeKey } from "../projects";
import { describeError } from "./StatusToast";
import "./ui.css";

export interface SettingsPanelProps {
  /** For credentials, tokens and adding folders; settings changes go through `onChange`. */
  backend: Backend;
  /** `null` until loaded. */
  settings: Settings | null;
  /** Sources from the last remote listing, for their errors. */
  sources: RemoteSource[];
  remotesLoading: boolean;
  /** Saves new settings; rejects with a backend error. */
  onChange(settings: Settings): Promise<void>;
  /** Lists remotes again, past the cache. */
  onRefresh(): void;
  /** `byKey` is true for Escape, false for the close button. */
  onClose(byKey: boolean): void;
}

const GITHUB_SOURCE: Record<Credentials["github"], string> = {
  ghCli: "GitHub CLI login",
  stored: "Stored token",
  missing: "Missing",
};

/** A one-line result under a form: done, or what went wrong. */
type Note = { tone: "ok" | "error"; text: string } | null;

function NoteLine({ note }: { note: Note }) {
  if (!note) return null;
  return (
    <p className={note.tone === "error" ? "rf-error" : "rf-ok"} role={note.tone === "error" ? "alert" : "status"}>
      {note.text}
    </p>
  );
}

/**
 * A token field. The input is uncontrolled and cleared on submit, so the token passes
 * straight to the backend and never sits in React state.
 */
function TokenForm({
  kind,
  label,
  stored,
  onSave,
}: {
  kind: RemoteKind;
  label: string;
  stored: boolean;
  onSave(kind: RemoteKind, token: string | null): Promise<void>;
}) {
  const id = useId();
  const input = useRef<HTMLInputElement>(null);
  const submit = (ev: FormEvent) => {
    ev.preventDefault();
    const el = input.current;
    if (!el) return;
    const token = el.value.trim();
    el.value = "";
    if (token) void onSave(kind, token);
  };
  return (
    <form className="rf-field" onSubmit={submit}>
      <label htmlFor={id}>{label}</label>
      <div className="rf-field__row">
        <input
          id={id}
          ref={input}
          className="rf-input"
          type="password"
          autoComplete="off"
          spellCheck={false}
          placeholder={stored ? "Replace the stored token" : "Paste a token"}
        />
        <button type="submit" className="rf-button">
          Save
        </button>
      </div>
      {stored && (
        <button type="button" className="rf-link" onClick={() => void onSave(kind, null)}>
          Remove stored token
        </button>
      )}
    </form>
  );
}

/** GitLab URL, tokens, forks and archived, hidden repos and extra folders (spec §5). */
export function SettingsPanel({
  backend,
  settings,
  sources,
  remotesLoading,
  onChange,
  onRefresh,
  onClose,
}: SettingsPanelProps) {
  const titleId = useId();
  const gitlabId = useId();
  const folderId = useId();
  const root = useRef<HTMLDivElement>(null);
  const [creds, setCreds] = useState<Credentials | null>(null);
  const [gitlabDraft, setGitlabDraft] = useState<string | null>(null);
  const [folder, setFolder] = useState("");
  const [notes, setNotes] = useState<Record<string, Note>>({});
  const note = (key: string, value: Note) => setNotes((n) => ({ ...n, [key]: value }));

  useEffect(() => root.current?.focus(), []);

  const loadCreds = useCallback(() => {
    backend.credentials().then(setCreds, (err: unknown) =>
      note("tokens", { tone: "error", text: describeError(err) }),
    );
  }, [backend]);
  useEffect(loadCreds, [loadCreds]);

  /** Resolves with whether it saved; a failure shows under `key`. */
  const save = (next: Settings, key: string): Promise<boolean> =>
    onChange(next).then(
      () => {
        note(key, null);
        return true;
      },
      (err: unknown) => {
        note(key, { tone: "error", text: describeError(err) });
        return false;
      },
    );

  const saveToken = async (kind: RemoteKind, token: string | null) => {
    try {
      await backend.setToken(kind, token);
      note(kind, { tone: "ok", text: token ? "Token stored." : "Token removed." });
      loadCreds();
      onRefresh();
    } catch (err) {
      note(kind, { tone: "error", text: describeError(err) });
    }
  };

  const gitlabValue = gitlabDraft ?? settings?.gitlabUrl ?? "";
  const saveGitlab = (ev: FormEvent) => {
    ev.preventDefault();
    if (!settings) return;
    const url = gitlabValue.trim().replace(/\/+$/, "");
    if (url && !/^https:\/\/[^/\s]+/.test(url)) {
      note("gitlab-url", { tone: "error", text: "Use an https:// address, e.g. https://gitlab.example.com." });
      return;
    }
    void save({ ...settings, gitlabUrl: url || null }, "gitlab-url").then((saved) => {
      if (!saved) return;
      setGitlabDraft(null);
      onRefresh();
    });
  };

  const addFolder = async (ev: FormEvent) => {
    ev.preventDefault();
    const path = folder.trim();
    if (!path) return;
    try {
      await backend.addRepo(path);
      setFolder("");
      note("folder", { tone: "ok", text: `Added ${path}.` });
    } catch (err) {
      note("folder", { tone: "error", text: describeError(err) });
    }
  };

  const sourceError = (kind: RemoteKind) => sources.find((s) => s.kind === kind)?.error ?? null;

  return (
    <div
      ref={root}
      className="rf-glass rf-settings"
      role="dialog"
      aria-labelledby={titleId}
      tabIndex={-1}
      onKeyDown={(ev) => {
        if (ev.key !== "Escape") return;
        ev.preventDefault();
        ev.stopPropagation();
        onClose(true);
      }}
    >
      <header className="rf-settings__header">
        <h2 id={titleId} className="rf-settings__title">
          Settings
        </h2>
        <button
          type="button"
          className="rf-button rf-button--ghost rf-icon-button"
          aria-label="Close settings (Escape)"
          onClick={() => onClose(false)}
        >
          ×
        </button>
      </header>

      {!settings ? (
        <p className="rf-settings__body rf-muted">Loading…</p>
      ) : (
        <div className="rf-settings__body">
          <section className="rf-section" aria-labelledby={`${titleId}-github`}>
            <h3 id={`${titleId}-github`} className="rf-section__label">
              GitHub
            </h3>
            <p className="rf-settings__line">
              <span className="rf-muted">Token:</span>{" "}
              {creds ? GITHUB_SOURCE[creds.github] : "…"}
            </p>
            {sourceError("github") && <p className="rf-error">{sourceError("github")}</p>}
            <TokenForm
              kind="github"
              label="GitHub token (optional; the GitHub CLI login is used otherwise)"
              stored={creds?.github === "stored"}
              onSave={saveToken}
            />
            <NoteLine note={notes.github ?? null} />
          </section>

          <section className="rf-section" aria-labelledby={`${titleId}-gitlab`}>
            <h3 id={`${titleId}-gitlab`} className="rf-section__label">
              GitLab
            </h3>
            <form className="rf-field" onSubmit={saveGitlab}>
              <label htmlFor={gitlabId}>GitLab address</label>
              <div className="rf-field__row">
                <input
                  id={gitlabId}
                  className="rf-input"
                  type="url"
                  inputMode="url"
                  spellCheck={false}
                  placeholder="https://gitlab.example.com"
                  value={gitlabValue}
                  onChange={(ev) => setGitlabDraft(ev.target.value)}
                />
                <button type="submit" className="rf-button">
                  Save
                </button>
              </div>
            </form>
            <NoteLine note={notes["gitlab-url"] ?? null} />
            <p className="rf-settings__line">
              <span className="rf-muted">Token:</span>{" "}
              {creds ? (creds.gitlab === "stored" ? "Stored token" : "Missing") : "…"}
            </p>
            {sourceError("gitlab") && <p className="rf-error">{sourceError("gitlab")}</p>}
            <TokenForm
              kind="gitlab"
              label="GitLab personal access token (read_api)"
              stored={creds?.gitlab === "stored"}
              onSave={saveToken}
            />
            <NoteLine note={notes.gitlab ?? null} />
            <NoteLine note={notes.tokens ?? null} />
          </section>

          <section className="rf-section" aria-labelledby={`${titleId}-show`}>
            <h3 id={`${titleId}-show`} className="rf-section__label">
              Remote repos
            </h3>
            <label className="rf-check">
              <input
                type="checkbox"
                checked={settings.showForks}
                onChange={(ev) => void save({ ...settings, showForks: ev.target.checked }, "show")}
              />
              <span>Show forks</span>
            </label>
            <label className="rf-check">
              <input
                type="checkbox"
                checked={settings.showArchived}
                onChange={(ev) => void save({ ...settings, showArchived: ev.target.checked }, "show")}
              />
              <span>Show archived repos</span>
            </label>
            <NoteLine note={notes.show ?? null} />
            <button type="button" className="rf-button" onClick={onRefresh} disabled={remotesLoading}>
              {remotesLoading ? "Refreshing…" : "Refresh remotes"}
            </button>
          </section>

          <section className="rf-section" aria-labelledby={`${titleId}-hidden`}>
            <h3 id={`${titleId}-hidden`} className="rf-section__label">
              Hidden
            </h3>
            {settings.hidden.length === 0 ? (
              <p className="rf-muted rf-settings__line">Nothing hidden.</p>
            ) : (
              <ul className="rf-hidden-list">
                {settings.hidden.map((key) => (
                  <li key={key}>
                    <span className="rf-mono rf-hidden-list__name" title={key}>
                      {describeKey(key)}
                    </span>
                    <button
                      type="button"
                      className="rf-button"
                      aria-label={`Unhide ${describeKey(key)}`}
                      onClick={() =>
                        void save({ ...settings, hidden: settings.hidden.filter((k) => k !== key) }, "hidden")
                      }
                    >
                      Unhide
                    </button>
                  </li>
                ))}
              </ul>
            )}
            <NoteLine note={notes.hidden ?? null} />
          </section>

          <section className="rf-section" aria-labelledby={`${titleId}-folders`}>
            <h3 id={`${titleId}-folders`} className="rf-section__label">
              Local folders
            </h3>
            <form className="rf-field" onSubmit={addFolder}>
              <label htmlFor={folderId}>Add a folder (a WSL path)</label>
              <div className="rf-field__row">
                <input
                  id={folderId}
                  className="rf-input rf-mono"
                  type="text"
                  spellCheck={false}
                  placeholder="/home/you/code/project"
                  value={folder}
                  onChange={(ev) => setFolder(ev.target.value)}
                />
                <button type="submit" className="rf-button" disabled={!folder.trim()}>
                  Add
                </button>
              </div>
            </form>
            <NoteLine note={notes.folder ?? null} />
          </section>
        </div>
      )}
    </div>
  );
}
