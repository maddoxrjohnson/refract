import { act, cleanup, fireEvent, render, screen } from "@testing-library/react";
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import type { CostEstimate, ModelUsage, SystemStats, UsageSummary } from "../backend/types";
import { SystemWidget, layoutBar } from "./SystemWidget";
import { UsageWidget, totalTokens } from "./UsageWidget";

afterEach(cleanup);

const GB = 1024 ** 3;
const STATS: SystemStats = {
  cpuPercent: 23.6,
  memUsedBytes: 13.4 * GB,
  memTotalBytes: 32 * GB,
  gpuPercent: 18,
  wsl: null,
  own: { cpuPercent: 0.6, memBytes: 210 * 1024 ** 2, gpuPercent: 1.5, wslCpuPercent: 0, wslMemBytes: 0 },
};

/** The same machine with WSL's share: 12% CPU, a 5.5 GB VM of which 4 GB is cache. */
const WSL_STATS: SystemStats = {
  ...STATS,
  wsl: { cpuPercent: 12, memBytes: 5.5 * GB, memProgramBytes: 1.5 * GB, memCacheBytes: 4 * GB, gpuPercent: 1 },
  own: { cpuPercent: 3, memBytes: 410 * 1024 ** 2, gpuPercent: 2, wslCpuPercent: 0.4, wslMemBytes: 40 * 1024 ** 2 },
};

const t = (input: number, output: number, cacheRead: number, cacheWrite: number) => ({
  input,
  output,
  cacheRead,
  cacheWrite,
});

const USAGE: UsageSummary = {
  today: t(50_000, 150_000, 3_600_000, 400_000),
  last7Days: t(400_000, 1_200_000, 27_000_000, 2_400_000),
  last5Hours: t(20_000, 80_000, 1_700_000, 100_000),
  liveAgents: 2,
  byRepo: [
    { path: "/home/demo/projects/tidepool", name: "tidepool", last7Days: t(100_000, 400_000, 10_000_000, 1_000_000) },
    { path: "/home/demo/projects/quill", name: "quill", last7Days: t(1_000, 2_000, 900_000, 0) },
    { path: "/home/demo/scratch", name: "scratch", last7Days: t(0, 0, 0, 0) },
  ],
  computedAt: "2026-10-30T12:00:00Z",
  ready: true,
};

