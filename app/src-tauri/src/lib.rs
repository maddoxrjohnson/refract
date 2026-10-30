//! Refract's Windows app: tray icon, hotkey and window, plus the WSL helper behind
//! the interface.

mod agent;
mod creds;
mod gpu;
mod hotkey;
mod launch;
mod logfile;
mod open;
mod process;
mod remote;
mod repos;
mod settings;
mod stats;
mod tray;
mod widgets;
mod window;

use tauri::{AppHandle, Manager, RunEvent, WindowEvent};
use tauri_plugin_autostart::MacosLauncher;

/// Stops the helper, then exits. Used by the tray's Quit item.
pub fn quit(app: &AppHandle) {
    agent::shutdown(app);
    app.exit(0);
}

pub fn run() {
    let app = tauri::Builder::default()
        // Must come first: a second launch shows this instance's window and exits.
        .plugin(tauri_plugin_single_instance::init(|app, _argv, _cwd| {
            window::show(app);
        }))
        // Off until the user turns it on from the tray menu.
        .plugin(tauri_plugin_autostart::init(
            MacosLauncher::LaunchAgent,
            None,
        ))
        .plugin(hotkey::plugin())
        .manage(remote::Remote::new())
        .setup(|app| {
            let handle = app.handle();
            // First, so everything after it is logged.
            logfile::init(handle);
            window::init(handle);
            tray::create(handle)?;
            hotkey::register(handle);
            agent::start(handle);
            Ok(())
        })
        .on_page_load(window::page_loaded)
        .on_window_event(|window, event| {
            if window.label() != window::MAIN {
                return;
            }
            match event {
                // The close button hides; only the tray's Quit exits.
                WindowEvent::CloseRequested { api, .. } => {
                    api.prevent_close();
                    window::hide(window.app_handle());
                }
                // Catches minimize and restore, which change what is visible.
                WindowEvent::Resized(_) => window::sync_visibility(window.app_handle()),
                WindowEvent::Focused(focused) => window::focus_changed(*focused),
                _ => {}
            }
        })
        .invoke_handler(tauri::generate_handler![
            window::is_visible,
            agent::agent_status,
            agent::agent_request,
            launch::launch,
            open::open_folder,
            open::open_url,
            open::open_log_folder,
            remote::remote_list,
            remote::remote_details,
            remote::credentials,
            remote::set_token,
            settings::settings_get,
            settings::settings_set,
            repos::clone_repo,
            repos::add_repo,
            widgets::hook_info,
            widgets::install_hook,
            widgets::uninstall_hook,
            widgets::usage,
        ])
        .build(tauri::generate_context!())
        .expect("building the Tauri app");

    app.run(|app, event| {
        if let RunEvent::Exit = event {
            agent::shutdown(app);
            // The last lines, the helper's included, still have to reach the log.
            logfile::flush();
        }
    });
}
