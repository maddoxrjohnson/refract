// Light and dark (banknote spec §5). The setting is saved with the other settings; the
// resolved mode drives the CSS tokens in ui/ui.css and the scene's palette.

import { useEffect, useLayoutEffect, useState } from "react";

/** What the user chose in settings: follow Windows, or a fixed mode. */
export type ThemeSetting = "system" | "light" | "dark";

/** The mode actually shown. */
export type ThemeMode = "light" | "dark";

const DARK_QUERY = "(prefers-color-scheme: dark)";

/** Where the last setting is kept, so a fixed mode shows before the settings load. */
export const THEME_STORAGE_KEY = "refract.theme";

/** The setting last shown on this machine, or system if there is none. */
export function rememberedTheme(): ThemeSetting {
  try {
    const v = localStorage.getItem(THEME_STORAGE_KEY);
    return v === "light" || v === "dark" ? v : "system";
  } catch {
    return "system";
  }
}

/** Whether Windows (or the browser) asks for dark right now. */
function systemDark(): boolean {
  return typeof window.matchMedia === "function" && window.matchMedia(DARK_QUERY).matches;
}

/**
 * The mode to show for `setting`. System follows `prefers-color-scheme` live (WebView2
 * reports the Windows app mode there). An override is written to `<html data-theme>` so
 * the CSS tokens follow it; system removes the attribute and the media query decides.
 * `titleBar` (the backend) matches the native title bar; the demo's does nothing.
 */
export function useThemeMode(
  setting: ThemeSetting,
  titleBar?: { setWindowTheme(theme: ThemeSetting): Promise<void> },
): ThemeMode {
  const [dark, setDark] = useState(systemDark);

  useEffect(() => {
    if (typeof window.matchMedia !== "function") return;
    const mq = window.matchMedia(DARK_QUERY);
    const change = () => setDark(mq.matches);
    change();
    mq.addEventListener("change", change);
    return () => mq.removeEventListener("change", change);
  }, []);

  // Before paint, so the first frame already has the right tokens.
  useLayoutEffect(() => {
    const root = document.documentElement;
    if (setting === "system") root.removeAttribute("data-theme");
    else root.setAttribute("data-theme", setting);
    try {
      localStorage.setItem(THEME_STORAGE_KEY, setting);
    } catch {
      // Storage may be unavailable; the next start then waits for the settings.
    }
  }, [setting]);

  useEffect(() => {
    titleBar?.setWindowTheme(setting).catch((err: unknown) => console.warn("setTheme failed", err));
  }, [titleBar, setting]);

  if (setting !== "system") return setting;
  return dark ? "dark" : "light";
}
