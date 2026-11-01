import { act, renderHook } from "@testing-library/react";
import { afterEach, describe, expect, it, vi } from "vitest";
import type { Backend } from "../backend/Backend";
import type { RefactorNotice } from "../backend/types";
import { NOTICE_MS, useRefactorNotices } from "./useRefactorNotices";

afterEach(() => vi.useRealTimers());

const PATH = "/home/demo/projects/tidepool";

function fakeBackend() {
  let send: ((n: RefactorNotice) => void) | null = null;
  const setZoomed = vi.fn(() => Promise.resolve());
  const backend = {
    onRefactorNotice: (cb: (n: RefactorNotice) => void) => {
      send = cb;
      return () => (send = null);
    },
    setZoomed,
  } as unknown as Backend;
  return { backend, setZoomed, send: (n: RefactorNotice) => act(() => send?.(n)) };
}

describe("useRefactorNotices", () => {
  it("tells the Rust side the zoomed repo and shows what it sends until it fades", () => {
    vi.useFakeTimers();
    const { backend, setZoomed, send } = fakeBackend();
    const { result, rerender } = renderHook((p: { zoomed: string | null }) => useRefactorNotices(backend, p.zoomed), {
      initialProps: { zoomed: null as string | null },
    });
    expect(setZoomed).toHaveBeenLastCalledWith(null);
    rerender({ zoomed: "/home/demo/projects/quill" });
    expect(setZoomed).toHaveBeenLastCalledWith("/home/demo/projects/quill");
    send({ path: PATH, repoName: "tidepool", alert: "planReady" });
    expect(result.current.notice?.alert).toBe("planReady");
    act(() => vi.advanceTimersByTime(NOTICE_MS));
    expect(result.current.notice).toBeNull();
  });

  it("clears a notice when its repo is zoomed, and on dismiss", () => {
    const { backend, send } = fakeBackend();
    const { result, rerender } = renderHook((p: { zoomed: string | null }) => useRefactorNotices(backend, p.zoomed), {
      initialProps: { zoomed: null as string | null },
    });
    send({ path: PATH, repoName: "tidepool", alert: "finished" });
    rerender({ zoomed: PATH });
    expect(result.current.notice).toBeNull();
    send({ path: PATH, repoName: "tidepool", alert: "finished" });
    act(() => result.current.dismiss());
    expect(result.current.notice).toBeNull();
  });
});
