import { act, renderHook } from "@testing-library/react";
import { describe, expect, it } from "vitest";
import type { Backend } from "../backend/Backend";
import { useVisibility } from "./useVisibility";

/** A backend whose visibility is driven by the test. */
function fakeBackend(initial: Promise<boolean>) {
  const listeners = new Set<(v: boolean) => void>();
  const backend = {
    isVisible: () => initial,
    onVisibility: (cb: (v: boolean) => void) => {
      listeners.add(cb);
      return () => listeners.delete(cb);
    },
  } as unknown as Backend;
  const emit = (v: boolean) => listeners.forEach((cb) => cb(v));
  return { backend, emit, listeners };
}

describe("useVisibility", () => {
  it("starts hidden, takes the initial answer, then follows events", async () => {
    const { backend, emit } = fakeBackend(Promise.resolve(true));
    const { result } = renderHook(() => useVisibility(backend));
    expect(result.current).toBe(false);
    await act(async () => {});
    expect(result.current).toBe(true);
    act(() => emit(false));
    expect(result.current).toBe(false);
    act(() => emit(true));
    expect(result.current).toBe(true);
  });

  it("lets an event that arrives first win over a late initial answer", async () => {
    let answer!: (v: boolean) => void;
    const { backend, emit } = fakeBackend(new Promise((r) => (answer = r)));
    const { result } = renderHook(() => useVisibility(backend));
    act(() => emit(true));
    await act(async () => answer(false));
    expect(result.current).toBe(true);
  });

  it("unsubscribes on unmount", () => {
    const { backend, listeners } = fakeBackend(Promise.resolve(false));
    const { unmount } = renderHook(() => useVisibility(backend));
    expect(listeners.size).toBe(1);
    unmount();
    expect(listeners.size).toBe(0);
  });
});
