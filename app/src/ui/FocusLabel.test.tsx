import { cleanup, render } from "@testing-library/react";
import { afterEach, describe, expect, it } from "vitest";
import { FocusLabel, stateLine } from "./FocusLabel";
import { testRepo, testSession } from "./testRepo";

afterEach(cleanup);

describe("FocusLabel", () => {
  it("shows name, host, language and state", () => {
    const { container } = render(<FocusLabel repo={testRepo()} />);
    expect(container.textContent).toContain("tidepool");
    expect(container.textContent).toContain("GitHub · TypeScript");
    expect(container.textContent).toContain("Clean");
  });

  it("falls back for a local repo without a language", () => {
    const { container } = render(<FocusLabel repo={testRepo({ remote: null, language: null })} />);
    expect(container.textContent).toContain("Local · Unknown language");
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
