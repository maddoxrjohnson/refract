import type { RepoSnapshot, SessionInfo } from "../backend/types";

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
