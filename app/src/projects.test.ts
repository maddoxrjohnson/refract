import { describe, expect, it } from "vitest";
import type { RemoteList, RemoteRepo, Settings } from "./backend/types";
import {
  describeKey,
  hideKey,
  mergeProjects,
  projectKey,
  REMOTE_MAX_MASS,
  remoteKey,
  remoteMass,
} from "./projects";
import { testRepo } from "./ui/testRepo";

const SETTINGS: Settings = { gitlabUrl: null, showForks: false, showArchived: false, hidden: [] };

function remote(name: string, extra: Partial<RemoteRepo> = {}): RemoteRepo {
  return {
    kind: "github",
    host: "github.com",
    owner: "demo-user",
    name,
    webUrl: `https://github.com/demo-user/${name}`,
    cloneUrl: `https://github.com/demo-user/${name}.git`,
    defaultBranch: "main",
    language: "Rust",
    sizeKb: 2048,
    pushedAt: null,
    fork: false,
    archived: false,
    private: false,
    description: null,
    ...extra,
  };
}

const list = (...repos: RemoteRepo[]): RemoteList => ({
  repos,
  sources: [],
  fetchedAt: "2026-10-30T12:00:00Z",
});

describe("keys", () => {
  it("uses the path for local repos and remote:<host>/<owner>/<name> for remote ones", () => {
    expect(projectKey(testRepo())).toBe("/home/demo/projects/tidepool");
    expect(projectKey(remote("Orbit", { owner: "Demo-User" }))).toBe("remote:github.com/demo-user/orbit");
    expect(remoteKey({ host: "gitlab.example.com", owner: "example-group/sub", name: "app" })).toBe(
      "remote:gitlab.example.com/example-group/sub/app",
    );
  });

  it("hides a local repo by its remote when it has one", () => {
    expect(hideKey(testRepo())).toBe("remote:github.com/demo-user/tidepool");
    expect(hideKey(testRepo({ remote: null }))).toBe("/home/demo/projects/tidepool");
  });

  it("describes keys for the hidden list", () => {
    expect(describeKey("remote:gitlab.example.com/example-group/sub/app")).toBe("example-group/sub/app · gitlab.example.com");
    expect(describeKey("/home/demo/projects/scratch")).toBe("/home/demo/projects/scratch");
  });
});

describe("mergeProjects", () => {
  it("keeps local repos and adds unmatched remotes as remote-only gems", () => {
    const local = testRepo();
    const { repos, remotes, remoteOnly } = mergeProjects(
      [local],
      list(remote("TIDEPOOL", { owner: "Demo-User" }), remote("orbit-cli")),
      SETTINGS,
    );
    expect(repos.map((r) => r.path)).toEqual([local.path, "remote:github.com/demo-user/orbit-cli"]);
    expect(repos[0]).toBe(local);
    expect(remotes.get(local.path)?.name).toBe("TIDEPOOL");
    expect([...remoteOnly]).toEqual(["remote:github.com/demo-user/orbit-cli"]);
    const gem = repos[1];
    expect(gem).toMatchObject({
      name: "orbit-cli",
      language: "Rust",
      changedFiles: 0,
      sessions: [],
      preselect: { kind: "noPicker" },
      remote: { host: "github.com", owner: "demo-user", name: "orbit-cli" },
    });
    expect(remotes.get(gem.path)?.webUrl).toBe("https://github.com/demo-user/orbit-cli");
  });

  it("gives remote-only gems a small log-scaled mass", () => {
    expect(remoteMass(0)).toBe(0);
    expect(remoteMass(10)).toBeLessThan(remoteMass(10_000));
    expect(remoteMass(1e9)).toBe(REMOTE_MAX_MASS);
    expect(remoteMass(Number.NaN)).toBe(0);
    const { repos } = mergeProjects([], list(remote("big", { sizeKb: 5e6 })), SETTINGS);
    expect(repos[0].mass).toBeLessThanOrEqual(REMOTE_MAX_MASS);
  });

  it("leaves out remote-only forks and archived repos unless settings show them", () => {
    const remotes = list(remote("fork", { fork: true }), remote("old", { archived: true }), remote("plain"));
    expect(mergeProjects([], remotes, SETTINGS).repos.map((r) => r.name)).toEqual(["plain"]);
    expect(
      mergeProjects([], remotes, { ...SETTINGS, showForks: true, showArchived: true }).repos.map((r) => r.name),
    ).toEqual(["fork", "old", "plain"]);
  });

  it("keeps a local clone of a fork or archived repo", () => {
    const local = testRepo();
    const { repos, remoteOnly } = mergeProjects([local], list(remote("tidepool", { fork: true, archived: true })), SETTINGS);
    expect(repos).toEqual([local]);
    expect(remoteOnly.size).toBe(0);
  });

  it("hides by path, by remote key (any case) and remote-only keys", () => {
    const a = testRepo({ path: "/p/a", name: "a", remote: null });
    const b = testRepo({ path: "/p/b", name: "b", remote: { host: "github.com", owner: "demo-user", name: "b" } });
    const c = testRepo({ path: "/p/c", name: "c", remote: null });
    const hidden = ["/p/a", "remote:GitHub.com/demo-user/B", "remote:github.com/demo-user/orbit"];
    const { repos } = mergeProjects([a, b, c], list(remote("b"), remote("orbit"), remote("lumen")), {
      ...SETTINGS,
      hidden,
    });
    expect(repos.map((r) => r.name)).toEqual(["c", "lumen"]);
  });

  it("a hidden local clone does not bring its remote back as a remote-only gem", () => {
    const local = testRepo({ path: "/p/tidepool" });
    const { repos } = mergeProjects([local], list(remote("tidepool")), { ...SETTINGS, hidden: ["/p/tidepool"] });
    expect(repos).toEqual([]);
  });

  it("works without a remote listing", () => {
    const local = testRepo();
    expect(mergeProjects([local], null, SETTINGS)).toEqual({
      repos: [local],
      remotes: new Map(),
      remoteOnly: new Set(),
    });
  });
});
