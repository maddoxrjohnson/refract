import { useEffect, useId, useMemo, useState } from "react";
import type { CostEstimate, ModelUsage, TokenTotals, UsageSummary } from "../backend/types";
import { estimateCost, formatUsd } from "../usage/cost";
import { LIMITS_SOURCE, limitsView, type LimitRow } from "../usage/limits";
import { formatCount } from "./format";
import "./ui.css";

export interface UsageWidgetProps {
  /** `null` until the first answer; counting until it is `ready`. */
  usage: UsageSummary | null;
  /** Faded out while the detail panel is docked over this corner. */
  away: boolean;
  /** The cost estimate settings; no estimate unless enabled. */
  costEstimate?: CostEstimate | null;
}

const ESTIMATE_NOTE = "Estimated at API list prices from the price table in settings";

/** Every token Claude Code counted: input, output and both kinds of cache. */
export function totalTokens(t: TokenTotals): number {
  return t.input + t.output + t.cacheRead + t.cacheWrite;
}

/** How a total splits, for the row's tooltip. */
function split(t: TokenTotals): string {
  return [
    `${t.input.toLocaleString("en")} input`,
    `${t.output.toLocaleString("en")} output`,
    `${t.cacheRead.toLocaleString("en")} cache read`,
    `${t.cacheWrite.toLocaleString("en")} cache write`,
  ].join(", ");
}

/** The model ids no price matched, for the note's tooltip. */
function describeModels(models: string[]): string {
  return models.map((m) => m || "(no model)").join(", ");
}

/** One plan-limit window: label, percentage and reset time over a thin meter. */
function Limit({ row }: { row: LimitRow }) {
  if (row.kind === "reset") {
    return (
      <div className="rf-limit">
        <p className="rf-limit__line">
          <span className="rf-limit__label">{row.label}</span>
          <span className="rf-limit__reset">reset since last reading</span>
        </p>
      </div>
    );
  }
  const fill = Math.min(100, Math.max(0, row.usedPercent));
  return (
    <div className="rf-limit">
      <p className="rf-limit__line" aria-hidden="true">
        <span className="rf-limit__label">{row.label}</span>
        <span className="rf-limit__used rf-mono">{row.used}</span>
        <span className="rf-limit__reset">· resets {row.resets}</span>
      </p>
      <div
        className="rf-limit__track"
        role="meter"
        aria-label={row.label}
        aria-valuemin={0}
        aria-valuemax={100}
        aria-valuenow={Math.round(fill)}
        aria-valuetext={`${row.used} used, resets ${row.resets}`}
      >
        <div
          className={`rf-limit__fill${row.high ? " rf-limit__fill--high" : ""}`}
          style={{ width: `${fill}%` }}
        />
      </div>
    </div>
  );
}

/**
 * Tokens today, over 7 days and over the last 5 hours, the plan limits Claude Code's
 * status line reports, and the agents running now (spec §3.5). With the cost estimate on, each window also shows its cost at the price
 * table's rates. Hovering or focusing it lists the 7 days by repo. Bottom right.
 */
