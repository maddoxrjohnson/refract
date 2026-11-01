//! Refactors end to end: the built binary over stdio, with temporary repos in a
//! temporary HOME, so nothing touches the real `~/projects` or `~/.local/state`. Agents
//! are faked with processes that look like `claude --session-id <id>`, transcripts and
//! status files.

mod common;

use std::io::{BufRead, BufReader, Write};
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStdin, Command, Stdio};
use std::sync::mpsc::{self, Receiver};
use std::time::{Duration, Instant};

use chrono::{TimeDelta, Utc};
use serde_json::{json, Value};

use common::*;

struct Agent {
    child: Child,
    stdin: ChildStdin,
    lines: Receiver<String>,
    next_id: u64,
}

impl Agent {
    fn spawn(env: &Env) -> Agent {
        Agent::spawn_with(env, &[])
    }

    /// [`Agent::spawn`] with more environment variables, such as shorter refactor times.
    fn spawn_with(env: &Env, vars: &[(&str, &str)]) -> Agent {
        let mut child = Command::new(env!("CARGO_BIN_EXE_refract-agent"))
            .env_remove("XDG_STATE_HOME")
            .env_remove("XDG_DATA_HOME")
            .env_remove("XDG_CONFIG_HOME")
            .env_remove("CLAUDE_CONFIG_DIR")
            .env("HOME", &env.home)
            .env("REFRACT_AGENT_CONFIG", &env.config)
            .env("WSL_DISTRO_NAME", "Ubuntu")
            .envs(GIT_ENV.iter().copied())
            .envs(vars.iter().copied())
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
        let mut agent = Agent {
            child,
            stdin,
            lines,
            next_id: 1,
        };
        agent.wait_loaded();
        agent
    }

    /// Sends a request and returns its response, skipping events.
    #[track_caller]
    fn request(&mut self, method: &str, params: Value) -> Value {
        let id = self.next_id;
        self.next_id += 1;
        let message = if params.is_null() {
            json!({"id": id, "method": method})
        } else {
            json!({"id": id, "method": method, "params": params})
        };
        writeln!(self.stdin, "{message}").unwrap();
        self.stdin.flush().unwrap();
        let deadline = Instant::now() + Duration::from_secs(30);
        loop {
            let left = deadline.saturating_duration_since(Instant::now());
            let line = self
                .lines
                .recv_timeout(left)
                .expect("agent answered nothing in time");
            let message: Value = serde_json::from_str(&line).unwrap();
            if message.get("id") == Some(&json!(id)) {
                return message;
            }
        }
    }

    fn wait_loaded(&mut self) {
        let deadline = Instant::now() + Duration::from_secs(20);
        loop {
            let list = self.request("listRepos", Value::Null);
            if list["result"]["loading"] == false {
                return;
            }
            assert!(Instant::now() < deadline, "load never completed");
            std::thread::sleep(Duration::from_millis(100));
        }
    }

    fn repo(&mut self, repo: &Path) -> Value {
        let reply = self.request("getRepo", json!({"path": repo}));
        reply["result"].clone()
    }

    /// Asks for the repo until its refactor matches `pred`, for at most 15 s.
    #[track_caller]
    fn wait_refactor(&mut self, repo: &Path, what: &str, pred: impl Fn(&Value) -> bool) -> Value {
        let deadline = Instant::now() + Duration::from_secs(15);
        loop {
            let refactor = self.repo(repo)["refactor"].clone();
            if pred(&refactor) {
                return refactor;
            }
            assert!(
                Instant::now() < deadline,
                "refactor never got {what}: {refactor}"
            );
            std::thread::sleep(Duration::from_millis(200));
        }
    }

    #[track_caller]
    fn wait_state(&mut self, repo: &Path, state: &str) -> Value {
        self.wait_refactor(repo, state, |r| r["state"] == state)
    }

    fn shutdown(mut self) {
        self.request("shutdown", Value::Null);
        assert!(self.child.wait().unwrap().success());
    }
}

/// A temporary HOME with `~/projects` as the only root.
struct Env {
    _tmp: tempfile::TempDir,
    tmp: PathBuf,
    home: PathBuf,
    root: PathBuf,
    config: PathBuf,
    /// `~/.local/state/refract`.
    state: PathBuf,
    /// `~/.claude/projects`.
    projects: PathBuf,
}

impl Env {
    fn new() -> Env {
        hermetic_git();
        let dir = tempfile::tempdir().unwrap();
        let tmp = dir.path().canonicalize().unwrap();
        let home = tmp.join("home");
        let root = home.join("projects");
        std::fs::create_dir_all(&root).unwrap();
        let config = tmp.join("agent.toml");
        write(
            &config,
            &format!("roots = [{:?}]\ndepth = 3\n", root.to_str().unwrap()),
        );
        let projects = home.join(".claude/projects");
        std::fs::create_dir_all(&projects).unwrap();
        Env {
            _tmp: dir,
            state: home.join(".local/state/refract"),
            tmp,
            home,
            root,
            config,
            projects,
        }
    }

    /// A repo `name` on `main` with one commit.
    fn repo(&self, name: &str) -> PathBuf {
        let repo = self.root.join(name);
        init_repo(&repo);
        repo
    }

    fn worktrees(&self) -> PathBuf {
        self.home.join("projects/.refract-worktrees")
    }

    fn records(&self) -> Vec<Value> {
        let Ok(entries) = std::fs::read_dir(self.state.join("refactors")) else {
            return Vec::new();
        };
        entries
            .flatten()
            .filter(|e| e.path().extension().is_some_and(|x| x == "json"))
            .map(|e| serde_json::from_slice(&std::fs::read(e.path()).unwrap()).unwrap())
            .collect()
    }

    fn launch_request(&self, ticket: &Value) -> Value {
        let id = ticket["requestId"].as_str().unwrap();
        let file = self.state.join(format!("launch/{id}.json"));
        serde_json::from_slice(&std::fs::read(file).unwrap()).unwrap()
    }

    /// A finished (or unfinished) transcript of `id` in `cwd`.
    fn transcript(&self, id: &str, cwd: &Path, finished: bool) {
        write(
            &project_dir(&self.projects, cwd).join(format!("{id}.jsonl")),
            &transcript(
                id,
                cwd,
                &cwd.join("src/main.rs"),
                Utc::now() - TimeDelta::seconds(30),
                finished,
            ),
        );
    }
}

/// A process that looks like `claude --session-id <id>` running in `cwd`, until killed.
struct FakeClaude(Child);

impl FakeClaude {
    fn start(cwd: &Path, id: &str) -> FakeClaude {
        let child = Command::new("bash")
            .arg0("claude")
            .args(["-c", "read -r _", "claude", "--session-id", id])
            .current_dir(cwd)
            .stdin(Stdio::piped())
            .stdout(Stdio::null())
            .spawn()
            .expect("spawn fake claude");
        FakeClaude(child)
    }
}

