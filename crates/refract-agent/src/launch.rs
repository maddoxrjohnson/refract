//! `prepareLaunch`: leaves a request file for the VS Code extension (see
//! `docs/launch.md`).

use std::fs;
use std::io::{self, Write};
use std::os::unix::fs::DirBuilderExt;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

use chrono::{DateTime, SubsecRound, Utc};
use refract_core::launch::{folder_uri, launch_argv, launch_title};
use refract_core::protocol::{
    code, ErrorBody, FocusTarget, LaunchMode, LaunchRequest, LaunchTicket, LAUNCH_REQUEST_VERSION,
};

use crate::procs::{self, LiveProcess};

/// Request, claimed and leftover temporary files older than this are deleted when a
/// new request is written.
pub const MAX_AGE: Duration = Duration::from_secs(60 * 60);

/// `$XDG_STATE_HOME/refract/launch`, defaulting to `~/.local/state/refract/launch`.
/// A relative `XDG_STATE_HOME` is ignored, as the XDG spec says and the extension does.
pub fn launch_dir() -> PathBuf {
    launch_dir_from(
        std::env::var_os("XDG_STATE_HOME").map(PathBuf::from),
        std::env::var_os("HOME").map(PathBuf::from),
    )
}

/// `$XDG_STATE_HOME/refract`, defaulting to `~/.local/state/refract`: where Refract
/// keeps launch requests, hook status files and the settings backup.
pub fn state_dir() -> PathBuf {
    state_dir_from(
        std::env::var_os("XDG_STATE_HOME").map(PathBuf::from),
        std::env::var_os("HOME").map(PathBuf::from),
    )
}

fn launch_dir_from(state_home: Option<PathBuf>, home: Option<PathBuf>) -> PathBuf {
    state_dir_from(state_home, home).join("launch")
}

fn state_dir_from(state_home: Option<PathBuf>, home: Option<PathBuf>) -> PathBuf {
    state_home
        .filter(|v| v.is_absolute())
        .unwrap_or_else(|| home.unwrap_or_else(|| "/".into()).join(".local/state"))
        .join("refract")
}

/// Checks the repo path, writes `<requestId>.json` into `dir` and returns the ticket.
/// `distro` is `WSL_DISTRO_NAME`. `running` is the process the agent last saw running
/// the session a focus names; other modes ignore it.
pub fn prepare(
    path: &str,
    mode: &LaunchMode,
    running: Option<LiveProcess>,
    dir: &Path,
    distro: Option<&str>,
    now: DateTime<Utc>,
) -> Result<LaunchTicket, ErrorBody> {
    prepare_with(path, mode, running, procs_chain, dir, distro, now)
}

/// A running `claude` process and its ancestors, or nothing once it has exited or its
/// pid has been given to a process that started later.
fn procs_chain(process: &LiveProcess) -> Vec<u32> {
    let pid = process.pid;
    if procs::is_claude(pid) && procs::same_process(process.started, procs::process_started(pid)) {
        procs::proc_ancestors(pid)
    } else {
        Vec::new()
    }
}

