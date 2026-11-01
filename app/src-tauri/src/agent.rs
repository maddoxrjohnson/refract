//! Runs refract-agent inside WSL and exposes it to the interface.

use std::io;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, PoisonError};
use std::time::Duration;

use crate::process::output_with_deadline;
use refract_client::{AgentClient, AgentStatus, Backoff, ClientEvent, RequestError, Spawn};
use refract_core::launch::wsl_path;
use refract_core::protocol::{self, code, Method, Reply};
use serde_json::Value;
use tauri::path::BaseDirectory;
use tauri::{AppHandle, Emitter, Manager, State};

/// Emitted with each [`AgentStatus`].
pub const STATUS_EVENT: &str = "agent://status";
/// Emitted with each helper event (`repoChanged` and so on), as the helper sent it.
pub const EVENT_EVENT: &str = "agent://event";

/// How long `agent_request` waits for an answer.
const REQUEST_TIMEOUT: Duration = Duration::from_secs(10);
/// How long the helper gets to exit on its own at quit.
const SHUTDOWN_GRACE: Duration = Duration::from_secs(2);
/// How long `wsl.exe --list` gets. It can hang while WSL itself starts up.
const LIST_TIMEOUT: Duration = Duration::from_secs(10);

/// The methods the interface may call through `agent_request`. Launching and setup go
/// through their own commands, which check what they send.
const INTERFACE_METHODS: &[&str] = &["ping", "listRepos", "getRepo"];

/// Where the app installs the helper, used when `REFRACT_AGENT` is not set.
const INSTALLED_HELPER: &str = "$HOME/.local/share/refract/bin/refract-agent";

/// The Linux helper bundled as a Tauri resource. The release workflow puts the real
/// build there; elsewhere it is an empty placeholder (`app/scripts/ensure-helper.mjs`),
/// and the installed helper is used as it is.
const BUNDLED_HELPER: &str = "refract-agent";

/// What the bundled helper prints for `--version`. Its crate version is kept equal to
/// the app's (a test below checks), so an installed helper that prints anything else
/// is replaced.
const HELPER_VERSION: &str = concat!("refract-agent ", env!("CARGO_PKG_VERSION"));

/// How long `refract-agent --version` gets. WSL may still be starting.
const VERSION_TIMEOUT: Duration = Duration::from_secs(30);
/// How long copying the bundled helper into WSL gets.
const INSTALL_TIMEOUT: Duration = Duration::from_secs(60);

/// Hides the console window a child process would otherwise open.
#[cfg(windows)]
const CREATE_NO_WINDOW: u32 = 0x0800_0000;

/// Distros that container tools install. They have no user setup and no repos, so they
/// are passed over when one is the default.
const UTILITY_DISTROS: &[&str] = &[
    "docker-desktop",
    "docker-desktop-data",
    "rancher-desktop",
    "rancher-desktop-data",
];

/// Starts `command` without a console window, so none flashes up. Every process the
/// app starts goes through this.
pub fn no_window(command: &mut Command) -> &mut Command {
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        command.creation_flags(CREATE_NO_WINDOW);
    }
    command
}

/// A plain Windows directory to run child processes in: `%USERPROFILE%`, else `C:\`.
/// The app's own working directory may be a `\\wsl.localhost` path, which `wsl.exe`
/// cannot translate and `cmd.exe` refuses.
pub fn neutral_dir() -> PathBuf {
    neutral_dir_from(std::env::var_os("USERPROFILE").map(PathBuf::from))
}

fn neutral_dir_from(user_profile: Option<PathBuf>) -> PathBuf {
    user_profile
        .filter(|dir| !dir.as_os_str().is_empty())
        .unwrap_or_else(|| PathBuf::from("C:\\"))
}

/// `wsl.exe` set up to print UTF-8 rather than UTF-16 and to run in [`neutral_dir`],
/// with no console window.
pub(crate) fn wsl_command() -> Command {
    let mut command = Command::new("wsl.exe");
    command.env("WSL_UTF8", "1").current_dir(neutral_dir());
    no_window(&mut command);
    command
}

