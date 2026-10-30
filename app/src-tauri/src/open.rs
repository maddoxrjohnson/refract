//! Opens a repo's folder in Explorer and links in the default browser.

use serde::Serialize;
use tauri::{AppHandle, Manager};

use crate::agent::Agent;

/// Why opening a folder or link failed, sent as `{"kind": "invalid", "message": "..."}`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct OpenError {
    pub kind: OpenErrorKind,
    pub message: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub enum OpenErrorKind {
    /// The path or URL was refused before anything was opened.
    Invalid,
    /// The WSL distro the repos live in is not known.
    NoDistro,
    /// Windows could not open it.
    Failed,
    /// The task itself failed.
    Internal,
}

impl OpenError {
    fn new(kind: OpenErrorKind, message: impl Into<String>) -> OpenError {
        OpenError {
            kind,
            message: message.into(),
        }
    }
}

/// The Windows path of a folder in WSL. An absolute Linux path under
/// `/mnt/<drive letter>` is that drive's path (`/mnt/c/dev` is `C:\dev`), assuming
/// the default automount root; any other is under `\\wsl.localhost\<distro>`, which
/// needs `distro`. Repeated `/` and `.` segments are dropped. Relative paths, `..`
/// segments, and `\` or NUL anywhere are refused.
pub fn windows_folder(path: &str, distro: Option<&str>) -> Result<String, OpenError> {
    let invalid = |why: &str| OpenError::new(OpenErrorKind::Invalid, format!("{why}: {path}"));
    if !path.starts_with('/') {
        return Err(invalid("Not an absolute path"));
    }
    if path.contains(['\\', '\0']) {
        return Err(invalid("Cannot open a path with this character in it"));
    }
    let segments: Vec<&str> = path
        .split('/')
        .filter(|s| !s.is_empty() && *s != ".")
        .collect();
    if segments.contains(&"..") {
        return Err(invalid("Cannot open a path with `..` in it"));
    }
    if let ["mnt", drive, rest @ ..] = segments.as_slice() {
        if let [letter] = drive.as_bytes() {
            if letter.is_ascii_alphabetic() {
                let letter = letter.to_ascii_uppercase() as char;
                return Ok(format!("{letter}:\\{}", rest.join("\\")));
            }
        }
    }
    let distro = distro.filter(|d| !d.is_empty()).ok_or_else(|| {
        OpenError::new(
            OpenErrorKind::NoDistro,
            "Refract does not know which WSL distro the repo is in.",
        )
    })?;
    Ok(format!(r"\\wsl.localhost\{distro}\{}", segments.join("\\")))
}

/// Checks that `url` is an `https://` URL with a host and no spaces or control
/// characters, and returns it as it is.
pub fn check_url(url: &str) -> Result<&str, OpenError> {
    let invalid = |why: &str| OpenError::new(OpenErrorKind::Invalid, format!("{why}: {url}"));
    let scheme = url.get(..8).filter(|s| s.eq_ignore_ascii_case("https://"));
    if scheme.is_none() {
        return Err(invalid("Only https links can be opened"));
    }
    if url.chars().any(|c| c.is_whitespace() || c.is_control()) {
        return Err(invalid("Cannot open a link with spaces in it"));
    }
    let host = url[8..].split(['/', '?', '#']).next().unwrap_or("");
    let host = host.rsplit('@').next().unwrap_or("");
    if host.is_empty() || host.starts_with(':') {
        return Err(invalid("The link has no host"));
    }
    Ok(url)
}

/// Checks that `folder` was found and is a directory, given the result of looking it
/// up. Anything else, a program or shortcut included, is refused so that opening
/// a folder can never run a file.
pub fn check_is_dir(folder: &str, is_dir: std::io::Result<bool>) -> Result<(), OpenError> {
    match is_dir {
        Ok(true) => Ok(()),
        Ok(false) => Err(OpenError::new(
            OpenErrorKind::Invalid,
            format!("Not a folder: {folder}"),
        )),
        Err(err) => Err(OpenError::new(
            OpenErrorKind::Invalid,
            format!("Cannot open {folder}, it is not a folder: {err}"),
        )),
    }
}

/// Opens a repo's folder, a Linux path in the helper's distro, in Explorer.
#[tauri::command]
pub async fn open_folder(app: AppHandle, path: String) -> Result<(), OpenError> {
    run_blocking(move || {
        let distro = app.state::<Agent>().distro();
        let folder = windows_folder(&path, distro.as_deref())?;
        check_is_dir(&folder, std::fs::metadata(&folder).map(|m| m.is_dir()))?;
        // `explore` only opens folders, so a file swapped in after the check still
        // does not run.
        shell_open("explore", &folder)
    })
    .await
}

/// Opens an `https://` link in the default browser.
#[tauri::command]
pub async fn open_url(url: String) -> Result<(), OpenError> {
    check_url(&url)?;
    run_blocking(move || shell_open("open", &url)).await
}

async fn run_blocking(
    task: impl FnOnce() -> Result<(), OpenError> + Send + 'static,
) -> Result<(), OpenError> {
    let result = tauri::async_runtime::spawn_blocking(task)
        .await
        .map_err(|err| {
            OpenError::new(
                OpenErrorKind::Internal,
                format!("The open task failed: {err}"),
            )
        })?;
    if let Err(err) = &result {
        eprintln!("refract: opening failed: {}", err.message);
    }
    result
}

