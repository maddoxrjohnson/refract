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
            // Never the real state or data directories.
            .env_remove("XDG_STATE_HOME")
            .env_remove("XDG_DATA_HOME")
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

    /// Next line on stdout, which must be valid JSON. A timeout is reported at the line
    /// of the test that waited.
    #[track_caller]
    fn next(&self, timeout: Duration) -> Value {
        let line = self
            .lines
            .recv_timeout(timeout)
            .expect("agent wrote nothing in time");
        serde_json::from_str(&line).unwrap_or_else(|e| panic!("not JSON: {line}: {e}"))
    }

    /// Next message that is a response (skipping events).
    #[track_caller]
    fn response(&self) -> Value {
        loop {
            let message = self.next(Duration::from_secs(20));
            if message.get("id").is_some() {
                return message;
            }
        }
    }

    /// Waits for an event matching `pred`.
    #[track_caller]
    fn event(&self, pred: impl Fn(&Value) -> bool) -> Value {
        self.event_within(Duration::from_secs(10), pred)
    }

    /// Waits at most `timeout` for an event matching `pred`.
    #[track_caller]
    fn event_within(&self, timeout: Duration, pred: impl Fn(&Value) -> bool) -> Value {
        let deadline = std::time::Instant::now() + timeout;
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
fn a_repo_git_cannot_read_yet_is_found_once_it_can() {
    let tmp = tempfile::tempdir().unwrap();
    let (home, root, config) = setup(tmp.path());
    let repos = ["kelp-cli", "group/aurora-notes", "group/team/reef-maps"];
    for repo in repos {
        std::fs::create_dir_all(root.join(repo)).unwrap();
    }
    let mut agent = Agent::spawn(&home, &config);
    agent.list_when_loaded(1);

    // A `.git` that is not a repo yet, as while `git init` or a copy is still writing
    // it, at each depth. The search finds it and git cannot read it.
    for repo in repos {
        std::fs::create_dir(root.join(repo).join(".git")).unwrap();
    }
    std::thread::sleep(Duration::from_millis(1200));

    for repo in repos {
        init_repo(&root.join(repo));
    }
    let mut missing: Vec<&str> = repos
        .iter()
        .map(|r| r.rsplit('/').next().unwrap())
        .collect();
    while !missing.is_empty() {
        let changed = agent.event(|e| e["event"] == "repoChanged");
        let name = changed["data"]["name"].as_str().unwrap();
        missing.retain(|m| *m != name);
    }
}

#[test]
fn a_repo_git_cannot_read_at_the_first_load_is_found_later() {
    let tmp = tempfile::tempdir().unwrap();
    let (home, root, config) = setup(tmp.path());
    let repo = root.join("kelp-cli");
    std::fs::create_dir_all(repo.join(".git")).unwrap();
    let mut agent = Agent::spawn(&home, &config);
    let list = agent.list_when_loaded(1);
    assert_eq!(list["result"]["repos"], json!([]));

    init_repo(&repo);
    agent.event(|e| e["event"] == "repoChanged" && e["data"]["name"] == "kelp-cli");
}

#[test]
fn a_repo_copied_in_with_its_objects_last_is_found() {
    let tmp = tempfile::tempdir().unwrap();
    let (home, root, config) = setup(tmp.path());
    let seed = tmp.path().join("seed/kelp-cli");
    init_repo(&seed);
    git(&seed, &["gc", "-q"]);
    let mut agent = Agent::spawn(&home, &config);
    agent.list_when_loaded(1);

    // A copy that writes `objects` last, as `cp -r` or `tar` may. When the search finds
    // the repo its commit is missing, and the pack then lands in `objects/pack`, which
    // nothing watches.
    let repo = root.join("kelp-cli");
    std::fs::create_dir_all(repo.join(".git")).unwrap();
    copy_dir(&seed.join("src"), &repo.join("src"));
    for entry in std::fs::read_dir(seed.join(".git")).unwrap() {
        let entry = entry.unwrap();
        let dest = repo.join(".git").join(entry.file_name());
        if entry.file_name() == "objects" {
            continue;
        } else if entry.file_type().unwrap().is_dir() {
            copy_dir(&entry.path(), &dest);
        } else {
            std::fs::copy(entry.path(), dest).unwrap();
        }
    }
    std::fs::create_dir(repo.join(".git/objects")).unwrap();
    std::thread::sleep(Duration::from_millis(1000));
    copy_dir(&seed.join(".git/objects"), &repo.join(".git/objects"));
    agent.event(|e| e["event"] == "repoChanged" && e["data"]["name"] == "kelp-cli");
}

#[test]
fn a_repo_copied_in_is_found_soon_after_the_copy_ends() {
    let tmp = tempfile::tempdir().unwrap();
    let (home, root, config) = setup(tmp.path());
    let seed = tmp.path().join("seed/kelp-cli");
    init_repo(&seed);
    git(&seed, &["gc", "-q"]);
    let mut agent = Agent::spawn(&home, &config);
    agent.list_when_loaded(1);

    // A copy that is slow in `objects` and writes `refs` last: git cannot read the repo
    // until then, and the timed retries have grown long by the time it can.
    let repo = root.join("kelp-cli");
    std::fs::create_dir_all(repo.join(".git/objects")).unwrap();
    for name in ["HEAD", "config", "description", "index", "packed-refs"] {
        std::fs::copy(seed.join(".git").join(name), repo.join(".git").join(name)).unwrap();
    }
    std::thread::sleep(Duration::from_millis(4000));
    copy_dir(&seed.join(".git/objects"), &repo.join(".git/objects"));
    copy_dir(&seed.join(".git/refs"), &repo.join(".git/refs"));
    agent.event_within(Duration::from_secs(5), |e| {
        e["event"] == "repoChanged" && e["data"]["name"] == "kelp-cli"
    });
}

#[test]
fn a_repo_replaced_at_the_same_path_is_watched_anew() {
    let tmp = tempfile::tempdir().unwrap();
    let (home, root, config) = setup(tmp.path());
    let repo = root.join("kelp-cli");
    init_repo(&repo);
    let seed = tmp.path().join("seed/kelp-cli");
    init_repo(&seed);
    let mut agent = Agent::spawn(&home, &config);
    agent.list_when_loaded(1);

    // Moved away, with its watches, and another clone put in its place at once.
    std::fs::rename(&repo, tmp.path().join("old-kelp-cli")).unwrap();
    git(
        tmp.path(),
        &[
            "clone",
            "-q",
            seed.to_str().unwrap(),
            repo.to_str().unwrap(),
        ],
    );
    agent.event(|e| e["event"] == "repoChanged" && e["data"]["name"] == "kelp-cli");

    write(&repo.join("src/main.rs"), "fn main() { todo!() }\n");
    agent.event(|e| {
        e["event"] == "repoChanged"
            && e["data"]["name"] == "kelp-cli"
            && e["data"]["changedFiles"] == 1
    });
}

#[test]
fn a_repo_git_cannot_read_does_not_report_the_work_tree_around_it() {
    let tmp = tempfile::tempdir().unwrap();
    let home = tmp.path().join("home");
    std::fs::create_dir_all(home.join(".claude/projects")).unwrap();
    // The root lies inside a work tree, and holds a `.git` git cannot read.
    let outer = tmp.path().join("outer");
    init_repo(&outer);
    let root = outer.join("projects");
    std::fs::create_dir_all(root.join("kelp-cli/.git")).unwrap();
    let config = tmp.path().join("agent.toml");
    write(
        &config,
        &format!("roots = [{:?}]\ndepth = 3\n", root.to_str().unwrap()),
    );
    let mut agent = Agent::spawn(&home, &config);
    let list = agent.list_when_loaded(1);
    assert_eq!(list["result"]["repos"], json!([]));
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

/// What the `git` of [`shim_git`] does for some calls.
enum Shim<'a> {
    /// Runs the real git, then holds its output back while the file exists.
    HoldWhile(&'a Path),
    /// Fails without running git while the file exists.
    FailWhile(&'a Path),
}

/// A `PATH` with a `git` first that, for calls whose arguments match a shell `case`
/// pattern, does what the matching [`Shim`] says, and otherwise runs the real git.
fn shim_git(tmp: &Path, arms: &[(&str, Shim)]) -> std::ffi::OsString {
    use std::os::unix::fs::PermissionsExt;
    let real_git = Command::new("sh")
        .args(["-c", "command -v git"])
        .output()
        .unwrap();
    let real_git = String::from_utf8(real_git.stdout).unwrap();
    let real_git = real_git.trim();
    let mut script = String::from("#!/bin/sh\ncase \"$*\" in\n");
    for (pattern, shim) in arms {
        script += &match shim {
            Shim::HoldWhile(file) => format!(
                "{pattern})\n  out=$(mktemp)\n  {real_git} \"$@\" >\"$out\"\n  code=$?\n  while [ -e {file:?} ]; do sleep 0.05; done\n  cat \"$out\"; rm -f \"$out\"; exit $code;;\n"
            ),
            Shim::FailWhile(file) => format!("{pattern})\n  [ -e {file:?} ] && exit 128;;\n"),
        };
    }
    script += &format!("esac\nexec {real_git} \"$@\"\n");
    let bin = tmp.join("bin");
    let shim = bin.join("git");
    write(&shim, &script);
    std::fs::set_permissions(&shim, std::fs::Permissions::from_mode(0o755)).unwrap();
    std::env::join_paths(
        std::iter::once(bin).chain(std::env::split_paths(&std::env::var_os("PATH").unwrap())),
    )
    .unwrap()
}

/// A config with one root for repos loaded as if on a Windows drive, plus an empty Claude
/// `projects` directory.
fn setup_drive(tmp: &Path) -> (PathBuf, PathBuf, PathBuf) {
    let (home, _, config) = setup(tmp);
    let drive = tmp.join("drive");
    std::fs::create_dir_all(&drive).unwrap();
    write(
        &config,
        &format!("roots = [{:?}]\ndepth = 3\n", drive.to_str().unwrap()),
    );
    (home, drive, config)
}

fn spawn_on_drive(home: &Path, config: &Path, drive: &Path, path: &std::ffi::OsStr) -> Agent {
    Agent::spawn_with(
        home,
        config,
        &[
            ("PATH", path),
            ("REFRACT_AGENT_TEST_SLOW_PREFIX", drive.as_os_str()),
        ],
    )
}

/// Every message up to the response to a `ping`, which must not include `loadComplete`.
#[track_caller]
fn assert_no_more_load_complete(agent: &mut Agent, id: u64) {
    agent.send(json!({"id": id, "method": "ping"}));
    loop {
        let message = agent.next(Duration::from_secs(10));
        assert_ne!(message["event"], "loadComplete");
        if message["id"] == id {
            return;
        }
    }
}

#[test]
fn repos_on_a_slow_drive_are_reported_one_at_a_time() {
    let tmp = tempfile::tempdir().unwrap();
    let (home, root, config) = setup(tmp.path());
    init_repo(&root.join("aurora-notes"));
    // Loaded as if under `/mnt/`.
    let drive = tmp.path().join("drive");
    let kelp = drive.join("kelp-cli");
    init_repo(&kelp);
    init_repo(&drive.join("tidepool-api"));
    write(
        &config,
        &format!(
            "roots = [{:?}, {:?}]\ndepth = 3\n",
            root.to_str().unwrap(),
            drive.to_str().unwrap()
        ),
    );
    // kelp-cli's `git status` is held back until `gate` is removed.
    let gate = tmp.path().join("gate");
    write(&gate, "");
    let path = shim_git(
        tmp.path(),
        &[("*kelp-cli*\" status \"*", Shim::HoldWhile(&gate))],
    );
    let mut agent = Agent::spawn_with(
        &home,
        &config,
        &[
            ("PATH", path.as_os_str()),
            ("REFRACT_AGENT_TEST_SLOW_PREFIX", drive.as_os_str()),
        ],
    );

    agent.send(json!({"id": 1, "method": "listRepos"}));
    assert_eq!(agent.response()["result"]["loading"], true);
    let mut reported = Vec::new();
    while !reported.iter().any(|name| name == "tidepool-api") {
        let message = agent.next(Duration::from_secs(20));
        assert_ne!(
            message["event"], "loadComplete",
            "kelp-cli is still loading"
        );
        if message["event"] == "repoChanged" {
            reported.push(message["data"]["name"].as_str().unwrap().to_owned());
        }
    }
    // The Linux repo first, then each slow repo once it is read.
    assert_eq!(reported.first().map(String::as_str), Some("aurora-notes"));
    assert!(!reported.iter().any(|name| name == "kelp-cli"));

    // The list so far, still loading.
    agent.send(json!({"id": 2, "method": "listRepos"}));
    let list = agent.response();
    assert_eq!(list["result"]["loading"], true);
    let mut names: Vec<&str> = list["result"]["repos"]
        .as_array()
        .unwrap()
        .iter()
        .map(|r| r["name"].as_str().unwrap())
        .collect();
    names.sort_unstable();
    assert_eq!(names, ["aurora-notes", "tidepool-api"]);

    // An edit after git read kelp-cli, before its facts arrive, is not lost.
    write(&kelp.join("README.md"), "# kelp\n");
    std::thread::sleep(Duration::from_secs(1));
    std::fs::remove_file(&gate).unwrap();
    let kelp_event =
        agent.event(|e| e["event"] == "repoChanged" && e["data"]["name"] == "kelp-cli");
    assert_eq!(kelp_event["data"]["changedFiles"], 1);
    agent.event(|e| e["event"] == "loadComplete");

    // loadComplete comes once.
    assert_no_more_load_complete(&mut agent, 3);
    let list = agent.list_when_loaded(4);
    assert_eq!(list["result"]["repos"].as_array().unwrap().len(), 3);
}

#[test]
fn repos_appearing_on_either_drive_while_slow_repos_load_are_found() {
    let tmp = tempfile::tempdir().unwrap();
    let (home, root, config) = setup(tmp.path());
    init_repo(&root.join("aurora-notes"));
    std::fs::create_dir_all(root.join("group")).unwrap();
    // Searched and loaded as if under `/mnt/`.
    let drive = tmp.path().join("drive");
    init_repo(&drive.join("kelp-cli"));
    std::fs::create_dir_all(drive.join("group")).unwrap();
    write(
        &config,
        &format!(
            "roots = [{:?}, {:?}]\ndepth = 3\n",
            root.to_str().unwrap(),
            drive.to_str().unwrap()
        ),
    );
    let gate = tmp.path().join("gate");
    write(&gate, "");
    let path = shim_git(
        tmp.path(),
        &[("*kelp-cli*\" status \"*", Shim::HoldWhile(&gate))],
    );
    let mut agent = spawn_on_drive(&home, &config, &drive, &path);

    agent.send(json!({"id": 1, "method": "listRepos"}));
    assert_eq!(agent.response()["result"]["loading"], true);
    agent.event(|e| e["event"] == "repoChanged" && e["data"]["name"] == "aurora-notes");
    // While kelp-cli is still loading.
    init_repo(&root.join("group/reef-maps"));
    init_repo(&drive.join("group/tidepool-api"));
    std::thread::sleep(Duration::from_millis(500));
    std::fs::remove_file(&gate).unwrap();
    // A late /mnt search can still list tidepool-api in the load itself, so its event
    // may come before or after loadComplete.
    let mut missing = vec!["reef-maps", "tidepool-api"];
    let mut complete = false;
    let deadline = std::time::Instant::now() + Duration::from_secs(30);
    while !missing.is_empty() || !complete {
        let left = deadline.saturating_duration_since(std::time::Instant::now());
        let message = agent.next(left);
        if message["event"] == "loadComplete" {
            assert!(!complete, "loadComplete sent twice");
            complete = true;
        } else if message["event"] == "repoChanged" {
            missing.retain(|name| message["data"]["name"] != *name);
        }
    }
    assert_no_more_load_complete(&mut agent, 2);
}

#[test]
fn a_held_get_repo_is_answered_when_its_slow_repo_arrives() {
    let tmp = tempfile::tempdir().unwrap();
    let (home, drive, config) = setup_drive(tmp.path());
    let kelp = drive.join("kelp-cli");
    init_repo(&kelp);
    init_repo(&drive.join("tidepool-api"));
    let (kelp_gate, tidepool_gate) = (tmp.path().join("kelp-gate"), tmp.path().join("tide-gate"));
    write(&kelp_gate, "");
    write(&tidepool_gate, "");
    let path = shim_git(
        tmp.path(),
        &[
            ("*kelp-cli*\" status \"*", Shim::HoldWhile(&kelp_gate)),
            (
                "*tidepool-api*\" status \"*",
                Shim::HoldWhile(&tidepool_gate),
            ),
        ],
    );
    let mut agent = spawn_on_drive(&home, &config, &drive, &path);

    agent.send(json!({"id": 1, "method": "listRepos"}));
    assert_eq!(agent.response()["result"]["loading"], true);
    agent.send(json!({"id": 2, "method": "getRepo", "params": {"path": kelp.to_str().unwrap()}}));
    std::thread::sleep(Duration::from_millis(300));
    std::fs::remove_file(&kelp_gate).unwrap();
    // Answered while tidepool-api is still held, so before loadComplete.
    let answer = loop {
        let message = agent.next(Duration::from_secs(20));
        assert_ne!(message["event"], "loadComplete");
        if message["id"] == 2 {
            break message;
        }
    };
    assert_eq!(answer["result"]["name"], "kelp-cli");
    std::fs::remove_file(&tidepool_gate).unwrap();
    agent.event(|e| e["event"] == "loadComplete");
}

#[test]
fn a_slow_repo_git_cannot_read_still_completes_the_load_and_is_found_later() {
    let tmp = tempfile::tempdir().unwrap();
    let (home, drive, config) = setup_drive(tmp.path());
    init_repo(&drive.join("kelp-cli"));
    init_repo(&drive.join("tidepool-api"));
    let broken = tmp.path().join("broken");
    write(&broken, "");
    let path = shim_git(
        tmp.path(),
        &[("*kelp-cli*\" status \"*", Shim::FailWhile(&broken))],
    );
    let mut agent = spawn_on_drive(&home, &config, &drive, &path);

    let list = agent.list_when_loaded(1);
    let names: Vec<&str> = list["result"]["repos"]
        .as_array()
        .unwrap()
        .iter()
        .map(|r| r["name"].as_str().unwrap())
        .collect();
    assert_eq!(names, ["tidepool-api"]);
    std::fs::remove_file(&broken).unwrap();
    // Tried again on a timer.
    agent.event_within(Duration::from_secs(20), |e| {
        e["event"] == "repoChanged" && e["data"]["name"] == "kelp-cli"
    });
}

#[test]
fn a_load_of_only_slow_repos_reports_each_and_completes_once() {
    let tmp = tempfile::tempdir().unwrap();
    let (home, drive, config) = setup_drive(tmp.path());
    init_repo(&drive.join("kelp-cli"));
    init_repo(&drive.join("group/tidepool-api"));
    let path = std::env::var_os("PATH").unwrap();
    let mut agent = spawn_on_drive(&home, &config, &drive, &path);

    agent.send(json!({"id": 1, "method": "listRepos"}));
    agent.response();
    let mut reported = Vec::new();
    loop {
        let message = agent.next(Duration::from_secs(20));
        if message["event"] == "loadComplete" {
            break;
        }
        if message["event"] == "repoChanged" {
            reported.push(message["data"]["name"].as_str().unwrap().to_owned());
        }
    }
    reported.sort_unstable();
    reported.dedup();
    assert_eq!(reported, ["kelp-cli", "tidepool-api"]);
    assert_no_more_load_complete(&mut agent, 2);
    let list = agent.list_when_loaded(3);
    assert_eq!(list["result"]["repos"].as_array().unwrap().len(), 2);
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

/// A vsix holding just `extension/package.json` at `version`.
fn write_vsix(path: &Path, version: &str) {
    let file = std::fs::File::create(path).unwrap();
    let mut zip = zip::ZipWriter::new(file);
    let options = zip::write::SimpleFileOptions::default();
    zip.start_file("extension/package.json", options).unwrap();
    zip.write_all(format!(r#"{{"name":"refract","version":"{version}"}}"#).as_bytes())
        .unwrap();
    zip.finish().unwrap();
}

/// A fake `code-server` for `commit` in `home`'s WSL server. It logs its arguments,
/// sleeps a second like a real install, and lists `refract.refract` 0.3.0 in the
/// server's `extensions.json`.
fn fake_code_server(home: &Path, commit: &str) -> PathBuf {
    let server = home.join(".vscode-server");
    let script = server.join(format!("bin/{commit}/bin/code-server"));
    let log = server.join("code-server.log");
    let registry = server.join("extensions/extensions.json");
    write(
        &script,
        &format!(
            "#!/bin/sh\necho \"$@\" >> '{}'\nsleep 1\nmkdir -p '{}'\nprintf '%s' '{}' > '{}'\n",
            log.display(),
            registry.parent().unwrap().display(),
            r#"[{"identifier":{"id":"refract.refract"},"version":"0.3.0","location":{"$mid":1,"path":"/x"}}]"#,
            registry.display(),
        ),
    );
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).unwrap();
    log
}

#[test]
fn setup_server_installs_once_without_blocking_other_requests() {
    let tmp = tempfile::tempdir().unwrap();
    let (home, _root, config) = setup(tmp.path());
    let vsix = tmp.path().join("dist/refract.vsix");
    std::fs::create_dir_all(vsix.parent().unwrap()).unwrap();
    write_vsix(&vsix, "0.3.0");
    let vsix_str = vsix.to_str().unwrap();
    let mut agent = Agent::spawn(&home, &config);

    // No server yet: VS Code has not connected to WSL.
    agent.send(json!({"id": 1, "method": "setupServer", "params": {"vsix": vsix_str}}));
    let skipped = agent.response();
    assert_eq!(skipped["id"], 1);
    assert_eq!(skipped["result"]["installed"], false);
    assert_eq!(skipped["result"]["applicationScoped"], false);
    assert!(skipped["result"]["skipped"]
        .as_str()
        .unwrap()
        .contains("no VS Code server"));

    let log = fake_code_server(&home, "1a2b3c");
    agent.send(json!({"id": 2, "method": "setupServer", "params": {
        "vsix": format!("{vsix_str}/"), "commit": "1a2b3c"}}));
    agent.send(json!({"id": 3, "method": "ping"}));
    let first = agent.response();
    assert_eq!(first["id"], 3, "ping waited for the install: {first}");
    let done = agent.response();
    assert_eq!(
        done,
        json!({"id": 2, "result": {"installed": true, "applicationScoped": true}})
    );
    let calls = std::fs::read_to_string(&log).unwrap();
    assert_eq!(calls, format!("--install-extension {vsix_str} --force\n"));
    let registry: Value = serde_json::from_str(
        &std::fs::read_to_string(home.join(".vscode-server/extensions/extensions.json")).unwrap(),
    )
    .unwrap();
    assert_eq!(registry[0]["metadata"]["isApplicationScoped"], true);
    assert_eq!(registry[0]["location"]["$mid"], 1);

    // Done already, and the newest server is used without a commit.
    agent.send(json!({"id": 4, "method": "setupServer", "params": {
        "vsix": vsix_str, "commit": null}}));
    assert_eq!(
        agent.response(),
        json!({"id": 4, "result": {"installed": false, "applicationScoped": false}})
    );
    assert_eq!(std::fs::read_to_string(&log).unwrap(), calls);

    let missing = tmp.path().join("dist/missing.vsix");
    agent.send(json!({"id": 5, "method": "setupServer", "params": {
        "vsix": missing.to_str().unwrap()}}));
    assert_eq!(
        agent.response()["result"]["skipped"],
        format!("no vsix at {}", missing.display())
    );

    agent.send(json!({"id": 6, "method": "setupServer", "params": {"vsix": "dist/refract.vsix"}}));
    assert_eq!(agent.response()["error"]["code"], -32602);

    // A registry that cannot be read is an error, not a skip.
    let registry = home.join(".vscode-server/extensions/extensions.json");
    std::fs::remove_file(&registry).unwrap();
    std::fs::create_dir(&registry).unwrap();
    agent.send(json!({"id": 7, "method": "setupServer", "params": {"vsix": vsix_str}}));
    let failed = agent.response();
    assert_eq!(failed["id"], 7);
    assert_eq!(failed["error"]["code"], 2);

    agent.send(json!({"id": 8, "method": "shutdown"}));
    assert_eq!(agent.response(), json!({"id": 8, "result": null}));
    assert!(agent.child.wait().unwrap().success());
}

#[test]
fn setup_server_requests_run_one_at_a_time() {
    let tmp = tempfile::tempdir().unwrap();
    let (home, _root, config) = setup(tmp.path());
    let vsix = tmp.path().join("refract.vsix");
    write_vsix(&vsix, "0.3.0");
    let log = fake_code_server(&home, "1a2b3c");
    let mut agent = Agent::spawn(&home, &config);
    let params = json!({"vsix": vsix.to_str().unwrap()});
    agent.send(json!({"id": 1, "method": "setupServer", "params": params}));
    agent.send(json!({"id": 2, "method": "setupServer", "params": params}));
    let first = agent.response();
    let second = agent.response();
    assert_eq!(first["id"], 1);
    assert_eq!(first["result"]["installed"], true);
    assert_eq!(second["id"], 2);
    assert_eq!(
        second["result"],
        json!({"installed": false, "applicationScoped": false})
    );
    assert_eq!(std::fs::read_to_string(&log).unwrap().lines().count(), 1);
}

#[test]
fn add_repo_reports_a_repo_outside_the_roots_and_remembers_it() {
    let tmp = tempfile::tempdir().unwrap();
    let (home, _root, config) = setup(tmp.path());
    let repo = tmp.path().join("work/aurora-notes");
    init_repo(&repo);
    let repo_path = std::fs::canonicalize(&repo).unwrap();
    let repo_str = repo_path.to_str().unwrap();
    let mut agent = Agent::spawn(&home, &config);
    agent.list_when_loaded(1);

    // A folder inside the tree adds the whole tree.
    agent.send(json!({"id": 2, "method": "addRepo", "params": {"path": repo.join("src")}}));
    let added = agent.event(|e| e["event"] == "repoChanged");
    assert_eq!(added["data"]["path"], repo_str);
    assert_eq!(agent.response(), json!({"id": 2, "result": null}));
    let text = std::fs::read_to_string(&config).unwrap();
    assert!(text.starts_with("roots = ["), "{text}");
    assert!(text.contains(&format!("extra = [{repo_str:?}]")), "{text}");

    // Adding it again reports it again and changes nothing.
    agent.send(json!({"id": 3, "method": "addRepo", "params": {"path": repo_str}}));
    let again = agent.event(|e| e["event"] == "repoChanged");
    assert_eq!(again["data"]["path"], repo_str);
    assert_eq!(agent.response(), json!({"id": 3, "result": null}));
    assert_eq!(std::fs::read_to_string(&config).unwrap(), text);

    let plain = tmp.path().join("work/notes");
    std::fs::create_dir_all(&plain).unwrap();
    agent.send(json!({"id": 4, "method": "addRepo", "params": {"path": plain}}));
    assert_eq!(agent.response()["error"]["code"], 1);
    agent.send(json!({"id": 5, "method": "addRepo", "params": {"path": "work/notes"}}));
    assert_eq!(agent.response()["error"]["code"], -32602);
    agent.send(json!({"id": 6, "method": "shutdown"}));
    assert_eq!(agent.response()["id"], 6);
    assert!(agent.child.wait().unwrap().success());

    // The next run finds it from the config.
    let mut agent = Agent::spawn(&home, &config);
    let list = agent.list_when_loaded(1);
    assert_eq!(list["result"]["repos"][0]["path"], repo_str);
}

#[test]
fn adding_or_cloning_a_hidden_repo_unhides_it() {
    let tmp = tempfile::tempdir().unwrap();
    let (home, root, config) = setup(tmp.path());
    let repo = root.join("aurora-notes");
    init_repo(&repo);
    let repo_path = std::fs::canonicalize(&repo).unwrap();
    let repo_str = repo_path.to_str().unwrap();
    let (bare, _) = remote(tmp.path(), "reef-maps");
    std::fs::create_dir_all(home.join("projects")).unwrap();
    let dest = std::fs::canonicalize(home.join("projects"))
        .unwrap()
        .join("reef-maps");
    let dest_str = dest.to_str().unwrap();
    let text = std::fs::read_to_string(&config).unwrap();
    write(
        &config,
        &format!("{text}# hidden on purpose\nhidden = [{repo_str:?}, {dest_str:?}]\n"),
    );
    let key = format!(
        "url.{}/.insteadOf",
        bare.parent().unwrap().to_str().unwrap()
    );
    let env: [(&str, &std::ffi::OsStr); 3] = [
        ("GIT_CONFIG_COUNT", "1".as_ref()),
        ("GIT_CONFIG_KEY_0", key.as_ref()),
        ("GIT_CONFIG_VALUE_0", "https://example.com/dev/".as_ref()),
    ];
    let mut agent = Agent::spawn_with(&home, &config, &env);
    let list = agent.list_when_loaded(1);
    assert_eq!(list["result"]["repos"], json!([]));

    agent.send(json!({"id": 2, "method": "addRepo", "params": {"path": repo_str}}));
    let added = agent.event(|e| e["event"] == "repoChanged");
    assert_eq!(added["data"]["path"], repo_str);
    assert_eq!(agent.response(), json!({"id": 2, "result": null}));
    let text = std::fs::read_to_string(&config).unwrap();
    assert!(text.contains("# hidden on purpose\n"), "{text}");
    assert!(!text.contains(&format!("{repo_str:?}, ")), "{text}");
    assert!(text.contains(&format!("{dest_str:?}")), "{text}");

    agent.send(json!({"id": 3, "method": "cloneRepo", "params": {
        "url": "https://example.com/dev/reef-maps.git", "name": "reef-maps"}}));
    let cloned = agent.event(|e| e["event"] == "repoChanged" && e["data"]["path"] == dest_str);
    assert_eq!(cloned["data"]["path"], dest_str);
    assert_eq!(agent.response(), json!({"id": 3, "result": dest_str}));
    let text = std::fs::read_to_string(&config).unwrap();
    assert!(!text.contains(&format!("hidden = [{dest_str:?}")), "{text}");
    agent.send(json!({"id": 4, "method": "shutdown"}));
    assert_eq!(agent.response()["id"], 4);
    assert!(agent.child.wait().unwrap().success());

    // The next run reports both.
    let mut agent = Agent::spawn(&home, &config);
    let list = agent.list_when_loaded(1);
    let paths: Vec<&str> = list["result"]["repos"]
        .as_array()
        .unwrap()
        .iter()
        .map(|r| r["path"].as_str().unwrap())
        .collect();
    assert!(
        paths.contains(&repo_str) && paths.contains(&dest_str),
        "{paths:?}"
    );
}

#[test]
fn clone_repo_clones_reports_and_reuses_the_clone() {
    let tmp = tempfile::tempdir().unwrap();
    let (home, _root, config) = setup(tmp.path());
    let (bare, _) = remote(tmp.path(), "reef-maps");
    let (other, _) = remote(tmp.path(), "kelp-cli");
    // The helper only clones https:// URLs; point this one at the local remotes.
    let key = format!(
        "url.{}/.insteadOf",
        bare.parent().unwrap().to_str().unwrap()
    );
    let env: [(&str, &std::ffi::OsStr); 3] = [
        ("GIT_CONFIG_COUNT", "1".as_ref()),
        ("GIT_CONFIG_KEY_0", key.as_ref()),
        ("GIT_CONFIG_VALUE_0", "https://example.com/dev/".as_ref()),
    ];
    let mut agent = Agent::spawn_with(&home, &config, &env);
    agent.list_when_loaded(1);

    let url = "https://example.com/dev/reef-maps.git";
    let dest = home.join("projects/reef-maps");
    let dest_str = dest.to_str().unwrap();
    agent
        .send(json!({"id": 2, "method": "cloneRepo", "params": {"url": url, "name": "reef-maps"}}));
    agent.send(json!({"id": 3, "method": "ping"}));
    assert_eq!(agent.response()["id"], 3);
    let added = agent.event(|e| e["event"] == "repoChanged");
    assert_eq!(added["data"]["path"], dest_str);
    assert_eq!(added["data"]["originUrl"], url);
    assert_eq!(agent.response(), json!({"id": 2, "result": dest_str}));
    assert!(dest.join("src/main.rs").is_file());
    let text = std::fs::read_to_string(&config).unwrap();
    assert!(text.contains(&format!("extra = [{dest_str:?}]")), "{text}");

    // The same repo again is the clone that is there.
    agent.send(json!({"id": 4, "method": "cloneRepo", "params": {
        "url": "https://EXAMPLE.com/dev/reef-maps", "name": "reef-maps"}}));
    agent.event(|e| e["event"] == "repoChanged" && e["data"]["path"] == dest_str);
    assert_eq!(agent.response(), json!({"id": 4, "result": dest_str}));

    // Another repo into the same folder is refused.
    assert!(other.is_dir());
    agent.send(json!({"id": 5, "method": "cloneRepo", "params": {
        "url": "https://example.com/dev/kelp-cli.git", "name": "reef-maps"}}));
    let refused = agent.response();
    assert_eq!(refused["error"]["code"], 3);
    assert!(refused["error"]["message"]
        .as_str()
        .unwrap()
        .ends_with("already exists"));

    // A failed clone is an error too.
    agent.send(json!({"id": 6, "method": "cloneRepo", "params": {
        "url": "https://example.com/dev/missing.git", "name": "missing"}}));
    assert_eq!(agent.response()["error"]["code"], 3);
    assert!(!home.join("projects/missing").exists());

    for params in [
        json!({"url": "ssh://git@example.com/dev/kelp-cli.git", "name": "kelp-cli"}),
        json!({"url": url, "name": "../reef-maps"}),
        json!({"url": url, "name": "--template=/tmp"}),
    ] {
        agent.send(json!({"id": 7, "method": "cloneRepo", "params": params}));
        assert_eq!(agent.response()["error"]["code"], -32602, "{params}");
    }
    agent.send(json!({"id": 8, "method": "shutdown"}));
    assert_eq!(agent.response()["id"], 8);
    assert!(agent.child.wait().unwrap().success());
}

#[test]
fn prints_its_version() {
    let out = Command::new(env!("CARGO_BIN_EXE_refract-agent"))
        .arg("--version")
        .stdin(Stdio::null())
        .output()
        .unwrap();
    assert!(out.status.success());
    assert_eq!(
        String::from_utf8(out.stdout).unwrap(),
        format!("refract-agent {}\n", env!("CARGO_PKG_VERSION"))
    );
    assert_eq!(
        format!("{}\n", refract_agent::VERSION_LINE),
        format!("refract-agent {}\n", env!("CARGO_PKG_VERSION"))
    );
}
