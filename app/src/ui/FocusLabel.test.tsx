import { cleanup, render } from "@testing-library/react";
import { afterEach, describe, expect, it } from "vitest";
import { FocusLabel, stateLine, statusSwatch } from "./FocusLabel";
import { testRepo, testSession } from "./testRepo";

afterEach(cleanup);

describe("FocusLabel", () => {
  it("shows name, host, language, stone and state", () => {
    const { container } = render(<FocusLabel repo={testRepo()} />);
    expect(container.textContent).toContain("tidepool");
    expect(container.textContent).toContain("GitHub · TypeScript · sapphire");
    expect(container.textContent).toContain("Clean");
    expect(container.querySelector(".rf-state .rf-sw--clean")).not.toBeNull();
  });

  it("falls back for a local repo without a language", () => {
    const { container } = render(<FocusLabel repo={testRepo({ remote: null, language: null })} />);
    expect(container.textContent).toContain("Local · Unknown language · quartz");
  });

  it("says when a repo is not cloned", () => {
    const { container } = render(<FocusLabel repo={testRepo()} notCloned />);
    expect(container.textContent).toContain("Not cloned");
    expect(container.textContent).not.toContain("Clean");
    expect(container.querySelector(".rf-sw--remote")).not.toBeNull();
  });

  it("shows a red line and swatch when the repo waits for you", () => {
    const repo = testRepo({ sessions: [testSession("a", { kind: "live", detail: "waitingOnUser" }, 1)] });
    const { container } = render(<FocusLabel repo={repo} />);
    expect(container.querySelector(".rf-state--attention .rf-sw--waiting")).not.toBeNull();
  });
});

describe("stateLine", () => {
  it("lists what needs attention first and counts changed files", () => {
    const repo = testRepo({
      changedFiles: 1,
      sessions: [
        testSession("a", { kind: "live", detail: "waitingOnUser" }, 1),
        testSession("b", { kind: "interrupted", detail: "apiError" }, 3),
      ],
    });
    expect(stateLine(repo)).toEqual({
      text: "Waiting for you · Session didn't finish · 1 changed file",
      tone: "attention",
    });
    expect(stateLine(testRepo({ changedFiles: 4 }))).toEqual({ text: "4 changed files", tone: "dirty" });
    expect(stateLine(testRepo())).toEqual({ text: "Clean", tone: "calm" });
  });
});

describe("statusSwatch", () => {
  const waiting = testSession("w", { kind: "live", detail: "waitingOnUser" }, 1);
  const working = testSession("k", { kind: "live", detail: "working" }, 1);
  const stopped = testSession("i", { kind: "interrupted", detail: "apiError" }, 2);

  it("picks waiting, then working, interrupted, dirty, not cloned, clean", () => {
    expect(statusSwatch(testRepo({ changedFiles: 2, sessions: [stopped, working, waiting] }))).toBe("waiting");
    expect(statusSwatch(testRepo({ changedFiles: 2, sessions: [stopped, working] }))).toBe("working");
    expect(statusSwatch(testRepo({ changedFiles: 2, sessions: [stopped] }))).toBe("interrupted");
    expect(statusSwatch(testRepo({ changedFiles: 2 }))).toBe("dirty");
    expect(statusSwatch(testRepo(), true)).toBe("remote");
    expect(statusSwatch(testRepo())).toBe("clean");
  });
});
