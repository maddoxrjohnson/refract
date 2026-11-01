import { act, cleanup, fireEvent, render, screen, within } from "@testing-library/react";
import { afterEach, describe, expect, it, vi } from "vitest";
import type { RepoSnapshot } from "../backend/types";
import { NOW, testRefactor, testSession, testWorktree } from "./testRepo";
import { WorktreesSection, type WorktreesSectionProps } from "./WorktreesSection";

afterEach(cleanup);

function setup(worktrees: RepoSnapshot[], extra: Partial<WorktreesSectionProps> = {}) {
  const props: WorktreesSectionProps = {
    worktrees,
    now: NOW,
    onOpen: vi.fn(),
    onRemove: vi.fn(() => Promise.resolve()),
    ...extra,
  };
  const view = render(<WorktreesSection {...props} />);
  return { props, view };
}

const row = (name: string) => screen.getByRole("listitem", { name });
const flush = () => act(async () => {});

const released = testWorktree(
  "wide-cards",
  { release: "v1.4.0", merged: "main", status: { kind: "release", tag: "v1.4.0" } },
  {
    sessions: [
      testSession("s1", { kind: "finished", detail: "completed" }, 26, "Should I tag v1.4.0?\nThe notes are ready.", 1, "askedQuestion"),
    ],
  },
);
const dirty = testWorktree("search", { status: { kind: "changes", count: 3 }, removable: false }, { changedFiles: 3 });

describe("WorktreesSection", () => {
  it("lists each worktree with its branch, status and last agent message", () => {
    setup([released, dirty]);
    const cards = row("tidepool-wide-cards");
    expect(cards.textContent).toContain("wide-cards");
    expect(cards.textContent).toContain("In v1.4.0 · Ready to remove");
    const said = within(cards).getByText("Should I tag v1.4.0?");
    expect(said.getAttribute("title")).toBe("Should I tag v1.4.0?\nThe notes are ready.");
    expect(cards.textContent).toContain("yesterday");
    const search = row("tidepool-search");
    expect(search.textContent).toContain("3 uncommitted changes");
    expect(search.textContent).not.toContain("Ready to remove");
  });

  it("says (detached) for a detached HEAD", () => {
    setup([testWorktree("probe", {}, { branch: null })]);
    expect(row("tidepool-probe").textContent).toContain("(detached)");
  });

  it("shows nothing without worktrees", () => {
    const { view } = setup([]);
    expect(view.container.textContent).toBe("");
  });

  it("offers Remove only when nothing can be lost and no agent is open", () => {
    const idle = testWorktree("idle", { status: { kind: "pushed" } }, {
      sessions: [testSession("i", { kind: "live", detail: "idle" }, 0.1)],
    });
    const waiting = testWorktree("asks", {}, {
      sessions: [testSession("w", { kind: "live", detail: "waitingOnUser" }, 0.1)],
    });
    setup([released, dirty, idle, waiting]);
    expect(within(row("tidepool-wide-cards")).queryByRole("button", { name: /Remove/ })).not.toBeNull();
    expect(within(row("tidepool-search")).queryByRole("button", { name: /Remove/ })).toBeNull();
    const open = row("tidepool-idle");
    expect(within(open).queryByRole("button", { name: /Remove/ })).toBeNull();
    expect(open.textContent).toContain("Pushed, not merged");
    expect(open.textContent).toContain("Close the agent here to remove");
    const asks = row("tidepool-asks");
    expect(asks.textContent).toContain("Agent waiting for you");
    expect(within(asks).queryByRole("button", { name: /Remove/ })).toBeNull();
  });

  it("offers no Remove for a worktree with a refactor, and says why", () => {
    setup([testWorktree("cards", { status: { kind: "pushed" } }, { refactor: testRefactor() })]);
    const cards = row("tidepool-cards");
    expect(cards.textContent).toContain("Has a refactor");
    expect(within(cards).queryByRole("button", { name: /Remove/ })).toBeNull();
  });

  it("moves focus into the confirm, and back to Remove… on Keep it", () => {
    setup([released]);
    const remove = screen.getByRole("button", { name: "Remove tidepool-wide-cards…" });
    remove.focus();
    fireEvent.click(remove);
    expect(document.activeElement).toBe(within(screen.getByRole("group")).getByRole("button", { name: "Keep it" }));
    fireEvent.click(document.activeElement!);
    expect(document.activeElement).toBe(screen.getByRole("button", { name: "Remove tidepool-wide-cards…" }));
  });

  it("opens a worktree as Enter does on its folder", () => {
    const { props } = setup([released, dirty]);
    fireEvent.click(screen.getByRole("button", { name: "Open tidepool-search" }));
    expect(props.onOpen).toHaveBeenCalledWith(dirty.path);
  });

  it("confirms Remove, naming the folder, the branch that stays and the ignored files", async () => {
    const ignored = Array.from({ length: 9 }, (_, i) => (i === 0 ? ".env" : `cache-${i}/`));
    const wt = testWorktree("wide-cards", { ignored, status: { kind: "merged", branch: "main" } });
    const { props } = setup([wt]);
    fireEvent.click(screen.getByRole("button", { name: "Remove tidepool-wide-cards…" }));
    const confirm = screen.getByRole("group", { name: "Confirm removing tidepool-wide-cards" });
    expect(confirm.textContent).toContain(`Remove the folder ${wt.path}? The branch wide-cards stays.`);
    expect(confirm.textContent).toContain("Also deletes ignored files:");
    const files = within(confirm).getByRole("list", { name: "Ignored files" });
    const items = within(files).getAllByRole("listitem").map((li) => li.textContent);
    expect(items).toEqual([...ignored.slice(0, 8), "and more"]);
    // Keep it backs out without removing.
    fireEvent.click(within(confirm).getByRole("button", { name: "Keep it" }));
    expect(props.onRemove).not.toHaveBeenCalled();
    fireEvent.click(screen.getByRole("button", { name: "Remove tidepool-wide-cards…" }));
    fireEvent.click(within(screen.getByRole("group")).getByRole("button", { name: "Remove" }));
    expect(props.onRemove).toHaveBeenCalledWith(wt.path);
    expect(within(screen.getByRole("group")).getByRole("button", { name: "Removing…" })).toHaveProperty("disabled", true);
    await flush();
  });

  it("leaves out the ignored list when there is none", () => {
    setup([released]);
    fireEvent.click(screen.getByRole("button", { name: /Remove/ }));
    expect(screen.getByRole("group").textContent).not.toContain("ignored");
  });

  it("shows the helper's refusal and lets Remove be tried again", async () => {
    const onRemove = vi.fn(() =>
      Promise.reject({ kind: "worktree", message: "Cannot remove the worktree: This worktree has uncommitted changes." }),
    );
    setup([released], { onRemove });
    fireEvent.click(screen.getByRole("button", { name: /Remove/ }));
    fireEvent.click(within(screen.getByRole("group")).getByRole("button", { name: "Remove" }));
    await flush();
    expect(screen.getByRole("alert").textContent).toBe(
      "Cannot remove the worktree: This worktree has uncommitted changes.",
    );
    expect(within(screen.getByRole("group")).getByRole("button", { name: "Remove" })).toHaveProperty("disabled", false);
  });
});
