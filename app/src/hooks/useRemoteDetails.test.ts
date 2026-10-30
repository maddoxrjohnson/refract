import { act, renderHook } from "@testing-library/react";
import { afterEach, describe, expect, it, vi } from "vitest";
import type { Backend } from "../backend/Backend";
import type { RemoteDetails, RemoteRef } from "../backend/types";
import { DETAILS_MIN_DELAY_MS, DETAILS_REFRESH_MS, useRemoteDetails } from "./useRemoteDetails";

const details = (n: number, fetchedAt = "2026-10-30T12:00:00Z"): RemoteDetails => ({
  openPullRequests: n,
  ci: "success",
  ciUrl: null,
  fetchedAt,
});
/** Details fetched `agoMs` before now (the fake clock's now inside the timer tests). */
const fresh = (n: number, agoMs = 0) => details(n, new Date(Date.now() - agoMs).toISOString());

/** A backend whose remoteDetails answers are resolved by hand. */
function deferredBackend() {
  const pending: Array<{ repo: RemoteRef; refresh: boolean; resolve(d: RemoteDetails): void; reject(e: unknown): void }> = [];
  const remoteDetails = vi.fn(
    (repo: RemoteRef, refresh = false) =>
      new Promise<RemoteDetails>((resolve, reject) => pending.push({ repo, refresh, resolve, reject })),
  );
  return { backend: { remoteDetails } as unknown as Backend, pending };
}

const A: RemoteRef = { host: "github.com", owner: "demo-user", name: "atlas-api" };
const B: RemoteRef = { host: "github.com", owner: "demo-user", name: "quill" };

