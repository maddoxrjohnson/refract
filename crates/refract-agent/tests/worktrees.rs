//! Linked worktrees end to end, over temporary repos with a bare remote: the facts and
//! status in their snapshots, how they are found, which repo gets their sessions, and
//! `removeWorktree` refusing for each data-safety rule and succeeding on a clean, pushed
//! one. All repo names are invented.

mod common;

use std::io::{BufRead, BufReader, Write};
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStdin, Command, Stdio};
use std::sync::mpsc::{self, Receiver};
use std::time::{Duration, Instant};

use chrono::{TimeDelta, Utc};
use refract_agent::config::Config;
use refract_agent::state::Agent;
use refract_core::protocol::{GitOperation, RepoSnapshot, WorktreeInfo, WorktreeStatus};
use serde_json::{json, Value};

use common::*;

const SESSION_ID: &str = "7c3e9a20-0000-4000-8000-000000000001";

/// Runs git in `dir` and returns whether it succeeded, for commands expected to fail.
fn git_ok(dir: &Path, args: &[&str]) -> bool {
    Command::new("git")
        .arg("-C")
        .arg(dir)
        .args(args)
        .envs(GIT_ENV.iter().copied())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .unwrap()
        .success()
}

fn add_worktree(main: &Path, dest: &Path, branch: &str) {
    git(
        main,
        &[
            "worktree",
            "add",
            "-q",
            "-b",
            branch,
            dest.to_str().unwrap(),
        ],
    );
}

fn branch_exists(repo: &Path, branch: &str) -> bool {
    git_ok(
        repo,
        &[
            "rev-parse",
            "--verify",
            "-q",
            &format!("refs/heads/{branch}"),
        ],
    )
}

/// A main repo cloned from a bare remote, with a committed `.gitignore`.
struct Fixture {
    _tmp: tempfile::TempDir,
    base: PathBuf,
    root: PathBuf,
    projects: PathBuf,
    main: PathBuf,
}

impl Fixture {
    fn new() -> Fixture {
        hermetic_git();
        let tmp = tempfile::tempdir().unwrap();
        let base = tmp.path().canonicalize().unwrap();
        let root = base.join("projects");
        let projects = base.join("home/.claude/projects");
        let (bare, _seed) = remote(&base, "harbor-notes");
        let main = root.join("harbor-notes");
        clone(&bare, &main);
        commit_file(
            &main,
            ".gitignore",
            "*.log\ntarget/\n",
            "Ignore build output",
        );
        git(&main, &["push", "-q", "origin", "main"]);
        Fixture {
            _tmp: tmp,
            base,
            root,
            projects,
            main,
        }
    }

    fn load(&self) -> Vec<RepoSnapshot> {
        let mut agent = Agent::new(Config {
            roots: vec![self.root.clone()],
            extra: vec![],
            hidden: vec![],
            depth: 3,
            claude_projects: self.projects.clone(),
            limits_file: self.projects.join("usage-state.json"),
        });
        agent.load_all();
        agent.snapshots()
    }
}

fn find<'a>(repos: &'a [RepoSnapshot], path: &Path) -> &'a RepoSnapshot {
    repos
        .iter()
        .find(|r| r.path == path.to_str().unwrap())
        .unwrap_or_else(|| panic!("no snapshot for {}", path.display()))
}

fn info<'a>(repos: &'a [RepoSnapshot], path: &Path) -> &'a WorktreeInfo {
    find(repos, path)
        .worktree
        .as_ref()
        .unwrap_or_else(|| panic!("no worktree facts for {}", path.display()))
}

