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

describe("App's worktrees", () => {
  const MAIN = "/home/demo/projects/harbor-android";

  async function zoomByWorktree() {
    const backend = new TestBackend();
    render(<App backend={backend} />);
    await act(() => new Promise((r) => setTimeout(r, 50)));
    // Search finds the main gem by a worktree's name.
    for (const c of "wide-cards") await key(c);
    await key("Enter");
    const panel = await screen.findByRole("complementary", { name: "harbor-android details" });
    return { backend, panel };
  }

  it("folds them into the main gem's panel and opens one as Enter would", async () => {
    const { panel } = await zoomByWorktree();
    const section = panel.querySelector('[aria-label="Worktrees"]')!;
    expect(section.textContent).toContain("harbor-android-wide-cards");
    expect(section.textContent).toContain("In v1.4.0 · Ready to remove");
    expect(section.textContent).toContain("3 uncommitted changes");
    fireEvent.click(screen.getByRole("button", { name: "Open harbor-android-wide-cards" }));
    // It has a session to resume, so the picker opens for the worktree.
    await screen.findByRole("dialog", { name: "Start an agent in harbor-android-wide-cards" });
  });

  it("keeps folded snapshots current through sessionChanged", async () => {
    const { backend, panel } = await zoomByWorktree();
    const search = (await backend.listRepos()).repos.find((r) => r.path === `${MAIN}-search`)!;
    act(() =>
      backend.send({
        event: "sessionChanged",
        data: {
          repoPath: search.path,
          session: { ...search.sessions[0], state: { kind: "live", detail: "waitingOnUser" }, resumeReason: null },
          preselect: search.preselect,
        },
      }),
    );
    await waitFor(() => expect(panel.textContent).toContain("Agent waiting for you"));
    // The main repo's status line counts it too.
    expect(panel.querySelector(".rf-detail__header .rf-state")?.textContent).toContain("Waiting for you");
  });

  it("removes the ready one, which leaves the panel", async () => {
    const { panel } = await zoomByWorktree();
    fireEvent.click(screen.getByRole("button", { name: "Remove harbor-android-wide-cards…" }));
    fireEvent.click(screen.getByRole("button", { name: "Remove" }));
    await waitFor(() => expect(panel.textContent).not.toContain("harbor-android-wide-cards"), { timeout: 3000 });
    expect(panel.textContent).toContain("harbor-android-search");
  });
});
