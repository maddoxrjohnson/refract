import type { SystemStats } from "../backend/types";
import { formatBytes, formatPercent } from "./format";
import "./ui.css";

export interface SystemWidgetProps {
  /** `null` until the first reading. */
  stats: SystemStats | null;
  /** Faded out while the detail panel is docked over this corner. */
  away: boolean;
}

/** One bar: the machine's use, with Refract's share of it drawn brighter at the start. */
function Bar({ label, total, own, value }: { label: string; total: number; own: number; value: string }) {
  const clamp = (v: number) => Math.min(100, Math.max(0, Number.isFinite(v) ? v : 0));
  return (
    <div className="rf-meter">
      <span className="rf-meter__label">{label}</span>
      <div
        className="rf-meter__track"
        role="meter"
        aria-label={label}
        aria-valuemin={0}
        aria-valuemax={100}
        aria-valuenow={Math.round(clamp(total))}
        aria-valuetext={value}
      >
        <div className="rf-meter__fill" style={{ width: `${clamp(total)}%` }} />
        <div className="rf-meter__own" style={{ width: `${Math.min(clamp(own), clamp(total))}%` }} />
      </div>
      <span className="rf-meter__value rf-mono">{value}</span>
    </div>
  );
}

/** CPU, memory and GPU use, and how much of it is Refract (spec §3.5). Top right. */
export function SystemWidget({ stats, away }: SystemWidgetProps) {
  const memPercent = stats && stats.memTotalBytes > 0 ? (stats.memUsedBytes / stats.memTotalBytes) * 100 : 0;
  const ownMemPercent = stats && stats.memTotalBytes > 0 ? (stats.own.memBytes / stats.memTotalBytes) * 100 : 0;
  return (
    <section
      className={`rf-glass rf-widget rf-widget--system${away ? " rf-widget--away" : ""}`}
      aria-label="System"
      aria-hidden={away || undefined}
    >
      <h2 className="rf-widget__title">System</h2>
      <Bar
        label="CPU"
        total={stats?.cpuPercent ?? 0}
        own={stats?.own.cpuPercent ?? 0}
        value={stats ? formatPercent(stats.cpuPercent) : "–"}
      />
      <Bar
        label="Memory"
        total={memPercent}
        own={ownMemPercent}
        value={stats ? formatBytes(stats.memUsedBytes) : "–"}
      />
      <Bar
        label="GPU"
        total={stats?.gpuPercent ?? 0}
        own={stats?.own.gpuPercent ?? 0}
        value={stats ? formatPercent(stats.gpuPercent) : "–"}
      />
      <p className="rf-widget__own">
        <span className="rf-widget__swatch" aria-hidden="true" />
        Refract{" "}
        <span className="rf-mono">
          {stats
            ? `${formatPercent(stats.own.cpuPercent)} CPU · ${formatBytes(stats.own.memBytes)} · ${formatPercent(stats.own.gpuPercent)} GPU`
            : "–"}
        </span>
      </p>
    </section>
  );
}
