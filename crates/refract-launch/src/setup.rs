//! First-run setup that a Refract window needs before it can open (see
//! `docs/launch.md`): the Refract profile with the WSL extension in it, the Refract
//! extension in the WSL server, and that extension applied to every profile. Commands
//! are built by pure functions and run through a [`Runner`], and windows are closed
//! through a [`WindowCloser`], so the Windows app reuses them.

use std::fs;
use std::io::{self, Read, Seek};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, SystemTime};

use serde_json::Value;

use crate::{invalid, read_profile_dir, write_replacing};

/// The extension that lets a Windows VS Code window connect to WSL.
pub const WSL_EXTENSION: &str = "ms-vscode-remote.remote-wsl";

/// The Refract extension's id in the WSL server.
pub const REFRACT_EXTENSION: &str = "refract.refract";

/// A command to run: a program, its arguments and the directory to run it in.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CliCommand {
    pub program: PathBuf,
    pub args: Vec<String>,
    pub cwd: Option<PathBuf>,
}

/// What a [`Runner`] reports about a finished command.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RunOutput {
    pub success: bool,
    pub stdout: String,
    pub stderr: String,
}

/// Runs commands. [`Processes`] runs them for real; tests script the output.
pub trait Runner {
    fn run(&mut self, command: &CliCommand) -> io::Result<RunOutput>;
}

/// A [`Runner`] that starts each command as a process, with no stdin.
pub struct Processes;

impl Runner for Processes {
    fn run(&mut self, command: &CliCommand) -> io::Result<RunOutput> {
        let mut process = Command::new(&command.program);
        process.args(&command.args).stdin(Stdio::null());
        if let Some(cwd) = &command.cwd {
            process.current_dir(cwd);
        }
        let out = process.output()?;
        Ok(RunOutput {
            success: out.status.success(),
            stdout: String::from_utf8_lossy(&out.stdout).into_owned(),
            stderr: String::from_utf8_lossy(&out.stderr).into_owned(),
        })
    }
}

/// How to reach the Windows VS Code's `code` for extension and version commands.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CodeCli {
    /// From WSL: `cmd.exe /c "code …"`, run in `cwd`, a directory on a Windows drive
    /// such as `/mnt/c`. Running VS Code's `bin/code` from WSL instead sends extension
    /// commands to the WSL server.
    WslCmd { cwd: PathBuf },
    /// On Windows: `code.cmd`, or the path to it.
    Direct { program: PathBuf },
}

impl CodeCli {
    /// The command that runs `code` with `args`. Through `cmd.exe` the arguments are
    /// joined into one command line, so they must be plain words with no spaces or
    /// shell characters, as every argument in this module is.
    pub fn command(&self, args: &[&str]) -> CliCommand {
        match self {
            CodeCli::WslCmd { cwd } => {
                let line = std::iter::once("code")
                    .chain(args.iter().copied())
                    .collect::<Vec<_>>()
                    .join(" ");
                CliCommand {
                    program: "cmd.exe".into(),
                    args: vec!["/c".into(), line],
                    cwd: Some(cwd.clone()),
                }
            }
            CodeCli::Direct { program } => CliCommand {
                program: program.clone(),
                args: args.iter().map(|a| (*a).to_owned()).collect(),
                cwd: None,
            },
        }
    }
}

/// How often to look for a profile that `code --new-window` is creating.
pub const PROFILE_POLL: Duration = Duration::from_millis(500);

/// How long to wait for that profile before giving up.
pub const PROFILE_TIMEOUT: Duration = Duration::from_secs(20);

/// Arguments for `code` that list the extensions of the profile `profile`. VS Code
/// fails with "Profile '<name>' not found." until the profile exists.
pub fn list_extensions_args(profile: &str) -> [&str; 3] {
    ["--profile", profile, "--list-extensions"]
}

/// Arguments for `code` that install the WSL extension in the profile `profile`.
pub fn install_wsl_extension_args(profile: &str) -> [&str; 4] {
    ["--profile", profile, "--install-extension", WSL_EXTENSION]
}

/// Arguments for `code` that create the profile `profile`, if it is missing, and open
/// an empty local window in it.
pub fn new_window_args(profile: &str) -> [&str; 3] {
    ["--profile", profile, "--new-window"]
}

/// The VS Code windows to close: those whose title contains `title` and does not
/// contain `without`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WindowMatch {
    pub title: String,
    pub without: String,
}

