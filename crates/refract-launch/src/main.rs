//! `refract-launch`: a development tool that runs the launch path from WSL before the
//! Windows app exists. It does the first-run setup, asks `refract-agent` for a
//! ticket, opens VS Code on it and adds the bare layout to the Refract profile.
//!
//! ```text
//! refract-launch <repo-path> [--resume <id>] [--prompt <text>]
//!                [--agent <refract-agent>] [--code <code>] [--user-dir <dir>]
//!                [--vsix <refract.vsix>] [--profile-name <name>]
//! ```
//!
//! `--profile-name` opens the windows in another profile than `Refract`, so a throwaway
//! name can exercise the first run, which creates the profile.

use std::io::{self, BufRead, BufReader, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, ExitCode, Stdio};
use std::time::{Duration, Instant};

use refract_core::protocol::{LaunchMode, LaunchTicket, Message, Outcome, Reply, Response};
use refract_launch::setup::{
    ensure_application_scoped, ensure_profile, ensure_vsix, server_cli, windows_code_commit,
    CliCommand, CodeCli, Processes, Runner, WindowCloser, WindowMatch, REFRACT_EXTENSION,
};
use refract_launch::{ensure_profile_settings, vscode_args, PROFILE};
use serde_json::json;

const USAGE: &str = "usage: refract-launch <repo-path> [--resume <id>] [--prompt <text>] \
                     [--agent <path>] [--code <path>] [--user-dir <path>] [--vsix <path>] \
                     [--profile-name <name>]";

/// The helper that lists and closes VS Code windows, copied to the Windows temp
/// directory so Windows PowerShell can run it.
const WINDOWS_SCRIPT: &str = include_str!("../../../scripts/vscode-windows.ps1");

/// The extension package built in this repo, used when `--vsix` is not given.
const DEFAULT_VSIX: &str = concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/../../vscode-extension/refract.vsix"
);

/// How long to wait for VS Code to create the Refract profile.
const PROFILE_WAIT: Duration = Duration::from_secs(15);

#[derive(Default)]
struct Args {
    repo: Option<String>,
    resume: Option<String>,
    prompt: Option<String>,
    agent: Option<PathBuf>,
    code: Option<PathBuf>,
    user_dir: Option<PathBuf>,
    vsix: Option<PathBuf>,
    profile: Option<String>,
}

fn main() -> ExitCode {
    match run() {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("refract-launch: {e}");
            ExitCode::FAILURE
        }
    }
}

fn run() -> Result<(), String> {
    let args = parse_args(std::env::args().skip(1))?;
    let repo = args.repo.ok_or(USAGE)?;
    let mode = match (args.resume, args.prompt) {
        (Some(_), Some(_)) => return Err("--resume and --prompt cannot be combined".into()),
        (Some(session_id), None) => LaunchMode::Resume { session_id },
        (None, prompt) => LaunchMode::New { prompt },
    };
    let agent = match args.agent {
        Some(path) => path,
        None => std::env::current_exe()
            .map_err(|e| format!("cannot find own path: {e}"))?
            .with_file_name("refract-agent"),
    };
    let profile = args.profile.as_deref().unwrap_or(PROFILE);
    let user = windows_user();
    let code = match args.code {
        Some(path) => path,
        None => find_code(user.as_deref()).ok_or("cannot find VS Code; pass --code")?,
    };
    let user_dir = match args.user_dir {
        Some(path) => path,
        None => {
            let user = user
                .as_deref()
                .ok_or("cannot find the Windows user; pass --user-dir")?;
            Path::new("/mnt/c/Users")
                .join(user)
                .join("AppData/Roaming/Code/User")
        }
    };

    setup(profile, &user_dir, user.as_deref(), args.vsix.as_deref())?;

    let ticket = prepare(&agent, &repo, &mode)?;
    println!(
        "{}",
        serde_json::to_string_pretty(&ticket).map_err(|e| e.to_string())?
    );

    let status = Command::new(&code)
        .args(vscode_args(profile, &ticket.folder_uri))
        .stdin(Stdio::null())
        .status()
        .map_err(|e| format!("cannot run {}: {e}", code.display()))?;
    if !status.success() {
        return Err(format!("{} exited with {status}", code.display()));
    }

    let deadline = Instant::now() + PROFILE_WAIT;
    loop {
        match ensure_profile_settings(&user_dir, profile) {
            Ok(true) => {
                eprintln!("refract-launch: {profile} profile settings are in place");
                return Ok(());
            }
            Ok(false) if Instant::now() < deadline => {
                std::thread::sleep(Duration::from_millis(500));
            }
            Ok(false) => {
                return Err(format!(
                    "no {profile} profile in {} after {}s",
                    user_dir.display(),
                    PROFILE_WAIT.as_secs()
                ))
            }
            Err(e) => return Err(format!("cannot update profile settings: {e}")),
        }
    }
}

