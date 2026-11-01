import { act, cleanup, fireEvent, render, screen, waitFor } from "@testing-library/react";
import { afterEach, beforeAll, describe, expect, it, vi } from "vitest";
import { App } from "./App";
import { DemoBackend } from "./backend/DemoBackend";
import type { AgentEvent } from "./backend/types";

// The scene needs WebGL; these tests are about the overlays.
vi.mock("./scene/Scene", () => ({ Scene: () => null }));

beforeAll(() => {
  window.matchMedia ??= ((query: string) => ({
    matches: false,
    media: query,
    addEventListener: () => {},
    removeEventListener: () => {},
  })) as unknown as typeof window.matchMedia;
});

afterEach(cleanup);

const DOTFILES = "/home/demo/projects/dotfiles";

/** A demo backend whose events a test can send. */
class TestBackend extends DemoBackend {
  send(event: AgentEvent) {
    (this as unknown as { emit(e: AgentEvent): void }).emit(event);
  }
}

const key = (k: string, extra: Partial<KeyboardEventInit> = {}) =>
  act(() => {
    fireEvent.keyDown(document.activeElement ?? document.body, { key: k, ...extra });
  });

async function zoomDotfiles() {
  const backend = new TestBackend();
  render(<App backend={backend} />);
  // The repos and the helper's status arrive.
  await act(() => new Promise((r) => setTimeout(r, 50)));
  for (const c of "dotfiles") await key(c);
  await key("Enter");
  await screen.findByRole("complementary", { name: "dotfiles details" });
  return backend;
}

const openDialog = async () => {
  await key("r", { code: "KeyR", altKey: true });
  return screen.findByRole("dialog", { name: "Refactor dotfiles" });
};

describe("App's Refactor dialog", () => {
  it("opens with Alt+R and closes with Escape even when focus is outside it", async () => {
    await zoomDotfiles();
    await openDialog();
    act(() => (document.activeElement as HTMLElement | null)?.blur());
    await key("Escape");
    expect(screen.queryByRole("dialog", { name: "Refactor dotfiles" })).toBeNull();
    // The panel stays; the next Escape is the scene's again.
    expect(screen.getByRole("complementary", { name: "dotfiles details" })).toBeTruthy();
  });

  it("closes when its repo goes away, and hands the keys back", async () => {
    const backend = await zoomDotfiles();
    await openDialog();
    act(() => backend.send({ event: "repoRemoved", data: { path: DOTFILES } }));
    await waitFor(() => expect(screen.queryByRole("dialog", { name: "Refactor dotfiles" })).toBeNull());
    for (const c of "quill") await key(c);
    expect(document.querySelector(".rf-filter")?.textContent).toContain("quill");
  });

  it("waits to start while another launch runs", async () => {
    await zoomDotfiles();
    // dotfiles has no sessions, so Enter launches a new agent straight away.
    await key("Enter");
    const dialog = await openDialog();
    const start = dialog.querySelector<HTMLButtonElement>('button[type="submit"]')!;
    expect(start.disabled).toBe(true);
    await waitFor(() => expect(start.disabled).toBe(false), { timeout: 4000 });
  });
});
