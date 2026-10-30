import { describe, expect, it } from "vitest";
import type { GpuUsage } from "../backend/types";
import { createQualityController, gpuOtherPercent, type QualityController } from "./controller";
import { TIERS } from "./tiers";

/**
 * Plays continuous animation from `from` to `to` (exclusive): a frame every `intervalMs`,
 * and, when `usage` is given, a GPU reading every 2 s as the app takes them.
 */
function play(
  ctrl: QualityController,
  from: number,
  to: number,
  intervalMs: number,
  usage?: GpuUsage,
): number {
  let t = from;
  let nextGpu = from;
  for (; t < to; t += intervalMs) {
    if (usage && t >= nextGpu) {
      ctrl.gpu(t, usage);
      nextGpu += 2_000;
    }
    ctrl.frame(t, intervalMs);
  }
  return t;
}

const gpu = (totalPercent: number, ownPercent = 0): GpuUsage => ({ totalPercent, ownPercent });
/** Frames well behind Medium's 30 fps cap. */
const late = TIERS.medium.budgetMs * 1.5;
/** Medium's cap, met. */
const onTimeMedium = TIERS.medium.budgetMs;
/** Plenty of room: Refract light, nothing else busy. */
const idleGpu = gpu(20, 15);

describe("createQualityController", () => {
  it("starts auto at Medium", () => {
    expect(createQualityController("auto").level).toBe("medium");
  });

  it("keeps a fixed level whatever the samples say", () => {
    const ctrl = createQualityController("high");
    ctrl.gpu(0, gpu(95));
    play(ctrl, 0, 20_000, 200);
    expect(ctrl.level).toBe("high");
  });

  it("drops to Low as soon as other programs use more than 60% of the GPU", () => {
    const ctrl = createQualityController("auto");
    play(ctrl, 0, 1_000, onTimeMedium);
    expect(ctrl.gpu(1_000, gpu(70, 9))).toBe("low");
  });

  it("does not drop at exactly 60%, and counts only other programs' share", () => {
    const ctrl = createQualityController("auto");
    expect(ctrl.gpu(0, gpu(60))).toBe("medium");
    expect(ctrl.gpu(2_000, gpu(90, 40))).toBe("medium");
  });

  it("steps down after 3 s of late frames, not before", () => {
    const ctrl = createQualityController("auto");
    play(ctrl, 0, 2_900, late);
    expect(ctrl.level).toBe("medium");
    play(ctrl, 2_900, 3_200, late);
    expect(ctrl.level).toBe("low");
  });

  it("keeps the level when frames are only a little late", () => {
    const ctrl = createQualityController("auto");
    play(ctrl, 0, 20_000, TIERS.medium.budgetMs * 1.2);
    expect(ctrl.level).toBe("medium");
  });

  it("ignores a short burst of late frames", () => {
    const ctrl = createQualityController("auto");
    let t = play(ctrl, 0, 2_000, onTimeMedium);
    t = play(ctrl, t, t + 500, late);
    play(ctrl, t, t + 6_000, onTimeMedium);
    expect(ctrl.level).toBe("medium");
  });

  it("needs enough animation: short late bursts between idle gaps do not count", () => {
    const ctrl = createQualityController("auto");
    // Five late frames every 2 s: a third of each window is animation.
    for (let t = 0; t < 20_000; t += 2_000) play(ctrl, t, t + 250, late);
    expect(ctrl.level).toBe("medium");
  });

  it("never steps below Low", () => {
    const ctrl = createQualityController("auto");
    play(ctrl, 0, 30_000, 500);
    expect(ctrl.level).toBe("low");
  });

  it("never steps up without GPU readings, however fast the frames", () => {
    const ctrl = createQualityController("auto");
    play(ctrl, 0, 60_000, onTimeMedium);
    expect(ctrl.level).toBe("medium");
  });

  it("steps up after 10 s on time with low GPU use, one level at a time", () => {
    const ctrl = createQualityController("auto");
    play(ctrl, 0, 9_900, onTimeMedium, idleGpu);
    expect(ctrl.level).toBe("medium");
    const t = play(ctrl, 9_900, 10_100, onTimeMedium, idleGpu);
    expect(ctrl.level).toBe("high");
    play(ctrl, t, t + 30_000, TIERS.high.budgetMs, idleGpu);
    expect(ctrl.level).toBe("high");
  });

  it("does not step up while Refract already uses a third of the GPU", () => {
    const ctrl = createQualityController("auto");
    play(ctrl, 0, 30_000, onTimeMedium, gpu(40, 36));
    expect(ctrl.level).toBe("medium");
  });

  it("does not step up when frames miss the cap, even with GPU room", () => {
    const ctrl = createQualityController("auto");
    play(ctrl, 0, 30_000, TIERS.medium.budgetMs * 1.2, idleGpu);
    expect(ctrl.level).toBe("medium");
  });

  it("does not step up while the GPU is busy, and waits 10 s after it calms", () => {
    const ctrl = createQualityController("auto");
    play(ctrl, 0, 15_000, TIERS.low.budgetMs, gpu(80, 5));
    expect(ctrl.level).toBe("low");
    play(ctrl, 15_000, 24_000, TIERS.low.budgetMs, idleGpu);
    expect(ctrl.level).toBe("low");
    play(ctrl, 24_000, 26_000, TIERS.low.budgetMs, idleGpu);
    expect(ctrl.level).toBe("medium");
  });

  /** GPU readings every 2 s from `from` to `to` (exclusive), with no frames at all. */
  const readings = (ctrl: QualityController, from: number, to: number, usage: GpuUsage) => {
    for (let t = from; t < to; t += 2_000) ctrl.gpu(t, usage);
  };

  it("returns to Medium on GPU readings alone 10 s after a game closes, with no frames", () => {
    const ctrl = createQualityController("auto");
    readings(ctrl, 0, 20_000, gpu(90, 5));
    expect(ctrl.level).toBe("low");
    readings(ctrl, 20_000, 30_000, idleGpu);
    expect(ctrl.level).toBe("low");
    readings(ctrl, 30_000, 32_000, idleGpu);
    expect(ctrl.level).toBe("medium");
    // Only to Medium: High still needs frames on time.
    readings(ctrl, 32_000, 90_000, idleGpu);
    expect(ctrl.level).toBe("medium");
  });

  it("does the same after switching to auto while a game runs", () => {
    const ctrl = createQualityController("high");
    ctrl.gpu(0, gpu(95, 5));
    expect(ctrl.setSetting("auto")).toBe("low");
    readings(ctrl, 2_000, 14_000, idleGpu);
    expect(ctrl.level).toBe("medium");
  });

  it("stays Low on GPU readings while Refract's own share is high or the GPU flickers busy", () => {
    const own = createQualityController("auto");
    readings(own, 0, 4_000, gpu(90, 5));
    readings(own, 4_000, 40_000, gpu(45, 36));
    expect(own.level).toBe("low");

    const flicker = createQualityController("auto");
    readings(flicker, 0, 4_000, gpu(90, 5));
    readings(flicker, 4_000, 12_000, idleGpu);
    flicker.gpu(12_000, gpu(70, 5));
    readings(flicker, 14_000, 22_000, idleGpu);
    expect(flicker.level).toBe("low");
    readings(flicker, 22_000, 26_000, idleGpu);
    expect(flicker.level).toBe("medium");
  });

  it("needs other programs under 45% throughout to lift Low on GPU readings alone", () => {
    const ctrl = createQualityController("auto");
    readings(ctrl, 0, 4_000, gpu(90, 5));
    // A game hovering near the threshold keeps it Low.
    readings(ctrl, 4_000, 60_000, gpu(55, 5));
    expect(ctrl.level).toBe("low");
    readings(ctrl, 60_000, 66_000, idleGpu);
    ctrl.gpu(66_000, gpu(50, 5));
    readings(ctrl, 68_000, 76_000, idleGpu);
    expect(ctrl.level).toBe("low");
    readings(ctrl, 76_000, 80_000, idleGpu);
    expect(ctrl.level).toBe("medium");
  });

  it("waits twice as long each time the GPU turns busy soon after a lift, then resets", () => {
    const ctrl = createQualityController("auto");
    const spike = gpu(75, 5);
    /** Idle readings until Medium again; returns the seconds of calm it took. */
    let t = 0;
    const calmUntilLifted = () => {
      const from = t;
      while (ctrl.level === "low" && t - from < 300_000) {
        t += 2_000;
        ctrl.gpu(t, idleGpu);
      }
      return (t - from) / 1_000;
    };
    ctrl.gpu(t, spike);
    expect(ctrl.level).toBe("low");
    expect(calmUntilLifted()).toBe(12);

    const waits: number[] = [];
    for (let i = 0; i < 6; i++) {
      t += 10_000;
      ctrl.gpu(t, spike);
      expect(ctrl.level).toBe("low");
      ctrl.gpu((t += 2_000), idleGpu);
      waits.push(calmUntilLifted());
    }
    // 10 s of calm, doubled each time, never beyond 2 minutes.
    expect(waits).toEqual([22, 42, 82, 122, 122, 122].map((w) => w - 2));

    // Calm for a minute after a lift forgets the back-off.
    readings(ctrl, t + 2_000, t + 62_000, idleGpu);
    t += 62_000;
    ctrl.gpu(t, spike);
    ctrl.gpu((t += 2_000), idleGpu);
    expect(calmUntilLifted()).toBe(10);
  });

  it("does not lift a level left for late frames on GPU readings, even after a game", () => {
    const ctrl = createQualityController("auto");
    const t = play(ctrl, 0, 3_200, late, idleGpu);
    expect(ctrl.level).toBe("low");
    readings(ctrl, t, t + 6_000, gpu(90, 5));
    readings(ctrl, t + 6_000, t + 60_000, idleGpu);
    expect(ctrl.level).toBe("low");
  });

  it("does not climb straight back to a level it just left for late frames", () => {
    const ctrl = createQualityController("auto");
    let t = play(ctrl, 0, 3_200, late, idleGpu);
    expect(ctrl.level).toBe("low");
    t = play(ctrl, t, t + 15_000, TIERS.low.budgetMs, idleGpu);
    expect(ctrl.level).toBe("low");
    play(ctrl, t, t + 16_000, TIERS.low.budgetMs, idleGpu);
    expect(ctrl.level).toBe("medium");
  });

  it("drops from High when it cannot hold 60 fps, and stays at Medium for 30 s", () => {
    const ctrl = createQualityController("auto");
    let t = play(ctrl, 0, 10_100, onTimeMedium, idleGpu);
    expect(ctrl.level).toBe("high");
    // 30 fps at High: on time for Medium, late for High.
    t = play(ctrl, t, t + 3_200, onTimeMedium, idleGpu);
    expect(ctrl.level).toBe("medium");
    t = play(ctrl, t, t + 20_000, onTimeMedium, idleGpu);
    expect(ctrl.level).toBe("medium");
  });

  it("finds the median however the intervals arrive", () => {
    // Mostly late frames with on-time ones mixed in: the median is late.
    const ctrl = createQualityController("auto");
    for (let k = 0, t = 0; t < 4_000; k++, t += 40) ctrl.frame(t, k % 3 === 0 ? 30 : 60 + (k % 7));
    expect(ctrl.level).toBe("low");
    // Mostly on time with late ones mixed in: no step.
    const steady = createQualityController("auto");
    for (let k = 0, t = 0; t < 4_000; k++, t += 40) steady.frame(t, k % 3 === 0 ? 90 : 30 + (k % 5));
    expect(steady.level).toBe("medium");
  });

  it("switching to auto starts again from Medium, or Low while the GPU is busy", () => {
    const ctrl = createQualityController("high");
    expect(ctrl.setSetting("auto")).toBe("medium");
    expect(ctrl.setSetting("low")).toBe("low");
    ctrl.gpu(0, gpu(95, 5));
    expect(ctrl.setSetting("auto")).toBe("low");
    expect(ctrl.setting).toBe("auto");
  });
});

describe("gpuOtherPercent", () => {
  it("is the total minus Refract's own share, never negative", () => {
    expect(gpuOtherPercent({ totalPercent: 70, ownPercent: 5 })).toBe(65);
    expect(gpuOtherPercent({ totalPercent: 3, ownPercent: 4 })).toBe(0);
  });
});
