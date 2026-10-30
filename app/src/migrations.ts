import type {
  MigrateCheck,
  MigrateInstallResult,
  MigrateProgress,
  MigrateSnapshotProgress,
  MigrateStep,
  MigrationReport,
  PendingMigration,
  RepoSnapshot,
  SessionInfo,
} from "./backend/types";

// Move to WSL (.plans/wsl-migration-spec.md §2) as the interface tracks it: one entry per
// source folder, from Start until the user answers the report. Built from the answers
// to migrateStart and migratePending and from the helper's migrate events; the commands
// are documented in app/src-tauri/src/migrate/mod.rs.

/** A folder on a Windows drive as WSL sees it: `/mnt/<letter>/...`. */
export function isWindowsDrive(path: string): boolean {
  return /^\/mnt\/[a-z]\//i.test(path);
}

/** `C:\Users\dev\aurora-notes` for `/mnt/c/Users/dev/aurora-notes`. */
export function windowsPath(path: string): string {
  const m = /^\/mnt\/([a-z])(\/.*)?$/i.exec(path);
  if (!m) return path;
  return `${m[1].toUpperCase()}:${(m[2] ?? "/").replace(/\/+$/, "").replace(/\//g, "\\") || "\\"}`;
}

export type MigrationPhase =
  /** `migrateStart` is reading the source; `progress` is its last `migrate://snapshot`. */
  | { kind: "snapshot"; progress: MigrateSnapshotProgress | null }
  /** The helper copies and checks; `step` is `null` until its first progress event. */
  | { kind: "running"; step: MigrateStep | null; done: number; total: number }
  /** The report waits for an answer. */
  | { kind: "ready"; report: MigrationReport };

export interface Migration {
  /** The source's WSL path; the key. */
  source: string;
  dest: string;
  /** `null` until `migrateStart` answers. */
  id: string | null;
  phase: MigrationPhase;
}

export type MigrationsState = ReadonlyMap<string, Migration>;

export const initialMigrations: MigrationsState = new Map();

export type MigrationAction =
  /** Start was pressed. */
  | { type: "starting"; source: string; dest: string }
  | { type: "snapshot"; progress: MigrateSnapshotProgress }
  /** `migrateStart` answered. */
  | { type: "started"; source: string; id: string }
  | { type: "progress"; progress: MigrateProgress }
  | { type: "ready"; id: string; report: MigrationReport }
  /** `migratePending` answered: reports from before a restart, or missed while hidden. */
  | { type: "pending"; list: PendingMigration[] }
  /** Re-sync was asked for: copying and checking start again. */
  | { type: "resyncing"; id: string }
  /** The helper stopped: a running copy is gone with it (it deletes the copy when it starts again). */
  | { type: "helperStopped" }
  /** Cancelled, failed to start, or finished. */
  | { type: "gone"; source: string };

function withEntry(state: MigrationsState, entry: Migration): MigrationsState {
  const next = new Map(state);
  next.set(entry.source, entry);
  return next;
}

function byId(state: MigrationsState, id: string): Migration | undefined {
  for (const m of state.values()) if (m.id === id) return m;
  return undefined;
}

function ready(state: MigrationsState, id: string, report: MigrationReport): MigrationsState {
  return withEntry(state, { source: report.source, dest: report.dest, id, phase: { kind: "ready", report } });
}

export function migrationsReducer(state: MigrationsState, action: MigrationAction): MigrationsState {
  switch (action.type) {
    case "starting":
      return withEntry(state, {
        source: action.source,
        dest: action.dest,
        id: null,
        phase: { kind: "snapshot", progress: null },
      });
    case "snapshot": {
      const m = state.get(action.progress.path);
      if (m?.phase.kind !== "snapshot") return state;
      return withEntry(state, { ...m, phase: { kind: "snapshot", progress: action.progress } });
    }
    case "started": {
      const m = state.get(action.source);
      // A report that arrived first wins.
      if (m?.phase.kind !== "snapshot") return state;
      return withEntry(state, { ...m, id: action.id, phase: { kind: "running", step: null, done: 0, total: 0 } });
    }
    case "progress": {
      const { id, source, dest, step, done, total } = action.progress;
      // A migration this window never started (a reload) is placed by its source.
      const m = byId(state, id) ?? (source ? { ...state.get(source), source, dest, id } : undefined);
      if (!m) return state;
      return withEntry(state, { ...m, id, dest: m.dest || dest, phase: { kind: "running", step, done, total } });
    }
    case "ready":
      return ready(state, action.id, action.report);
    case "pending":
      return action.list.reduce((s, p) => ready(s, p.id, p.report), state);
    case "resyncing": {
      const m = byId(state, action.id);
      if (!m) return state;
      return withEntry(state, { ...m, phase: { kind: "running", step: "rechecking", done: 0, total: 0 } });
    }
    case "helperStopped": {
      const running = [...state.values()].filter((m) => m.phase.kind === "running");
      if (running.length === 0) return state;
      const next = new Map(state);
      for (const m of running) next.delete(m.source);
      return next;
    }
    case "gone": {
      if (!state.has(action.source)) return state;
      const next = new Map(state);
      next.delete(action.source);
      return next;
    }
  }
}

