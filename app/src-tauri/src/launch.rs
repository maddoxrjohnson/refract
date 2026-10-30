//! Opens a repo in VS Code from Windows: the first-run setup, a ticket from the
//! helper, then `code` on the ticket's folder (see `docs/launch.md`).

use std::io;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::{Mutex, PoisonError};
use std::time::{Duration, Instant};

use refract_client::RequestError;
use refract_core::protocol::{code, LaunchMode, LaunchTicket, Method, Reply};
use refract_launch::setup::{
    ensure_profile, windows_code_commit, CliCommand, CodeCli, RunOutput, Runner, WindowCloser,
    WindowMatch,
};
use refract_launch::windows::{
    close_window_command, closed_windows, WINDOWS_SCRIPT, WINDOWS_SCRIPT_NAME,
};
use refract_launch::{ensure_profile_settings, vscode_args, PROFILE};
use serde::Serialize;
use tauri::path::BaseDirectory;
use tauri::{AppHandle, Emitter, Manager};

use crate::agent::{neutral_dir, no_window, Agent};
use crate::process::output_with_deadline;

/// Emitted with each [`Step`] of a launch.
pub const PROGRESS_EVENT: &str = "launch://progress";

/// How long the helper gets to install the extension in the WSL server.
const SERVER_SETUP_TIMEOUT: Duration = Duration::from_secs(120);
/// How long the helper gets to write the request file.
const PREPARE_TIMEOUT: Duration = Duration::from_secs(10);
/// How long to wait for VS Code to record the profile before adding its settings.
const SETTINGS_WAIT: Duration = Duration::from_secs(15);
const SETTINGS_POLL: Duration = Duration::from_millis(500);

/// How long a `code` command that installs an extension gets.
const INSTALL_TIMEOUT: Duration = Duration::from_secs(60);
/// How long a `code` command that opens a window gets. A cold VS Code start can be slow.
const OPEN_TIMEOUT: Duration = Duration::from_secs(90);
/// How long any other command gets.
const COMMAND_TIMEOUT: Duration = Duration::from_secs(30);

/// Where the VS Code user installer puts `code.cmd`, under `%LOCALAPPDATA%`.
const USER_INSTALL_CODE: &str = r"Programs\Microsoft VS Code\bin\code.cmd";

/// Setup is not re-entrant, so launches run one at a time; a second one waits.
static LAUNCHES: Mutex<()> = Mutex::new(());

/// What a launch is doing, sent as `{"step": "setup"}`. The first run can take
/// twenty seconds or so.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub enum Step {
    /// The Refract profile and its WSL extension.
    Setup,
    /// The Refract extension in the WSL server.
    Server,
    /// Asking the helper for a ticket.
    Prepare,
    /// Running `code`.
    Opening,
    Done,
}

#[derive(Debug, Clone, Serialize)]
struct Progress {
    step: Step,
}

/// Why a launch failed, sent as `{"kind": "setup", "message": "..."}`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct LaunchError {
    pub kind: LaunchErrorKind,
    pub message: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub enum LaunchErrorKind {
    /// No `code.cmd` was found.
    VscodeNotFound,
    /// The first-run setup failed.
    Setup,
    /// The helper failed or did not answer.
    Agent,
    /// `code` failed to open the folder.
    Vscode,
    /// The launch task itself failed.
    Internal,
}

impl LaunchError {
    fn new(kind: LaunchErrorKind, message: impl Into<String>) -> LaunchError {
        LaunchError {
            kind,
            message: message.into(),
        }
    }

    /// A failed helper request, made while `doing` something.
    fn agent(doing: &str, err: RequestError) -> LaunchError {
        LaunchError::new(LaunchErrorKind::Agent, format!("{doing}: {err}"))
    }

    /// A failed `prepareLaunch`. The helper's message for an agent that stopped
    /// running is meant to be shown as it is.
    fn prepare(err: RequestError) -> LaunchError {
        match err {
            RequestError::Agent {
                code: code::NOT_RUNNING,
                message,
            } => LaunchError::new(
                LaunchErrorKind::Agent,
                format!("Cannot go to the running agent: {message}."),
            ),
            err => LaunchError::agent("Cannot prepare the launch", err),
        }
    }
}

/// The directories in a Windows `PATH` value, which is split on `;` and may quote
/// entries.
pub fn path_dirs(path: &str) -> Vec<PathBuf> {
    path.split(';')
        .map(|dir| dir.trim().trim_matches('"'))
        .filter(|dir| !dir.is_empty())
        .map(PathBuf::from)
        .collect()
}

