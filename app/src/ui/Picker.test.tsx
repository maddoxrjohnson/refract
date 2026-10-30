import { cleanup, fireEvent, render, screen } from "@testing-library/react";
import { afterEach, describe, expect, it, vi } from "vitest";
import { DIRTY_REPO_PROMPT } from "../repoStatus";
import type { PickerOption } from "./navigation";
import { liveStatus, Picker } from "./Picker";
import { NOW, testSession } from "./testRepo";

afterEach(cleanup);

const options: PickerOption[] = [
  {
    kind: "resume",
    session: testSession("s1", { kind: "interrupted", detail: "apiError" }, 2, "Running the migration.\nMore", 420_000),
  },
  { kind: "new", prompt: DIRTY_REPO_PROMPT },
];

function setup(index = 0) {
  const onMove = vi.fn();
  const onAccept = vi.fn();
  const onClose = vi.fn();
  render(
    <Picker repoName="tidepool" options={options} index={index} now={NOW} onMove={onMove} onAccept={onAccept} onClose={onClose} />,
  );
  return { onMove, onAccept, onClose, listbox: screen.getByRole("listbox") };
}

describe("Picker", () => {
  it("is a focused listbox whose active descendant is the highlighted option", () => {
    const { listbox } = setup(1);
    expect(document.activeElement).toBe(listbox);
    const opts = screen.getAllByRole("option");
    expect(opts).toHaveLength(2);
    expect(listbox.getAttribute("aria-activedescendant")).toBe(opts[1].id);
    expect(opts[1].getAttribute("aria-selected")).toBe("true");
    expect(opts[0].getAttribute("aria-selected")).toBe("false");
  });

  it("describes each option: age, size and first line, or what a new agent does", () => {
    setup();
    const [resume, fresh] = screen.getAllByRole("option");
    expect(resume.textContent).toContain("2 hours ago");
    expect(resume.textContent).toContain("410 KB");
    expect(resume.textContent).toContain("Running the migration.");
    expect(resume.textContent).not.toContain("More");
    expect(fresh.textContent).toContain("New agent");
    expect(fresh.textContent).toContain("uncommitted changes");
  });

  it("hover highlights, click accepts, Cancel closes", () => {
    const { onMove, onAccept, onClose } = setup();
    const [, fresh] = screen.getAllByRole("option");
    fireEvent.mouseEnter(fresh);
    expect(onMove).toHaveBeenCalledWith(1);
    fireEvent.click(fresh);
    expect(onAccept).toHaveBeenCalledWith(1);
    fireEvent.click(screen.getByRole("button", { name: /Cancel/ }));
    expect(onClose).toHaveBeenCalledOnce();
  });

  it("keeps the highlighted option scrolled into view", () => {
    const scrolled: Element[] = [];
    const scrollIntoView = vi.fn(function (this: Element) {
      scrolled.push(this);
    });
    Element.prototype.scrollIntoView = scrollIntoView;
    try {
      const props = { repoName: "tidepool", options, now: NOW, onMove: vi.fn(), onAccept: vi.fn(), onClose: vi.fn() };
      const { rerender } = render(<Picker {...props} index={0} />);
      rerender(<Picker {...props} index={1} />);
      const all = screen.getAllByRole("option");
      expect(scrolled).toEqual([all[0], all[1]]);
      expect(scrollIntoView).toHaveBeenLastCalledWith({ block: "nearest" });
    } finally {
      delete (Element.prototype as Partial<Element>).scrollIntoView;
    }
  });

  it("offers to go to a running agent with its status, age and last reply", () => {
    const running = {
      ...testSession("s2", { kind: "live", detail: "waitingOnUser" }, 1, "Shall I run the tide import?\nMore"),
      focusable: true,
    };
    const withFocus: PickerOption[] = [{ kind: "focus", session: running }, ...options];
    render(
      <Picker repoName="tidepool" options={withFocus} index={0} now={NOW} onMove={vi.fn()} onAccept={vi.fn()} onClose={vi.fn()} />,
    );
    const [focus, resume] = screen.getAllByRole("option");
    expect(focus.textContent).toContain("Go to the running agent");
    expect(focus.textContent).toContain("waiting on you · 1 hour ago");
    expect(focus.textContent).toContain("Shall I run the tide import?");
    expect(focus.textContent).not.toContain("More");
    expect(resume.textContent).toContain("Resume");
  });

  it("names each live status", () => {
    const at = (detail: "working" | "waitingOnUser" | "idle" | "unknown") =>
      liveStatus(testSession("s", { kind: "live", detail }, 1));
    expect([at("waitingOnUser"), at("working"), at("idle"), at("unknown")]).toEqual([
      "waiting on you",
      "working",
      "idle",
      "running",
    ]);
  });

  it("is labelled by the repo name", () => {
    setup();
    expect(screen.getByRole("dialog", { name: "Start an agent in tidepool" })).toBeTruthy();
  });
});
