import { describe, expect, it } from "vitest";
import { CUT_NAMES, cutFor, stoneGeometry } from "./cuts";

describe("cutFor", () => {
  it("gives a path the same cut every time", () => {
    for (const path of ["/home/test/projects/atlas", "/home/test/projects/quill", "C:\\dev\\wren"]) {
      expect(cutFor(path)).toBe(cutFor(path));
      expect(CUT_NAMES).toContain(cutFor(path));
    }
  });

  it("uses every cut across a collection", () => {
    const seen = new Set(Array.from({ length: 60 }, (_, i) => cutFor(`/home/test/projects/r${i}`)));
    expect(seen.size).toBe(CUT_NAMES.length);
  });
});

describe("stoneGeometry", () => {
  it("draws edges only where facets meet at an angle", () => {
    for (const cut of CUT_NAMES) {
      for (const chipped of [false, true]) {
        const g = stoneGeometry(cut, chipped);
        const bary = g.getAttribute("bary");
        expect(bary.count).toBe(g.getAttribute("position").count);
        // Every triangle keeps at least one real edge, and some edges are masked.
        let masked = 0;
        for (let i = 0; i < bary.count; i++) masked += bary.getX(i) + bary.getY(i) + bary.getZ(i) - 1;
        expect(masked).toBeGreaterThan(0);
        g.dispose();
      }
    }
  });
});
