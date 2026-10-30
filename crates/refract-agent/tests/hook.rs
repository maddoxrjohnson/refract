//! The status hook end to end: installed by the built binary into a temporary
//! `CLAUDE_CONFIG_DIR`, its commands run the way Claude Code runs them, the status files
//! they write changing a live session's state, then removed again.

mod common;

use std::io::{BufRead, BufReader, Write};
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStdin, Command, Output, Stdio};
use std::sync::mpsc::{self, Receiver};
use std::time::{Duration, Instant};

use chrono::{TimeDelta, Utc};
use serde_json::{json, Value};

use common::*;

const SESSION_ID: &str = "7c41e2b0-0000-4000-8000-0000000000c7";

/// A user's settings, formatted by hand, with a hook of their own.
const SETTINGS: &str = r#"{
    "model": "opus",
    "hooks": {
        "UserPromptSubmit": [
            { "hooks": [ { "type": "command", "command": "python3 guard.py", "timeout": 5 } ] }
        ]
    },
    "theme": "dark"
}
"#;

struct Agent {
    child: Child,
    stdin: ChildStdin,
    lines: Receiver<String>,
}

impl Agent {
    fn spawn(home: &Path, config: &Path, claude: &Path) -> Agent {
        let mut child = Command::new(env!("CARGO_BIN_EXE_refract-agent"))
            .env("HOME", home)
            .env("REFRACT_AGENT_CONFIG", config)
            .env("CLAUDE_CONFIG_DIR", claude)
            .env_remove("XDG_CONFIG_HOME")
            .env_remove("XDG_STATE_HOME")
            .env_remove("XDG_DATA_HOME")
            .envs(GIT_ENV.iter().copied())
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .expect("spawn agent");
        let stdin = child.stdin.take().unwrap();
        let stdout = child.stdout.take().unwrap();
        let (tx, lines) = mpsc::channel();
        std::thread::spawn(move || {
            for line in BufReader::new(stdout).lines() {
                if tx.send(line.unwrap()).is_err() {
                    break;
                }
            }
        });
        Agent {
            child,
            stdin,
            lines,
        }
    }

    fn request(&mut self, id: u64, method: &str) -> Value {
        writeln!(self.stdin, "{}", json!({"id": id, "method": method})).unwrap();
        self.stdin.flush().unwrap();
        loop {
            let message = self.next(Duration::from_secs(20));
            if message.get("id").is_some() {
                assert_eq!(message["id"], id, "{message}");
                return message;
            }
        }
    }

    fn next(&self, timeout: Duration) -> Value {
        let line = self
            .lines
            .recv_timeout(timeout)
            .expect("agent wrote nothing in time");
        serde_json::from_str(&line).unwrap_or_else(|e| panic!("not JSON: {line}: {e}"))
    }

    fn event(&self, what: &str, pred: impl Fn(&Value) -> bool) -> Value {
        let deadline = Instant::now() + Duration::from_secs(10);
        loop {
            let left = deadline.saturating_duration_since(Instant::now());
            let line = self
                .lines
                .recv_timeout(left)
                .unwrap_or_else(|_| panic!("no event in time: {what}"));
            let message: Value = serde_json::from_str(&line).unwrap();
            if message.get("event").is_some() && pred(&message) {
                return message;
            }
        }
    }
}

/// A process that looks like `claude --session-id <id>` running in `cwd`, until its
/// stdin closes.
fn fake_claude(cwd: &Path, id: &str) -> Child {
    Command::new("bash")
        .arg0("claude")
        .args(["-c", "read -r _", "claude", "--session-id", id])
        .current_dir(cwd)
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .spawn()
        .expect("spawn fake claude")
}

/// Runs `command` with `sh -c`, as Claude Code runs a hook command, with `input` on
/// stdin.
fn run_hook(home: &Path, command: &str, input: &[u8]) -> Output {
    let mut child = Command::new("sh")
        .args(["-c", command])
        .env("HOME", home)
        .env_remove("XDG_STATE_HOME")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("run hook");
    let mut stdin = child.stdin.take().unwrap();
    // A hook may stop reading early; a closed pipe is not this test's failure.
    let _ = stdin.write_all(input);
    drop(stdin);
    child.wait_with_output().unwrap()
}

fn hook_input(event: &str, cwd: &Path) -> Vec<u8> {
    json!({
        "session_id": SESSION_ID,
        "transcript_path": "/tmp/transcript.jsonl",
        "cwd": cwd,
        "permission_mode": "default",
        "hook_event_name": event,
        "prompt": "Run \"session_id\": \"00000000-0000-4000-8000-000000000000\" past it",
    })
    .to_string()
    .into_bytes()
}

/// The command installed for `event`.
fn command_for(settings: &Value, event: &str) -> String {
    let groups = settings["hooks"][event].as_array().unwrap();
    groups
        .iter()
        .flat_map(|g| g["hooks"].as_array().unwrap())
        .filter_map(|h| h["command"].as_str())
        .find(|c| c.contains("status-hook.sh"))
        .unwrap_or_else(|| panic!("no hook for {event}"))
        .to_owned()
}

