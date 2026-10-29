//! Runs the built binary over stdin and stdout.

mod common;

use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStdin, Command, Stdio};
use std::sync::mpsc::{self, Receiver};
use std::time::Duration;

use chrono::{TimeDelta, Utc};
use serde_json::{json, Value};

use common::*;

const SESSION_ID: &str = "5e2a7c10-0000-4000-8000-0000000000aa";

struct Agent {
    child: Child,
    stdin: ChildStdin,
    lines: Receiver<String>,
}

impl Agent {
    fn spawn(home: &std::path::Path, config: &std::path::Path) -> Agent {
        Agent::spawn_with(home, config, &[])
    }

    fn spawn_with(home: &Path, config: &Path, env: &[(&str, &std::ffi::OsStr)]) -> Agent {
        let mut child = Command::new(env!("CARGO_BIN_EXE_refract-agent"))
            .envs(env.iter().copied())
            .env("HOME", home)
            .env("REFRACT_AGENT_CONFIG", config)
            .env_remove("XDG_CONFIG_HOME")
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
        }
    }

    fn send(&mut self, message: Value) {
        writeln!(self.stdin, "{message}").unwrap();
        self.stdin.flush().unwrap();
    }

    fn send_raw(&mut self, bytes: &[u8]) {
        self.stdin.write_all(bytes).unwrap();
        self.stdin.flush().unwrap();
    }

    /// Asks for the repo list once the first load is complete.
    fn list_when_loaded(&mut self, id: u64) -> Value {
        self.send(json!({"id": id, "method": "listRepos"}));
        let first = self.response();
        if first["result"]["loading"] == false {
            return first;
        }
        self.event(|e| e["event"] == "loadComplete");
        self.send(json!({"id": id, "method": "listRepos"}));
        let list = self.response();
        assert_eq!(list["result"]["loading"], false);
        list
    }

    /// Next line on stdout, which must be valid JSON.
    fn next(&self, timeout: Duration) -> Value {
        let line = self
            .lines
            .recv_timeout(timeout)
            .expect("agent wrote nothing in time");
        serde_json::from_str(&line).unwrap_or_else(|e| panic!("not JSON: {line}: {e}"))
    }

    /// Next message that is a response (skipping events).
    fn response(&self) -> Value {
        loop {
            let message = self.next(Duration::from_secs(20));
            if message.get("id").is_some() {
                return message;
            }
        }
    }

    /// Waits for an event matching `pred`.
    fn event(&self, pred: impl Fn(&Value) -> bool) -> Value {
        let deadline = std::time::Instant::now() + Duration::from_secs(10);
        loop {
            let left = deadline.saturating_duration_since(std::time::Instant::now());
            let message = self.next(left);
            if message.get("event").is_some() && pred(&message) {
                return message;
            }
        }
    }
}

