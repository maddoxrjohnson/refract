import type { SystemStats } from "../backend/types";
import { formatBytes, formatPercent } from "./format";
import "./ui.css";

export interface SystemWidgetProps {
  /** `null` until the first reading. */
  stats: SystemStats | null;
  /** Faded out while the detail panel is docked over this corner. */
  away: boolean;
}

/** A share of the bar, 0 to 100. */
const clamp = (v: number) => Math.min(100, Math.max(0, Number.isFinite(v) ? v : 0));

/** What one bar shows, each as a share of the bar (0 to 100). */
export interface BarShares {
  /** The machine's use. */
  total: number;
  /** Refract's share, both sides together. */
  own: number;
  /** WSL's part of `total`, or undefined when the bar is not split. */
  wsl?: number;
  /** The helper's part of `own`, inside WSL's. */
  wslOwn?: number;
  /**
   * What Linux programs use of WSL's part, drawn solid from its start. The rest of it,
   * file cache and memory Linux has freed but not handed back, is drawn fainter and
   * striped. Undefined when all of WSL's part is drawn solid.
   */
  wslPrograms?: number;
}

/** Where each piece of a bar goes, as `[left, width]` in percent of the track. */
export interface BarLayout {
  fill: [number, number];
  /** Absent when the bar is not split. */
  wsl?: [number, number];
  /** The striped part at the end of WSL's; absent unless `wslPrograms` was given. */
  wslCache?: [number, number];
  wslOwn?: [number, number];
  own: [number, number];
  /** Where the 1 px gap between WSL's part and Windows' goes; absent when one is empty. */
  gap?: number;
}

/**
 * Lays out a bar (spec §3.5): WSL first, then Windows, with Refract's mark at the start of
 * each, sized to its share there and kept inside its segment. A bar that isn't split is the
 * machine's use with Refract's share from the start, as before the split.
 */
export function layoutBar({ total, own, wsl, wslOwn = 0, wslPrograms }: BarShares): BarLayout {
  const t = clamp(total);
  if (wsl === undefined) return { fill: [0, t], own: [0, Math.min(clamp(own), t)] };
  const w = Math.min(clamp(wsl), t);
  const helper = Math.min(clamp(wslOwn), w);
  const programs = wslPrograms === undefined ? undefined : Math.min(clamp(wslPrograms), w);
  return {
    fill: [0, t],
    wsl: [0, w],
    ...(programs !== undefined && { wslCache: [programs, w - programs] as [number, number] }),
    wslOwn: [0, helper],
    own: [w, Math.min(Math.max(0, clamp(own) - helper), t - w)],
    ...(w > 0 && w < t && { gap: w }),
  };
}

function Piece({ className, at }: { className: string; at: [number, number] | undefined }) {
  if (!at) return null;
  return <div className={className} style={{ left: `${at[0]}%`, width: `${at[1]}%` }} />;
}

/** One bar: the machine's use, split into WSL and Windows when there is a WSL figure. */
function Bar({ label, shares, value, wslValue }: { label: string; shares: BarShares; value: string; wslValue?: string }) {
  const layout = layoutBar(shares);
  return (
    <div className="rf-meter">
      <span className="rf-meter__label">{label}</span>
      <div
        className="rf-meter__track"
        role="meter"
        aria-label={label}
        aria-valuemin={0}
        aria-valuemax={100}
        aria-valuenow={Math.round(clamp(shares.total))}
        aria-valuetext={wslValue === undefined ? value : `${value}, WSL ${wslValue}`}
      >
        <Piece className="rf-meter__fill" at={layout.fill} />
        <Piece className="rf-meter__wsl" at={layout.wsl} />
        <Piece className="rf-meter__wsl-cache" at={layout.wslCache} />
        {/* Before Refract's marks, so a 1 px mark at the start of Windows' part stays on top. */}
        {layout.gap !== undefined && <div className="rf-meter__gap" style={{ left: `${layout.gap}%` }} />}
        <Piece className="rf-meter__own" at={layout.wslOwn} />
        <Piece className="rf-meter__own" at={layout.own} />
      </div>
      <span className="rf-meter__value rf-mono">{value}</span>
    </div>
  );
}

