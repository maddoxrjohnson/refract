import type { ModelUsage, PriceRow } from "../backend/types";
import defaults from "./defaultPrices.json";

/**
 * The prices shipped with the app, from Anthropic's pricing page on the date in
 * `PRICES_TAKEN_ON`. The Windows side reads the same file for settings saved before the
 * cost estimate existed.
 */
export const DEFAULT_PRICES: readonly PriceRow[] = defaults.prices;
export const PRICES_TAKEN_ON: string = defaults.takenOn;

/** Limits the Windows side also checks: rows, characters in a match, and a price. */
export const MAX_PRICE_ROWS = 50;
export const MAX_MATCH_LEN = 100;
export const MAX_PRICE = 10_000;

/** An estimate for one window. */
export interface CostEstimateResult {
  /** US dollars at the table's prices, for the tokens it could price. */
  usd: number;
  /** Tokens of models no row matched, left out of `usd`. */
  unpricedTokens: number;
  /** Model ids no row matched; "" for entries that named no model. */
  unpricedModels: string[];
}

/** The first row whose `match` is in `model`, ignoring case. Blank matches match nothing. */
export function priceFor(model: string, prices: readonly PriceRow[]): PriceRow | null {
  const id = model.toLowerCase();
  for (const row of prices) {
    const match = row.match.trim().toLowerCase();
    if (match && id.includes(match)) return row;
  }
  return null;
}

/** A window's cost at `prices` (US dollars per million tokens). */
export function estimateCost(models: readonly ModelUsage[], prices: readonly PriceRow[]): CostEstimateResult {
  let usd = 0;
  let unpricedTokens = 0;
  const unpricedModels: string[] = [];
  for (const m of models) {
    const row = priceFor(m.model, prices);
    if (!row) {
      unpricedTokens += m.input + m.output + m.cacheRead + m.cacheWrite;
      unpricedModels.push(m.model);
      continue;
    }
    // A count from the helper is never above the total, but stay safe if it is.
    const write1h = Math.min(m.cacheWrite1h, m.cacheWrite);
    usd +=
      (m.input * row.input +
        m.output * row.output +
        (m.cacheWrite - write1h) * row.cacheWrite5m +
        write1h * row.cacheWrite1h +
        m.cacheRead * row.cacheRead) /
      1_000_000;
  }
  return { usd, unpricedTokens, unpricedModels };
}

/**
 * Dollars, short: "$0.42", "$12.30", "$148", "$1,204", "$12.3k". Each form is chosen by
 * the amount as it would be shown, so 99.996 reads "$100", not "$100.00". Not a number
 * reads "–".
 */
export function formatUsd(usd: number): string {
  if (!Number.isFinite(usd)) return "–";
  if (usd > 0 && usd < 0.005) return "<$0.01";
  if (Math.round(usd * 100) < 10_000) return `$${usd.toFixed(2)}`;
  if (Math.round(usd) < 10_000) return `$${Math.round(usd).toLocaleString("en")}`;
  return `$${(usd / 1000).toFixed(1)}k`;
}

/**
 * For each row, the index of an earlier row that always matches first, so this one is
 * never used; `null` when none does. Blank matches neither hide nor are hidden.
 */
export function shadowedBy(matches: readonly string[]): (number | null)[] {
  const clean = matches.map((m) => m.trim().toLowerCase());
  return clean.map((m, i) => {
    if (!m) return null;
    const j = clean.slice(0, i).findIndex((earlier) => earlier !== "" && m.includes(earlier));
    return j === -1 ? null : j;
  });
}