export function UsageWidget({ usage, away, costEstimate }: UsageWidgetProps) {
  const [open, setOpen] = useState(false);
  const listId = useId();
  // Before the helper's first read, its zeros are not real totals.
  const counted = usage?.ready ? usage : null;
  const byModel = costEstimate?.enabled ? counted?.byModel : undefined;
  const prices = costEstimate?.prices ?? [];
  const rows: [string, TokenTotals | undefined, ModelUsage[] | undefined][] = [
    ["Today", counted?.today, byModel?.today],
    ["7 days", counted?.last7Days, byModel?.last7Days],
    ["5 hours", counted?.last5Hours, byModel?.last5Hours],
  ];
  // The 7 days hold every model of the shorter windows, so they say what is unpriced.
  const week = byModel ? estimateCost(byModel.last7Days, prices) : null;
  const repos = counted?.byRepo.filter((r) => totalTokens(r.last7Days) > 0) ?? [];
  const showRepos = open && !away && repos.length > 0;
  const agents = usage?.liveAgents ?? 0;
  // The limits come from their own file, read whether or not the transcripts are. Their
  // age is measured on the helper's clock, from the moment its answer arrived.
  const receivedAt = useMemo(() => (usage ? Date.now() : 0), [usage]);
  const [, setTick] = useState(0);
  const limits = limitsView(
    usage?.limits,
    Date.now(),
    usage ? { computedAt: usage.computedAt, receivedAt } : undefined,
  );
  // Resets and staleness happen with no new answer too, such as when the helper stops:
  // render again when the view next changes by itself.
  const nextChangeAt = limits?.nextChangeAt;
  useEffect(() => {
    if (nextChangeAt === undefined) return;
    const delay = Math.min(Math.max(nextChangeAt - Date.now(), 1000), 3600_000);
    const timer = setTimeout(() => setTick((n) => n + 1), delay);
    return () => clearTimeout(timer);
  });
  return (
    <section
      className={[
        "rf-glass rf-widget rf-widget--usage",
        away && "rf-widget--away",
        limits && (limits.asOf ? "rf-widget--limits rf-widget--limits-stale" : "rf-widget--limits"),
      ]
        .filter(Boolean)
        .join(" ")}
      aria-label="Claude usage"
      aria-hidden={away || undefined}
      // Focusable so the breakdown opens from the keyboard as well as on hover.
      tabIndex={away ? -1 : 0}
      aria-describedby={showRepos ? listId : undefined}
      onMouseEnter={() => setOpen(true)}
      onMouseLeave={() => setOpen(false)}
      onFocus={() => setOpen(true)}
      onBlur={() => setOpen(false)}
    >
      {showRepos && (
        <div className="rf-usage__repos" id={listId}>
          <h3 className="rf-widget__title">Last 7 days by repo</h3>
          <ul>
            {repos.map((r) => (
              <li key={r.path} title={`${r.path}: ${split(r.last7Days)}`}>
                <span className="rf-usage__repo rf-mono">{r.name}</span>
                <span className="rf-mono">{formatCount(totalTokens(r.last7Days))}</span>
              </li>
            ))}
          </ul>
        </div>
      )}
      <h2 className="rf-widget__title">Claude usage</h2>
      <dl className="rf-usage__totals">
        {rows.map(([label, t, models]) => (
          <div key={label} title={t ? split(t) : undefined}>
            <dt>{label}</dt>
            {models && (
              <dd className="rf-usage__cost rf-mono" title={ESTIMATE_NOTE}>
                ≈ {formatUsd(estimateCost(models, prices).usd)}
              </dd>
            )}
            <dd className="rf-usage__tokens rf-mono">{t ? formatCount(totalTokens(t)) : "–"}</dd>
          </div>
        ))}
      </dl>
      {week && (
        <p className="rf-usage__note" title={ESTIMATE_NOTE}>
          {week.unpricedTokens > 0 ? (
            <span title={`No price for ${describeModels(week.unpricedModels)}`}>
              ≈ API list prices; {formatCount(week.unpricedTokens)} tokens unpriced
              <span className="rf-visually-hidden"> (no price for {describeModels(week.unpricedModels)})</span>
            </span>
          ) : (
            "≈ estimated at API list prices"
          )}
        </p>
      )}
      {costEstimate?.enabled && counted && !counted.byModel && (
        <p className="rf-usage__note">Cost needs a newer helper</p>
      )}
      {limits && (
        <div className="rf-limits" title={LIMITS_SOURCE}>
          {limits.rows.map((row) => (
            <Limit key={row.label} row={row} />
          ))}
          {limits.asOf && <p className="rf-usage__note">Limits as of {limits.asOf}</p>}
          <span className="rf-visually-hidden">{LIMITS_SOURCE}</span>
        </div>
      )}
      <p className={`rf-usage__agents${agents > 0 ? " rf-usage__agents--live" : ""}`}>
        {counted ? `${agents} ${agents === 1 ? "agent" : "agents"} running` : "Counting…"}
      </p>
    </section>
  );
}
