//! `usage` through the built binary, against a temporary Claude Code `projects`
//! directory. All names are invented.

mod common;

use std::fs;
use std::io::{BufRead, BufReader, Write};
use std::path::Path;
use std::process::{Child, ChildStdin, Command, Stdio};
use std::sync::mpsc::{self, Receiver};
use std::time::{Duration, Instant};

use chrono::{DateTime, TimeDelta, Utc};
use serde_json::{json, Value};

use common::*;

struct Agent {
    child: Child,
    stdin: ChildStdin,
    lines: Receiver<String>,
    next_id: u64,
}

impl Agent {
    fn spawn(home: &Path, config: &Path) -> Agent {
        let mut child = Command::new(env!("CARGO_BIN_EXE_refract-agent"))
            .env("HOME", home)
            .env("REFRACT_AGENT_CONFIG", config)
            .env_remove("XDG_CONFIG_HOME")
            .env_remove("XDG_STATE_HOME")
            .env_remove("CLAUDE_CONFIG_DIR")
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
            next_id: 1,
        }
    }

    /// Sends a request and returns its response, skipping events.
    fn call(&mut self, method: &str) -> Value {
        let id = self.next_id;
        self.next_id += 1;
        writeln!(self.stdin, "{}", json!({"id": id, "method": method})).unwrap();
        self.stdin.flush().unwrap();
        loop {
            let line = self
                .lines
                .recv_timeout(Duration::from_secs(20))
                .expect("agent wrote nothing in time");
            let message: Value = serde_json::from_str(&line).unwrap();
            if message["id"] == id {
                return message;
            }
        }
    }

    /// Asks for usage until `done` holds for the result, for at most 10 s.
    fn usage_until(&mut self, done: impl Fn(&Value) -> bool) -> Value {
        let deadline = Instant::now() + Duration::from_secs(10);
        loop {
            let usage = self.call("usage")["result"].clone();
            if done(&usage) {
                return usage;
            }
            assert!(Instant::now() < deadline, "usage never matched: {usage}");
            std::thread::sleep(Duration::from_millis(50));
        }
    }
}

/// One assistant response with input `input`, output 2·input, cache read 3·input and
/// cache write 4·input, written as `copies` lines the way Claude Code splits a
/// response into content blocks.
fn response(cwd: &Path, id: &str, at: DateTime<Utc>, input: u64, copies: usize) -> String {
    let line = format!(
        r#"{{"type":"assistant","isSidechain":false,"timestamp":"{}","cwd":"{}","sessionId":"6d4e0000-0000-4000-8000-000000000001","requestId":"req_{id}","message":{{"id":"{id}","role":"assistant","content":[{{"type":"text","text":"Done."}}],"usage":{{"input_tokens":{input},"output_tokens":{},"cache_read_input_tokens":{},"cache_creation_input_tokens":{}}}}}}}"#,
        at.to_rfc3339(),
        cwd.display(),
        2 * input,
        3 * input,
        4 * input
    );
    (line + "\n").repeat(copies)
}

fn append(path: &Path, text: &str) {
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    let mut f = fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)
        .unwrap();
    f.write_all(text.as_bytes()).unwrap();
}

fn input(usage: &Value, window: &str) -> u64 {
    usage[window]["input"].as_u64().unwrap()
}

