import type { Backend, DemoLaunch, Unsubscribe } from "./Backend";
import type {
  AgentEvent,
  AgentStatus,
  CiStatus,
  Credentials,
  GpuUsage,
  HookInfo,
  LaunchMode,
  LaunchStep,
  LaunchTicket,
  MergeOutcome,
  ModelUsage,
  RefactorNotice,
  PlanLimits,
  Preselect,
  RemoteDetails,
  RemoteKind,
  RemoteList,
  RemoteRef,
  RemoteRepo,
  RepoList,
  RefactorInfo,
  RepoSnapshot,
  ResumeReason,
  SessionInfo,
  Settings,
  SystemStats,
  TokenTotals,
  UsageSummary,
} from "./types";
import type {
  DeleteOutcome,
  Forced,
  FinishAction,
  InstallStep,
  MigrateError,
  MigratePreflight,
  MigrateSnapshotProgress,
  MigrateStep,
  MigrationReport,
  PendingMigration,
  CancelOutcome,
  SkippedFolder,
  VsCodeWindow,
} from "./types";
import { NO_FORCE } from "./types";
import { DEFAULT_PRICES } from "../usage/cost";

// Invented repos covering every state a gem can show, for running the interface in a
// plain browser (`npm run dev`) and, later, the public demo. No real repo names.

const HOUR = 3600;
const now = () => Math.floor(Date.now() / 1000);
const iso = (secondsAgo: number) => new Date((now() - secondsAgo) * 1000).toISOString();

interface Seed {
  name: string;
  host?: "github.com" | "gitlab.example.com" | null;
  language: string | null;
  mass: number;
  changedFiles?: number;
  ahead?: number;
  behind?: number;
  commit: [string, number];
  sessions?: SessionInfo[];
  preselect?: Preselect;
  /** Where it is cloned; `~/projects/<name>` by default. */
  path?: string;
  /** A refactor already under way, for the refactor section and the stone's mark. */
  refactor?: "ready";
}

/** The demo's repo on a Windows drive. */
export const DEMO_WINDOWS_REPO = "/mnt/c/Users/demo/Desktop/marigold-site";

/** An interrupted session's resume reason is always "interrupted"; others pass theirs. */
const session = (
  id: string,
  state: SessionInfo["state"],
  secondsAgo: number,
  text: string | null,
  byteSize: number,
  focusable = false,
  reason: ResumeReason | null = null,
): SessionInfo => ({
  id,
  state,
  lastTimestamp: iso(secondsAgo),
  lastAssistantText: text,
  byteSize,
  focusable,
  resumeReason: state.kind === "interrupted" ? "interrupted" : reason,
});

const SEEDS: Seed[] = [
  {
    name: "atlas-api",
    host: "github.com",
    language: "TypeScript",
    mass: 0.92,
    ahead: 2,
    commit: ["Add rate limiting to the public endpoints", 3 * HOUR],
    sessions: [
      session("5f0c2a1e-0000-4000-8000-000000000001", { kind: "finished", detail: "completed" }, 5 * HOUR, "Rate limiting is in place and the tests pass.", 180_000),
    ],
  },
  {
    name: "tidepool",
    host: "github.com",
    language: "TypeScript",
    mass: 0.74,
    changedFiles: 3,
    commit: ["Split the sync worker into stages", 26 * HOUR],
    sessions: [
      session("5f0c2a1e-0000-4000-8000-000000000002", { kind: "interrupted", detail: "toolCallWithoutResult" }, 2 * HOUR, "Running the migration against the staging copy now.", 420_000),
      session("5f0c2a1e-0000-4000-8000-000000000003", { kind: "interrupted", detail: "apiError" }, 50 * HOUR, "I'll start by reading the scheduler.", 90_000),
    ],
    preselect: { kind: "resume", sessionId: "5f0c2a1e-0000-4000-8000-000000000002" },
  },
  {
    name: "ledger-sync",
    host: "github.com",
    language: "Python",
    mass: 0.7,
    changedFiles: 5,
    behind: 1,
    commit: ["Handle duplicate ledger rows", 4 * 24 * HOUR],
    preselect: { kind: "newAgent" },
  },
  {
    name: "trailhead-ios",
    host: "github.com",
    language: "Swift",
    mass: 0.46,
    commit: ["Bump the deployment target", 9 * 24 * HOUR],
    sessions: [
      session("5f0c2a1e-0000-4000-8000-000000000008", { kind: "finished", detail: "completed" }, 3 * HOUR, "The trail widget is in. Should it show on the lock screen too?", 210_000, false, "askedQuestion"),
    ],
    preselect: { kind: "resume", sessionId: "5f0c2a1e-0000-4000-8000-000000000008" },
  },
  {
    name: "quill",
    host: "github.com",
    language: "Go",
    mass: 0.4,
    changedFiles: 1,
    commit: ["Render footnotes", 30 * HOUR],
    sessions: [
      session("5f0c2a1e-0000-4000-8000-000000000004", { kind: "live", detail: "working" }, 60, null, 64_000),
    ],
    preselect: { kind: "newAgent" },
  },
  { name: "dotfiles", host: "github.com", language: "Shell", mass: 0.28, commit: ["Alias for git switch", 20 * 24 * HOUR] },
  {
    name: "tessera",
    host: "github.com",
    language: "Rust",
    mass: 0.36,
    commit: ["Tile cache eviction", 2 * 24 * HOUR],
    sessions: [
      session("5f0c2a1e-0000-4000-8000-000000000005", { kind: "live", detail: "waitingOnUser" }, 120, "Should I also update the benchmarks?", 140_000, true),
    ],
    preselect: { kind: "focus", sessionId: "5f0c2a1e-0000-4000-8000-000000000005" },
  },
  { name: "sweep", host: "github.com", language: "C#", mass: 0.18, commit: ["Initial commit", 60 * 24 * HOUR], refactor: "ready" },
  {
    name: "harbor-android",
    host: "github.com",
    language: "Kotlin",
    mass: 0.52,
    ahead: 3,
    commit: ["Offline mode for the map", 4 * HOUR],
    sessions: [
      session("5f0c2a1e-0000-4000-8000-000000000009", { kind: "finished", detail: "completed" }, 3 * HOUR, "Offline mode is committed on main.", 150_000, false, "unpushedCommits"),
    ],
    preselect: { kind: "resume", sessionId: "5f0c2a1e-0000-4000-8000-000000000009" },
  },
  {
    name: "mailroom",
    host: "github.com",
    language: "JavaScript",
    mass: 0.56,
    commit: ["Retry failed deliveries", 12 * HOUR],
    sessions: [
      session("5f0c2a1e-0000-4000-8000-000000000006", { kind: "interrupted", detail: "unansweredPrompt" }, 7 * HOUR, "The queue tests are green.", 2_600_000),
    ],
    preselect: { kind: "resume", sessionId: "5f0c2a1e-0000-4000-8000-000000000006" },
  },
  {
    name: "shell-kit",
    host: "gitlab.example.com",
    language: "C",
    mass: 0.62,
    changedFiles: 7,
    commit: ["Pipes between built-ins", 8 * HOUR],
    sessions: [
      session("5f0c2a1e-0000-4000-8000-00000000000a", { kind: "finished", detail: "completed" }, 1 * HOUR, "Redirection works for built-ins too.", 120_000, false, "uncommittedChanges"),
    ],
    preselect: { kind: "resume", sessionId: "5f0c2a1e-0000-4000-8000-00000000000a" },
  },
  { name: "notebook-lab", host: "gitlab.example.com", language: "Python", mass: 0.24, commit: ["Problem set 4", 3 * 24 * HOUR] },
  { name: "scratch", host: null, language: null, mass: 0.12, commit: ["wip", 40 * 24 * HOUR] },
  // On a Windows drive, for Move to WSL.
  {
    name: "marigold-site",
    host: "github.com",
    language: "JavaScript",
    mass: 0.48,
    commit: ["Gallery page and image sizes", 5 * HOUR],
    sessions: [
      session("5f0c2a1e-0000-4000-8000-000000000007", { kind: "finished", detail: "completed" }, 6 * HOUR, "The gallery builds and the links work.", 96_000),
    ],
    path: DEMO_WINDOWS_REPO,
  },
];

