import { useId, useState } from "react";
import type { CostEstimate, ModelUsage, TokenTotals, UsageSummary } from "../backend/types";
import { estimateCost, formatUsd } from "../usage/cost";
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

/**
 * Tokens today, over 7 days and over the last 5 hours, and the agents running now
 * (spec §3.5). With the cost estimate on, each window also shows its cost at the price
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
  return (
    <section
      className={`rf-glass rf-widget rf-widget--usage${away ? " rf-widget--away" : ""}`}
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
      <p className={`rf-usage__agents${agents > 0 ? " rf-usage__agents--live" : ""}`}>
        {counted ? `${agents} ${agents === 1 ? "agent" : "agents"} running` : "Counting…"}
      </p>
    </section>
  );
}
