//! The global show/hide hotkey.

use tauri::plugin::TauriPlugin;
use tauri::{AppHandle, Wry};
use tauri_plugin_global_shortcut::{GlobalShortcutExt, ShortcutState};

use crate::window;

/// Toggles the window from anywhere: Win+Alt+Space. Windows keeps Win+Space and
/// Win+Shift+Space for switching input languages and refuses to register them. If
/// another app already holds this one, the app runs without a hotkey.
pub const SHORTCUT: &str = "Super+Alt+Space";

pub fn plugin() -> TauriPlugin<Wry> {
    tauri_plugin_global_shortcut::Builder::new()
        .with_handler(|app, _shortcut, event| {
            if event.state() == ShortcutState::Pressed {
                window::toggle(app);
            }
        })
        .build()
}

/// Registers [`SHORTCUT`], logging instead of failing when Windows refuses it.
pub fn register(app: &AppHandle) {
    match app.global_shortcut().register(SHORTCUT) {
        Ok(()) => eprintln!("refract: hotkey {SHORTCUT} registered"),
        Err(err) => eprintln!(
            "refract: could not register the hotkey {SHORTCUT}: {err}. \
             Another app may hold it; the tray icon still works."
        ),
    }
}