export function demoRepos(): RepoSnapshot[] {
  return SEEDS.map((s, i) => {
    const path = s.path ?? `/home/demo/projects/${s.name}`;
    const remote = s.host ? { host: s.host, owner: "demo-user", name: s.name } : null;
    const changedFiles = s.changedFiles ?? 0;
    return {
      path,
      name: s.name,
      originUrl: remote ? `git@${remote.host}:${remote.owner}/${remote.name}.git` : null,
      remote,
      branch: "main",
      ahead: remote ? (s.ahead ?? 0) : null,
      behind: remote ? (s.behind ?? 0) : null,
      changedFiles,
      dirtyPaths: Array.from({ length: changedFiles }, (_, i) => `src/file-${i + 1}.txt`),
      lastCommit: { summary: s.commit[0], time: now() - s.commit[1], sha: demoSha(i) },
      language: s.language,
      trackedBytes: Math.round(s.mass * 8_000_000),
      commits30d: Math.round(s.mass * 60),
      mass: s.mass,
      sessions: s.sessions ?? [],
      preselect: s.preselect ?? { kind: "noPicker" },
      refactor: s.refactor ? seededRefactor(s.name) : null,
    };
  });
}

/** An invented abbreviated commit id. */
function demoSha(n: number): string {
  return ((n + 1) * 0x9e3779b1 >>> 0).toString(16).padStart(8, "0").slice(0, 7);
}

// Refactors (.plans/refactor-spec.md), simulated: Starting, then Claude plans, waits on
// you for the plan, works and ends ready with three commits. Merge and Discard act on the
// demo's repos only.

/** How long the simulated refactor stays in each state, in milliseconds. */
export const DEMO_REFACTOR_STEPS: [state: "working" | "needsYou" | "ready", afterMs: number][] = [
  ["working", 1200],
  ["needsYou", 2500],
  ["working", 4000],
  ["ready", 3000],
];

const DEMO_REFACTOR_COMMITS = [
  "Extract the retry policy into its own module",
  "Remove the unused legacy export helpers",
  "Give the request handlers clearer names",
];

/** The test and lint commands a repo in `language` would have. */
function demoChecks(language: string | null): string[] {
  switch (language) {
    case "TypeScript":
    case "JavaScript":
      return ["npm test", "npm run lint"];
    case "Rust":
      return ["cargo test", "cargo clippy"];
    case "Python":
      return ["pytest", "ruff check ."];
    case "Go":
      return ["go test ./...", "go vet ./..."];
    case "Swift":
      return ["swift test"];
    case "Kotlin":
      return ["./gradlew test"];
    case "C#":
      return ["dotnet test"];
    case "Shell":
      return ["shellcheck *.sh"];
    default:
      return ["make test"];
  }
}

/** A stamp like the helper's `20261031-1405`, in local time. */
function stamp(date: Date): string {
  const pad = (n: number) => String(n).padStart(2, "0");
  return `${date.getFullYear()}${pad(date.getMonth() + 1)}${pad(date.getDate())}-${pad(date.getHours())}${pad(date.getMinutes())}`;
}

function newRefactor(name: string, base: string, created = new Date()): RefactorInfo {
  const worktree = `/home/demo/projects/.refract-worktrees/${name}-${stamp(created)}`;
  return {
    state: "starting",
    worktree,
    folderUri: `vscode-remote://wsl+Demo${worktree}`,
    branch: `refactor/${stamp(created)}`,
    targetBranch: "main",
    baseCommit: base.padEnd(40, "0"),
    sessionId: crypto.randomUUID(),
    createdAt: created.toISOString(),
    live: false,
    interrupted: false,
    commits: [],
    filesChanged: 0,
    insertions: 0,
    deletions: 0,
    checks: null,
    summary: null,
    blocked: null,
  };
}

/** The refactor a finished run leaves: three commits, checks before and after, a summary. */
function finishedRefactor(info: RefactorInfo, language: string | null): RefactorInfo {
  return {
    ...info,
    state: "ready",
    live: false,
    commits: DEMO_REFACTOR_COMMITS.map((summary, i) => ({ sha: demoSha(40 + i), summary })),
    filesChanged: 7,
    insertions: 142,
    deletions: 188,
    checks: demoChecks(language).map((command) => ({ command, before: "pass", after: "pass" })),
    summary: "Three refactors, one commit each. Every check that passed before still passes.",
    blocked: null,
  };
}

/** The demo's refactor that is already done: two commits and no result file. */
function seededRefactor(name: string): RefactorInfo {
  const created = new Date(Date.now() - 2 * HOUR * 1000);
  return {
    ...newRefactor(name, demoSha(99), created),
    state: "ready",
    sessionId: "5f0c2a1e-0000-4000-8000-0000000000b1",
    commits: [
      { sha: demoSha(50), summary: "Split the sweep loop out of Program.cs" },
      { sha: demoSha(51), summary: "Drop the unused settings reader" },
    ],
    filesChanged: 3,
    insertions: 61,
    deletions: 94,
  };
}

/** The opening prompt the demo's pretend terminal shows for a refactor. */
export const DEMO_REFACTOR_PROMPT =
  "Refactor this repository: up to 3 behaviour-preserving refactors, one commit each.";

interface RemoteSeed {
  name: string;
  host?: "github.com" | "gitlab.example.com";
  owner?: string;
  language: string | null;
  sizeKb: number;
  pushedDaysAgo: number;
  fork?: boolean;
  archived?: boolean;
  description?: string;
  /** Open pull or merge requests and CI on the default branch, for the detail panel. */
  prs: number;
  ci: CiStatus;
}

