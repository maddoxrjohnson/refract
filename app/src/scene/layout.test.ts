import { describe, expect, it } from "vitest";
import type { RepoSnapshot } from "../backend/types";
import { DISC, hashString, layoutGems, layoutRadius, rng } from "./layout";

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

/** Invented repos with a spread of masses. */
const many = (count: number) =>
  Array.from({ length: count }, (_, i) => repo(`r${i}`, rng(hashString(`m${i}`))()));

function expectNoOverlaps(layout: ReturnType<typeof layoutGems>) {
  const ps = [...layout.values()];
  for (let i = 0; i < ps.length; i++) {
    for (let j = i + 1; j < ps.length; j++) {
      const d = Math.hypot(ps[i].x - ps[j].x, ps[i].y - ps[j].y);
      expect(d).toBeGreaterThanOrEqual((ps[i].size + ps[j].size) * DISC - 1e-9);
    }
  }
}

describe("layoutGems", () => {
  it("returns an empty map for no repos", () => {
    expect(layoutGems([]).size).toBe(0);
    expect(layoutRadius(layoutGems([]))).toBe(0);
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

  it("never lets two stones' discs overlap", () => {
    for (const count of [2, 6, 14, 40, 120]) expectNoOverlaps(layoutGems(many(count)));
    expectNoOverlaps(layoutGems(REPOS));
  });

  it("stays within a bounded radius however many repos there are", () => {
    for (const count of [1, 14, 40, 120]) {
      const layout = layoutGems(many(count));
      expect(layoutRadius(layout)).toBeLessThan(6);
      for (const p of layout.values()) expect(Number.isFinite(p.x + p.y + p.size)).toBe(true);
    }
  });

  it("does not depend on the order repos arrive in", () => {
    const a = layoutGems(REPOS);
    const b = layoutGems([...REPOS].reverse());
    const c = layoutGems([REPOS[3], REPOS[0], REPOS[5], REPOS[1], REPOS[4], REPOS[2]]);
    expect(b).toEqual(a);
    expect(c).toEqual(a);
    const big = many(40);
    expect(layoutGems([...big].reverse())).toEqual(layoutGems(big));
  });

  it("lays out 120 repos quickly", () => {
    // Around 10 ms on a desktop; the bound only catches a return to quadratic-per-round work.
    // New paths, so the cache from the other tests cannot answer.
    const crowd = many(120).map((r) => ({ ...r, path: `${r.path}-timed` }));
    const t = performance.now();
    layoutGems(crowd);
    expect(performance.now() - t).toBeLessThan(250);
  });

  it("reuses the last layout for the same paths and masses, as a copy", () => {
    const a = layoutGems(REPOS);
    a.delete("/home/test/projects/alpha");
    const b = layoutGems([...REPOS].reverse());
    expect(b.size).toBe(REPOS.length);
    expect(layoutGems([...REPOS.slice(1), repo("alpha", 0.31)]).get("/home/test/projects/alpha")).not.toEqual(
      b.get("/home/test/projects/alpha"),
    );
  });

  it("is deterministic across calls", () => {
    expect(layoutGems(REPOS)).toEqual(layoutGems(REPOS));
  });

  it("makes heavier stones bigger", () => {
    const layout = layoutGems(REPOS);
    const s = (name: string) => layout.get(`/home/test/projects/${name}`)!.size;
    expect(s("bravo")).toBeGreaterThan(s("echo"));
    expect(s("echo")).toBeGreaterThan(s("delta"));
    expect(s("alpha")).toBe(s("foxtrot"));
  });

  it("shrinks stones when there are many", () => {
    const few = layoutGems([repo("solo", 0.5)]).get("/home/test/projects/solo")!.size;
    const crowd = Array.from({ length: 40 }, (_, i) => repo(`r${i}`, 0.2));
    crowd.push(repo("solo", 0.5));
    const crowded = layoutGems(crowd).get("/home/test/projects/solo")!.size;
    expect(crowded).toBeLessThan(few);
  });

  it("treats out-of-range masses as the nearest valid one", () => {
    const layout = layoutGems([repo("big", 4), repo("nan", Number.NaN), repo("neg", -1)]);
    const s = (name: string) => layout.get(`/home/test/projects/${name}`)!.size;
    expect(s("nan")).toBe(s("neg"));
    expect(s("big")).toBeGreaterThan(s("nan"));
    for (const p of layout.values()) {
      expect(Number.isFinite(p.x + p.y + p.size)).toBe(true);
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
