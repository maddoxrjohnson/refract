import type { GpuUsage } from "../backend/types";
import { LEVELS, TIERS, type QualityLevel, type QualitySetting } from "./tiers";

// Picks the active graphics level when the setting is "auto" (spec §3.6). Pure: the
// caller passes the time with every sample, in milliseconds (performance.now()).
//
// Frame CPU time says nothing about the GPU in a WebView (GL calls queue to another
// process), so the scene reports the interval between consecutive frames of continuous
// animation instead. Frames that arrive late mean the level cannot keep its frame cap.
// Stepping up also needs GPU samples showing headroom; without them the level only falls.
// Low draws only on change, so after a game forces it there may be no frames to judge;
// once the GPU has been calm for 10 s (others under 45% throughout), GPU readings alone
// lift it back to Medium. A game that then turns busy again within a minute doubles that
// wait, up to 2 minutes, so a game hovering near the threshold does not flip the level
// (and rebuild the effects) every few seconds.
// Samples live in fixed-size rings so a frame allocates nothing.

/** Other programs above this share of the GPU (a game, a render) force Low. */
export const GPU_HEAVY_PERCENT = 60;
/** Late frames for this long step down a level. */
const DOWN_AFTER_MS = 3_000;
/** Frames on time with GPU headroom for this long step up a level. */
const UP_AFTER_MS = 10_000;
/** Late: the median interval is over the level's budget by this factor. */
const LATE = 1.25;
/** On time: the median interval is within the level's budget by this factor. */
const ON_TIME = 1.1;
/** Lifting Low on GPU readings alone needs other programs under this share throughout. */
const GPU_CALM_PERCENT = 45;
/** The calm wait doubles, up to this, each time the GPU turns busy soon after such a lift. */
const MAX_CALM_WAIT_MS = 120_000;
/** Busy again within this long after such a lift counts as soon; calm for this long resets the wait. */
const LIFT_SETTLE_MS = 60_000;
/** Stepping up needs Refract's own GPU use under this share throughout. */
const OWN_HEADROOM_PERCENT = 35;
/** After stepping down for late frames, the level just left stays out of reach this long. */
const RETRY_AFTER_MS = 30_000;
/**
 * A window counts once its intervals add up to this share of it (an idle scene reports
 * none, so sparse bursts of animation do not decide anything) and hold a few frames.
 */
const MIN_COVERED = 0.5;
const MIN_FRAMES = 5;
/** GPU use is read every 2 s, so 10 s gives about five. */
const MIN_UP_GPU_SAMPLES = 3;

/** 10 s at 60 fps with room to spare; older samples are overwritten. */
const FRAME_RING = 1024;
/** Two minutes of readings, the longest calm wait. */
const GPU_RING = 64;

export interface QualityController {
  readonly setting: QualitySetting;
  /** The level the scene should draw at. */
  readonly level: QualityLevel;
  /**
   * Milliseconds since the previous frame of continuous animation (not after an idle
   * gap). Returns the level after it.
   */
  frame(now: number, intervalMs: number): QualityLevel;
  /** A GPU use reading. Returns the level after it. */
  gpu(now: number, usage: GpuUsage): QualityLevel;
  /** A new setting from the user; "auto" starts again from Medium. Returns the level. */
  setSetting(setting: QualitySetting): QualityLevel;
}

/** The share of the GPU that is not Refract's own. */
export function gpuOtherPercent(usage: GpuUsage): number {
  return Math.max(0, usage.totalPercent - usage.ownPercent);
}

/** The element of rank `floor(n / 2)` in `a[0..n)`, reordering `a` in place (quickselect). */
function median(a: Float64Array, n: number): number {
  const k = n >> 1;
  let lo = 0;
  let hi = n - 1;
  while (lo < hi) {
    const pivot = a[(lo + hi) >> 1];
    let i = lo;
    let j = hi;
    while (i <= j) {
      while (a[i] < pivot) i++;
      while (a[j] > pivot) j--;
      if (i <= j) {
        const t = a[i];
        a[i] = a[j];
        a[j] = t;
        i++;
        j--;
      }
    }
    if (k <= j) hi = j;
    else if (k >= i) lo = i;
    else break;
  }
  return a[k];
}