/// [`prepare`], reading a process's ancestors with `chain`, which is empty when the
/// process no longer runs `claude` or is not the one that was recorded.
fn prepare_with(
    path: &str,
    mode: &LaunchMode,
    running: Option<LiveProcess>,
    chain: impl Fn(&LiveProcess) -> Vec<u32>,
    dir: &Path,
    distro: Option<&str>,
    now: DateTime<Utc>,
) -> Result<LaunchTicket, ErrorBody> {
    let fail = |code, message: String| ErrorBody { code, message };
    let trimmed = path.trim_end_matches('/');
    let folder = if trimmed.is_empty() { "/" } else { trimmed };
    if !folder.starts_with('/') || !Path::new(folder).is_dir() {
        return Err(fail(
            code::REPO_NOT_FOUND,
            format!("no directory at {path}"),
        ));
    }
    let Some(distro) = distro.filter(|d| !d.is_empty()) else {
        return Err(fail(
            code::INTERNAL_ERROR,
            "WSL_DISTRO_NAME is not set, so the folder URI cannot name the distro".into(),
        ));
    };
    let session_id = match mode {
        LaunchMode::New { .. } => uuid::Uuid::new_v4().to_string(),
        LaunchMode::Resume { session_id } | LaunchMode::Focus { session_id } => {
            // Only the hyphenated form, which is the only one 36 characters long.
            if session_id.len() != 36 || uuid::Uuid::try_parse(session_id).is_err() {
                return Err(fail(
                    code::INVALID_PARAMS,
                    format!("sessionId {session_id:?} is not a UUID"),
                ));
            }
            session_id.clone()
        }
    };
    let focus = match mode {
        LaunchMode::Focus { .. } => {
            let gone = || fail(code::NOT_RUNNING, "that agent is no longer running".into());
            let process = running.ok_or_else(gone)?;
            if !process.in_refract_window(folder) {
                return Err(fail(
                    code::NOT_RUNNING,
                    "that agent is not running in a Refract VS Code window on this folder".into(),
                ));
            }
            let pids = chain(&process);
            if pids.is_empty() {
                return Err(gone());
            }
            Some(FocusTarget {
                session_id: session_id.clone(),
                pids,
            })
        }
        _ => None,
    };
    let request_id = uuid::Uuid::new_v4().to_string();
    let request = LaunchRequest {
        version: LAUNCH_REQUEST_VERSION,
        request_id: request_id.clone(),
        folder: folder.to_owned(),
        argv: launch_argv(mode, &session_id),
        title: launch_title(mode).to_owned(),
        created_at: now.trunc_subsecs(0),
        focus,
    };
    write_request(dir, &request).map_err(|e| {
        fail(
            code::INTERNAL_ERROR,
            format!("cannot write launch request in {}: {e}", dir.display()),
        )
    })?;
    Ok(LaunchTicket {
        request_id,
        session_id,
        folder_uri: folder_uri(distro, folder),
    })
}

/// Writes the request through a temporary file in the same directory, so readers never
/// see a partial file, after deleting old requests. A new launch directory is private
/// to the user (mode 0700), since requests hold prompts.
fn write_request(dir: &Path, request: &LaunchRequest) -> io::Result<()> {
    if let Some(parent) = dir.parent() {
        fs::create_dir_all(parent)?;
    }
    match fs::DirBuilder::new().mode(0o700).create(dir) {
        Err(e) if e.kind() != io::ErrorKind::AlreadyExists => return Err(e),
        _ => {}
    }
    prune(dir, SystemTime::now());
    let tmp = dir.join(format!(".{}.tmp", request.request_id));
    let mut file = fs::File::create(&tmp)?;
    let written = serde_json::to_writer_pretty(&mut file, request)
        .map_err(io::Error::from)
        .and_then(|()| file.write_all(b"\n"))
        .and_then(|()| file.sync_all());
    if let Err(e) = written {
        let _ = fs::remove_file(&tmp);
        return Err(e);
    }
    fs::rename(&tmp, dir.join(format!("{}.json", request.request_id))).inspect_err(|_| {
        let _ = fs::remove_file(&tmp);
    })
}