/// VS Code's `code.cmd`: the user install under `local_app_data`, else the first one
/// on `path`. `is_file` is the file system check.
pub fn find_code_cmd(
    local_app_data: Option<&Path>,
    path: Option<&str>,
    is_file: impl Fn(&Path) -> bool,
) -> Option<PathBuf> {
    let user_install = local_app_data.map(|dir| dir.join(USER_INSTALL_CODE));
    let on_path = path
        .map(path_dirs)
        .unwrap_or_default()
        .into_iter()
        .map(|dir| dir.join("code.cmd"));
    user_install
        .into_iter()
        .chain(on_path)
        .find(|candidate| is_file(candidate))
}

/// The vsix to install in the WSL server: `REFRACT_VSIX` when set (in development, a
/// Linux path to the one built in the repo), else the bundled one if it is there, in
/// the plain form [`simplify_verbatim`] gives.
pub fn choose_vsix(from_env: Option<String>, bundled: Option<PathBuf>) -> Option<String> {
    from_env
        .filter(|vsix| !vsix.is_empty())
        .or_else(|| bundled.map(|path| simplify_verbatim(&path.to_string_lossy())))
}

/// Turns a verbatim path, which Tauri's resource paths are (they come from
/// `canonicalize`), into the ordinary form: `\\?\C:\x` becomes `C:\x` and
/// `\\?\UNC\server\share` becomes `\\server\share`. Other paths, including verbatim
/// ones with no plain equivalent such as `\\?\Volume{..}`, are returned unchanged.
pub fn simplify_verbatim(path: &str) -> String {
    if let Some(rest) = path.strip_prefix(r"\\?\UNC\") {
        return format!(r"\\{rest}");
    }
    if let Some(rest) = path.strip_prefix(r"\\?\") {
        let bytes = rest.as_bytes();
        let is_drive = bytes.len() >= 2 && bytes[0].is_ascii_alphabetic() && bytes[1] == b':';
        if is_drive && (bytes.len() == 2 || bytes[2] == b'\\') {
            return rest.to_owned();
        }
    }
    path.to_owned()
}

/// How long `args` get to run: see [`INSTALL_TIMEOUT`] and [`OPEN_TIMEOUT`].
fn command_timeout(args: &[String]) -> Duration {
    let has = |flag: &str| args.iter().any(|arg| arg == flag);
    if has("--folder-uri") || has("--new-window") {
        OPEN_TIMEOUT
    } else if has("--install-extension") {
        INSTALL_TIMEOUT
    } else {
        COMMAND_TIMEOUT
    }
}

/// What a failed command said: its standard error, else its standard output.
fn failure_output(out: &RunOutput) -> String {
    let text = if out.stderr.trim().is_empty() {
        out.stdout.trim()
    } else {
        out.stderr.trim()
    };
    if text.is_empty() {
        "no output".into()
    } else {
        text.into()
    }
}

/// Runs commands without a console window, with no stdin, in [`neutral_dir`] unless
/// the command names a directory, killing any that outlive [`command_timeout`].
/// `.cmd` files go through `cmd.exe`; Rust's standard library quotes the arguments
/// for it and refuses any it cannot pass safely.
struct Processes;

impl Runner for Processes {
    fn run(&mut self, command: &CliCommand) -> io::Result<RunOutput> {
        let mut process = Command::new(&command.program);
        process
            .args(&command.args)
            .current_dir(command.cwd.clone().unwrap_or_else(neutral_dir))
            .stdin(Stdio::null());
        let out = output_with_deadline(no_window(&mut process), command_timeout(&command.args))
            .map_err(|err| {
                let name = Path::new(&command.program).file_name().map_or_else(
                    || command.program.display().to_string(),
                    |n| n.to_string_lossy().into_owned(),
                );
                io::Error::new(
                    err.kind(),
                    format!("{name} {}: {err}", command.args.join(" ")),
                )
            })?;
        Ok(RunOutput {
            success: out.status.success(),
            stdout: String::from_utf8_lossy(&out.stdout).into_owned(),
            stderr: String::from_utf8_lossy(&out.stderr).into_owned(),
        })
    }
}

/// Closes VS Code windows with [`WINDOWS_SCRIPT`], run by Windows PowerShell from a
/// copy in `%TEMP%`.
struct PowerShell;