export function createQualityController(initial: QualitySetting): QualityController {
  let setting = initial;
  let level: QualityLevel = initial === "auto" ? "medium" : initial;
  /** When the current level began; `null` until the first sample after a change. */
  let levelSince: number | null = null;
  let gpuHeavy = false;
  /** The level is Low because other programs were busy on the GPU, not for late frames. */
  let lowForGpu = false;
  /** First calm GPU sample after a busy one; stepping up waits `calmWaitMs` from here. */
  let gpuCalmSince = -Infinity;
  /** How long the GPU must stay calm before GPU readings alone lift Low; backs off. */
  let calmWaitMs = UP_AFTER_MS;
  /** When GPU readings last lifted Low, until a busy reading or a long calm settles it. */
  let liftedAt: number | null = null;
  /** The level that late frames pushed us off, and until when it stays out of reach. */
  let blockedLevel: QualityLevel | null = null;
  let blockedUntil = 0;

  // Frame intervals at the current level, newest at `frameHead - 1`.
  const frameT = new Float64Array(FRAME_RING);
  const frameMs = new Float64Array(FRAME_RING);
  let frameHead = 0;
  let frameCount = 0;
  const scratch = new Float64Array(FRAME_RING);

  // GPU readings, newest at `gpuHead - 1`.
  const gpuT = new Float64Array(GPU_RING);
  const gpuOwn = new Float64Array(GPU_RING);
  const gpuOther = new Float64Array(GPU_RING);
  let gpuHead = 0;
  let gpuCount = 0;

  const setLevel = (next: QualityLevel, now: number | null) => {
    level = next;
    lowForGpu = false;
    levelSince = now;
    frameCount = 0;
  };

  /**
   * Copies the intervals in the last `windowMs` into `scratch`; returns how many, or 0
   * when they cover too little of the window to count.
   */
  const collect = (now: number, windowMs: number) => {
    let n = 0;
    let covered = 0;
    for (let k = 0; k < frameCount; k++) {
      const i = (frameHead - 1 - k + FRAME_RING) % FRAME_RING;
      if (frameT[i] < now - windowMs) break;
      scratch[n++] = frameMs[i];
      covered += frameMs[i];
    }
    return n >= MIN_FRAMES && covered >= MIN_COVERED * windowMs ? n : 0;
  };

  /**
   * Enough GPU readings from `since` on, all with room for Refract to draw more and other
   * programs under `maxOther`.
   */
  const gpuHeadroom = (since: number, maxOther = GPU_HEAVY_PERCENT) => {
    let n = 0;
    for (let k = 0; k < gpuCount; k++) {
      const i = (gpuHead - 1 - k + GPU_RING) % GPU_RING;
      if (gpuT[i] < since) break;
      if (gpuOwn[i] >= OWN_HEADROOM_PERCENT || gpuOther[i] >= maxOther) return false;
      n++;
    }
    return n >= MIN_UP_GPU_SAMPLES;
  };

  return {
    get setting() {
      return setting;
    },
    get level() {
      return level;
    },

    frame(now, intervalMs) {
      if (setting !== "auto" || !(intervalMs > 0)) return level;
      levelSince ??= now;
      frameT[frameHead] = now;
      frameMs[frameHead] = intervalMs;
      frameHead = (frameHead + 1) % FRAME_RING;
      frameCount = Math.min(frameCount + 1, FRAME_RING);

      const i = LEVELS.indexOf(level);
      const budget = TIERS[level].budgetMs;
      if (i > 0 && now - levelSince >= DOWN_AFTER_MS) {
        const n = collect(now, DOWN_AFTER_MS);
        if (n > 0 && median(scratch, n) > budget * LATE) {
          blockedLevel = level;
          blockedUntil = now + RETRY_AFTER_MS;
          setLevel(LEVELS[i - 1], now);
          return level;
        }
      }

      const next = LEVELS[i + 1];
      if (
        next &&
        !gpuHeavy &&
        now - gpuCalmSince >= UP_AFTER_MS &&
        !(blockedLevel === next && now < blockedUntil) &&
        now - levelSince >= UP_AFTER_MS &&
        gpuHeadroom(Math.max(now - UP_AFTER_MS, levelSince))
      ) {
        const n = collect(now, UP_AFTER_MS);
        if (n > 0 && median(scratch, n) <= budget * ON_TIME) setLevel(next, now);
      }
      return level;
    },

    gpu(now, usage) {
      const other = gpuOtherPercent(usage);
      gpuT[gpuHead] = now;
      gpuOwn[gpuHead] = usage.ownPercent;
      gpuOther[gpuHead] = other;
      gpuHead = (gpuHead + 1) % GPU_RING;
      gpuCount = Math.min(gpuCount + 1, GPU_RING);

      const heavy = other > GPU_HEAVY_PERCENT;
      if (gpuHeavy && !heavy) gpuCalmSince = now;
      gpuHeavy = heavy;
      if (liftedAt !== null) {
        if (now - liftedAt >= LIFT_SETTLE_MS) calmWaitMs = UP_AFTER_MS;
        else if (heavy) calmWaitMs = Math.min(calmWaitMs * 2, MAX_CALM_WAIT_MS);
        if (heavy || now - liftedAt >= LIFT_SETTLE_MS) liftedAt = null;
      }
      if (heavy && setting === "auto" && level !== "low") {
        setLevel("low", now);
        lowForGpu = true;
      } else if (
        lowForGpu &&
        setting === "auto" &&
        !heavy &&
        now - gpuCalmSince >= calmWaitMs &&
        gpuHeadroom(now - calmWaitMs, GPU_CALM_PERCENT)
      ) {
        setLevel("medium", now);
        liftedAt = now;
      }
      return level;
    },

    setSetting(next) {
      setting = next;
      blockedLevel = null;
      calmWaitMs = UP_AFTER_MS;
      liftedAt = null;
      setLevel(next === "auto" ? (gpuHeavy ? "low" : "medium") : next, null);
      lowForGpu = next === "auto" && gpuHeavy;
      return level;
    },
  };
}