// Remote repos: some match the local seeds above, the rest are not cloned.
const REMOTE_SEEDS: RemoteSeed[] = [
  { name: "atlas-api", language: "TypeScript", sizeKb: 48_000, pushedDaysAgo: 0, prs: 3, ci: "success" },
  { name: "tidepool", language: "TypeScript", sizeKb: 31_000, pushedDaysAgo: 1, prs: 1, ci: "failure" },
  { name: "ledger-sync", language: "Python", sizeKb: 12_000, pushedDaysAgo: 4, prs: 0, ci: "pending" },
  { name: "quill", language: "Go", sizeKb: 6_400, pushedDaysAgo: 2, prs: 2, ci: "none" },
  { name: "shell-kit", host: "gitlab.example.com", language: "C", sizeKb: 2_200, pushedDaysAgo: 0, prs: 1, ci: "success" },
  { name: "marigold-site", language: "JavaScript", sizeKb: 14_000, pushedDaysAgo: 0, prs: 0, ci: "success" },
  { name: "orbit-cli", language: "Rust", sizeKb: 9_800, pushedDaysAgo: 12, prs: 1, ci: "success", description: "A small command line for orbital mechanics." },
  { name: "lumen-docs", language: "TypeScript", sizeKb: 2_600, pushedDaysAgo: 30, prs: 0, ci: "failure", description: "Documentation site." },
  { name: "pixel-garden", language: "JavaScript", sizeKb: 640, pushedDaysAgo: 90, prs: 0, ci: "none" },
  { name: "chart-kit", owner: "example-org", language: "TypeScript", sizeKb: 21_000, pushedDaysAgo: 200, fork: true, prs: 0, ci: "success" },
  { name: "old-portfolio", language: null, sizeKb: 310, pushedDaysAgo: 900, archived: true, prs: 0, ci: "none" },
  { name: "parser-project", host: "gitlab.example.com", owner: "example-group/platform", language: "Kotlin", sizeKb: 4_100, pushedDaysAgo: 3, prs: 2, ci: "pending", description: "Shared project in a nested group." },
  { name: "storage-lab", host: "gitlab.example.com", language: "C", sizeKb: 1_300, pushedDaysAgo: 18, prs: 0, ci: "failure" },
];

const DEMO_GITLAB = "https://gitlab.example.com";

function demoRemote(s: RemoteSeed): RemoteRepo {
  const host = s.host ?? "github.com";
  const owner = s.owner ?? "demo-user";
  return {
    kind: host === "github.com" ? "github" : "gitlab",
    host,
    owner,
    name: s.name,
    webUrl: `https://${host}/${owner}/${s.name}`,
    cloneUrl: `https://${host}/${owner}/${s.name}.git`,
    defaultBranch: "main",
    language: s.language,
    sizeKb: s.sizeKb,
    pushedAt: iso(s.pushedDaysAgo * 24 * HOUR),
    fork: s.fork ?? false,
    archived: s.archived ?? false,
    private: host !== "github.com",
    description: s.description ?? null,
  };
}

const DEMO_HOME = "https://github.com/maddoxrjohnson/refract";

function isGithubUrl(url: string): boolean {
  try {
    return new URL(url).hostname === "github.com";
  } catch {
    return false;
  }
}

const sleep = (ms: number) => new Promise((r) => setTimeout(r, ms));

// Status hook (spec §6.2): what installing would change in an invented settings.json.

const DEMO_SETTINGS_PATH = "/home/demo/.claude/settings.json";
const DEMO_HOOK_SCRIPT = "/home/demo/.local/share/refract/status-hook.sh";
const DEMO_STATUS_DIR = "/home/demo/.local/state/refract/sessions";
// The events, statuses and matchers refract-core's `hook::EVENTS` registers.
const HOOK_EVENTS: [event: string, status: string, matcher: string | null][] = [
  ["SessionStart", "finished", "startup|resume|clear"],
  ["UserPromptSubmit", "working", null],
  ["PostToolUse", "working", "*"],
  ["Notification", "waitingOnUser", "permission_prompt|elicitation_dialog"],
  ["PreToolUse", "waitingOnUser", "AskUserQuestion|ExitPlanMode"],
  ["Stop", "finished", null],
  ["SessionEnd", "ended", null],
];

/** The command `hook::command` writes, for the demo's paths. */
function demoHookCommand(status: string): string {
  const script = `'${DEMO_HOOK_SCRIPT}'`;
  return `[ -f ${script} ] && sh ${script} ${status} '${DEMO_STATUS_DIR}' || cat >/dev/null; exit 0`;
}

export function demoHookDiff(): string {
  const entries = HOOK_EVENTS.flatMap(([event, status, matcher], i) => [
    `+    "${event}": [`,
    `+      { ${matcher ? `"matcher": "${matcher}", ` : ""}"hooks": [{ "type": "command", "command": "${demoHookCommand(status)}" }] }`,
    `+    ]${i < HOOK_EVENTS.length - 1 ? "," : ""}`,
  ]);
  const added = ['+  "theme": "dark",', '+  "hooks": {', ...entries, "+  }"];
  return [
    `--- ${DEMO_SETTINGS_PATH}`,
    `+++ ${DEMO_SETTINGS_PATH}`,
    `@@ -1,4 +1,${added.length + 3} @@`,
    " {",
    '   "model": "opus",',
    '-  "theme": "dark"',
    ...added,
    " }",
    "",
  ].join("\n");
}

// Widgets: numbers that drift slowly, so the demo looks alive without jumping about.

const GB = 1024 ** 3;

/** A smooth wobble between 0 and 1 with a period of `periodS` seconds. */
const wave = (t: number, periodS: number, phase = 0) =>
  0.5 + 0.5 * Math.sin((2 * Math.PI * t) / periodS + phase);

function demoStats(t: number): SystemStats {
  return {
    cpuPercent: 14 + 22 * wave(t, 47) + 6 * wave(t, 7, 1),
    memUsedBytes: (13.2 + 1.6 * wave(t, 90, 2)) * GB,
    memTotalBytes: 32 * GB,
    gpuPercent: 9 + 18 * wave(t, 31, 0.5),
    own: {
      cpuPercent: 0.4 + 0.6 * wave(t, 11, 2),
      memBytes: (0.21 + 0.02 * wave(t, 23)) * GB,
      gpuPercent: 1.2 + 1.5 * wave(t, 13, 1),
    },
  };
}

/** Token totals that grow with `scale`, split the way Claude Code's mostly are. */
function totals(scale: number): TokenTotals {
  return {
    input: Math.round(scale * 0.012),
    output: Math.round(scale * 0.038),
    cacheRead: Math.round(scale * 0.86),
    cacheWrite: Math.round(scale * 0.09),
  };
}