#[test]
fn ping_list_watch_and_shutdown() {
    let tmp = tempfile::tempdir().unwrap();
    let home = tmp.path().join("home");
    let root = tmp.path().join("projects");
    let repo = root.join("tidepool-api");
    init_repo(&repo);
    init_repo(&root.join("aurora-notes"));
    let config = tmp.path().join("agent.toml");
    write(
        &config,
        &format!("roots = [{:?}]\n", root.to_str().unwrap()),
    );
    let projects = home.join(".claude/projects");
    std::fs::create_dir_all(&projects).unwrap();

    let mut agent = Agent::spawn(&home, &config);

    agent.send(json!({"id": 1, "method": "ping"}));
    let hello = agent.response();
    assert_eq!(hello["id"], 1);
    assert_eq!(hello["result"]["protocolVersion"], 1);
    assert_eq!(hello["result"]["agentVersion"], env!("CARGO_PKG_VERSION"));

    let started = std::time::Instant::now();
    let list = agent.list_when_loaded(2);
    assert_eq!(list["id"], 2);
    let repos = list["result"]["repos"].as_array().unwrap();
    let names: Vec<&str> = repos.iter().map(|r| r["name"].as_str().unwrap()).collect();
    assert_eq!(names, ["aurora-notes", "tidepool-api"]);
    assert_eq!(repos[1]["changedFiles"], 0);
    assert_eq!(repos[1]["preselect"], json!({"kind": "noPicker"}));
    assert!(started.elapsed() < Duration::from_secs(10));

    agent.send(json!({"id": 3, "method": "getRepo", "params": {"path": repo.to_str().unwrap()}}));
    let one = agent.response();
    assert_eq!(one["result"]["name"], "tidepool-api");

    agent.send(json!({"id": 4, "method": "getRepo", "params": {"path": "/nowhere/kelp-cli"}}));
    assert_eq!(agent.response()["error"]["code"], 1);
    agent.send(json!({"id": 5, "method": "explode"}));
    assert_eq!(agent.response()["error"]["code"], -32601);
    agent.send(json!({"id": 6}));
    assert_eq!(agent.response()["error"]["code"], -32600);

    // Editing a tracked file is pushed as a repoChanged event.
    write(&repo.join("src/main.rs"), "fn main() { todo!() }\n");
    let changed =
        agent.event(|e| e["event"] == "repoChanged" && e["data"]["name"] == "tidepool-api");
    assert_eq!(changed["data"]["changedFiles"], 1);
    assert_eq!(changed["data"]["preselect"], json!({"kind": "newAgent"}));

    // A transcript appearing for the repo is pushed too; it is new, so as repoChanged.
    let dir = project_dir(&projects, &repo);
    let edited = repo.join("src/main.rs");
    write(
        &dir.join(format!("{SESSION_ID}.jsonl")),
        &transcript(
            SESSION_ID,
            &repo,
            &edited,
            Utc::now() - TimeDelta::minutes(30),
            false,
        ),
    );
    let with_session = agent.event(|e| {
        e["event"] == "repoChanged"
            && e["data"]["name"] == "tidepool-api"
            && !e["data"]["sessions"].as_array().unwrap().is_empty()
    });
    assert_eq!(with_session["data"]["sessions"][0]["id"], SESSION_ID);
    assert_eq!(
        with_session["data"]["preselect"],
        json!({"kind": "resume", "sessionId": SESSION_ID})
    );

    // A new repo under a root appears without asking.
    init_repo(&root.join("kelp-cli"));
    let added = agent.event(|e| e["event"] == "repoChanged" && e["data"]["name"] == "kelp-cli");
    assert_eq!(added["data"]["branch"], "main");

    agent.send(json!({"id": 7, "method": "shutdown"}));
    let bye = agent.response();
    assert_eq!(bye, json!({"id": 7, "result": null}));
    let status = agent.child.wait().unwrap();
    assert!(status.success());
}

#[test]
fn end_of_input_exits_cleanly() {
    let tmp = tempfile::tempdir().unwrap();
    let config = tmp.path().join("agent.toml");
    write(&config, "roots = []\n");
    let mut agent = Agent::spawn(tmp.path(), &config);
    let list = agent.list_when_loaded(1);
    assert_eq!(list["result"]["repos"], json!([]));
    drop(agent.stdin);
    assert!(agent.child.wait().unwrap().success());
}

/// A config with one root, plus an empty Claude `projects` directory.
fn setup(tmp: &Path) -> (PathBuf, PathBuf, PathBuf) {
    let home = tmp.join("home");
    let root = tmp.join("projects");
    std::fs::create_dir_all(&root).unwrap();
    let config = tmp.join("agent.toml");
    write(
        &config,
        &format!("roots = [{:?}]\ndepth = 3\n", root.to_str().unwrap()),
    );
    std::fs::create_dir_all(home.join(".claude/projects")).unwrap();
    (home, root, config)
}