fn parse_args(mut args: impl Iterator<Item = String>) -> Result<Args, String> {
    let mut parsed = Args::default();
    while let Some(arg) = args.next() {
        let mut value = || args.next().ok_or_else(|| format!("{arg} needs a value"));
        match arg.as_str() {
            "--resume" => parsed.resume = Some(value()?),
            "--prompt" => parsed.prompt = Some(value()?),
            "--agent" => parsed.agent = Some(value()?.into()),
            "--code" => parsed.code = Some(value()?.into()),
            "--user-dir" => parsed.user_dir = Some(value()?.into()),
            "--vsix" => parsed.vsix = Some(value()?.into()),
            "--profile-name" => {
                let name = value()?;
                // It goes into a `cmd.exe` command line and a window-title match.
                if name.is_empty()
                    || !name
                        .chars()
                        .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
                {
                    return Err(format!(
                        "--profile-name takes letters, digits, '-' and '_', not {name:?}"
                    ));
                }
                parsed.profile = Some(name);
            }
            "-h" | "--help" => return Err(USAGE.into()),
            _ if arg.starts_with('-') => return Err(format!("unknown option {arg}\n{USAGE}")),
            _ if parsed.repo.is_none() => parsed.repo = Some(arg),
            _ => return Err(USAGE.into()),
        }
    }
    Ok(parsed)
}

/// The first-run setup: the profile with the WSL extension in it, the Refract
/// extension in the WSL server, and that extension applied to every profile. A
/// missing server or vsix is reported and skipped, so the window still opens.
fn setup(
    profile: &str,
    user_dir: &Path,
    user: Option<&str>,
    vsix: Option<&Path>,
) -> Result<(), String> {
    let cli = CodeCli::WslCmd {
        cwd: "/mnt/c".into(),
    };
    let mut runner = Processes;
    let mut closer = PowerShell { user };
    let mut sleep = std::thread::sleep;
    let done = ensure_profile(
        &cli,
        profile,
        user_dir,
        &mut runner,
        &mut closer,
        &mut sleep,
    )
    .map_err(|e| format!("cannot set up the {profile} profile: {e}"))?;
    if done.created {
        eprintln!("refract-launch: created the {profile} profile");
    }
    if done.installed_wsl {
        eprintln!("refract-launch: installed the WSL extension in the {profile} profile");
    }

    let vsix = vsix.unwrap_or(Path::new(DEFAULT_VSIX));
    if !vsix.is_file() {
        eprintln!(
            "refract-launch: skipping the extension install: no {}",
            vsix.display()
        );
        return Ok(());
    }
    let home = std::env::var_os("HOME").ok_or("HOME is not set")?;
    let server = Path::new(&home).join(".vscode-server");
    let commit = windows_code_commit(&cli, &mut runner);
    let code_server = match server_cli(&server, commit.as_deref()) {
        Ok(path) => path,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            eprintln!("refract-launch: skipping the extension install: {e}");
            return Ok(());
        }
        Err(e) => return Err(format!("cannot find the VS Code server: {e}")),
    };
    let registry = server.join("extensions/extensions.json");
    if ensure_vsix(
        &code_server,
        vsix,
        &registry,
        REFRACT_EXTENSION,
        &mut runner,
    )
    .map_err(|e| format!("cannot install the extension: {e}"))?
    {
        eprintln!("refract-launch: installed the Refract extension in the WSL server");
    }
    if ensure_application_scoped(&registry, REFRACT_EXTENSION)
        .map_err(|e| format!("cannot update {}: {e}", registry.display()))?
    {
        eprintln!("refract-launch: applied the Refract extension to all profiles");
    }
    Ok(())
}

/// Closes VS Code windows with [`WINDOWS_SCRIPT`], run by Windows PowerShell from a
/// copy in the Windows temp directory.
struct PowerShell<'a> {
    /// The Windows user, for the fallback temp directory.
    user: Option<&'a str>,
}

