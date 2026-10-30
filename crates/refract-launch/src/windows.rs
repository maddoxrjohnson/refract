//! Closing VS Code windows on Windows with [`WINDOWS_SCRIPT`], run by Windows
//! PowerShell. The caller writes the script to a Windows directory, such as `%TEMP%`,
//! and runs [`close_window_command`]; finding that directory is up to the caller.

use std::path::PathBuf;

use crate::setup::{CliCommand, WindowMatch};

/// The helper that lists and closes VS Code windows. Windows PowerShell runs it from a
/// file, so the caller writes it out first, as [`WINDOWS_SCRIPT_NAME`].
pub const WINDOWS_SCRIPT: &str = include_str!("../../../scripts/vscode-windows.ps1");

/// The file name to give [`WINDOWS_SCRIPT`].
pub const WINDOWS_SCRIPT_NAME: &str = "refract-vscode-windows.ps1";

/// The command that closes the windows matching `window`. `powershell` is
/// `powershell.exe` or its path, and `script` the Windows path of the written
/// [`WINDOWS_SCRIPT`].
pub fn close_window_command(
    powershell: impl Into<PathBuf>,
    script: &str,
    window: &WindowMatch,
    cwd: Option<PathBuf>,
) -> CliCommand {
    CliCommand {
        program: powershell.into(),
        args: vec![
            "-NoProfile".into(),
            "-ExecutionPolicy".into(),
            "Bypass".into(),
            "-File".into(),
            script.into(),
            "-Close".into(),
            window.title.clone(),
            "-Without".into(),
            window.without.clone(),
        ],
        cwd,
    }
}

/// The lines of the script's output that report a closed window, as
/// `closed: <title>`.
pub fn closed_windows(stdout: &str) -> impl Iterator<Item = &str> {
    stdout.lines().filter(|l| l.starts_with("closed:"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::setup::empty_window;

    #[test]
    fn close_command_runs_the_script_file() {
        let window = empty_window("Refract");
        let command = close_window_command(
            "powershell.exe",
            r"C:\Users\dev\AppData\Local\Temp\refract-vscode-windows.ps1",
            &window,
            Some("/mnt/c".into()),
        );
        assert_eq!(command.program, PathBuf::from("powershell.exe"));
        assert_eq!(
            command.args,
            [
                "-NoProfile",
                "-ExecutionPolicy",
                "Bypass",
                "-File",
                r"C:\Users\dev\AppData\Local\Temp\refract-vscode-windows.ps1",
                "-Close",
                &window.title,
                "-Without",
                &window.without,
            ]
        );
        assert_eq!(command.cwd, Some(PathBuf::from("/mnt/c")));
        assert!(WINDOWS_SCRIPT.contains("param("));
    }

    #[test]
    fn closed_lines_are_picked_out() {
        let out = "closed: Refract - Visual Studio Code\r\nnoise\nclosed: Welcome - Refract\n";
        assert_eq!(
            closed_windows(out).collect::<Vec<_>>(),
            [
                "closed: Refract - Visual Studio Code",
                "closed: Welcome - Refract"
            ]
        );
        assert_eq!(closed_windows("").count(), 0);
    }
}
