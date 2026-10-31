import { describe, expect, it } from "vitest";
import { PICKER_CLEAR_PX, ZOOM_REACH, ZOOM_TOP_MARGIN_PX, rosetteSpin, zoomLift } from "./Scene";

describe("zoomLift", () => {
  // The gem's lowest and highest points in pixels from the top, for a window `height` tall.
  const gemBottom = (height: number) => (height / 2) * (1 + ZOOM_REACH - zoomLift(height));
  const gemTop = (height: number) => (height / 2) * (1 - ZOOM_REACH - zoomLift(height));

  it("lifts a zoomed gem clear of the picker in common windows", () => {
    for (const height of [800, 1080]) {
      expect(gemBottom(height)).toBeLessThanOrEqual(height - PICKER_CLEAR_PX + 1e-9);
    }
  });

  it("leaves tall windows alone and keeps the gem on screen in short ones", () => {
    expect(zoomLift(1440)).toBe(0);
    for (const height of [400, 560, 640]) {
      expect(gemTop(height)).toBeGreaterThanOrEqual(ZOOM_TOP_MARGIN_PX - 1e-9);
    }
    expect(zoomLift(20)).toBe(0);
  });

  it("is zero for a collapsed or unmeasured canvas", () => {
    for (const height of [0, -5, Number.NaN]) expect(zoomLift(height)).toBe(0);
  });
});

describe("rosetteSpin", () => {
  it("stays within one turn after weeks of uptime, so float32 keeps its precision", () => {
    for (const t of [0, 1, 12.5, 86_400 * 3, 86_400 * 30 + 0.37]) {
      const spin = rosetteSpin(t);
      expect(spin).toBeGreaterThanOrEqual(0);
      expect(spin).toBeLessThan(2 * Math.PI);
    }
  });

  it("matches the unwrapped angle modulo a full turn, so the wrap is invisible", () => {
    const t = 86_400 * 9 + 4.2;
    const raw = t * 0.5;
    const diff = raw - rosetteSpin(t);
    expect(Math.abs(diff / (2 * Math.PI) - Math.round(diff / (2 * Math.PI)))).toBeLessThan(1e-9);
  });

  it("turns smoothly between frames well into a long session", () => {
    const t = 86_400 * 5;
    const step = rosetteSpin(t + 1 / 60) - rosetteSpin(t);
    expect(Math.fround(step)).toBeCloseTo(0.5 / 60, 6);
  });
});