/// One line of `wsl.exe --list --verbose`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Distro {
    pub name: String,
    /// Marked with `*`: the distro `wsl.exe` uses without `-d`.
    pub default: bool,
}

/// Reads `wsl.exe --list --verbose` output. The first line is a header (its words
/// depend on the Windows language), and the default distro's line starts with `*`.
/// Only rows shaped like the table count: a name, a state and a numeric WSL version,
/// so a message such as "no installed distributions" yields nothing.
/// NULs, byte order marks and replacement characters are dropped first, in case
/// `wsl.exe` wrote UTF-16 despite `WSL_UTF8`; distro names are plain ASCII.
pub fn parse_distros(output: &str) -> Vec<Distro> {
    let text: String = output
        .chars()
        .filter(|c| !matches!(c, '\0' | '\u{feff}' | '\u{fffd}'))
        .collect();
    let mut lines = text.lines().filter(|line| !line.trim().is_empty());
    let is_version = |word: &str| !word.is_empty() && word.bytes().all(|b| b.is_ascii_digit());
    // The header has three words and no version number in the last column.
    match lines
        .next()
        .map(|header| header.split_whitespace().collect::<Vec<_>>())
    {
        Some(words) if words.len() == 3 && !is_version(words[2]) => {}
        _ => return Vec::new(),
    }
    lines
        .filter_map(|line| {
            let line = line.trim();
            let (default, rest) = match line.strip_prefix('*') {
                Some(rest) => (true, rest),
                None => (false, line),
            };
            let words: Vec<&str> = rest.split_whitespace().collect();
            if words.len() < 3 || !is_version(words[words.len() - 1]) {
                return None;
            }
            Some(Distro {
                name: words[0].to_owned(),
                default,
            })
        })
        .collect()
}

/// The distro to run the helper in: the default one, unless it is a utility distro
/// such as `docker-desktop`. Then the first other distro, preferring one whose name
/// starts with `Ubuntu`. `None` leaves the choice to `wsl.exe`.
pub fn choose_distro(distros: &[Distro]) -> Option<String> {
    let is_utility = |d: &&Distro| {
        UTILITY_DISTROS
            .iter()
            .any(|u| d.name.eq_ignore_ascii_case(u))
    };
    if let Some(default) = distros.iter().find(|d| d.default) {
        if !is_utility(&default) {
            return Some(default.name.clone());
        }
    }
    let others: Vec<&Distro> = distros.iter().filter(|d| !is_utility(d)).collect();
    others
        .iter()
        .find(|d| d.name.to_ascii_lowercase().starts_with("ubuntu"))
        .or(others.first())
        .map(|d| d.name.clone())
}

/// The distro from `REFRACT_WSL_DISTRO`, else the one [`choose_distro`] picks from
/// `wsl.exe --list --verbose`. The outer `None` means the list could not be read,
/// so the choice is worth trying again; the inner one leaves it to `wsl.exe`.
fn pick_distro() -> Option<Option<String>> {
    if let Some(distro) = std::env::var("REFRACT_WSL_DISTRO")
        .ok()
        .filter(|d| !d.is_empty())
    {
        eprintln!("refract: using WSL distro {distro} from REFRACT_WSL_DISTRO");
        return Some(Some(distro));
    }
    let mut command = wsl_command();
    command.args(["--list", "--verbose"]).stdin(Stdio::null());
    let out = match output_with_deadline(&mut command, LIST_TIMEOUT) {
        Ok(out) => out,
        Err(err) => {
            eprintln!("refract: cannot list WSL distros: {err}; using wsl.exe's default");
            return None;
        }
    };
    if !out.status.success() {
        eprintln!(
            "refract: wsl.exe --list failed ({}); using wsl.exe's default",
            out.status
        );
        return None;
    }
    let distros = parse_distros(&String::from_utf8_lossy(&out.stdout));
    let chosen = choose_distro(&distros);
    match &chosen {
        Some(distro) => eprintln!("refract: using WSL distro {distro}"),
        None => eprintln!("refract: no WSL distro found; using wsl.exe's default"),
    }
    Some(chosen)
}

