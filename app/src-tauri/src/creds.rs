//! Tokens for GitHub and GitLab. Stored ones live in Windows Credential Manager as
//! generic credentials; a GitHub token can also come from the GitHub CLI's login.
//! Tokens are never written to files or logs, or sent to the interface.

use std::io;
use std::process::{Command, Stdio};
use std::time::Duration;

use crate::agent::{neutral_dir, no_window};
use crate::process::output_with_deadline;

pub const GITHUB_TARGET: &str = "refract:github";
pub const GITLAB_TARGET: &str = "refract:gitlab";

/// How long `gh auth token` gets, on Windows or in WSL (which may have to start).
const GH_TIMEOUT: Duration = Duration::from_secs(15);
/// Longer than any real token; Credential Manager allows 2,560 bytes.
const MAX_TOKEN_LEN: usize = 1024;

/// Where a GitHub token came from.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GithubSource {
    Stored,
    GhCli,
}

/// Checks a token pasted into settings: trimmed, not empty, no spaces or control
/// characters, and not overly long. The error never repeats the token.
pub fn check_token(token: &str) -> Result<&str, String> {
    let token = token.trim();
    if token.is_empty() {
        return Err("The token is empty.".into());
    }
    if token.len() > MAX_TOKEN_LEN {
        return Err("The token is too long to be a real one.".into());
    }
    if token.chars().any(|c| c.is_whitespace() || c.is_control()) {
        return Err("The token has spaces or line breaks in it; paste just the token.".into());
    }
    Ok(token)
}

/// Prefixes of the tokens GitHub issues.
const GITHUB_TOKEN_PREFIXES: [&str; 5] = ["gho_", "ghp_", "ghu_", "ghs_", "github_pat_"];

/// The token `gh auth token` printed: its last non-empty line, since a login shell's
/// profile may print first, if that looks like a GitHub token.
pub fn parse_gh_token(stdout: &[u8]) -> Option<String> {
    let text = String::from_utf8_lossy(stdout);
    let line = text.lines().map(str::trim).rfind(|l| !l.is_empty())?;
    let token = check_token(line).ok()?;
    let shaped = GITHUB_TOKEN_PREFIXES
        .iter()
        .any(|prefix| token.len() > prefix.len() && token.starts_with(prefix));
    shaped.then(|| token.to_owned())
}

/// The GitHub token to use: the stored one, else the Windows GitHub CLI's, else the
/// one from `gh` in WSL (`distro` names the helper's distro; only called if needed).
pub fn github_token(distro: impl FnOnce() -> Option<String>) -> Option<(String, GithubSource)> {
    match read(GITHUB_TARGET) {
        Ok(Some(token)) => return Some((token, GithubSource::Stored)),
        Ok(None) => {}
        Err(err) => eprintln!("refract: cannot read the stored GitHub token: {err}"),
    }
    gh_cli_token(distro).map(|token| (token, GithubSource::GhCli))
}

/// The GitHub CLI's token: `gh.exe` on Windows, then `gh` in WSL.
pub fn gh_cli_token(distro: impl FnOnce() -> Option<String>) -> Option<String> {
    let mut windows = Command::new("gh.exe");
    windows.args(["auth", "token"]);
    if let Some(token) = run_for_token(windows) {
        return Some(token);
    }
    let mut wsl = Command::new("wsl.exe");
    wsl.args(wsl_gh_args(distro().as_deref()))
        .env("WSL_UTF8", "1");
    run_for_token(wsl)
}

/// Arguments for `wsl.exe` that print `gh`'s token. A login shell, so `gh` is found
/// where the user's profile puts it on `PATH` (such as `~/.local/bin`).
pub fn wsl_gh_args(distro: Option<&str>) -> Vec<String> {
    let mut args = Vec::new();
    if let Some(distro) = distro.filter(|d| !d.is_empty()) {
        args.extend(["-d".to_owned(), distro.to_owned()]);
    }
    args.extend(["--exec", "sh", "-lc", "gh auth token"].map(str::to_owned));
    args
}

