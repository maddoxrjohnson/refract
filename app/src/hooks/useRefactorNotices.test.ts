import { act, renderHook } from "@testing-library/react";
import { afterEach, describe, expect, it, vi } from "vitest";
import type { RefactorState, RepoSnapshot } from "../backend/types";
import { testRefactor, testRepo } from "../ui/testRepo";
import { NOTICE_MS, QUIET_AFTER_SHOW_MS, useRefactorNotices } from "./useRefactorNotices";

afterEach(() => vi.useRealTimers());

const PATH = "/home/demo/projects/tidepool";
const OTHER = "/home/demo/projects/quill";

function repos(state: RefactorState | null): Map<string, RepoSnapshot> {
  const repo = testRepo({ refactor: state ? testRefactor({ state }) : null });
  return new Map([[repo.path, repo]]);
}

function setup(visible = true, zoomed: string | null = OTHER) {
  vi.useFakeTimers();
  const hook = renderHook(
    (p: { repos: Map<string, RepoSnapshot>; visible: boolean; zoomed: string | null }) =>
      useRefactorNotices(p.repos, p.visible, p.zoomed),
    { initialProps: { repos: repos("working"), visible, zoomed } },
  );
  act(() => vi.advanceTimersByTime(QUIET_AFTER_SHOW_MS));
  return hook;
}

describe("useRefactorNotices", () => {
  it("tells a plan that is ready once, for a repo that isn't zoomed, then fades", () => {
    const { result, rerender } = setup();
    rerender({ repos: repos("needsYou"), visible: true, zoomed: OTHER });
    expect(result.current.notice).toEqual({ path: PATH, repoName: "tidepool", alert: "planReady" });
    act(() => result.current.dismiss());
    rerender({ repos: repos("needsYou"), visible: true, zoomed: OTHER });
    expect(result.current.notice).toBeNull();
    rerender({ repos: repos("ready"), visible: true, zoomed: OTHER });
    expect(result.current.notice?.alert).toBe("finished");
    act(() => vi.advanceTimersByTime(NOTICE_MS));
    expect(result.current.notice).toBeNull();
  });

  it("stays quiet for the zoomed repo, and clears when its repo is zoomed", () => {
    const { result, rerender } = setup(true, PATH);
    rerender({ repos: repos("needsYou"), visible: true, zoomed: PATH });
    expect(result.current.notice).toBeNull();
    rerender({ repos: repos("ready"), visible: true, zoomed: OTHER });
    expect(result.current.notice?.alert).toBe("finished");
    rerender({ repos: repos("ready"), visible: true, zoomed: PATH });
    expect(result.current.notice).toBeNull();
  });

  it("stays quiet while hidden and just after showing, when Windows has told it", () => {
    const { result, rerender } = setup(false);
    rerender({ repos: repos("needsYou"), visible: false, zoomed: OTHER });
    rerender({ repos: repos("needsYou"), visible: true, zoomed: OTHER });
    rerender({ repos: repos("ready"), visible: true, zoomed: OTHER });
    expect(result.current.notice).toBeNull();
  });
});