/**
 * CPU, memory and GPU use, how much of it is WSL and how much is Refract (spec §3.5,
 * .plans/system-split-spec.md). Top right.
 */
export function SystemWidget({ stats, away }: SystemWidgetProps) {
  const memShare = (bytes: number) => (stats && stats.memTotalBytes > 0 ? (bytes / stats.memTotalBytes) * 100 : 0);
  const wsl = stats?.wsl ?? null;
  const wslGpu = wsl?.gpuPercent ?? null;
  const own = stats
    ? `${formatPercent(stats.own.cpuPercent)} CPU · ${formatBytes(stats.own.memBytes)} · ${formatPercent(stats.own.gpuPercent)} GPU`
    : "–";
  return (
    <section
      className={`rf-glass rf-widget rf-widget--system${wsl ? " rf-widget--wsl" : ""}${away ? " rf-widget--away" : ""}`}
      aria-label="System"
      aria-hidden={away || undefined}
    >
      <h2 className="rf-widget__title">System</h2>
      <Bar
        label="CPU"
        shares={{
          total: stats?.cpuPercent ?? 0,
          own: stats?.own.cpuPercent ?? 0,
          ...(stats && wsl && { wsl: wsl.cpuPercent, wslOwn: stats.own.wslCpuPercent }),
        }}
        value={stats ? formatPercent(stats.cpuPercent) : "–"}
        wslValue={wsl ? formatPercent(wsl.cpuPercent) : undefined}
      />
      <Bar
        label="Memory"
        shares={{
          total: memShare(stats?.memUsedBytes ?? 0),
          own: memShare(stats?.own.memBytes ?? 0),
          ...(stats &&
            wsl && {
              wsl: memShare(wsl.memBytes),
              wslOwn: memShare(stats.own.wslMemBytes),
              wslPrograms: memShare(wsl.memProgramBytes),
            }),
        }}
        value={stats ? formatBytes(stats.memUsedBytes) : "–"}
        wslValue={
          wsl
            ? `${formatBytes(wsl.memBytes)} (${formatBytes(Math.max(0, wsl.memBytes - wsl.memProgramBytes))} cache)`
            : undefined
        }
      />
      <Bar
        label="GPU"
        shares={{
          total: stats?.gpuPercent ?? 0,
          own: stats?.own.gpuPercent ?? 0,
          // Refract has no GPU share inside WSL.
          ...(wslGpu !== null && { wsl: wslGpu }),
        }}
        value={stats ? formatPercent(stats.gpuPercent) : "–"}
        wslValue={wslGpu !== null ? formatPercent(wslGpu) : undefined}
      />
      {wsl ? (
        <>
          <p className="rf-widget__legend" aria-hidden="true">
            <span>
              <span className="rf-widget__swatch rf-widget__swatch--wsl" />
              WSL
            </span>
            <span>
              <span className="rf-widget__swatch rf-widget__swatch--cache" />
              cache
            </span>
            <span>
              <span className="rf-widget__swatch rf-widget__swatch--windows" />
              Windows
            </span>
            <span>
              <span className="rf-widget__swatch" />
              Refract
            </span>
          </p>
          <p className="rf-widget__own rf-widget__own--line rf-widget__own--wsl">
            <span className="rf-widget__line-label">WSL</span>{" "}
            <span className="rf-mono">
              {`${formatPercent(wsl.cpuPercent)} CPU · ${formatBytes(wsl.memBytes)}`}
              {wslGpu !== null && ` · ${formatPercent(wslGpu)} GPU`}
            </span>
          </p>
          <p className="rf-widget__own rf-widget__own--line">
            <span className="rf-widget__line-label">
              {/* Stands in for the legend when that is left out on short windows. */}
              <span className="rf-widget__swatch rf-widget__swatch--short" aria-hidden="true" />
              Refract
            </span>{" "}
            <span className="rf-mono">{own}</span>
          </p>
        </>
      ) : (
        <p className="rf-widget__own">
          <span className="rf-widget__swatch" aria-hidden="true" />
          Refract <span className="rf-mono">{own}</span>
        </p>
      )}
    </section>
  );
}
