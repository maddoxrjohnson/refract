//! Showing, hiding and toggling the main window, and telling the interface about it.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Mutex;
use std::time::{Duration, Instant};

use serde::Serialize;
use tauri::webview::{PageLoadEvent, PageLoadPayload};
use tauri::{AppHandle, Emitter, Manager, Webview, WebviewWindow, Wry};

/// The main window's label in `tauri.conf.json`.
pub const MAIN: &str = "main";

/// Emitted with [`Visibility`] on every show and hide.
pub const VISIBILITY_EVENT: &str = "refract://visibility";

#[derive(Debug, Clone, Copy, Serialize)]
pub struct Visibility {
    pub visible: bool,
}

/// What a toggle should do, given the window's current state.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Toggle {
    Hide,
    Focus,
    Show,
}

/// Visible and focused: hide. Visible behind other windows: bring it forward.
/// Hidden or minimized: show it.
pub fn toggle_action(visible: bool, minimized: bool, focused: bool) -> Toggle {
    match (visible && !minimized, focused) {
        (true, true) => Toggle::Hide,
        (true, false) => Toggle::Focus,
        (false, _) => Toggle::Show,
    }
}

fn main_window(app: &AppHandle) -> Option<WebviewWindow<Wry>> {
    let window = app.get_webview_window(MAIN);
    if window.is_none() {
        eprintln!("refract: main window not found");
    }
    window
}

/// Shows, unminimizes and focuses the main window.
pub fn show(app: &AppHandle) {
    let Some(window) = main_window(app) else {
        return;
    };
    for err in [window.show(), window.unminimize()]
        .into_iter()
        .filter_map(Result::err)
    {
        eprintln!("refract: showing the window: {err}");
    }
    // Shows the webview before focusing, so focus reaches it.
    sync_visibility(app);
    if let Err(err) = window.set_focus() {
        eprintln!("refract: focusing the window: {err}");
    }
}

/// Hides the main window. The app keeps running in the tray.
pub fn hide(app: &AppHandle) {
    let Some(window) = main_window(app) else {
        return;
    };
    if let Err(err) = window.hide() {
        eprintln!("refract: hiding the window: {err}");
    }
    sync_visibility(app);
}

/// The tray click and the hotkey: see [`toggle_action`].
pub fn toggle(app: &AppHandle) {
    toggle_inner(app, false);
}

/// Clicking the tray activates the taskbar, so the window has already lost focus when
/// the click arrives. A window that lost focus this recently counts as focused.
const TRAY_BLUR_GRACE: Duration = Duration::from_millis(500);

/// When the main window last lost focus.
static LAST_BLUR: Mutex<Option<Instant>> = Mutex::new(None);

/// Records focus changes of the main window, for [`toggle_from_tray`].
pub fn focus_changed(focused: bool) {
    if let Ok(mut last) = LAST_BLUR.lock() {
        *last = if focused { None } else { Some(Instant::now()) };
    }
}

/// The tray icon's left click: [`toggle`], allowing for the focus the click took.
pub fn toggle_from_tray(app: &AppHandle) {
    toggle_inner(app, true);
}

fn toggle_inner(app: &AppHandle, from_tray: bool) {
    let Some(window) = main_window(app) else {
        return;
    };
    let just_blurred = from_tray
        && LAST_BLUR
            .lock()
            .ok()
            .and_then(|last| *last)
            .is_some_and(|at| at.elapsed() < TRAY_BLUR_GRACE);
    let action = toggle_action(
        window.is_visible().unwrap_or(false),
        window.is_minimized().unwrap_or(false),
        window.is_focused().unwrap_or(false) || just_blurred,
    );
    match action {
        Toggle::Hide => hide(app),
        Toggle::Focus | Toggle::Show => show(app),
    }
}

/// Whether the main window is shown and not minimized.
pub fn window_visible(app: &AppHandle) -> bool {
    app.get_webview_window(MAIN)
        .is_some_and(|w| w.is_visible().unwrap_or(false) && !w.is_minimized().unwrap_or(false))
}

/// Whether the main window is shown, not minimized and focused: in front of the user,
/// not behind VS Code.
pub fn window_in_front(app: &AppHandle) -> bool {
    window_visible(app)
        && app
            .get_webview_window(MAIN)
            .is_some_and(|w| w.is_focused().unwrap_or(false))
}

/// The visibility last reported to the interface. The window starts hidden.
static REPORTED: AtomicBool = AtomicBool::new(false);

/// Reports the window's visibility if it changed since the last report: emits
/// [`VISIBILITY_EVENT`], updates the tray menu, tells WebView2 whether it is shown and
/// starts or stops the GPU sampler.
/// Called after show and hide, and when the window is minimized or restored.
pub fn sync_visibility(app: &AppHandle) {
    let visible = window_visible(app);
    if REPORTED.swap(visible, Ordering::SeqCst) == visible {
        return;
    }
    set_webview_visible(app, visible);
    crate::tray::set_shown(app, visible);
    crate::gpu::set_sampling(app, visible);
    if let Err(err) = app.emit(VISIBILITY_EVENT, Visibility { visible }) {
        eprintln!("refract: emitting {VISIBILITY_EVENT}: {err}");
    }
}