/// Closes VS Code windows. The operating system part lives with the caller.
pub trait WindowCloser {
    fn close(&mut self, window: &WindowMatch) -> io::Result<()>;
}

/// The empty local window that [`new_window_args`] opens. Its title names the
/// profile; a WSL window's title also does, but contains `[WSL`.
pub fn empty_window(profile: &str) -> WindowMatch {
    WindowMatch {
        title: profile.into(),
        without: "[WSL".into(),
    }
}

/// What [`ensure_profile`] did.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ProfileSetup {
    /// The profile was missing and has been created.
    pub created: bool,
    /// The WSL extension was missing from the profile and has been installed.
    pub installed_wsl: bool,
}

/// Whether `--list-extensions` output names `id`. Ids are compared ignoring case.
pub fn lists_extension(list_output: &str, id: &str) -> bool {
    list_output
        .lines()
        .any(|line| line.trim().eq_ignore_ascii_case(id))
}

/// The commit hash from `code --version` output, which is its second line.
pub fn version_commit(version_output: &str) -> Option<String> {
    let commit = version_output.lines().nth(1)?.trim();
    (commit.len() == 40 && commit.bytes().all(|b| b.is_ascii_hexdigit()))
        .then(|| commit.to_ascii_lowercase())
}

/// Installs the WSL extension in the profile `profile` unless it is listed there. The
/// profile must exist; see [`ensure_profile`]. Returns true when it was installed now.
pub fn ensure_wsl_extension(
    cli: &CodeCli,
    profile: &str,
    runner: &mut impl Runner,
) -> io::Result<bool> {
    let listed = run_ok(runner, &cli.command(&list_extensions_args(profile)))?;
    if lists_extension(&listed, WSL_EXTENSION) {
        return Ok(false);
    }
    run_ok(runner, &cli.command(&install_wsl_extension_args(profile)))?;
    Ok(true)
}

/// Makes sure the profile `profile` exists in VS Code's user directory `user_dir` and
/// has the WSL extension, before any folder is opened in it.
///
/// A missing profile is created with `code --profile <name> --new-window`, which also
/// opens an empty local window. `storage.json` is then checked every [`PROFILE_POLL`],
/// calling `sleep` in between, until the profile appears or [`PROFILE_TIMEOUT`] passes.
/// Once the WSL extension is installed, the empty window is closed through `closer`.
pub fn ensure_profile(
    cli: &CodeCli,
    profile: &str,
    user_dir: &Path,
    runner: &mut impl Runner,
    closer: &mut impl WindowCloser,
    sleep: &mut impl FnMut(Duration),
) -> io::Result<ProfileSetup> {
    let created = read_profile_dir(user_dir, profile)?.is_none();
    if created {
        run_ok(runner, &cli.command(&new_window_args(profile)))?;
        let mut polls = PROFILE_TIMEOUT.as_millis() / PROFILE_POLL.as_millis();
        while read_profile_dir(user_dir, profile)?.is_none() {
            if polls == 0 {
                return Err(io::Error::new(
                    io::ErrorKind::TimedOut,
                    format!(
                        "VS Code did not create the profile {profile} within {}s",
                        PROFILE_TIMEOUT.as_secs()
                    ),
                ));
            }
            polls -= 1;
            sleep(PROFILE_POLL);
        }
    }
    let installed_wsl = ensure_wsl_extension(cli, profile, runner)?;
    if created {
        closer
            .close(&empty_window(profile))
            .map_err(|e| io::Error::new(e.kind(), format!("cannot close the empty window: {e}")))?;
    }
    Ok(ProfileSetup {
        created,
        installed_wsl,
    })
}

/// The Windows VS Code's commit, or `None` if `code --version` gives none.
pub fn windows_code_commit(cli: &CodeCli, runner: &mut impl Runner) -> Option<String> {
    let out = runner.run(&cli.command(&["--version"])).ok()?;
    out.success.then(|| version_commit(&out.stdout)).flatten()
}

/// The commit directory to use among the server's `bin` directories, given as names
/// and modification times: `commit` if it is there, else the most recently modified.
pub fn pick_server_commit(dirs: &[(String, SystemTime)], commit: Option<&str>) -> Option<String> {
    if let Some(commit) = commit {
        if let Some((name, _)) = dirs.iter().find(|(name, _)| name == commit) {
            return Some(name.clone());
        }
    }
    dirs.iter()
        .max_by_key(|(_, modified)| *modified)
        .map(|(name, _)| name.clone())
}

