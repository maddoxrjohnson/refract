import { act, renderHook } from "@testing-library/react";
import { describe, expect, it, vi } from "vitest";
import type { Backend } from "../backend/Backend";
import type { RemoteDetails, RemoteRef } from "../backend/types";
import { useRemoteDetails } from "./useRemoteDetails";

const details = (n: number): RemoteDetails => ({
  openPullRequests: n,
  ci: "success",
  ciUrl: null,
  fetchedAt: "2026-10-30T12:00:00Z",
});

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
});