impl WindowCloser for PowerShell {
    fn close(&mut self, window: &WindowMatch) -> io::Result<()> {
        let script = std::env::temp_dir().join(WINDOWS_SCRIPT_NAME);
        std::fs::write(&script, WINDOWS_SCRIPT)?;
        let command = close_window_command(
            "powershell.exe",
            &script.to_string_lossy(),
            window,
            Some(neutral_dir()),
        );
        let out = Processes.run(&command)?;
        if !out.success {
            return Err(io::Error::other(format!(
                "powershell.exe failed: {}",
                failure_output(&out)
            )));
        }
        for line in closed_windows(&out.stdout) {
            eprintln!("refract: {line}");
        }
        Ok(())
    }
}

/// Opens `path` in VS Code as `mode` says and returns the helper's ticket.
#[tauri::command]
pub async fn launch(
    app: AppHandle,
    path: String,
    mode: LaunchMode,
) -> Result<LaunchTicket, LaunchError> {
    tauri::async_runtime::spawn_blocking(move || {
        let _one_at_a_time = LAUNCHES.lock().unwrap_or_else(PoisonError::into_inner);
        let result = run(&app, &path, &mode);
        if let Err(err) = &result {
            eprintln!("refract: launching {path} failed: {}", err.message);
        }
        result
    })
    .await
    .map_err(|err| {
        LaunchError::new(
            LaunchErrorKind::Internal,
            format!("The launch task failed: {err}"),
        )
    })?
}

fn progress(app: &AppHandle, step: Step) {
    if let Err(err) = app.emit(PROGRESS_EVENT, Progress { step }) {
        eprintln!("refract: sending launch progress: {err}");
    }
}

fn run(app: &AppHandle, path: &str, mode: &LaunchMode) -> Result<LaunchTicket, LaunchError> {
    let agent = app.state::<Agent>();

    progress(app, Step::Setup);
    let local_app_data = std::env::var_os("LOCALAPPDATA").map(PathBuf::from);
    let path_var = std::env::var("PATH").ok();
    let code = find_code_cmd(
        local_app_data.as_deref(),
        path_var.as_deref(),
        Path::is_file,
    )
    .ok_or_else(|| {
        LaunchError::new(
            LaunchErrorKind::VscodeNotFound,
            "VS Code not found. Install it from https://code.visualstudio.com, or make \
                 sure code.cmd is on your PATH.",
        )
    })?;
    let cli = CodeCli::Direct { program: code };
    let user_dir = std::env::var_os("APPDATA")
        .map(|dir| Path::new(&dir).join(r"Code\User"))
        .ok_or_else(|| {
            LaunchError::new(
                LaunchErrorKind::Setup,
                "Cannot find VS Code's settings: %APPDATA% is not set.",
            )
        })?;
    let done = ensure_profile(
        &cli,
        PROFILE,
        &user_dir,
        &mut Processes,
        &mut PowerShell,
        &mut std::thread::sleep,
    )
    .map_err(|err| {
        LaunchError::new(
            LaunchErrorKind::Setup,
            format!("Cannot set up the {PROFILE} profile in VS Code: {err}"),
        )
    })?;
    if done.created {
        eprintln!("refract: created the {PROFILE} profile");
    }
    if done.installed_wsl {
        eprintln!("refract: installed the WSL extension in the {PROFILE} profile");
    }

    progress(app, Step::Server);
    let bundled = app
        .path()
        .resolve("refract.vsix", BaseDirectory::Resource)
        .ok()
        .filter(|path| path.is_file());
    match choose_vsix(std::env::var("REFRACT_VSIX").ok(), bundled) {
        Some(vsix) => {
            let commit = windows_code_commit(&cli, &mut Processes);
            setup_server(&agent, vsix, commit)?;
        }
        None => eprintln!(
            "refract: warning: no refract.vsix bundled and REFRACT_VSIX is not set; \
             skipping the extension install"
        ),
    }

    progress(app, Step::Prepare);
    let method = Method::PrepareLaunch {
        path: path.to_owned(),
        mode: mode.clone(),
    };
    let ticket = match agent.request(method, PREPARE_TIMEOUT) {
        Ok(Reply::Launch(ticket)) => ticket,
        Ok(other) => {
            return Err(LaunchError::new(
                LaunchErrorKind::Agent,
                format!("The helper sent an unexpected answer to prepareLaunch: {other:?}"),
            ))
        }
        Err(err) => return Err(LaunchError::prepare(err)),
    };

    progress(app, Step::Opening);
    let args = vscode_args(PROFILE, &ticket.folder_uri);
    let args: Vec<&str> = args.iter().map(String::as_str).collect();
    let out = Processes.run(&cli.command(&args)).map_err(|err| {
        LaunchError::new(
            LaunchErrorKind::Vscode,
            format!("Cannot run VS Code: {err}"),
        )
    })?;
    if !out.success {
        return Err(LaunchError::new(
            LaunchErrorKind::Vscode,
            format!(
                "VS Code could not open the folder: {}",
                failure_output(&out)
            ),
        ));
    }

    std::thread::spawn(move || add_profile_settings(&user_dir));
    progress(app, Step::Done);
    Ok(ticket)
}