const GB = 1024 ** 3;
const MB = 1024 ** 2;

/** "1.2 / 3.4 GB", or in MB below a gigabyte. */
export function byteRange(done: number, total: number): string {
  if (total >= GB) return `${(done / GB).toFixed(1)} / ${(total / GB).toFixed(1)} GB`;
  return `${Math.round(done / MB)} / ${Math.max(1, Math.round(total / MB))} MB`;
}

const STEP_TEXT: Record<MigrateStep, string> = {
  copying: "Copying",
  normalising: "Fixing line endings",
  hashing: "Hashing",
  checkingGit: "Checking git",
  rechecking: "Checking for changes",
  movingHistory: "Moving session history",
  installing: "Installing",
};

const SNAPSHOT_TEXT: Record<MigrateSnapshotProgress["phase"], string> = {
  git: "Reading git state",
  walking: "Listing files",
  probing: "Checking files",
  hashing: "Hashing the original",
  blobs: "Hashing tracked files",
};

/** What a preflight or snapshot is doing, for the dialog and the panel. */
export function snapshotText(progress: MigrateSnapshotProgress | null): string {
  if (!progress) return "Reading the folder";
  const text = SNAPSHOT_TEXT[progress.phase];
  if (progress.phase === "hashing" && progress.bytesTotal > 0) {
    return `${text} ${byteRange(progress.bytesDone, progress.bytesTotal)}`;
  }
  if (progress.filesTotal > 0) {
    return `${text} ${progress.filesDone.toLocaleString("en-US")} / ${progress.filesTotal.toLocaleString("en-US")}`;
  }
  return text;
}

/** The step line in the panel: "Copying 1.2 / 3.4 GB", "Hashing", "Checking git", … */
export function stepText(phase: MigrationPhase): string {
  switch (phase.kind) {
    case "snapshot":
      return snapshotText(phase.progress);
    case "running":
      if (!phase.step) return "Starting";
      if (phase.step === "copying" && phase.total > 0) return `Copying ${byteRange(phase.done, phase.total)}`;
      return STEP_TEXT[phase.step];
    case "ready":
      return canDelete(phase.report) ? "Ready for your OK" : "A check failed";
  }
}

// Shares of the whole run, for the ring: the snapshot, then the helper's steps in order.
const SNAPSHOT_SHARE = 0.15;
const STEP_SHARES: [MigrateStep, number][] = [
  ["copying", 0.45],
  ["normalising", 0.08],
  ["hashing", 0.14],
  ["checkingGit", 0.06],
  ["rechecking", 0.04],
  ["movingHistory", 0.03],
  ["installing", 0.05],
];

/** How far the whole migration is, 0 to 1; a ready report is 1. */
export function overallProgress(phase: MigrationPhase): number {
  const part = (done: number, total: number) => (total > 0 ? Math.min(1, Math.max(0, done / total)) : 0);
  switch (phase.kind) {
    case "snapshot": {
      const p = phase.progress;
      const f = !p ? 0 : p.bytesTotal > 0 ? part(p.bytesDone, p.bytesTotal) : part(p.filesDone, p.filesTotal);
      return SNAPSHOT_SHARE * f;
    }
    case "running": {
      let before = SNAPSHOT_SHARE;
      for (const [step, share] of STEP_SHARES) {
        if (step === phase.step) return before + share * part(phase.done, phase.total);
        before += share;
      }
      return SNAPSHOT_SHARE;
    }
    case "ready":
      return 1;
  }
}

/** The checks that guard the original (spec §5): all but History. */
export const BLOCKING_CHECKS: MigrateCheck[] = ["files", "gitState", "nothingSkipped", "unchanged"];

/** True when Delete original may be offered, as refract-core's `Report::can_delete`. */
export function canDelete(report: MigrationReport): boolean {
  return BLOCKING_CHECKS.every((check) =>
    report.checks.some((c) => c.check === check && c.status === "passed"),
  );
}

/** One line for an install step's result. */
export function installText(result: MigrateInstallResult): string {
  switch (result.status) {
    case "succeeded":
      return "Done";
    case "failed":
      return result.exitCode === undefined ? "Failed" : `Failed (exit code ${result.exitCode})`;
    case "timedOut":
      return "Stopped after 15 minutes";
    case "toolMissing":
      return `${result.step.tool} isn't installed in WSL`;
  }
}

/** An agent on the repo that blocks Start: one that is working can't be stopped. */
export interface BusyAgent {
  session: SessionInfo;
  working: boolean;
}

/** The repo's live agents, which block Start until stopped (spec §2.2). */
export function busyAgents(repo: RepoSnapshot | undefined): BusyAgent[] {
  if (!repo) return [];
  return repo.sessions
    .filter((s) => s.state.kind === "live")
    .map((session) => ({
      session,
      working: session.state.kind === "live" && session.state.detail === "working",
    }));
}