impl WindowCloser for PowerShell<'_> {
    fn close(&mut self, window: &WindowMatch) -> io::Result<()> {
        let (linux_temp, windows_temp) = windows_temp(self.user).ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::NotFound,
                "cannot find the Windows temp directory",
            )
        })?;
        let name = "refract-vscode-windows.ps1";
        std::fs::write(linux_temp.join(name), WINDOWS_SCRIPT)?;
        let command = CliCommand {
            program: "powershell.exe".into(),
            args: vec![
                "-NoProfile".into(),
                "-ExecutionPolicy".into(),
                "Bypass".into(),
                "-File".into(),
                format!("{windows_temp}\\{name}"),
                "-Close".into(),
                window.title.clone(),
                "-Without".into(),
                window.without.clone(),
            ],
            cwd: Some("/mnt/c".into()),
        };
        let out = Processes.run(&command)?;
        if !out.success {
            return Err(io::Error::other(format!(
                "powershell.exe failed: {}",
                out.stderr.trim()
            )));
        }
        for line in out.stdout.lines().filter(|l| l.starts_with("closed:")) {
            eprintln!("refract-launch: {line}");
        }
        Ok(())
    }
}

/// The Windows temp directory as a WSL path and a Windows path: `%TEMP%`, or else
/// `C:\Users\<user>\AppData\Local\Temp`.
fn windows_temp(user: Option<&str>) -> Option<(PathBuf, String)> {
    let from_env = windows_output("cmd.exe", &["/c", "echo %TEMP%"])
        .filter(|t| !t.contains('%'))
        .and_then(|windows| {
            let linux = windows_output("wslpath", &["-u", &windows])?;
            Some((PathBuf::from(linux), windows))
        });
    if let Some((linux, windows)) = from_env.filter(|(linux, _)| linux.is_dir()) {
        return Some((linux, windows));
    }
    let user = user?;
    let linux = Path::new("/mnt/c/Users")
        .join(user)
        .join("AppData/Local/Temp");
    linux
        .is_dir()
        .then(|| (linux, format!("C:\\Users\\{user}\\AppData\\Local\\Temp")))
}

/// The trimmed stdout of `program`, run in `/mnt/c`, when it succeeds with output.
fn windows_output(program: &str, args: &[&str]) -> Option<String> {
    let out = Command::new(program)
        .args(args)
        .current_dir("/mnt/c")
        .stdin(Stdio::null())
        .stderr(Stdio::null())
        .output()
        .ok()?;
    let text = String::from_utf8_lossy(&out.stdout).trim().to_owned();
    (out.status.success() && !text.is_empty()).then_some(text)
}

/// Runs the agent for `ping` and `prepareLaunch`, then shuts it down.
fn prepare(agent: &Path, repo: &str, mode: &LaunchMode) -> Result<LaunchTicket, String> {
    let mut child = Command::new(agent)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::inherit())
        .spawn()
        .map_err(|e| format!("cannot start {}: {e}", agent.display()))?;
    let mut stdin = child.stdin.take().expect("piped stdin");
    let mut lines = BufReader::new(child.stdout.take().expect("piped stdout")).lines();
    let mut call = |id: u64, request: serde_json::Value| -> Result<Response, String> {
        writeln!(stdin, "{request}").map_err(|e| format!("agent stdin: {e}"))?;
        loop {
            let line = lines
                .next()
                .ok_or("agent exited without answering")?
                .map_err(|e| format!("agent stdout: {e}"))?;
            let message: Message =
                serde_json::from_str(&line).map_err(|e| format!("agent wrote {line:?}: {e}"))?;
            if let Message::Response(response) = message {
                if response.id == Some(id.into()) {
                    return Ok(response);
                }
            }
        }
    };
    call(1, json!({"id": 1, "method": "ping"}))?;
    let response = call(
        2,
        json!({"id": 2, "method": "prepareLaunch", "params": {"path": repo, "mode": mode}}),
    )?;
    let _ = call(3, json!({"id": 3, "method": "shutdown"}));
    let _ = child.wait();
    match response.outcome {
        Outcome::Result(Reply::Launch(ticket)) => Ok(ticket),
        Outcome::Error(e) => Err(format!("prepareLaunch failed ({}): {}", e.code, e.message)),
        Outcome::Result(other) => Err(format!("unexpected prepareLaunch result: {other:?}")),
    }
}

/// The Windows user name, from `%USERNAME%`.
fn windows_user() -> Option<String> {
    windows_output("cmd.exe", &["/c", "echo %USERNAME%"]).filter(|user| !user.contains('%'))
}

/// VS Code's `bin/code` for `user`, or for the first user that has one.
fn find_code(user: Option<&str>) -> Option<PathBuf> {
    let bin = "AppData/Local/Programs/Microsoft VS Code/bin/code";
    let users = Path::new("/mnt/c/Users");
    if let Some(path) = user.map(|u| users.join(u).join(bin)) {
        if path.is_file() {
            return Some(path);
        }
    }
    let mut dirs: Vec<PathBuf> = std::fs::read_dir(users)
        .ok()?
        .flatten()
        .map(|e| e.path())
        .collect();
    dirs.sort();
    dirs.into_iter().map(|d| d.join(bin)).find(|p| p.is_file())
}
