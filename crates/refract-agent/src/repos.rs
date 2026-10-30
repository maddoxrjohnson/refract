//! `cloneRepo` and `addRepo`: bringing a repo into view. Cloning can take minutes, so
//! the server hands each [`Job`] to a worker thread; the server then reports the new
//! repo itself, since it owns the repo list.
//!
//! `cloneRepo` params are checked for `https://` and a plain folder name where the
//! request is parsed ([`refract_core::protocol::check_clone`]); the functions here take
//! any URL git accepts, so tests can clone from a local path.

use std::io::Read;
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use refract_core::protocol::Id;

use crate::git::redact_userinfo;
use crate::scan;

/// Longest a clone may run before it is stopped.
pub const CLONE_TIMEOUT: Duration = Duration::from_secs(10 * 60);

/// How often a running clone is checked for having finished.
const POLL: Duration = Duration::from_millis(100);

/// How long a stopped clone gets to exit after SIGTERM before it is killed.
const GRACE: Duration = Duration::from_secs(2);

/// One `cloneRepo` request.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Job {
    pub id: Id,
    pub url: String,
    pub name: String,
}

/// Runs `job` into `$HOME/projects`. The error is a message for the app.
pub fn run(job: &Job) -> Result<PathBuf, String> {
    let Some(home) = std::env::var_os("HOME") else {
        return Err("HOME is not set".into());
    };
    let dest = Path::new(&home).join("projects").join(&job.name);
    clone_into(&job.url, &dest, CLONE_TIMEOUT)
}

/// Clones `url` to `dest`, creating its parent if needed, and returns `dest`. When
/// `dest` already exists it is returned as it is if it is a repo with a commit checked
/// out whose origin is `url` (see [`same_origin`]), and is an error otherwise. git
/// clones into a hidden sibling first and the result is renamed into place, so a clone
/// that fails or is killed never leaves a half-written `dest` behind. git never
/// prompts: a clone that needs credentials git cannot get on its own fails. A clone
/// still running after `timeout` is stopped and what it wrote is removed.
pub fn clone_into(url: &str, dest: &Path, timeout: Duration) -> Result<PathBuf, String> {
    if std::fs::symlink_metadata(dest).is_ok() {
        let reusable = scan::is_repo(dest)
            && has_head(dest)
            && origin_url(dest).is_some_and(|origin| same_origin(&origin, url));
        if !reusable {
            return Err(format!("{} already exists", dest.display()));
        }
        return Ok(dest.to_owned());
    }
    let (Some(parent), Some(name)) = (dest.parent(), dest.file_name()) else {
        return Err(format!("{} is not a folder path", dest.display()));
    };
    std::fs::create_dir_all(parent)
        .map_err(|e| format!("cannot create {}: {e}", parent.display()))?;
    let partial = parent.join(format!(
        ".{}.partial-{}",
        name.to_string_lossy(),
        uuid::Uuid::new_v4()
    ));
    let cloned = run_clone(url, &partial, timeout).and_then(|()| {
        std::fs::rename(&partial, dest)
            .map_err(|e| format!("cannot move the clone to {}: {e}", dest.display()))
    });
    if let Err(message) = cloned {
        let _ = std::fs::remove_dir_all(&partial);
        return Err(message);
    }
    Ok(dest.to_owned())
}

/// Runs `git clone url dest` in its own process group, so on timeout git and the
/// helpers it started can be stopped together.
fn run_clone(url: &str, dest: &Path, timeout: Duration) -> Result<(), String> {
    let mut child = Command::new("git")
        .args(["clone", "--quiet", "--"])
        .arg(url)
        .arg(dest)
        .env("GIT_TERMINAL_PROMPT", "0")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .process_group(0)
        .spawn()
        .map_err(|e| format!("cannot run git: {e}"))?;
    let mut stderr = child.stderr.take().expect("stderr is piped");
    // Read on the side, so a chatty git never blocks on a full pipe.
    let reader = std::thread::spawn(move || {
        let mut text = String::new();
        let _ = stderr.read_to_string(&mut text);
        text
    });
    let deadline = Instant::now() + timeout;
    let status = loop {
        match child.try_wait() {
            Ok(Some(status)) => break status,
            Ok(None) if Instant::now() < deadline => std::thread::sleep(POLL),
            Ok(None) => {
                stop_group(&mut child, &reader);
                let _ = reader.join();
                return Err(format!(
                    "cloning {} took longer than {} seconds",
                    redact_userinfo(url),
                    timeout.as_secs()
                ));
            }
            Err(e) => {
                stop_group(&mut child, &reader);
                let _ = reader.join();
                return Err(format!("cannot wait for git: {e}"));
            }
        }
    };
    let stderr = reader.join().unwrap_or_default();
    if !status.success() {
        let why = stderr
            .lines()
            .map(str::trim)
            .rfind(|line| !line.is_empty())
            .unwrap_or("no message");
        return Err(format!(
            "git clone {} failed ({status}): {why}",
            redact_userinfo(url)
        ));
    }
    Ok(())
}

