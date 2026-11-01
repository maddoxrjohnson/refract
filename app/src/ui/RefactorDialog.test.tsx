import { act, cleanup, fireEvent, render, screen } from "@testing-library/react";
import { afterEach, describe, expect, it, vi } from "vitest";
import type { HookControls } from "../hooks/useHook";
import { RefactorDialog } from "./RefactorDialog";
import { testRepo, testSession } from "./testRepo";

afterEach(cleanup);

function hook(installed: boolean, install = vi.fn(() => Promise.resolve())): HookControls {
  return {
    info: { installed, outdated: false, settingsPath: "/home/dev/.claude/settings.json", diff: installed ? null : "" },
    error: null,
    install,
    remove: vi.fn(),
  };
}

function setup(repo = testRepo(), controls = hook(true)) {
  const onConfirm = vi.fn();
  const onClose = vi.fn();
  render(<RefactorDialog repo={repo} hook={controls} onConfirm={onConfirm} onClose={onClose} />);
  return { onConfirm, onClose, dialog: screen.getByRole("dialog", { name: "Refactor tidepool" }) };
}

describe("RefactorDialog", () => {
  it("starts with the focus line, trimmed, on Enter", () => {
    const { onConfirm } = setup();
    const input = screen.getByRole("textbox", { name: /Focus on/ });
    expect(document.activeElement).toBe(input);
    fireEvent.change(input, { target: { value: "  the sync worker " } });
    fireEvent.submit(input);
    expect(onConfirm).toHaveBeenCalledWith("the sync worker");
  });

  it("starts without a focus from the button", () => {
    const { onConfirm } = setup();
    fireEvent.click(screen.getByRole("button", { name: /Start refactor/ }));
    expect(onConfirm).toHaveBeenCalledWith("");
  });

  it("closes on Escape and Cancel", () => {
    const { onClose, dialog } = setup();
    fireEvent.keyDown(dialog, { key: "Escape" });
    fireEvent.click(screen.getByRole("button", { name: /Cancel/ }));
    expect(onClose).toHaveBeenCalledTimes(2);
  });

  it("has no note for a clean, quiet repo, and no hook nudge with the hook installed", () => {
    const { dialog } = setup();
    expect(dialog.getAttribute("aria-describedby")).toBeNull();
    expect(screen.queryByRole("button", { name: /hook/ })).toBeNull();
  });

  it("notes the uncommitted files and the running agent", () => {
    const { dialog } = setup(
      testRepo({
        changedFiles: 3,
        lastCommit: { summary: "x", time: 0, sha: "a1b2c3d" },
        sessions: [testSession("s", { kind: "live", detail: "working" }, 0)],
      }),
    );
    expect(dialog.textContent).toContain("Starts from a1b2c3d. Your 3 uncommitted files aren't included.");
    expect(dialog.textContent).toContain("The agent running here");
  });

  it("says alerts need the hook and installs it", async () => {
    const install = vi.fn(() => Promise.resolve());
    const { dialog } = setup(testRepo(), hook(false, install));
    expect(dialog.textContent).toContain("need the status hook");
    fireEvent.click(screen.getByRole("button", { name: "Install the hook" }));
    await act(async () => {});
    expect(install).toHaveBeenCalled();
  });

  it("shows why an install failed", async () => {
    setup(testRepo(), hook(false, vi.fn(() => Promise.reject({ kind: "agent", code: -32603, message: "denied" }))));
    fireEvent.click(screen.getByRole("button", { name: "Install the hook" }));
    await act(async () => {});
    expect(screen.getByRole("alert").textContent).toBe("denied (-32603)");
  });
});
