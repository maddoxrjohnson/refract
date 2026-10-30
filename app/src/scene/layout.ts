import type { RepoSnapshot } from "../backend/types";

// Where each gem sits (spec §3.2): a golden-angle spiral from the centre outwards, heaviest
// first, as in docs/mockups/frozen-shatter.html. Depth and every other random choice are
// seeded by the repo's path, so a gem keeps its place whatever order the repos arrive in.

export interface GemPosition {
  x: number;
  y: number;
  z: number;
  radius: number;
}

const GOLDEN_ANGLE = 2.39996;

/** FNV-1a over the string's UTF-16 code units: a stable 32-bit seed for a path. */
export function hashString(s: string): number {
  let h = 0x811c9dc5;
  for (let i = 0; i < s.length; i++) {
    h ^= s.charCodeAt(i);
    h = Math.imul(h, 0x01000193);
  }
  return h >>> 0;
}

/** Mulberry32: a small seeded generator returning numbers in [0, 1). */
export function rng(seed: number): () => number {
  return () => {
    seed = (seed + 0x6d2b79f5) | 0;
    let t = Math.imul(seed ^ (seed >>> 15), 1 | seed);
    t = (t + Math.imul(t ^ (t >>> 7), 61 | t)) ^ t;
    return ((t ^ (t >>> 14)) >>> 0) / 4294967296;
  };
}

const clampMass = (m: number) => (Number.isFinite(m) ? Math.min(1, Math.max(0, m)) : 0);

export function layoutGems(repos: RepoSnapshot[]): Map<string, GemPosition> {
  // Ties break on path so equal masses don't depend on arrival order.
  const sorted = [...repos].sort(
    (a, b) =>
      clampMass(b.mass) - clampMass(a.mass) || (a.path < b.path ? -1 : a.path > b.path ? 1 : 0),
  );
  // Gems shrink once there are more than about 14, so a big collection still fits.
  const shrink = Math.min(1, Math.sqrt(14 / Math.max(sorted.length, 1)));
  const out = new Map<string, GemPosition>();
  sorted.forEach((repo, i) => {
    const rr = 0.72 * Math.sqrt(i + 0.35);
    const a = i * GOLDEN_ANGLE;
    const rand = rng(hashString(repo.path));
    out.set(repo.path, {
      x: Math.cos(a) * rr * 1.6,
      y: Math.sin(a) * rr * 0.95,
      z: (rand() - 0.5) * 1.6 - rr * 0.2,
      radius: (0.32 + 0.45 * Math.sqrt(clampMass(repo.mass))) * shrink,
    });
  });
  return out;
}