/// Stops the process group led by `child`: SIGTERM, then SIGKILL after [`GRACE`] or
/// sooner once nothing holds the stderr pipe `reader` reads. `child` is only reaped
/// after the SIGKILL, so its group id cannot have been reused by then.
fn stop_group<T>(child: &mut Child, reader: &JoinHandle<T>) {
    let group = format!("-{}", child.id());
    signal("TERM", &group);
    let deadline = Instant::now() + GRACE;
    while !reader.is_finished() && Instant::now() < deadline {
        std::thread::sleep(POLL);
    }
    signal("KILL", &group);
    let _ = child.wait();
}

/// Sends `signal` to `target` (a pid, or `-pgid` for a group) with `kill(1)`.
fn signal(signal: &str, target: &str) {
    let _ = Command::new("kill")
        .args(["-s", signal, "--", target])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status();
}

/// True when `HEAD` in the repo at `repo` names a commit.
fn has_head(repo: &Path) -> bool {
    Command::new("git")
        .arg("-C")
        .arg(repo)
        .args(["rev-parse", "--verify", "--quiet", "HEAD"])
        .env("GIT_TERMINAL_PROMPT", "0")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .is_ok_and(|status| status.success())
}

/// True when two remote URLs name the same repo: equal once a trailing `/` and `.git`
/// are dropped and, in a `scheme://` URL, the scheme and host are lowercased and any
/// user part is dropped.
pub fn same_origin(a: &str, b: &str) -> bool {
    normalise_url(a) == normalise_url(b)
}

fn normalise_url(url: &str) -> String {
    let url = url.trim().trim_end_matches('/');
    let url = url
        .strip_suffix(".git")
        .unwrap_or(url)
        .trim_end_matches('/');
    let Some((scheme, rest)) = url.split_once("://") else {
        return url.to_owned();
    };
    let (authority, path) = rest.split_at(rest.find('/').unwrap_or(rest.len()));
    let host = authority.rsplit('@').next().unwrap_or(authority);
    format!(
        "{}://{}{path}",
        scheme.to_ascii_lowercase(),
        host.to_ascii_lowercase()
    )
}

/// `remote.origin.url` of the repo at `repo`, if it has one.
pub fn origin_url(repo: &Path) -> Option<String> {
    let out = Command::new("git")
        .arg("-C")
        .arg(repo)
        .args(["config", "--get", "remote.origin.url"])
        .env("GIT_TERMINAL_PROMPT", "0")
        .stdin(Stdio::null())
        .stderr(Stdio::null())
        .output()
        .ok()?;
    let url = String::from_utf8_lossy(&out.stdout).trim().to_owned();
    (out.status.success() && !url.is_empty()).then_some(url)
}

/// The top of the git working tree that holds `path`, with symlinks resolved. The
/// error is a message for the app.
pub fn work_tree_root(path: &Path) -> Result<PathBuf, String> {
    let path = std::fs::canonicalize(path).map_err(|e| format!("{}: {e}", path.display()))?;
    let out = Command::new("git")
        .arg("-C")
        .arg(&path)
        .args(["rev-parse", "--show-toplevel"])
        .env("GIT_TERMINAL_PROMPT", "0")
        .stdin(Stdio::null())
        .stderr(Stdio::null())
        .output()
        .map_err(|e| format!("cannot run git: {e}"))?;
    let top = String::from_utf8_lossy(&out.stdout).trim().to_owned();
    if !out.status.success() || top.is_empty() {
        return Err(format!("{} is not in a git working tree", path.display()));
    }
    let top = std::fs::canonicalize(&top).map_err(|e| format!("{top}: {e}"))?;
    if !scan::is_repo(&top) {
        return Err(format!("{} is not in a git working tree", path.display()));
    }
    Ok(top)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn origins_match_despite_git_suffix_slash_and_host_case() {
        let url = "https://example.com/dev/kelp-cli.git";
        for other in [
            "https://example.com/dev/kelp-cli",
            "https://Example.COM/dev/kelp-cli.git",
            "HTTPS://example.com/dev/kelp-cli/",
            "https://token@example.com/dev/kelp-cli.git",
            " https://example.com/dev/kelp-cli.git\n",
        ] {
            assert!(same_origin(url, other), "{other}");
        }
        for other in [
            "https://example.com/dev/Kelp-cli.git",
            "https://example.com/other/kelp-cli.git",
            "https://example.org/dev/kelp-cli.git",
            "git@example.com:dev/kelp-cli.git",
        ] {
            assert!(!same_origin(url, other), "{other}");
        }
        assert!(same_origin("/srv/git/kelp-cli.git", "/srv/git/kelp-cli"));
    }
}
