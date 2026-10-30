import type { Backend, Unsubscribe } from "./Backend";
import type {
  AgentEvent,
  AgentStatus,
  GpuUsage,
  LaunchMode,
  LaunchStep,
  LaunchTicket,
  Preselect,
  RepoList,
  RepoSnapshot,
  SessionInfo,
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
    name: "os-shell-lab",
    host: "gitlab.example.com",
    language: "C",
    mass: 0.62,
    changedFiles: 7,
    commit: ["Pipes between built-ins", 8 * HOUR],
    preselect: { kind: "newAgent" },
  },
  { name: "ml-homework", host: "gitlab.example.com", language: "Python", mass: 0.24, commit: ["Problem set 4", 3 * 24 * HOUR] },
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

const sleep = (ms: number) => new Promise((r) => setTimeout(r, ms));

/** A backend with invented data. Launching only walks through the progress steps. */
export class DemoBackend implements Backend {
  private repos = demoRepos();
  private progress = new Set<(step: LaunchStep) => void>();

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

  onAgentEvent(_cb: (event: AgentEvent) => void): Unsubscribe {
    return () => {};
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
}
