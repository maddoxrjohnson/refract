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

describe("App's Codex rows", () => {
  const ORCHARD = "/home/demo/projects/orchard-db";
  const IDLE = "5f0c2a1e-0000-4000-8000-0000000000e6";

  async function pickStopIdle(backend: TestBackend) {
    render(<App backend={backend} />);
    await act(() => new Promise((r) => setTimeout(r, 50)));
    for (const c of "orchard") await key(c);
    await key("Enter");
    await screen.findByRole("complementary", { name: "orchard-db details" });
    await key("Enter");
    const picker = await screen.findByRole("dialog", { name: "Start an agent in orchard-db" });
    const row = [...picker.querySelectorAll('[role="option"]')].find((o) =>
      o.textContent?.includes("Stop the idle Claude agent and start Codex"),
    )!;
    fireEvent.click(row);
  }

  it("stops the idle Claude agent, then starts Codex", async () => {
    const backend = new TestBackend();
    const order: string[] = [];
    const stop = vi.spyOn(backend, "migrateStopAgent").mockImplementation(async () => void order.push("stop"));
    const launch = vi.spyOn(backend, "launch").mockImplementation(async () => {
      order.push("launch");
      return { requestId: "r", sessionId: "s", folderUri: "vscode-remote://wsl+Demo/x" };
    });
    await pickStopIdle(backend);
    await waitFor(() => expect(launch).toHaveBeenCalledWith(ORCHARD, { kind: "new", agent: "codex" }));
    expect(stop).toHaveBeenCalledWith(ORCHARD, IDLE, true);
    expect(order).toEqual(["stop", "launch"]);
  });

  it("starts nothing and says why when the agent got busy", async () => {
    const backend = new TestBackend();
    vi.spyOn(backend, "migrateStopAgent").mockRejectedValue({ kind: "busy", message: "The agent is working again." });
    const launch = vi.spyOn(backend, "launch");
    await pickStopIdle(backend);
    await screen.findByText(/The agent is working again\./);
    expect(launch).not.toHaveBeenCalled();
  });

  it("says which sessions it stopped when a later stop is refused, and starts nothing", async () => {
    const backend = new TestBackend();
    const OTHER = "7c1d93b4-0000-4000-8000-0000000000f1";
    const orchard = (await backend.listRepos()).repos.find((r) => r.path === ORCHARD)!;
    const stop = vi
      .spyOn(backend, "migrateStopAgent")
      .mockResolvedValueOnce(undefined)
      .mockRejectedValueOnce({ kind: "busy", message: "The agent is working again." });
    const launch = vi.spyOn(backend, "launch");
    render(<App backend={backend} />);
    await act(() => new Promise((r) => setTimeout(r, 50)));
    act(() =>
      backend.send({
        event: "repoChanged",
        data: { ...orchard, codex: { ...orchard.codex!, gates: { codex: { kind: "stopIdle", sessionIds: [IDLE, OTHER] } } } },
      }),
    );
    for (const c of "orchard") await key(c);
    await key("Enter");
    await screen.findByRole("complementary", { name: "orchard-db details" });
    await key("Enter");
    const picker = await screen.findByRole("dialog", { name: "Start an agent in orchard-db" });
    fireEvent.click(
      [...picker.querySelectorAll('[role="option"]')].find((o) =>
        o.textContent?.includes("Stop the idle Claude agent and start Codex"),
      )!,
    );
    await screen.findByText(
      /Stopped the idle Claude session 5f0c2a1e, but not the next one: The agent is working again\. Nothing was started\./,
    );
    expect(stop).toHaveBeenCalledTimes(2);
    expect(launch).not.toHaveBeenCalled();
  });

  it("shows the helper's refusal of a busy agent in the launch toast", async () => {
    const backend = await zoomDotfiles();
    const message = "Codex is working in this repo. Wait for it to finish, or close it first.";
    vi.spyOn(backend, "launch").mockRejectedValue({ kind: "agent", message });
    fireEvent.click(screen.getByRole("button", { name: "New agent" }));
    expect((await screen.findByRole("alert")).textContent).toContain(message);
  });

  it("shows a refused Make Codex-ready in the launch toast", async () => {
    const backend = new TestBackend();
    const message = "Claude is working in this repo. Wait for it to finish, or close it first.";
    vi.spyOn(backend, "makeCodexReady").mockRejectedValue({ kind: "agent", message });
    render(<App backend={backend} />);
    await act(() => new Promise((r) => setTimeout(r, 50)));
    for (const c of "kestrel") await key(c);
    await key("Enter");
    await screen.findByRole("complementary", { name: "kestrel-cli details" });
    fireEvent.click(screen.getByRole("button", { name: "Make Codex-ready" }));
    expect((await screen.findByRole("alert")).textContent).toContain(message);
  });
});