/** How the demo's tokens split by model, and the 1-hour share of each one's cache writes. */
const USAGE_MODELS: [model: string, share: number, oneHour: number][] = [
  ["claude-opus-5-5", 0.71, 0.4],
  ["claude-sonnet-5-5", 0.24, 0.1],
  ["claude-haiku-5-5", 0.05, 0],
];

/** `scale` worth of tokens split across the demo's models. */
function byModel(scale: number): ModelUsage[] {
  return USAGE_MODELS.map(([model, share, oneHour]) => {
    const t = totals(scale * share);
    return { model, ...t, cacheWrite1h: Math.round(t.cacheWrite * oneHour) };
  });
}

/**
 * Invented plan limits, as a status line would relay them: about 37% of the 5-hour
 * window and 58% of the week, creeping up, read on the last whole minute. The 5-hour
 * window first resets a little over 2 hours after the demo starts and every 5 hours
 * after that, starting again from 0; the week resets 3 days and 9 hours in.
 */
function demoLimits(started: number): PlanLimits {
  const now = Date.now();
  const FIVE_HOURS = 5 * 3600_000;
  const firstReset = started + 133 * 60_000;
  const windows = Math.max(0, Math.ceil((now - firstReset) / FIVE_HOURS));
  const fiveReset = firstReset + windows * FIVE_HOURS;
  const fiveStart = windows === 0 ? started : fiveReset - FIVE_HOURS;
  const fiveBase = windows === 0 ? 37 : 0;
  const minutes = (now - started) / 60_000;
  return {
    fiveHour: {
      usedPercent: Math.min(99, fiveBase + ((now - fiveStart) / 60_000) * 0.2),
      resetsAt: new Date(fiveReset).toISOString(),
    },
    sevenDay: {
      usedPercent: Math.min(99, 58 + minutes * 0.02),
      resetsAt: new Date(started + 81 * 3600_000).toISOString(),
    },
    updatedAt: new Date(now - (now % 60_000)).toISOString(),
  };
}

const USAGE_REPOS: [name: string, share: number][] = [
  ["tidepool", 0.34],
  ["atlas-api", 0.24],
  ["quill", 0.16],
  ["tessera", 0.12],
  ["mailroom", 0.09],
  ["ledger-sync", 0.05],
];

/**
 * A backend with invented data. Launching only walks through the progress steps and
 * tells `onDemoLaunch` listeners, which show a pretend terminal.
 */
export class DemoBackend implements Backend {
  private repos = demoRepos();
  private progress = new Set<(step: LaunchStep) => void>();
  private events = new Set<(event: AgentEvent) => void>();
  private demoLaunches = new Set<(launch: DemoLaunch) => void>();
  private settings: Settings = {
    gitlabUrl: DEMO_GITLAB,
    showForks: false,
    showArchived: false,
    hidden: [],
    // On in the demo, so it shows what the estimate looks like.
    costEstimate: { enabled: true, prices: [...DEFAULT_PRICES] },
    theme: "system",
  };
  private creds: Credentials = { github: "ghCli", gitlab: "stored" };
  private hookInstalled = false;
  private started = Date.now();
  // Move to WSL: snapshots running by path (true once cancelled), migrations by id.
  private snapshotting = new Map<string, boolean>();
  private snapshotListeners = new Set<(progress: MigrateSnapshotProgress) => void>();
  private demoMoves = new Map<string, DemoMove>();
  private moveCount = 0;
  private moveWindows: VsCodeWindow[] = [{ id: 4242, title: "marigold-site - Visual Studio Code" }];
  private moveOutcome: DemoMoveOutcome;
  // Refactors: the pending step of each repo's simulated run, by path.
  private refactorTimers = new Map<string, ReturnType<typeof setTimeout>>();
  private noticeListeners = new Set<(notice: RefactorNotice) => void>();
  private zoomed: string | null = null;

  /**
   * `moveOutcome` is what Delete original first says; by default the original goes to the
   * Recycle Bin. The page's `?move=wontfit` asks for the permanent-delete question instead,
   * and `?move=unfinished` stops the first delete halfway, as a file in use would.
   */
  constructor(options: { moveOutcome?: DemoMoveOutcome } = {}) {
    let fromPage: DemoMoveOutcome | undefined;
    try {
      const move = new URLSearchParams(globalThis.location?.search ?? "").get("move");
      if (move === "wontfit") fromPage = "wontFit";
      else if (move === "unfinished") fromPage = "unfinished";
    } catch {
      // No page address: the default.
    }
    this.moveOutcome = options.moveOutcome ?? fromPage ?? "deleted";
  }

  private emit(event: AgentEvent) {
    for (const cb of this.events) cb(event);
  }

  agentStatus(): Promise<AgentStatus> {
    return Promise.resolve({ state: "running" });
  }

  onAgentStatus(): Unsubscribe {
    return () => {};
  }

  listRepos(): Promise<RepoList> {
    return Promise.resolve({ repos: this.repos, loading: false });
  }

  getRepo(path: string): Promise<RepoSnapshot> {
    const repo = this.repos.find((r) => r.path === path);
    return repo ? Promise.resolve(repo) : Promise.reject(new Error(`no repo at ${path}`));
  }

  onAgentEvent(cb: (event: AgentEvent) => void): Unsubscribe {
    this.events.add(cb);
    return () => this.events.delete(cb);
  }

  async launch(path: string, mode: LaunchMode): Promise<LaunchTicket> {
    for (const step of ["prepare", "opening", "done"] as const) {
      await sleep(400);
      for (const cb of this.progress) cb(step);
    }
    const sessionId = mode.kind === "new" ? crypto.randomUUID() : mode.sessionId;
    const repoName = this.repos.find((r) => r.path === path)?.name ?? path.split("/").pop() ?? path;
    for (const cb of this.demoLaunches) cb({ repoName, path, mode, sessionId });
    return {
      requestId: crypto.randomUUID(),
      sessionId,
      folderUri: `vscode-remote://wsl+Demo${path}`,
    };
  }

  onDemoLaunch(cb: (launch: DemoLaunch) => void): Unsubscribe {
    this.demoLaunches.add(cb);
    return () => this.demoLaunches.delete(cb);
  }

  onLaunchProgress(cb: (step: LaunchStep) => void): Unsubscribe {
    this.progress.add(cb);
    return () => this.progress.delete(cb);
  }

  isVisible(): Promise<boolean> {
    return Promise.resolve(true);
  }

  onVisibility(): Unsubscribe {
    return () => {};
  }

  openFolder(): Promise<void> {
    return Promise.resolve();
  }

  // The demo's repos, owners and pull requests are invented, but their github.com
  // links would land on whoever owns those names. GitHub links open Refract's own page
  // instead, and any other link opens nothing.
  openUrl(url: string): Promise<void> {
    if (isGithubUrl(url)) window.open(DEMO_HOME, "_blank", "noopener");
    return Promise.resolve();
  }

  // The demo keeps no log file.
  openLogFolder(): Promise<void> {
    return Promise.resolve();
  }

