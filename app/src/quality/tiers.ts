// The three graphics levels from the spec (§3.6). The scene reads these settings; the
// controller in ./controller.ts picks which level is active.

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
  /** Refraction samples per colour channel. */
  samples: number;
  /** Rainbow split at the edges, 0 to 1. */
  chroma: number;
  /** Resolution of the texture behind the glass, as a share of the canvas. */
  backScale: number;
  /** Facet glints, 0 to 1. */
  sparkle: number;
  /** Thin-film colour at grazing angles, 0 to 1. */
  irid: number;
  /** Points on each rough-cut gem's hull. */
  facets: number;
  bloom: boolean;
  depthOfField: boolean;
  /** Gems spin and the scene breathes. Off on Low, which only draws on change. */
  ambientMotion: boolean;
}

export const TIERS: Record<QualityLevel, Tier> = {
  low: {
    maxPixelRatio: 1,
    fpsCap: 30,
    budgetMs: 1000 / 30,
    samples: 1,
    chroma: 0,
    backScale: 0.5,
    sparkle: 0,
    irid: 0.4,
    facets: 18,
    bloom: false,
    depthOfField: false,
    ambientMotion: false,
  },
  medium: {
    maxPixelRatio: 1.5,
    fpsCap: 30,
    budgetMs: 1000 / 30,
    samples: 3,
    chroma: 0.6,
    backScale: 0.75,
    sparkle: 0.45,
    irid: 0.8,
    facets: 28,
    bloom: true,
    depthOfField: false,
    ambientMotion: true,
  },
  high: {
    maxPixelRatio: 2,
    fpsCap: 60,
    budgetMs: 1000 / 60,
    samples: 12,
    chroma: 1,
    backScale: 1,
    sparkle: 1,
    irid: 1,
    facets: 40,
    bloom: true,
    depthOfField: true,
    ambientMotion: true,
  },
};