/// Opens a folder or link with `ShellExecuteW`: `explore` for a folder, `open` for a
/// link. COM is set up first, as the shell asks of its callers.
#[cfg(windows)]
fn shell_open(verb: &str, target: &str) -> Result<(), OpenError> {
    use windows::core::HSTRING;
    use windows::Win32::System::Com::{
        CoInitializeEx, CoUninitialize, COINIT_APARTMENTTHREADED, COINIT_DISABLE_OLE1DDE,
    };
    use windows::Win32::UI::Shell::ShellExecuteW;
    use windows::Win32::UI::WindowsAndMessaging::SW_SHOWNORMAL;

    // SAFETY: COM is set up and torn down on this thread around one shell call.
    let com = unsafe { CoInitializeEx(None, COINIT_APARTMENTTHREADED | COINIT_DISABLE_OLE1DDE) };
    let code = unsafe {
        ShellExecuteW(
            None,
            &HSTRING::from(verb),
            &HSTRING::from(target),
            None,
            None,
            SW_SHOWNORMAL,
        )
    }
    .0 as isize;
    if com.is_ok() {
        unsafe { CoUninitialize() };
    }
    // Values above 32 mean success; the rest are `SE_ERR_*` codes.
    let why = match code {
        33.. => return Ok(()),
        2 | 3 => "it was not found",
        5 => "access was denied",
        31 => "no program is set up to open it",
        _ => "Windows could not open it",
    };
    Err(OpenError::new(
        OpenErrorKind::Failed,
        format!("Cannot open {target}: {why} (code {code})."),
    ))
}

#[cfg(not(windows))]
fn shell_open(_verb: &str, target: &str) -> Result<(), OpenError> {
    Err(OpenError::new(
        OpenErrorKind::Internal,
        format!("Cannot open {target}: only supported on Windows."),
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn linux_folders_are_under_the_wsl_share() {
        let ok = |path| windows_folder(path, Some("Ubuntu")).unwrap();
        assert_eq!(
            ok("/home/dev/projects/tidepool-api"),
            r"\\wsl.localhost\Ubuntu\home\dev\projects\tidepool-api"
        );
        assert_eq!(
            ok("//home/dev/./projects/tidepool-api/"),
            r"\\wsl.localhost\Ubuntu\home\dev\projects\tidepool-api"
        );
        assert_eq!(ok("/"), r"\\wsl.localhost\Ubuntu\");
        assert_eq!(
            ok("/mnt/wsl/shared"),
            r"\\wsl.localhost\Ubuntu\mnt\wsl\shared"
        );
        assert_eq!(ok("/mnt"), r"\\wsl.localhost\Ubuntu\mnt");
        assert_eq!(
            ok("/home/dev/caf\u{e9} notes"),
            "\\\\wsl.localhost\\Ubuntu\\home\\dev\\caf\u{e9} notes"
        );
    }

    #[test]
    fn drive_folders_are_windows_paths() {
        assert_eq!(
            windows_folder("/mnt/c/Users/dev/lanternfish", None).unwrap(),
            r"C:\Users\dev\lanternfish"
        );
        assert_eq!(windows_folder("/mnt/D/", None).unwrap(), r"D:\");
    }

    #[test]
    fn bad_folders_are_refused() {
        for bad in [
            "",
            "home/dev",
            "./x",
            r"C:\Users",
            r"\\wsl.localhost\Ubuntu\home",
            "/home/dev/../root",
            "/home/a\\b",
            "/home/a\0b",
        ] {
            let err = windows_folder(bad, Some("Ubuntu")).unwrap_err();
            assert_eq!(err.kind, OpenErrorKind::Invalid, "{bad}");
        }
        let err = windows_folder("/home/dev", None).unwrap_err();
        assert_eq!(err.kind, OpenErrorKind::NoDistro);
        assert!(windows_folder("/home/dev", Some("")).is_err());
    }

    #[test]
    fn only_folders_are_opened() {
        let dir = std::env::temp_dir().join(format!("refract-open-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let file = dir.join("tool.exe");
        std::fs::write(&file, b"").unwrap();
        let is_dir = |p: &std::path::Path| std::fs::metadata(p).map(|m| m.is_dir());
        let dir_str = dir.to_string_lossy();
        let file_str = file.to_string_lossy();
        let missing = dir.join("missing");

        assert_eq!(check_is_dir(&dir_str, is_dir(&dir)), Ok(()));
        let err = check_is_dir(&file_str, is_dir(&file)).unwrap_err();
        assert_eq!(err.kind, OpenErrorKind::Invalid);
        assert!(err.message.contains("Not a folder"), "{}", err.message);
        let err = check_is_dir(&missing.to_string_lossy(), is_dir(&missing)).unwrap_err();
        assert_eq!(err.kind, OpenErrorKind::Invalid);

        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn only_https_links_pass() {
        for good in [
            "https://github.com/example/tidepool-api",
            "HTTPS://example.com",
            "https://example.com:8443/a?b=c#d",
            "https://user@example.com/",
        ] {
            assert_eq!(check_url(good), Ok(good));
        }
        for bad in [
            "",
            "http://example.com",
            "file:///C:/Windows",
            "javascript:alert(1)",
            "https://",
            "https:///path",
            "https://:443/",
            "https://example.com/a b",
            "https://example.com/\n",
            "C:\\Windows\\notepad.exe",
            "https:/example.com",
        ] {
            let err = check_url(bad).unwrap_err();
            assert_eq!(err.kind, OpenErrorKind::Invalid, "{bad}");
        }
    }

    #[test]
    fn errors_serialize_with_a_kind() {
        let err = check_url("http://x").unwrap_err();
        assert_eq!(
            serde_json::to_value(&err).unwrap(),
            serde_json::json!({
                "kind": "invalid",
                "message": "Only https links can be opened: http://x",
            })
        );
        assert_eq!(
            serde_json::to_value(OpenErrorKind::NoDistro).unwrap(),
            "noDistro"
        );
    }
}
