import type { Backend, Unsubscribe } from "./Backend";
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
): SessionInfo => ({ id, state, lastTimestamp: iso(secondsAgo), lastAssistantText: text, byteSize });

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
      session("5f0c2a1e-0000-4000-8000-000000000005", { kind: "live", detail: "waitingOnUser" }, 120, "Should I also update the benchmarks?", 140_000),
    ],
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

const sleep = (ms: number) => new Promise((r) => setTimeout(r, ms));

// Status hook (spec §6.2): what installing would change in an invented settings.json.

const DEMO_SETTINGS_PATH = "/home/demo/.claude/settings.json";
const DEMO_HOOK_SCRIPT = "/home/demo/.local/share/refract/status-hook.sh";
const HOOK_EVENTS: [event: string, status: string][] = [
  ["SessionStart", "finished"],
  ["UserPromptSubmit", "working"],
  ["PostToolUse", "working"],
  ["Notification", "waitingOnUser"],
  ["Stop", "finished"],
  ["SessionEnd", "ended"],
];

function demoHookDiff(): string {
  const entries = HOOK_EVENTS.flatMap(([event, status], i) => [
    `+    "${event}": [`,
    `+      { "hooks": [{ "type": "command", "command": "sh '${DEMO_HOOK_SCRIPT}' ${status}" }] }`,
    `+    ]${i < HOOK_EVENTS.length - 1 ? "," : ""}`,
  ]);
  return [
    `--- ${DEMO_SETTINGS_PATH}`,
    `+++ ${DEMO_SETTINGS_PATH}`,
    "@@ -1,4 +1,23 @@",
    " {",
    '   "model": "opus",',
    '-  "theme": "dark"',
    '+  "theme": "dark",',
    '+  "hooks": {',
    ...entries,
    "+  }",
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

const USAGE_REPOS: [name: string, share: number][] = [
  ["tidepool", 0.34],
  ["atlas-api", 0.24],
  ["quill", 0.16],
  ["tessera", 0.12],
  ["mailroom", 0.09],
  ["ledger-sync", 0.05],
];

/** A backend with invented data. Launching only walks through the progress steps. */
export class DemoBackend implements Backend {
  private repos = demoRepos();
  private progress = new Set<(step: LaunchStep) => void>();
  private events = new Set<(event: AgentEvent) => void>();
  private settings: Settings = {
    gitlabUrl: DEMO_GITLAB,
    showForks: false,
    showArchived: false,
    hidden: [],
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
    return {
      requestId: crypto.randomUUID(),
      sessionId: mode.kind === "resume" ? mode.sessionId : crypto.randomUUID(),
      folderUri: `vscode-remote://wsl+Demo${path}`,
    };
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

  openUrl(url: string): Promise<void> {
    window.open(url, "_blank", "noopener");
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
    const live = this.repos.flatMap((r) => r.sessions).filter((s) => s.state.kind === "live").length;
    return Promise.resolve({
      today: totals(today),
      last7Days: totals(week),
      last5Hours: totals(1_900_000 + minutes * 38_000),
      liveAgents: live,
      byRepo: USAGE_REPOS.map(([name, share]) => ({
        path: `/home/demo/projects/${name}`,
        name,
        last7Days: totals(week * share),
      })),
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
}