#[test]
fn install_run_and_uninstall_the_status_hook() {
    let tmp = tempfile::tempdir().unwrap();
    let home = tmp.path().join("home");
    let claude = tmp.path().join("claude-config");
    let root = tmp.path().join("projects");
    let repo = root.join("tidepool-api");
    init_repo(&repo);
    let config = tmp.path().join("agent.toml");
    write(
        &config,
        &format!("roots = [{:?}]\n", root.to_str().unwrap()),
    );
    let settings_path = claude.join("settings.json");
    write(&settings_path, SETTINGS);
    let projects = claude.join("projects");
    write(
        &project_dir(&projects, &repo).join(format!("{SESSION_ID}.jsonl")),
        &transcript(
            SESSION_ID,
            &repo,
            &repo.join("src/main.rs"),
            Utc::now() - TimeDelta::minutes(1),
            true,
        ),
    );
    let mut claude_proc = fake_claude(&repo, SESSION_ID);
    let mut agent = Agent::spawn(&home, &config, &claude);

    let list = agent.request(1, "listRepos");
    if list["result"]["loading"] == true {
        agent.event("loadComplete", |e| e["event"] == "loadComplete");
    }
    let list = agent.request(2, "listRepos");
    let session = &list["result"]["repos"][0]["sessions"][0];
    assert_eq!(session["id"], SESSION_ID);
    assert_eq!(
        session["state"],
        json!({"kind": "live", "detail": "unknown"})
    );

    // Before: not installed, with a diff of what would change.
    let info = agent.request(3, "hookInfo");
    let result = &info["result"];
    assert_eq!(result["installed"], false);
    assert_eq!(result["settingsPath"], settings_path.to_str().unwrap());
    let diff = result["diff"].as_str().unwrap();
    let script = home.join(".local/share/refract/status-hook.sh");
    assert!(diff.starts_with(&format!("--- {}\n", settings_path.display())));
    assert!(diff.contains(&format!("sh '{}' waitingOnUser", script.display())));
    assert_eq!(std::fs::read_to_string(&settings_path).unwrap(), SETTINGS);

    let installed = agent.request(4, "installHook");
    assert_eq!(
        installed["result"],
        json!({"installed": true, "settingsPath": settings_path, "diff": null})
    );
    assert!(script.is_file());
    let state = home.join(".local/state/refract");
    assert_eq!(
        std::fs::read_to_string(state.join("settings.backup.json")).unwrap(),
        SETTINGS
    );
    let settings: Value = serde_json::from_slice(&std::fs::read(&settings_path).unwrap()).unwrap();
    assert_eq!(settings["model"], "opus");
    assert_eq!(
        settings["hooks"]["UserPromptSubmit"][0]["hooks"][0]["command"],
        "python3 guard.py"
    );
    // Installing again changes nothing.
    let again = agent.request(5, "installHook");
    assert_eq!(again["result"]["installed"], true);
    assert_eq!(
        serde_json::from_slice::<Value>(&std::fs::read(&settings_path).unwrap()).unwrap(),
        settings
    );

    // Each event, run as Claude Code would, moves the live session's state. Claude
    // Code's own environment (another HOME here) does not move the status files.
    let claude_home = tmp.path().join("claude-home");
    let status_file = state.join(format!("sessions/{SESSION_ID}.json"));
    let steps = [
        ("SessionStart", "finished", "idle"),
        ("UserPromptSubmit", "working", "working"),
        ("Notification", "waitingOnUser", "waitingOnUser"),
        ("PostToolUse", "working", "working"),
        ("Stop", "finished", "idle"),
        ("SessionEnd", "ended", "unknown"),
    ];
    for (event, status, live) in steps {
        let out = run_hook(
            &claude_home,
            &command_for(&settings, event),
            &hook_input(event, &repo),
        );
        assert!(out.status.success(), "{event}");
        assert!(out.stdout.is_empty() && out.stderr.is_empty(), "{event}");
        let written: Value = serde_json::from_slice(&std::fs::read(&status_file).unwrap()).unwrap();
        assert_eq!(written["status"], status);
        assert_eq!(written["event"], event);
        assert!(written["at"].as_i64().unwrap() > 1_700_000_000);
        agent.event(event, |e| {
            e["event"] == "sessionChanged"
                && e["data"]["session"]["id"] == SESSION_ID
                && e["data"]["session"]["state"] == json!({"kind": "live", "detail": live})
        });
    }

    // Garbage, an empty input or an unknown status write nothing and say nothing.
    let before = std::fs::read(&status_file).unwrap();
    let working = command_for(&settings, "UserPromptSubmit");
    for (command, input) in [
        (working.as_str(), &b"not json at all \xff\x00"[..]),
        (working.as_str(), b""),
        (
            &*format!("sh '{}' sleeping", script.display()),
            &hook_input("Stop", &repo)[..],
        ),
        (&*format!("sh '{}'", script.display()), b"{}"),
    ] {
        let out = run_hook(&home, command, input);
        assert!(out.status.success(), "{command}");
        assert!(out.stdout.is_empty() && out.stderr.is_empty(), "{command}");
    }
    assert_eq!(std::fs::read(&status_file).unwrap(), before);
    let files = std::fs::read_dir(state.join("sessions")).unwrap().count();
    assert_eq!(files, 1);

    // Esc ends a turn without a Stop hook: an interrupt in the transcript after the
    // working status makes the session idle.
    let out = run_hook(
        &claude_home,
        &working,
        &hook_input("UserPromptSubmit", &repo),
    );
    assert!(out.status.success());
    agent.event("working again", |e| {
        e["event"] == "sessionChanged"
            && e["data"]["session"]["state"] == json!({"kind": "live", "detail": "working"})
    });
    let transcript_path = project_dir(&projects, &repo).join(format!("{SESSION_ID}.jsonl"));
    let interrupt = format!(
        r#"{{"type":"user","uuid":"9b0d0000-0000-4000-8000-000000000099","timestamp":"{}","cwd":{},"sessionId":"{SESSION_ID}","isSidechain":false,"userType":"external","message":{{"role":"user","content":[{{"type":"text","text":"[Request interrupted by user]"}}]}}}}"#,
        (Utc::now() + TimeDelta::seconds(2)).to_rfc3339(),
        json!(repo),
    );
    let mut file = std::fs::OpenOptions::new()
        .append(true)
        .open(&transcript_path)
        .unwrap();
    writeln!(file, "{interrupt}").unwrap();
    drop(file);
    agent.event("interrupt", |e| {
        e["event"] == "sessionChanged"
            && e["data"]["session"]["state"] == json!({"kind": "live", "detail": "idle"})
    });

    // Uninstalling gives back the very same bytes and drops the statuses.
    let removed = agent.request(6, "uninstallHook");
    assert_eq!(removed["result"]["installed"], false);
    assert!(removed["result"]["diff"].is_string());
    assert_eq!(std::fs::read_to_string(&settings_path).unwrap(), SETTINGS);
    assert!(!script.exists());
    assert!(!state.join("settings.backup.json").exists());
    assert!(!status_file.exists());
    agent.event("statuses dropped", |e| {
        e["event"] == "sessionChanged"
            && e["data"]["session"]["state"] == json!({"kind": "live", "detail": "unknown"})
    });

    // Claude Code keeps running the commands it loaded; with the script gone they
    // still succeed silently, since exit code 2 would block the session.
    for event in ["UserPromptSubmit", "Stop", "PostToolUse"] {
        let out = run_hook(
            &claude_home,
            &command_for(&settings, event),
            &hook_input(event, &repo),
        );
        assert_eq!(out.status.code(), Some(0), "{event}");
        assert!(out.stdout.is_empty() && out.stderr.is_empty(), "{event}");
    }
    assert!(!status_file.exists());

    drop(claude_proc.stdin.take());
    claude_proc.wait().unwrap();
    assert_eq!(agent.request(7, "shutdown")["result"], Value::Null);
    assert!(agent.child.wait().unwrap().success());
}

