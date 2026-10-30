//! Pure pieces of the launch path (see `docs/launch.md`): the command a launch runs,
//! its terminal title, the `vscode-remote` URI of the repo folder, and the Linux path
//! of a file the Windows app names.

use crate::protocol::LaunchMode;

/// The command to run in the new terminal. `session_id` is the ticket's session id,
/// which for a resume is the one being resumed. A prompt follows `--`, which ends
/// `claude`'s options, so a prompt starting with `-` is not read as a flag. A focus
/// starts nothing, so its command is empty.
pub fn launch_argv(mode: &LaunchMode, session_id: &str) -> Vec<String> {
    match mode {
        LaunchMode::New { prompt } => {
            let mut argv = vec!["claude".into(), "--session-id".into(), session_id.into()];
            if let Some(prompt) = prompt.as_deref().filter(|p| !p.is_empty()) {
                argv.push("--".into());
                argv.push(prompt.into());
            }
            argv
        }
        LaunchMode::Resume { .. } => {
            vec!["claude".into(), "--resume".into(), session_id.into()]
        }
        LaunchMode::Focus { .. } => Vec::new(),
    }
}

/// The terminal tab's name.
pub fn launch_title(mode: &LaunchMode) -> &'static str {
    match mode {
        LaunchMode::New { .. } => "New agent",
        LaunchMode::Resume { .. } => "Resume",
        LaunchMode::Focus { .. } => "Running agent",
    }
}

/// The URI VS Code opens a WSL folder by: `vscode-remote://wsl+<distro><path>`, with
/// the distro and each path segment percent-encoded and any trailing slash removed.
pub fn folder_uri(distro: &str, path: &str) -> String {
    let trimmed = path.trim_end_matches('/');
    let path = if trimmed.is_empty() { "/" } else { trimmed };
    let segments: Vec<String> = path.split('/').map(percent_encode).collect();
    format!(
        "vscode-remote://wsl+{}{}",
        percent_encode(distro),
        segments.join("/")
    )
}