/// Steps 3 and 4 of the first-run setup, done by the helper. A `skipped` result is
/// logged; the window can still open.
fn setup_server(agent: &Agent, vsix: String, commit: Option<String>) -> Result<(), LaunchError> {
    match agent.request(Method::SetupServer { vsix, commit }, SERVER_SETUP_TIMEOUT) {
        Ok(Reply::ServerSetup(done)) => {
            if let Some(reason) = done.skipped {
                eprintln!("refract: skipping the extension install: {reason}");
            }
            if done.installed {
                eprintln!("refract: installed the Refract extension in the WSL server");
            }
            if done.application_scoped {
                eprintln!("refract: applied the Refract extension to all profiles");
            }
            Ok(())
        }
        Ok(other) => Err(LaunchError::new(
            LaunchErrorKind::Agent,
            format!("The helper sent an unexpected answer to setupServer: {other:?}"),
        )),
        Err(err) => Err(LaunchError::agent(
            "Cannot install the Refract extension in WSL",
            err,
        )),
    }
}

/// Adds the bare layout to the Refract profile once VS Code has recorded it, checking
/// every [`SETTINGS_POLL`] for up to [`SETTINGS_WAIT`].
fn add_profile_settings(user_dir: &Path) {
    let deadline = Instant::now() + SETTINGS_WAIT;
    loop {
        match ensure_profile_settings(user_dir, PROFILE) {
            Ok(true) => {
                eprintln!("refract: {PROFILE} profile settings are in place");
                return;
            }
            Ok(false) if Instant::now() < deadline => std::thread::sleep(SETTINGS_POLL),
            Ok(false) => {
                eprintln!(
                    "refract: no {PROFILE} profile in {} after {}s; its settings were not added",
                    user_dir.display(),
                    SETTINGS_WAIT.as_secs()
                );
                return;
            }
            Err(err) => {
                eprintln!("refract: cannot update the {PROFILE} profile settings: {err}");
                return;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    /// An `is_file` that answers true for `files` only.
    fn in_set(files: &[PathBuf]) -> impl Fn(&Path) -> bool + '_ {
        move |path| files.iter().any(|f| f.as_path() == path)
    }

    #[test]
    fn path_is_split_like_windows() {
        assert_eq!(
            path_dirs(r#"C:\Windows;;"C:\Program Files\Tool";  C:\bin ;"#),
            [
                PathBuf::from(r"C:\Windows"),
                PathBuf::from(r"C:\Program Files\Tool"),
                PathBuf::from(r"C:\bin"),
            ]
        );
        assert!(path_dirs("").is_empty());
    }

    #[test]
    fn code_cmd_prefers_the_user_install() {
        let local = Path::new(r"C:\Users\dev\AppData\Local");
        let user_install = local.join(USER_INSTALL_CODE);
        let system = Path::new(r"C:\Program Files\Microsoft VS Code\bin").join("code.cmd");
        let path = r"C:\Windows;C:\Program Files\Microsoft VS Code\bin";
        let both = [user_install.clone(), system.clone()];
        assert_eq!(
            find_code_cmd(Some(local), Some(path), in_set(&both)),
            Some(user_install)
        );
        let system_only = [system.clone()];
        assert_eq!(
            find_code_cmd(Some(local), Some(path), in_set(&system_only)),
            Some(system.clone())
        );
        assert_eq!(
            find_code_cmd(None, Some(path), in_set(&system_only)),
            Some(system)
        );
        assert_eq!(find_code_cmd(Some(local), Some(path), in_set(&[])), None);
        assert_eq!(find_code_cmd(None, None, |_| true), None);
    }

    #[test]
    fn vsix_from_the_environment_wins() {
        let bundled = PathBuf::from(r"C:\Program Files\Refract\refract.vsix");
        assert_eq!(
            choose_vsix(
                Some("/home/dev/refract/vscode-extension/refract.vsix".into()),
                Some(bundled.clone())
            ),
            Some("/home/dev/refract/vscode-extension/refract.vsix".into())
        );
        assert_eq!(
            choose_vsix(Some(String::new()), Some(bundled.clone())),
            Some(bundled.to_string_lossy().into_owned())
        );
        assert_eq!(choose_vsix(None, None), None);
    }

    #[test]
    fn bundled_vsix_loses_the_verbatim_prefix() {
        assert_eq!(
            choose_vsix(
                None,
                Some(PathBuf::from(r"\\?\C:\Program Files\Refract\refract.vsix"))
            ),
            Some(r"C:\Program Files\Refract\refract.vsix".into())
        );
        assert_eq!(
            simplify_verbatim(r"\\?\UNC\server\share\refract.vsix"),
            r"\\server\share\refract.vsix"
        );
        assert_eq!(simplify_verbatim(r"\\?\D:"), "D:");
        assert_eq!(
            simplify_verbatim(r"C:\x\refract.vsix"),
            r"C:\x\refract.vsix"
        );
        assert_eq!(
            simplify_verbatim(r"\\server\share\refract.vsix"),
            r"\\server\share\refract.vsix"
        );
        // No plain form: left alone.
        assert_eq!(
            simplify_verbatim(r"\\?\Volume{1234}\refract.vsix"),
            r"\\?\Volume{1234}\refract.vsix"
        );
        assert_eq!(simplify_verbatim(r"\\?\C"), r"\\?\C");
    }

    #[test]
    fn commands_that_open_windows_get_longest() {
        let args = |list: &[&str]| list.iter().map(|a| a.to_string()).collect::<Vec<_>>();
        assert_eq!(
            command_timeout(&args(&["--profile", "Refract", "--folder-uri", "x"])),
            OPEN_TIMEOUT
        );
        assert_eq!(
            command_timeout(&args(&["--profile", "Refract", "--new-window"])),
            OPEN_TIMEOUT
        );
        assert_eq!(
            command_timeout(&args(&["--install-extension", "x", "--force"])),
            INSTALL_TIMEOUT
        );
        assert_eq!(command_timeout(&args(&["--version"])), COMMAND_TIMEOUT);
    }

    #[test]
    fn failures_report_what_the_command_said() {
        let out = |stdout: &str, stderr: &str| RunOutput {
            success: false,
            stdout: stdout.into(),
            stderr: stderr.into(),
        };
        assert_eq!(failure_output(&out("ignored", " boom\r\n")), "boom");
        assert_eq!(failure_output(&out("said this\n", "  ")), "said this");
        assert_eq!(failure_output(&out("", "")), "no output");
    }

    #[test]
    fn errors_and_progress_have_the_wire_shape() {
        let err = LaunchError::agent("Cannot prepare the launch", RequestError::Timeout);
        assert_eq!(
            serde_json::to_value(&err).unwrap(),
            json!({
                "kind": "agent",
                "message": "Cannot prepare the launch: the helper did not answer in time"
            })
        );
        let err = LaunchError::new(LaunchErrorKind::VscodeNotFound, "VS Code not found.");
        assert_eq!(
            serde_json::to_value(&err).unwrap()["kind"],
            json!("vscodeNotFound")
        );
        assert_eq!(
            serde_json::to_value(Progress {
                step: Step::Opening
            })
            .unwrap(),
            json!({"step": "opening"})
        );
    }

    #[test]
    fn launch_modes_arrive_in_the_wire_shape() {
        let new: LaunchMode =
            serde_json::from_value(json!({"kind": "new", "prompt": "hi"})).unwrap();
        assert_eq!(
            new,
            LaunchMode::New {
                prompt: Some("hi".into())
            }
        );
        let resume: LaunchMode =
            serde_json::from_value(json!({"kind": "resume", "sessionId": "abc"})).unwrap();
        assert_eq!(
            resume,
            LaunchMode::Resume {
                session_id: "abc".into()
            }
        );
        let focus: LaunchMode =
            serde_json::from_value(json!({"kind": "focus", "sessionId": "abc"})).unwrap();
        assert_eq!(
            focus,
            LaunchMode::Focus {
                session_id: "abc".into()
            }
        );
    }

    #[test]
    fn an_agent_that_stopped_running_is_reported_plainly() {
        let err = LaunchError::prepare(RequestError::Agent {
            code: code::NOT_RUNNING,
            message: "that agent is no longer running".into(),
        });
        assert_eq!(
            err.message,
            "Cannot go to the running agent: that agent is no longer running."
        );
        let err = LaunchError::prepare(RequestError::Agent {
            code: code::INVALID_PARAMS,
            message: "bad".into(),
        });
        assert_eq!(
            err.message,
            "Cannot prepare the launch: the helper answered with error -32602: bad"
        );
    }
}
