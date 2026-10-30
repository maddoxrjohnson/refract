import { cleanup, fireEvent, render, screen } from "@testing-library/react";
import { afterEach, describe, expect, it, vi } from "vitest";
import { DIRTY_REPO_PROMPT } from "../repoStatus";
import { DetailPanel, type DetailPanelProps } from "./DetailPanel";
import { NOW, testRepo, testSession } from "./testRepo";

afterEach(cleanup);

function setup(repo = testRepo()) {
  const props: DetailPanelProps = {
    repo,
    now: NOW,
    onPrimary: vi.fn(),
    onLaunch: vi.fn(),
    onOpenFolder: vi.fn(),
    onOpenUrl: vi.fn(),
    onClose: vi.fn(),
  };
  const view = render(<DetailPanel {...props} />);
  return { props, panel: screen.getByRole("complementary", { name: `${repo.name} details` }), view };
}

describe("DetailPanel", () => {
  it("shows branch, upstream, changes and the last commit with its age", () => {
    const { panel } = setup(testRepo({ ahead: 2, behind: 1, changedFiles: 3 }));
    const text = panel.textContent ?? "";
    expect(text).toContain("main");
    expect(text).toContain("2 ahead, 1 behind");
    expect(text).toContain("3 changed files");
    expect(text).toContain("Split the sync worker into stages");
    expect(text).toContain("2 hours ago");
    expect(text).toContain("No agent sessions yet");
  });

  it("says when there is no upstream or the branch is detached", () => {
    const { panel } = setup(testRepo({ ahead: null, behind: null, branch: null, remote: null }));
    expect(panel.textContent).toContain("No upstream");
    expect(panel.textContent).toContain("detached HEAD");
    expect(screen.queryByRole("button", { name: /Open on/ })).toBeNull();
  });

  it("shows the newest session: when, whether it finished, its first line", () => {
    const { panel } = setup(
      testRepo({
        sessions: [
          testSession("old", { kind: "finished", detail: "completed" }, 30, "Old news"),
          testSession("new", { kind: "interrupted", detail: "apiError" }, 5, "\nRunning the migration.\nMore"),
        ],
      }),
    );
    expect(panel.textContent).toContain("Didn't finish · 5 hours ago");
    expect(panel.textContent).toContain("Running the migration.");
    expect(panel.textContent).not.toContain("More");
    expect(panel.textContent).not.toContain("Old news");
  });

  it("for a clean repo the primary action starts a new agent on Enter", () => {
    const { props } = setup();
    const primary = screen.getByRole("button", { name: /New agent/ });
    expect(primary.getAttribute("aria-describedby")).toBeTruthy();
    expect(document.getElementById(primary.getAttribute("aria-describedby")!)?.textContent).toMatch(
      /Enter starts a new agent/,
    );
    expect(screen.getAllByRole("button", { name: /New agent/ })).toHaveLength(1);
    expect(screen.queryByRole("button", { name: "Resume" })).toBeNull();
    fireEvent.click(primary);
    expect(props.onPrimary).toHaveBeenCalledOnce();
  });

  it("with unfinished work offers the picker on Enter plus direct Resume and New agent", () => {
    const repo = testRepo({
      changedFiles: 2,
      sessions: [testSession("s1", { kind: "interrupted", detail: "apiError" }, 3, "Hi")],
      preselect: { kind: "resume", sessionId: "s1" },
    });
    const { props } = setup(repo);
    fireEvent.click(screen.getByRole("button", { name: /Launch/ }));
    expect(props.onPrimary).toHaveBeenCalledOnce();
    fireEvent.click(screen.getByRole("button", { name: "Resume" }));
    expect(props.onLaunch).toHaveBeenLastCalledWith({ kind: "resume", sessionId: "s1" });
    fireEvent.click(screen.getByRole("button", { name: "New agent" }));
    expect(props.onLaunch).toHaveBeenLastCalledWith({ kind: "new", prompt: DIRTY_REPO_PROMPT });
  });

  it("opens the folder, the remote and closes", () => {
    const { props } = setup();
    fireEvent.click(screen.getByRole("button", { name: "Open folder" }));
    expect(props.onOpenFolder).toHaveBeenCalledOnce();
    fireEvent.click(screen.getByRole("button", { name: "Open on GitHub" }));
    expect(props.onOpenUrl).toHaveBeenCalledWith("https://github.com/demo-user/tidepool");
    fireEvent.click(screen.getByRole("button", { name: /Close details/ }));
    expect(props.onClose).toHaveBeenCalledOnce();
  });
});