/// Runs `command` quietly and reads a token from its output. Failures are logged
/// by exit status only, since `gh` may print account details.
fn run_for_token(mut command: Command) -> Option<String> {
    command.current_dir(neutral_dir()).stdin(Stdio::null());
    no_window(&mut command);
    let program = command.get_program().to_string_lossy().into_owned();
    match output_with_deadline(&mut command, GH_TIMEOUT) {
        Ok(out) if out.status.success() => parse_gh_token(&out.stdout),
        Ok(out) => {
            eprintln!("refract: {program} gh auth token failed ({})", out.status);
            None
        }
        Err(err) if err.kind() == io::ErrorKind::NotFound => None,
        Err(err) => {
            eprintln!("refract: running {program} for gh auth token: {err}");
            None
        }
    }
}

/// The secret stored under `target`, or `None` if there is none.
#[cfg(windows)]
pub fn read(target: &str) -> io::Result<Option<String>> {
    use windows::core::HSTRING;
    use windows::Win32::Foundation::ERROR_NOT_FOUND;
    use windows::Win32::Security::Credentials::{
        CredFree, CredReadW, CREDENTIALW, CRED_TYPE_GENERIC,
    };

    let mut credential: *mut CREDENTIALW = std::ptr::null_mut();
    // SAFETY: on success `credential` points to a block Windows allocated, read here
    // and released with `CredFree`.
    let found = unsafe {
        CredReadW(
            &HSTRING::from(target),
            CRED_TYPE_GENERIC,
            None,
            &mut credential,
        )
    };
    if let Err(err) = found {
        if err.code() == ERROR_NOT_FOUND.to_hresult() {
            return Ok(None);
        }
        return Err(io::Error::other(err.message()));
    }
    let secret = unsafe {
        let blob = &*credential;
        let bytes = if blob.CredentialBlob.is_null() {
            &[][..]
        } else {
            std::slice::from_raw_parts(blob.CredentialBlob, blob.CredentialBlobSize as usize)
        };
        let secret = String::from_utf8(bytes.to_vec());
        CredFree(credential as *const _);
        secret
    };
    match secret {
        Ok(secret) if !secret.is_empty() => Ok(Some(secret)),
        Ok(_) => Ok(None),
        Err(_) => Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!("the credential {target} is not text Refract wrote"),
        )),
    }
}

/// Stores `secret` under `target` for this Windows user, replacing any there.
#[cfg(windows)]
pub fn write(target: &str, secret: &str) -> io::Result<()> {
    use windows::core::PWSTR;
    use windows::Win32::Security::Credentials::{
        CredWriteW, CREDENTIALW, CRED_PERSIST_LOCAL_MACHINE, CRED_TYPE_GENERIC,
    };

    let mut target_name: Vec<u16> = target.encode_utf16().chain([0]).collect();
    let mut user_name: Vec<u16> = "refract".encode_utf16().chain([0]).collect();
    let mut blob = secret.as_bytes().to_vec();
    let credential = CREDENTIALW {
        Type: CRED_TYPE_GENERIC,
        TargetName: PWSTR(target_name.as_mut_ptr()),
        CredentialBlobSize: blob.len() as u32,
        CredentialBlob: blob.as_mut_ptr(),
        Persist: CRED_PERSIST_LOCAL_MACHINE,
        UserName: PWSTR(user_name.as_mut_ptr()),
        ..Default::default()
    };
    // SAFETY: every pointer in `credential` is to a buffer that outlives the call.
    let written = unsafe { CredWriteW(&credential, 0) };
    blob.fill(0);
    written.map_err(|err| io::Error::other(err.message()))
}

/// Removes the secret under `target`; there being none is not an error.
#[cfg(windows)]
pub fn delete(target: &str) -> io::Result<()> {
    use windows::core::HSTRING;
    use windows::Win32::Foundation::ERROR_NOT_FOUND;
    use windows::Win32::Security::Credentials::{CredDeleteW, CRED_TYPE_GENERIC};

    // SAFETY: the target name is a valid string for the length of the call.
    match unsafe { CredDeleteW(&HSTRING::from(target), CRED_TYPE_GENERIC, None) } {
        Ok(()) => Ok(()),
        Err(err) if err.code() == ERROR_NOT_FOUND.to_hresult() => Ok(()),
        Err(err) => Err(io::Error::other(err.message())),
    }
}

