import type {
  MigrationReport,
  RefactorInfo,
  RepoSnapshot,
  ResumeReason,
  SessionInfo,
  WorktreeInfo,
} from "../backend/types";

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
  reason: ResumeReason | null = state.kind === "interrupted" ? "interrupted" : null,
): SessionInfo {
  return {
    id,
    state,
    lastTimestamp: hoursAgo(hours),
    lastAssistantText: text,
    byteSize,
    focusable: false,
    resumeReason: reason,
  };
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

/** A linked worktree's facts: clean and pushed, so it can be removed. */
export function testWorktreeInfo(extra: Partial<WorktreeInfo> = {}): WorktreeInfo {
  return {
    locked: false,
    operation: null,
    untrackedChecked: true,
    onlyHere: 0,
    merged: null,
    release: null,
    ignored: [],
    status: { kind: "pushed" },
    removable: true,
    checkable: false,
    ...extra,
  };
}

/** A linked worktree of the test repo, `tidepool-<name>` on branch `<name>`. */
export function testWorktree(
  name: string,
  info: Partial<WorktreeInfo> = {},
  extra: Partial<RepoSnapshot> = {},
): RepoSnapshot {
  const main = testRepo();
  return testRepo({
    path: `${main.path}-${name}`,
    name: `${main.name}-${name}`,
    branch: name,
    worktreeOf: main.path,
    worktree: testWorktreeInfo(info),
    ...extra,
  });
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
    checks: (
      [
        ["files", "Every file matches"],
        ["gitState", "Git state is identical"],
        ["nothingSkipped", "Nothing skipped without you knowing"],
        ["unchanged", "Nothing touched it during the copy"],
        ["history", "History copied"],
      ] as const
    ).map(([check, title]) => ({
      check,
      title,
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

/** A refactor of the test repo: ready, with two commits and its checks. */
export function testRefactor(extra: Partial<RefactorInfo> = {}): RefactorInfo {
  const worktree = "/home/demo/projects/.refract-worktrees/tidepool-20261031-1405";
  return {
    state: "ready",
    worktree,
    folderUri: `vscode-remote://wsl+Ubuntu${worktree}`,
    branch: "refactor/20261031-1405",
    targetBranch: "main",
    baseCommit: "0123456789abcdef0123456789abcdef01234567",
    sessionId: "5f0c2a1e-0000-4000-8000-0000000000c1",
    createdAt: "2026-10-31T14:05:00Z",
    live: false,
    interrupted: false,
    commits: [
      { sha: "b1c2d3e", summary: "Extract the retry policy" },
      { sha: "c2d3e4f", summary: "Remove dead export helpers" },
    ],
    filesChanged: 5,
    insertions: 80,
    deletions: 120,
    checks: [
      { command: "npm test", before: "pass", after: "pass" },
      { command: "npm run lint", before: "fail", after: "notRun" },
    ],
    summary: "Two refactors; the tests still pass.",
    blocked: null,
    ...extra,
  };
}
