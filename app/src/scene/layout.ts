import type { RepoSnapshot } from "../backend/types";

// Where each stone sits on the sheet (banknote spec §4), as in docs/mockups/gem-studies.html:
// a golden-angle spiral from the centre outwards, heaviest first, then rounds of pull to
// the centre and push apart until no two stones' discs overlap. Sorted by mass then path
// first, so a stone keeps its place whatever order the repos arrive in.

export interface GemPosition {
  /** On the sheet as seen from the camera: x to the right, y away from the viewer (the scene's -z). */
  x: number;
  y: number;
  /** The stone's scale: its girdle radius in scene units. */
  size: number;
}

const GOLDEN_ANGLE = 2.39996;
/** A stone's disc on the sheet, as a share of its size: room for its rosette to breathe. */
export const DISC = 1.22;
const ROUNDS = 260;
/** Extra push-only rounds at most, should the pull leave a sliver of overlap. */
const SETTLE_ROUNDS = 400;

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

/** Pushes every overlapping pair apart once; returns whether any overlapped. */
function pushApart(xs: Float64Array, zs: Float64Array, rs: Float64Array): boolean {
  let any = false;
  for (let i = 0; i < xs.length; i++) {
    for (let j = i + 1; j < xs.length; j++) {
      let dx = xs[j] - xs[i];
      let dz = zs[j] - zs[i];
      const d = Math.hypot(dx, dz) || 1e-3;
      const min = rs[i] + rs[j];
      if (d >= min) continue;
      any = true;
      // A little past touching, so float error never leaves the pair overlapping.
      const p = (min - d) / 2 + 1e-6;
      dx /= d;
      dz /= d;
      xs[i] -= dx * p;
      zs[i] -= dz * p;
      xs[j] += dx * p;
      zs[j] += dz * p;
    }
  }
  return any;
}

export function layoutGems(repos: RepoSnapshot[]): Map<string, GemPosition> {
  // Ties break on path so equal masses don't depend on arrival order.
  const sorted = [...repos].sort(
    (a, b) =>
      clampMass(b.mass) - clampMass(a.mass) || (a.path < b.path ? -1 : a.path > b.path ? 1 : 0),
  );
  const n = sorted.length;
  // Stones shrink once there are more than about 14, so a big collection still fits.
  const shrink = Math.min(1, Math.sqrt(14 / Math.max(n, 1)));
  const xs = new Float64Array(n);
  const zs = new Float64Array(n);
  const rs = new Float64Array(n);
  const ws = new Float64Array(n);
  const sizes = new Float64Array(n);
  sorted.forEach((repo, i) => {
    const m = clampMass(repo.mass);
    const a = i * GOLDEN_ANGLE;
    const rr = 1.4 * Math.sqrt(i) * shrink;
    xs[i] = Math.cos(a) * rr;
    zs[i] = Math.sin(a) * rr * 0.8;
    sizes[i] = (0.42 + 0.52 * m) * shrink;
    rs[i] = sizes[i] * DISC;
    // Heavier stones pull harder, so they end up nearer the middle.
    ws[i] = 0.4 + m;
  });
  for (let k = 0; k < ROUNDS; k++) {
    for (let i = 0; i < n; i++) {
      xs[i] -= xs[i] * 0.02 * ws[i];
      zs[i] -= zs[i] * 0.02 * ws[i];
    }
    pushApart(xs, zs, rs);
  }
  for (let k = 0; k < SETTLE_ROUNDS && pushApart(xs, zs, rs); k++);
  const out = new Map<string, GemPosition>();
  sorted.forEach((repo, i) => out.set(repo.path, { x: xs[i], y: -zs[i], size: sizes[i] }));
  return out;
}

/** How far the layout reaches from the centre, discs included. */
export function layoutRadius(layout: ReadonlyMap<string, GemPosition>): number {
  let r = 0;
  for (const p of layout.values()) r = Math.max(r, Math.hypot(p.x, p.y) + p.size * DISC);
  return r;
}