#[test]
fn facts_and_status_for_each_kind_of_worktree() {
    let f = Fixture::new();
    let main = &f.main;
    let wt = |name: &str| f.root.join(format!("harbor-notes-{name}"));

    let pushed = wt("pushed");
    add_worktree(main, &pushed, "pushed");
    commit_file(&pushed, "src/pushed.rs", "// pushed\n", "Add pushed");
    git(&pushed, &["push", "-q", "origin", "pushed"]);

    let merged = wt("merged");
    add_worktree(main, &merged, "merged");
    commit_file(&merged, "src/merged.rs", "// merged\n", "Add merged");
    git(&merged, &["push", "-q", "origin", "HEAD:main"]);
    git(&merged, &["fetch", "-q", "origin"]);

    let tagged = wt("tagged");
    add_worktree(main, &tagged, "tagged");
    commit_file(&tagged, "src/tagged.rs", "// tagged\n", "Add tagged");
    git(&tagged, &["push", "-q", "origin", "tagged"]);
    git(&tagged, &["tag", "v1.4.0"]);
    git(&tagged, &["tag", "v1.4.1"]);

    let dirty = wt("dirty");
    add_worktree(main, &dirty, "dirty");
    write(&dirty.join("README.md"), "# changed\n");

    let untracked = wt("untracked");
    add_worktree(main, &untracked, "untracked");
    write(&untracked.join("notes/todo.txt"), "todo\n");

    let ignored = wt("ignored");
    add_worktree(main, &ignored, "ignored");
    write(&ignored.join("build.log"), "log\n");
    write(&ignored.join("target/debug/app"), "bin\n");
    write(&ignored.join("src/cache.log"), "log\n");

    let locked = wt("locked");
    add_worktree(main, &locked, "locked");
    git(main, &["worktree", "lock", locked.to_str().unwrap()]);

    let rebasing = wt("rebasing");
    add_worktree(main, &rebasing, "rebasing");
    commit_file(
        &rebasing,
        "README.md",
        "# rebasing\n",
        "Change the readme here",
    );
    commit_file(main, "README.md", "# upstream\n", "Change the readme there");
    assert!(!git_ok(&rebasing, &["rebase", "main"]), "the rebase stops");

    let detached = wt("detached");
    git(
        main,
        &[
            "worktree",
            "add",
            "-q",
            "--detach",
            detached.to_str().unwrap(),
            "origin/main",
        ],
    );

    // From the remote's main: the main checkout has an unpushed commit by now.
    let only_here = wt("only-here");
    git(
        main,
        &[
            "worktree",
            "add",
            "-q",
            "-b",
            "only-here",
            only_here.to_str().unwrap(),
            "origin/main",
        ],
    );
    commit_file(&only_here, "src/a.rs", "// a\n", "Add a");
    commit_file(&only_here, "src/b.rs", "// b\n", "Add b");

    let repos = f.load();
    let main_snapshot = find(&repos, main);
    assert_eq!(main_snapshot.worktree_of, None);
    assert_eq!(main_snapshot.worktree, None);

    let main_path = main.to_str().unwrap();
    for path in [
        &pushed, &merged, &tagged, &dirty, &untracked, &ignored, &locked, &rebasing, &detached,
        &only_here,
    ] {
        let snapshot = find(&repos, path);
        assert_eq!(snapshot.worktree_of.as_deref(), Some(main_path), "{path:?}");
        assert!(info(&repos, path).untracked_checked);
    }

    let pushed_info = info(&repos, &pushed);
    assert_eq!(pushed_info.status, WorktreeStatus::Pushed);
    assert!(pushed_info.removable);
    assert_eq!(pushed_info.only_here, 0);
    assert_eq!(pushed_info.merged, None);

    let merged_info = info(&repos, &merged);
    assert_eq!(merged_info.merged.as_deref(), Some("main"));
    assert_eq!(
        merged_info.status,
        WorktreeStatus::Merged {
            branch: "main".into()
        }
    );
    assert!(merged_info.removable);

    let tagged_info = info(&repos, &tagged);
    assert_eq!(tagged_info.release.as_deref(), Some("v1.4.0"));
    assert_eq!(
        tagged_info.status,
        WorktreeStatus::Release {
            tag: "v1.4.0".into()
        }
    );
    assert!(tagged_info.removable);

    let dirty_info = info(&repos, &dirty);
    assert_eq!(dirty_info.status, WorktreeStatus::Changes { count: 1 });
    assert!(!dirty_info.removable);
    assert_eq!(find(&repos, &dirty).changed_files, 1);

    let untracked_info = info(&repos, &untracked);
    assert_eq!(untracked_info.status, WorktreeStatus::Changes { count: 1 });
    assert!(!untracked_info.removable);

    let ignored_info = info(&repos, &ignored);
    // Its commit is in the tag made on a later commit.
    assert_eq!(
        ignored_info.status,
        WorktreeStatus::Release {
            tag: "v1.4.0".into()
        }
    );
    assert!(ignored_info.removable);
    assert_eq!(ignored_info.ignored, ["build.log", "src/", "target/"]);
    assert_eq!(find(&repos, &ignored).changed_files, 0);

    let locked_info = info(&repos, &locked);
    assert!(locked_info.locked);
    assert_eq!(locked_info.status, WorktreeStatus::Locked);
    assert!(!locked_info.removable);

    let rebasing_info = info(&repos, &rebasing);
    assert_eq!(rebasing_info.operation, Some(GitOperation::Rebase));
    assert_eq!(
        rebasing_info.status,
        WorktreeStatus::Operation {
            op: GitOperation::Rebase
        }
    );
    assert!(!rebasing_info.removable);

    let detached_snapshot = find(&repos, &detached);
    assert_eq!(detached_snapshot.branch, None);
    assert_eq!(
        info(&repos, &detached).status,
        WorktreeStatus::Merged {
            branch: "main".into()
        }
    );

    let only_here_info = info(&repos, &only_here);
    assert_eq!(only_here_info.only_here, 2);
    assert_eq!(only_here_info.status, WorktreeStatus::OnlyHere { count: 2 });
    assert!(!only_here_info.removable);
}