/// Arguments for `wsl.exe` that start the helper: the one at `helper` if given, else
/// the installed one, in the home directory of `distro`. `sh` expands `$HOME` inside WSL and `exec`s the helper, so the
/// process on the other end of the pipes is the helper itself. The path is passed as
/// `$1` rather than spliced into the script, so it needs no quoting.
pub fn wsl_args(helper: Option<&str>, distro: Option<&str>) -> Vec<String> {
    let mut args = distro_args(distro);
    args.extend([
        "--cd".to_owned(),
        "~".to_owned(),
        "--exec".to_owned(),
        "sh".to_owned(),
        "-c".to_owned(),
        format!("exec \"${{1:-{INSTALLED_HELPER}}}\""),
        "sh".to_owned(),
    ]);
    if let Some(helper) = helper.filter(|h| !h.is_empty()) {
        args.push(helper.to_owned());
    }
    args
}

/// `-d <distro>`, or nothing when the choice is left to `wsl.exe`.
pub(crate) fn distro_args(distro: Option<&str>) -> Vec<String> {
    match distro.filter(|d| !d.is_empty()) {
        Some(distro) => vec!["-d".to_owned(), distro.to_owned()],
        None => Vec::new(),
    }
}

/// Which helper to start: `REFRACT_AGENT` as it is, else the installed one.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum HelperSource {
    /// A Linux path from `REFRACT_AGENT`, used in development. Nothing is installed.
    Path(String),
    /// [`INSTALLED_HELPER`], first replaced by `bundled` (the Linux path of the
    /// bundled helper) unless it already prints [`HELPER_VERSION`].
    Installed { bundled: Option<String> },
}

/// `REFRACT_AGENT` wins when set and not empty; otherwise the installed helper, kept in
/// step with `bundled`.
pub fn helper_source(from_env: Option<String>, bundled: Option<String>) -> HelperSource {
    match from_env.filter(|path| !path.is_empty()) {
        Some(path) => HelperSource::Path(path),
        None => HelperSource::Installed { bundled },
    }
}

/// The Linux path of the bundled helper at `resource` (a Windows path, maybe
/// verbatim), or `None` when it is missing or the empty placeholder (`len` 0).
pub fn bundled_helper(resource: Option<&Path>, len: Option<u64>) -> Option<String> {
    let resource = resource?;
    if len.unwrap_or(0) == 0 {
        return None;
    }
    wsl_path(&resource.to_string_lossy())
}

/// Whether the installed helper is the bundled one: `refract-agent --version` exited
/// successfully and printed `want`.
pub fn installed_is_current(success: bool, stdout: &str, want: &str) -> bool {
    success && stdout.trim() == want
}

/// Whether a command that failed to run means `wsl.exe` is not there at all.
fn wsl_missing(err: &io::Error) -> bool {
    err.kind() == io::ErrorKind::NotFound
}

/// Why a command failed: its stderr, else its stdout (`wsl.exe` reports some errors
/// there), else its exit `status`. NULs are dropped, as in [`parse_distros`], in case
/// `wsl.exe` wrote UTF-16.
fn failure_reason(stderr: &str, stdout: &str, status: &str) -> String {
    let clean = |text: &str| text.replace('\0', "").trim().to_owned();
    [clean(stderr), clean(stdout)]
        .into_iter()
        .find(|text| !text.is_empty())
        .unwrap_or_else(|| status.to_owned())
}

/// Arguments for `wsl.exe` that print the installed helper's version.
pub fn version_args(distro: Option<&str>) -> Vec<String> {
    let mut args = distro_args(distro);
    args.extend([
        "--exec".to_owned(),
        "sh".to_owned(),
        "-c".to_owned(),
        format!("exec \"{INSTALLED_HELPER}\" --version"),
    ]);
    args
}

