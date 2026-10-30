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
  ModelUsage,
  PlanLimits,
  Preselect,
  RemoteDetails,
  RemoteKind,
  RemoteList,
  RemoteRef,
  RemoteRepo,
  RepoList,
  RepoSnapshot,
  SessionInfo,
  Settings,
  SystemStats,
  TokenTotals,
  UsageSummary,
} from "./types";
import type {
  DeleteOutcome,
  MigrateError,
  MigratePreflight,
  PendingMigration,
  VsCodeWindow,
} from "./types";
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
}

const session = (
  id: string,
  state: SessionInfo["state"],
  secondsAgo: number,
  text: string | null,
  byteSize: number,
  focusable = false,
): SessionInfo => ({ id, state, lastTimestamp: iso(secondsAgo), lastAssistantText: text, byteSize, focusable });

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
  { name: "trailhead-ios", host: "github.com", language: "Swift", mass: 0.46, commit: ["Bump the deployment target", 9 * 24 * HOUR] },
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
  { name: "sweep", host: "github.com", language: "C#", mass: 0.18, commit: ["Initial commit", 60 * 24 * HOUR] },
  { name: "harbor-android", host: "github.com", language: "Kotlin", mass: 0.52, commit: ["Offline mode for the map", 6 * 24 * HOUR] },
  {
    name: "mailroom",
    host: "github.com",
    language: "JavaScript",
    mass: 0.56,
    commit: ["Retry failed deliveries", 12 * HOUR],
    sessions: [
      session("5f0c2a1e-0000-4000-8000-000000000006", { kind: "interrupted", detail: "unansweredPrompt" }, 7 * HOUR, "The queue tests are green.", 2_600_000),
    ],
    preselect: { kind: "newAgent" },
  },
  {
    name: "shell-kit",
    host: "gitlab.example.com",
    language: "C",
    mass: 0.62,
    changedFiles: 7,
    commit: ["Pipes between built-ins", 8 * HOUR],
    preselect: { kind: "newAgent" },
  },
  { name: "notebook-lab", host: "gitlab.example.com", language: "Python", mass: 0.24, commit: ["Problem set 4", 3 * 24 * HOUR] },
  { name: "scratch", host: null, language: null, mass: 0.12, commit: ["wip", 40 * 24 * HOUR] },
];

export function demoRepos(): RepoSnapshot[] {
  return SEEDS.map((s) => {
    const path = `/home/demo/projects/${s.name}`;
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
      lastCommit: { summary: s.commit[0], time: now() - s.commit[1] },
      language: s.language,
      trackedBytes: Math.round(s.mass * 8_000_000),
      commits30d: Math.round(s.mass * 60),
      mass: s.mass,
      sessions: s.sessions ?? [],
      preselect: s.preselect ?? { kind: "noPicker" },
    };
  });
}

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
  };
  private creds: Credentials = { github: "ghCli", gitlab: "stored" };
  private hookInstalled = false;
  private started = Date.now();

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

  hookInfo(): Promise<HookInfo> {
    return Promise.resolve({
      installed: this.hookInstalled,
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

  // Move to WSL is not in the demo yet (spec §7 has it simulated): asking for one is
  // refused, and the lists are empty.

  private noMigration<T>(): Promise<T> {
    const error: MigrateError = { kind: "internal", message: "Move to WSL isn't in the demo yet." };
    return Promise.reject(error);
  }

  migratePreflight(): Promise<MigratePreflight> {
    return this.noMigration();
  }

  migrateStart(): Promise<{ id: string }> {
    return this.noMigration();
  }

  migrateCancelSnapshot(): Promise<boolean> {
    return Promise.resolve(false);
  }

  migrateCancel(): Promise<void> {
    return this.noMigration();
  }

  migrateWindows(): Promise<VsCodeWindow[]> {
    return Promise.resolve([]);
  }

  migrateCloseWindow(): Promise<boolean> {
    return Promise.resolve(false);
  }

  migrateStopAgent(): Promise<void> {
    return this.noMigration();
  }

  migrateResync(): Promise<void> {
    return this.noMigration();
  }

  migrateDelete(): Promise<DeleteOutcome> {
    return this.noMigration();
  }

  migrateFinish(): Promise<void> {
    return this.noMigration();
  }

  migratePending(): Promise<PendingMigration[]> {
    return Promise.resolve([]);
  }

  onMigrateSnapshot(): Unsubscribe {
    return () => {};
  }
}
