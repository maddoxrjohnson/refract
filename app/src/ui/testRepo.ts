import type { MigrationReport, RepoSnapshot, SessionInfo } from "../backend/types";

// Fixtures for the overlay tests. Invented names only.

export const NOW = Date.UTC(2026, 9, 30, 12);

export function hoursAgo(hours: number): string {
  return new Date(NOW - hours * 3_600_000).toISOString();
}

export function testSession(
  id: string,
  state: SessionInfo["state"],
  hours: number,
  text: string | null = null,
  byteSize = 100_000,
): SessionInfo {
  return { id, state, lastTimestamp: hoursAgo(hours), lastAssistantText: text, byteSize, focusable: false };
}

export function testRepo(extra: Partial<RepoSnapshot> = {}): RepoSnapshot {
  return {
    path: "/home/demo/projects/tidepool",
    name: "tidepool",
    originUrl: "git@github.com:demo-user/tidepool.git",
    remote: { host: "github.com", owner: "demo-user", name: "tidepool" },
    branch: "main",
    ahead: 0,
    behind: 0,
    changedFiles: 0,
    dirtyPaths: [],
    lastCommit: { summary: "Split the sync worker into stages", time: (NOW - 2 * 3_600_000) / 1000 },
    language: "TypeScript",
    trackedBytes: 0,
    commits30d: 0,
    mass: 0.5,
    sessions: [],
    preselect: { kind: "noPicker" },
    ...extra,
  };
}

/** A repo on a Windows drive, and where Move to WSL puts it. */
export const TEST_SOURCE = "/mnt/c/Users/dev/Desktop/aurora-notes";
export const TEST_DEST = "/home/dev/projects/aurora-notes";

/** A Move to WSL report where every check passed. */
export function testReport(extra: Partial<MigrationReport> = {}): MigrationReport {
  return {
    version: 1,
    id: "m-1",
    source: TEST_SOURCE,
    dest: TEST_DEST,
    startedAt: "2026-10-30T12:00:00Z",
    finishedAt: "2026-10-30T12:01:00Z",
    checks: (["files", "gitState", "nothingSkipped", "unchanged", "history"] as const).map((check) => ({
      check,
      status: "passed",
      problems: [],
    })),
    skipped: [{ path: "node_modules", rule: "nodeModules", bytes: 400 * 1024 ** 2, files: 30_000 }],
    droppedStreams: [],
    newObjects: 3,
    sourceTotals: { files: 120, dirs: 14, bytes: 2_000_000 },
    copyTotals: { files: 120, dirs: 14, bytes: 2_000_000 },
    install: [],
    history: { sessions: 2, fromDirs: [], toDirs: [] },
    linuxGit: false,
    warnings: [],
    ...extra,
  };
}
