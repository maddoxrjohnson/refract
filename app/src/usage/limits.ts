import type { LimitWindow, PlanLimits } from "../backend/types";

/**
 * How the usage widget shows the plan limits Claude Code's status line relays
 * (spec §3.5). The percentages are only ever the status line's; when a reading cannot
 * be trusted any more it is marked or withheld, never estimated.
 */

/** A reading older than this is shown "as of" its time. */
export const STALE_AFTER_MS = 15 * 60_000;

/** A reading time further ahead of the helper's clock than this is not trusted. */
export const FUTURE_SLACK_MS = 60_000;

const DAY_MS = 24 * 3600_000;

/**
 * Where the readings come from, for the widget's tooltip and screen readers. The file
 * is `usage-state.json` in Claude Code's config directory unless agent.toml moves it.
 */
export const LIMITS_SOURCE = "From Claude Code's status line, via the helper's limits file";

/** One window, ready to show. */
export type LimitRow =
  | {
      kind: "reading";
      label: string;
      usedPercent: number;
      /** "37%"; ">999%" at most. */
      used: string;
      /** Whether the rounded percentage is 90 or more. */
      high: boolean;
      /** "15:30", or "Thu 03:00" on another day. */
      resets: string;
    }
  | {
      /** The window reset after the reading, so what it holds now is unknown. */
      kind: "reset";
      label: string;
    };

export interface LimitsView {
  rows: LimitRow[];
  /** The reading's time when it is older than `STALE_AFTER_MS`, else `null`. */
  asOf: string | null;
  /** When, on the app's clock, this view next changes by itself. */
  nextChangeAt: number;
}

/**
 * The helper's clock as of one answer: its `computedAt` and the app's time when the
 * answer arrived. WSL's clock can drift from Windows' (after sleep, say), and the
 * reading's `updatedAt` was taken on WSL's, so its age is measured on that clock.
 */
export interface HelperClock {
  computedAt: string;
  receivedAt: number;
}

const time = new Intl.DateTimeFormat("en-GB", { hour: "2-digit", minute: "2-digit", hourCycle: "h23" });
const weekday = new Intl.DateTimeFormat("en-GB", { weekday: "short" });
const date = new Intl.DateTimeFormat("en-GB", { day: "numeric", month: "short" });

function sameLocalDay(a: Date, b: Date): boolean {
  return a.getFullYear() === b.getFullYear() && a.getMonth() === b.getMonth() && a.getDate() === b.getDate();
}

/** The next local midnight after `now`. */
function nextMidnight(now: number): number {
  const d = new Date(now);
  return new Date(d.getFullYear(), d.getMonth(), d.getDate() + 1).getTime();
}

/**
 * Local "15:30" when `at` falls on today's date, else with the weekday ("Thu 03:00"),
 * or with the date ("20 Oct 08:00") when it is more than 6 days away.
 */
export function formatClock(at: Date, now: number): string {
  if (sameLocalDay(at, new Date(now))) return time.format(at);
  if (Math.abs(at.getTime() - now) > 6 * DAY_MS) return `${date.format(at)} ${time.format(at)}`;
  return `${weekday.format(at)} ${time.format(at)}`;
}

/** "37%", "<1%" for a sliver, "104%" past the limit, ">999%" beyond what fits. */
export function formatUsed(percent: number): string {
  if (percent > 0 && percent < 0.5) return "<1%";
  const rounded = Math.round(percent);
  return rounded > 999 ? ">999%" : `${rounded}%`;
}

/**
 * How old the reading `updatedAt` is at `now`: its age on the helper's clock when it
 * answered, plus the time since, on the app's. Without a readable `computedAt` the
 * app's clock stands in. A reading more than a minute ahead of the helper's clock is
 * taken as infinitely old.
 */
export function readingAge(updatedAt: number, clock: HelperClock | undefined, now: number): number {
  const computed = clock ? Date.parse(clock.computedAt) : NaN;
  if (!clock || !Number.isFinite(computed)) return now - updatedAt;
  if (updatedAt - computed > FUTURE_SLACK_MS) return Infinity;
  return computed - updatedAt + Math.max(0, now - clock.receivedAt);
}

function row(label: string, window: LimitWindow | undefined, now: number): LimitRow | null {
  if (!window) return null;
  const resetsAt = new Date(window.resetsAt);
  if (!Number.isFinite(resetsAt.getTime()) || !Number.isFinite(window.usedPercent)) return null;
  if (resetsAt.getTime() <= now) return { kind: "reset", label };
  return {
    kind: "reading",
    label,
    usedPercent: window.usedPercent,
    used: formatUsed(window.usedPercent),
    high: Math.round(window.usedPercent) >= 90,
    resets: formatClock(resetsAt, now),
  };
}

/**
 * The rows to show at `now` (milliseconds on the app's clock), or `null` when there is
 * nothing to show: no reading, or one with neither window.
 * - A window whose reset time has passed on the app's clock shows as reset, without
 *   its old percentage.
 * - A reading older than 15 minutes, by `readingAge`, keeps its values and says when
 *   it was taken.
 * `nextChangeAt` is the earliest of the next reset, the moment the reading turns
 * stale, and the next local midnight (when "Thu" may become "today").
 */
export function limitsView(
  limits: PlanLimits | undefined,
  now: number,
  clock?: HelperClock,
): LimitsView | null {
  if (!limits) return null;
  const updated = Date.parse(limits.updatedAt);
  if (!Number.isFinite(updated)) return null;
  const rows = [row("5-hour limit", limits.fiveHour, now), row("Weekly limit", limits.sevenDay, now)].filter(
    (r): r is LimitRow => r !== null,
  );
  if (rows.length === 0) return null;
  const age = readingAge(updated, clock, now);
  const stale = age > STALE_AFTER_MS;
  const changes = [nextMidnight(now)];
  if (!stale) changes.push(now + (STALE_AFTER_MS - age) + 1);
  for (const w of [limits.fiveHour, limits.sevenDay]) {
    const at = w ? Date.parse(w.resetsAt) : NaN;
    if (at > now) changes.push(at);
  }
  return {
    rows,
    asOf: stale ? formatClock(new Date(updated), now) : null,
    nextChangeAt: Math.min(...changes),
  };
}
