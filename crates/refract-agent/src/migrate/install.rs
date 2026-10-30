//! Reinstalling the skipped build folders in the placed copy (spec §3.5), through a
//! login shell so tools from nvm and the like are found. A failed or slow install never
//! fails the migration: the report shows it, with the log.

use std::fs::{self, File, OpenOptions};
use std::io::{Read, Write};
use std::os::unix::process::CommandExt;
use std::path::Path;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use refract_core::migrate::build::{install_plan, InstallStep, RootFiles, SkippedFolder};
use refract_core::migrate::manifest::{Manifest, RelPath, StatChanges, StatEntry};
use refract_core::migrate::report::{InstallChanges, InstallResult, InstallStatus, Problem};

use super::fsx;
use super::tree::Cancel;

/// Longest one install step may run.
pub const INSTALL_TIMEOUT: Duration = Duration::from_secs(15 * 60);

const POLL: Duration = Duration::from_millis(100);

/// The install steps for the copy at `dest`, from its root files.
pub fn plan(dest: &Path, skipped: &[SkippedFolder]) -> Vec<InstallStep> {
    let names: Vec<String> = fs::read_dir(dest)
        .map(|entries| {
            entries
                .flatten()
                .filter_map(|e| e.file_name().into_string().ok())
                .collect()
        })
        .unwrap_or_default();
    let package_json = fs::read_to_string(dest.join("package.json")).ok();
    let yarn_lock = File::open(dest.join("yarn.lock")).ok().map(|f| {
        let mut start = String::new();
        let _ = f.take(4096).read_to_string(&mut start);
        start
    });
    install_plan(
        skipped,
        &RootFiles::new(names, package_json.as_deref(), yarn_lock.as_deref()),
    )
}

/// What the install did to the copy at `dest`, from the stat changes of its tree
/// (skipped folders left out) over the install. A folder whose time alone changed is
/// not a change, and neither is a git index (check 2 compares its entries).
pub fn changes(dest: &Path, stats: &StatChanges) -> InstallChanges {
    let keep = |path: &&RelPath| {
        let parts: Vec<&str> = path.as_str().split('/').collect();
        !(parts.contains(&".git") && parts.last() == Some(&"index"))
    };
    let list = |paths: &[RelPath]| -> Vec<String> {
        paths
            .iter()
            .filter(keep)
            .map(|p| p.as_str().to_owned())
            .collect()
    };
    InstallChanges {
        created: list(&stats.added),
        changed: stats
            .changed
            .iter()
            .filter(keep)
            .filter(|p| !fs::symlink_metadata(dest.join(p.as_str())).is_ok_and(|m| m.is_dir()))
            .map(|p| p.as_str().to_owned())
            .collect(),
        removed: list(&stats.removed),
    }
}

/// Backs up into `backup` the original's version of each path the install changed or
/// removed in the copy (spec §3.5), read from `source`, so that deleting the original
/// loses none of them. A file is copied only while the source still has the size and
/// time `final_stats` recorded, and the backup must hash as `manifest` (the snapshot
/// the copy was checked against) says the source did. Returns the problems, by path:
/// each one means a version would be lost, and fails check 3.
pub fn backup(
    source: &Path,
    changes: &InstallChanges,
    backup: &Path,
    final_stats: &[StatEntry],
    manifest: &Manifest,
) -> Vec<Problem> {
    let mut problems = Vec::new();
    let mut paths: Vec<&String> = changes.changed.iter().chain(&changes.removed).collect();
    paths.sort();
    for path in paths {
        if let Err(message) = backup_one(source, path, backup, final_stats, manifest) {
            problems.push(Problem {
                path: Some(path.clone()),
                message: format!(
                    "The install changed it, and the original's version could not be backed up: {message}"
                ),
            });
        }
    }
    problems
}

fn backup_one(
    source: &Path,
    path: &str,
    backup: &Path,
    final_stats: &[StatEntry],
    manifest: &Manifest,
) -> Result<(), String> {
    let rel = RelPath::new(path.to_owned())?;
    let entry = manifest
        .entries
        .iter()
        .find(|e| e.path == rel)
        .ok_or("the snapshot does not list it")?;
    let stat = final_stats
        .iter()
        .find(|s| s.path == rel)
        .ok_or("the last check did not see it")?;
    let src = source.join(path);
    let dest = backup.join(path);
    let meta = fs::symlink_metadata(&src).map_err(|e| e.to_string())?;
    let size = if meta.is_file() { meta.len() } else { 0 };
    if size != stat.size || fsx::mtime_ns(&meta) != stat.mtime_ns {
        return Err("it changed in the original since the check".into());
    }
    if let Some(parent) = dest.parent() {
        fs::create_dir_all(parent).map_err(|e| e.to_string())?;
    }
    if meta.is_dir() {
        return fs::create_dir_all(&dest).map_err(|e| e.to_string());
    }
    if meta.file_type().is_symlink() {
        let target = fs::read_link(&src).map_err(|e| e.to_string())?;
        if entry.link_target.as_deref() != Some(&*target.to_string_lossy()) {
            return Err("its target differs from the snapshot's".into());
        }
        let _ = fs::remove_file(&dest);
        return std::os::unix::fs::symlink(target, &dest).map_err(|e| e.to_string());
    }
    let bytes = fs::read(&src).map_err(|e| e.to_string())?;
    let want = entry
        .sha256
        .as_deref()
        .ok_or("the snapshot has no hash of it")?;
    if fsx::sha256_bytes(&bytes) != want {
        return Err("it differs from the snapshot".into());
    }
    fs::write(&dest, &bytes).map_err(|e| e.to_string())?;
    if !fsx::sha256_file(&dest).is_ok_and(|h| h == want) {
        return Err("the backup differs from what was written".into());
    }
    let _ = fsx::set_mtime(&dest, fsx::mtime_ns(&meta));
    Ok(())
}