#[test]
fn merged_into_local_main_without_a_remote() {
    let f = Fixture::new();
    let local = f.root.join("coral-db");
    init_repo(&local);
    let wt = f.root.join("coral-db-feature");
    add_worktree(&local, &wt, "feature");
    commit_file(&wt, "src/feature.rs", "// feature\n", "Add the feature");
    git(&local, &["merge", "-q", "--ff-only", "feature"]);
    let repos = f.load();
    let facts = info(&repos, &wt);
    assert!(facts.only_here > 0);
    assert_eq!(
        facts.status,
        WorktreeStatus::Merged {
            branch: "main".into()
        }
    );
}

#[test]
fn worktrees_outside_the_roots_and_nested_ones_are_found() {
    let f = Fixture::new();
    let outside = f.base.join("elsewhere/harbor-notes-outside");
    add_worktree(&f.main, &outside, "outside");
    let nested = f.main.join(".claude/worktrees/wide-cards");
    add_worktree(&f.main, &nested, "wide-cards");
    write(
        &project_dir(&f.projects, &nested).join(format!("{SESSION_ID}.jsonl")),
        &transcript(
            SESSION_ID,
            &nested,
            &nested.join("src/main.rs"),
            Utc::now() - TimeDelta::hours(1),
            true,
        ),
    );

    let repos = f.load();
    let paths: Vec<&str> = repos.iter().map(|r| r.path.as_str()).collect();
    assert!(paths.contains(&outside.to_str().unwrap()), "{paths:?}");
    assert!(paths.contains(&nested.to_str().unwrap()), "{paths:?}");
    assert_eq!(repos.len(), 3, "{paths:?}");
    let main_path = f.main.to_str().unwrap();
    assert_eq!(
        find(&repos, &outside).worktree_of.as_deref(),
        Some(main_path)
    );
    assert_eq!(
        find(&repos, &nested).worktree_of.as_deref(),
        Some(main_path)
    );
    let sessions: Vec<&str> = find(&repos, &nested)
        .sessions
        .iter()
        .map(|s| s.id.as_str())
        .collect();
    assert_eq!(sessions, [SESSION_ID]);
    assert!(find(&repos, &f.main).sessions.is_empty());

    // A worktree whose folder is gone is not reported.
    std::fs::remove_dir_all(&outside).unwrap();
    let repos = f.load();
    assert!(!repos.iter().any(|r| r.path == outside.to_str().unwrap()));
}

/// The built helper over stdio, with a temporary HOME.
struct Helper {
    child: Child,
    stdin: ChildStdin,
    lines: Receiver<String>,
    next_id: u64,
    /// Events seen while waiting for responses.
    events: Vec<Value>,
}

