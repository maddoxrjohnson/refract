// Paces redraws while something animates. The canvas runs R3F's "demand" frame loop, so a
// frame happens only when invalidated; after each drawn frame the scene asks for the next
// one here, and the ticker invalidates when the tier's frame interval is nearly up. The
// animation frame that follows lands on the next display refresh, so the ticker fires a
// little early (`lead`) to avoid skipping one.

export interface Ticker {
  /** Ask for another frame, `1000 / fps` ms after the frame that started at `frameStart`. */
  request(fps: number, frameStart: number): void;
  /** Drop any pending request. */
  cancel(): void;
}

export function createTicker(
  invalidate: () => void,
  now: () => number = () => performance.now(),
  lead = 4,
): Ticker {
  let timer: ReturnType<typeof setTimeout> | null = null;
  const cancel = () => {
    if (timer !== null) clearTimeout(timer);
    timer = null;
  };
  return {
    request(fps, frameStart) {
      cancel();
      const delay = 1000 / fps - (now() - frameStart) - lead;
      if (delay <= 0) {
        invalidate();
        return;
      }
      timer = setTimeout(() => {
        timer = null;
        invalidate();
      }, delay);
    },
    cancel,
  };
}
