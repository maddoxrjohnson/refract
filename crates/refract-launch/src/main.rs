//! `refract-launch`: a development tool that runs the launch path from WSL before the
//! Windows app exists. It does the first-run setup, with the WSL server's part done by
//! `refract-agent`'s `setupServer` as the app will, asks the agent for a ticket, opens
//! VS Code on it and adds the bare layout to the Refract profile.
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
use std::process::{Child, ChildStdin, ChildStdout, Command, ExitCode, Stdio};
use std::time::{Duration, Instant};

use refract_core::protocol::{
    Id, LaunchMode, LaunchTicket, Message, Method, Outcome, Reply, Request, Response,
};
use refract_launch::setup::{
    ensure_profile, windows_code_commit, CodeCli, Processes, Runner, WindowCloser, WindowMatch,
};
use refract_launch::windows::{
    close_window_command, closed_windows, WINDOWS_SCRIPT, WINDOWS_SCRIPT_NAME,
};
use refract_launch::{ensure_profile_settings, vscode_args, PROFILE};

const USAGE: &str = "usage: refract-launch <repo-path> [--resume <id>] [--prompt <text>] \
                     [--agent <path>] [--code <path>] [--user-dir <path>] [--vsix <path>] \
                     [--profile-name <name>]";

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

    let cli = CodeCli::WslCmd {
        cwd: "/mnt/c".into(),
    };
    setup_profile(&cli, profile, &user_dir, user.as_deref())?;

    let mut agent = AgentProcess::start(&agent)?;
    let ticket = setup_server(&mut agent, &cli, args.vsix.as_deref())
        .and_then(|()| prepare(&mut agent, &repo, &mode));
    agent.shutdown();
    let ticket = ticket?;
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

/// Steps 1 and 2 of the first-run setup, on the Windows side: the profile with the
/// WSL extension in it.
fn setup_profile(
    cli: &CodeCli,
    profile: &str,
    user_dir: &Path,
    user: Option<&str>,
) -> Result<(), String> {
    let mut runner = Processes;
    let mut closer = PowerShell { user };
    let mut sleep = std::thread::sleep;
    let done = ensure_profile(cli, profile, user_dir, &mut runner, &mut closer, &mut sleep)
        .map_err(|e| format!("cannot set up the {profile} profile: {e}"))?;
    if done.created {
        eprintln!("refract-launch: created the {profile} profile");
    }
    if done.installed_wsl {
        eprintln!("refract-launch: installed the WSL extension in the {profile} profile");
    }
    Ok(())
}

/// Steps 3 and 4, run by the agent's `setupServer`: the Refract extension in the WSL
/// server, applied to every profile. A missing server or vsix is reported and
/// skipped, so the window still opens.
fn setup_server(
    agent: &mut AgentProcess,
    cli: &CodeCli,
    vsix: Option<&Path>,
) -> Result<(), String> {
    let vsix = vsix.unwrap_or(Path::new(DEFAULT_VSIX));
    let vsix =
        std::path::absolute(vsix).map_err(|e| format!("cannot resolve {}: {e}", vsix.display()))?;
    let vsix = vsix
        .to_str()
        .ok_or_else(|| format!("{} is not UTF-8", vsix.display()))?
        .to_owned();
    let commit = windows_code_commit(cli, &mut Processes);
    let response = agent.call(Method::SetupServer { vsix, commit })?;
    match response.outcome {
        Outcome::Result(Reply::ServerSetup(done)) => {
            if let Some(reason) = done.skipped {
                eprintln!("refract-launch: skipping the extension install: {reason}");
            }
            if done.installed {
                eprintln!("refract-launch: installed the Refract extension in the WSL server");
            }
            if done.application_scoped {
                eprintln!("refract-launch: applied the Refract extension to all profiles");
            }
            Ok(())
        }
        Outcome::Error(e) => Err(format!("setupServer failed ({}): {}", e.code, e.message)),
        Outcome::Result(other) => Err(format!("unexpected setupServer result: {other:?}")),
    }
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
        std::fs::write(linux_temp.join(WINDOWS_SCRIPT_NAME), WINDOWS_SCRIPT)?;
        let command = close_window_command(
            "powershell.exe",
            &format!("{windows_temp}\\{WINDOWS_SCRIPT_NAME}"),
            window,
            Some("/mnt/c".into()),
        );
        let out = Processes.run(&command)?;
        if !out.success {
            return Err(io::Error::other(format!(
                "powershell.exe failed: {}",
                out.stderr.trim()
            )));
        }
        for line in closed_windows(&out.stdout) {
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

/// A running `refract-agent`, asked one request at a time.
struct AgentProcess {
    child: Child,
    stdin: ChildStdin,
    lines: io::Lines<BufReader<ChildStdout>>,
    next_id: u64,
}

impl AgentProcess {
    /// Starts the agent and checks that it answers `ping`.
    fn start(agent: &Path) -> Result<AgentProcess, String> {
        let mut child = Command::new(agent)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::inherit())
            .spawn()
            .map_err(|e| format!("cannot start {}: {e}", agent.display()))?;
        let stdin = child.stdin.take().expect("piped stdin");
        let lines = BufReader::new(child.stdout.take().expect("piped stdout")).lines();
        let mut process = AgentProcess {
            child,
            stdin,
            lines,
            next_id: 1,
        };
        process.call(Method::Ping)?;
        Ok(process)
    }

    /// Sends `method` and waits for its response, skipping events.
    fn call(&mut self, method: Method) -> Result<Response, String> {
        let id = Id::from(self.next_id);
        self.next_id += 1;
        let request = Request {
            id: id.clone(),
            method,
        };
        let line = serde_json::to_string(&request).map_err(|e| e.to_string())?;
        writeln!(self.stdin, "{line}").map_err(|e| format!("agent stdin: {e}"))?;
        loop {
            let line = self
                .lines
                .next()
                .ok_or("agent exited without answering")?
                .map_err(|e| format!("agent stdout: {e}"))?;
            let message: Message =
                serde_json::from_str(&line).map_err(|e| format!("agent wrote {line:?}: {e}"))?;
            if let Message::Response(response) = message {
                if response.id.as_ref() == Some(&id) {
                    return Ok(response);
                }
            }
        }
    }

    fn shutdown(mut self) {
        let _ = self.call(Method::Shutdown);
        let _ = self.child.wait();
    }
}

/// Asks the agent for a launch ticket with `prepareLaunch`.
fn prepare(
    agent: &mut AgentProcess,
    repo: &str,
    mode: &LaunchMode,
) -> Result<LaunchTicket, String> {
    let response = agent.call(Method::PrepareLaunch {
        path: repo.to_owned(),
        mode: mode.clone(),
    })?;
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