impl Helper {
    fn spawn(home: &Path, config: &Path, vars: &[(&str, &Path)]) -> Helper {
        let mut child = Command::new(env!("CARGO_BIN_EXE_refract-agent"))
            .env_remove("XDG_STATE_HOME")
            .env_remove("XDG_DATA_HOME")
            .env_remove("XDG_CONFIG_HOME")
            .env_remove("CLAUDE_CONFIG_DIR")
            .env("HOME", home)
            .env("REFRACT_AGENT_CONFIG", config)
            .env("WSL_DISTRO_NAME", "Ubuntu")
            .envs(GIT_ENV.iter().copied())
            .envs(vars.iter().map(|(k, v)| (*k, v.as_os_str())))
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
        let mut helper = Helper {
            child,
            stdin,
            lines,
            next_id: 1,
            events: Vec::new(),
        };
        let deadline = Instant::now() + Duration::from_secs(20);
        while helper.request("listRepos", Value::Null)["result"]["loading"] != false {
            assert!(Instant::now() < deadline, "load never completed");
            std::thread::sleep(Duration::from_millis(100));
        }
        helper
    }

    /// Sends a request and returns its response, keeping the events.
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
            if message.get("event").is_some() {
                self.events.push(message);
            }
        }
    }

    fn paths(&mut self) -> Vec<String> {
        let list = self.request("listRepos", Value::Null);
        list["result"]["repos"]
            .as_array()
            .unwrap()
            .iter()
            .map(|r| r["path"].as_str().unwrap().to_owned())
            .collect()
    }

    /// Asks to remove `path` and returns the refusal's message, checking its code.
    #[track_caller]
    fn refusal(&mut self, path: &Path) -> String {
        let reply = self.request("removeWorktree", json!({"path": path}));
        assert_eq!(reply["error"]["code"], 9, "{reply}");
        reply["error"]["message"].as_str().unwrap().to_owned()
    }

    fn shutdown(mut self) {
        self.request("shutdown", Value::Null);
        assert!(self.child.wait().unwrap().success());
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

#[test]
fn remove_worktree_refuses_for_each_rule_and_removes_a_clean_pushed_one() {
    hermetic_git();
    let tmp = tempfile::tempdir().unwrap();
    let base = tmp.path().canonicalize().unwrap();
    let home = base.join("home");
    let root = home.join("projects");
    let drive = base.join("drive");
    let config = base.join("agent.toml");
    write(
        &config,
        &format!(
            "roots = [{:?}, {:?}]\ndepth = 3\n",
            root.to_str().unwrap(),
            drive.to_str().unwrap()
        ),
    );
    std::fs::create_dir_all(home.join(".claude/projects")).unwrap();
    let (bare, _seed) = remote(&base, "harbor-notes");
    let main = root.join("harbor-notes");
    clone(&bare, &main);
    commit_file(&main, ".gitignore", "target/\n", "Ignore build output");
    git(&main, &["push", "-q", "origin", "main"]);
    let wt = |name: &str| root.join(format!("harbor-notes-{name}"));

    let refactor = home.join("projects/.refract-worktrees/harbor-notes-20261101-0900");
    add_worktree(&main, &refactor, "refactor/20261101-0900");

    let on_drive = drive.join("harbor-notes-drive");
    add_worktree(&main, &on_drive, "drive");

    let locked = wt("locked");
    add_worktree(&main, &locked, "locked");
    git(&main, &["worktree", "lock", locked.to_str().unwrap()]);

    let rebasing = wt("rebasing");
    add_worktree(&main, &rebasing, "rebasing");
    commit_file(
        &rebasing,
        "README.md",
        "# rebasing\n",
        "Change the readme here",
    );
    git(&rebasing, &["push", "-q", "origin", "rebasing"]);
    commit_file(
        &main,
        "README.md",
        "# upstream\n",
        "Change the readme there",
    );
    assert!(!git_ok(&rebasing, &["rebase", "main"]), "the rebase stops");

    let dirty = wt("dirty");
    add_worktree(&main, &dirty, "dirty");
    write(&dirty.join("scratch.txt"), "untracked\n");

    let only_here = wt("only-here");
    add_worktree(&main, &only_here, "only-here");
    commit_file(&only_here, "src/a.rs", "// a\n", "Add a");

    let busy = wt("busy");
    add_worktree(&main, &busy, "busy");

    let ready = base.join("elsewhere/harbor-notes-ready");
    add_worktree(&main, &ready, "ready");
    commit_file(&ready, "src/ready.rs", "// ready\n", "Add ready");
    git(&ready, &["push", "-q", "origin", "ready"]);
    write(&ready.join("target/debug/app"), "bin\n");

    let mut helper = Helper::spawn(
        &home,
        &config,
        &[("REFRACT_AGENT_TEST_SLOW_PREFIX", drive.as_path())],
    );
    let paths = helper.paths();
    // Refactor worktrees are never listed; one outside the roots is.
    assert!(
        !paths.iter().any(|p| p.contains(".refract-worktrees")),
        "{paths:?}"
    );
    assert!(
        paths.contains(&ready.to_str().unwrap().to_owned()),
        "{paths:?}"
    );
    assert!(
        paths.contains(&on_drive.to_str().unwrap().to_owned()),
        "{paths:?}"
    );
    let drive_snapshot = helper.request("getRepo", json!({"path": on_drive}));
    assert_eq!(
        drive_snapshot["result"]["worktree"]["status"],
        json!({"kind": "unchecked"})
    );
    assert_eq!(
        drive_snapshot["result"]["worktree"]["untrackedChecked"],
        false
    );
    assert_eq!(drive_snapshot["result"]["worktree"]["removable"], false);

    // 1. Not a linked worktree: the main checkout, a plain folder, a missing path.
    assert_eq!(helper.refusal(&main), "Not a linked worktree.");
    std::fs::create_dir_all(base.join("plain")).unwrap();
    assert_eq!(
        helper.refusal(&base.join("plain")),
        "Not a linked worktree."
    );
    assert_eq!(
        helper.refusal(&base.join("missing")),
        "Not a linked worktree."
    );
    // 2. Refract's refactor worktree.
    assert_eq!(
        helper.refusal(&refactor),
        "Refract's own refactor worktrees are removed from the Refactor section."
    );
    // 3. A Windows drive.
    assert_eq!(
        helper.refusal(&on_drive),
        "Worktrees on Windows drives can't be removed from Refract yet."
    );
    // 4. Locked, mid-rebase.
    assert_eq!(helper.refusal(&locked), "This worktree is locked.");
    assert_eq!(
        helper.refusal(&rebasing),
        "A rebase is in progress in this worktree."
    );
    // 5. Untracked files count as changes.
    assert_eq!(
        helper.refusal(&dirty),
        "This worktree has uncommitted changes."
    );
    // 6. Commits only here.
    assert_eq!(
        helper.refusal(&only_here),
        "Some commits are only on this computer."
    );
    // 7. A claude process in the folder.
    {
        let _claude = FakeClaude::start(&busy, SESSION_ID);
        std::thread::sleep(Duration::from_millis(200));
        assert_eq!(
            helper.refusal(&busy),
            "An agent is running in this worktree."
        );
    }
    // Nothing refused was changed.
    for path in [
        &refactor, &on_drive, &locked, &rebasing, &dirty, &only_here, &busy,
    ] {
        assert!(path.join(".git").is_file(), "{path:?}");
    }
    assert!(dirty.join("scratch.txt").is_file());

    // A clean, pushed worktree goes; its branch stays, and so does everything else.
    helper.events.clear();
    let reply = helper.request("removeWorktree", json!({"path": ready}));
    assert_eq!(reply["result"], Value::Null, "{reply}");
    assert!(reply.get("error").is_none(), "{reply}");
    assert!(!ready.exists());
    assert!(branch_exists(&main, "ready"));
    assert!(branch_exists(&main, "only-here"));
    let paths = helper.paths();
    assert!(
        !paths.contains(&ready.to_str().unwrap().to_owned()),
        "{paths:?}"
    );
    assert!(
        paths.contains(&busy.to_str().unwrap().to_owned()),
        "{paths:?}"
    );
    let removed = helper
        .events
        .iter()
        .any(|e| e["event"] == "repoRemoved" && e["data"]["path"] == ready.to_str().unwrap());
    assert!(removed, "{:?}", helper.events);
    let main_changed = helper
        .events
        .iter()
        .any(|e| e["event"] == "repoChanged" && e["data"]["path"] == main.to_str().unwrap());
    assert!(main_changed, "{:?}", helper.events);
    // Gone now, so a second removal is refused.
    assert_eq!(helper.refusal(&ready), "Not a linked worktree.");
    helper.shutdown();
}