/// Runs `steps` in `dest`, appending their output to `log`. `progress` gets the steps
/// done so far.
pub fn run(
    dest: &Path,
    steps: Vec<InstallStep>,
    log: &Path,
    timeout: Duration,
    cancel: &dyn Cancel,
    progress: &mut dyn FnMut(u64),
) -> Vec<InstallResult> {
    let mut results = Vec::new();
    for (n, step) in steps.into_iter().enumerate() {
        let result = run_step(dest, &step, log, timeout, cancel);
        results.push(InstallResult {
            step,
            status: result.0,
            exit_code: result.1,
            log_path: Some(log.to_string_lossy().into_owned()),
        });
        progress(n as u64 + 1);
    }
    results
}

fn run_step(
    dest: &Path,
    step: &InstallStep,
    log: &Path,
    timeout: Duration,
    cancel: &dyn Cancel,
) -> (InstallStatus, Option<i32>) {
    let mut out = match OpenOptions::new().create(true).append(true).open(log) {
        Ok(out) => out,
        Err(_) => return (InstallStatus::Failed, None),
    };
    let _ = writeln!(out, "$ {}", step.command);
    // `command -v` through the same login shell, so nvm's PATH counts.
    let found = Command::new("bash")
        .args(["-lc", &format!("command -v {}", step.tool)])
        .current_dir(dest)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .is_ok_and(|s| s.success());
    if !found {
        let _ = writeln!(
            out,
            "{} is not installed in WSL: {}",
            step.tool, step.install_hint
        );
        return (InstallStatus::ToolMissing, None);
    }
    let (Ok(stdout), Ok(stderr)) = (out.try_clone(), out.try_clone()) else {
        return (InstallStatus::Failed, None);
    };
    let child = Command::new("bash")
        .args(["-lc", &step.command])
        .current_dir(dest)
        .stdin(Stdio::null())
        .stdout(stdout)
        .stderr(stderr)
        // Its own group, so a timeout stops everything it started.
        .process_group(0)
        .spawn();
    let mut child = match child {
        Ok(child) => child,
        Err(e) => {
            let _ = writeln!(out, "cannot start bash: {e}");
            return (InstallStatus::Failed, None);
        }
    };
    let started = Instant::now();
    loop {
        match child.try_wait() {
            Ok(Some(status)) => {
                let code = status.code();
                let ok = status.success();
                let _ = writeln!(
                    out,
                    "exit {}",
                    code.map_or("signal".into(), |c| c.to_string())
                );
                return (
                    if ok {
                        InstallStatus::Succeeded
                    } else {
                        InstallStatus::Failed
                    },
                    code,
                );
            }
            Ok(None) if started.elapsed() >= timeout || cancel.cancelled() => {
                // SAFETY: kill takes a process group id (negated) and a signal number.
                unsafe {
                    libc::kill(-(child.id() as i32), libc::SIGKILL);
                }
                let _ = child.wait();
                let _ = writeln!(out, "stopped after {} seconds", started.elapsed().as_secs());
                return (InstallStatus::TimedOut, None);
            }
            Ok(None) => std::thread::sleep(POLL),
            Err(_) => return (InstallStatus::Failed, None),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::AtomicBool;

    fn step(tool: &str, command: &str) -> InstallStep {
        InstallStep {
            folder: RelPath::new("node_modules").unwrap(),
            tool: tool.into(),
            command: command.into(),
            install_hint: "get it".into(),
        }
    }

    #[test]
    fn runs_logs_and_times_out() {
        let tmp = tempfile::tempdir().unwrap();
        let log = tmp.path().join("install.log");
        let cancel = AtomicBool::new(false);
        let results = run(
            tmp.path(),
            vec![
                step("sh", "echo installed"),
                step("sh", "exit 3"),
                step("refract-no-such-tool", "refract-no-such-tool install"),
                step("sh", "sleep 5"),
            ],
            &log,
            Duration::from_millis(300),
            &cancel,
            &mut |_| {},
        );
        let statuses: Vec<InstallStatus> = results.iter().map(|r| r.status).collect();
        assert_eq!(
            statuses,
            [
                InstallStatus::Succeeded,
                InstallStatus::Failed,
                InstallStatus::ToolMissing,
                InstallStatus::TimedOut
            ]
        );
        assert_eq!(results[1].exit_code, Some(3));
        let text = fs::read_to_string(&log).unwrap();
        assert!(text.contains("installed"), "{text}");
        assert!(text.contains("get it"), "{text}");
    }
}
