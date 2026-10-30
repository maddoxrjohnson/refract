import { describe, expect, it } from "vitest";
import { PICKER_CLEAR_PX, ZOOM_REACH, ZOOM_TOP_MARGIN_PX, zoomLift } from "./Scene";

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