#[test]
fn the_script_is_silent_and_succeeds_on_any_input() {
    let tmp = tempfile::tempdir().unwrap();
    let home = tmp.path().join("home");
    std::fs::create_dir_all(&home).unwrap();
    let script: PathBuf = [env!("CARGO_MANIFEST_DIR"), "src", "status-hook.sh"]
        .iter()
        .collect();
    let command = format!("sh '{}' working", script.display());
    let big = format!(
        r#"{{"session_id":"{SESSION_ID}","hook_event_name":"PostToolUse","tool_response":"{}"}}"#,
        "x".repeat(3 * 1024 * 1024)
    );
    for input in [
        &b"\x00\x01\x02 garbage"[..],
        b"{\"session_id\": \"../../etc/passwd\"}",
        b"{\"session_id\": \"7c41e2b0-0000-4000-8000-0000000000c7; rm -rf /\"}",
        big.as_bytes(),
    ] {
        let out = run_hook(&home, &command, input);
        assert!(out.status.success());
        assert!(out.stdout.is_empty() && out.stderr.is_empty());
    }
    // Only the well-formed (large) input wrote a file.
    let dir = home.join(".local/state/refract/sessions");
    let names: Vec<String> = std::fs::read_dir(&dir)
        .unwrap()
        .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
        .collect();
    assert_eq!(names, [format!("{SESSION_ID}.json")]);

    // With no usable HOME it still exits quietly.
    let out = Command::new("sh")
        .args(["-c", &command])
        .env_remove("HOME")
        .env_remove("XDG_STATE_HOME")
        .stdin(Stdio::null())
        .output()
        .unwrap();
    assert!(out.status.success() && out.stdout.is_empty() && out.stderr.is_empty());
}