/// The WSL server's `code-server` in `server_dir` (`~/.vscode-server`), for the
/// Windows VS Code's `commit` when known. A `NotFound` error means VS Code has not
/// connected to WSL yet, which is what creates the server.
pub fn server_cli(server_dir: &Path, commit: Option<&str>) -> io::Result<PathBuf> {
    let bin = server_dir.join("bin");
    let not_connected = || {
        io::Error::new(
            io::ErrorKind::NotFound,
            format!(
                "no VS Code server in {}: open any WSL folder in VS Code once, then run again",
                bin.display()
            ),
        )
    };
    let entries = match fs::read_dir(&bin) {
        Ok(entries) => entries,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Err(not_connected()),
        Err(e) => return Err(e),
    };
    let mut dirs = Vec::new();
    for entry in entries {
        let entry = entry?;
        if !entry.path().join("bin/code-server").is_file() {
            continue;
        }
        let modified = entry.metadata()?.modified()?;
        dirs.push((entry.file_name().to_string_lossy().into_owned(), modified));
    }
    let name = pick_server_commit(&dirs, commit).ok_or_else(not_connected)?;
    Ok(bin.join(name).join("bin/code-server"))
}

/// The command that installs `vsix` in the WSL server, replacing any installed copy.
pub fn install_vsix_command(code_server: &Path, vsix: &Path) -> CliCommand {
    CliCommand {
        program: code_server.to_owned(),
        args: vec![
            "--install-extension".into(),
            vsix.to_string_lossy().into_owned(),
            "--force".into(),
        ],
        cwd: None,
    }
}

/// Installs `vsix` with the server's `code_server`.
pub fn install_vsix(code_server: &Path, vsix: &Path, runner: &mut impl Runner) -> io::Result<()> {
    run_ok(runner, &install_vsix_command(code_server, vsix)).map(drop)
}

/// The `version` field of an extension's `package.json`.
pub fn package_version(package_json: &str) -> io::Result<String> {
    let package: Value = serde_json::from_str(package_json)
        .map_err(|e| invalid(format!("package.json does not parse: {e}")))?;
    package
        .get("version")
        .and_then(Value::as_str)
        .map(str::to_owned)
        .ok_or_else(|| invalid("package.json has no version".into()))
}

/// The version of a `.vsix` package, from its `extension/package.json`.
pub fn vsix_version(vsix: impl Read + Seek) -> io::Result<String> {
    let mut archive = zip::ZipArchive::new(vsix)
        .map_err(|e| invalid(format!("the vsix is not a zip file: {e}")))?;
    let mut text = String::new();
    archive
        .by_name("extension/package.json")
        .map_err(|e| invalid(format!("the vsix has no extension/package.json: {e}")))?
        .read_to_string(&mut text)?;
    package_version(&text)
}

/// The installed version of `id`, from the text of the server's
/// `extensions/extensions.json`. `None` when it is not installed or the file does not
/// parse.
pub fn installed_version(extensions_json: &str, id: &str) -> Option<String> {
    let entries: Value = serde_json::from_str(extensions_json).ok()?;
    entries
        .as_array()?
        .iter()
        .find(|e| {
            e.pointer("/identifier/id")
                .and_then(Value::as_str)
                .is_some_and(|i| i.eq_ignore_ascii_case(id))
        })?
        .get("version")?
        .as_str()
        .map(str::to_owned)
}

/// Installs `vsix` with the server's `code_server` unless the server's extension
/// registry (`extensions/extensions.json`) already lists `id` at the vsix's version.
/// Returns true when it was installed now.
pub fn ensure_vsix(
    code_server: &Path,
    vsix: &Path,
    registry: &Path,
    id: &str,
    runner: &mut impl Runner,
) -> io::Result<bool> {
    let version = vsix_version(fs::File::open(vsix)?)
        .map_err(|e| io::Error::new(e.kind(), format!("{}: {e}", vsix.display())))?;
    let installed = match fs::read_to_string(registry) {
        Ok(text) => installed_version(&text, id),
        Err(e) if e.kind() == io::ErrorKind::NotFound => None,
        Err(e) => return Err(e),
    };
    if installed.as_deref() == Some(version.as_str()) {
        return Ok(false);
    }
    install_vsix(code_server, vsix, runner)?;
    Ok(true)
}

