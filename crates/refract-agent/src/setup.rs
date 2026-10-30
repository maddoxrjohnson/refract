//! `setupServer`: steps 3 and 4 of the first-run setup in `docs/launch.md`. It installs
//! the Refract extension in the WSL VS Code server, under `~/.vscode-server`, and
//! applies it to every profile. Installing runs `code-server`, which takes seconds, so
//! the server hands each [`Job`] to a worker thread.

use std::io;
use std::path::{Path, PathBuf};

use refract_core::protocol::{code, Id, Reply, Response, ServerSetup};
use refract_launch::setup::{
    ensure_application_scoped, ensure_vsix, server_cli, Processes, REFRACT_EXTENSION,
};

/// One `setupServer` request, with `vsix` already a Linux path.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Job {
    pub id: Id,
    pub vsix: PathBuf,
    /// The Windows VS Code's commit; `None` uses the newest server.
    pub commit: Option<String>,
}

/// Runs `job` against the server in `$HOME` and returns its response.
pub fn run(job: Job) -> Response {
    let Some(home) = std::env::var_os("HOME") else {
        return Response::err(job.id, code::SETUP_FAILED, "HOME is not set");
    };
    match setup_server(Path::new(&home), &job.vsix, job.commit.as_deref()) {
        Ok(done) => Response::ok(job.id, Reply::ServerSetup(done)),
        Err(message) => Response::err(job.id, code::SETUP_FAILED, message),
    }
}

/// Installs `vsix` in the server in `home`'s `.vscode-server` unless that version is
/// installed, then marks the extension application-scoped. A missing vsix or server is
/// reported in `skipped`, so the launch can go on without them. The error is a message
/// for the app.
pub fn setup_server(home: &Path, vsix: &Path, commit: Option<&str>) -> Result<ServerSetup, String> {
    let skipped = |reason: String| ServerSetup {
        installed: false,
        application_scoped: false,
        skipped: Some(reason),
    };
    if !vsix.is_file() {
        return Ok(skipped(format!("no vsix at {}", vsix.display())));
    }
    let server = home.join(".vscode-server");
    let code_server = match server_cli(&server, commit) {
        Ok(path) => path,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(skipped(e.to_string())),
        Err(e) => return Err(format!("cannot find the VS Code server: {e}")),
    };
    let registry = server.join("extensions/extensions.json");
    let installed = ensure_vsix(
        &code_server,
        vsix,
        &registry,
        REFRACT_EXTENSION,
        &mut Processes,
    )
    .map_err(|e| format!("cannot install the extension: {e}"))?;
    let application_scoped = ensure_application_scoped(&registry, REFRACT_EXTENSION)
        .map_err(|e| format!("cannot update {}: {e}", registry.display()))?;
    Ok(ServerSetup {
        installed,
        application_scoped,
        skipped: None,
    })
}