impl Drop for FakeClaude {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

/// Starts a refactor of `repo` and returns the ticket and the refactor's snapshot part.
#[track_caller]
fn start(agent: &mut Agent, repo: &Path, focus: Option<&str>) -> (Value, Value) {
    let reply = agent.request("startRefactor", json!({"path": repo, "focus": focus}));
    let ticket = reply["result"].clone();
    assert!(ticket["requestId"].is_string(), "{reply}");
    let refactor = agent.repo(repo)["refactor"].clone();
    assert!(refactor.is_object(), "no refactor in the snapshot");
    (ticket, refactor)
}

fn path(value: &Value) -> PathBuf {
    PathBuf::from(value.as_str().unwrap())
}

fn rev(dir: &Path, spec: &str) -> String {
    git(dir, &["rev-parse", spec])
}

fn branch_exists(repo: &Path, branch: &str) -> bool {
    Command::new("git")
        .arg("-C")
        .arg(repo)
        .args([
            "rev-parse",
            "--verify",
            "-q",
            &format!("refs/heads/{branch}"),
        ])
        .envs(GIT_ENV.iter().copied())
        .stdout(Stdio::null())
        .status()
        .unwrap()
        .success()
}

#[test]
fn start_makes_a_worktree_branch_record_and_plan_mode_launch() {
    let env = Env::new();
    let repo = env.repo("tidepool-api");
    let head = rev(&repo, "HEAD");
    let mut agent = Agent::spawn(&env);
    let snapshot = agent.repo(&repo);
    assert_eq!(snapshot["refactor"], Value::Null);
    let short = snapshot["lastCommit"]["sha"].as_str().unwrap();
    assert!(head.starts_with(short) && short.len() >= 7);

    let (ticket, refactor) = start(&mut agent, &repo, Some("the parser\nand its tests"));
    let worktree = path(&refactor["worktree"]);
    let branch = refactor["branch"].as_str().unwrap().to_owned();
    assert!(
        worktree.starts_with(env.worktrees()),
        "{}",
        worktree.display()
    );
    let name = worktree.file_name().unwrap().to_str().unwrap();
    let stamp = branch.strip_prefix("refactor/").unwrap();
    assert_eq!(name, format!("tidepool-api-{stamp}"));
    assert_eq!(stamp.len(), "20261031-1405".len());
    assert_eq!(rev(&worktree, "HEAD"), head);
    assert_eq!(git(&worktree, &["branch", "--show-current"]), branch);
    assert_eq!(refactor["targetBranch"], "main");
    assert_eq!(refactor["baseCommit"], head);
    assert_eq!(refactor["sessionId"], ticket["sessionId"]);
    assert_eq!(refactor["state"], "starting");
    assert_eq!(refactor["live"], false);
    assert_eq!(refactor["commits"], json!([]));
    assert_eq!(refactor["checks"], Value::Null);
    let uri = format!("vscode-remote://wsl+Ubuntu{}", worktree.display());
    assert_eq!(ticket["folderUri"], uri);
    assert_eq!(refactor["folderUri"], ticket["folderUri"]);
    // The user's working copy is as it was.
    assert_eq!(git(&repo, &["branch", "--show-current"]), "main");
    assert_eq!(git(&repo, &["status", "--porcelain"]), "");

    let request = env.launch_request(&ticket);
    assert_eq!(request["folder"], worktree.to_str().unwrap());
    assert_eq!(request["title"], "Refactor");
    let argv: Vec<&str> = request["argv"]
        .as_array()
        .unwrap()
        .iter()
        .map(|a| a.as_str().unwrap())
        .collect();
    let session = ticket["sessionId"].as_str().unwrap();
    assert_eq!(
        argv[..6],
        [
            "claude",
            "--session-id",
            session,
            "--permission-mode",
            "plan",
            "--"
        ]
    );
    let prompt = argv[6];
    let git_dir = git(&worktree, &["rev-parse", "--absolute-git-dir"]);
    assert!(prompt.contains(&format!("`{git_dir}/refract-refactor.json`")));
    assert!(prompt.contains(&format!("branch `{branch}`")));
    assert!(prompt.ends_with("Focus on: the parser and its tests"));

    let records = env.records();
    assert_eq!(records.len(), 1);
    let record = &records[0];
    assert_eq!(record["repo"], repo.to_str().unwrap());
    assert_eq!(record["worktree"], worktree.to_str().unwrap());
    assert_eq!(record["branch"], branch);
    assert_eq!(record["baseCommit"], head);
    assert_eq!(record["targetBranch"], "main");
    assert_eq!(record["sessionId"], session);
    assert_eq!(record["focus"], "the parser and its tests");

    // One refactor per repo.
    let again = agent.request("startRefactor", json!({"path": repo}));
    assert_eq!(again["error"]["code"], 8, "{again}");
    assert_eq!(env.records().len(), 1);

    // The worktree is no stone of its own.
    let list = agent.request("listRepos", Value::Null);
    assert_eq!(list["result"]["repos"].as_array().unwrap().len(), 1);
    agent.shutdown();
}

#[test]
fn a_busy_repo_can_be_refactored_from_its_last_commit() {
    let env = Env::new();
    let repo = env.repo("kelp-cli");
    write(&repo.join("src/main.rs"), "fn main() { todo!() }\n");
    write(&repo.join("notes.txt"), "draft\n");
    let _running = FakeClaude::start(&repo, "5e2a7c10-0000-4000-8000-0000000000a1");
    let mut agent = Agent::spawn(&env);
    let (_, refactor) = start(&mut agent, &repo, None);
    let worktree = path(&refactor["worktree"]);
    assert_eq!(
        std::fs::read_to_string(worktree.join("src/main.rs")).unwrap(),
        "fn main() {}\n"
    );
    assert!(!worktree.join("notes.txt").exists());
    assert_eq!(
        std::fs::read_to_string(repo.join("src/main.rs")).unwrap(),
        "fn main() { todo!() }\n"
    );
    assert_eq!(agent.repo(&repo)["changedFiles"], 2);
    agent.shutdown();
}

#[test]
fn a_detached_head_or_a_repo_without_commits_is_refused() {
    let env = Env::new();
    let detached = env.repo("reef-maps");
    git(&detached, &["checkout", "-q", "--detach"]);
    let empty = env.root.join("aurora-notes");
    std::fs::create_dir_all(&empty).unwrap();
    git(&empty, &["init", "-q", "-b", "main"]);
    let mut agent = Agent::spawn(&env);

    let refused = agent.request("startRefactor", json!({"path": detached}));
    assert_eq!(refused["error"]["code"], 8);
    assert_eq!(refused["error"]["message"], "Check out a branch first.");
    let refused = agent.request("startRefactor", json!({"path": empty}));
    assert_eq!(refused["error"]["code"], 8);
    let unknown = agent.request("startRefactor", json!({"path": env.root.join("nope")}));
    assert_eq!(unknown["error"]["code"], 1);
    let none = agent.request("mergeRefactor", json!({"path": detached}));
    assert_eq!(none["error"]["code"], 8);
    assert!(env.records().is_empty());
    assert!(!env.worktrees().exists() || std::fs::read_dir(env.worktrees()).unwrap().count() == 0);
    agent.shutdown();
}

#[test]
fn states_follow_the_agent_its_hook_and_its_commits() {
    let env = Env::new();
    let repo = env.repo("tidepool-api");
    let mut agent = Agent::spawn(&env);
    let installed = agent.request("installHook", Value::Null);
    assert_eq!(installed["result"]["installed"], true, "{installed}");
    let (ticket, refactor) = start(&mut agent, &repo, None);
    let session = ticket["sessionId"].as_str().unwrap().to_owned();
    let worktree = path(&refactor["worktree"]);
    assert_eq!(refactor["state"], "starting");
    let status = env.state.join(format!("sessions/{session}.json"));
    let hook = |status_name: &str, event: &str| {
        write(
            &status,
            &json!({"status": status_name, "event": event, "at": Utc::now().timestamp()})
                .to_string(),
        );
    };

    // The agent starts: working, before and after it writes a transcript.
    let claude = FakeClaude::start(&worktree, &session);
    let working = agent.wait_state(&repo, "working");
    assert_eq!(working["live"], true);
    env.transcript(&session, &worktree, false);
    // Plan approval waits on the user.
    hook("waitingOnUser", "PreToolUse");
    agent.wait_state(&repo, "needsYou");
    hook("working", "PostToolUse");
    agent.wait_state(&repo, "working");

    // It stops mid-turn with nothing committed: interrupted.
    drop(claude);
    let interrupted = agent.wait_state(&repo, "interrupted");
    assert_eq!(interrupted["interrupted"], true);
    assert_eq!(interrupted["live"], false);

    // Resumed, it commits and reports while it works.
    let resumed = agent.request("resumeRefactor", json!({"path": repo}));
    assert_eq!(resumed["result"]["sessionId"], session, "{resumed}");
    let claude = FakeClaude::start(&worktree, &session);
    agent.wait_state(&repo, "working");
    commit_file(
        &worktree,
        "src/lib.rs",
        "pub fn tide() {}\n",
        "Split out tide",
    );
    let result = PathBuf::from(git(&worktree, &["rev-parse", "--absolute-git-dir"]))
        .join("refract-refactor.json");
    write(
        &result,
        r#"{"checks":[{"command":"cargo test","before":"pass","after":"pass"}],"refactors":[{"title":"Split out tide","commit":"x"}],"summary":"Split out tide."}"#,
    );
    let busy = agent.wait_refactor(&repo, "checks", |r| r["checks"].is_array());
    assert_eq!(busy["state"], "working");
    assert_eq!(busy["checks"][0]["command"], "cargo test");
    assert_eq!(busy["summary"], "Split out tide.");
    // Its turn over, still open: ready, with the commit and the diffstat.
    hook("finished", "Stop");
    let ready = agent.wait_state(&repo, "ready");
    assert_eq!(ready["live"], true);
    assert_eq!(ready["commits"].as_array().unwrap().len(), 1);
    assert_eq!(ready["commits"][0]["summary"], "Split out tide");
    assert_eq!(
        ready["commits"][0]["sha"],
        git(&worktree, &["rev-parse", "--short", "HEAD"])
    );
    assert_eq!(ready["filesChanged"], 1);
    assert_eq!(ready["insertions"], 1);
    assert_eq!(ready["deletions"], 0);
    // A malformed result file is no checks.
    write(
        &result,
        r#"{"checks":[{"command":"x","before":"maybe","after":"pass"}],"refactors":[]}"#,
    );
    agent.wait_refactor(&repo, "no checks", |r| r["checks"].is_null());
    drop(claude);
    let closed = agent.wait_refactor(&repo, "closed", |r| r["live"] == false);
    assert_eq!(closed["state"], "ready");
    agent.shutdown();
}

#[test]
fn a_finished_agent_with_nothing_to_refactor_is_nothing_to_do() {
    let env = Env::new();
    let repo = env.repo("kelp-cli");
    let mut agent = Agent::spawn(&env);
    let (ticket, refactor) = start(&mut agent, &repo, None);
    let session = ticket["sessionId"].as_str().unwrap();
    let worktree = path(&refactor["worktree"]);
    env.transcript(session, &worktree, true);
    // Finished without commits or a result file: interrupted, but not mid-turn.
    let stopped = agent.wait_state(&repo, "interrupted");
    assert_eq!(stopped["interrupted"], false);
    let result = PathBuf::from(git(&worktree, &["rev-parse", "--absolute-git-dir"]))
        .join("refract-refactor.json");
    write(
        &result,
        r#"{"checks":[],"refactors":[],"summary":"Nothing worth it."}"#,
    );
    let done = agent.wait_state(&repo, "nothingToDo");
    assert_eq!(done["summary"], "Nothing worth it.");
    agent.shutdown();
}

#[test]
fn worktree_sessions_attach_to_no_stone() {
    let env = Env::new();
    let repo = env.repo("tidepool-api");
    let other = env.repo("kelp-cli");
    let mut agent = Agent::spawn(&env);
    let (ticket, refactor) = start(&mut agent, &repo, None);
    let session = ticket["sessionId"].as_str().unwrap();
    let worktree = path(&refactor["worktree"]);
    env.transcript(session, &worktree, false);
    let _claude = FakeClaude::start(&worktree, session);
    agent.wait_state(&repo, "working");
    let list = agent.request("listRepos", Value::Null);
    let repos = list["result"]["repos"].as_array().unwrap();
    assert_eq!(repos.len(), 2);
    for snapshot in repos {
        assert_eq!(snapshot["sessions"], json!([]), "{}", snapshot["name"]);
        assert_ne!(snapshot["path"], worktree.to_str().unwrap());
    }
    assert_eq!(agent.repo(&other)["refactor"], Value::Null);
    agent.shutdown();
}

#[test]
fn a_live_agent_blocks_merge_discard_and_resume() {
    let env = Env::new();
    let repo = env.repo("tidepool-api");
    let mut agent = Agent::spawn(&env);
    let (ticket, refactor) = start(&mut agent, &repo, None);
    let session = ticket["sessionId"].as_str().unwrap();
    let worktree = path(&refactor["worktree"]);
    commit_file(
        &worktree,
        "src/lib.rs",
        "pub fn tide() {}\n",
        "Split out tide",
    );
    let _claude = FakeClaude::start(&worktree, session);
    agent.wait_state(&repo, "working");
    let merge = agent.request("mergeRefactor", json!({"path": repo}));
    assert_eq!(
        merge["result"],
        json!({"outcome": "blocked", "reason": "live", "files": []})
    );
    let discard = agent.request("discardRefactor", json!({"path": repo}));
    assert_eq!(discard["error"]["code"], 8);
    let resume = agent.request("resumeRefactor", json!({"path": repo}));
    assert_eq!(resume["error"]["code"], 8);
    assert!(worktree.is_dir());
    assert!(branch_exists(&repo, refactor["branch"].as_str().unwrap()));
    agent.shutdown();
}

/// Starts a refactor and commits `file` in its worktree; returns the refactor.
fn ready(agent: &mut Agent, repo: &Path, file: &str, text: &str) -> Value {
    let (_, refactor) = start(agent, repo, None);
    let worktree = path(&refactor["worktree"]);
    commit_file(&worktree, file, text, "Tidy the entry point");
    agent.wait_state(repo, "ready")
}

#[test]
fn merge_fast_forwards_the_checked_out_target_and_cleans_up() {
    let env = Env::new();
    let repo = env.repo("tidepool-api");
    let mut agent = Agent::spawn(&env);
    let refactor = ready(&mut agent, &repo, "src/main.rs", "fn main() { tidy() }\n");
    let worktree = path(&refactor["worktree"]);
    let branch = refactor["branch"].as_str().unwrap().to_owned();
    let head = rev(&worktree, "HEAD");
    let merged = agent.request("mergeRefactor", json!({"path": repo}));
    assert_eq!(
        merged["result"],
        json!({"outcome": "merged", "commits": 1}),
        "{merged}"
    );
    assert_eq!(rev(&repo, "HEAD"), head);
    assert_eq!(git(&repo, &["branch", "--show-current"]), "main");
    assert_eq!(
        std::fs::read_to_string(repo.join("src/main.rs")).unwrap(),
        "fn main() { tidy() }\n"
    );
    assert_eq!(git(&repo, &["status", "--porcelain"]), "");
    assert!(!worktree.exists());
    assert!(!branch_exists(&repo, &branch));
    assert!(env.records().is_empty());
    assert_eq!(agent.repo(&repo)["refactor"], Value::Null);
    agent.shutdown();
}

#[test]
fn merge_rebases_onto_a_target_that_moved_on() {
    let env = Env::new();
    let repo = env.repo("tidepool-api");
    let mut agent = Agent::spawn(&env);
    let refactor = ready(&mut agent, &repo, "src/main.rs", "fn main() { tidy() }\n");
    let worktree = path(&refactor["worktree"]);
    commit_file(&repo, "docs/tides.md", "# Tides\n", "Document tides");
    let moved = rev(&repo, "HEAD");
    let merged = agent.request("mergeRefactor", json!({"path": repo}));
    assert_eq!(merged["result"]["outcome"], "merged", "{merged}");
    assert_eq!(merged["result"]["commits"], 1);
    assert_eq!(rev(&repo, "HEAD~1"), moved);
    assert_eq!(
        git(&repo, &["log", "-1", "--format=%s"]),
        "Tidy the entry point"
    );
    assert!(repo.join("docs/tides.md").is_file());
    assert!(!worktree.exists());
    agent.shutdown();
}

#[test]
fn a_rebase_conflict_is_undone_and_blocks_until_fixed() {
    let env = Env::new();
    let repo = env.repo("tidepool-api");
    let mut agent = Agent::spawn(&env);
    let refactor = ready(&mut agent, &repo, "README.md", "# demo, tidied\n");
    let worktree = path(&refactor["worktree"]);
    let session = refactor["sessionId"].as_str().unwrap().to_owned();
    let head = rev(&worktree, "HEAD");
    commit_file(&repo, "README.md", "# demo, edited\n", "Edit the readme");
    let main = rev(&repo, "HEAD");

    let blocked = agent.request("mergeRefactor", json!({"path": repo}));
    assert_eq!(
        blocked["result"],
        json!({"outcome": "blocked", "reason": "conflict", "files": ["README.md"]})
    );
    // The worktree is as it was, with no rebase under way; the repo too.
    assert_eq!(rev(&worktree, "HEAD"), head);
    assert_eq!(git(&worktree, &["status", "--porcelain"]), "");
    let git_dir = PathBuf::from(git(&worktree, &["rev-parse", "--absolute-git-dir"]));
    assert!(!git_dir.join("rebase-merge").exists() && !git_dir.join("rebase-apply").exists());
    assert_eq!(rev(&repo, "HEAD"), main);
    assert_eq!(
        std::fs::read_to_string(repo.join("README.md")).unwrap(),
        "# demo, edited\n"
    );
    let state = agent.wait_state(&repo, "mergeBlocked");
    assert_eq!(
        state["blocked"],
        json!({"reason": "conflict", "files": ["README.md"]})
    );
    assert_eq!(env.records()[0]["blocked"]["reason"], "conflict");

    // Fix with Claude resumes the session with the rebase prompt and clears the block.
    env.transcript(&session, &worktree, true);
    let fix = agent.request("resumeRefactor", json!({"path": repo, "fix": true}));
    let ticket = &fix["result"];
    assert_eq!(ticket["sessionId"], session, "{fix}");
    let request = env.launch_request(ticket);
    assert_eq!(request["title"], "Refactor");
    let argv = request["argv"].as_array().unwrap();
    assert_eq!(
        argv[..4],
        [
            json!("claude"),
            json!("--resume"),
            json!(session),
            json!("--")
        ]
    );
    let prompt = request["argv"][4].as_str().unwrap();
    assert!(prompt.contains("`git rebase main`"), "{prompt}");
    let state = agent.wait_state(&repo, "ready");
    assert_eq!(state["blocked"], Value::Null);
    assert_eq!(env.records()[0]["blocked"], Value::Null);

    // A plain resume continues where it stopped.
    let resume = agent.request("resumeRefactor", json!({"path": repo}));
    let request = env.launch_request(&resume["result"]);
    assert_eq!(
        request["argv"],
        json!([
            "claude",
            "--resume",
            session,
            "--",
            "Continue the refactor from where it stopped."
        ])
    );
    agent.shutdown();
}

#[test]
fn an_uncommitted_file_the_refactor_changed_blocks_the_merge() {
    let env = Env::new();
    let repo = env.repo("tidepool-api");
    let mut agent = Agent::spawn(&env);
    let refactor = ready(&mut agent, &repo, "src/main.rs", "fn main() { tidy() }\n");
    let worktree = path(&refactor["worktree"]);
    write(&repo.join("src/main.rs"), "fn main() { mine() }\n");
    let main = rev(&repo, "HEAD");
    let blocked = agent.request("mergeRefactor", json!({"path": repo}));
    assert_eq!(
        blocked["result"],
        json!({"outcome": "blocked", "reason": "dirtyOverlap", "files": ["src/main.rs"]})
    );
    assert_eq!(rev(&repo, "HEAD"), main);
    assert_eq!(
        std::fs::read_to_string(repo.join("src/main.rs")).unwrap(),
        "fn main() { mine() }\n"
    );
    assert!(worktree.is_dir());
    agent.wait_state(&repo, "mergeBlocked");

    // Once the user's change is gone, merging again works.
    git(&repo, &["checkout", "--", "src/main.rs"]);
    let merged = agent.request("mergeRefactor", json!({"path": repo}));
    assert_eq!(merged["result"]["outcome"], "merged", "{merged}");
    agent.shutdown();
}

#[test]
fn an_uncommitted_file_elsewhere_does_not_block_the_merge() {
    let env = Env::new();
    let repo = env.repo("tidepool-api");
    let mut agent = Agent::spawn(&env);
    let refactor = ready(&mut agent, &repo, "src/main.rs", "fn main() { tidy() }\n");
    let head = rev(&path(&refactor["worktree"]), "HEAD");
    write(&repo.join("README.md"), "# demo, mine\n");
    write(&repo.join("notes.txt"), "draft\n");
    let merged = agent.request("mergeRefactor", json!({"path": repo}));
    assert_eq!(merged["result"]["outcome"], "merged", "{merged}");
    assert_eq!(rev(&repo, "HEAD"), head);
    assert_eq!(
        std::fs::read_to_string(repo.join("README.md")).unwrap(),
        "# demo, mine\n"
    );
    assert_eq!(
        std::fs::read_to_string(repo.join("notes.txt")).unwrap(),
        "draft\n"
    );
    assert_eq!(
        std::fs::read_to_string(repo.join("src/main.rs")).unwrap(),
        "fn main() { tidy() }\n"
    );
    agent.shutdown();
}

#[test]
fn a_target_not_checked_out_moves_without_touching_the_working_copy() {
    let env = Env::new();
    let repo = env.repo("tidepool-api");
    let mut agent = Agent::spawn(&env);
    let refactor = ready(&mut agent, &repo, "src/main.rs", "fn main() { tidy() }\n");
    let branch = refactor["branch"].as_str().unwrap().to_owned();
    let head = rev(&path(&refactor["worktree"]), "HEAD");
    git(&repo, &["checkout", "-q", "-b", "spike"]);
    write(&repo.join("src/main.rs"), "fn main() { spike() }\n");
    let merged = agent.request("mergeRefactor", json!({"path": repo}));
    assert_eq!(
        merged["result"],
        json!({"outcome": "merged", "commits": 1}),
        "{merged}"
    );
    assert_eq!(rev(&repo, "main"), head);
    assert_eq!(git(&repo, &["branch", "--show-current"]), "spike");
    assert_eq!(
        std::fs::read_to_string(repo.join("src/main.rs")).unwrap(),
        "fn main() { spike() }\n"
    );
    assert!(!branch_exists(&repo, &branch));
    agent.shutdown();
}

#[test]
fn a_missing_target_blocks_the_merge() {
    let env = Env::new();
    let repo = env.repo("tidepool-api");
    let mut agent = Agent::spawn(&env);
    ready(&mut agent, &repo, "src/main.rs", "fn main() { tidy() }\n");
    git(&repo, &["checkout", "-q", "-b", "trunk"]);
    git(&repo, &["branch", "-q", "-D", "main"]);
    let blocked = agent.request("mergeRefactor", json!({"path": repo}));
    assert_eq!(
        blocked["result"],
        json!({"outcome": "blocked", "reason": "targetMissing", "files": []})
    );
    agent.shutdown();
}

#[test]
fn discard_deletes_the_worktree_and_branch() {
    let env = Env::new();
    let repo = env.repo("tidepool-api");
    let mut agent = Agent::spawn(&env);
    let refactor = ready(&mut agent, &repo, "src/main.rs", "fn main() { tidy() }\n");
    let worktree = path(&refactor["worktree"]);
    let branch = refactor["branch"].as_str().unwrap().to_owned();
    // Leftovers of the agent's setup go too.
    write(&worktree.join("build/out.txt"), "built\n");
    let main = rev(&repo, "HEAD");
    let discarded = agent.request("discardRefactor", json!({"path": repo}));
    assert_eq!(discarded["result"], Value::Null, "{discarded}");
    assert!(!worktree.exists());
    assert!(!branch_exists(&repo, &branch));
    assert!(env.records().is_empty());
    assert_eq!(rev(&repo, "HEAD"), main);
    assert_eq!(agent.repo(&repo)["refactor"], Value::Null);
    let again = agent.request("discardRefactor", json!({"path": repo}));
    assert_eq!(again["error"]["code"], 8);
    agent.shutdown();
}

#[test]
fn records_survive_a_restart() {
    let env = Env::new();
    let repo = env.repo("tidepool-api");
    let mut agent = Agent::spawn(&env);
    let before = ready(&mut agent, &repo, "src/main.rs", "fn main() { tidy() }\n");
    agent.shutdown();
    let mut agent = Agent::spawn(&env);
    let after = agent.wait_state(&repo, "ready");
    for key in [
        "worktree",
        "branch",
        "targetBranch",
        "baseCommit",
        "sessionId",
        "createdAt",
    ] {
        assert_eq!(after[key], before[key], "{key}");
    }
    assert_eq!(env.records().len(), 1);
    agent.shutdown();
}

#[test]
fn reconcile_drops_records_without_worktrees_and_adopts_worktrees_without_records() {
    let env = Env::new();
    let repo = env.repo("tidepool-api");
    let other = env.repo("kelp-cli");
    let mut agent = Agent::spawn(&env);
    let refactor = ready(&mut agent, &repo, "src/main.rs", "fn main() { tidy() }\n");
    agent.shutdown();

    // The worktree removed by hand: the record goes, the branch stays.
    let worktree = path(&refactor["worktree"]);
    let branch = refactor["branch"].as_str().unwrap().to_owned();
    git(
        &repo,
        &["worktree", "remove", "--force", worktree.to_str().unwrap()],
    );
    // A refactor worktree of another repo with no record.
    let adopted = env.worktrees().join("kelp-cli-20200101-0000");
    git(
        &other,
        &[
            "worktree",
            "add",
            "-q",
            "-b",
            "refactor/20200101-0000",
            adopted.to_str().unwrap(),
        ],
    );
    commit_file(
        &adopted,
        "src/lib.rs",
        "pub fn kelp() {}\n",
        "Split out kelp",
    );
    // A worktree elsewhere, or on another branch, is not adopted.
    git(
        &repo,
        &[
            "worktree",
            "add",
            "-q",
            "-b",
            "spike",
            env.worktrees().join("spike").to_str().unwrap(),
        ],
    );

    let mut agent = Agent::spawn(&env);
    assert_eq!(agent.repo(&repo)["refactor"], Value::Null);
    assert!(branch_exists(&repo, &branch));
    let found = agent.wait_state(&other, "ready");
    assert_eq!(found["branch"], "refactor/20200101-0000");
    assert_eq!(found["targetBranch"], "main");
    assert_eq!(found["baseCommit"], rev(&other, "main"));
    assert_eq!(found["sessionId"], "");
    assert_eq!(found["worktree"], adopted.to_str().unwrap());
    let records = env.records();
    assert_eq!(records.len(), 1);
    assert_eq!(records[0]["repo"], other.to_str().unwrap());
    agent.shutdown();
}

#[test]
fn a_record_pointing_outside_the_worktrees_folder_is_never_deleted() {
    let env = Env::new();
    let repo = env.repo("tidepool-api");
    let elsewhere = env.tmp.join("elsewhere");
    git(
        &repo,
        &[
            "worktree",
            "add",
            "-q",
            "-b",
            "refactor/20200101-0000",
            elsewhere.to_str().unwrap(),
        ],
    );
    commit_file(
        &elsewhere,
        "src/lib.rs",
        "pub fn tide() {}\n",
        "Precious work",
    );
    // A symlink inside the folder that leads out of it is no better.
    std::fs::create_dir_all(env.worktrees()).unwrap();
    let link = env.worktrees().join("tidepool-api-20200101-0000");
    std::os::unix::fs::symlink(&elsewhere, &link).unwrap();
    let record = json!({
        "repo": repo,
        "worktree": link,
        "branch": "refactor/20200101-0000",
        "baseCommit": rev(&repo, "main"),
        "targetBranch": "main",
        "sessionId": "",
        "createdAt": "2026-10-31T14:05:00Z",
    });
    write(
        &env.state
            .join("refactors/0b7e0000-0000-4000-8000-000000000009.json"),
        &record.to_string(),
    );
    let head = rev(&elsewhere, "HEAD");
    let mut agent = Agent::spawn(&env);
    let refactor = agent.wait_state(&repo, "ready");
    assert_eq!(refactor["worktree"], link.to_str().unwrap());

    let discard = agent.request("discardRefactor", json!({"path": repo}));
    assert_eq!(discard["error"]["code"], 8, "{discard}");
    let merge = agent.request("mergeRefactor", json!({"path": repo}));
    assert_eq!(merge["error"]["code"], 8, "{merge}");
    assert!(elsewhere.join("src/lib.rs").is_file());
    assert_eq!(rev(&elsewhere, "HEAD"), head);
    assert!(branch_exists(&repo, "refactor/20200101-0000"));
    assert_eq!(rev(&repo, "main"), record["baseCommit"].as_str().unwrap());
    assert_eq!(env.records().len(), 1);
    agent.shutdown();
}

/// Runs git in `dir`, which may fail; returns whether it succeeded.
fn try_git(dir: &Path, args: &[&str]) -> bool {
    Command::new("git")
        .arg("-C")
        .arg(dir)
        .args(args)
        .envs(GIT_ENV.iter().copied())
        .env("GIT_EDITOR", "true")
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .unwrap()
        .success()
}

#[track_caller]
fn assert_blocked(reply: &Value, reason: &str, files: Value) {
    assert_eq!(
        reply["result"],
        json!({"outcome": "blocked", "reason": reason, "files": files}),
        "{reply}"
    );
}

#[test]
fn an_untracked_file_or_a_staged_rename_the_refactor_touches_blocks_the_merge() {
    let env = Env::new();
    let repo = env.repo("tidepool-api");
    let mut agent = Agent::spawn(&env);
    let (_, refactor) = start(&mut agent, &repo, None);
    let worktree = path(&refactor["worktree"]);
    commit_file(
        &worktree,
        "src/tide.rs",
        "pub fn tide() {}\n",
        "Split out tide",
    );
    commit_file(
        &worktree,
        "README.md",
        "# demo, tidied\n",
        "Tidy the readme",
    );
    agent.wait_state(&repo, "ready");

    // An untracked file where the refactor adds one.
    write(&repo.join("src/tide.rs"), "mine\n");
    let blocked = agent.request("mergeRefactor", json!({"path": repo}));
    assert_blocked(&blocked, "dirtyOverlap", json!(["src/tide.rs"]));
    assert_eq!(
        std::fs::read_to_string(repo.join("src/tide.rs")).unwrap(),
        "mine\n"
    );
    std::fs::remove_file(repo.join("src/tide.rs")).unwrap();

    // A staged rename away from a file the refactor changed.
    git(&repo, &["mv", "README.md", "DOCS.md"]);
    let blocked = agent.request("mergeRefactor", json!({"path": repo}));
    assert_blocked(&blocked, "dirtyOverlap", json!(["README.md"]));
    assert!(repo.join("DOCS.md").is_file());
    git(&repo, &["mv", "DOCS.md", "README.md"]);

    let merged = agent.request("mergeRefactor", json!({"path": repo}));
    assert_eq!(merged["result"]["outcome"], "merged", "{merged}");
    agent.shutdown();
}

#[test]
fn a_target_checked_out_in_another_worktree_blocks_the_merge() {
    let env = Env::new();
    let repo = env.repo("tidepool-api");
    let mut agent = Agent::spawn(&env);
    let refactor = ready(&mut agent, &repo, "src/main.rs", "fn main() { tidy() }\n");
    let worktree = path(&refactor["worktree"]);
    let main = rev(&repo, "main");
    git(&repo, &["checkout", "-q", "-b", "spike"]);
    let hold = env.tmp.join("hold");
    git(
        &repo,
        &["worktree", "add", "-q", hold.to_str().unwrap(), "main"],
    );
    let blocked = agent.request("mergeRefactor", json!({"path": repo}));
    assert_blocked(&blocked, "targetBusy", json!([hold.to_str().unwrap()]));
    assert_eq!(rev(&repo, "main"), main);
    assert!(worktree.is_dir());
    agent.wait_state(&repo, "mergeBlocked");

    git(&repo, &["worktree", "remove", hold.to_str().unwrap()]);
    let merged = agent.request("mergeRefactor", json!({"path": repo}));
    assert_eq!(merged["result"]["outcome"], "merged", "{merged}");
    assert_eq!(git(&repo, &["branch", "--show-current"]), "spike");
    agent.shutdown();
}

#[test]
fn a_repo_rebasing_its_target_blocks_the_merge() {
    let env = Env::new();
    let repo = env.repo("tidepool-api");
    let mut agent = Agent::spawn(&env);
    let refactor = ready(&mut agent, &repo, "src/main.rs", "fn main() { tidy() }\n");
    let head = rev(&path(&refactor["worktree"]), "HEAD");
    commit_file(&repo, "docs/tides.md", "# Tides\n", "Document tides");
    // A rebase of main that stops half way.
    assert!(!try_git(
        &repo,
        &["rebase", "-q", "--exec", "false", "HEAD~1"]
    ));
    let git_dir = repo.join(".git");
    assert_eq!(
        std::fs::read_to_string(git_dir.join("rebase-merge/head-name"))
            .unwrap()
            .trim(),
        "refs/heads/main"
    );
    let blocked = agent.request("mergeRefactor", json!({"path": repo}));
    assert_blocked(&blocked, "targetBusy", json!([repo.to_str().unwrap()]));
    assert!(git_dir.join("rebase-merge").is_dir());

    git(&repo, &["rebase", "--abort"]);
    let merged = agent.request("mergeRefactor", json!({"path": repo}));
    assert_eq!(merged["result"]["outcome"], "merged", "{merged}");
    assert_eq!(
        git(&repo, &["log", "-1", "--format=%s"]),
        "Tidy the entry point"
    );
    assert_ne!(rev(&repo, "HEAD"), head);
    agent.shutdown();
}

#[test]
fn uncommitted_changes_in_the_worktree_block_the_merge() {
    let env = Env::new();
    let repo = env.repo("tidepool-api");
    let mut agent = Agent::spawn(&env);
    let refactor = ready(&mut agent, &repo, "src/main.rs", "fn main() { tidy() }\n");
    let worktree = path(&refactor["worktree"]);
    write(&worktree.join("README.md"), "# half done\n");
    let main = rev(&repo, "main");
    let blocked = agent.request("mergeRefactor", json!({"path": repo}));
    assert_blocked(&blocked, "worktreeDirty", json!(["README.md"]));
    assert_eq!(rev(&repo, "main"), main);
    assert_eq!(
        std::fs::read_to_string(worktree.join("README.md")).unwrap(),
        "# half done\n"
    );
    let state = agent.wait_state(&repo, "mergeBlocked");
    assert_eq!(
        state["blocked"],
        json!({"reason": "worktreeDirty", "files": ["README.md"]})
    );
    agent.shutdown();
}

#[test]
fn a_worktree_off_its_branch_or_mid_rebase_is_left_alone() {
    let env = Env::new();
    let repo = env.repo("tidepool-api");
    let mut agent = Agent::spawn(&env);
    let refactor = ready(&mut agent, &repo, "src/main.rs", "fn main() { tidy() }\n");
    let worktree = path(&refactor["worktree"]);
    let branch = refactor["branch"].as_str().unwrap().to_owned();
    let main = rev(&repo, "main");

    git(&worktree, &["checkout", "-q", "--detach"]);
    let blocked = agent.request("mergeRefactor", json!({"path": repo}));
    assert_blocked(&blocked, "worktreeBusy", json!([]));
    git(&worktree, &["checkout", "-q", &branch]);

    // The agent's own rebase, stopped half way, is not aborted.
    commit_file(&repo, "docs/tides.md", "# Tides\n", "Document tides");
    let main_moved = rev(&repo, "main");
    assert!(!try_git(
        &worktree,
        &["rebase", "-q", "--exec", "false", "HEAD~1"]
    ));
    let git_dir = PathBuf::from(git(&worktree, &["rev-parse", "--absolute-git-dir"]));
    let todo = std::fs::read_to_string(git_dir.join("rebase-merge/head-name")).unwrap();
    let blocked = agent.request("mergeRefactor", json!({"path": repo}));
    assert_blocked(&blocked, "worktreeBusy", json!([]));
    assert_eq!(
        std::fs::read_to_string(git_dir.join("rebase-merge/head-name")).unwrap(),
        todo
    );
    assert_eq!(rev(&repo, "main"), main_moved);
    assert_ne!(main_moved, main);
    git(&worktree, &["rebase", "--abort"]);
    let merged = agent.request("mergeRefactor", json!({"path": repo}));
    assert_eq!(merged["result"]["outcome"], "merged", "{merged}");
    agent.shutdown();
}

#[test]
fn stash_and_squash_config_does_not_change_the_merge() {
    let env = Env::new();
    let repo = env.repo("tidepool-api");
    for (key, value) in [
        ("merge.autoStash", "true"),
        ("rebase.autoStash", "true"),
        ("rebase.autoSquash", "true"),
        ("rebase.updateRefs", "true"),
        ("merge.ff", "false"),
    ] {
        git(&repo, &["config", key, value]);
    }
    let mut agent = Agent::spawn(&env);
    let refactor = ready(&mut agent, &repo, "src/main.rs", "fn main() { tidy() }\n");
    let worktree = path(&refactor["worktree"]);
    commit_file(
        &worktree,
        "src/lib.rs",
        "pub fn x() {}\n",
        "fixup! Tidy the entry point",
    );
    commit_file(&repo, "docs/tides.md", "# Tides\n", "Document tides");
    write(&repo.join("notes.txt"), "draft\n");
    git(&repo, &["add", "notes.txt"]);
    let merged = agent.request("mergeRefactor", json!({"path": repo}));
    assert_eq!(
        merged["result"],
        json!({"outcome": "merged", "commits": 2}),
        "{merged}"
    );
    assert_eq!(
        git(&repo, &["diff", "--cached", "--name-only"]),
        "notes.txt"
    );
    assert_eq!(git(&repo, &["stash", "list"]), "");
    assert_eq!(
        git(&repo, &["log", "-3", "--format=%s"]),
        "fixup! Tidy the entry point\nTidy the entry point\nDocument tides"
    );
    agent.shutdown();
}

#[test]
fn a_worktree_git_will_not_remove_after_a_merge_is_kept_until_discarded() {
    let env = Env::new();
    let repo = env.repo("tidepool-api");
    let mut agent = Agent::spawn(&env);
    let refactor = ready(&mut agent, &repo, "src/main.rs", "fn main() { tidy() }\n");
    let worktree = path(&refactor["worktree"]);
    let branch = refactor["branch"].as_str().unwrap().to_owned();
    let head = rev(&worktree, "HEAD");
    write(&worktree.join("scratch.txt"), "agent notes\n");
    let merged = agent.request("mergeRefactor", json!({"path": repo}));
    assert_eq!(merged["result"]["outcome"], "merged", "{merged}");
    assert_eq!(merged["result"]["commits"], 1);
    let kept = merged["result"]["kept"].as_array().unwrap();
    assert_eq!(kept.len(), 1, "{merged}");
    assert!(kept[0].as_str().unwrap().starts_with("Kept the worktree"));
    assert_eq!(rev(&repo, "main"), head);
    assert!(worktree.join("scratch.txt").is_file());
    assert!(branch_exists(&repo, &branch));

    let left = agent.wait_state(&repo, "nothingToDo");
    let summary = left["summary"].as_str().unwrap();
    assert!(
        summary.starts_with("Merged; the worktree could not be removed: "),
        "{summary}"
    );
    let records = env.records();
    assert_eq!(records.len(), 1);
    assert_eq!(records[0]["leftover"], true);
    let again = agent.request("mergeRefactor", json!({"path": repo}));
    assert_eq!(again["error"]["code"], 8, "{again}");

    // After a restart it is kept, not adopted afresh.
    agent.shutdown();
    let mut agent = Agent::spawn(&env);
    agent.wait_state(&repo, "nothingToDo");
    assert_eq!(env.records().len(), 1);
    let discarded = agent.request("discardRefactor", json!({"path": repo}));
    assert_eq!(discarded["result"], Value::Null, "{discarded}");
    assert!(!worktree.exists());
    assert!(!branch_exists(&repo, &branch));
    assert_eq!(rev(&repo, "main"), head);
    assert!(env.records().is_empty());
    agent.shutdown();
}

#[test]
fn a_launch_never_picked_up_is_interrupted_and_resumes_as_a_new_session() {
    let env = Env::new();
    let repo = env.repo("tidepool-api");
    let mut agent = Agent::spawn_with(&env, &[("REFRACT_REFACTOR_GRACE_SECS", "2")]);
    let (ticket, refactor) = start(&mut agent, &repo, Some("the parser"));
    assert_eq!(refactor["state"], "starting");
    let old_session = ticket["sessionId"].as_str().unwrap().to_owned();
    let old_request = ticket["requestId"].as_str().unwrap().to_owned();
    assert_eq!(env.records()[0]["requestId"], old_request);
    let interrupted = agent.wait_state(&repo, "interrupted");
    assert_eq!(interrupted["live"], false);

    // The extension had half claimed it; both files go.
    let launch = env.state.join("launch");
    let old_json = launch.join(format!("{old_request}.json"));
    let old_claimed = launch.join(format!("{old_request}.claimed"));
    std::fs::copy(&old_json, &old_claimed).unwrap();
    let resumed = agent.request("resumeRefactor", json!({"path": repo}));
    let ticket = &resumed["result"];
    let session = ticket["sessionId"].as_str().unwrap();
    assert_ne!(session, old_session, "{resumed}");
    assert!(!old_json.exists() && !old_claimed.exists());
    let request = env.launch_request(ticket);
    let argv = request["argv"].as_array().unwrap();
    assert_eq!(
        argv[..5],
        [
            json!("claude"),
            json!("--session-id"),
            json!(session),
            json!("--permission-mode"),
            json!("plan")
        ]
    );
    assert!(argv[6].as_str().unwrap().ends_with("Focus on: the parser"));
    let records = env.records();
    assert_eq!(records[0]["sessionId"], session);
    assert_eq!(records[0]["requestId"], ticket["requestId"]);
    let state = agent.repo(&repo)["refactor"].clone();
    assert_eq!(state["state"], "starting");
    assert_eq!(state["sessionId"], session);
    agent.shutdown();
}

#[test]
fn a_folder_git_does_not_list_for_the_repo_is_never_deleted() {
    let env = Env::new();
    let repo = env.repo("tidepool-api");
    let other = env.repo("kelp-cli");
    // A folder of another repo's worktree, under the worktrees folder.
    let theirs = env.worktrees().join("kelp-cli-20200101-0000");
    git(
        &other,
        &[
            "worktree",
            "add",
            "-q",
            "-b",
            "spike",
            theirs.to_str().unwrap(),
        ],
    );
    let record = json!({
        "repo": repo,
        "worktree": theirs,
        "branch": "refactor/20200101-0000",
        "baseCommit": rev(&repo, "main"),
        "targetBranch": "main",
        "sessionId": "",
        "createdAt": "2026-10-31T14:05:00Z",
    });
    write(
        &env.state
            .join("refactors/0b7e0000-0000-4000-8000-00000000000a.json"),
        &record.to_string(),
    );
    let mut agent = Agent::spawn(&env);
    let discard = agent.request("discardRefactor", json!({"path": repo}));
    assert_eq!(discard["error"]["code"], 8, "{discard}");
    assert!(theirs.join("README.md").is_file());
    assert!(branch_exists(&other, "spike"));

    // A refactor whose worktree git stops listing while the helper runs.
    let refactor = ready(&mut agent, &repo, "src/main.rs", "fn main() { tidy() }\n");
    let worktree = path(&refactor["worktree"]);
    let git_dir = PathBuf::from(git(&worktree, &["rev-parse", "--absolute-git-dir"]));
    std::fs::remove_dir_all(&git_dir).unwrap();
    let discard = agent.request("discardRefactor", json!({"path": repo}));
    assert_eq!(discard["error"]["code"], 8, "{discard}");
    let merge = agent.request("mergeRefactor", json!({"path": repo}));
    assert_eq!(merge["error"]["code"], 8, "{merge}");
    assert!(worktree.join("src/main.rs").is_file());
    agent.shutdown();
}

#[test]
fn two_repos_of_one_name_get_their_own_worktrees() {
    let env = Env::new();
    let first = env.repo("north/tidepool-api");
    let second = env.repo("south/tidepool-api");
    let mut agent = Agent::spawn(&env);
    let (_, a) = start(&mut agent, &first, None);
    let (_, b) = start(&mut agent, &second, None);
    let (wa, wb) = (path(&a["worktree"]), path(&b["worktree"]));
    assert_ne!(wa, wb);
    assert!(wa.starts_with(env.worktrees()) && wb.starts_with(env.worktrees()));
    assert!(git(&first, &["worktree", "list"]).contains(wa.to_str().unwrap()));
    assert!(git(&second, &["worktree", "list"]).contains(wb.to_str().unwrap()));
    assert_eq!(env.records().len(), 2);
    let discarded = agent.request("discardRefactor", json!({"path": first}));
    assert_eq!(discarded["result"], Value::Null, "{discarded}");
    assert!(!wa.exists() && wb.is_dir());
    assert_eq!(agent.repo(&second)["refactor"]["worktree"], b["worktree"]);
    agent.shutdown();
}

#[test]
fn records_of_gone_or_unknown_repos_are_dropped_or_kept_without_deleting() {
    let env = Env::new();
    let repo = env.repo("tidepool-api");
    // A repo outside the roots, with a refactor worktree.
    let outside = env.tmp.join("elsewhere/kelp-cli");
    init_repo(&outside);
    let kept = env.worktrees().join("kelp-cli-20200101-0000");
    git(
        &outside,
        &[
            "worktree",
            "add",
            "-q",
            "-b",
            "refactor/20200101-0000",
            kept.to_str().unwrap(),
        ],
    );
    // A folder for a repo that is gone.
    let orphan = env.worktrees().join("reef-maps-20200101-0000");
    write(&orphan.join("notes.txt"), "keep me\n");
    let record = |repo: &Path, worktree: &Path| {
        json!({
            "repo": repo,
            "worktree": worktree,
            "branch": "refactor/20200101-0000",
            "baseCommit": rev(&outside, "main"),
            "targetBranch": "main",
            "sessionId": "",
            "createdAt": "2026-10-31T14:05:00Z",
        })
        .to_string()
    };
    let records = env.state.join("refactors");
    write(
        &records.join("0b7e0000-0000-4000-8000-00000000000b.json"),
        &record(&outside, &kept),
    );
    write(
        &records.join("0b7e0000-0000-4000-8000-00000000000c.json"),
        &record(&env.root.join("reef-maps"), &orphan),
    );
    let mut agent = Agent::spawn(&env);
    let list = agent.request("listRepos", Value::Null);
    assert_eq!(list["result"]["repos"].as_array().unwrap().len(), 1);
    assert_eq!(agent.repo(&repo)["refactor"], Value::Null);
    let left = env.records();
    assert_eq!(left.len(), 1, "{left:?}");
    assert_eq!(left[0]["repo"], outside.to_str().unwrap());
    assert!(orphan.join("notes.txt").is_file());
    assert!(kept.join("README.md").is_file());
    let discard = agent.request("discardRefactor", json!({"path": outside}));
    assert_eq!(discard["error"]["code"], 8, "{discard}");
    assert!(kept.is_dir());
    agent.shutdown();
}

#[test]
fn without_a_hook_an_open_agent_counts_as_idle_only_once_quiet() {
    let env = Env::new();
    let repo = env.repo("tidepool-api");
    let mut agent = Agent::spawn_with(&env, &[("REFRACT_REFACTOR_QUIET_SECS", "5")]);
    let refactor = ready(&mut agent, &repo, "src/main.rs", "fn main() { tidy() }\n");
    let worktree = path(&refactor["worktree"]);
    let session = refactor["sessionId"].as_str().unwrap().to_owned();
    let _claude = FakeClaude::start(&worktree, &session);
    env.transcript(&session, &worktree, true);
    // Just written: it may still be at work.
    agent.wait_state(&repo, "working");
    let merge = agent.request("mergeRefactor", json!({"path": repo}));
    assert_blocked(&merge, "live", json!([]));

    // Quiet for a minute: its turn is over.
    let file = project_dir(&env.projects, &worktree).join(format!("{session}.jsonl"));
    let past = format!("@{}", (Utc::now() - TimeDelta::minutes(1)).timestamp());
    let touched = Command::new("touch")
        .args(["-d", &past])
        .arg(&file)
        .status()
        .unwrap();
    assert!(touched.success());
    let ready = agent.wait_state(&repo, "ready");
    assert_eq!(ready["live"], true);

    // Another agent in the worktree is busy whatever it does.
    let other = FakeClaude::start(
        &worktree.join("src"),
        "5e2a7c10-0000-4000-8000-0000000000b2",
    );
    agent.wait_state(&repo, "working");
    let merge = agent.request("mergeRefactor", json!({"path": repo}));
    assert_blocked(&merge, "live", json!([]));
    let discard = agent.request("discardRefactor", json!({"path": repo}));
    assert_eq!(discard["error"]["code"], 8, "{discard}");
    drop(other);
    agent.wait_state(&repo, "ready");
    let merged = agent.request("mergeRefactor", json!({"path": repo}));
    assert_eq!(merged["result"]["outcome"], "merged", "{merged}");
    agent.shutdown();
}

#[test]
fn a_tag_named_like_the_target_does_not_get_in_the_way() {
    let env = Env::new();
    let repo = env.repo("tidepool-api");
    git(&repo, &["tag", "main"]);
    let tagged = rev(&repo, "refs/tags/main");
    let mut agent = Agent::spawn(&env);
    let refactor = ready(&mut agent, &repo, "src/main.rs", "fn main() { tidy() }\n");
    assert_eq!(refactor["targetBranch"], "main");
    commit_file(&repo, "docs/tides.md", "# Tides\n", "Document tides");
    let moved = rev(&repo, "refs/heads/main");
    let merged = agent.request("mergeRefactor", json!({"path": repo}));
    assert_eq!(merged["result"]["outcome"], "merged", "{merged}");
    assert_eq!(rev(&repo, "refs/heads/main~1"), moved);
    assert_eq!(rev(&repo, "refs/tags/main"), tagged);
    assert!(repo.join("docs/tides.md").is_file());
    agent.shutdown();
}

#[test]
fn a_target_rewritten_under_the_refactor_blocks_the_merge() {
    let env = Env::new();
    let repo = env.repo("tidepool-api");
    let mut agent = Agent::spawn(&env);
    let refactor = ready(&mut agent, &repo, "src/main.rs", "fn main() { tidy() }\n");
    let worktree = path(&refactor["worktree"]);
    git(&repo, &["commit", "-q", "--amend", "-m", "Start over"]);
    let main = rev(&repo, "main");
    let blocked = agent.request("mergeRefactor", json!({"path": repo}));
    assert_blocked(&blocked, "targetRewound", json!([]));
    assert_eq!(rev(&repo, "main"), main);
    assert!(worktree.is_dir());
    agent.shutdown();
}
