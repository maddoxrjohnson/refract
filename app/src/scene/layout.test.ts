import { describe, expect, it } from "vitest";
import type { RepoSnapshot } from "../backend/types";
import { hashString, layoutGems, rng } from "./layout";

function repo(name: string, mass: number): RepoSnapshot {
  return {
    path: `/home/test/projects/${name}`,
    name,
    originUrl: null,
    remote: null,
    branch: "main",
    ahead: null,
    behind: null,
    changedFiles: 0,
    dirtyPaths: [],
    lastCommit: null,
    language: null,
    trackedBytes: 0,
    commits30d: 0,
    mass,
    sessions: [],
    preselect: { kind: "noPicker" },
  };
}

const REPOS = [
  repo("alpha", 0.3),
  repo("bravo", 0.9),
  repo("charlie", 0.55),
  repo("delta", 0.1),
  repo("echo", 0.7),
  repo("foxtrot", 0.3),
];

const centreDistance = (p: { x: number; y: number }) => Math.hypot(p.x, p.y);

describe("layoutGems", () => {
  it("returns an empty map for no repos", () => {
    expect(layoutGems([]).size).toBe(0);
  });

  it("places every repo, keyed by path", () => {
    const layout = layoutGems(REPOS);
    expect([...layout.keys()].sort()).toEqual(REPOS.map((r) => r.path).sort());
  });

  it("puts the heaviest repo nearest the centre", () => {
    const layout = layoutGems(REPOS);
    const heaviest = layout.get("/home/test/projects/bravo")!;
    for (const [path, p] of layout) {
      if (path !== "/home/test/projects/bravo") {
        expect(centreDistance(p)).toBeGreaterThan(centreDistance(heaviest));
      }
    }
  });

  it("does not depend on the order repos arrive in", () => {
    const a = layoutGems(REPOS);
    const b = layoutGems([...REPOS].reverse());
    const c = layoutGems([REPOS[3], REPOS[0], REPOS[5], REPOS[1], REPOS[4], REPOS[2]]);
    expect(b).toEqual(a);
    expect(c).toEqual(a);
  });

  it("is deterministic across calls", () => {
    expect(layoutGems(REPOS)).toEqual(layoutGems(REPOS));
  });

  it("makes heavier gems bigger", () => {
    const layout = layoutGems(REPOS);
    const r = (name: string) => layout.get(`/home/test/projects/${name}`)!.radius;
    expect(r("bravo")).toBeGreaterThan(r("echo"));
    expect(r("echo")).toBeGreaterThan(r("delta"));
    expect(r("alpha")).toBe(r("foxtrot"));
  });

  it("shrinks gems when there are many", () => {
    const few = layoutGems([repo("solo", 0.5)]).get("/home/test/projects/solo")!.radius;
    const many = Array.from({ length: 40 }, (_, i) => repo(`r${i}`, 0.2));
    many.push(repo("solo", 0.5));
    const crowded = layoutGems(many).get("/home/test/projects/solo")!.radius;
    expect(crowded).toBeLessThan(few);
  });

  it("treats out-of-range masses as the nearest valid one", () => {
    const layout = layoutGems([repo("big", 4), repo("nan", Number.NaN), repo("neg", -1)]);
    const r = (name: string) => layout.get(`/home/test/projects/${name}`)!.radius;
    expect(r("nan")).toBe(r("neg"));
    expect(r("big")).toBeGreaterThan(r("nan"));
    for (const p of layout.values()) {
      expect(Number.isFinite(p.x + p.y + p.z + p.radius)).toBe(true);
    }
  });
});

describe("rng", () => {
  it("repeats for a seed and stays in [0, 1)", () => {
    const a = rng(hashString("x"));
    const b = rng(hashString("x"));
    for (let i = 0; i < 100; i++) {
      const v = a();
      expect(v).toBe(b());
      expect(v).toBeGreaterThanOrEqual(0);
      expect(v).toBeLessThan(1);
    }
  });
});
