//! Refract's Windows app: tray icon, hotkey and window, plus the WSL helper behind
//! the interface.

mod agent;
mod alerts;
mod creds;
mod gpu;
mod hotkey;
mod launch;
mod logfile;
mod migrate;
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

/// Passed by the start-with-Windows entry, so a launch at sign-in stays in the tray.
/// Any other launch (a shortcut, the exe) opens the window.
const HIDDEN_ARG: &str = "--hidden";

/// Whether this launch starts in the tray without showing the window.
fn starts_hidden(mut args: impl Iterator<Item = String>) -> bool {
    args.any(|arg| arg == HIDDEN_ARG)
}

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
            Some(vec![HIDDEN_ARG]),
        ))
        .plugin(hotkey::plugin())
        .plugin(tauri_plugin_notification::init())
        .manage(remote::Remote::new())
        .manage(migrate::Migrations::default())
        .setup(|app| {
            let handle = app.handle();
            // First, so everything after it is logged.
            logfile::init(handle);
            window::init(handle);
            if !starts_hidden(std::env::args().skip(1)) {
                window::show(handle);
            }
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
            launch::start_refactor,
            launch::resume_refactor,
            launch::open_refactor,
            alerts::set_zoomed,
            migrate::migrate_preflight,
            migrate::migrate_start,
            migrate::migrate_cancel_snapshot,
            migrate::migrate_cancel,
            migrate::migrate_windows,
            migrate::migrate_close_window,
            migrate::migrate_stop_agent,
            migrate::migrate_resync,
            migrate::migrate_delete,
            migrate::migrate_finish,
            migrate::migrate_pending,
            migrate::migrate_source_exists,
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
            repos::merge_refactor,
            repos::discard_refactor,
            repos::remove_worktree,
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

#[cfg(test)]
mod tests {
    use super::*;

    fn args(list: &[&str]) -> impl Iterator<Item = String> {
        list.iter()
            .map(|arg| (*arg).to_owned())
            .collect::<Vec<_>>()
            .into_iter()
    }

    #[test]
    fn only_the_sign_in_launch_starts_hidden() {
        assert!(starts_hidden(args(&["--hidden"])));
        assert!(!starts_hidden(args(&[])));
        assert!(!starts_hidden(args(&["--hiddenx"])));
    }
}