/// Arguments for `wsl.exe` that copy the helper at `source` (a Linux path) to
/// [`INSTALLED_HELPER`]: into a temporary file next to it, made executable, then
/// renamed over it, so a helper still running keeps its old file and a failed copy
/// leaves the old one in place. `source` is passed as `$1`, so it needs no quoting.
pub fn install_args(distro: Option<&str>, source: &str) -> Vec<String> {
    let script = format!(
        "set -e; dest=\"{INSTALLED_HELPER}\"; dir=\"${{dest%/*}}\"; mkdir -p \"$dir\"; \
         tmp=\"$dir/.refract-agent.$$\"; trap 'rm -f \"$tmp\"' EXIT; \
         cp \"$1\" \"$tmp\"; chmod 755 \"$tmp\"; mv -f \"$tmp\" \"$dest\""
    );
    let mut args = distro_args(distro);
    args.extend([
        "--exec".to_owned(),
        "sh".to_owned(),
        "-c".to_owned(),
        script,
        "sh".to_owned(),
        source.to_owned(),
    ]);
    args
}

/// Makes sure [`INSTALLED_HELPER`] in `distro` is the bundled helper at `source`,
/// copying it in when it is missing or prints another version. Errors say why the
/// copy failed and end up in the helper status's `lastError`.
fn ensure_installed(distro: Option<&str>, source: &str) -> io::Result<()> {
    let mut check = wsl_command();
    check.args(version_args(distro)).stdin(Stdio::null());
    match output_with_deadline(&mut check, VERSION_TIMEOUT) {
        Ok(out)
            if installed_is_current(
                out.status.success(),
                &String::from_utf8_lossy(&out.stdout),
                HELPER_VERSION,
            ) =>
        {
            return Ok(());
        }
        Ok(out) => eprintln!(
            "refract: the installed helper is missing or out of date ({}); installing {source}",
            String::from_utf8_lossy(&out.stdout).trim()
        ),
        // No `wsl.exe`: installing would fail the same way.
        Err(err) if wsl_missing(&err) => {
            return Err(io::Error::new(
                io::ErrorKind::NotFound,
                "WSL is not installed (wsl.exe was not found)",
            ));
        }
        Err(err) => {
            eprintln!("refract: cannot check the installed helper: {err}; installing {source}")
        }
    }
    let mut install = wsl_command();
    install
        .args(install_args(distro, source))
        .stdin(Stdio::null());
    let out = output_with_deadline(&mut install, INSTALL_TIMEOUT).map_err(|err| {
        io::Error::new(
            err.kind(),
            format!("cannot install the helper into WSL: {err}"),
        )
    })?;
    if !out.status.success() {
        let reason = failure_reason(
            &String::from_utf8_lossy(&out.stderr),
            &String::from_utf8_lossy(&out.stdout),
            &out.status.to_string(),
        );
        return Err(io::Error::other(format!(
            "cannot install the helper into WSL: {reason}"
        )));
    }
    eprintln!("refract: installed the helper at {INSTALLED_HELPER}");
    Ok(())
}

/// The helper's distro as [`pick_distro`] returned it, shared by the spawner and
/// [`Agent::distro`]. `None` until the list was read.
type DistroSlot = Arc<Mutex<Option<Option<String>>>>;

/// The distro in `slot`, choosing it first if it is not known yet. The choice is kept
/// once the list was read.
fn chosen_distro(slot: &DistroSlot) -> Option<String> {
    let mut slot = slot.lock().unwrap_or_else(PoisonError::into_inner);
    if slot.is_none() {
        *slot = pick_distro();
    }
    slot.clone().flatten()
}

/// Starts the helper through [`wsl_command`] with piped stdio. Runs on the client's
/// supervisor thread, so choosing the distro there keeps `wsl.exe --list` off the
/// main thread.
/// Installing the bundled helper happens here too, before each start, so a failed copy
/// is retried with the same backoff.
struct WslSpawner {
    helper: HelperSource,
    distro: DistroSlot,
}

impl Spawn for WslSpawner {
    fn spawn(&mut self) -> io::Result<Child> {
        let distro = chosen_distro(&self.distro);
        let path = match &self.helper {
            HelperSource::Path(path) => Some(path.as_str()),
            HelperSource::Installed { bundled } => {
                if let Some(source) = bundled {
                    ensure_installed(distro.as_deref(), source)?;
                }
                None
            }
        };
        let args = wsl_args(path, distro.as_deref());
        eprintln!("refract: starting the helper: wsl.exe {}", args.join(" "));
        wsl_command()
            .args(&args)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
    }
}