/// Deletes `.json`, `.claimed` and leftover `.<id>.tmp` files last modified more than
/// [`MAX_AGE`] before `now`. Failures are ignored; another window may be claiming the
/// same file.
fn prune(dir: &Path, now: SystemTime) {
    let Ok(entries) = fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        let hidden = entry.file_name().to_string_lossy().starts_with('.');
        let ours = match path.extension().and_then(|e| e.to_str()) {
            Some("json" | "claimed") => true,
            Some("tmp") => hidden,
            _ => false,
        };
        let old = entry
            .metadata()
            .and_then(|m| m.modified())
            .is_ok_and(|t| now.duration_since(t).is_ok_and(|age| age > MAX_AGE));
        if ours && old {
            let _ = fs::remove_file(&path);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn now() -> DateTime<Utc> {
        "2026-10-29T17:54:44.5Z".parse().unwrap()
    }

    #[test]
    fn writes_the_request_file() {
        let tmp = tempfile::tempdir().unwrap();
        let repo = tmp.path().join("tidepool api");
        fs::create_dir(&repo).unwrap();
        let dir = tmp.path().join("state/refract/launch");
        let path = format!("{}/", repo.to_str().unwrap());
        let mode = LaunchMode::New {
            prompt: Some("Add tide tables".into()),
        };
        let ticket = prepare(&path, &mode, None, &dir, Some("Ubuntu"), now()).unwrap();
        assert_ne!(ticket.request_id, ticket.session_id);
        assert_eq!(
            ticket.folder_uri,
            format!(
                "vscode-remote://wsl+Ubuntu{}",
                repo.to_str().unwrap().replace(' ', "%20")
            )
        );
        let names: Vec<_> = fs::read_dir(&dir)
            .unwrap()
            .map(|e| e.unwrap().file_name())
            .collect();
        assert_eq!(names, [format!("{}.json", ticket.request_id).as_str()]);
        let text = fs::read_to_string(dir.join(&names[0])).unwrap();
        let request: LaunchRequest = serde_json::from_str(&text).unwrap();
        assert_eq!(
            request,
            LaunchRequest {
                version: 1,
                request_id: ticket.request_id.clone(),
                folder: repo.to_str().unwrap().to_owned(),
                argv: vec![
                    "claude".into(),
                    "--session-id".into(),
                    ticket.session_id.clone(),
                    "--".into(),
                    "Add tide tables".into()
                ],
                title: "New agent".into(),
                created_at: "2026-10-29T17:54:44Z".parse().unwrap(),
                focus: None,
            }
        );
        assert!(text.contains(r#""createdAt": "2026-10-29T17:54:44Z""#));
    }

    #[test]
    fn rejects_missing_dirs_and_missing_distro() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().join("launch");
        let mode = LaunchMode::New { prompt: None };
        let missing = tmp.path().join("nope");
        let err = prepare(
            missing.to_str().unwrap(),
            &mode,
            None,
            &dir,
            Some("Ubuntu"),
            now(),
        );
        assert_eq!(err.unwrap_err().code, code::REPO_NOT_FOUND);
        let file = tmp.path().join("file");
        fs::write(&file, "").unwrap();
        let err = prepare(
            file.to_str().unwrap(),
            &mode,
            None,
            &dir,
            Some("Ubuntu"),
            now(),
        );
        assert_eq!(err.unwrap_err().code, code::REPO_NOT_FOUND);
        let err = prepare("relative", &mode, None, &dir, Some("Ubuntu"), now());
        assert_eq!(err.unwrap_err().code, code::REPO_NOT_FOUND);
        let err = prepare(tmp.path().to_str().unwrap(), &mode, None, &dir, None, now());
        assert_eq!(err.unwrap_err().code, code::INTERNAL_ERROR);
        assert!(!dir.exists());
    }

    #[test]
    fn prunes_old_requests_only() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path();
        let now = SystemTime::now();
        let old = now - MAX_AGE - Duration::from_secs(1);
        for name in [
            "old.json",
            "old.claimed",
            "old.txt",
            ".old.tmp",
            "old.tmp",
            "new.json",
            "new.claimed",
            ".new.tmp",
        ] {
            let file = fs::File::create(dir.join(name)).unwrap();
            if name.contains("old") {
                file.set_modified(old).unwrap();
            }
        }
        prune(dir, now);
        let mut names: Vec<_> = fs::read_dir(dir)
            .unwrap()
            .map(|e| e.unwrap().file_name().into_string().unwrap())
            .collect();
        names.sort();
        assert_eq!(
            names,
            [".new.tmp", "new.claimed", "new.json", "old.tmp", "old.txt"]
        );
    }

    #[test]
    fn launch_dir_needs_an_absolute_state_home() {
        let home = Some(PathBuf::from("/home/dev"));
        assert_eq!(
            launch_dir_from(Some("/state".into()), home.clone()),
            Path::new("/state/refract/launch")
        );
        for state_home in [None, Some("".into()), Some("relative/state".into())] {
            assert_eq!(
                launch_dir_from(state_home, home.clone()),
                Path::new("/home/dev/.local/state/refract/launch")
            );
        }
    }

    #[test]
    fn launch_dir_is_private() {
        use std::os::unix::fs::PermissionsExt;
        let tmp = tempfile::tempdir().unwrap();
        let repo = tmp.path().join("repo");
        fs::create_dir(&repo).unwrap();
        let dir = tmp.path().join("state/refract/launch");
        let mode = LaunchMode::New { prompt: None };
        prepare(
            repo.to_str().unwrap(),
            &mode,
            None,
            &dir,
            Some("Ubuntu"),
            now(),
        )
        .unwrap();
        let mode = fs::metadata(&dir).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o700);
    }

    #[test]
    fn resume_needs_a_uuid() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().join("launch");
        let repo = tmp.path().to_str().unwrap();
        for bad in [
            "",
            "abc",
            "--help",
            "0b7e4d2c-5a6f-4e1b-8c9d",
            "0b7e4d2c5a6f4e1b8c9d3f2a1b0c9d8e",
            "{0b7e4d2c-5a6f-4e1b-8c9d-3f2a1b0c9d8e}",
        ] {
            let mode = LaunchMode::Resume {
                session_id: bad.into(),
            };
            let err = prepare(repo, &mode, None, &dir, Some("Ubuntu"), now()).unwrap_err();
            assert_eq!(err.code, code::INVALID_PARAMS, "{bad:?}");
        }
        assert!(!dir.exists());
        let id = "0b7e4d2c-5a6f-4e1b-8c9d-3f2a1b0c9d8e";
        let mode = LaunchMode::Resume {
            session_id: id.into(),
        };
        let ticket = prepare(repo, &mode, None, &dir, Some("Ubuntu"), now()).unwrap();
        assert_eq!(ticket.session_id, id);
    }

    const RUNNING_ID: &str = "0b7e4d2c-5a6f-4e1b-8c9d-3f2a1b0c9d8e";

    fn focus() -> LaunchMode {
        LaunchMode::Focus {
            session_id: RUNNING_ID.into(),
        }
    }

    fn started() -> Option<DateTime<Utc>> {
        Some("2026-10-29T17:00:00Z".parse().unwrap())
    }

    fn running(folder: &str) -> Option<LiveProcess> {
        Some(LiveProcess {
            pid: 812,
            started: started(),
            refract_folder: Some(folder.into()),
        })
    }

    /// A chain read from a process table where pid 812 started at [`started`].
    fn chain(process: &LiveProcess) -> Vec<u32> {
        if procs::same_process(process.started, started()) {
            vec![process.pid, 640, 600]
        } else {
            Vec::new()
        }
    }

    #[test]
    fn focus_writes_a_request_naming_the_process_chain() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().join("launch");
        let repo = tmp.path().to_str().unwrap();
        let ticket = prepare_with(
            &format!("{repo}/"),
            &focus(),
            running(repo),
            chain,
            &dir,
            Some("Ubuntu"),
            now(),
        )
        .unwrap();
        assert_eq!(ticket.session_id, RUNNING_ID);
        let text = fs::read_to_string(dir.join(format!("{}.json", ticket.request_id))).unwrap();
        let request: LaunchRequest = serde_json::from_str(&text).unwrap();
        assert!(request.argv.is_empty());
        assert_eq!(request.title, "Running agent");
        assert_eq!(
            request.focus,
            Some(FocusTarget {
                session_id: RUNNING_ID.into(),
                pids: vec![812, 640, 600],
            })
        );
    }

    #[test]
    fn focus_needs_the_same_agent_in_a_refract_window_on_this_folder() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().join("launch");
        let repo = tmp.path().to_str().unwrap();
        let parent = tmp.path().parent().unwrap().to_str().unwrap();
        let reused = running(repo).map(|p| LiveProcess {
            started: Some("2026-10-29T17:30:00Z".parse().unwrap()),
            ..p
        });
        let no_folder = running(repo).map(|p| LiveProcess {
            refract_folder: None,
            ..p
        });
        let exited = |_: &LiveProcess| Vec::new();
        for (running, chain) in [
            (None, &chain as &dyn Fn(&LiveProcess) -> Vec<u32>),
            (no_folder, &chain),
            (running(parent), &chain),
            (running(&format!("{repo}/sub")), &chain),
            (reused, &chain),
            (running(repo), &exited),
        ] {
            let err = prepare_with(
                repo,
                &focus(),
                running.clone(),
                chain,
                &dir,
                Some("Ubuntu"),
                now(),
            )
            .unwrap_err();
            assert_eq!(err.code, code::NOT_RUNNING, "{running:?}");
        }
        let bad = LaunchMode::Focus {
            session_id: "--help".into(),
        };
        let err = prepare_with(
            repo,
            &bad,
            running(repo),
            chain,
            &dir,
            Some("Ubuntu"),
            now(),
        )
        .unwrap_err();
        assert_eq!(err.code, code::INVALID_PARAMS);
        assert!(!dir.exists());
    }
}