describe("SystemWidget", () => {
  it("shows machine load and Refract's share", () => {
    render(<SystemWidget stats={STATS} away={false} />);
    const cpu = screen.getByRole("meter", { name: "CPU" });
    expect(cpu.getAttribute("aria-valuenow")).toBe("24");
    expect(cpu.getAttribute("aria-valuetext")).toBe("24%");
    expect(screen.getByRole("meter", { name: "Memory" }).getAttribute("aria-valuetext")).toBe("13.4 GB");
    expect(screen.getByRole("meter", { name: "GPU" }).getAttribute("aria-valuetext")).toBe("18%");
    expect(screen.getByRole("region", { name: "System" }).textContent).toContain(
      "Refract 0.6% CPU · 210 MB · 1.5% GPU",
    );
  });

  it("looks as before the split with no WSL figures", () => {
    const { container } = render(<SystemWidget stats={STATS} away={false} />);
    expect(container.querySelector(".rf-meter__wsl")).toBeNull();
    expect(container.querySelector(".rf-widget__legend")).toBeNull();
    expect(screen.getByRole("region", { name: "System" }).textContent).not.toContain("WSL");
    expect(container.querySelectorAll(".rf-meter__own")).toHaveLength(3);
  });

  it("splits the bars into WSL and Windows and lists WSL's share", () => {
    const { container } = render(<SystemWidget stats={WSL_STATS} away={false} />);
    expect(screen.getByRole("meter", { name: "CPU" }).getAttribute("aria-valuetext")).toBe("24%, WSL 12%");
    expect(screen.getByRole("meter", { name: "Memory" }).getAttribute("aria-valuetext")).toBe(
      "13.4 GB, WSL 5.5 GB (4 GB cache)",
    );
    expect(screen.getByRole("meter", { name: "GPU" }).getAttribute("aria-valuetext")).toBe("18%, WSL 1%");
    const text = screen.getByRole("region", { name: "System" }).textContent;
    expect(text).toContain("WSLcacheWindowsRefract");
    expect(text).toContain("WSL 12% CPU · 5.5 GB · 1% GPU");
    // Refract's line is both sides together.
    expect(text).toContain("Refract 3% CPU · 410 MB · 2% GPU");
    expect(container.querySelectorAll(".rf-meter__wsl")).toHaveLength(3);
    expect(container.querySelectorAll(".rf-meter__gap")).toHaveLength(3);
    // Only memory has a striped part, after what programs use.
    expect(container.querySelectorAll(".rf-meter__wsl-cache")).toHaveLength(1);
    const memory = screen.getByRole("meter", { name: "Memory" });
    const cache = memory.querySelector<HTMLElement>(".rf-meter__wsl-cache")!;
    expect(parseFloat(cache.style.width)).toBeCloseTo((4 / 32) * 100);
    expect(parseFloat(cache.style.left)).toBeCloseTo((1.5 / 32) * 100);
    // The label sits on its own line whatever the digits; the Refract swatch waits for
    // short windows.
    expect(container.querySelectorAll(".rf-widget__line-label")).toHaveLength(2);
    expect(container.querySelectorAll(".rf-widget__swatch--short")).toHaveLength(1);
  });

  it("leaves GPU whole when WSL has no GPU figure", () => {
    render(<SystemWidget stats={{ ...WSL_STATS, wsl: { ...WSL_STATS.wsl!, gpuPercent: null } }} away={false} />);
    const gpu = screen.getByRole("meter", { name: "GPU" });
    expect(gpu.getAttribute("aria-valuetext")).toBe("18%");
    expect(gpu.querySelector(".rf-meter__wsl")).toBeNull();
    const text = screen.getByRole("region", { name: "System" }).textContent;
    expect(text).toContain("WSL 12% CPU · 5.5 GB");
    expect(text).not.toContain("5.5 GB ·");
  });

  it("lays out a bar WSL first, with Refract's mark at the start of each part", () => {
    expect(layoutBar({ total: 40, own: 5 })).toEqual({ fill: [0, 40], own: [0, 5] });
    // Programs solid from the start; the rest of WSL's part (cache, and anything beyond
    // programs and cache) striped after it; a gap where Windows' part starts.
    expect(layoutBar({ total: 40, own: 5, wsl: 10, wslOwn: 1, wslPrograms: 3 })).toEqual({
      fill: [0, 40],
      wsl: [0, 10],
      wslCache: [3, 7],
      wslOwn: [0, 1],
      own: [10, 4],
      gap: 10,
    });
    // CPU and GPU have no striped part.
    expect(layoutBar({ total: 40, own: 5, wsl: 10 }).wslCache).toBeUndefined();
    // Each piece stays inside its part and the bar; no gap with no Windows part.
    expect(layoutBar({ total: 40, own: 50, wsl: 60, wslOwn: 80, wslPrograms: 90 })).toEqual({
      fill: [0, 40],
      wsl: [0, 40],
      wslCache: [40, 0],
      wslOwn: [0, 40],
      own: [40, 0],
    });
    expect(layoutBar({ total: Number.NaN, own: 1, wsl: 1 })).toEqual({
      fill: [0, 0],
      wsl: [0, 0],
      wslOwn: [0, 0],
      own: [0, 0],
    });
  });

  it("waits for the first reading and hides while away", () => {
    const { container, rerender } = render(<SystemWidget stats={null} away={false} />);
    expect(screen.getByRole("meter", { name: "CPU" }).getAttribute("aria-valuetext")).toBe("–");
    rerender(<SystemWidget stats={STATS} away />);
    const widget = container.querySelector(".rf-widget")!;
    expect(widget.classList).toContain("rf-widget--away");
    expect(widget.getAttribute("aria-hidden")).toBe("true");
  });
});