/// The running client, managed as Tauri state.
pub struct Agent {
    client: AgentClient,
    stopped: AtomicBool,
    distro: DistroSlot,
}

impl Agent {
    /// Sends one request to the helper and waits up to `timeout` for its reply.
    pub fn request(&self, method: Method, timeout: Duration) -> Result<Reply, RequestError> {
        self.client.request(method, timeout)
    }

    /// The distro the helper runs in, or `None` when that is left to `wsl.exe`'s
    /// default. May run `wsl.exe --list`, so not for the main thread.
    pub fn distro(&self) -> Option<String> {
        chosen_distro(&self.distro)
    }

    /// Stops the helper once; later calls do nothing.
    pub fn shutdown(&self) {
        if !self.stopped.swap(true, Ordering::SeqCst) {
            self.client.shutdown(SHUTDOWN_GRACE);
        }
    }
}

/// Starts the helper and forwards its status and events to the interface.
pub fn start(app: &AppHandle) {
    let resource = app
        .path()
        .resolve(BUNDLED_HELPER, BaseDirectory::Resource)
        .ok();
    let len = resource
        .as_deref()
        .and_then(|path| std::fs::metadata(path).ok())
        .filter(|meta| meta.is_file())
        .map(|meta| meta.len());
    let helper = helper_source(
        std::env::var("REFRACT_AGENT").ok(),
        bundled_helper(resource.as_deref(), len),
    );
    if let HelperSource::Installed { bundled: None } = &helper {
        eprintln!("refract: no helper bundled; using the installed one as it is");
    }
    let handle = app.clone();
    let distro = DistroSlot::default();
    let client = AgentClient::start(
        WslSpawner {
            helper,
            distro: distro.clone(),
        },
        move |event| {
            let result = match event {
                ClientEvent::Status(status) => {
                    crate::migrate::on_helper_status(&handle, &status);
                    crate::alerts::on_helper_status(&status);
                    handle.emit(STATUS_EVENT, status)
                }
                ClientEvent::Event(event) => {
                    crate::migrate::on_helper_event(&handle, &event);
                    crate::alerts::on_helper_event(&handle, &event);
                    handle.emit(EVENT_EVENT, event)
                }
            };
            if let Err(err) = result {
                eprintln!("refract: forwarding a helper event: {err}");
            }
        },
        Backoff::default(),
    );
    app.manage(Agent {
        client,
        stopped: AtomicBool::new(false),
        distro,
    });
}

/// Stops the helper, if it was started.
pub fn shutdown(app: &AppHandle) {
    if let Some(agent) = app.try_state::<Agent>() {
        agent.shutdown();
    }
}

/// Reads `{"method": ..., "params": ...}` with the same rules as the helper. A bad
/// shape is reported as an `Agent` error with the helper's code for it
/// (`-32600` no method, `-32601` unknown method, `-32602` bad params). Methods
/// outside [`INTERFACE_METHODS`] are refused with `-32601` too.
fn parse_method(request: Value) -> Result<Method, RequestError> {
    let agent_error = |code, message| RequestError::Agent { code, message };
    let Value::Object(mut object) = request else {
        return Err(agent_error(
            code::INVALID_REQUEST,
            "request is not an object".into(),
        ));
    };
    if let Some(Value::String(method)) = object.get("method") {
        if !INTERFACE_METHODS.contains(&method.as_str()) {
            return Err(agent_error(
                code::METHOD_NOT_FOUND,
                format!("{method} is not available to the interface"),
            ));
        }
    }
    // `parse_request` wants a whole request line; the client assigns the real id.
    object.insert("id".into(), Value::from(0));
    protocol::parse_request(&Value::Object(object).to_string())
        .map(|request| request.method)
        .map_err(|err| agent_error(err.error.code, err.error.message))
}

#[tauri::command]
pub fn agent_status(agent: State<'_, Agent>) -> AgentStatus {
    agent.client.status()
}

