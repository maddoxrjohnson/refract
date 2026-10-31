import { describe, expect, it } from "vitest";
import { QUARTZ, stoneFor } from "./stones";

describe("stoneFor", () => {
  it("names the stone for a known language", () => {
    expect(stoneFor("TypeScript")).toEqual({ name: "sapphire", hex: 0x3d63c9 });
  });

  it("falls back to quartz for unknown, missing or inherited names", () => {
    for (const lang of [null, "", "COBOL", "constructor", "toString"]) expect(stoneFor(lang)).toBe(QUARTZ);
  });
});