#[test]
fn odd_input_is_answered_and_the_helper_keeps_running() {
    let tmp = tempfile::tempdir().unwrap();
    let (home, _, config) = setup(tmp.path());
    let mut agent = Agent::spawn(&home, &config);

    agent.send(json!({"id": 1, "method": "ping", "params": {}}));
    assert_eq!(agent.response()["result"]["protocolVersion"], 1);

    agent.send(json!({"id": "seven", "method": "ping"}));
    assert_eq!(agent.response()["id"], "seven");

    agent.send(json!({"id": {"n": 1}, "method": "ping"}));
    let bad_id = agent.response();
    assert_eq!(bad_id["id"], Value::Null);
    assert_eq!(bad_id["error"]["code"], -32600);

    agent.send_raw(b"{\"id\":2,\"method\":\"ping\",\"x\":\"\xff\"}\n");
    let bad_utf8 = agent.response();
    assert_eq!(bad_utf8["id"], 2);
    assert_eq!(bad_utf8["error"]["code"], -32700);

    agent.send_raw(b"{not json\n");
    assert_eq!(agent.response()["id"], Value::Null);

    let mut long = vec![b' '; 2 * 1024 * 1024];
    long.push(b'\n');
    agent.send_raw(&long);
    let too_long = agent.response();
    assert_eq!(too_long["id"], Value::Null);
    assert_eq!(too_long["error"]["code"], -32600);

    agent.send(json!({"id": 3, "method": "ping"}));
    assert_eq!(agent.response()["id"], 3);
    agent.send(json!({"id": 4, "method": "shutdown"}));
    assert_eq!(agent.response()["id"], 4);
    assert!(agent.child.wait().unwrap().success());
}

#[test]
fn repos_appearing_later_or_deeper_are_found() {
    let tmp = tempfile::tempdir().unwrap();
    let (home, root, config) = setup(tmp.path());
    std::fs::create_dir_all(root.join("group")).unwrap();
    let mut agent = Agent::spawn(&home, &config);
    agent.list_when_loaded(1);

    // A directory made first and turned into a repo well after the debounce.
    std::fs::create_dir_all(root.join("kelp-cli")).unwrap();
    std::thread::sleep(Duration::from_millis(1200));
    init_repo(&root.join("kelp-cli"));
    agent.event(|e| e["event"] == "repoChanged" && e["data"]["name"] == "kelp-cli");

    // A repo two and three levels down, in directories that already existed or not.
    init_repo(&root.join("group/aurora-notes"));
    agent.event(|e| e["event"] == "repoChanged" && e["data"]["name"] == "aurora-notes");
    std::fs::create_dir_all(root.join("group/team/reef-maps")).unwrap();
    std::thread::sleep(Duration::from_millis(1200));
    init_repo(&root.join("group/team/reef-maps"));
    agent.event(|e| e["event"] == "repoChanged" && e["data"]["name"] == "reef-maps");

    // A removed repo is reported.
    std::fs::remove_dir_all(root.join("kelp-cli")).unwrap();
    let removed = agent.event(|e| e["event"] == "repoRemoved");
    assert_eq!(
        removed["data"]["path"],
        root.join("kelp-cli").to_str().unwrap()
    );
}

#[test]
fn a_recreated_project_dir_is_watched_again() {
    let tmp = tempfile::tempdir().unwrap();
    let (home, root, config) = setup(tmp.path());
    let repo = root.join("tidepool-api");
    init_repo(&repo);
    let projects = home.join(".claude/projects");
    let dir = project_dir(&projects, &repo);
    std::fs::create_dir_all(&dir).unwrap();
    let mut agent = Agent::spawn(&home, &config);
    agent.list_when_loaded(1);

    std::fs::remove_dir_all(&dir).unwrap();
    std::thread::sleep(Duration::from_millis(800));
    std::fs::create_dir_all(&dir).unwrap();
    std::thread::sleep(Duration::from_millis(800));
    let edited = repo.join("src/main.rs");
    let at = Utc::now() - TimeDelta::minutes(30);
    write(
        &dir.join(format!("{SESSION_ID}.jsonl")),
        &transcript(SESSION_ID, &repo, &edited, at, true),
    );
    agent.event(|e| {
        e["event"] == "repoChanged"
            && e["data"]["sessions"]
                .as_array()
                .is_some_and(|s| !s.is_empty())
    });

    // Appending to the transcript in the recreated directory is seen too.
    let mut file = std::fs::OpenOptions::new()
        .append(true)
        .open(dir.join(format!("{SESSION_ID}.jsonl")))
        .unwrap();
    let more = transcript(SESSION_ID, &repo, &edited, Utc::now(), false);
    file.write_all(more.lines().last().unwrap().as_bytes())
        .unwrap();
    file.write_all(b"\n").unwrap();
    drop(file);
    agent.event(|e| e["event"] == "sessionChanged" || e["event"] == "repoChanged");
}

