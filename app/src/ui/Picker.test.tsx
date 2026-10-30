import { cleanup, fireEvent, render, screen } from "@testing-library/react";
import { afterEach, describe, expect, it, vi } from "vitest";
import { DIRTY_REPO_PROMPT } from "../repoStatus";
import type { PickerOption } from "./navigation";
import { Picker } from "./Picker";
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

  it("is labelled by the repo name", () => {
    setup();
    expect(screen.getByRole("dialog", { name: "Start an agent in tidepool" })).toBeTruthy();
  });
});
