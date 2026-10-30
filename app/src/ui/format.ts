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
