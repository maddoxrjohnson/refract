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
use refract_core::migrate::report::{InstallResult, InstallStatus};

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
    use refract_core::migrate::manifest::RelPath;
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