  onGpuUsage(_cb: (usage: GpuUsage) => void): Unsubscribe {
    return () => {};
  }

  async listRemotes(): Promise<RemoteList> {
    await sleep(250);
    const gitlab = this.settings.gitlabUrl === DEMO_GITLAB && this.creds.gitlab === "stored";
    const github = this.creds.github !== "missing";
    const repos = REMOTE_SEEDS.map(demoRemote).filter((r) => (r.kind === "github" ? github : gitlab));
    return {
      repos,
      sources: [
        { kind: "github", host: "github.com", configured: github, error: null },
        ...(this.settings.gitlabUrl
          ? [
              {
                kind: "gitlab" as const,
                host: new URL(this.settings.gitlabUrl).host,
                configured: this.creds.gitlab === "stored",
                error:
                  this.settings.gitlabUrl === DEMO_GITLAB ? null : "The demo only knows gitlab.example.com.",
              },
            ]
          : []),
      ],
      fetchedAt: new Date().toISOString(),
    };
  }

  async remoteDetails(repo: RemoteRef): Promise<RemoteDetails> {
    await sleep(500);
    const seed = REMOTE_SEEDS.find(
      (s) => s.name === repo.name && (s.host ?? "github.com") === repo.host,
    );
    if (!seed) throw { kind: "notFound", message: `${repo.owner}/${repo.name} is not on ${repo.host}.` };
    const remote = demoRemote(seed);
    return {
      openPullRequests: seed.prs,
      ci: seed.ci,
      ciUrl: seed.ci === "none" ? null : `${remote.webUrl}/actions`,
      fetchedAt: iso(7 * 60),
    };
  }

  getSettings(): Promise<Settings> {
    return Promise.resolve(structuredClone(this.settings));
  }

  setSettings(settings: Settings): Promise<void> {
    this.settings = structuredClone(settings);
    return Promise.resolve();
  }

  setWindowTheme(): Promise<void> {
    return Promise.resolve();
  }

  credentials(): Promise<Credentials> {
    return Promise.resolve({ ...this.creds });
  }

  setToken(kind: RemoteKind, token: string | null): Promise<void> {
    if (kind === "github") this.creds.github = token ? "stored" : "ghCli";
    else this.creds.gitlab = token ? "stored" : "missing";
    return Promise.resolve();
  }

  /** Resolves after a moment and adds the clone as a local repo. */
  async cloneRepo(repo: RemoteRepo): Promise<string> {
    const path = `/home/demo/projects/${repo.name}`;
    await sleep(1200);
    if (this.repos.some((r) => r.path === path)) return path;
    const snapshot: RepoSnapshot = {
      path,
      name: repo.name,
      originUrl: repo.cloneUrl,
      remote: { host: repo.host, owner: repo.owner, name: repo.name },
      branch: repo.defaultBranch,
      ahead: 0,
      behind: 0,
      changedFiles: 0,
      dirtyPaths: [],
      lastCommit: { summary: "Initial commit", time: now() - 3 * 24 * HOUR },
      language: repo.language,
      trackedBytes: repo.sizeKb * 1024,
      commits30d: 4,
      mass: 0.3,
      sessions: [],
      preselect: { kind: "noPicker" },
    };
    this.repos = [...this.repos, snapshot];
    this.emit({ event: "repoChanged", data: snapshot });
    return path;
  }

  async addRepo(path: string): Promise<void> {
    await sleep(300);
    const name = path.replace(/\/+$/, "").split("/").pop();
    if (!path.startsWith("/") || !name) throw { kind: "invalid", message: `Not a folder: ${path}` };
    if (this.repos.some((r) => r.path === path)) return;
    const snapshot: RepoSnapshot = {
      path,
      name,
      originUrl: null,
      remote: null,
      branch: "main",
      ahead: null,
      behind: null,
      changedFiles: 0,
      dirtyPaths: [],
      lastCommit: { summary: "Initial commit", time: now() - HOUR },
      language: null,
      trackedBytes: 0,
      commits30d: 1,
      mass: 0.15,
      sessions: [],
      preselect: { kind: "noPicker" },
    };
    this.repos = [...this.repos, snapshot];
    this.emit({ event: "repoChanged", data: snapshot });
  }

  /** Replaces the repo at `path` with `change(repo)` and reports it. */
  private updateRepo(path: string, change: (repo: RepoSnapshot) => RepoSnapshot): RepoSnapshot | undefined {
    const repo = this.repos.find((r) => r.path === path);
    if (!repo) return undefined;
    const next = change(repo);
    this.repos = this.repos.map((r) => (r.path === path ? next : r));
    this.emit({ event: "repoChanged", data: next });
    return next;
  }

  private setRefactor(path: string, change: (info: RefactorInfo, repo: RepoSnapshot) => RefactorInfo | null) {
    return this.updateRepo(path, (repo) => (repo.refactor ? { ...repo, refactor: change(repo.refactor, repo) } : repo));
  }

  /** Walks the refactor through `steps`, each after its delay. */
  private runRefactor(path: string, steps: typeof DEMO_REFACTOR_STEPS) {
    clearTimeout(this.refactorTimers.get(path));
    const [first, ...rest] = steps;
    if (!first) {
      this.refactorTimers.delete(path);
      return;
    }
    const [state, afterMs] = first;
    this.refactorTimers.set(
      path,
      setTimeout(() => {
        const before = this.repos.find((r) => r.path === path)?.refactor?.state;
        const repo = this.setRefactor(path, (info, repo) =>
          state === "ready" ? finishedRefactor(info, repo.language) : { ...info, state, live: true, blocked: null },
        );
        // As the Rust side does: the plan and the finish, told outside the zoomed repo.
        const alert = state === "needsYou" ? "planReady" : state === "ready" ? "finished" : null;
        if (repo && alert && before !== state && path !== this.zoomed) {
          for (const cb of this.noticeListeners) cb({ path, repoName: repo.name, alert });
        }
        this.runRefactor(path, rest);
      }, afterMs),
    );
  }

  private refactorFail(message: string): never {
    throw { kind: "refactor", message };
  }

  private demoRefactorLaunch(path: string, mode: LaunchMode, sessionId: string) {
    const repo = this.repos.find((r) => r.path === path);
    if (repo) for (const cb of this.demoLaunches) cb({ repoName: repo.name, path, mode, sessionId });
  }

  async startRefactor(path: string, focus?: string): Promise<LaunchTicket> {
    const repo = this.repos.find((r) => r.path === path);
    if (!repo) throw { kind: "agent", message: `No repo at ${path}.` };
    if (repo.refactor) throw { kind: "agent", message: "This repo already has a refactor." };
    if (!repo.branch) throw { kind: "agent", message: "Check out a branch first" };
    const info = newRefactor(repo.name, repo.lastCommit?.sha ?? demoSha(0));
    this.updateRepo(path, (r) => ({ ...r, refactor: info }));
    for (const step of ["prepare", "opening", "done"] as const) {
      await sleep(400);
      for (const cb of this.progress) cb(step);
    }
    const prompt = focus?.trim() ? `${DEMO_REFACTOR_PROMPT} Focus on: ${focus.trim()}` : DEMO_REFACTOR_PROMPT;
    this.demoRefactorLaunch(path, { kind: "new", prompt, planMode: true }, info.sessionId);
    this.runRefactor(path, DEMO_REFACTOR_STEPS);
    return { requestId: crypto.randomUUID(), sessionId: info.sessionId, folderUri: info.folderUri };
  }

