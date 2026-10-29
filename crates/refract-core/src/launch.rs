//! Pure pieces of the launch path (see `docs/launch.md`): the command a launch runs,
//! its terminal title and the `vscode-remote` URI of the repo folder.

use crate::protocol::LaunchMode;

/// The command to run in the new terminal. `session_id` is the ticket's session id,
/// which for a resume is the one being resumed. A prompt follows `--`, which ends
/// `claude`'s options, so a prompt starting with `-` is not read as a flag.
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
    }
}

/// The terminal tab's name.
pub fn launch_title(mode: &LaunchMode) -> &'static str {
    match mode {
        LaunchMode::New { .. } => "New agent",
        LaunchMode::Resume { .. } => "Resume",
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
    }

    #[test]
    fn titles() {
        assert_eq!(launch_title(&LaunchMode::New { prompt: None }), "New agent");
        let resume = LaunchMode::Resume {
            session_id: ID.into(),
        };
        assert_eq!(launch_title(&resume), "Resume");
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
