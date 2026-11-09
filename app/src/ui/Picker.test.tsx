import { cleanup, fireEvent, render, screen } from "@testing-library/react";
import { afterEach, describe, expect, it, vi } from "vitest";
import { DIRTY_REPO_PROMPT } from "../repoStatus";
import type { PickerOption } from "./navigation";
import { liveStatus, newLabel, newText, Picker, resumeLabel, resumeOptionLabel } from "./Picker";
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

  it("names why each session is worth resuming", () => {
    setup();
    expect(screen.getAllByRole("option")[0].textContent).toContain("Resume: stopped mid-task");
    const finished = { kind: "finished", detail: "completed" } as const;
    const at = (reason: "cutOff" | "askedQuestion" | "uncommittedChanges" | "unpushedCommits") =>
      resumeLabel(testSession("s", finished, 1, null, 1000, reason));
    expect([at("cutOff"), at("askedQuestion"), at("uncommittedChanges"), at("unpushedCommits")]).toEqual([
      "Resume: cut off by a restart",
      "Resume: asked you a question",
      "Resume: uncommitted changes",
      "Resume: unpushed commits",
    ]);
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

  it("names the agent of each row with Codex installed, and dims a row that can't start", () => {
    const codexCut = { ...testSession("c1", { kind: "interrupted", detail: "apiError" }, 1, "Smoothing the strokes."), agent: "codex" as const };
    const rows: PickerOption[] = [
      { kind: "new", agent: "claude", brief: "c1", reason: "Codex worked here last" },
      { kind: "new", agent: "codex", blocked: "Not ready: No AGENTS.md at the repo root" },
      { kind: "resume", session: codexCut, agent: "codex" },
    ];
    render(<Picker repoName="tidepool" options={rows} index={0} now={NOW} onMove={vi.fn()} onAccept={vi.fn()} onClose={vi.fn()} />);
    const [claude, codex, resume] = screen.getAllByRole("option");
    expect(claude.textContent).toContain("New Claude agent, briefed");
    expect(claude.textContent).toContain("Codex worked here last · Picks up where Codex left off");
    expect(claude.getAttribute("aria-disabled")).toBeNull();
    expect(codex.textContent).toContain("New Codex agent");
    expect(codex.textContent).toContain("Not ready: No AGENTS.md at the repo root");
    expect(codex.getAttribute("aria-disabled")).toBe("true");
    expect(codex.className).toContain("rf-option--dimmed");
    expect(resume.textContent).toContain("[Codex] Resume: stopped mid-task");
  });

  it("dims a busy resume row with its reason, and labels a stop-idle one", () => {
    const claudeCut = testSession("k1", { kind: "interrupted", detail: "apiError" }, 1, "Writing the exporter.");
    const rows: PickerOption[] = [
      { kind: "resume", session: claudeCut, agent: "claude", blocked: "Busy: Codex is working" },
      { kind: "resume", session: { ...claudeCut, id: "k2" }, agent: "claude", stop: ["x"] },
    ];
    render(<Picker repoName="tidepool" options={rows} index={0} now={NOW} onMove={vi.fn()} onAccept={vi.fn()} onClose={vi.fn()} />);
    const [busy, stop] = screen.getAllByRole("option");
    expect(busy.getAttribute("aria-disabled")).toBe("true");
    expect(busy.className).toContain("rf-option--dimmed");
    expect(busy.textContent).toContain("Busy: Codex is working");
    expect(busy.textContent).not.toContain("Writing the exporter.");
    expect(stop.getAttribute("aria-disabled")).toBeNull();
    expect(stop.textContent).toContain("[Claude] Stop the idle Codex agent and resume");
    expect(resumeOptionLabel({ kind: "resume", session: claudeCut, agent: "codex", stop: ["x"] })).toBe(
      "Stop the idle Claude agent and resume",
    );
  });

  it("labels the stop-idle rows and leaves tags out without Codex", () => {
    expect(newLabel({ kind: "new", agent: "codex", stop: ["x"] })).toBe("Stop the idle Claude agent and start Codex");
    expect(newLabel({ kind: "new", agent: "claude", stop: ["x"] })).toBe("Stop the idle Codex agent and start Claude");
    expect(newLabel({ kind: "new" })).toBe("New agent");
    expect(newText({ kind: "new", agent: "codex", reason: "Claude at 97%" })).toBe("Claude at 97% · A fresh session");
    setup();
    expect(screen.getAllByRole("option")[0].textContent).not.toContain("[Claude]");
  });
});