/// Tells WebView2 itself whether it is shown (`ICoreWebView2Controller::SetIsVisible`
/// through wry). Hiding the window alone leaves WebView2 believing it is visible; a
/// hidden WebView2 stops painting and the page sees `document.hidden`, which pauses
/// `requestAnimationFrame`. A hidden webview is also suspended to save memory, and
/// resumed before it is shown again.
fn set_webview_visible(app: &AppHandle, visible: bool) {
    let Some(window) = app.get_webview_window(MAIN) else {
        return;
    };
    let webview: &Webview<Wry> = window.as_ref();
    let result = if visible {
        set_suspended(webview, false);
        webview.show()
    } else {
        let hidden = webview.hide();
        // `TrySuspend` needs the webview hidden first; both run in order on the main
        // thread.
        set_suspended(webview, true);
        hidden
    };
    if let Err(err) = result {
        eprintln!("refract: setting the webview's visibility: {err}");
    }
}

/// The window starts hidden but its webview starts visible; hide the webview too.
/// It is suspended once its page has loaded, by [`page_loaded`].
pub fn init(app: &AppHandle) {
    if !window_visible(app) {
        set_webview_visible(app, false);
    }
}

/// Suspends the main webview once its page has loaded, if the window is still hidden.
/// A webview cannot be suspended before its first page has loaded.
pub fn page_loaded(webview: &Webview<Wry>, payload: &PageLoadPayload<'_>) {
    if webview.label() == MAIN
        && payload.event() == PageLoadEvent::Finished
        && !window_visible(webview.app_handle())
    {
        set_suspended(webview, true);
    }
}

/// Suspends a hidden webview (`ICoreWebView2_3::TrySuspend`) with a low memory target
/// (`ICoreWebView2_19`), which lets WebView2 drop most of its renderer memory, or
/// resumes it with the normal target. An interface the installed WebView2 runtime
/// lacks is skipped with a log line.
#[cfg(windows)]
fn set_suspended(webview: &Webview<Wry>, suspend: bool) {
    let result = webview.with_webview(move |platform| {
        use webview2_com::Microsoft::Web::WebView2::Win32::{
            ICoreWebView2_19, ICoreWebView2_3, COREWEBVIEW2_MEMORY_USAGE_TARGET_LEVEL_LOW,
            COREWEBVIEW2_MEMORY_USAGE_TARGET_LEVEL_NORMAL,
        };
        use webview2_com::TrySuspendCompletedHandler;
        use windows::core::Interface;

        // SAFETY: plain COM calls on the main thread, where `with_webview` runs.
        let core = match unsafe { platform.controller().CoreWebView2() } {
            Ok(core) => core,
            Err(err) => {
                eprintln!("refract: no WebView2 to suspend or resume: {err}");
                return;
            }
        };
        let level = if suspend {
            COREWEBVIEW2_MEMORY_USAGE_TARGET_LEVEL_LOW
        } else {
            COREWEBVIEW2_MEMORY_USAGE_TARGET_LEVEL_NORMAL
        };
        match core.cast::<ICoreWebView2_19>() {
            Ok(core) => {
                if let Err(err) = unsafe { core.SetMemoryUsageTargetLevel(level) } {
                    eprintln!("refract: setting the WebView2 memory target: {err}");
                }
            }
            Err(err) => eprintln!("refract: WebView2 has no memory target setting: {err}"),
        }
        let core = match core.cast::<ICoreWebView2_3>() {
            Ok(core) => core,
            Err(err) => {
                eprintln!("refract: WebView2 cannot be suspended: {err}");
                return;
            }
        };
        if !suspend {
            if let Err(err) = unsafe { core.Resume() } {
                eprintln!("refract: resuming WebView2: {err}");
            }
            return;
        }
        let handler = TrySuspendCompletedHandler::create(Box::new(|result, suspended| {
            match result {
                Err(err) => eprintln!("refract: suspending WebView2: {err}"),
                Ok(()) if !suspended => eprintln!("refract: WebView2 declined to suspend"),
                Ok(()) => {}
            }
            Ok(())
        }));
        if let Err(err) = unsafe { core.TrySuspend(&handler) } {
            eprintln!("refract: suspending WebView2: {err}");
        }
    });
    if let Err(err) = result {
        eprintln!("refract: reaching WebView2: {err}");
    }
}

#[cfg(not(windows))]
fn set_suspended(_webview: &Webview<Wry>, _suspend: bool) {}

/// For the interface's first render; later changes arrive as [`VISIBILITY_EVENT`].
#[tauri::command]
pub fn is_visible(app: AppHandle) -> bool {
    window_visible(&app)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn toggle_hides_only_a_focused_visible_window() {
        assert_eq!(toggle_action(true, false, true), Toggle::Hide);
        assert_eq!(toggle_action(true, false, false), Toggle::Focus);
        assert_eq!(toggle_action(false, false, false), Toggle::Show);
        assert_eq!(toggle_action(false, false, true), Toggle::Show);
        // A minimized window can still report itself visible and focused.
        assert_eq!(toggle_action(true, true, true), Toggle::Show);
    }
}