/// Sets `metadata.isApplicationScoped` to true on the `id` entry of the text of the
/// server's `extensions/extensions.json`, so the extension applies to every profile.
/// Every other entry and field is kept, in order. Returns `None` when it is already
/// set, and an error when there is no such entry.
pub fn set_application_scoped(extensions_json: &str, id: &str) -> io::Result<Option<String>> {
    let mut entries: Value = serde_json::from_str(extensions_json)
        .map_err(|e| invalid(format!("extensions.json does not parse: {e}")))?;
    let entry = entries
        .as_array_mut()
        .ok_or_else(|| invalid("extensions.json is not an array".into()))?
        .iter_mut()
        .find(|e| {
            e.pointer("/identifier/id")
                .and_then(Value::as_str)
                .is_some_and(|i| i.eq_ignore_ascii_case(id))
        })
        .ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::NotFound,
                format!("{id} is not in extensions.json"),
            )
        })?;
    let entry = entry
        .as_object_mut()
        .ok_or_else(|| invalid(format!("the {id} entry is not an object")))?;
    let metadata = entry
        .entry("metadata")
        .or_insert_with(|| Value::Object(Default::default()));
    if metadata.is_null() {
        *metadata = Value::Object(Default::default());
    }
    let metadata = metadata
        .as_object_mut()
        .ok_or_else(|| invalid(format!("the {id} metadata is not an object")))?;
    if metadata.get("isApplicationScoped") == Some(&Value::Bool(true)) {
        return Ok(None);
    }
    metadata.insert("isApplicationScoped".into(), Value::Bool(true));
    serde_json::to_string(&entries)
        .map(Some)
        .map_err(|e| invalid(e.to_string()))
}

/// Applies [`set_application_scoped`] to the file at `path`, writing through a
/// temporary file. Returns true when the file changed.
pub fn ensure_application_scoped(path: &Path, id: &str) -> io::Result<bool> {
    let text = fs::read_to_string(path)?;
    match set_application_scoped(&text, id)? {
        Some(changed) => write_replacing(path, &changed).map(|()| true),
        None => Ok(false),
    }
}

