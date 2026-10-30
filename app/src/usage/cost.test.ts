import { describe, expect, it } from "vitest";
import type { ModelUsage, PriceRow } from "../backend/types";
import {
  DEFAULT_PRICES,
  MAX_PRICE_ROWS,
  PRICES_TAKEN_ON,
  estimateCost,
  formatUsd,
  priceFor,
  shadowedBy,
} from "./cost";

const row = (match: string, input: number, extra: Partial<PriceRow> = {}): PriceRow => ({
  match,
  input,
  output: 0,
  cacheWrite5m: 0,
  cacheWrite1h: 0,
  cacheRead: 0,
  ...extra,
});

const usage = (model: string, counts: Partial<ModelUsage> = {}): ModelUsage => ({
  model,
  input: 0,
  output: 0,
  cacheRead: 0,
  cacheWrite: 0,
  cacheWrite1h: 0,
  ...counts,
});

describe("priceFor", () => {
  it("takes the first row that matches, ignoring case", () => {
    const prices = [row("opus-5-5", 4), row("OPUS", 5), row("", 99)];
    expect(priceFor("claude-opus-5-5", prices)?.input).toBe(4);
    expect(priceFor("claude-opus-5", prices)?.input).toBe(5);
    expect(priceFor("Claude-Opus-4-8", prices)?.input).toBe(5);
    // Order decides: a broad row first hides the narrower one.
    expect(priceFor("claude-opus-5-5", [row("opus", 5), row("opus-5-5", 4)])?.input).toBe(5);
    // Blank rows match nothing.
    expect(priceFor("claude-sonnet-5-5", prices)).toBeNull();
  });

  it("prices every model seen in real transcripts by default", () => {
    const at = (model: string) => priceFor(model, DEFAULT_PRICES);
    expect(at("claude-opus-5-5")).toMatchObject({ input: 4, output: 20, cacheRead: 0.2 });
    expect(at("claude-opus-5")).toMatchObject({ input: 5, output: 25 });
    expect(at("claude-fable-5-1")).toMatchObject({ input: 10, cacheRead: 0.25 });
    expect(at("claude-fable-5")).toMatchObject({ input: 10, cacheRead: 1 });
    expect(at("claude-sonnet-5-5")).toMatchObject({ input: 2, cacheRead: 0.1 });
    expect(at("claude-sonnet-5")).toMatchObject({ input: 2, cacheRead: 0.2 });
    expect(at("claude-haiku-5-5")).toMatchObject({ input: 0.1, cacheWrite5m: 0.125 });
    expect(at("claude-opus-4-1-20250805")).toMatchObject({ input: 15 });
    expect(at("claude-opus-4-8")).toMatchObject({ input: 5 });
    expect(at("<synthetic>")).toBeNull();
    expect(at("")).toBeNull();
    expect(DEFAULT_PRICES.length).toBeLessThanOrEqual(MAX_PRICE_ROWS);
    expect(PRICES_TAKEN_ON).toMatch(/^\d{4}-\d{2}-\d{2}$/);
  });
});

describe("estimateCost", () => {
  const prices = [row("opus", 5, { output: 25, cacheWrite5m: 6.25, cacheWrite1h: 10, cacheRead: 0.5 })];

  it("prices each kind of token, splitting cache writes by lifetime", () => {
    const result = estimateCost(
      [
        usage("claude-opus-5", {
          input: 1_000_000,
          output: 2_000_000,
          cacheRead: 10_000_000,
          cacheWrite: 3_000_000,
          cacheWrite1h: 1_000_000,
        }),
      ],
      prices,
    );
    // 5 + 50 + 5 + 2 × 6.25 + 10
    expect(result.usd).toBeCloseTo(82.5, 10);
    expect(result.unpricedTokens).toBe(0);
    expect(result.unpricedModels).toEqual([]);
  });

  it("counts writes with no lifetime split as 5-minute and caps the 1-hour part", () => {
    expect(estimateCost([usage("opus", { cacheWrite: 1_000_000 })], prices).usd).toBeCloseTo(6.25, 10);
    expect(estimateCost([usage("opus", { cacheWrite: 1_000_000, cacheWrite1h: 5_000_000 })], prices).usd).toBeCloseTo(
      10,
      10,
    );
  });

  it("leaves unknown models out and counts their tokens", () => {
    const result = estimateCost(
      [
        usage("claude-opus-5", { input: 1_000_000 }),
        usage("claude-sonnet-5-5", { input: 10, output: 20, cacheRead: 30, cacheWrite: 40 }),
        usage("", { input: 5 }),
      ],
      prices,
    );
    expect(result.usd).toBeCloseTo(5, 10);
    expect(result.unpricedTokens).toBe(105);
    expect(result.unpricedModels).toEqual(["claude-sonnet-5-5", ""]);
  });

  it("is zero with no usage or no prices", () => {
    expect(estimateCost([], prices)).toEqual({ usd: 0, unpricedTokens: 0, unpricedModels: [] });
    expect(estimateCost([usage("opus", { input: 7 })], []).unpricedTokens).toBe(7);
  });
});

describe("formatUsd", () => {
  it("keeps amounts short", () => {
    expect(formatUsd(0)).toBe("$0.00");
    expect(formatUsd(0.004)).toBe("<$0.01");
    expect(formatUsd(0.42)).toBe("$0.42");
    expect(formatUsd(12.3)).toBe("$12.30");
    expect(formatUsd(148.4)).toBe("$148");
    expect(formatUsd(1204.2)).toBe("$1,204");
    expect(formatUsd(12_345)).toBe("$12.3k");
  });

  it("picks the form by the rounded amount and survives non-numbers", () => {
    expect(formatUsd(0.005)).toBe("$0.01");
    expect(formatUsd(99.994)).toBe("$99.99");
    expect(formatUsd(99.996)).toBe("$100");
    expect(formatUsd(9999.4)).toBe("$9,999");
    expect(formatUsd(9999.6)).toBe("$10.0k");
    expect(formatUsd(NaN)).toBe("–");
    expect(formatUsd(Infinity)).toBe("–");
  });
});

describe("shadowedBy", () => {
  it("finds rows an earlier, shorter match always wins", () => {
    expect(shadowedBy(["opus", "opus-5-5", "", "sonnet", "OPUS-4"])).toEqual([null, 0, null, null, 0]);
    expect(shadowedBy(["opus-5-5", "opus"])).toEqual([null, null]);
    expect(shadowedBy(DEFAULT_PRICES.map((p) => p.match)).every((s) => s === null)).toBe(true);
  });
});