/// The Linux path of a file named from Windows or WSL, without running `wslpath`:
///
/// - a Linux path (`/home/dev/refract.vsix`) as it is, split only on `/` because `\`
///   is an ordinary character in a Linux file name;
/// - a drive path (`C:\Users\dev\refract.vsix`, `c:/Users/…` or the verbatim
///   `\\?\C:\Users\…` that `canonicalize` returns) under `/mnt/<drive>`;
/// - a WSL path (`\\wsl.localhost\<distro>\home\…`, `\\wsl$\<distro>\…` or the
///   verbatim `\\?\UNC\wsl.localhost\<distro>\…`) under `/`.
///
/// Repeated and trailing separators are dropped. The distro of a WSL path is not
/// checked. Anything else, such as a relative path, `C:file`, another share or another
/// verbatim form (`\\?\Volume{…}\…`), is `None`.
pub fn wsl_path(path: &str) -> Option<String> {
    if path.contains('\0') {
        return None;
    }
    if path.starts_with('/') && !path.starts_with("//") {
        return Some(join("", path, |c| c == '/'));
    }
    if let Some(verbatim) = path.strip_prefix(r"\\?\") {
        return match verbatim.get(..4) {
            Some(unc) if unc.eq_ignore_ascii_case(r"UNC\") => wsl_share(&verbatim[4..]),
            _ => drive_path(verbatim),
        };
    }
    if let Some(linux) = drive_path(path) {
        return Some(linux);
    }
    let share = path
        .strip_prefix("\\\\")
        .or_else(|| path.strip_prefix("//"))?;
    wsl_share(share)
}

fn is_windows_sep(c: char) -> bool {
    c == '\\' || c == '/'
}

/// `root` followed by the non-empty segments of `rest`, with no trailing `/`.
fn join(root: &str, rest: &str, is_sep: fn(char) -> bool) -> String {
    let segments: Vec<&str> = rest.split(is_sep).filter(|s| !s.is_empty()).collect();
    let joined = format!("{root}/{}", segments.join("/"));
    match joined.trim_end_matches('/') {
        "" => "/".to_owned(),
        trimmed => trimmed.to_owned(),
    }
}

/// `C:\…` or `C:/…` under `/mnt/c`.
fn drive_path(path: &str) -> Option<String> {
    let bytes = path.as_bytes();
    if bytes.len() < 3 || !bytes[0].is_ascii_alphabetic() || bytes[1] != b':' {
        return None;
    }
    let drive = (bytes[0] as char).to_ascii_lowercase();
    path[2..]
        .starts_with(is_windows_sep)
        .then(|| join(&format!("/mnt/{drive}"), &path[2..], is_windows_sep))
}

/// `wsl.localhost\<distro>\…` or `wsl$\<distro>\…`, the part of a share path after
/// its leading `\\`, under `/`.
fn wsl_share(share: &str) -> Option<String> {
    let (host, rest) = share.split_once(is_windows_sep)?;
    if !host.eq_ignore_ascii_case("wsl.localhost") && host != "wsl$" {
        return None;
    }
    let (distro, rest) = rest.split_once(is_windows_sep).unwrap_or((rest, ""));
    (!distro.is_empty()).then(|| join("", rest, is_windows_sep))
}

/// Percent-encodes every byte except the URI unreserved characters.
fn percent_encode(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for &b in text.as_bytes() {
        if b.is_ascii_alphanumeric() || matches!(b, b'-' | b'.' | b'_' | b'~') {
            out.push(b as char);
        } else {
            out.push_str(&format!("%{b:02X}"));
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    const ID: &str = "0b7e0000-0000-4000-8000-000000000002";

    #[test]
    fn argv_for_each_mode() {
        assert_eq!(
            launch_argv(&LaunchMode::New { prompt: None }, ID),
            ["claude", "--session-id", ID]
        );
        assert_eq!(
            launch_argv(
                &LaunchMode::New {
                    prompt: Some("Add tide tables".into())
                },
                ID
            ),
            ["claude", "--session-id", ID, "--", "Add tide tables"]
        );
        assert_eq!(
            launch_argv(
                &LaunchMode::New {
                    prompt: Some("--help me".into())
                },
                ID
            ),
            ["claude", "--session-id", ID, "--", "--help me"]
        );
        assert_eq!(
            launch_argv(
                &LaunchMode::New {
                    prompt: Some(String::new())
                },
                ID
            ),
            ["claude", "--session-id", ID]
        );
        let resume = LaunchMode::Resume {
            session_id: ID.into(),
        };
        assert_eq!(launch_argv(&resume, ID), ["claude", "--resume", ID]);
        let focus = LaunchMode::Focus {
            session_id: ID.into(),
        };
        assert!(launch_argv(&focus, ID).is_empty());
    }

    #[test]
    fn titles() {
        assert_eq!(launch_title(&LaunchMode::New { prompt: None }), "New agent");
        let resume = LaunchMode::Resume {
            session_id: ID.into(),
        };
        assert_eq!(launch_title(&resume), "Resume");
        let focus = LaunchMode::Focus {
            session_id: ID.into(),
        };
        assert_eq!(launch_title(&focus), "Running agent");
    }

    #[test]
    fn linux_paths_are_kept() {
        assert_eq!(
            wsl_path("/home/dev/refract.vsix").as_deref(),
            Some("/home/dev/refract.vsix")
        );
        assert_eq!(
            wsl_path("/home//dev/dist/").as_deref(),
            Some("/home/dev/dist")
        );
        assert_eq!(wsl_path("/").as_deref(), Some("/"));
        // A backslash is part of a Linux file name, not a separator.
        assert_eq!(
            wsl_path(r"/home/dev/a\b.vsix").as_deref(),
            Some(r"/home/dev/a\b.vsix")
        );
        assert_eq!(wsl_path(r"/tmp\").as_deref(), Some(r"/tmp\"));
    }

    #[test]
    fn verbatim_paths_are_read() {
        for (windows, linux) in [
            (
                r"\\?\C:\Program Files\Refract\resources\refract.vsix",
                "/mnt/c/Program Files/Refract/resources/refract.vsix",
            ),
            (r"\\?\d:\tools\", "/mnt/d/tools"),
            (
                r"\\?\UNC\wsl.localhost\Ubuntu\home\dev\refract.vsix",
                "/home/dev/refract.vsix",
            ),
            (
                r"\\?\unc\wsl$\Ubuntu-22.04\home\dev\refract.vsix",
                "/home/dev/refract.vsix",
            ),
            (r"\\?\UNC\wsl$\Ubuntu", "/"),
        ] {
            assert_eq!(wsl_path(windows).as_deref(), Some(linux), "{windows}");
        }
    }

    #[test]
    fn drive_paths_go_under_mnt() {
        for (windows, linux) in [
            (
                r"C:\Users\dev\Downloads\refract.vsix",
                "/mnt/c/Users/dev/Downloads/refract.vsix",
            ),
            (r"d:\tools\refract.vsix", "/mnt/d/tools/refract.vsix"),
            ("C:/Users/dev/refract.vsix", "/mnt/c/Users/dev/refract.vsix"),
            (r"C:\Users/dev\\My Files\", "/mnt/c/Users/dev/My Files"),
            (r"C:\", "/mnt/c"),
        ] {
            assert_eq!(wsl_path(windows).as_deref(), Some(linux), "{windows}");
        }
    }

    #[test]
    fn wsl_paths_lose_their_distro() {
        for (windows, linux) in [
            (
                r"\\wsl.localhost\Ubuntu\home\dev\refract.vsix",
                "/home/dev/refract.vsix",
            ),
            (
                r"\\wsl$\Ubuntu-22.04\home\dev\refract.vsix",
                "/home/dev/refract.vsix",
            ),
            (r"\\WSL.LOCALHOST\my-distro\home\dev\", "/home/dev"),
            (
                "//wsl.localhost/Ubuntu/tmp/refract.vsix",
                "/tmp/refract.vsix",
            ),
            (r"\\wsl$\Ubuntu", "/"),
            (r"\\wsl$\Ubuntu\", "/"),
        ] {
            assert_eq!(wsl_path(windows).as_deref(), Some(linux), "{windows}");
        }
    }

    #[test]
    fn other_paths_are_rejected() {
        for path in [
            "",
            "refract.vsix",
            "./refract.vsix",
            "../dist/refract.vsix",
            "C:",
            "C:refract.vsix",
            "1:\\x",
            r"\\server\share\refract.vsix",
            r"\\wsl$",
            r"\\wsl$\",
            r"\\wsl$\\home",
            "//server/share",
            "~/refract.vsix",
            "/home/dev/\0x",
            "https://example.com/refract.vsix",
            r"\\?\",
            r"\\?\C:",
            r"\\?\C:refract.vsix",
            r"\\?\Volume{0b7e0000-0000-4000-8000-000000000002}\refract.vsix",
            r"\\?\GLOBALROOT\Device\HarddiskVolume1\refract.vsix",
            r"\\?\UNC\server\share\refract.vsix",
            r"\\?\UNC\wsl$",
            r"\\?\UNC\wsl.localhost\\home",
            r"\\.\C:\refract.vsix",
        ] {
            assert_eq!(wsl_path(path), None, "{path:?}");
        }
    }

    #[test]
    fn folder_uris_encode_segments() {
        assert_eq!(
            folder_uri("Ubuntu", "/home/dev/projects/tidepool-api"),
            "vscode-remote://wsl+Ubuntu/home/dev/projects/tidepool-api"
        );
        assert_eq!(
            folder_uri("Ubuntu-22.04", "/home/dev/projects/tidepool-api/"),
            "vscode-remote://wsl+Ubuntu-22.04/home/dev/projects/tidepool-api"
        );
        assert_eq!(
            folder_uri("Ubuntu", "/home/dev/my repo/50%#1?x"),
            "vscode-remote://wsl+Ubuntu/home/dev/my%20repo/50%25%231%3Fx"
        );
        assert_eq!(
            folder_uri("Ubuntu", "/home/dev/café"),
            "vscode-remote://wsl+Ubuntu/home/dev/caf%C3%A9"
        );
        assert_eq!(folder_uri("Ubuntu", "/"), "vscode-remote://wsl+Ubuntu/");
    }
}