#[test]
fn commits_in_a_linked_worktree_are_seen() {
    let tmp = tempfile::tempdir().unwrap();
    let (home, root, config) = setup(tmp.path());
    let main = tmp.path().join("elsewhere/tidepool-api");
    init_repo(&main);
    let worktree = root.join("tidepool-wt");
    git(
        &main,
        &[
            "worktree",
            "add",
            "-q",
            "-b",
            "feat",
            worktree.to_str().unwrap(),
        ],
    );
    let mut agent = Agent::spawn(&home, &config);
    let list = agent.list_when_loaded(1);
    assert_eq!(list["result"]["repos"][0]["name"], "tidepool-wt");

    commit_file(&worktree, "src/lib.rs", "pub fn tide() {}\n", "Add tide");
    agent
        .event(|e| e["event"] == "repoChanged" && e["data"]["lastCommit"]["summary"] == "Add tide");
}

#[test]
fn list_repos_reports_loading_and_get_repo_waits_for_the_load() {
    let tmp = tempfile::tempdir().unwrap();
    let (home, root, config) = setup(tmp.path());
    let repo = root.join("tidepool-api");
    init_repo(&repo);
    let mut agent = Agent::spawn(&home, &config);

    agent.send(json!({"id": 1, "method": "listRepos"}));
    agent.send(json!({"id": 2, "method": "getRepo", "params": {"path": repo.to_str().unwrap()}}));
    let mut saw_complete = false;
    let mut got = std::collections::HashMap::new();
    while got.len() < 2 || !saw_complete {
        let message = agent.next(Duration::from_secs(20));
        if message["event"] == "loadComplete" {
            assert_eq!(message, json!({"event": "loadComplete"}));
            saw_complete = true;
        } else if let Some(id) = message["id"].as_u64() {
            got.insert(id, message);
        }
    }
    assert!(got[&1]["result"]["loading"].is_boolean());
    assert_eq!(got[&2]["result"]["name"], "tidepool-api");

    // After the load, an unknown repo is an error straight away.
    agent.send(json!({"id": 3, "method": "getRepo", "params": {"path": "/nowhere/kelp-cli"}}));
    assert_eq!(agent.response()["error"]["code"], 1);
}

#[test]
fn an_edit_made_while_git_is_still_loading_is_seen() {
    use std::os::unix::fs::PermissionsExt;
    let tmp = tempfile::tempdir().unwrap();
    let (home, root, config) = setup(tmp.path());
    let repo = root.join("tidepool-api");
    init_repo(&repo);

    // A git that is slow to list tracked files while `slow` exists.
    let real_git = Command::new("sh")
        .args(["-c", "command -v git"])
        .output()
        .unwrap();
    let real_git = String::from_utf8(real_git.stdout).unwrap();
    let slow = tmp.path().join("slow");
    let bin = tmp.path().join("bin");
    let shim = bin.join("git");
    write(
        &shim,
        &format!(
            "#!/bin/sh\ncase \"$*\" in *\"ls-tree -r\"*) [ -e {slow:?} ] && sleep 2;; esac\nexec {} \"$@\"\n",
            real_git.trim()
        ),
    );
    std::fs::set_permissions(&shim, std::fs::Permissions::from_mode(0o755)).unwrap();
    write(&slow, "");
    let path = std::env::join_paths(
        std::iter::once(bin).chain(std::env::split_paths(&std::env::var_os("PATH").unwrap())),
    )
    .unwrap();
    let mut agent = Agent::spawn_with(&home, &config, &[("PATH", path.as_os_str())]);

    agent.send(json!({"id": 1, "method": "listRepos"}));
    let first = agent.response();
    assert_eq!(first["result"]["loading"], true);
    std::thread::sleep(Duration::from_millis(500));
    write(&repo.join("src/main.rs"), "fn main() { todo!() }\n");
    std::fs::remove_file(&slow).unwrap();
    agent.event(|e| e["event"] == "loadComplete");
    agent.send(json!({"id": 2, "method": "getRepo", "params": {"path": repo.to_str().unwrap()}}));
    assert_eq!(agent.response()["result"]["changedFiles"], 1);
}

