// Light and dark (banknote spec §5). The setting is saved with the other settings; the
// resolved mode drives the CSS tokens in ui/ui.css and the scene's palette.

/** What the user chose in settings: follow Windows, or a fixed mode. */
export type ThemeSetting = "system" | "light" | "dark";

/** The mode actually shown. */
export type ThemeMode = "light" | "dark";
