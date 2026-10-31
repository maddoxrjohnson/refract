// The three graphics levels from the spec (§3.6, banknote spec §7). The look is the same
// at every level; levels change only resolution, smoothing, splinters and frame rate. The
// scene reads these settings; the controller in ./controller.ts picks which level is active.

export type QualityLevel = "low" | "medium" | "high";

/** What the user chose in settings: a fixed level, or let Refract pick. */
export type QualitySetting = QualityLevel | "auto";

export const LEVELS: readonly QualityLevel[] = ["low", "medium", "high"];

export interface Tier {
  /** Upper bound on the device pixel ratio. */
  maxPixelRatio: number;
  fpsCap: number;
  /** Frame budget in milliseconds: 1000 / fpsCap. */
  budgetMs: number;
  /** Multisampling of the scene's render target; 0 is none. */
  msaa: number;
  /** Paper splinters thrown out by the opening that stay on the sheet. */
  splinters: number;
  /** Stones turn, rosettes spin and the waiting wash pulses. Off on Low, which only draws on change. */
  ambientMotion: boolean;
}

export const TIERS: Record<QualityLevel, Tier> = {
  low: {
    maxPixelRatio: 1,
    fpsCap: 30,
    budgetMs: 1000 / 30,
    msaa: 0,
    splinters: 40,
    ambientMotion: false,
  },
  medium: {
    maxPixelRatio: 1.5,
    fpsCap: 30,
    budgetMs: 1000 / 30,
    msaa: 4,
    splinters: 90,
    ambientMotion: true,
  },
  high: {
    maxPixelRatio: 2,
    fpsCap: 60,
    budgetMs: 1000 / 60,
    msaa: 4,
    splinters: 90,
    ambientMotion: true,
  },
};