describe("useRemoteDetails", () => {
  it("is null without a repo, loads, and keeps details while refreshing", async () => {
    const { backend, pending } = deferredBackend();
    const { result, rerender } = renderHook(({ ref }) => useRemoteDetails(backend, ref), {
      initialProps: { ref: null as RemoteRef | null },
    });
    expect(result.current.state).toBeNull();
    rerender({ ref: A });
    expect(result.current.state).toEqual({ status: "loading", details: null });
    await act(async () => pending.at(-1)!.resolve(details(3)));
    expect(result.current.state).toEqual({ status: "ready", details: details(3) });
    act(() => result.current.refresh());
    expect(pending.at(-1)!.refresh).toBe(true);
    expect(result.current.state).toEqual({ status: "loading", details: details(3) });
    await act(async () => pending.at(-1)!.reject({ kind: "network", message: "Offline." }));
    expect(result.current.state).toEqual({ status: "error", message: "Offline.", details: details(3) });
  });

  it("drops answers for a repo that is no longer shown", async () => {
    const { backend, pending } = deferredBackend();
    const { result, rerender } = renderHook(({ ref }) => useRemoteDetails(backend, ref), {
      initialProps: { ref: A as RemoteRef | null },
    });
    const first = pending.at(-1)!;
    rerender({ ref: B });
    await act(async () => first.resolve(details(9)));
    expect(result.current.state).toEqual({ status: "loading", details: null });
    await act(async () => pending.at(-1)!.resolve(details(1)));
    expect(result.current.state).toEqual({ status: "ready", details: details(1) });
    expect(pending.at(-1)!.repo).toEqual(B);
  });

  describe("while the panel stays open", () => {
    afterEach(() => {
      vi.useRealTimers();
    });

    it("fetches again every 15 minutes without showing loading, and keeps details on failure", async () => {
      vi.useFakeTimers();
      const { backend, pending } = deferredBackend();
      const { result } = renderHook(() => useRemoteDetails(backend, A, true));
      const d3 = fresh(3);
      await act(async () => pending.at(-1)!.resolve(d3));
      expect(pending).toHaveLength(1);

      await act(async () => vi.advanceTimersByTime(DETAILS_REFRESH_MS - 1));
      expect(pending).toHaveLength(1);
      await act(async () => vi.advanceTimersByTime(1));
      expect(pending).toHaveLength(2);
      expect(pending[1]).toMatchObject({ repo: A, refresh: true });
      expect(result.current.state).toEqual({ status: "ready", details: d3 });
      const d5 = fresh(5);
      await act(async () => pending[1].resolve(d5));
      expect(result.current.state).toEqual({ status: "ready", details: d5 });

      await act(async () => vi.advanceTimersByTime(DETAILS_REFRESH_MS));
      expect(pending).toHaveLength(3);
      await act(async () => pending[2].reject({ kind: "network", message: "Offline." }));
      expect(result.current.state).toEqual({ status: "ready", details: d5 });
    });

    it("runs no timer while hidden, and catches up when shown again", async () => {
      vi.useFakeTimers();
      const { backend, pending } = deferredBackend();
      const { result, rerender } = renderHook(({ visible }) => useRemoteDetails(backend, A, visible), {
        initialProps: { visible: true },
      });
      const d3 = fresh(3);
      await act(async () => pending.at(-1)!.resolve(d3));
      rerender({ visible: false });
      expect(vi.getTimerCount()).toBe(0);
      await act(async () => vi.advanceTimersByTime(DETAILS_REFRESH_MS * 3));
      expect(pending).toHaveLength(1);

      rerender({ visible: true });
      await act(async () => vi.advanceTimersByTime(DETAILS_MIN_DELAY_MS));
      expect(pending).toHaveLength(2);
      expect(result.current.state).toEqual({ status: "ready", details: d3 });
    });

    it("starts the 15 minutes again for each repo, and stops when none is zoomed", async () => {
      vi.useFakeTimers();
      const { backend, pending } = deferredBackend();
      const { rerender } = renderHook(({ ref }) => useRemoteDetails(backend, ref, true), {
        initialProps: { ref: A as RemoteRef | null },
      });
      await act(async () => pending[0].resolve(fresh(1)));
      await act(async () => vi.advanceTimersByTime(DETAILS_REFRESH_MS - 1_000));
      rerender({ ref: B });
      expect(pending).toHaveLength(2);
      await act(async () => pending[1].resolve(fresh(2)));
      await act(async () => vi.advanceTimersByTime(DETAILS_REFRESH_MS - 1));
      expect(pending).toHaveLength(2);
      await act(async () => vi.advanceTimersByTime(1));
      expect(pending).toHaveLength(3);
      expect(pending[2].repo).toEqual(B);

      rerender({ ref: null });
      expect(vi.getTimerCount()).toBe(0);
    });

    it("does not start a timed fetch over a Refresh, which can then fail and be retried", async () => {
      vi.useFakeTimers();
      const { backend, pending } = deferredBackend();
      const { result } = renderHook(() => useRemoteDetails(backend, A, true));
      await act(async () => pending.at(-1)!.resolve(fresh(3)));

      await act(async () => vi.advanceTimersByTime(DETAILS_REFRESH_MS - 1_000));
      act(() => result.current.refresh());
      expect(pending).toHaveLength(2);
      await act(async () => vi.advanceTimersByTime(2_000));
      expect(pending).toHaveLength(2);
      expect(result.current.state).toMatchObject({ status: "loading" });

      await act(async () => pending[1].reject({ kind: "network", message: "Offline." }));
      expect(result.current.state).toMatchObject({ status: "error", message: "Offline." });
    });

    it("keeps the Refresh in charge when a timed fetch is still out", async () => {
      vi.useFakeTimers();
      const { backend, pending } = deferredBackend();
      const { result } = renderHook(() => useRemoteDetails(backend, A, true));
      await act(async () => pending.at(-1)!.resolve(fresh(3)));
      await act(async () => vi.advanceTimersByTime(DETAILS_REFRESH_MS));
      expect(pending).toHaveLength(2);

      act(() => result.current.refresh());
      expect(pending).toHaveLength(3);
      await act(async () => pending[1].reject({ kind: "network", message: "Offline." }));
      expect(result.current.state).toMatchObject({ status: "loading" });
      await act(async () => pending[2].resolve(fresh(7)));
      expect(result.current.state).toEqual({ status: "ready", details: fresh(7) });
    });

    it("skips a timed fetch while another request is out, and tries again later", async () => {
      vi.useFakeTimers();
      const { backend, pending } = deferredBackend();
      renderHook(() => useRemoteDetails(backend, A, true));
      // The first load never answers.
      await act(async () => vi.advanceTimersByTime(DETAILS_REFRESH_MS));
      expect(pending).toHaveLength(1);
      await act(async () => pending[0].reject({ kind: "network", message: "Offline." }));
      await act(async () => vi.advanceTimersByTime(DETAILS_REFRESH_MS));
      expect(pending).toHaveLength(2);
    });

    it("times the next fetch from the age of the details, not from when they arrived", async () => {
      vi.useFakeTimers();
      const { backend, pending } = deferredBackend();
      renderHook(() => useRemoteDetails(backend, A, true));
      // Cached by the backend for 10 of its 15 minutes.
      await act(async () => pending.at(-1)!.resolve(fresh(3, 10 * 60_000)));
      await act(async () => vi.advanceTimersByTime(5 * 60_000 - 1));
      expect(pending).toHaveLength(1);
      await act(async () => vi.advanceTimersByTime(1));
      expect(pending).toHaveLength(2);

      // A manual Refresh answered at once pushes the timed fetch back by 15 minutes.
      await act(async () => pending[1].resolve(fresh(4)));
      await act(async () => vi.advanceTimersByTime(DETAILS_REFRESH_MS - 1));
      expect(pending).toHaveLength(2);
      await act(async () => vi.advanceTimersByTime(1));
      expect(pending).toHaveLength(3);
    });

    it("waits at least a moment when the details are older than 15 minutes", async () => {
      vi.useFakeTimers();
      const { backend, pending } = deferredBackend();
      renderHook(() => useRemoteDetails(backend, A, true));
      await act(async () => pending.at(-1)!.resolve(fresh(3, 2 * DETAILS_REFRESH_MS)));
      await act(async () => vi.advanceTimersByTime(DETAILS_MIN_DELAY_MS - 1));
      expect(pending).toHaveLength(1);
      await act(async () => vi.advanceTimersByTime(1));
      expect(pending).toHaveLength(2);
    });
  });
});