/// Sends one request to the helper and returns its result as JSON.
#[tauri::command]
pub async fn agent_request(agent: State<'_, Agent>, request: Value) -> Result<Value, RequestError> {
    let method = parse_method(request)?;
    let client = agent.client.clone();
    let internal = |message: String| RequestError::Agent {
        code: code::INTERNAL_ERROR,
        message,
    };
    let reply =
        tauri::async_runtime::spawn_blocking(move || client.request(method, REQUEST_TIMEOUT))
            .await
            .map_err(|err| internal(format!("request task failed: {err}")))??;
    // The refactor toasts compare later changes with what the interface was shown.
    if let Reply::Repos(list) = &reply {
        crate::alerts::on_list(&list.repos);
    }
    serde_json::to_value(reply).map_err(|err| internal(err.to_string()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    const SCRIPT: &str = "exec \"${1:-$HOME/.local/share/refract/bin/refract-agent}\"";

    #[test]
    fn runs_the_installed_helper_by_default() {
        assert_eq!(
            wsl_args(None, None),
            ["--cd", "~", "--exec", "sh", "-c", SCRIPT, "sh"]
        );
        assert_eq!(wsl_args(Some(""), Some("")), wsl_args(None, None));
    }

    #[test]
    fn passes_the_helper_path_and_distro() {
        assert_eq!(
            wsl_args(
                Some("/home/dev/refract/target/debug/refract-agent"),
                Some("Ubuntu")
            ),
            [
                "-d",
                "Ubuntu",
                "--cd",
                "~",
                "--exec",
                "sh",
                "-c",
                SCRIPT,
                "sh",
                "/home/dev/refract/target/debug/refract-agent"
            ]
        );
    }

    #[test]
    fn refract_agent_from_the_environment_wins() {
        let bundled = Some("/mnt/c/Program Files/Refract/refract-agent".to_owned());
        assert_eq!(
            helper_source(Some("/home/dev/refract-agent".into()), bundled.clone()),
            HelperSource::Path("/home/dev/refract-agent".into())
        );
        assert_eq!(
            helper_source(Some(String::new()), bundled.clone()),
            HelperSource::Installed {
                bundled: bundled.clone()
            }
        );
        assert_eq!(
            helper_source(None, None),
            HelperSource::Installed { bundled: None }
        );
    }

    #[test]
    fn bundled_helper_is_used_from_its_linux_path() {
        let resource = PathBuf::from(r"\\?\C:\Program Files\Refract\refract-agent");
        assert_eq!(
            bundled_helper(Some(&resource), Some(5_000_000)).as_deref(),
            Some("/mnt/c/Program Files/Refract/refract-agent")
        );
        // The empty placeholder of a build without the real helper, or no file.
        assert_eq!(bundled_helper(Some(&resource), Some(0)), None);
        assert_eq!(bundled_helper(Some(&resource), None), None);
        assert_eq!(bundled_helper(None, Some(5)), None);
        // A path with no Linux form.
        let volume = PathBuf::from(r"\\?\Volume{1234}\refract-agent");
        assert_eq!(bundled_helper(Some(&volume), Some(5)), None);
    }

    #[test]
    fn failures_are_explained_from_stderr_then_stdout_then_status() {
        assert_eq!(failure_reason(" no space\n", "out", "exit 1"), "no space");
        assert_eq!(
            failure_reason("", "n\0o \0distro\0\r\n", "exit 1"),
            "no distro"
        );
        assert_eq!(failure_reason(" \n", "\0", "exit code: 1"), "exit code: 1");
        assert!(wsl_missing(&io::Error::from(io::ErrorKind::NotFound)));
        assert!(!wsl_missing(&io::Error::from(io::ErrorKind::TimedOut)));
    }

    #[test]
    fn installed_helper_is_replaced_unless_it_prints_this_version() {
        let want = "refract-agent 0.2.0";
        assert!(installed_is_current(true, "refract-agent 0.2.0\n", want));
        assert!(!installed_is_current(true, "refract-agent 0.1.0\n", want));
        // Missing: `sh` cannot run it and prints nothing on stdout.
        assert!(!installed_is_current(false, "", want));
        // A helper from before `--version`, which ignored it and served stdin.
        assert!(!installed_is_current(true, "", want));
        assert!(!installed_is_current(false, "refract-agent 0.2.0\n", want));
        assert_eq!(
            HELPER_VERSION,
            format!("refract-agent {}", env!("CARGO_PKG_VERSION"))
        );
    }

    /// The `version` of the first `[package]` in a Cargo.toml, or the top-level one
    /// in tauri.conf.json.
    fn version_in(text: &str, prefix: &str) -> String {
        text.lines()
            .find_map(|line| line.trim().strip_prefix(prefix))
            .map(|rest| rest.trim_matches(|c: char| c == '"' || c == ',' || c == ' '))
            .unwrap()
            .to_owned()
    }

    #[test]
    fn app_and_helper_versions_match() {
        // The installed helper is compared with the app's version, so a release must
        // bump both together.
        let app = env!("CARGO_PKG_VERSION");
        let helper = include_str!("../../../crates/refract-agent/Cargo.toml");
        assert_eq!(version_in(helper, "version = "), app);
        let conf: Value = serde_json::from_str(include_str!("../tauri.conf.json")).unwrap();
        assert_eq!(conf["version"], app);
    }

    #[test]
    fn checks_and_installs_the_helper_in_the_chosen_distro() {
        assert_eq!(
            version_args(Some("Ubuntu")),
            [
                "-d",
                "Ubuntu",
                "--exec",
                "sh",
                "-c",
                "exec \"$HOME/.local/share/refract/bin/refract-agent\" --version"
            ]
        );
        assert_eq!(version_args(None)[..2], ["--exec", "sh"]);
        let args = install_args(Some("Ubuntu"), "/mnt/c/Program Files/Refract/refract-agent");
        assert_eq!(args[..5], ["-d", "Ubuntu", "--exec", "sh", "-c"]);
        assert_eq!(
            args[6..],
            ["sh", "/mnt/c/Program Files/Refract/refract-agent"]
        );
        let script = &args[5];
        assert!(
            script.starts_with("set -e; dest=\"$HOME/.local/share/refract/bin/refract-agent\";")
        );
        assert!(script.contains("cp \"$1\" \"$tmp\"; chmod 755 \"$tmp\"; mv -f \"$tmp\" \"$dest\""));
        assert_eq!(install_args(None, "/x")[..2], ["--exec", "sh"]);
    }

    #[test]
    fn children_run_in_a_plain_windows_directory() {
        assert_eq!(
            neutral_dir_from(Some(r"C:\Users\dev".into())),
            PathBuf::from(r"C:\Users\dev")
        );
        assert_eq!(neutral_dir_from(Some("".into())), PathBuf::from("C:\\"));
        assert_eq!(neutral_dir_from(None), PathBuf::from("C:\\"));
    }

    fn distro(name: &str, default: bool) -> Distro {
        Distro {
            name: name.into(),
            default,
        }
    }

    /// `wsl.exe --list --verbose` with `WSL_UTF8=1`, as on a machine with Docker Desktop.
    const DOCKER_DEFAULT: &str = "  NAME              STATE           VERSION\r\n\
                                  * docker-desktop    Stopped         2\r\n\
                                  \x20 Ubuntu            Running         2\r\n";

    #[test]
    fn distro_list_is_parsed() {
        assert_eq!(
            parse_distros(DOCKER_DEFAULT),
            [distro("docker-desktop", true), distro("Ubuntu", false)]
        );
        // A localized header, extra spaces and a blank line.
        let out = "\n  NOM      ÉTAT       VERSION\n*   Ubuntu-22.04   Running   2\n  Debian  Stopped  2\n\n";
        assert_eq!(
            parse_distros(out),
            [distro("Ubuntu-22.04", true), distro("Debian", false)]
        );
        assert_eq!(parse_distros(""), []);
        assert_eq!(parse_distros("  NAME   STATE   VERSION\r\n"), []);
        assert_eq!(parse_distros(NO_DISTROS), []);
        // Text after the table that is not a row.
        let out = "NAME STATE VERSION\n* Ubuntu Running 2\nsomething went wrong here\n";
        assert_eq!(parse_distros(out), [distro("Ubuntu", true)]);
    }

    #[test]
    fn utf16_distro_list_is_parsed() {
        // What `wsl.exe` prints when it ignores `WSL_UTF8`: UTF-16LE with a byte order
        // mark, read as UTF-8.
        let utf16: Vec<u8> = DOCKER_DEFAULT
            .encode_utf16()
            .flat_map(u16::to_le_bytes)
            .collect();
        let bytes = [&[0xff, 0xfe][..], &utf16].concat();
        assert_eq!(
            parse_distros(&String::from_utf8_lossy(&bytes)),
            [distro("docker-desktop", true), distro("Ubuntu", false)]
        );
    }

    #[test]
    fn default_distro_is_chosen_unless_it_is_a_utility() {
        assert_eq!(
            choose_distro(&[distro("Debian", false), distro("Ubuntu", true)]),
            Some("Ubuntu".into())
        );
        assert_eq!(
            choose_distro(&parse_distros(DOCKER_DEFAULT)),
            Some("Ubuntu".into())
        );
        assert_eq!(
            choose_distro(&[
                distro("Docker-Desktop-Data", false),
                distro("Debian", false),
                distro("Ubuntu-24.04", false),
                distro("docker-desktop", true),
            ]),
            Some("Ubuntu-24.04".into())
        );
        assert_eq!(
            choose_distro(&[
                distro("rancher-desktop", true),
                distro("Debian", false),
                distro("archlinux", false),
            ]),
            Some("Debian".into())
        );
        assert_eq!(
            choose_distro(&[distro("Debian", false), distro("archlinux", false)]),
            Some("Debian".into())
        );
        assert_eq!(choose_distro(&[distro("docker-desktop", true)]), None);
        assert_eq!(choose_distro(&[]), None);
    }

    #[test]
    fn parses_methods_like_the_helper() {
        assert_eq!(
            parse_method(json!({"method": "listRepos"})),
            Ok(Method::ListRepos)
        );
        assert_eq!(
            parse_method(json!({"method": "ping", "params": {}})),
            Ok(Method::Ping)
        );
        assert_eq!(
            parse_method(json!({"method": "getRepo", "params": {"path": "/x"}})),
            Ok(Method::GetRepo { path: "/x".into() })
        );
    }

    #[test]
    fn interface_cannot_call_launch_or_setup_methods() {
        let refused = |method: &str| RequestError::Agent {
            code: code::METHOD_NOT_FOUND,
            message: format!("{method} is not available to the interface"),
        };
        assert_eq!(
            parse_method(
                json!({"method": "prepareLaunch", "params": {"path": "/x", "mode": {"kind": "new"}}})
            ),
            Err(refused("prepareLaunch"))
        );
        assert_eq!(
            parse_method(json!({"method": "setupServer", "params": {"vsix": "/x.vsix"}})),
            Err(refused("setupServer"))
        );
        assert_eq!(
            parse_method(json!({"method": "shutdown"})),
            Err(refused("shutdown"))
        );
    }

    /// What `wsl.exe --list --verbose` prints when no distro is installed.
    const NO_DISTROS: &str = "Windows Subsystem for Linux has no installed distributions.\r\n\
                              Distributions can be installed by visiting the Microsoft Store:\r\n\
                              https://aka.ms/wslstore\r\n";

    #[test]
    fn reports_bad_requests_with_the_helper_codes() {
        let code_of = |request: Value| match parse_method(request) {
            Err(RequestError::Agent { code, .. }) => code,
            other => panic!("expected an agent error, got {other:?}"),
        };
        assert_eq!(code_of(json!([1])), code::INVALID_REQUEST);
        assert_eq!(code_of(json!({})), code::INVALID_REQUEST);
        assert_eq!(
            code_of(json!({"method": "explode"})),
            code::METHOD_NOT_FOUND
        );
        assert_eq!(code_of(json!({"method": "getRepo"})), code::INVALID_PARAMS);
    }
}
