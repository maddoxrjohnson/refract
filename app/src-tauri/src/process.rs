//! Running a child process to completion with a deadline.

use std::io::{self, Read};
use std::process::{Child, Command, ExitStatus, Stdio};
use std::sync::mpsc;
use std::thread;
use std::time::{Duration, Instant};

/// How often a running child is checked.
const POLL: Duration = Duration::from_millis(50);
/// How long the output pipes get to close once the child has exited. A grandchild
/// that inherited them (`code.cmd` starting VS Code) can keep them open.
const DRAIN_GRACE: Duration = Duration::from_secs(2);

/// What a child that finished in time printed, and how it exited.
#[derive(Debug)]
pub struct Finished {
    pub status: ExitStatus,
    pub stdout: Vec<u8>,
    pub stderr: Vec<u8>,
}

/// Runs `command` with stdout and stderr piped, waiting up to `timeout` for it to
/// exit. On expiry the child is killed and the error is [`io::ErrorKind::TimedOut`].
/// Both pipes are read on their own threads, so a child that fills one cannot block.
pub fn output_with_deadline(command: &mut Command, timeout: Duration) -> io::Result<Finished> {
    let mut child = command
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()?;
    let stdout = read_on_thread(child.stdout.take());
    let stderr = read_on_thread(child.stderr.take());
    let status = match wait_with_deadline(|| child.try_wait(), timeout, POLL, thread::sleep) {
        Ok(Some(status)) => status,
        Ok(None) => {
            kill(&mut child);
            return Err(io::Error::new(
                io::ErrorKind::TimedOut,
                format!("timed out after {timeout:?}"),
            ));
        }
        Err(err) => {
            kill(&mut child);
            return Err(err);
        }
    };
    let drain_until = Instant::now() + DRAIN_GRACE;
    let collect = |rx: mpsc::Receiver<Vec<u8>>| {
        rx.recv_timeout(drain_until.saturating_duration_since(Instant::now()))
            .unwrap_or_default()
    };
    Ok(Finished {
        status,
        stdout: collect(stdout),
        stderr: collect(stderr),
    })
}

fn kill(child: &mut Child) {
    // Errors mean it already exited.
    let _ = child.kill();
    let _ = child.wait();
}

/// Reads `pipe` to the end on a new thread and sends what it read.
fn read_on_thread(pipe: Option<impl Read + Send + 'static>) -> mpsc::Receiver<Vec<u8>> {
    let (tx, rx) = mpsc::channel();
    match pipe {
        Some(mut pipe) => {
            thread::spawn(move || {
                let mut bytes = Vec::new();
                // A read error keeps what was read before it.
                let _ = pipe.read_to_end(&mut bytes);
                let _ = tx.send(bytes);
            });
        }
        None => {
            let _ = tx.send(Vec::new());
        }
    }
    rx
}

/// Calls `try_wait` every `poll` until it returns a value, for up to `timeout`.
/// `None` means the time ran out.
pub fn wait_with_deadline<T>(
    mut try_wait: impl FnMut() -> io::Result<Option<T>>,
    timeout: Duration,
    poll: Duration,
    mut sleep: impl FnMut(Duration),
) -> io::Result<Option<T>> {
    let mut waited = Duration::ZERO;
    loop {
        if let Some(done) = try_wait()? {
            return Ok(Some(done));
        }
        if waited >= timeout {
            return Ok(None);
        }
        let step = poll.min(timeout - waited);
        sleep(step);
        waited += step;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agent::{neutral_dir, no_window};

    #[test]
    fn waits_until_done_or_out_of_time() {
        let mut calls = 0;
        let mut slept = Duration::ZERO;
        let done = wait_with_deadline(
            || {
                calls += 1;
                Ok((calls == 3).then_some("exited"))
            },
            Duration::from_secs(1),
            Duration::from_millis(100),
            |d| slept += d,
        );
        assert_eq!(done.unwrap(), Some("exited"));
        assert_eq!(slept, Duration::from_millis(200));

        let mut slept = Duration::ZERO;
        let never = wait_with_deadline(
            || Ok(None::<()>),
            Duration::from_millis(250),
            Duration::from_millis(100),
            |d| slept += d,
        );
        assert_eq!(never.unwrap(), None);
        assert_eq!(slept, Duration::from_millis(250));

        let failed = wait_with_deadline(
            || Err::<Option<()>, _>(io::Error::other("gone")),
            Duration::from_secs(1),
            Duration::from_millis(100),
            |_| panic!("should not sleep"),
        );
        assert!(failed.is_err());
    }

    fn cmd(args: &[&str]) -> Command {
        let mut command = Command::new("cmd.exe");
        command
            .args(args)
            .current_dir(neutral_dir())
            .stdin(Stdio::null());
        no_window(&mut command);
        command
    }

    #[test]
    fn collects_the_output_of_a_quick_command() {
        let done = output_with_deadline(
            &mut cmd(&["/c", "echo out& echo err 1>&2"]),
            Duration::from_secs(30),
        )
        .unwrap();
        assert!(done.status.success());
        assert_eq!(String::from_utf8_lossy(&done.stdout).trim(), "out");
        assert_eq!(String::from_utf8_lossy(&done.stderr).trim(), "err");
    }

    #[test]
    fn kills_a_command_that_runs_too_long() {
        let started = Instant::now();
        let err = output_with_deadline(
            &mut cmd(&["/c", "ping -n 30 127.0.0.1 >nul"]),
            Duration::from_millis(500),
        )
        .unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::TimedOut);
        assert!(started.elapsed() < Duration::from_secs(10));
    }
}
