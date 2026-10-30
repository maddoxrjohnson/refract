import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import { createTicker } from "./ticker";

describe("createTicker", () => {
  beforeEach(() => vi.useFakeTimers());
  afterEach(() => vi.useRealTimers());

  it("invalidates shortly before the frame interval is up", () => {
    const invalidate = vi.fn();
    let t = 1000;
    const ticker = createTicker(invalidate, () => t, 4);
    // 30 fps: 33.3 ms per frame; the frame took 5 ms, so wait about 24 ms more.
    t = 1005;
    ticker.request(30, 1000);
    vi.advanceTimersByTime(23);
    expect(invalidate).not.toHaveBeenCalled();
    vi.advanceTimersByTime(2);
    expect(invalidate).toHaveBeenCalledTimes(1);
  });

  it("invalidates at once when the frame ran over", () => {
    const invalidate = vi.fn();
    const ticker = createTicker(invalidate, () => 1040, 4);
    ticker.request(30, 1000);
    expect(invalidate).toHaveBeenCalledTimes(1);
  });

  it("keeps only the latest request and can cancel it", () => {
    const invalidate = vi.fn();
    const ticker = createTicker(invalidate, () => 0, 0);
    ticker.request(30, 0);
    ticker.request(30, 0);
    vi.advanceTimersByTime(40);
    expect(invalidate).toHaveBeenCalledTimes(1);
    ticker.request(30, 0);
    ticker.cancel();
    vi.advanceTimersByTime(40);
    expect(invalidate).toHaveBeenCalledTimes(1);
  });
});
