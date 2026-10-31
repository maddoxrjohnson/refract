import { BufferAttribute, BufferGeometry, Vector3 } from "three";
import { ConvexGeometry } from "three/examples/jsm/geometries/ConvexGeometry.js";
import { hashString } from "./layout";

// Stone shapes for the Banknote scene (banknote spec §3), ported from
// docs/mockups/gem-studies.html. Built once per stone, never in the frame loop.

interface Ring {
  y: number;
  /** 0 is an apex. */
  r: number;
  /** Turn by half a step, so neighbouring rings meet in kite facets. */
  off?: number;
}

/**
 * Convex faceted solid from horizontal rings, bottom to top, with `seg` points each.
 * Rings offset by half a step are joined with zig-zag triangles (kite facets).
 */
export function cutGeometry(rings: Ring[], seg: number, sx = 1, sz = 1): BufferGeometry {
  const ys = rings.map((r) => r.y);
  const yc = (Math.min(...ys) + Math.max(...ys)) / 2;
  const ring = (R: Ring) =>
    Array.from({ length: seg }, (_, i) => {
      const a = ((i + (R.off ?? 0)) / seg) * Math.PI * 2;
      return new Vector3(Math.cos(a) * R.r * sx, R.y, Math.sin(a) * R.r * sz);
    });
  const out: Vector3[] = [];
  const c = new Vector3(0, yc, 0);
  const ab = new Vector3();
  const ac = new Vector3();
  const n = new Vector3();
  const cen = new Vector3();
  // Winds each triangle to face away from the middle.
  const tri = (a: Vector3, b: Vector3, d: Vector3) => {
    n.crossVectors(ab.subVectors(b, a), ac.subVectors(d, a));
    cen.copy(a).add(b).add(d).multiplyScalar(1 / 3).sub(c);
    if (n.dot(cen) < 0) out.push(a, d, b);
    else out.push(a, b, d);
  };
  for (let k = 0; k < rings.length - 1; k++) {
    const A = rings[k];
    const B = rings[k + 1];
    if (A.r === 0) {
      const ap = new Vector3(0, A.y, 0);
      const P = ring(B);
      for (let i = 0; i < seg; i++) tri(ap, P[i], P[(i + 1) % seg]);
      continue;
    }
    if (B.r === 0) {
      const ap = new Vector3(0, B.y, 0);
      const P = ring(A);
      for (let i = 0; i < seg; i++) tri(P[i], P[(i + 1) % seg], ap);
      continue;
    }
    const P = ring(A);
    const Q = ring(B);
    const j = (i: number) => (i + 1) % seg;
    const ao = A.off ?? 0;
    const bo = B.off ?? 0;
    for (let i = 0; i < seg; i++) {
      if (ao === bo) {
        tri(P[i], P[j(i)], Q[j(i)]);
        tri(P[i], Q[j(i)], Q[i]);
      } else if (bo > ao) {
        tri(P[i], P[j(i)], Q[i]);
        tri(Q[i], P[j(i)], Q[j(i)]);
      } else {
        tri(P[i], P[j(i)], Q[j(i)]);
        tri(Q[i], P[i], Q[j(i)]);
      }
    }
  }
  return new BufferGeometry().setFromPoints(out);
}

/** A stone stopped mid-task loses a corner: points past a plane are pushed back onto it, then re-hulled. */
export function chip(geo: BufferGeometry, dir: Vector3, depth: number): BufferGeometry {
  const pos = geo.attributes.position;
  const n = dir.clone().normalize();
  const v = new Vector3();
  let max = -Infinity;
  for (let i = 0; i < pos.count; i++) max = Math.max(max, v.fromBufferAttribute(pos, i).dot(n));
  const d = max - depth;
  const pts: Vector3[] = [];
  for (let i = 0; i < pos.count; i++) {
    v.fromBufferAttribute(pos, i);
    const s = v.dot(n);
    pts.push(s > d ? v.clone().addScaledVector(n, d - s) : v.clone());
  }
  geo.dispose();
  return new ConvexGeometry(pts);
}

/**
 * Flat normals plus a barycentric attribute for drawing edges. Edges between coplanar
 * triangles are masked, so only real facet edges are drawn.
 */
export function prepare(geo: BufferGeometry): BufferGeometry {
  const g = geo.index ? geo.toNonIndexed() : geo;
  if (g !== geo) geo.dispose();
  g.deleteAttribute("normal");
  g.computeVertexNormals();
  const pos = g.attributes.position;
  const triCount = pos.count / 3;
  const key = (i: number) =>
    `${pos.getX(i).toFixed(3)},${pos.getY(i).toFixed(3)},${pos.getZ(i).toFixed(3)}`;
  const normals: Vector3[] = [];
  const edges = new Map<string, [number, number][]>();
  const a = new Vector3();
  const b = new Vector3();
  const c = new Vector3();
  for (let t = 0; t < triCount; t++) {
    a.fromBufferAttribute(pos, t * 3);
    b.fromBufferAttribute(pos, t * 3 + 1);
    c.fromBufferAttribute(pos, t * 3 + 2);
    normals.push(new Vector3().crossVectors(b.clone().sub(a), c.clone().sub(a)).normalize());
    for (let e = 0; e < 3; e++) {
      const k1 = key(t * 3 + ((e + 1) % 3));
      const k2 = key(t * 3 + ((e + 2) % 3));
      const ek = k1 < k2 ? `${k1}|${k2}` : `${k2}|${k1}`;
      let list = edges.get(ek);
      if (!list) edges.set(ek, (list = []));
      list.push([t, e]);
    }
  }
  const bary = new Float32Array(pos.count * 3);
  for (let t = 0; t < triCount; t++) for (let v = 0; v < 3; v++) bary[(t * 3 + v) * 3 + v] = 1;
  for (const list of edges.values()) {
    if (list.length !== 2) continue;
    const [[t1, e1], [t2, e2]] = list;
    if (normals[t1].dot(normals[t2]) > 0.999) {
      for (const [t, e] of [
        [t1, e1],
        [t2, e2],
      ]) {
        for (let v = 0; v < 3; v++) bary[(t * 3 + v) * 3 + e] = 1;
      }
    }
  }
  g.setAttribute("bary", new BufferAttribute(bary, 3));
  return g;
}