  async resumeRefactor(path: string, fix: boolean): Promise<LaunchTicket> {
    const info = this.repos.find((r) => r.path === path)?.refactor;
    if (!info) throw { kind: "agent", message: "This repo has no refactor." };
    if (info.live) throw { kind: "agent", message: "The refactor agent is still running." };
    for (const step of ["prepare", "opening", "done"] as const) {
      await sleep(400);
      for (const cb of this.progress) cb(step);
    }
    const prompt = fix ? "Rebase the refactor onto main and resolve the conflicts." : "Continue the refactor from where it stopped.";
    this.demoRefactorLaunch(path, { kind: "resume", sessionId: info.sessionId, prompt }, info.sessionId);
    this.setRefactor(path, (i) => ({ ...i, state: "working", live: true, interrupted: false, blocked: null }));
    this.runRefactor(path, [["ready", 3000]]);
    return { requestId: crypto.randomUUID(), sessionId: info.sessionId, folderUri: info.folderUri };
  }

  /** Merges unless an uncommitted file is one the refactor changed (the demo's refactor touches `src/file-1.txt`). */
  async mergeRefactor(path: string): Promise<MergeOutcome> {
    await sleep(700);
    const repo = this.repos.find((r) => r.path === path);
    const info = repo?.refactor;
    if (!repo || !info) this.refactorFail("This repo has no refactor.");
    if (info.live) return { outcome: "blocked", reason: "live", files: [] };
    const clash = repo.dirtyPaths.filter((p) => p === "src/file-1.txt");
    if (clash.length > 0) {
      this.setRefactor(path, (i) => ({ ...i, state: "mergeBlocked", blocked: { reason: "dirtyOverlap", files: clash } }));
      return { outcome: "blocked", reason: "dirtyOverlap", files: clash };
    }
    const commits = info.commits.length;
    const last = info.commits[commits - 1];
    clearTimeout(this.refactorTimers.get(path));
    this.updateRepo(path, (r) => ({
      ...r,
      refactor: null,
      lastCommit: last ? { summary: last.summary, time: now(), sha: last.sha } : r.lastCommit,
      ahead: r.ahead === null ? null : r.ahead + commits,
      commits30d: r.commits30d + commits,
    }));
    return { outcome: "merged", commits };
  }

  async discardRefactor(path: string): Promise<void> {
    await sleep(500);
    const info = this.repos.find((r) => r.path === path)?.refactor;
    if (!info) this.refactorFail("This repo has no refactor.");
    if (info.live) this.refactorFail("The refactor agent is still running. Close it first.");
    clearTimeout(this.refactorTimers.get(path));
    this.refactorTimers.delete(path);
    this.updateRepo(path, (r) => ({ ...r, refactor: null }));
  }

  onRefactorNotice(cb: (notice: RefactorNotice) => void): Unsubscribe {
    this.noticeListeners.add(cb);
    return () => this.noticeListeners.delete(cb);
  }

  setZoomed(path: string | null): Promise<void> {
    this.zoomed = path;
    return Promise.resolve();
  }

  /** Shows the pretend terminal of the refactor's window. */
  openRefactor(path: string): Promise<void> {
    const repo = this.repos.find((r) => r.path === path);
    if (repo?.refactor) {
      this.demoRefactorLaunch(repo.path, { kind: "focus", sessionId: repo.refactor.sessionId }, repo.refactor.sessionId);
    }
    return Promise.resolve();
  }

  hookInfo(): Promise<HookInfo> {
    return Promise.resolve({
      installed: this.hookInstalled,
      outdated: false,
      settingsPath: DEMO_SETTINGS_PATH,
      diff: this.hookInstalled ? null : demoHookDiff(),
    });
  }

  async installHook(): Promise<HookInfo> {
    await sleep(400);
    this.hookInstalled = true;
    return this.hookInfo();
  }

  async uninstallHook(): Promise<HookInfo> {
    await sleep(400);
    this.hookInstalled = false;
    return this.hookInfo();
  }

  /** Totals that creep up while the demo runs, as if the live agents were working. */
  usage(): Promise<UsageSummary> {
    const minutes = (Date.now() - this.started) / 60_000;
    const today = 4_200_000 + minutes * 38_000;
    const week = 31_000_000 + minutes * 38_000;
    const five = 1_900_000 + minutes * 38_000;
    const live = this.repos.flatMap((r) => r.sessions).filter((s) => s.state.kind === "live").length;
    return Promise.resolve({
      today: totals(today),
      last7Days: totals(week),
      last5Hours: totals(five),
      liveAgents: live,
      byRepo: USAGE_REPOS.map(([name, share]) => ({
        path: `/home/demo/projects/${name}`,
        name,
        last7Days: totals(week * share),
      })),
      byModel: { today: byModel(today), last7Days: byModel(week), last5Hours: byModel(five) },
      limits: demoLimits(this.started),
      computedAt: new Date().toISOString(),
      ready: true,
    });
  }

  onSystemStats(cb: (stats: SystemStats) => void): Unsubscribe {
    const tick = () => cb(demoStats((Date.now() - this.started) / 1000));
    tick();
    const timer = setInterval(tick, 2000);
    return () => clearInterval(timer);
  }

  // Move to WSL, simulated (spec §7): a preflight with one VS Code window to close, a
  // snapshot, every helper step, a passing report and the Recycle Bin. Nothing is copied.

  private moveFail(kind: MigrateError["kind"], message: string): never {
    const error: MigrateError = { kind, message };
    throw error;
  }

  /** Sends `migrate://snapshot` ticks through `phases`; stops on `migrateCancelSnapshot`. */
  private async demoSnapshot(path: string, phases: MigrateSnapshotProgress["phase"][], tickMs: number) {
    if (this.snapshotting.has(path)) this.moveFail("busy", "This folder is already being checked.");
    this.snapshotting.set(path, false);
    try {
      const bytesTotal = DEMO_MOVE_TOTALS.bytes;
      const filesTotal = DEMO_MOVE_TOTALS.files;
      for (const phase of phases) {
        for (let i = 1; i <= DEMO_TICKS; i++) {
          if (this.snapshotting.get(path)) this.moveFail("cancelled", "Cancelled.");
          const share = i / DEMO_TICKS;
          const progress: MigrateSnapshotProgress = {
            path,
            phase,
            filesDone: Math.round(filesTotal * share),
            filesTotal,
            bytesDone: phase === "hashing" ? Math.round(bytesTotal * share) : 0,
            bytesTotal: phase === "hashing" ? bytesTotal : 0,
          };
          for (const cb of this.snapshotListeners) cb(progress);
          await sleep(tickMs);
        }
      }
      if (this.snapshotting.get(path)) this.moveFail("cancelled", "Cancelled.");
    } finally {
      this.snapshotting.delete(path);
    }
  }