describe("UsageWidget", () => {
  it("adds up every kind of token", () => {
    expect(totalTokens(t(1, 2, 3, 4))).toBe(10);
  });

  it("shows compact totals and the agents running", () => {
    render(<UsageWidget usage={USAGE} away={false} />);
    const widget = screen.getByRole("region", { name: "Claude usage" });
    expect(widget.textContent).toContain("Today4.2M");
    expect(widget.textContent).toContain("7 days31M");
    expect(widget.textContent).toContain("5 hours1.9M");
    expect(widget.textContent).toContain("2 agents running");
    expect(widget.textContent).not.toContain("by repo");
  });

  it("lists the 7 days by repo on hover and focus, leaving out empty ones", () => {
    render(<UsageWidget usage={USAGE} away={false} />);
    const widget = screen.getByRole("region", { name: "Claude usage" });
    fireEvent.mouseEnter(widget);
    const items = screen.getAllByRole("listitem").map((li) => li.textContent);
    expect(items).toEqual(["tidepool11.5M", "quill903K"]);
    fireEvent.mouseLeave(widget);
    expect(screen.queryAllByRole("listitem")).toEqual([]);
    fireEvent.focus(widget);
    expect(screen.getAllByRole("listitem")).toHaveLength(2);
    fireEvent.blur(widget);
    expect(screen.queryAllByRole("listitem")).toEqual([]);
  });

  it("says when it is still counting and is not focusable while away", () => {
    const { container } = render(<UsageWidget usage={null} away />);
    const widget = container.querySelector(".rf-widget")!;
    expect(widget.textContent).toContain("Counting…");
    expect(widget.getAttribute("tabindex")).toBe("-1");
    fireEvent.mouseEnter(widget);
    expect(screen.queryAllByRole("listitem")).toEqual([]);
  });

  it("shows no numbers until the helper has read the transcripts once", () => {
    const notReady: UsageSummary = { ...USAGE, ready: false };
    const { rerender } = render(<UsageWidget usage={notReady} away={false} />);
    const widget = screen.getByRole("region", { name: "Claude usage" });
    expect(widget.textContent).toContain("Counting…");
    expect(widget.textContent).toContain("Today–");
    expect(widget.textContent).not.toContain("4.2M");
    fireEvent.mouseEnter(widget);
    expect(screen.queryAllByRole("listitem")).toEqual([]);
    rerender(<UsageWidget usage={USAGE} away={false} />);
    expect(widget.textContent).toContain("Today4.2M");
    expect(widget.textContent).toContain("2 agents running");
  });

  const m = (model: string, input: number, cacheWrite = 0, cacheWrite1h = 0): ModelUsage => ({
    model,
    ...t(input, 0, 0, cacheWrite),
    cacheWrite1h,
  });
  const COST: CostEstimate = {
    enabled: true,
    prices: [
      { match: "opus", input: 5, output: 25, cacheWrite5m: 6.25, cacheWrite1h: 10, cacheRead: 0.5 },
      { match: "sonnet", input: 2, output: 10, cacheWrite5m: 2.5, cacheWrite1h: 4, cacheRead: 0.2 },
    ],
  };
  const WITH_MODELS: UsageSummary = {
    ...USAGE,
    byModel: {
      // $5 + $2
      today: [m("claude-opus-5", 1_000_000), m("claude-sonnet-5-5", 1_000_000)],
      // $500 + 1M 5-minute writes at $6.25 + 1M 1-hour at $10
      last7Days: [m("claude-opus-5", 100_000_000, 2_000_000, 1_000_000), m("claude-sonnet-5-5", 1_000_000)],
      last5Hours: [],
    },
  };

  it("estimates each window's cost when the estimate is on", () => {
    render(<UsageWidget usage={WITH_MODELS} away={false} costEstimate={COST} />);
    const widget = screen.getByRole("region", { name: "Claude usage" });
    expect(widget.textContent).toContain("Today≈ $7.004.2M");
    expect(widget.textContent).toContain("7 days≈ $51831M");
    expect(widget.textContent).toContain("5 hours≈ $0.001.9M");
    expect(widget.textContent).toContain("≈ estimated at API list prices");
    expect(screen.getAllByTitle(/Estimated at API list prices/).length).toBeGreaterThan(0);
  });

  it("notes tokens of models with no price", () => {
    const usage: UsageSummary = {
      ...WITH_MODELS,
      byModel: { ...WITH_MODELS.byModel!, last7Days: [m("claude-opus-5", 1_000_000), m("", 12_000)] },
    };
    render(<UsageWidget usage={usage} away={false} costEstimate={COST} />);
    const widget = screen.getByRole("region", { name: "Claude usage" });
    expect(widget.textContent).toContain("12K tokens unpriced");
    expect(screen.getByTitle("No price for (no model)")).toBeTruthy();
    // The ids are read out too, not only shown on hover.
    expect(widget.textContent).toContain("(no price for (no model))");
  });

  it("shows no cost when the estimate is off or the helper sends no split", () => {
    const { rerender } = render(
      <UsageWidget usage={WITH_MODELS} away={false} costEstimate={{ ...COST, enabled: false }} />,
    );
    const widget = screen.getByRole("region", { name: "Claude usage" });
    expect(widget.textContent).not.toContain("$");
    expect(widget.textContent).not.toContain("newer helper");
    rerender(<UsageWidget usage={USAGE} away={false} costEstimate={COST} />);
    expect(widget.textContent).not.toContain("$");
    expect(widget.textContent).toContain("Today4.2M");
    // On, but the helper sends no split: say so rather than show nothing.
    expect(widget.textContent).toContain("Cost needs a newer helper");
  });
});