export const CUTS = {
  round: () =>
    cutGeometry(
      [
        { y: -0.86, r: 0 },
        { y: -0.42, r: 0.58, off: 0.5 },
        { y: -0.04, r: 1 },
        { y: 0.04, r: 1 },
        { y: 0.19, r: 0.8, off: 0.5 },
        { y: 0.32, r: 0.55 },
        { y: 0.32, r: 0 },
      ],
      16,
    ),
  emerald: () =>
    cutGeometry(
      [
        { y: -0.6, r: 0, off: 0.5 },
        { y: -0.44, r: 0.36, off: 0.5 },
        { y: -0.2, r: 0.72, off: 0.5 },
        { y: -0.03, r: 1, off: 0.5 },
        { y: 0.03, r: 1, off: 0.5 },
        { y: 0.14, r: 0.86, off: 0.5 },
        { y: 0.24, r: 0.72, off: 0.5 },
        { y: 0.3, r: 0.6, off: 0.5 },
        { y: 0.3, r: 0, off: 0.5 },
      ],
      8,
      1.3,
      0.92,
    ),
  brilliant: () =>
    cutGeometry(
      [
        { y: -0.86, r: 0 },
        { y: -0.4, r: 0.6, off: 0.5 },
        { y: -0.04, r: 1 },
        { y: 0.04, r: 1 },
        { y: 0.2, r: 0.8, off: 0.5 },
        { y: 0.34, r: 0.56 },
        { y: 0.34, r: 0 },
      ],
      8,
    ),
  oval: () =>
    cutGeometry(
      [
        { y: -0.82, r: 0 },
        { y: -0.4, r: 0.6, off: 0.5 },
        { y: -0.04, r: 1 },
        { y: 0.04, r: 1 },
        { y: 0.2, r: 0.8, off: 0.5 },
        { y: 0.32, r: 0.55 },
        { y: 0.32, r: 0 },
      ],
      12,
      1.32,
      0.86,
    ),
  princess: () =>
    cutGeometry(
      [
        { y: -0.78, r: 0, off: 0.5 },
        { y: -0.36, r: 0.72, off: 0.5 },
        { y: -0.03, r: 1.12, off: 0.5 },
        { y: 0.03, r: 1.12, off: 0.5 },
        { y: 0.24, r: 0.82, off: 0.5 },
        { y: 0.24, r: 0, off: 0.5 },
      ],
      4,
    ),
  cushion: () =>
    cutGeometry(
      [
        { y: -0.8, r: 0 },
        { y: -0.38, r: 0.62, off: 0.5 },
        { y: -0.04, r: 1 },
        { y: 0.04, r: 1 },
        { y: 0.22, r: 0.76, off: 0.5 },
        { y: 0.34, r: 0.5 },
        { y: 0.34, r: 0 },
      ],
      12,
      1.08,
      1,
    ),
} satisfies Record<string, () => BufferGeometry>;

export type CutName = keyof typeof CUTS;

export const CUT_NAMES = Object.keys(CUTS) as CutName[];

/** The cut a repo is drawn as: seeded by its path, so it keeps its cut. */
export function cutFor(path: string): CutName {
  // A different mix from the layout's seed, so cut and tilt don't move together.
  return CUT_NAMES[(Math.imul(hashString(path), 0x9e3779b1) >>> 0) % CUT_NAMES.length];
}

/** Where the chip comes off a stone stopped mid-task, and how deep. */
const CHIP_DIR = new Vector3(0.9, 0.5, 0.35);
const CHIP_DEPTH = 0.42;

/** A stone's solid, ready for the stone material: chipped when its agent was stopped mid-task. */
export function stoneGeometry(cut: CutName, chipped: boolean): BufferGeometry {
  const geo = CUTS[cut]();
  return prepare(chipped ? chip(geo, CHIP_DIR, CHIP_DEPTH) : geo);
}

/** A thin flat splinter `L` long and `W` wide, with a seeded five-sided outline. */
export function shardGeometry(r: () => number, L: number, W: number, T: number): BufferGeometry {
  const outline = [
    [L / 2, 0],
    [L * (0.05 + r() * 0.15), (W / 2) * (0.7 + r() * 0.3)],
    [-L / 2, W * (0.15 + r() * 0.2)],
    [(-L / 2) * (0.8 + r() * 0.15), -W * (0.25 + r() * 0.15)],
    [L * (r() * 0.2 - 0.05), (-W / 2) * (0.6 + r() * 0.4)],
  ];
  const pts: Vector3[] = [];
  for (const [x, y] of outline) {
    pts.push(new Vector3(x, 0, y));
    pts.push(new Vector3(x * 0.86, T / 2, y * 0.7));
    pts.push(new Vector3(x * 0.86, -T / 2, y * 0.7));
  }
  return new ConvexGeometry(pts);
}
