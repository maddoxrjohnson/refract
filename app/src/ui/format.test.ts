import { describe, expect, it } from "vitest";
import type { SessionInfo } from "../backend/types";
import {
  firstLine,
  formatBytes,
  formatCount,
  formatPercent,
  formatSize,
  relativeTime,
  sessionStateLabel,
} from "./format";

const NOW = Date.UTC(2026, 9, 30, 12);
const ago = (seconds: number) => NOW - seconds * 1000;

describe("relativeTime", () => {
  it("says how long ago, in the largest whole unit", () => {
    expect(relativeTime(ago(10), NOW)).toBe("just now");
    expect(relativeTime(ago(5 * 60), NOW)).toBe("5 minutes ago");
    expect(relativeTime(ago(2 * 3600), NOW)).toBe("2 hours ago");
    expect(relativeTime(ago(26 * 3600), NOW)).toBe("yesterday");
    expect(relativeTime(ago(4 * 86400), NOW)).toBe("4 days ago");
    expect(relativeTime(ago(20 * 86400), NOW)).toBe("2 weeks ago");
    expect(relativeTime(ago(90 * 86400), NOW)).toBe("3 months ago");
    expect(relativeTime(ago(800 * 86400), NOW)).toBe("2 years ago");
  });

  it("treats a time slightly in the future as now", () => {
    expect(relativeTime(NOW + 5000, NOW)).toBe("just now");
  });
});

describe("formatSize", () => {
  it("uses KB below a megabyte and MB above", () => {
    expect(formatSize(300)).toBe("1 KB");
    expect(formatSize(64_000)).toBe("63 KB");
    expect(formatSize(2_600_000)).toBe("2.5 MB");
    expect(formatSize(40 * 1024 * 1024)).toBe("40 MB");
  });
});

describe("firstLine", () => {
  it("takes the first non-empty line, trimmed", () => {
    expect(firstLine("\n  Done.  \nMore")).toBe("Done.");
    expect(firstLine(null)).toBeNull();
    expect(firstLine("   ")).toBeNull();
  });
});

describe("sessionStateLabel", () => {
  const s = (state: SessionInfo["state"]) => sessionStateLabel(state);
  it("names each state", () => {
    expect(s({ kind: "finished", detail: "completed" })).toBe("Finished");
    expect(s({ kind: "finished", detail: "userInterrupted" })).toBe("Stopped by you");
    expect(s({ kind: "interrupted", detail: "apiError" })).toBe("Didn't finish");
    expect(s({ kind: "live", detail: "working" })).toBe("Working now");
    expect(s({ kind: "live", detail: "waitingOnUser" })).toBe("Waiting for you");
    expect(s({ kind: "live", detail: "idle" })).toBe("Open");
  });
});

describe("formatCount", () => {
  it("shortens to K, M and B", () => {
    expect(formatCount(0)).toBe("0");
    expect(formatCount(950)).toBe("950");
    expect(formatCount(12_345)).toBe("12.3K");
    expect(formatCount(1_000)).toBe("1K");
    expect(formatCount(1_234_567)).toBe("1.2M");
    expect(formatCount(31_040_000)).toBe("31M");
    expect(formatCount(412_000_000)).toBe("412M");
    expect(formatCount(4_000_000_000)).toBe("4B");
  });

  it("carries rounding up a unit and ignores odd values", () => {
    expect(formatCount(999_600)).toBe("1M");
    expect(formatCount(999_400)).toBe("999K");
    expect(formatCount(-5)).toBe("0");
    expect(formatCount(Number.NaN)).toBe("0");
  });
});

describe("formatBytes and formatPercent", () => {
  it("formats memory and percentages", () => {
    expect(formatBytes(210 * 1024 ** 2)).toBe("210 MB");
    expect(formatBytes(13.44 * 1024 ** 3)).toBe("13.4 GB");
    expect(formatBytes(32 * 1024 ** 3)).toBe("32 GB");
    expect(formatPercent(0.44)).toBe("0.4%");
    expect(formatPercent(7)).toBe("7%");
    expect(formatPercent(23.6)).toBe("24%");
    expect(formatPercent(-1)).toBe("0%");
  });
});