describe("UsageWidget plan limits", () => {
  // Local times, so the clock text holds in any time zone.
  const NOW = new Date(2026, 9, 30, 14, 10);
  const local = (day: number, hour: number, minute = 0) => new Date(2026, 9, day, hour, minute).toISOString();
  const WITH_LIMITS: UsageSummary = {
    ...USAGE,
    computedAt: NOW.toISOString(),
    limits: {
      fiveHour: { usedPercent: 2, resetsAt: local(30, 15, 30) },
      sevenDay: { usedPercent: 94.6, resetsAt: local(33, 3) },
      updatedAt: local(30, 14, 9),
    },
  };

  beforeEach(() => {
    vi.useFakeTimers({ toFake: ["Date", "setTimeout", "clearTimeout"] });
    vi.setSystemTime(NOW);
  });
  afterEach(() => {
    vi.useRealTimers();
  });

  it("shows each window as a meter with its reset time and says where it comes from", () => {
    const { container } = render(<UsageWidget usage={WITH_LIMITS} away={false} />);
    const five = screen.getByRole("meter", { name: "5-hour limit" });
    expect(five.getAttribute("aria-valuenow")).toBe("2");
    expect(five.getAttribute("aria-valuetext")).toBe("2% used, resets 15:30");
    const week = screen.getByRole("meter", { name: "Weekly limit" });
    expect(week.getAttribute("aria-valuetext")).toBe("95% used, resets Mon 03:00");
    expect(week.querySelector(".rf-limit__fill--high")).not.toBeNull();
    const widget = screen.getByRole("region", { name: "Claude usage" });
    expect(widget.textContent).toContain("5-hour limit2%· resets 15:30");
    expect(widget.textContent).toContain("From Claude Code's status line, via the helper's limits file");
    expect(container.querySelector(".rf-limits")!.getAttribute("title")).toContain("status line");
    // Below the token rows and above the agents.
    const order = [...widget.children].map((el) => el.className);
    expect(order.indexOf("rf-limits")).toBeGreaterThan(order.indexOf("rf-usage__totals"));
    expect(order.indexOf("rf-limits")).toBeLessThan(order.findIndex((c) => c.startsWith("rf-usage__agents")));
  });

  it("shows a window that reset since the reading without its old percentage", () => {
    const usage: UsageSummary = {
      ...WITH_LIMITS,
      limits: { ...WITH_LIMITS.limits!, fiveHour: { usedPercent: 81, resetsAt: local(30, 14, 0) } },
    };
    render(<UsageWidget usage={usage} away={false} />);
    const widget = screen.getByRole("region", { name: "Claude usage" });
    expect(widget.textContent).toContain("5-hour limitreset since last reading");
    expect(widget.textContent).not.toContain("81%");
    expect(screen.queryByRole("meter", { name: "5-hour limit" })).toBeNull();
    expect(screen.getByRole("meter", { name: "Weekly limit" })).toBeTruthy();
  });

  it("marks an old reading with its time", () => {
    const usage: UsageSummary = { ...WITH_LIMITS, limits: { ...WITH_LIMITS.limits!, updatedAt: local(30, 13, 2) } };
    render(<UsageWidget usage={usage} away={false} />);
    const widget = screen.getByRole("region", { name: "Claude usage" });
    expect(widget.textContent).toContain("Limits as of 13:02");
    expect(screen.getByRole("meter", { name: "5-hour limit" }).getAttribute("aria-valuetext")).toBe(
      "2% used, resets 15:30",
    );
  });

  it("turns amber from a rounded 90%", () => {
    const at = (usedPercent: number): UsageSummary => ({
      ...WITH_LIMITS,
      limits: { ...WITH_LIMITS.limits!, fiveHour: { usedPercent, resetsAt: local(30, 15, 30) } },
    });
    const { rerender } = render(<UsageWidget usage={at(89.4)} away={false} />);
    const five = () => screen.getByRole("meter", { name: "5-hour limit" });
    expect(five().querySelector(".rf-limit__fill--high")).toBeNull();
    rerender(<UsageWidget usage={at(89.5)} away={false} />);
    expect(five().getAttribute("aria-valuetext")).toBe("90% used, resets 15:30");
    expect(five().querySelector(".rf-limit__fill--high")).not.toBeNull();
  });

  it("follows resets and staleness with no new answer, as when the helper stops", () => {
    const usage: UsageSummary = {
      ...WITH_LIMITS,
      limits: { ...WITH_LIMITS.limits!, fiveHour: { usedPercent: 40, resetsAt: local(30, 14, 12) } },
    };
    const { unmount } = render(<UsageWidget usage={usage} away={false} />);
    const widget = screen.getByRole("region", { name: "Claude usage" });
    expect(widget.textContent).toContain("resets 14:12");
    act(() => vi.advanceTimersByTime(2 * 60_000));
    expect(widget.textContent).toContain("5-hour limitreset since last reading");
    expect(widget.textContent).not.toContain("40%");
    // The reading was taken at 14:09; at 14:24 it is stale.
    expect(widget.textContent).not.toContain("Limits as of");
    act(() => vi.advanceTimersByTime(13 * 60_000));
    expect(widget.textContent).toContain("Limits as of 14:09");
    unmount();
    expect(vi.getTimerCount()).toBe(0);
  });

  it("shows nothing for limits without a reading or a window", () => {
    const { container, rerender } = render(<UsageWidget usage={USAGE} away={false} />);
    expect(container.querySelector(".rf-limits")).toBeNull();
    rerender(<UsageWidget usage={{ ...USAGE, limits: { updatedAt: local(30, 14, 9) } }} away={false} />);
    expect(container.querySelector(".rf-limits")).toBeNull();
    expect(screen.queryAllByRole("meter")).toEqual([]);
  });
});
