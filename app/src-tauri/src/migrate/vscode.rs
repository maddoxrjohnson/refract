//! Which VS Code windows have the source folder open (spec §2.2, §5 check 4), told
//! from their titles.
//!
//! VS Code's default title is `<dirty>file - <folder> - <profile> - Visual Studio
//! Code`, its parts joined by ` - `, and a remote window's folder part carries a
//! suffix such as ` [WSL: Ubuntu]`. A window counts when one part is the folder's
//! name, alone or with a `[WSL…]` suffix. Windows opened on the folder from Windows
//! (no suffix) count too: they hold the same files. Only the name shows, so another
//! folder of the same name also matches; the dialog shows each window's title next
//! to its **Close** button. The windows themselves are found by `win`, among the
//! visible top-level windows of `Code.exe` processes.

use serde::Serialize;

/// A VS Code window that has the folder open.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct VsCodeWindow {
    /// The window handle, for `migrate_close_window`.
    pub id: u64,
    pub title: String,
}

/// True when `title` names `folder` as the open folder (see the module docs).
pub fn title_matches(title: &str, folder: &str) -> bool {
    if folder.is_empty() {
        return false;
    }
    title.split(" - ").any(|part| {
        let part = part.trim_start_matches('●').trim();
        match part.strip_prefix(folder) {
            Some("") => true,
            Some(rest) => {
                let rest = rest.trim_start();
                rest.starts_with("[WSL") && rest.ends_with(']')
            }
            None => false,
        }
    })
}

/// The executable names of VS Code's windows.
pub fn is_vscode_exe(file_name: &str) -> bool {
    ["Code.exe", "Code - Insiders.exe"]
        .iter()
        .any(|exe| file_name.eq_ignore_ascii_case(exe))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn titles_name_the_folder_with_or_without_the_wsl_suffix() {
        let folder = "aurora-notes";
        for title in [
            "README.md - aurora-notes [WSL: Ubuntu] - Refract - Visual Studio Code",
            "● main.rs - aurora-notes [WSL: Ubuntu-24.04] - Visual Studio Code",
            "aurora-notes [WSL: Ubuntu] - Visual Studio Code",
            "Welcome - aurora-notes - Visual Studio Code",
            "aurora-notes - Visual Studio Code",
        ] {
            assert!(title_matches(title, folder), "{title}");
        }
        for title in [
            "aurora-notes-2 [WSL: Ubuntu] - Visual Studio Code",
            "README.md - aurora - Visual Studio Code",
            "my aurora-notes - Visual Studio Code",
            "aurora-notes (Workspace) - Visual Studio Code",
            "Visual Studio Code",
        ] {
            assert!(!title_matches(title, folder), "{title}");
        }
        assert!(!title_matches("x - Visual Studio Code", ""));
    }

    #[test]
    fn only_vscode_processes_count() {
        assert!(is_vscode_exe("Code.exe"));
        assert!(is_vscode_exe("code.EXE"));
        assert!(is_vscode_exe("Code - Insiders.exe"));
        assert!(!is_vscode_exe("notepad.exe"));
    }
}