  private demoRefusals(dest: string) {
    if (!dest.startsWith("/") || /^\/mnt\/[a-z](\/|$)/i.test(dest)) {
      return [{ kind: "destNotLinux" as const, message: "The destination isn't on the Linux filesystem." }];
    }
    if (this.repos.some((r) => r.path === dest.replace(/\/+$/, ""))) {
      return [{ kind: "destExists" as const, message: "The destination already exists. Pick another folder." }];
    }
    return [];
  }

  async migratePreflight(path: string, dest?: string): Promise<MigratePreflight> {
    const repo = this.repos.find((r) => r.path === path);
    if (!repo || !path.startsWith("/mnt/")) {
      const error: MigrateError = {
        kind: "refused",
        message: "This repo isn't on a Windows drive.",
        refusals: [{ kind: "notOnWindowsDrive", message: "This repo isn't on a Windows drive." }],
      };
      throw error;
    }
    await this.demoSnapshot(path, ["walking", "probing"], 120);
    const target = dest ?? `/home/demo/projects/${repo.name}`;
    return {
      source: path,
      windowsPath: DEMO_WINDOWS_PATH,
      name: repo.name,
      dest: target,
      refusals: this.demoRefusals(target),
      warnings: [],
      ...DEMO_MOVE_TOTALS,
      skipped: DEMO_MOVE_SKIPPED,
      install: [{ ...DEMO_MOVE_INSTALL, missing: false }],
      droppedStreams: 1,
      historySessions: 1,
      vscodeWindows: [...this.moveWindows],
      linuxGit: false,
      cloudSynced: false,
    };
  }

  async migrateStart(path: string, dest: string): Promise<{ id: string }> {
    if ([...this.demoMoves.values()].some((m) => m.source === path)) {
      this.moveFail("busy", "This folder is already being moved.");
    }
    if (this.moveWindows.length > 0) {
      this.moveFail("busy", "Close the VS Code windows that have this folder open first.");
    }
    const refusals = this.demoRefusals(dest);
    if (refusals.length > 0) {
      const error: MigrateError = { kind: "refused", message: refusals[0].message, refusals };
      throw error;
    }
    await this.demoSnapshot(path, ["git", "hashing", "blobs"], 120);
    const move: DemoMove = {
      id: `demo-move-${++this.moveCount}`,
      source: path,
      dest,
      report: null,
      cancelled: false,
      deleting: false,
      placed: false,
      stopped: false,
    };
    this.demoMoves.set(move.id, move);
    void this.runMove(move, DEMO_MOVE_STEPS);
    return { id: move.id };
  }

  /**
   * Walks `steps` with progress events, then sends a passing report. Stopped once the copy is in place (install
   * or history, or a re-sync), it sends a report saying so instead, as the helper does.
   */
  private async runMove(move: DemoMove, steps: [MigrateStep, number][], resync = false) {
    move.stopped = false;
    walk: for (const [step, total] of steps) {
      if (step === "installing" || step === "movingHistory") move.placed = true;
      for (let i = 1; i <= DEMO_TICKS; i++) {
        await sleep(step === "copying" ? 260 : 130);
        if (move.cancelled) return;
        if (move.stopped) break walk;
        const done = Math.round((total * i) / DEMO_TICKS);
        this.emit({ event: "migrateProgress", data: { id: move.id, source: move.source, dest: move.dest, step, done, total, placed: move.placed } });
      }
    }
    move.report = demoMoveReport(move, move.stopped ? (resync ? "resync" : "install") : null);
    this.emit({ event: "migrateReady", data: { id: move.id, report: move.report } });
  }

  private readyMove(id: string): DemoMove & { report: MigrationReport } {
    const move = this.demoMoves.get(id);
    if (!move) this.moveFail("notReady", `No migration ${id} is known.`);
    if (!move.report) this.moveFail("notReady", "The move hasn't finished its checks yet.");
    return move as DemoMove & { report: MigrationReport };
  }

  migrateCancelSnapshot(path: string): Promise<boolean> {
    if (!this.snapshotting.has(path)) return Promise.resolve(false);
    this.snapshotting.set(path, true);
    return Promise.resolve(true);
  }

  /** As the helper refuses everything but finishing once the delete has begun. */
  private notDeleting(move: DemoMove) {
    if (move.deleting) this.moveFail("notReady", "The original is being deleted; finish deleting it.");
  }

  async migrateCancel(id: string): Promise<CancelOutcome> {
    const move = this.demoMoves.get(id);
    if (move) this.notDeleting(move);
    if (move?.report) this.moveFail("notReady", "The copy is no longer running; answer its report instead.");
    if (move?.placed) {
      // The copy is in place: it stays, and the report follows.
      move.stopped = true;
      await sleep(300);
      return { ended: false };
    }
    if (move) move.cancelled = true;
    this.demoMoves.delete(id);
    await sleep(300);
    return { ended: true };
  }

  migrateWindows(_path: string): Promise<VsCodeWindow[]> {
    return Promise.resolve([...this.moveWindows]);
  }

  async migrateCloseWindow(_path: string, id: number): Promise<boolean> {
    await sleep(300);
    const before = this.moveWindows.length;
    this.moveWindows = this.moveWindows.filter((w) => w.id !== id);
    return this.moveWindows.length < before;
  }

  async migrateStopAgent(path: string, sessionId: string): Promise<void> {
    await sleep(300);
    const repo = this.repos.find((r) => r.path === path);
    if (!repo) return;
    const next: RepoSnapshot = {
      ...repo,
      sessions: repo.sessions.map((s) =>
        s.id === sessionId ? { ...s, state: { kind: "finished", detail: "userInterrupted" } } : s,
      ),
    };
    this.repos = this.repos.map((r) => (r.path === path ? next : r));
    this.emit({ event: "repoChanged", data: next });
  }

  async migrateResync(id: string): Promise<void> {
    const move: DemoMove = this.readyMove(id);
    this.notDeleting(move);
    move.report = null;
    void this.runMove(move, [["rechecking", 12]], true);
  }

