import { cleanup, fireEvent, render, screen } from "@testing-library/react";
import { afterEach, describe, expect, it } from "vitest";
import type { SystemStats, UsageSummary } from "../backend/types";
import { SystemWidget } from "./SystemWidget";
import { UsageWidget, totalTokens } from "./UsageWidget";

afterEach(cleanup);

const GB = 1024 ** 3;
const STATS: SystemStats = {
  cpuPercent: 23.6,
  memUsedBytes: 13.4 * GB,
  memTotalBytes: 32 * GB,
  gpuPercent: 18,
  own: { cpuPercent: 0.6, memBytes: 210 * 1024 ** 2, gpuPercent: 1.5 },
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
});
