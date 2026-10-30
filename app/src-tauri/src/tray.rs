//! The tray icon and its menu.

use tauri::image::Image;
use tauri::menu::{CheckMenuItem, Menu, MenuEvent, MenuItem, PredefinedMenuItem};
use tauri::tray::{MouseButton, MouseButtonState, TrayIconBuilder, TrayIconEvent};
use tauri::{AppHandle, Manager, Wry};
use tauri_plugin_autostart::ManagerExt;

use crate::window;

const TOGGLE: &str = "toggle";
const AUTOSTART: &str = "autostart";
const QUIT: &str = "quit";

/// Menu items whose state changes while the app runs.
struct TrayMenu {
    toggle: MenuItem<Wry>,
}

/// The toggle item's label for a window that is shown or hidden.
fn toggle_label(shown: bool) -> &'static str {
    if shown {
        "Hide Refract"
    } else {
        "Show Refract"
    }
}

pub fn create(app: &AppHandle) -> tauri::Result<()> {
    let autostart_on = app.autolaunch().is_enabled().unwrap_or_else(|err| {
        eprintln!("refract: reading the start-with-Windows setting: {err}");
        false
    });
    let toggle = MenuItem::with_id(app, TOGGLE, toggle_label(false), true, None::<&str>)?;
    let autostart = CheckMenuItem::with_id(
        app,
        AUTOSTART,
        "Start with Windows",
        true,
        autostart_on,
        None::<&str>,
    )?;
    let separator = PredefinedMenuItem::separator(app)?;
    let quit = MenuItem::with_id(app, QUIT, "Quit Refract", true, None::<&str>)?;
    let menu = Menu::with_items(app, &[&toggle, &autostart, &separator, &quit])?;
    app.manage(TrayMenu { toggle });

    TrayIconBuilder::with_id("main")
        .icon(Image::from_bytes(include_bytes!("../icons/32x32.png"))?)
        .tooltip("Refract")
        .menu(&menu)
        .show_menu_on_left_click(false)
        .on_menu_event(move |app, event| on_menu(app, event, &autostart))
        .on_tray_icon_event(|tray, event| {
            if let TrayIconEvent::Click {
                button: MouseButton::Left,
                button_state: MouseButtonState::Up,
                ..
            } = event
            {
                window::toggle_from_tray(tray.app_handle());
            }
        })
        .build(app)?;
    Ok(())
}

fn on_menu(app: &AppHandle, event: MenuEvent, autostart: &CheckMenuItem<Wry>) {
    match event.id().as_ref() {
        TOGGLE => {
            if window::window_visible(app) {
                window::hide(app);
            } else {
                window::show(app);
            }
        }
        AUTOSTART => set_autostart(app, autostart),
        QUIT => crate::quit(app),
        _ => {}
    }
}

/// The check item has already flipped when this runs; make the setting match it, and
/// flip it back if that fails.
fn set_autostart(app: &AppHandle, item: &CheckMenuItem<Wry>) {
    let want = item.is_checked().unwrap_or(false);
    let launcher = app.autolaunch();
    let result = if want {
        launcher.enable()
    } else {
        launcher.disable()
    };
    if let Err(err) = result {
        eprintln!(
            "refract: turning start-with-Windows {}: {err}",
            if want { "on" } else { "off" }
        );
        let actual = launcher.is_enabled().unwrap_or(!want);
        if let Err(err) = item.set_checked(actual) {
            eprintln!("refract: resetting the start-with-Windows check: {err}");
        }
    }
}

/// Keeps the toggle item's label in step with the window.
pub fn set_shown(app: &AppHandle, shown: bool) {
    if let Some(menu) = app.try_state::<TrayMenu>() {
        if let Err(err) = menu.toggle.set_text(toggle_label(shown)) {
            eprintln!("refract: updating the tray menu: {err}");
        }
    }
}