/// Runs `command` and returns its stdout, or an error naming it if it fails.
fn run_ok(runner: &mut impl Runner, command: &CliCommand) -> io::Result<String> {
    let name = || {
        std::iter::once(command.program.to_string_lossy().into_owned())
            .chain(command.args.iter().cloned())
            .collect::<Vec<_>>()
            .join(" ")
    };
    let out = runner
        .run(command)
        .map_err(|e| io::Error::new(e.kind(), format!("cannot run {}: {e}", name())))?;
    if !out.success {
        return Err(io::Error::other(format!(
            "{} failed: {}",
            name(),
            out.stderr.trim()
        )));
    }
    Ok(out.stdout)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{storage_json, PROFILE};
    use std::cell::RefCell;
    use std::io::Write;
    use std::rc::Rc;

    const COMMIT: &str = "2c33b50b16c472efe58357d1d2a8a5414bc23fbf";

    /// Records commands and answers each with the next scripted output.
    struct Script {
        ran: Vec<CliCommand>,
        outputs: Vec<RunOutput>,
    }

    impl Script {
        fn new(outputs: &[(bool, &str)]) -> Script {
            Script {
                ran: Vec::new(),
                outputs: outputs
                    .iter()
                    .rev()
                    .map(|(success, stdout)| RunOutput {
                        success: *success,
                        stdout: (*stdout).into(),
                        stderr: if *success { "" } else { "boom" }.into(),
                    })
                    .collect(),
            }
        }
    }

    impl Runner for Script {
        fn run(&mut self, command: &CliCommand) -> io::Result<RunOutput> {
            self.ran.push(command.clone());
            self.outputs
                .pop()
                .ok_or_else(|| io::Error::other("unexpected command"))
        }
    }

    fn wsl() -> CodeCli {
        CodeCli::WslCmd {
            cwd: "/mnt/c".into(),
        }
    }

    #[test]
    fn commands_through_cmd_and_direct() {
        assert_eq!(
            wsl().command(&install_wsl_extension_args(PROFILE)),
            CliCommand {
                program: "cmd.exe".into(),
                args: vec![
                    "/c".into(),
                    "code --profile Refract --install-extension ms-vscode-remote.remote-wsl".into()
                ],
                cwd: Some("/mnt/c".into()),
            }
        );
        let direct = CodeCli::Direct {
            program: "code.cmd".into(),
        };
        assert_eq!(
            direct.command(&list_extensions_args(PROFILE)),
            CliCommand {
                program: "code.cmd".into(),
                args: vec![
                    "--profile".into(),
                    "Refract".into(),
                    "--list-extensions".into()
                ],
                cwd: None,
            }
        );
    }

    #[test]
    fn extension_lists_and_versions() {
        let list = "GitHub.copilot\r\nMS-vscode-remote.Remote-WSL\r\n";
        assert!(lists_extension(list, WSL_EXTENSION));
        assert!(!lists_extension(
            "ms-vscode-remote.remote-wsl-recommender\n",
            WSL_EXTENSION
        ));
        assert!(!lists_extension("", WSL_EXTENSION));

        let version = format!("1.106.0\r\n{}\r\nx64\r\n", COMMIT.to_uppercase());
        assert_eq!(version_commit(&version).as_deref(), Some(COMMIT));
        assert_eq!(version_commit("1.106.0\n"), None);
        assert_eq!(version_commit("1.106.0\nnot-a-commit\nx64\n"), None);
    }

    #[test]
    fn wsl_extension_is_installed_only_when_missing() {
        let mut runner = Script::new(&[(true, "ms-vscode-remote.remote-wsl\n")]);
        assert!(!ensure_wsl_extension(&wsl(), PROFILE, &mut runner).unwrap());
        assert_eq!(runner.ran.len(), 1);
        assert_eq!(
            runner.ran[0].args[1],
            "code --profile Refract --list-extensions"
        );

        let mut runner = Script::new(&[(true, ""), (true, "installed\n")]);
        assert!(ensure_wsl_extension(&wsl(), PROFILE, &mut runner).unwrap());
        assert_eq!(runner.ran.len(), 2);
        assert_eq!(
            runner.ran[1],
            wsl().command(&install_wsl_extension_args(PROFILE))
        );

        let mut runner = Script::new(&[(true, ""), (false, "")]);
        let error = ensure_wsl_extension(&wsl(), PROFILE, &mut runner).unwrap_err();
        assert!(error.to_string().contains("boom"), "{error}");
    }

    #[test]
    fn windows_commit_comes_from_code_version() {
        let mut runner = Script::new(&[(true, &format!("1.106.0\n{COMMIT}\nx64\n"))]);
        assert_eq!(
            windows_code_commit(&wsl(), &mut runner).as_deref(),
            Some(COMMIT)
        );
        assert_eq!(runner.ran[0].args[1], "code --version");
        let mut runner = Script::new(&[(false, "")]);
        assert_eq!(windows_code_commit(&wsl(), &mut runner), None);
        let mut runner = Script::new(&[]);
        assert_eq!(windows_code_commit(&wsl(), &mut runner), None);
    }

    #[test]
    fn server_commit_prefers_the_windows_one() {
        let t = SystemTime::UNIX_EPOCH;
        let dirs = vec![
            ("aaaa".to_owned(), t + Duration::from_secs(10)),
            ("bbbb".to_owned(), t + Duration::from_secs(30)),
            ("cccc".to_owned(), t + Duration::from_secs(20)),
        ];
        assert_eq!(
            pick_server_commit(&dirs, Some("aaaa")).as_deref(),
            Some("aaaa")
        );
        assert_eq!(
            pick_server_commit(&dirs, Some("dddd")).as_deref(),
            Some("bbbb")
        );
        assert_eq!(pick_server_commit(&dirs, None).as_deref(), Some("bbbb"));
        assert_eq!(pick_server_commit(&[], Some("aaaa")), None);
    }

    #[test]
    fn server_cli_needs_a_connected_server() {
        let tmp = tempfile::tempdir().unwrap();
        let server = tmp.path().join(".vscode-server");
        let error = server_cli(&server, None).unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::NotFound);
        assert!(error.to_string().contains("open any WSL folder"), "{error}");

        fs::create_dir_all(server.join("bin/partial")).unwrap();
        assert_eq!(
            server_cli(&server, None).unwrap_err().kind(),
            io::ErrorKind::NotFound
        );

        for commit in ["old", COMMIT] {
            fs::create_dir_all(server.join("bin").join(commit).join("bin")).unwrap();
            fs::write(server.join("bin").join(commit).join("bin/code-server"), "").unwrap();
        }
        assert_eq!(
            server_cli(&server, Some(COMMIT)).unwrap(),
            server.join("bin").join(COMMIT).join("bin/code-server")
        );
        assert_eq!(
            server_cli(&server, Some("old")).unwrap(),
            server.join("bin/old/bin/code-server")
        );
    }

    #[test]
    fn vsix_install_command() {
        let command = install_vsix_command(
            Path::new("/home/dev/.vscode-server/bin/abc/bin/code-server"),
            Path::new("/home/dev/refract/vscode-extension/refract.vsix"),
        );
        assert_eq!(
            command.args,
            [
                "--install-extension",
                "/home/dev/refract/vscode-extension/refract.vsix",
                "--force"
            ]
        );
        assert_eq!(command.cwd, None);
    }

    const EXTENSIONS: &str = r#"[{"identifier":{"id":"tidepool.charts","uuid":"1234"},"version":"2.0.1","location":{"$mid":1,"path":"/home/dev/.vscode-server/extensions/tidepool.charts-2.0.1","scheme":"file"},"relativeLocation":"tidepool.charts-2.0.1","metadata":{"isMachineScoped":false,"installedTimestamp":1761000000000,"pinned":false,"source":"gallery"}},{"identifier":{"id":"refract.refract"},"version":"0.1.0","location":{"$mid":1,"path":"/home/dev/.vscode-server/extensions/refract.refract-0.1.0","scheme":"file"},"relativeLocation":"refract.refract-0.1.0","metadata":{"isMachineScoped":false,"isBuiltin":false,"installedTimestamp":1761700000000,"pinned":true,"source":"vsix","isApplicationScoped":false}}]"#;

    #[test]
    fn application_scope_changes_only_that_field() {
        let changed = set_application_scoped(EXTENSIONS, REFRACT_EXTENSION)
            .unwrap()
            .unwrap();
        assert_eq!(
            changed,
            EXTENSIONS.replace(
                r#""isApplicationScoped":false"#,
                r#""isApplicationScoped":true"#
            )
        );
        assert_eq!(
            set_application_scoped(&changed, REFRACT_EXTENSION).unwrap(),
            None
        );
    }

    #[test]
    fn application_scope_is_added_when_absent() {
        let text = r#"[{"identifier":{"id":"Refract.Refract"},"version":"0.1.0","metadata":{"source":"vsix","pinned":true}},{"identifier":{"id":"other.ext"}}]"#;
        let changed = set_application_scoped(text, REFRACT_EXTENSION)
            .unwrap()
            .unwrap();
        assert_eq!(
            changed,
            r#"[{"identifier":{"id":"Refract.Refract"},"version":"0.1.0","metadata":{"source":"vsix","pinned":true,"isApplicationScoped":true}},{"identifier":{"id":"other.ext"}}]"#
        );
        let no_metadata = r#"[{"identifier":{"id":"refract.refract"},"version":"0.1.0"}]"#;
        assert_eq!(
            set_application_scoped(no_metadata, REFRACT_EXTENSION)
                .unwrap()
                .unwrap(),
            r#"[{"identifier":{"id":"refract.refract"},"version":"0.1.0","metadata":{"isApplicationScoped":true}}]"#
        );
    }

    #[test]
    fn application_scope_errors() {
        let missing = r#"[{"identifier":{"id":"tidepool.charts"}}]"#;
        assert_eq!(
            set_application_scoped(missing, REFRACT_EXTENSION)
                .unwrap_err()
                .kind(),
            io::ErrorKind::NotFound
        );
        assert!(set_application_scoped("[]", REFRACT_EXTENSION).is_err());
        assert!(set_application_scoped("{}", REFRACT_EXTENSION).is_err());
        assert!(set_application_scoped("[", REFRACT_EXTENSION).is_err());
    }

    #[test]
    fn application_scope_file_is_replaced_once() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("extensions.json");
        fs::write(&path, EXTENSIONS).unwrap();
        assert!(ensure_application_scoped(&path, REFRACT_EXTENSION).unwrap());
        let text = fs::read_to_string(&path).unwrap();
        assert!(text.contains(r#""isApplicationScoped":true"#));
        assert!(!ensure_application_scoped(&path, REFRACT_EXTENSION).unwrap());
        assert_eq!(fs::read_to_string(&path).unwrap(), text);
        let names: Vec<_> = fs::read_dir(tmp.path())
            .unwrap()
            .map(|e| e.unwrap().file_name())
            .collect();
        assert_eq!(names, ["extensions.json"]);
    }

    const STORAGE_WITH_PROFILE: &str =
        r#"{"userDataProfiles": [{"location": "1f3d9c40", "name": "Refract"}]}"#;

    type Log = Rc<RefCell<Vec<String>>>;

    /// The Windows VS Code as seen through `cmd.exe`: the profile exists once
    /// `storage.json` names it, and until then extension commands for it fail the way
    /// VS Code's do. Every command is logged.
    struct FakeCode {
        user_dir: PathBuf,
        log: Log,
        wsl_installed: bool,
    }

    impl Runner for FakeCode {
        fn run(&mut self, command: &CliCommand) -> io::Result<RunOutput> {
            let line = command.args.join(" ");
            self.log.borrow_mut().push(line.clone());
            let ok = |stdout: &str| RunOutput {
                success: true,
                stdout: stdout.into(),
                stderr: String::new(),
            };
            if line.ends_with("--new-window") || line.contains("--folder-uri") {
                return Ok(ok(""));
            }
            if read_profile_dir(&self.user_dir, PROFILE)?.is_none() {
                return Ok(RunOutput {
                    success: false,
                    stdout: String::new(),
                    stderr: format!("Profile '{PROFILE}' not found.\r\n"),
                });
            }
            if line.ends_with("--list-extensions") {
                let listed = if self.wsl_installed {
                    WSL_EXTENSION
                } else {
                    ""
                };
                return Ok(ok(listed));
            }
            if line.ends_with(&format!("--install-extension {WSL_EXTENSION}")) {
                self.wsl_installed = true;
                return Ok(ok("installed"));
            }
            Err(io::Error::other(format!("unexpected command {line}")))
        }
    }

    struct Closer(Log);

    impl WindowCloser for Closer {
        fn close(&mut self, window: &WindowMatch) -> io::Result<()> {
            self.0.borrow_mut().push(format!(
                "close {:?} without {:?}",
                window.title, window.without
            ));
            Ok(())
        }
    }

    fn fake_code(user_dir: &Path, wsl_installed: bool) -> (FakeCode, Closer, Log) {
        let log = Log::default();
        let code = FakeCode {
            user_dir: user_dir.to_owned(),
            log: log.clone(),
            wsl_installed,
        };
        (code, Closer(log.clone()), log)
    }

    #[test]
    fn listing_a_missing_profile_fails() {
        let tmp = tempfile::tempdir().unwrap();
        let (mut code, _, _) = fake_code(tmp.path(), false);
        let error = ensure_wsl_extension(&wsl(), PROFILE, &mut code).unwrap_err();
        assert!(
            error.to_string().contains("Profile 'Refract' not found."),
            "{error}"
        );
    }

    #[test]
    fn a_missing_profile_is_created_before_the_folder_opens() {
        let tmp = tempfile::tempdir().unwrap();
        let user = tmp.path().to_owned();
        let (mut code, mut closer, log) = fake_code(&user, false);
        let mut polls = 0;
        let mut sleep = |interval: Duration| {
            assert_eq!(interval, PROFILE_POLL);
            log.borrow_mut().push("poll".into());
            polls += 1;
            if polls == 2 {
                fs::create_dir_all(user.join("globalStorage")).unwrap();
                fs::write(storage_json(&user), STORAGE_WITH_PROFILE).unwrap();
            }
        };
        let done =
            ensure_profile(&wsl(), PROFILE, &user, &mut code, &mut closer, &mut sleep).unwrap();
        assert_eq!(
            done,
            ProfileSetup {
                created: true,
                installed_wsl: true
            }
        );
        let open = CodeCli::Direct {
            program: "code".into(),
        };
        let uri = "vscode-remote://wsl+Ubuntu/home/dev/projects/tidepool-api";
        let args = crate::vscode_args(PROFILE, uri);
        let args: Vec<&str> = args.iter().map(String::as_str).collect();
        code.run(&open.command(&args)).unwrap();
        assert_eq!(
            *log.borrow(),
            [
                "/c code --profile Refract --new-window",
                "poll",
                "poll",
                "/c code --profile Refract --list-extensions",
                "/c code --profile Refract --install-extension ms-vscode-remote.remote-wsl",
                r#"close "Refract" without "[WSL""#,
                &format!("--profile Refract --folder-uri {uri}"),
            ]
        );
    }

    #[test]
    fn an_existing_profile_is_left_alone() {
        let tmp = tempfile::tempdir().unwrap();
        let user = tmp.path();
        fs::create_dir_all(user.join("globalStorage")).unwrap();
        fs::write(storage_json(user), STORAGE_WITH_PROFILE).unwrap();
        let (mut code, mut closer, log) = fake_code(user, true);
        let mut sleep = |_| panic!("no wait for an existing profile");
        let done =
            ensure_profile(&wsl(), PROFILE, user, &mut code, &mut closer, &mut sleep).unwrap();
        assert_eq!(
            done,
            ProfileSetup {
                created: false,
                installed_wsl: false
            }
        );
        assert_eq!(
            *log.borrow(),
            ["/c code --profile Refract --list-extensions"]
        );
    }

    #[test]
    fn profile_creation_times_out() {
        let tmp = tempfile::tempdir().unwrap();
        let (mut code, mut closer, log) = fake_code(tmp.path(), false);
        let mut waited = Duration::ZERO;
        let mut sleep = |interval| waited += interval;
        let error = ensure_profile(
            &wsl(),
            "Scratch",
            tmp.path(),
            &mut code,
            &mut closer,
            &mut sleep,
        )
        .unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::TimedOut);
        assert_eq!(waited, PROFILE_TIMEOUT);
        assert_eq!(*log.borrow(), ["/c code --profile Scratch --new-window"]);
    }

    fn vsix_bytes(package_json: &str) -> Vec<u8> {
        let mut zip = zip::ZipWriter::new(io::Cursor::new(Vec::new()));
        let options = zip::write::SimpleFileOptions::default();
        zip.start_file("extension.vsixmanifest", options).unwrap();
        zip.write_all(b"<PackageManifest/>").unwrap();
        zip.start_file("extension/package.json", options).unwrap();
        zip.write_all(package_json.as_bytes()).unwrap();
        zip.finish().unwrap().into_inner()
    }

    #[test]
    fn versions_of_the_vsix_and_the_installed_extension() {
        let vsix = vsix_bytes(r#"{"name": "refract", "version": "0.2.0"}"#);
        assert_eq!(vsix_version(io::Cursor::new(vsix)).unwrap(), "0.2.0");
        let no_version = vsix_bytes(r#"{"name": "refract"}"#);
        assert!(vsix_version(io::Cursor::new(no_version)).is_err());
        assert!(vsix_version(io::Cursor::new(b"not a zip".to_vec())).is_err());

        assert_eq!(
            installed_version(EXTENSIONS, REFRACT_EXTENSION).as_deref(),
            Some("0.1.0")
        );
        assert_eq!(
            installed_version(EXTENSIONS, "tidepool.charts").as_deref(),
            Some("2.0.1")
        );
        assert_eq!(installed_version("[]", REFRACT_EXTENSION), None);
        assert_eq!(installed_version("[", REFRACT_EXTENSION), None);
    }

    #[test]
    fn vsix_is_installed_only_when_the_version_differs() {
        let tmp = tempfile::tempdir().unwrap();
        let vsix = tmp.path().join("refract.vsix");
        fs::write(&vsix, vsix_bytes(r#"{"version": "0.1.0"}"#)).unwrap();
        let registry = tmp.path().join("extensions.json");
        let code_server = Path::new("/home/dev/.vscode-server/bin/abc/bin/code-server");

        let mut runner = Script::new(&[(true, "")]);
        assert!(ensure_vsix(
            code_server,
            &vsix,
            &registry,
            REFRACT_EXTENSION,
            &mut runner
        )
        .unwrap());
        assert_eq!(runner.ran, [install_vsix_command(code_server, &vsix)]);

        fs::write(&registry, EXTENSIONS).unwrap();
        let mut runner = Script::new(&[]);
        assert!(!ensure_vsix(
            code_server,
            &vsix,
            &registry,
            REFRACT_EXTENSION,
            &mut runner
        )
        .unwrap());
        assert!(runner.ran.is_empty());

        fs::write(&vsix, vsix_bytes(r#"{"version": "0.1.1"}"#)).unwrap();
        let mut runner = Script::new(&[(true, "")]);
        assert!(ensure_vsix(
            code_server,
            &vsix,
            &registry,
            REFRACT_EXTENSION,
            &mut runner
        )
        .unwrap());
        assert_eq!(runner.ran.len(), 1);
    }
}
