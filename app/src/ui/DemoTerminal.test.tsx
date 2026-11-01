import { act, cleanup, fireEvent, render, screen } from "@testing-library/react";
import { afterEach, describe, expect, it, vi } from "vitest";
import type { Backend, DemoLaunch } from "../backend/Backend";
import { demoCommand, DemoTerminal } from "./DemoTerminal";

afterEach(() => {
  cleanup();
  vi.useRealTimers();
});

const ID = "5f0c2a1e-0000-4000-8000-0000000000aa";
const launch = (mode: DemoLaunch["mode"]): DemoLaunch => ({
  repoName: "atlas-api",
  path: "/home/demo/projects/atlas-api",
  mode,
  sessionId: ID,
});

/** A backend with only the demo launch hook; the terminal asks for nothing else. */
function demoBackend() {
  let listener: ((l: DemoLaunch) => void) | null = null;
  const backend = {
    onDemoLaunch: (cb: (l: DemoLaunch) => void) => {
      listener = cb;
      return () => (listener = null);
    },
  } as unknown as Backend;
  return { backend, fire: (l: DemoLaunch) => act(() => listener?.(l)) };
}

describe("demoCommand", () => {
  it("starts a new session, resumes one, and quotes a prompt for the shell", () => {
    expect(demoCommand(launch({ kind: "new" }))).toBe(`claude --session-id ${ID}`);
    expect(demoCommand(launch({ kind: "resume", sessionId: ID }))).toBe(`claude --resume ${ID}`);
    expect(demoCommand(launch({ kind: "new", prompt: "it's dirty" }))).toBe(
      `claude --session-id ${ID} 'it'\\''s dirty'`,
    );
    expect(demoCommand(launch({ kind: "focus", sessionId: ID }))).toBe("");
    expect(demoCommand(launch({ kind: "new", prompt: "Refactor", planMode: true }))).toBe(
      `claude --session-id ${ID} --permission-mode plan 'Refactor'`,
    );
    expect(demoCommand(launch({ kind: "resume", sessionId: ID, prompt: "Go on" }))).toBe(`claude --resume ${ID} 'Go on'`);
  });
});

describe("DemoTerminal", () => {
  it("shows nothing for a backend without demo launches", () => {
    const { container } = render(<DemoTerminal backend={{} as Backend} />);
    expect(container.innerHTML).toBe("");
  });

  it("types the command after a demo launch and closes on Escape", () => {
    vi.useFakeTimers();
    const { backend, fire } = demoBackend();
    render(<DemoTerminal backend={backend} />);
    fire(launch({ kind: "new" }));
    const dialog = screen.getByRole("dialog", { name: "Simulated terminal for atlas-api" });
    expect(dialog.textContent).toContain("Demo: nothing is launched");
    // Each character is its own timer, set after the last one renders.
    for (let i = 0; i < 200; i++) act(() => vi.advanceTimersByTime(50));
    expect(screen.getByTestId("demo-terminal").textContent).toContain(`claude --session-id ${ID}`);
    expect(screen.getByTestId("demo-terminal").textContent).toContain("What should we work on?");

    // Keys stop at the terminal, so the scene's own listener never sees them.
    const scene = vi.fn();
    window.addEventListener("keydown", scene);
    fireEvent.keyDown(window, { key: "a" });
    fireEvent.keyDown(window, { key: "Escape" });
    window.removeEventListener("keydown", scene);
    expect(scene).not.toHaveBeenCalled();
    expect(screen.queryByRole("dialog")).toBeNull();
  });

  it("going to a running agent types no command, only a note that nothing started", () => {
    vi.useFakeTimers();
    const { backend, fire } = demoBackend();
    render(<DemoTerminal backend={backend} />);
    fire(launch({ kind: "focus", sessionId: ID }));
    for (let i = 0; i < 20; i++) act(() => vi.advanceTimersByTime(600));
    const screenText = screen.getByTestId("demo-terminal").textContent ?? "";
    expect(screenText).toContain("Switched to the agent already running in atlas-api.");
    expect(screenText).not.toContain("demo@refract");
    expect(screenText).not.toContain("claude --");
  });
});