#[cfg(not(windows))]
pub fn read(_target: &str) -> io::Result<Option<String>> {
    Ok(None)
}

#[cfg(not(windows))]
pub fn write(_target: &str, _secret: &str) -> io::Result<()> {
    Err(io::Error::other("Credential Manager is only on Windows"))
}

#[cfg(not(windows))]
pub fn delete(_target: &str) -> io::Result<()> {
    Ok(())
}

#[cfg(test)]
pub mod tests {
    use super::*;
    use crate::agent::{choose_distro, parse_distros};

    /// The distro the app would pick, for tests that reach WSL.
    pub fn test_distro() -> Option<String> {
        let mut command = Command::new("wsl.exe");
        command
            .args(["--list", "--verbose"])
            .env("WSL_UTF8", "1")
            .current_dir(neutral_dir())
            .stdin(Stdio::null());
        let out = output_with_deadline(&mut command, Duration::from_secs(10)).ok()?;
        choose_distro(&parse_distros(&String::from_utf8_lossy(&out.stdout)))
    }

    #[test]
    fn pasted_tokens_are_checked() {
        assert_eq!(check_token("  ghp_example123\n"), Ok("ghp_example123"));
        assert!(check_token("").is_err());
        assert!(check_token(" \n").is_err());
        assert!(check_token("ghp_a ghp_b").is_err());
        assert!(check_token(&"x".repeat(MAX_TOKEN_LEN + 1)).is_err());
        let err = check_token("secret value").unwrap_err();
        assert!(!err.contains("secret"), "{err}");
    }

    #[test]
    fn gh_output_is_read() {
        assert_eq!(
            parse_gh_token(b"gho_example123\n").as_deref(),
            Some("gho_example123")
        );
        assert_eq!(
            parse_gh_token(b"gho_example123\r\n").as_deref(),
            Some("gho_example123")
        );
        assert_eq!(parse_gh_token(b""), None);
        assert_eq!(
            parse_gh_token(b"no oauth token found for github.com\n"),
            None
        );
        // A login shell's profile may print before `gh` does.
        assert_eq!(
            parse_gh_token(b"Welcome back!\nnvm: using node 20\ngho_example123\n\n").as_deref(),
            Some("gho_example123")
        );
        for token in ["ghp_x", "ghu_x", "ghs_x", "github_pat_x"] {
            let out = format!("{token}\n");
            assert_eq!(parse_gh_token(out.as_bytes()).as_deref(), Some(token));
        }
        for bad in [
            &b"gho_example123\nprofile says bye\n"[..],
            b"Welcome back!\n",
            b"gho_\n",
            b"glpat-example123\n",
            b"gho_a gho_b\n",
        ] {
            assert_eq!(
                parse_gh_token(bad),
                None,
                "{}",
                String::from_utf8_lossy(bad)
            );
        }
    }

    #[test]
    fn wsl_runs_gh_in_a_login_shell() {
        assert_eq!(
            wsl_gh_args(Some("Ubuntu")),
            ["-d", "Ubuntu", "--exec", "sh", "-lc", "gh auth token"]
        );
        assert_eq!(wsl_gh_args(None), wsl_gh_args(Some("")).as_slice()[..]);
        assert_eq!(wsl_gh_args(None), ["--exec", "sh", "-lc", "gh auth token"]);
    }

    #[cfg(windows)]
    #[test]
    fn credentials_round_trip() {
        let target = format!("refract:test-{}", std::process::id());
        assert_eq!(read(&target).unwrap(), None);
        write(&target, "glpat-example-1").unwrap();
        assert_eq!(read(&target).unwrap().as_deref(), Some("glpat-example-1"));
        write(&target, "glpat-example-2").unwrap();
        assert_eq!(read(&target).unwrap().as_deref(), Some("glpat-example-2"));
        delete(&target).unwrap();
        assert_eq!(read(&target).unwrap(), None);
        delete(&target).unwrap();
    }
}