#[test]
fn usage_is_summed_and_follows_writes() {
    let tmp = tempfile::tempdir().unwrap();
    let home = tmp.path().join("home");
    let root = tmp.path().join("projects");
    let repo = root.join("tidepool-api");
    init_repo(&repo);
    let config = tmp.path().join("agent.toml");
    write(
        &config,
        &format!("roots = [{:?}]\n", root.to_str().unwrap()),
    );
    let projects = home.join(".claude/projects");
    let now = Utc::now();
    let today_start = refract_agent::usage::local_midnight(now);

    // In the repo, from a subdirectory: one response today (three lines), one 3 days
    // ago and one 8 days ago, which is too old to count.
    let session = project_dir(&projects, &repo.join("src"))
        .join("6d4e0000-0000-4000-8000-000000000001.jsonl");
    let recent = now - TimeDelta::seconds(5);
    append(
        &session,
        &response(&repo.join("src"), "msg_1", recent, 100, 3),
    );
    append(
        &session,
        &response(&repo, "msg_2", now - TimeDelta::days(3), 20, 1),
    );
    append(
        &session,
        &response(&repo, "msg_3", now - TimeDelta::days(8), 9_000, 1),
    );
    // Outside any repo: a subagent transcript from yesterday.
    let scratch = tmp.path().join("scratch");
    let subagent = project_dir(&projects, &scratch)
        .join("6d4e0000-0000-4000-8000-000000000002/subagents/agent-a1.jsonl");
    append(
        &subagent,
        &response(&scratch, "msg_4", now - TimeDelta::days(1), 1_000, 2),
    );

    let mut agent = Agent::spawn(&home, &config);
    // Let the repo list load so usage can be split by repo.
    let deadline = Instant::now() + Duration::from_secs(10);
    while agent.call("listRepos")["result"]["loading"] != false {
        assert!(Instant::now() < deadline, "repos never loaded");
        std::thread::sleep(Duration::from_millis(50));
    }
    let usage = agent.usage_until(|u| {
        input(u, "last7Days") == 1_120 && u["byRepo"].as_array().unwrap().len() == 2
    });
    assert_eq!(
        usage["last7Days"],
        json!({"input": 1120, "output": 2240, "cacheRead": 3360, "cacheWrite": 4480})
    );
    assert_eq!(input(&usage, "last5Hours"), 100);
    let today = if recent >= today_start { 100 } else { 0 };
    assert_eq!(input(&usage, "today"), today);
    assert!(usage["liveAgents"].is_u64());
    assert!(usage["computedAt"].is_string());
    assert_eq!(usage["ready"], true);
    let by_repo: Vec<(&str, &str, u64)> = usage["byRepo"]
        .as_array()
        .unwrap()
        .iter()
        .map(|r| {
            (
                r["path"].as_str().unwrap(),
                r["name"].as_str().unwrap(),
                r["last7Days"]["input"].as_u64().unwrap(),
            )
        })
        .collect();
    assert_eq!(
        by_repo,
        [
            (scratch.to_str().unwrap(), "scratch", 1_000),
            (repo.to_str().unwrap(), "tidepool-api", 120),
        ]
    );

    // A line written in two parts counts once it is complete.
    let line = response(&repo, "msg_5", Utc::now(), 3, 1);
    let (head, tail) = line.split_at(line.len() / 2);
    append(&session, head);
    std::thread::sleep(Duration::from_millis(300));
    assert_eq!(input(&agent.call("usage")["result"], "last7Days"), 1_120);
    append(&session, tail);
    agent.usage_until(|u| input(u, "last5Hours") == 103);

    // A new project directory, and a new session directory with a subagent in it.
    let kelp = tmp.path().join("kelp-cli");
    append(
        &project_dir(&projects, &kelp).join("6d4e0000-0000-4000-8000-000000000003.jsonl"),
        &response(&kelp, "msg_6", Utc::now(), 40_000, 1),
    );
    append(
        &project_dir(&projects, &scratch)
            .join("6d4e0000-0000-4000-8000-000000000004/subagents/agent-b2.jsonl"),
        &response(&scratch, "msg_7", Utc::now(), 500_000, 2),
    );
    let usage = agent.usage_until(|u| input(u, "last5Hours") == 540_103);
    assert_eq!(usage["byRepo"][0]["name"], "scratch");
    assert_eq!(usage["byRepo"][0]["last7Days"]["input"], 501_000);
    assert_eq!(usage["byRepo"][1]["name"], "kelp-cli");

    // A transcript replaced by a shorter one is read again from the start.
    fs::write(&session, response(&repo, "msg_8", Utc::now(), 7, 1)).unwrap();
    agent.usage_until(|u| input(u, "last5Hours") == 540_007);

    agent.call("shutdown");
    assert!(agent.child.wait().unwrap().success());
}

#[test]
fn usage_with_no_projects_directory_is_zero() {
    let tmp = tempfile::tempdir().unwrap();
    let home = tmp.path().join("home");
    let config = tmp.path().join("agent.toml");
    write(&config, "roots = []\n");
    let mut agent = Agent::spawn(&home, &config);
    // With no transcripts at all, the zeros are real.
    let usage = agent.usage_until(|u| u["ready"] == true);
    assert_eq!(
        usage["last7Days"],
        json!({"input": 0, "output": 0, "cacheRead": 0, "cacheWrite": 0})
    );
    assert_eq!(usage["byRepo"], json!([]));
    agent.call("shutdown");
    assert!(agent.child.wait().unwrap().success());
}

/// Sets a directory's mtime `days` ago.
fn age(path: &Path, days: u64) {
    let then = std::time::SystemTime::now() - Duration::from_secs(days * 24 * 3600);
    fs::File::open(path).unwrap().set_modified(then).unwrap();
}

#[test]
fn a_resumed_old_session_has_its_new_subagents_counted() {
    let tmp = tempfile::tempdir().unwrap();
    let home = tmp.path().join("home");
    let config = tmp.path().join("agent.toml");
    write(&config, "roots = []\n");
    let projects = home.join(".claude/projects");
    let scratch = tmp.path().join("scratch");
    let project = project_dir(&projects, &scratch);
    // A session last written 30 days ago, with a subagent transcript from then.
    let session = project.join("6d4e0000-0000-4000-8000-000000000005");
    let subagents = session.join("subagents");
    let old = subagents.join("agent-a1.jsonl");
    append(
        &old,
        &response(&scratch, "msg_1", Utc::now() - TimeDelta::days(30), 9, 1),
    );
    let transcript = session.with_extension("jsonl");
    append(
        &transcript,
        &response(&scratch, "msg_2", Utc::now() - TimeDelta::days(30), 9, 1),
    );
    for path in [&old, &transcript, &subagents, &session] {
        age(path, 30);
    }

    let mut agent = Agent::spawn(&home, &config);
    agent.usage_until(|u| u["ready"] == true && input(u, "last7Days") == 0);
    // Resumed: the session transcript is written, then once that is counted (so
    // its directory is watched), a new subagent starts in the old directory.
    append(&transcript, &response(&scratch, "msg_3", Utc::now(), 10, 1));
    agent.usage_until(|u| input(u, "last5Hours") == 10);
    append(
        &subagents.join("agent-b2.jsonl"),
        &response(&scratch, "msg_4", Utc::now(), 300, 1),
    );
    agent.usage_until(|u| input(u, "last5Hours") == 310);

    agent.call("shutdown");
    assert!(agent.child.wait().unwrap().success());
}
