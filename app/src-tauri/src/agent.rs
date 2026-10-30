//! Runs refract-agent inside WSL and exposes it to the interface.

use std::io;
use std::path::PathBuf;
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use crate::process::output_with_deadline;
use refract_client::{AgentClient, AgentStatus, Backoff, ClientEvent, RequestError, Spawn};
use refract_core::protocol::{self, code, Method, Reply};
use serde_json::Value;
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

/// Where the installer puts the helper, used when `REFRACT_AGENT` is not set.
const INSTALLED_HELPER: &str = "$HOME/.local/share/refract/bin/refract-agent";

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
fn wsl_command() -> Command {
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
    let mut args = Vec::new();
    if let Some(distro) = distro.filter(|d| !d.is_empty()) {
        args.extend(["-d".to_owned(), distro.to_owned()]);
    }
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

/// Starts the helper through [`wsl_command`] with piped stdio. Runs on the client's
/// supervisor thread, so choosing the distro there keeps `wsl.exe --list` off the
/// main thread. The choice is kept once the list was read.
struct WslSpawner {
    helper: Option<String>,
    distro: Option<Option<String>>,
}

impl Spawn for WslSpawner {
    fn spawn(&mut self) -> io::Result<Child> {
        if self.distro.is_none() {
            self.distro = pick_distro();
        }
        let distro = self.distro.clone().flatten();
        let args = wsl_args(self.helper.as_deref(), distro.as_deref());
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
}

impl Agent {
    /// Sends one request to the helper and waits up to `timeout` for its reply.
    pub fn request(&self, method: Method, timeout: Duration) -> Result<Reply, RequestError> {
        self.client.request(method, timeout)
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
    let helper = std::env::var("REFRACT_AGENT").ok();
    let handle = app.clone();
    let client = AgentClient::start(
        WslSpawner {
            helper,
            distro: None,
        },
        move |event| {
            let result = match event {
                ClientEvent::Status(status) => handle.emit(STATUS_EVENT, status),
                ClientEvent::Event(event) => handle.emit(EVENT_EVENT, event),
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
