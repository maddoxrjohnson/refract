//! The status hook and the usage widget: commands that ask the helper (`hookInfo`,
//! `installHook`, `uninstallHook`, `usage`) and hand its answer to the interface as
//! the helper sent it (`HookInfo` or `UsageSummary` JSON).

use std::time::Duration;

use refract_client::RequestError;
use refract_core::protocol::{code, Method};
use serde_json::Value;
use tauri::{AppHandle, Manager};

use crate::agent::Agent;

/// How long the hook commands wait: the helper reads and writes `settings.json`.
const HOOK_TIMEOUT: Duration = Duration::from_secs(10);
/// How long `usage` waits. The helper answers from memory and the widget asks again
/// in two seconds, so a slow answer is not worth waiting long for.
const USAGE_TIMEOUT: Duration = Duration::from_secs(5);

fn internal(message: String) -> RequestError {
    RequestError::Agent {
        code: code::INTERNAL_ERROR,
        message,
    }
}

/// Sends `method` to the helper off the main thread and returns its result as JSON.
async fn ask(app: AppHandle, method: Method, timeout: Duration) -> Result<Value, RequestError> {
    let reply =
        tauri::async_runtime::spawn_blocking(move || app.state::<Agent>().request(method, timeout))
            .await
            .map_err(|err| internal(format!("request task failed: {err}")))??;
    serde_json::to_value(reply).map_err(|err| internal(err.to_string()))
}

/// Whether the hook is installed, and the diff installing it would make.
#[tauri::command]
pub async fn hook_info(app: AppHandle) -> Result<Value, RequestError> {
    ask(app, Method::HookInfo, HOOK_TIMEOUT).await
}

#[tauri::command]
pub async fn install_hook(app: AppHandle) -> Result<Value, RequestError> {
    ask(app, Method::InstallHook, HOOK_TIMEOUT).await
}

#[tauri::command]
pub async fn uninstall_hook(app: AppHandle) -> Result<Value, RequestError> {
    ask(app, Method::UninstallHook, HOOK_TIMEOUT).await
}

/// Token totals for the usage widget.
#[tauri::command]
pub async fn usage(app: AppHandle) -> Result<Value, RequestError> {
    ask(app, Method::Usage, USAGE_TIMEOUT).await
}
