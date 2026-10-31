import { act, renderHook } from "@testing-library/react";
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import { useThemeMode, type ThemeSetting } from "./theme";

/** A `matchMedia` whose dark-mode answer the test changes. */
function fakeMedia(dark: boolean) {
  const listeners = new Set<() => void>();
  const mq = {
    get matches() {
      return dark;
    },
    addEventListener: (_: string, cb: () => void) => listeners.add(cb),
    removeEventListener: (_: string, cb: () => void) => listeners.delete(cb),
  };
  window.matchMedia = vi.fn(() => mq as unknown as MediaQueryList);
  return {
    listeners,
    set(next: boolean) {
      dark = next;
      listeners.forEach((cb) => cb());
    },
  };
}

const original = window.matchMedia;
const root = document.documentElement;

beforeEach(() => root.removeAttribute("data-theme"));
afterEach(() => {
  window.matchMedia = original;
});

describe("useThemeMode", () => {
  it("follows the system and leaves data-theme off", () => {
    fakeMedia(true);
    const { result } = renderHook(() => useThemeMode("system"));
    expect(result.current).toBe("dark");
    expect(root.hasAttribute("data-theme")).toBe(false);
  });

  it("follows a live change of the system mode and stops listening on unmount", () => {
    const media = fakeMedia(false);
    const { result, unmount } = renderHook(() => useThemeMode("system"));
    expect(result.current).toBe("light");
    act(() => media.set(true));
    expect(result.current).toBe("dark");
    act(() => media.set(false));
    expect(result.current).toBe("light");
    unmount();
    expect(media.listeners.size).toBe(0);
  });

  it("lets light and dark override the system and writes data-theme", () => {
    const media = fakeMedia(true);
    const { result, rerender } = renderHook(({ s }: { s: ThemeSetting }) => useThemeMode(s), {
      initialProps: { s: "light" },
    });
    expect(result.current).toBe("light");
    expect(root.getAttribute("data-theme")).toBe("light");
    act(() => media.set(false));
    rerender({ s: "dark" });
    expect(result.current).toBe("dark");
    expect(root.getAttribute("data-theme")).toBe("dark");
  });

  it("removes data-theme when going back to system", () => {
    fakeMedia(false);
    const { result, rerender } = renderHook(({ s }: { s: ThemeSetting }) => useThemeMode(s), {
      initialProps: { s: "dark" },
    });
    expect(root.getAttribute("data-theme")).toBe("dark");
    rerender({ s: "system" });
    expect(result.current).toBe("light");
    expect(root.hasAttribute("data-theme")).toBe(false);
  });

  it("matches the title bar to the setting", () => {
    fakeMedia(false);
    const titleBar = { setWindowTheme: vi.fn().mockResolvedValue(undefined) };
    const { rerender } = renderHook(({ s }: { s: ThemeSetting }) => useThemeMode(s, titleBar), {
      initialProps: { s: "system" },
    });
    rerender({ s: "dark" });
    expect(titleBar.setWindowTheme.mock.calls).toEqual([["system"], ["dark"]]);
  });

  it("reads as light where matchMedia is missing", () => {
    // @ts-expect-error jsdom without matchMedia
    window.matchMedia = undefined;
    const { result } = renderHook(() => useThemeMode("system"));
    expect(result.current).toBe("light");
  });
});