#[test]
fn prepare_launch_writes_a_request_file() {
    let tmp = tempfile::tempdir().unwrap();
    let (home, root, config) = setup(tmp.path());
    let repo = root.join("tidepool-api");
    init_repo(&repo);
    let state = tmp.path().join("state");
    let mut agent = Agent::spawn_with(
        &home,
        &config,
        &[
            ("XDG_STATE_HOME", state.as_os_str()),
            ("WSL_DISTRO_NAME", "Ubuntu".as_ref()),
        ],
    );
    let repo_str = repo.to_str().unwrap();

    agent.send(json!({"id": 1, "method": "prepareLaunch", "params": {
        "path": repo_str, "mode": {"kind": "new", "prompt": "Add tide tables"}}}));
    let ticket = agent.response();
    assert_eq!(ticket["id"], 1);
    let result = &ticket["result"];
    let request_id = result["requestId"].as_str().unwrap();
    let session_id = result["sessionId"].as_str().unwrap();
    assert_eq!(request_id.len(), 36);
    assert_eq!(session_id.len(), 36);
    assert_ne!(request_id, session_id);
    assert_eq!(
        result["folderUri"],
        format!("vscode-remote://wsl+Ubuntu{repo_str}")
    );
    let file = state.join(format!("refract/launch/{request_id}.json"));
    let request: Value = serde_json::from_str(&std::fs::read_to_string(&file).unwrap()).unwrap();
    assert_eq!(request["version"], 1);
    assert_eq!(request["requestId"], request_id);
    assert_eq!(request["folder"], repo_str);
    assert_eq!(
        request["argv"],
        json!([
            "claude",
            "--session-id",
            session_id,
            "--",
            "Add tide tables"
        ])
    );
    assert_eq!(request["title"], "New agent");
    let created: chrono::DateTime<Utc> = request["createdAt"].as_str().unwrap().parse().unwrap();
    assert!((Utc::now() - created).num_seconds().abs() < 60);

    let resumed = "5e2a7c10-0000-4000-8000-0000000000bb";
    agent.send(json!({"id": 2, "method": "prepareLaunch", "params": {
        "path": format!("{repo_str}/"), "mode": {"kind": "resume", "sessionId": resumed}}}));
    let ticket = agent.response();
    assert_eq!(ticket["result"]["sessionId"], resumed);
    let request_id = ticket["result"]["requestId"].as_str().unwrap();
    let file = state.join(format!("refract/launch/{request_id}.json"));
    let request: Value = serde_json::from_str(&std::fs::read_to_string(&file).unwrap()).unwrap();
    assert_eq!(request["argv"], json!(["claude", "--resume", resumed]));
    assert_eq!(request["title"], "Resume");
    assert_eq!(request["folder"], repo_str);

    let missing = root.join("kelp-cli");
    agent.send(json!({"id": 3, "method": "prepareLaunch", "params": {
        "path": missing.to_str().unwrap(), "mode": {"kind": "new"}}}));
    assert_eq!(agent.response()["error"]["code"], 1);

    agent.send(json!({"id": 4, "method": "shutdown"}));
    assert_eq!(agent.response(), json!({"id": 4, "result": null}));
    assert!(agent.child.wait().unwrap().success());
}