  async migrateDelete(id: string, permanent: boolean, _force: Forced = NO_FORCE): Promise<DeleteOutcome> {
    const move = this.readyMove(id);
    await sleep(700);
    if (this.moveOutcome === "wontFit" && !permanent && !move.deleting) {
      return { kind: "wontFit", reason: "The Recycle Bin on C: holds 1.5 GB, and this needs 1.8 GB.", deleting: false };
    }
    // migrateBeginDelete: the recheck passes and the delete begins.
    const firstTry = !move.deleting;
    move.deleting = true;
    if (this.moveOutcome === "unfinished" && firstTry) {
      return {
        kind: "unfinished",
        reason: "Cannot delete the original: The process cannot access the file because it is being used by another process.",
      };
    }
    this.demoMoves.delete(id);
    const original = this.repos.find((r) => r.path === move.source);
    this.repos = this.repos.filter((r) => r.path !== move.source);
    this.emit({ event: "repoRemoved", data: { path: move.source } });
    if (original) {
      const moved: RepoSnapshot = { ...original, path: move.dest, name: move.dest.split("/").pop() ?? original.name };
      this.repos = [...this.repos, moved];
      this.emit({ event: "repoChanged", data: moved });
    }
    return { kind: "deleted", permanent, alreadyGone: false, shortcut: `${DEMO_WINDOWS_PATH}.lnk` };
  }

  async migrateFinish(id: string, _action: FinishAction): Promise<void> {
    this.notDeleting(this.readyMove(id));
    await sleep(300);
    this.demoMoves.delete(id);
  }

  migratePending(): Promise<PendingMigration[]> {
    return Promise.resolve(
      [...this.demoMoves.values()].flatMap((m) =>
        m.report ? [{ id: m.id, report: m.report, ...(m.deleting ? { deleting: true } : {}) }] : [],
      ),
    );
  }

  migrateSourceExists(id: string): Promise<boolean> {
    return Promise.resolve(this.demoMoves.has(id));
  }

  onMigrateSnapshot(cb: (progress: MigrateSnapshotProgress) => void): Unsubscribe {
    this.snapshotListeners.add(cb);
    return () => this.snapshotListeners.delete(cb);
  }
}

// Move to WSL in the demo: an invented web project on the Windows desktop.

const MB = 1024 ** 2;
const DEMO_WINDOWS_PATH = "C:\\Users\\demo\\Desktop\\marigold-site";
/** Progress events per step. */
const DEMO_TICKS = 4;
const DEMO_MOVE_TOTALS = { files: 2_418, dirs: 212, bytes: Math.round(1.3 * GB) };
const DEMO_MOVE_SKIPPED: SkippedFolder[] = [
  { path: "node_modules", rule: "nodeModules", bytes: 412 * MB, files: 38_210 },
  { path: ".next", rule: "toolCache", bytes: 38 * MB, files: 1_204 },
];
const DEMO_MOVE_INSTALL: InstallStep = {
  folder: "node_modules",
  tool: "npm",
  command: "npm ci",
  installHint: "curl -o- https://raw.githubusercontent.com/nvm-sh/nvm/v0.40.3/install.sh | bash && nvm install --lts",
};
/** The helper's steps, with what each counts (bytes for copying, items otherwise). */
const DEMO_MOVE_STEPS: [MigrateStep, number][] = [
  ["copying", DEMO_MOVE_TOTALS.bytes],
  ["normalising", 1_906],
  ["hashing", DEMO_MOVE_TOTALS.files],
  ["checkingGit", 6],
  ["rechecking", DEMO_MOVE_TOTALS.files],
  ["installing", 1],
  ["movingHistory", 1],
];

interface DemoMove {
  id: string;
  source: string;
  dest: string;
  /** Set once the checks are done. */
  report: MigrationReport | null;
  cancelled: boolean;
  /** Delete original has begun (the helper's deleting state). */
  deleting: boolean;
  /** The copy is at its destination: a cancel keeps it. */
  placed: boolean;
  /** Cancelled once placed: the step at hand stops, and a report says so. */
  stopped: boolean;
}

/**
 * What `migrateDelete` first says: `wontFit` shows the permanent-delete question, and
 * `unfinished` stops the first delete halfway, so the next one finishes it.
 */
export type DemoMoveOutcome = "deleted" | "wontFit" | "unfinished";

/** The checks as the helper names them (refract-core `Check::title`). */
const DEMO_CHECKS = [
  ["files", "Every file matches"],
  ["gitState", "Git state is identical"],
  ["nothingSkipped", "Nothing skipped without you knowing"],
  ["unchanged", "Nothing touched it during the copy"],
  ["history", "History copied"],
] as const;

/**
 * A report where every check passed and `npm ci` ran; or, `stopped` once the copy was in place, one where the
 * install (and history) or the re-sync didn't finish.
 */
function demoMoveReport(move: DemoMove, stopped: "install" | "resync" | null = null): MigrationReport {
  const name = move.dest.split("/").pop() ?? "repo";
  const stamp = new Date();
  const report: MigrationReport = {
    version: 1,
    id: move.id,
    source: move.source,
    dest: move.dest,
    startedAt: new Date(stamp.getTime() - 9_000).toISOString(),
    finishedAt: stamp.toISOString(),
    checks: DEMO_CHECKS.map(([check, title]) => ({
      check,
      title,
      status: "passed",
      problems: [],
    })),
    skipped: DEMO_MOVE_SKIPPED,
    droppedStreams: [{ path: "assets/hero.jpg", stream: "Zone.Identifier", size: 26 }],
    newObjects: 312,
    sourceTotals: DEMO_MOVE_TOTALS,
    copyTotals: DEMO_MOVE_TOTALS,
    installChanges: { created: [], changed: [], removed: [] },
    install: [
      {
        step: DEMO_MOVE_INSTALL,
        status: "succeeded",
        exitCode: 0,
        logPath: `/home/demo/.local/share/refract/migrations/demo-${name}-install.log`,
      },
    ],
    history: {
      sessions: 1,
      fromDirs: ["/home/demo/.claude/projects/-mnt-c-Users-demo-Desktop-marigold-site"],
      toDirs: [`/home/demo/.claude/projects/${move.dest.replace(/[/.]/g, "-")}`],
    },
    linuxGit: false,
    warnings: [],
    placed: true,
  };
  if (stopped === "install") {
    report.install = report.install.map((r) => ({ ...r, status: "cancelled", exitCode: undefined }));
    report.history = { ...report.history, sessions: 0 };
    report.checks = report.checks.map((c) =>
      c.check === "history"
        ? {
            ...c,
            status: "failed",
            problems: [
              { message: "You stopped the move before the session history was moved, so the old transcripts stay where they are." },
            ],
          }
        : c,
    );
    report.warnings = [
      "You stopped the move once the copy was in place, so the copy was kept and the rest of the install and history steps didn't run.",
    ];
  } else if (stopped === "resync") {
    report.checks = report.checks.map((c) =>
      c.check === "history"
        ? c
        : c.check === "unchanged"
          ? {
              ...c,
              status: "failed",
              problems: [
                {
                  message:
                    "You stopped the re-sync. The copy is part way between the original as it was checked and as it is now. Re-sync again to finish it, or keep both, or discard the copy.",
                },
              ],
            }
          : { ...c, status: "notRun", problems: [] },
    );
  }
  return report;
}
