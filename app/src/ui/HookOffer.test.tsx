import { act, cleanup, fireEvent, render, screen } from "@testing-library/react";
import { afterEach, describe, expect, it, vi } from "vitest";
import type { HookInfo } from "../backend/types";
import type { HookControls } from "../hooks/useHook";
import { HookOffer, HookSection, loadHookOfferDismissed, saveHookOfferDismissed } from "./HookOffer";

afterEach(() => {
  cleanup();
  vi.restoreAllMocks();
  localStorage.clear();
});

const INFO: HookInfo = {
  installed: false,
  outdated: false,
  settingsPath: "/home/demo/.claude/settings.json",
  diff: '--- a\n+++ b\n {\n-  "theme": "dark"\n+  "theme": "dark",\n+  "hooks": {}\n }\n',
};

function controls(extra: Partial<HookControls> = {}): HookControls {
  return { info: INFO, error: null, install: vi.fn().mockResolvedValue(undefined), remove: vi.fn().mockResolvedValue(undefined), ...extra };
}

describe("HookOffer", () => {
  it("explains the hook and shows the diff on request", () => {
    render(<HookOffer hook={controls()} onDismiss={() => {}} />);
    const card = screen.getByRole("region", { name: "See when agents are working" });
    expect(card.textContent).toContain("costs no tokens");
    expect(card.querySelector(".rf-diff")).toBeNull();
    fireEvent.click(screen.getByRole("button", { name: "Show what changes" }));
    const diff = screen.getByLabelText("What installing changes");
    expect(diff.textContent).toBe(INFO.diff + "\n");
    expect(diff.querySelectorAll(".rf-diff__add")).toHaveLength(2);
    expect(diff.querySelectorAll(".rf-diff__del")).toHaveLength(1);
    expect(card.textContent).toContain(INFO.settingsPath);
  });

  it("installs, then goes away", async () => {
    const hook = controls();
    const onDismiss = vi.fn();
    render(<HookOffer hook={hook} onDismiss={onDismiss} />);
    await act(async () => fireEvent.click(screen.getByRole("button", { name: "Install" })));
    expect(hook.install).toHaveBeenCalled();
    expect(onDismiss).toHaveBeenCalled();
  });

  it("offers an update for an outdated install", () => {
    render(<HookOffer hook={controls({ info: { ...INFO, outdated: true } })} onDismiss={() => {}} />);
    expect(screen.getByRole("button", { name: "Update" })).toBeTruthy();
    expect(screen.queryByRole("button", { name: "Install" })).toBeNull();
  });

  it("shows a failed install inline and stays", async () => {
    const hook = controls({ install: vi.fn().mockRejectedValue({ kind: "agent", code: -32603, message: "settings.json is not valid JSON" }) });
    const onDismiss = vi.fn();
    render(<HookOffer hook={hook} onDismiss={onDismiss} />);
    await act(async () => fireEvent.click(screen.getByRole("button", { name: "Install" })));
    expect(screen.getByRole("alert").textContent).toBe("settings.json is not valid JSON (-32603)");
    expect(onDismiss).not.toHaveBeenCalled();
    fireEvent.click(screen.getByRole("button", { name: "Not now" }));
    expect(onDismiss).toHaveBeenCalled();
  });

  it("remembers Not now, and copes without storage", () => {
    expect(loadHookOfferDismissed()).toBe(false);
    saveHookOfferDismissed();
    expect(loadHookOfferDismissed()).toBe(true);
    vi.spyOn(Storage.prototype, "getItem").mockImplementation(() => {
      throw new Error("blocked");
    });
    vi.spyOn(Storage.prototype, "setItem").mockImplementation(() => {
      throw new Error("blocked");
    });
    expect(loadHookOfferDismissed()).toBe(false);
    expect(() => saveHookOfferDismissed()).not.toThrow();
  });
});

describe("HookSection", () => {
  it("installs when not installed", async () => {
    const hook = controls();
    render(<HookSection hook={hook} labelId="h" />);
    expect(screen.getByRole("region", { name: "Status hook" }).textContent).toContain("State: Not installed");
    await act(async () => fireEvent.click(screen.getByRole("button", { name: "Install" })));
    expect(hook.install).toHaveBeenCalled();
    expect(screen.getByRole("status").textContent).toBe("Installed.");
  });

  it("removes when installed, showing errors inline", async () => {
    const hook = controls({
      info: { ...INFO, installed: true, diff: null },
      remove: vi.fn().mockRejectedValue({ kind: "notRunning" }),
    });
    render(<HookSection hook={hook} labelId="h" />);
    expect(screen.queryByRole("button", { name: "Show what changes" })).toBeNull();
    await act(async () => fireEvent.click(screen.getByRole("button", { name: "Remove" })));
    expect(screen.getByRole("alert").textContent).toBe("notRunning");
  });

  it("updates or removes an outdated install", async () => {
    const hook = controls({ info: { ...INFO, outdated: true } });
    render(<HookSection hook={hook} labelId="h" />);
    const section = screen.getByRole("region", { name: "Status hook" });
    expect(section.textContent).toContain("State: Needs update");
    expect(section.textContent).toContain("sessions started after the update");
    expect(screen.queryByRole("button", { name: "Install" })).toBeNull();
    expect(screen.getByRole("button", { name: "Show what changes" })).toBeTruthy();
    expect(screen.getByRole("button", { name: "Remove" })).toBeTruthy();
    await act(async () => fireEvent.click(screen.getByRole("button", { name: "Update" })));
    expect(hook.install).toHaveBeenCalled();
    expect(hook.remove).not.toHaveBeenCalled();
    expect(screen.getByRole("status").textContent).toBe("Updated.");
  });

  it("says only the script is missing when the settings already have the hook", () => {
    render(<HookSection hook={controls({ info: { ...INFO, outdated: true, diff: "" } })} labelId="h" />);
    expect(screen.getByRole("region", { name: "Status hook" }).textContent).toContain("updating restores its script");
  });

  it("says when the state is unknown", () => {
    render(<HookSection hook={controls({ info: null, error: "unknown method \"hookInfo\" (-32601)" })} labelId="h" />);
    const section = screen.getByRole("region", { name: "Status hook" });
    expect(section.textContent).toContain("State: Unknown");
    expect(screen.getByRole("alert").textContent).toContain("hookInfo");
    expect(screen.queryByRole("button")).toBeNull();
  });
});
