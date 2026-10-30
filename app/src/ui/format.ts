import type { SessionState } from "../backend/types";

// Wording shared by the overlays.

const UNITS: [Intl.RelativeTimeFormatUnit, number][] = [
  ["year", 365 * 86400],
  ["month", 30 * 86400],
  ["week", 7 * 86400],
  ["day", 86400],
  ["hour", 3600],
  ["minute", 60],
];

const relative = new Intl.RelativeTimeFormat("en", { numeric: "auto" });

/** "2 hours ago", "yesterday"; both arguments in milliseconds. */
export function relativeTime(thenMs: number, nowMs: number): string {
  const seconds = Math.max(0, (nowMs - thenMs) / 1000);
  if (seconds < 45) return "just now";
  for (const [unit, size] of UNITS) {
    if (seconds >= size) return relative.format(-Math.floor(seconds / size), unit);
  }
  return relative.format(-1, "minute");
}

/** A transcript size in KB below a megabyte, MB above. */
export function formatSize(bytes: number): string {
  const kb = bytes / 1024;
  if (kb < 1024) return `${Math.max(1, Math.round(kb))} KB`;
  const mb = kb / 1024;
  return `${mb < 10 ? mb.toFixed(1).replace(/\.0$/, "") : Math.round(mb)} MB`;
}

/** First non-empty line of a message, or `null`. */
export function firstLine(text: string | null): string | null {
  if (!text) return null;
  for (const line of text.split("\n")) {
    const t = line.trim();
    if (t) return t;
  }
  return null;
}

export function sessionStateLabel(state: SessionState): string {
  switch (state.kind) {
    case "finished":
      return state.detail === "completed" ? "Finished" : "Stopped by you";
    case "interrupted":
      return "Didn't finish";
    case "live":
      if (state.detail === "working") return "Working now";
      if (state.detail === "waitingOnUser") return "Waiting for you";
      return "Open";
  }
}

/** A count in compact form: 950, 12.3K, 1.2M, 4B. */
export function formatCount(n: number): string {
  if (!Number.isFinite(n) || n < 0) return "0";
  for (const [size, suffix] of [
    [1e9, "B"],
    [1e6, "M"],
    [1e3, "K"],
  ] as const) {
    // Rounding can carry up a unit: 999,960 is 1M, not 1000K.
    if (n >= size * 0.9995) {
      const v = n / size;
      return `${v < 100 ? v.toFixed(1).replace(/\.0$/, "") : Math.round(v)}${suffix}`;
    }
  }
  return String(Math.round(n));
}

/** Memory in GB with one decimal below 100 GB, or in MB below a gigabyte. */
export function formatBytes(bytes: number): string {
  const mb = bytes / 1024 ** 2;
  if (mb < 1024) return `${Math.round(mb)} MB`;
  const gb = mb / 1024;
  return `${gb < 100 ? gb.toFixed(1).replace(/\.0$/, "") : Math.round(gb)} GB`;
}

/** A percentage for the widgets: one decimal below 10, whole above. */
export function formatPercent(p: number): string {
  const v = Number.isFinite(p) ? Math.max(0, p) : 0;
  return `${v < 10 ? v.toFixed(1).replace(/\.0$/, "") : Math.round(v)}%`;
}
