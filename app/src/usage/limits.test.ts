import { describe, expect, it } from "vitest";
import type { PlanLimits } from "../backend/types";
import { formatClock, formatUsed, limitsView, readingAge, STALE_AFTER_MS } from "./limits";

// Local times, so the expected clock text holds in any time zone.
const NOW = new Date(2026, 9, 30, 14, 10).getTime(); // a Friday
const at = (day: number, hour: number, minute = 0) => new Date(2026, 9, day, hour, minute).toISOString();

const LIMITS: PlanLimits = {
  fiveHour: { usedPercent: 37.4, resetsAt: at(30, 15, 30) },
  sevenDay: { usedPercent: 58, resetsAt: at(33, 3) },
  updatedAt: new Date(NOW - 60_000).toISOString(),
};

describe("limitsView", () => {
  it("shows both windows with their reset times", () => {
    const view = limitsView(LIMITS, NOW);
    expect(view).toEqual({
      rows: [
        { kind: "reading", label: "5-hour limit", usedPercent: 37.4, used: "37%", high: false, resets: "15:30" },
        { kind: "reading", label: "Weekly limit", usedPercent: 58, used: "58%", high: false, resets: "Mon 03:00" },
      ],
      asOf: null,
      // The reading turns stale before either window resets.
      nextChangeAt: NOW - 60_000 + STALE_AFTER_MS + 1,
    });
  });

  it("gives a reset on another date its weekday, however close", () => {
    const view = limitsView(
      {
        ...LIMITS,
        fiveHour: { usedPercent: 3, resetsAt: at(31, 1) },
        sevenDay: { usedPercent: 90, resetsAt: at(31, 9) },
      },
      NOW,
    );
    expect(view?.rows.map((r) => r.kind === "reading" && r.resets)).toEqual(["Sat 01:00", "Sat 09:00"]);
    // Late in the evening, a reset an hour away, past midnight.
    const late = new Date(2026, 9, 30, 23, 30).getTime();
    const limits = { ...LIMITS, fiveHour: { usedPercent: 3, resetsAt: at(31, 0, 30) }, updatedAt: at(30, 23, 29) };
    expect(limitsView(limits, late)?.rows[0]).toMatchObject({ resets: "Sat 00:30" });
  });

  it("turns amber from a rounded 90%", () => {
    const high = (usedPercent: number) =>
      limitsView({ ...LIMITS, fiveHour: { usedPercent, resetsAt: at(30, 15) } }, NOW)?.rows[0];
    expect(high(89.4)).toMatchObject({ used: "89%", high: false });
    expect(high(89.5)).toMatchObject({ used: "90%", high: true });
    expect(high(90)).toMatchObject({ high: true });
  });

  it("changes by itself at the next reset, staleness or midnight", () => {
    const limits = { ...LIMITS, fiveHour: { usedPercent: 3, resetsAt: at(30, 14, 12) } };
    expect(limitsView(limits, NOW)?.nextChangeAt).toBe(Date.parse(at(30, 14, 12)));
    const stale = { ...LIMITS, updatedAt: at(30, 9) };
    expect(limitsView(stale, NOW)?.nextChangeAt).toBe(Date.parse(at(30, 15, 30)));
    const weekOnly = { sevenDay: LIMITS.sevenDay, updatedAt: at(30, 9) };
    expect(limitsView(weekOnly, NOW)?.nextChangeAt).toBe(new Date(2026, 9, 31).getTime());
  });

  it("withholds a percentage whose window has reset since the reading", () => {
    const view = limitsView({ ...LIMITS, fiveHour: { usedPercent: 96, resetsAt: at(30, 14, 10) } }, NOW);
    expect(view?.rows[0]).toEqual({ kind: "reset", label: "5-hour limit" });
    expect(view?.rows[1].kind).toBe("reading");
  });

  it("marks a reading older than 15 minutes with its time", () => {
    const fresh = { ...LIMITS, updatedAt: new Date(NOW - STALE_AFTER_MS).toISOString() };
    expect(limitsView(fresh, NOW)?.asOf).toBeNull();
    const stale = { ...LIMITS, updatedAt: at(30, 13, 2) };
    const view = limitsView(stale, NOW);
    expect(view?.asOf).toBe("13:02");
    // The values are still shown.
    expect(view?.rows[0]).toMatchObject({ kind: "reading", used: "37%" });
    expect(limitsView({ ...LIMITS, updatedAt: at(28, 22, 15) }, NOW)?.asOf).toBe("Wed 22:15");
    expect(limitsView({ ...LIMITS, updatedAt: at(20, 8) }, NOW)?.asOf).toBe("20 Oct 08:00");
  });

  it("measures the reading's age on the helper's clock", () => {
    // WSL's clock is 20 minutes behind Windows': the reading is 1 minute old by it.
    const behind = { computedAt: new Date(NOW - 20 * 60_000).toISOString(), receivedAt: NOW };
    const limits = { ...LIMITS, updatedAt: new Date(NOW - 21 * 60_000).toISOString() };
    expect(limitsView(limits, NOW)?.asOf).not.toBeNull();
    expect(limitsView(limits, NOW, behind)?.asOf).toBeNull();
    expect(readingAge(Date.parse(limits.updatedAt), behind, NOW)).toBe(60_000);
    // The time since the answer arrived counts on the app's clock.
    expect(readingAge(Date.parse(limits.updatedAt), behind, NOW + 14 * 60_000)).toBe(15 * 60_000);
    expect(limitsView(limits, NOW + 14 * 60_000 + 1, behind)?.asOf).not.toBeNull();
    // An unreadable computedAt falls back to the app's clock.
    expect(readingAge(NOW - 5_000, { computedAt: "", receivedAt: NOW }, NOW)).toBe(5_000);
  });

  it("takes a reading from the future as stale", () => {
    const clock = { computedAt: new Date(NOW).toISOString(), receivedAt: NOW };
    const slightly = { ...LIMITS, updatedAt: new Date(NOW + 30_000).toISOString() };
    expect(limitsView(slightly, NOW, clock)?.asOf).toBeNull();
    const ahead = { ...LIMITS, updatedAt: at(30, 14, 30) };
    expect(readingAge(Date.parse(ahead.updatedAt), clock, NOW)).toBe(Infinity);
    expect(limitsView(ahead, NOW, clock)?.asOf).toBe("14:30");
  });

  it("shows nothing without a reading or with neither window", () => {
    expect(limitsView(undefined, NOW)).toBeNull();
    expect(limitsView({ updatedAt: LIMITS.updatedAt }, NOW)).toBeNull();
    expect(limitsView({ ...LIMITS, updatedAt: "not a time" }, NOW)).toBeNull();
    const one = limitsView({ sevenDay: { usedPercent: 6, resetsAt: at(33, 3) }, updatedAt: LIMITS.updatedAt }, NOW);
    expect(one?.rows.map((r) => r.label)).toEqual(["Weekly limit"]);
    // A window with an unreadable time is left out.
    const bad = limitsView({ ...LIMITS, fiveHour: { usedPercent: 3, resetsAt: "soon" } }, NOW);
    expect(bad?.rows.map((r) => r.label)).toEqual(["Weekly limit"]);
  });
});

describe("formatting", () => {
  it("rounds percentages and keeps slivers visible", () => {
    expect(formatUsed(0)).toBe("0%");
    expect(formatUsed(0.2)).toBe("<1%");
    expect(formatUsed(2.0)).toBe("2%");
    expect(formatUsed(104.4)).toBe("104%");
    expect(formatUsed(999.4)).toBe("999%");
    expect(formatUsed(999.5)).toBe(">999%");
  });

  it("uses local 24-hour clock times", () => {
    expect(formatClock(new Date(2026, 9, 30, 0, 5), NOW)).toBe("00:05");
    expect(formatClock(new Date(2026, 9, 29, 23, 55), NOW)).toBe("Thu 23:55");
    expect(formatClock(new Date(2026, 10, 2, 3, 0), NOW)).toBe("Mon 03:00");
  });
});
