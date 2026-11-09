//! Whether the Codex CLI is installed (codex spec §3, "Finding it").
//!
//! Codex is usually installed with npm under nvm, so it is on `PATH` only in an
//! interactive shell, and not on the helper's own `PATH`. The quick probe looks for an
//! executable `codex` in the helper's `PATH`, nvm's node versions and the usual npm,
//! pnpm, Volta and Bun bin directories; when that finds nothing, the slow probe asks an
//! interactive shell (`bash -ic 'command -v codex'`), with [`SHELL_TIMEOUT`], since a
//! shell's startup files can take a while.

use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

/// How long the interactive shell gets to answer.
pub const SHELL_TIMEOUT: Duration = Duration::from_secs(10);

/// How often the shell is checked for having exited.
const POLL: Duration = Duration::from_millis(50);

/// The directories the quick probe looks in, from `HOME`, `NVM_DIR` and `PATH`, in that
/// order of preference, without repeats.
pub fn bin_dirs(home: Option<&Path>, nvm_dir: Option<&Path>, path: Option<&str>) -> Vec<PathBuf> {
    let mut dirs: Vec<PathBuf> = path
        .unwrap_or_default()
        .split(':')
        .filter(|d| d.starts_with('/'))
        .map(PathBuf::from)
        .collect();
    let nvm = nvm_dir
        .map(Path::to_path_buf)
        .or_else(|| home.map(|h| h.join(".nvm")));
    if let Some(Ok(versions)) = nvm.map(|n| std::fs::read_dir(n.join("versions/node"))) {
        let mut found: Vec<PathBuf> = versions.flatten().map(|v| v.path().join("bin")).collect();
        found.sort();
        dirs.extend(found.into_iter().rev());
    }
    if let Some(home) = home {
        for sub in [
            ".npm-global/bin",
            ".local/bin",
            ".local/share/pnpm",
            ".volta/bin",
            ".bun/bin",
        ] {
            dirs.push(home.join(sub));
        }
    }
    dirs.extend(["/usr/local/bin", "/usr/bin"].map(PathBuf::from));
    let mut seen = Vec::new();
    dirs.retain(|d| {
        let new = !seen.contains(d);
        seen.push(d.clone());
        new
    });
    dirs
}

/// True when one of `dirs` holds an executable `codex`.
pub fn found_in(dirs: &[PathBuf]) -> bool {
    dirs.iter().any(|dir| {
        std::fs::metadata(dir.join("codex"))
            .is_ok_and(|m| m.is_file() && m.permissions().mode() & 0o111 != 0)
    })
}

/// The quick probe, from the helper's environment.
pub fn quick_probe() -> bool {
    let home = std::env::var_os("HOME").map(PathBuf::from);
    let nvm = std::env::var_os("NVM_DIR")
        .filter(|v| !v.is_empty())
        .map(PathBuf::from);
    let path = std::env::var("PATH").ok();
    found_in(&bin_dirs(home.as_deref(), nvm.as_deref(), path.as_deref()))
}

/// The slow probe: `bash -ic 'command -v codex'` printed a path within
/// [`SHELL_TIMEOUT`]. A shell that takes longer is killed and counts as not found.
pub fn shell_probe() -> bool {
    let child = Command::new("bash")
        .args(["-ic", "command -v codex"])
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn();
    let Ok(mut child) = child else {
        return false;
    };
    let deadline = Instant::now() + SHELL_TIMEOUT;
    loop {
        match child.try_wait() {
            Ok(Some(_)) => break,
            Ok(None) if Instant::now() < deadline => std::thread::sleep(POLL),
            _ => {
                let _ = child.kill();
                let _ = child.wait();
                return false;
            }
        }
    }
    child
        .wait_with_output()
        .is_ok_and(|out| out.status.success() && printed_a_path(&out.stdout))
}

/// `command -v` output naming a program by path; an alias or function prints no path.
fn printed_a_path(stdout: &[u8]) -> bool {
    String::from_utf8_lossy(stdout)
        .lines()
        .any(|line| line.trim().starts_with('/'))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    #[test]
    fn bin_dirs_cover_path_nvm_and_the_usual_places() {
        let tmp = tempfile::tempdir().unwrap();
        let home = tmp.path();
        for version in ["v22.4.0", "v24.1.0"] {
            fs::create_dir_all(home.join(".nvm/versions/node").join(version).join("bin")).unwrap();
        }
        let dirs = bin_dirs(Some(home), None, Some("/usr/bin:relative:/opt/tools/bin"));
        assert_eq!(dirs[0], Path::new("/usr/bin"));
        assert_eq!(dirs[1], Path::new("/opt/tools/bin"));
        // The newest node version first.
        assert_eq!(dirs[2], home.join(".nvm/versions/node/v24.1.0/bin"));
        assert_eq!(dirs[3], home.join(".nvm/versions/node/v22.4.0/bin"));
        assert!(dirs.contains(&home.join(".npm-global/bin")));
        assert_eq!(
            dirs.iter().filter(|d| *d == Path::new("/usr/bin")).count(),
            1
        );
    }

    #[test]
    fn only_an_executable_codex_counts() {
        let tmp = tempfile::tempdir().unwrap();
        let bin = tmp.path().join("bin");
        fs::create_dir(&bin).unwrap();
        let dirs = [bin.clone()];
        assert!(!found_in(&dirs));
        fs::write(bin.join("codex"), "#!/bin/sh\n").unwrap();
        assert!(!found_in(&dirs));
        fs::set_permissions(bin.join("codex"), fs::Permissions::from_mode(0o755)).unwrap();
        assert!(found_in(&dirs));
        fs::create_dir(tmp.path().join("dir")).unwrap();
        fs::create_dir(tmp.path().join("dir/codex")).unwrap();
        assert!(!found_in(&[tmp.path().join("dir")]));
    }

    #[test]
    fn command_v_must_print_a_path() {
        assert!(printed_a_path(
            b"/home/dev/.nvm/versions/node/v24.1.0/bin/codex\n"
        ));
        assert!(!printed_a_path(b"alias codex='npx codex'\n"));
        assert!(!printed_a_path(b""));
    }
}
