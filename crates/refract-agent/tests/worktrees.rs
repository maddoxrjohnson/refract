//! Linked worktrees end to end, over temporary repos with a bare remote: the facts and
//! status in their snapshots, how they are found, which repo gets their sessions, and
//! `removeWorktree` refusing for each data-safety rule and succeeding on a clean, pushed
//! one; `checkWorktree` and removal on a Windows drive (the test prefix stands in for
//! one), and missing worktrees with `pruneWorktree`. All repo names are invented.

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

/// Adds a worktree of `main` at `dest` on a new branch with one commit, pushed.
fn pushed_worktree(main: &Path, dest: &Path, branch: &str) {
    add_worktree(main, dest, branch);
    commit_file(
        dest,
        &format!("src/{branch}.rs"),
        "// work\n",
        "Add the work",
    );
    git(dest, &["push", "-q", "origin", branch]);
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
        self.load_hiding(vec![])
    }

    fn load_hiding(&self, hidden: Vec<PathBuf>) -> Vec<RepoSnapshot> {
        let mut agent = Agent::new(Config {
            roots: vec![self.root.clone()],
            extra: vec![],
            hidden,
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

#[test]
fn worktrees_holding_another_repo_or_uncounted_commits_are_kept() {
    let f = Fixture::new();
    let main = &f.main;
    let wt = |name: &str| f.root.join(format!("harbor-notes-{name}"));

    // Another worktree, with work of its own, inside an ignored folder.
    let outer = wt("outer");
    pushed_worktree(main, &outer, "outer");
    let inner = outer.join("target/inner");
    add_worktree(main, &inner, "inner");
    write(&inner.join("notes.txt"), "not committed\n");

    // A plain repo inside an ignored folder, which nothing else knows about.
    let holder = wt("holder");
    pushed_worktree(main, &holder, "holder");
    commit_file(
        &holder,
        ".gitignore",
        "*.log\ntarget/\nscratch/\n",
        "Ignore scratch",
    );
    git(&holder, &["push", "-q", "origin", "holder"]);
    init_repo(&holder.join("scratch/cache/vendored"));

    // A submodule, committed and pushed.
    let (sub_remote, _) = remote(&f.base, "harbor-icons");
    let with_sub = wt("with-sub");
    pushed_worktree(main, &with_sub, "with-sub");
    git(
        &with_sub,
        &[
            "-c",
            "protocol.file.allow=always",
            "submodule",
            "add",
            "-q",
            sub_remote.to_str().unwrap(),
            "icons",
        ],
    );
    git(&with_sub, &["commit", "-q", "-m", "Add the icons"]);
    git(&with_sub, &["push", "-q", "origin", "with-sub"]);

    // No commits yet, so git cannot count the unpushed ones.
    let orphan = wt("orphan");
    git(
        main,
        &[
            "worktree",
            "add",
            "-q",
            "--orphan",
            "-b",
            "orphan",
            orphan.to_str().unwrap(),
        ],
    );

    let repos = f.load();
    for path in [&outer, &holder, &with_sub] {
        let facts = info(&repos, path);
        assert_eq!(facts.status, WorktreeStatus::HoldsRepo, "{path:?}");
        assert!(!facts.removable, "{path:?}");
        assert!(facts.ignored.is_empty(), "{path:?}");
    }
    let orphan_facts = info(&repos, &orphan);
    assert_eq!(orphan_facts.status, WorktreeStatus::OnlyHereUnknown);
    assert!(!orphan_facts.removable);
    // The inner worktree is a worktree of its own.
    assert_eq!(
        find(&repos, &inner).worktree_of.as_deref(),
        Some(main.to_str().unwrap())
    );
}

#[test]
fn big_build_folders_are_not_searched_but_other_ignored_folders_are() {
    let f = Fixture::new();
    let main = &f.main;
    let ignore = "*.log\ntarget/\nnode_modules/\nscratch/\n";

    // More folders than the search reads in one ignored folder, and a package cloned
    // with git, all in node_modules.
    let modules = f.root.join("harbor-notes-modules");
    pushed_worktree(main, &modules, "modules");
    commit_file(&modules, ".gitignore", ignore, "Ignore dependencies");
    git(&modules, &["push", "-q", "origin", "modules"]);
    for i in 0..5_500 {
        write(
            &modules.join(format!("node_modules/pkg-{i}/lib/index.js")),
            "export {};\n",
        );
    }
    init_repo(&modules.join("node_modules/git-dep"));

    // A plain repo deep in an ignored folder that is not build output.
    let scratch = f.root.join("harbor-notes-scratch");
    pushed_worktree(main, &scratch, "scratch");
    commit_file(&scratch, ".gitignore", ignore, "Ignore scratch");
    git(&scratch, &["push", "-q", "origin", "scratch"]);
    init_repo(&scratch.join("scratch/experiments/vendored"));

    let repos = f.load();
    let modules_facts = info(&repos, &modules);
    assert_eq!(modules_facts.status, WorktreeStatus::Pushed);
    assert!(modules_facts.removable);
    assert_eq!(modules_facts.ignored, ["node_modules/"]);
    let scratch_facts = info(&repos, &scratch);
    assert_eq!(scratch_facts.status, WorktreeStatus::HoldsRepo);
    assert!(!scratch_facts.removable);
}

#[test]
fn hiding_a_repo_hides_the_worktrees_found_from_it() {
    let f = Fixture::new();
    let outside = f.base.join("elsewhere/harbor-notes-outside");
    add_worktree(&f.main, &outside, "outside");
    let nested = f.main.join(".claude/worktrees/wide-cards");
    add_worktree(&f.main, &nested, "wide-cards");
    assert_eq!(f.load().len(), 3);
    let repos = f.load_hiding(vec![f.main.clone()]);
    let paths: Vec<&str> = repos.iter().map(|r| r.path.as_str()).collect();
    assert!(paths.is_empty(), "{paths:?}");
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

    // Git for Windows writes Windows paths into the admin dir's `gitdir`.
    let by_windows = wt("by-windows");
    add_worktree(&main, &by_windows, "by-windows");
    write(
        &main.join(".git/worktrees/harbor-notes-by-windows/gitdir"),
        "C:/Users/dev/harbor-notes-by-windows/.git\n",
    );

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
    assert_eq!(drive_snapshot["result"]["worktree"]["checkable"], true);
    write(&on_drive.join("scratch.txt"), "untracked\n");

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
    // 3. Made by Git for Windows. A Windows drive itself is no reason: there the full
    // status runs, and finds the untracked file the snapshot did not list.
    assert_eq!(
        helper.refusal(&by_windows),
        "This worktree was made by Git for Windows; remove it from Windows."
    );
    assert_eq!(
        helper.refusal(&on_drive),
        "This worktree has uncommitted changes."
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
        &refactor,
        &on_drive,
        &by_windows,
        &locked,
        &rebasing,
        &dirty,
        &only_here,
        &busy,
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

#[test]
fn remove_worktree_refuses_what_git_would_delete_unseen() {
    hermetic_git();
    let tmp = tempfile::tempdir().unwrap();
    let base = tmp.path().canonicalize().unwrap();
    let home = base.join("home");
    let root = home.join("projects");
    let config = base.join("agent.toml");
    write(
        &config,
        &format!("roots = [{:?}]\ndepth = 3\n", root.to_str().unwrap()),
    );
    std::fs::create_dir_all(home.join(".claude/projects")).unwrap();
    let (bare, _seed) = remote(&base, "harbor-notes");
    let main = root.join("harbor-notes");
    clone(&bare, &main);
    commit_file(&main, ".gitignore", "target/\n", "Ignore build output");
    git(&main, &["push", "-q", "origin", "main"]);
    // Git's own clean check in `worktree remove` would miss untracked files.
    git(&main, &["config", "status.showUntrackedFiles", "no"]);
    let wt = |name: &str| root.join(format!("harbor-notes-{name}"));

    let outer = wt("outer");
    pushed_worktree(&main, &outer, "outer");
    let inner = outer.join("target/inner");
    add_worktree(&main, &inner, "inner");
    write(&inner.join("notes.txt"), "not committed\n");

    let holder = wt("holder");
    pushed_worktree(&main, &holder, "holder");
    commit_file(
        &holder,
        ".gitignore",
        "target/\nscratch/\n",
        "Ignore scratch",
    );
    git(&holder, &["push", "-q", "origin", "holder"]);
    let vendored = holder.join("scratch/cache/vendored");
    init_repo(&vendored);

    let (sub_remote, _) = remote(&base, "harbor-icons");
    let with_sub = wt("with-sub");
    pushed_worktree(&main, &with_sub, "with-sub");
    git(
        &with_sub,
        &[
            "-c",
            "protocol.file.allow=always",
            "submodule",
            "add",
            "-q",
            sub_remote.to_str().unwrap(),
            "icons",
        ],
    );
    git(&with_sub, &["commit", "-q", "-m", "Add the icons"]);
    git(&with_sub, &["push", "-q", "origin", "with-sub"]);

    let skipped = wt("skipped");
    pushed_worktree(&main, &skipped, "skipped");
    git(&skipped, &["update-index", "--skip-worktree", "README.md"]);
    write(&skipped.join("README.md"), "# edited\n");

    let assumed = wt("assumed");
    pushed_worktree(&main, &assumed, "assumed");
    git(
        &assumed,
        &["update-index", "--assume-unchanged", "README.md"],
    );
    write(&assumed.join("README.md"), "# edited\n");

    let untracked = wt("untracked");
    pushed_worktree(&main, &untracked, "untracked");
    write(&untracked.join("scratch.txt"), "untracked\n");

    let orphan = wt("orphan");
    git(
        &main,
        &[
            "worktree",
            "add",
            "-q",
            "--orphan",
            "-b",
            "orphan",
            orphan.to_str().unwrap(),
        ],
    );

    let refactored = wt("refactored");
    pushed_worktree(&main, &refactored, "refactored");

    let mut helper = Helper::spawn(&home, &config, &[]);
    let untracked_snapshot = helper.request("getRepo", json!({"path": untracked}));
    assert_eq!(
        untracked_snapshot["result"]["worktree"]["status"],
        json!({"kind": "changes", "count": 1})
    );
    let ticket = helper.request("startRefactor", json!({"path": refactored}));
    assert!(ticket["result"]["requestId"].is_string(), "{ticket}");

    assert_eq!(
        helper.refusal(&outer),
        "Another worktree or repo is inside this worktree's folder."
    );
    assert_eq!(
        helper.refusal(&holder),
        "An ignored folder in this worktree holds another repo."
    );
    assert_eq!(helper.refusal(&with_sub), "This worktree has submodules.");
    let hidden =
        "Some files are marked so git hides their changes (assume-unchanged or skip-worktree).";
    assert_eq!(helper.refusal(&skipped), hidden);
    assert_eq!(helper.refusal(&assumed), hidden);
    assert_eq!(
        helper.refusal(&untracked),
        "This worktree has uncommitted changes."
    );
    assert_eq!(
        helper.refusal(&orphan),
        "Cannot tell whether this worktree's commits are pushed."
    );
    assert_eq!(
        helper.refusal(&refactored),
        "This worktree has a refactor; merge or discard it first."
    );

    // Nothing refused was changed.
    assert!(inner.join("notes.txt").is_file());
    assert!(vendored.join(".git").is_dir());
    assert!(untracked.join("scratch.txt").is_file());
    assert_eq!(
        std::fs::read_to_string(skipped.join("README.md")).unwrap(),
        "# edited\n"
    );
    for path in [
        &outer,
        &holder,
        &with_sub,
        &skipped,
        &assumed,
        &untracked,
        &orphan,
        &refactored,
    ] {
        assert!(path.join(".git").is_file(), "{path:?}");
    }
    helper.shutdown();
}

/// A helper over a home with `~/projects/harbor-notes` cloned from a bare remote, with
/// `drive` (outside the home) read as a Windows drive.
struct DriveFixture {
    _tmp: tempfile::TempDir,
    home: PathBuf,
    root: PathBuf,
    drive: PathBuf,
    config: PathBuf,
    main: PathBuf,
}

impl DriveFixture {
    fn new() -> DriveFixture {
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
        commit_file(
            &main,
            ".gitignore",
            "target/\nnode_modules/\nscratch/\n",
            "Ignore build output",
        );
        git(&main, &["push", "-q", "origin", "main"]);
        DriveFixture {
            _tmp: tmp,
            home,
            root,
            drive,
            config,
            main,
        }
    }

    fn spawn(&self) -> Helper {
        Helper::spawn(
            &self.home,
            &self.config,
            &[("REFRACT_AGENT_TEST_SLOW_PREFIX", self.drive.as_path())],
        )
    }
}

impl Helper {
    /// Sends a request expected to be refused with `WORKTREE_REFUSED`, and returns the
    /// message.
    #[track_caller]
    fn refused(&mut self, method: &str, params: Value) -> String {
        let reply = self.request(method, params);
        assert_eq!(reply["error"]["code"], 9, "{reply}");
        reply["error"]["message"].as_str().unwrap().to_owned()
    }

    fn missing(&mut self, main: &Path) -> Vec<Value> {
        let snapshot = self.request("getRepo", json!({"path": main}));
        snapshot["result"]["missingWorktrees"]
            .as_array()
            .cloned()
            .unwrap_or_default()
    }
}

#[test]
fn worktrees_on_a_windows_drive_are_checked_on_demand_and_removed() {
    let f = DriveFixture::new();
    let main = &f.main;
    let on_drive = |name: &str| f.drive.join(format!("harbor-notes-{name}"));

    let clean = on_drive("clean");
    pushed_worktree(main, &clean, "clean");
    write(&clean.join("node_modules/dep/index.js"), "// dep\n");

    let untracked = on_drive("untracked");
    pushed_worktree(main, &untracked, "untracked");

    let holder = on_drive("holder");
    pushed_worktree(main, &holder, "holder");
    init_repo(&holder.join("scratch/vendored"));

    // An edit git hides counts as a change.
    let hiding = on_drive("hiding");
    pushed_worktree(main, &hiding, "hiding");
    git(
        &hiding,
        &["update-index", "--skip-worktree", "src/hiding.rs"],
    );
    write(&hiding.join("src/hiding.rs"), "// edited\n");

    // Git for Windows repaired this one: its admin dir names it by a Windows path.
    let by_windows = on_drive("by-windows");
    pushed_worktree(main, &by_windows, "by-windows");
    write(
        &main.join(".git/worktrees/harbor-notes-by-windows/gitdir"),
        "D:/harbor-notes-by-windows/.git\n",
    );

    let refactor = f
        .home
        .join("projects/.refract-worktrees/harbor-notes-20261101-0900");
    add_worktree(main, &refactor, "refactor/20261101-0900");

    let mut helper = f.spawn();
    for path in [&clean, &untracked, &holder] {
        let snapshot = helper.request("getRepo", json!({"path": path}));
        let worktree = &snapshot["result"]["worktree"];
        assert_eq!(worktree["status"], json!({"kind": "unchecked"}), "{path:?}");
        assert_eq!(worktree["removable"], false, "{path:?}");
        assert_eq!(worktree["checkable"], true, "{path:?}");
    }
    let snapshot = helper.request("getRepo", json!({"path": by_windows}));
    assert_eq!(
        snapshot["result"]["worktree"]["checkable"], false,
        "{snapshot}"
    );
    assert_eq!(
        helper.refused("checkWorktree", json!({"path": by_windows})),
        "This worktree was made by Git for Windows; remove it from Windows."
    );
    // The test prefix lists untracked files in the status; write this one after the
    // snapshot, as a Windows drive would not have shown it.
    write(&untracked.join("notes/todo.txt"), "todo\n");

    let check = helper.request("checkWorktree", json!({"path": clean}));
    assert_eq!(
        check["result"],
        json!({"status": {"kind": "pushed"}, "removable": true, "ignored": ["node_modules/"]}),
        "{check}"
    );
    let check = helper.request("checkWorktree", json!({"path": untracked}));
    assert_eq!(
        check["result"],
        json!({"status": {"kind": "changes", "count": 1}, "removable": false, "ignored": []}),
        "{check}"
    );
    let check = helper.request("checkWorktree", json!({"path": holder}));
    assert_eq!(
        check["result"],
        json!({"status": {"kind": "holdsRepo"}, "removable": false, "ignored": []}),
        "{check}"
    );
    let check = helper.request("checkWorktree", json!({"path": hiding}));
    assert_eq!(
        check["result"],
        json!({"status": {"kind": "changes", "count": 1}, "removable": false, "ignored": []}),
        "{check}"
    );
    assert_eq!(
        helper.refused("checkWorktree", json!({"path": main})),
        "Not a linked worktree."
    );
    assert_eq!(
        helper.refused("checkWorktree", json!({"path": refactor})),
        "Refract's own refactor worktrees are removed from the Refactor section."
    );
    // Checking changed nothing.
    assert!(untracked.join("notes/todo.txt").is_file());
    assert!(holder.join("scratch/vendored/.git").is_dir());

    // A clean worktree on the drive goes; its branch stays.
    helper.events.clear();
    let reply = helper.request("removeWorktree", json!({"path": clean}));
    assert_eq!(reply["result"], Value::Null, "{reply}");
    assert!(!clean.exists());
    assert!(branch_exists(main, "clean"));
    assert_eq!(
        helper.refused("removeWorktree", json!({"path": untracked})),
        "This worktree has uncommitted changes."
    );
    helper.shutdown();
}

#[test]
fn a_worktree_git_removed_halfway_is_dropped_and_said_so() {
    let f = DriveFixture::new();
    let stuck = f.drive.join("harbor-notes-stuck");
    pushed_worktree(&f.main, &stuck, "stuck");
    // A folder git cannot empty, as a Windows program holding a file open makes it.
    let held = stuck.join("target/held");
    write(&held.join("app.exe"), "bin\n");
    let mode = |dir: &Path, mode: u32| {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(dir, std::fs::Permissions::from_mode(mode)).unwrap();
    };
    mode(&held, 0o555);

    let mut helper = f.spawn();
    helper.events.clear();
    let message = helper.refused("removeWorktree", json!({"path": stuck}));
    mode(&held, 0o755);
    assert_eq!(
        message,
        format!(
            "Git removed the worktree, but a Windows program had files open, so some are \
             still in {}. Close it and delete the folder.",
            stuck.display()
        )
    );
    assert!(held.join("app.exe").is_file());
    assert!(branch_exists(&f.main, "stuck"));
    let paths = helper.paths();
    assert!(
        !paths.contains(&stuck.to_str().unwrap().to_owned()),
        "{paths:?}"
    );
    let removed = helper
        .events
        .iter()
        .any(|e| e["event"] == "repoRemoved" && e["data"]["path"] == stuck.to_str().unwrap());
    assert!(removed, "{:?}", helper.events);
    let main_changed = helper
        .events
        .iter()
        .any(|e| e["event"] == "repoChanged" && e["data"]["path"] == f.main.to_str().unwrap());
    assert!(main_changed, "{:?}", helper.events);
    helper.shutdown();
}

#[test]
fn missing_worktrees_are_listed_and_cleaned_up_only_when_nothing_is_lost() {
    let f = DriveFixture::new();
    let main = &f.main;
    let wt = |name: &str| f.root.join(format!("harbor-notes-{name}"));
    let delete = |path: &Path| std::fs::remove_dir_all(path).unwrap();

    let gone = wt("gone");
    pushed_worktree(main, &gone, "gone");
    delete(&gone);

    let locked = wt("locked");
    add_worktree(main, &locked, "locked");
    git(main, &["worktree", "lock", locked.to_str().unwrap()]);
    delete(&locked);

    let moved = wt("moved");
    add_worktree(main, &moved, "moved");
    let moved_to = f.root.join("spikes/harbor-notes-moved");
    std::fs::create_dir_all(moved_to.parent().unwrap()).unwrap();
    std::fs::rename(&moved, &moved_to).unwrap();

    let own_refs = wt("own-refs");
    add_worktree(main, &own_refs, "own-refs");
    git(&own_refs, &["update-ref", "refs/worktree/keep", "HEAD"]);
    delete(&own_refs);

    let lost = wt("lost");
    git(
        main,
        &["worktree", "add", "-q", "--detach", lost.to_str().unwrap()],
    );
    commit_file(&lost, "src/lost.rs", "// lost\n", "Add lost");
    delete(&lost);

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
    delete(&detached);

    let back = wt("back");
    pushed_worktree(main, &back, "back");
    delete(&back);

    let refactor = f
        .home
        .join("projects/.refract-worktrees/harbor-notes-20261101-0900");
    add_worktree(main, &refactor, "refactor/20261101-0900");
    delete(&refactor);

    let mut helper = f.spawn();
    let missing = helper.missing(main);
    let entry = |path: &Path| {
        missing
            .iter()
            .find(|m| m["path"] == path.to_str().unwrap())
            .unwrap_or_else(|| panic!("{path:?} not in {missing:?}"))
            .clone()
    };
    assert_eq!(
        entry(&gone),
        json!({"path": gone, "branch": "gone", "status": {"kind": "gone"}, "cleanable": true})
    );
    assert_eq!(entry(&locked)["status"], json!({"kind": "locked"}));
    assert_eq!(
        entry(&moved)["status"],
        json!({"kind": "moved", "to": moved_to})
    );
    assert_eq!(entry(&own_refs)["status"], json!({"kind": "ownRefs"}));
    assert_eq!(entry(&lost)["status"], json!({"kind": "commitsOnlyHere"}));
    assert!(entry(&lost).get("branch").is_none());
    assert_eq!(entry(&detached)["status"], json!({"kind": "gone"}));
    for path in [&locked, &moved, &own_refs, &lost] {
        assert_eq!(entry(path)["cleanable"], false, "{path:?}");
    }
    assert!(
        !missing
            .iter()
            .any(|m| m["path"].as_str().unwrap().contains(".refract-worktrees")),
        "{missing:?}"
    );
    assert_eq!(missing.len(), 7, "{missing:?}");

    let prune = |helper: &mut Helper, path: &Path| {
        helper.refused("pruneWorktree", json!({"repo": main, "path": path}))
    };
    assert_eq!(prune(&mut helper, &locked), "This worktree is locked.");
    assert_eq!(
        prune(&mut helper, &moved),
        format!(
            "This worktree was moved to {}; run `git worktree repair` there.",
            moved_to.display()
        )
    );
    assert_eq!(
        prune(&mut helper, &own_refs),
        "This worktree has refs of its own (such as a bisect), which would go with it."
    );
    assert_eq!(
        prune(&mut helper, &lost),
        "Its commits are on no branch, tag or remote, so they would be lost."
    );
    assert_eq!(
        prune(&mut helper, &refactor),
        "Refract's own refactor worktrees are removed from the Refactor section."
    );
    assert_eq!(
        prune(&mut helper, main),
        "Git does not list this worktree as missing."
    );
    assert_eq!(
        helper.refused("pruneWorktree", json!({"repo": moved_to, "path": gone})),
        "Not a main checkout Refract knows."
    );
    std::fs::create_dir_all(&back).unwrap();
    assert_eq!(prune(&mut helper, &back), "The folder is back.");
    // Nothing refused was changed.
    let admin = |name: &str| main.join(".git/worktrees").join(name);
    for name in [
        "harbor-notes-locked",
        "harbor-notes-moved",
        "harbor-notes-own-refs",
        "harbor-notes-lost",
        "harbor-notes-back",
    ] {
        assert!(admin(name).is_dir(), "{name}");
    }

    // Only git's record of a gone one goes: its branch and the other records stay.
    helper.events.clear();
    let reply = helper.request("pruneWorktree", json!({"repo": main, "path": gone}));
    assert_eq!(reply["result"], Value::Null, "{reply}");
    assert!(!admin("harbor-notes-gone").exists());
    assert!(branch_exists(main, "gone"));
    assert!(admin("harbor-notes-locked").is_dir());
    assert!(admin("harbor-notes-detached").is_dir());
    helper.paths();
    let sent = helper
        .events
        .iter()
        .find(|e| e["event"] == "repoChanged" && e["data"]["path"] == main.to_str().unwrap())
        .unwrap_or_else(|| panic!("no repoChanged for main in {:?}", helper.events));
    let listed: Vec<&str> = sent["data"]["missingWorktrees"]
        .as_array()
        .unwrap()
        .iter()
        .map(|m| m["path"].as_str().unwrap())
        .collect();
    assert!(!listed.contains(&gone.to_str().unwrap()), "{listed:?}");
    // The one whose folder is back is no longer listed as missing either.
    assert!(!listed.contains(&back.to_str().unwrap()), "{listed:?}");
    assert_eq!(listed.len(), 5, "{listed:?}");
    helper.shutdown();
}

#[test]
fn a_dropped_worktree_shows_up_as_missing() {
    let f = DriveFixture::new();
    let wide = f.root.join("harbor-notes-wide");
    pushed_worktree(&f.main, &wide, "wide");
    let mut helper = f.spawn();
    assert!(helper.missing(&f.main).is_empty());
    std::fs::remove_dir_all(&wide).unwrap();
    let deadline = Instant::now() + Duration::from_secs(20);
    loop {
        let missing = helper.missing(&f.main);
        if missing.iter().any(|m| m["path"] == wide.to_str().unwrap()) {
            break;
        }
        assert!(Instant::now() < deadline, "never listed: {missing:?}");
        std::thread::sleep(Duration::from_millis(200));
    }
    helper.shutdown();
}

/// A folder made unreadable (mode 000) until dropped, as an unreadable parent or a
/// network drive that stopped answering leaves a worktree's folder.
struct Unreadable(PathBuf);

impl Unreadable {
    fn new(dir: &Path) -> Unreadable {
        set_mode(dir, 0o000);
        Unreadable(dir.to_owned())
    }
}

impl Drop for Unreadable {
    fn drop(&mut self) {
        set_mode(&self.0, 0o755);
    }
}

fn set_mode(dir: &Path, mode: u32) {
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(dir, std::fs::Permissions::from_mode(mode)).unwrap();
}

#[test]
fn a_folder_that_cannot_be_looked_at_is_not_taken_for_gone() {
    let f = DriveFixture::new();
    let away = f.home.join("away");
    let wt = away.join("harbor-notes-away");
    pushed_worktree(&f.main, &wt, "away");
    let guard = Unreadable::new(&away);
    if std::fs::read_dir(&away).is_ok() {
        eprintln!("skipped: this user reads folders whatever their mode");
        return;
    }
    let mut helper = f.spawn();
    // Git lists it as prunable, but the folder may well be there.
    let missing = helper.missing(&f.main);
    assert!(missing.is_empty(), "{missing:?}");
    let message = helper.refused("pruneWorktree", json!({"repo": f.main, "path": wt}));
    assert!(
        message.starts_with("Can't tell whether the folder is there: "),
        "{message}"
    );
    drop(guard);
    assert!(f.main.join(".git/worktrees/harbor-notes-away").is_dir());
    assert!(wt.join(".git").is_file());
    helper.shutdown();
}

#[test]
fn a_removal_is_refused_when_ignored_files_appeared_since_the_confirm() {
    let f = DriveFixture::new();
    let built = f.root.join("harbor-notes-built");
    pushed_worktree(&f.main, &built, "built");
    write(&built.join("target/debug/app"), "bin\n");
    let many = f.root.join("harbor-notes-many");
    pushed_worktree(&f.main, &many, "many");
    write(&many.join("target/debug/app"), "bin\n");

    let mut helper = f.spawn();
    let snapshot = helper.request("getRepo", json!({"path": built}));
    assert_eq!(
        snapshot["result"]["worktree"]["ignored"],
        json!(["target/"]),
        "{snapshot}"
    );
    // Installed after the confirm showed the list.
    write(&built.join("node_modules/dep/index.js"), "// dep\n");
    helper.events.clear();
    assert_eq!(
        helper.refused(
            "removeWorktree",
            json!({"path": built, "ignored": ["target/"]})
        ),
        "New ignored files appeared since you confirmed (node_modules/). Review the list and \
         remove again."
    );
    assert!(built.join("node_modules/dep/index.js").is_file());
    // The status did not change, so the snapshot kept its ignored list; after the refusal
    // the worktree is read again and sent before the job's end, for the next confirm.
    helper.paths();
    let built_str = built.to_str().unwrap();
    let changed = helper
        .events
        .iter()
        .position(|e| e["event"] == "repoChanged" && e["data"]["path"] == built_str)
        .expect("the worktree is sent again after the refusal");
    let done = helper
        .events
        .iter()
        .position(|e| e["event"] == "worktreeJobDone" && e["data"]["path"] == built_str)
        .expect("the job ends with an event");
    assert!(changed < done, "{:?}", helper.events);
    let ignored = helper.events[changed]["data"]["worktree"]["ignored"].clone();
    assert_eq!(ignored, json!(["node_modules/", "target/"]), "{ignored}");
    let snapshot = helper.request("getRepo", json!({"path": built}));
    assert_eq!(snapshot["result"]["worktree"]["ignored"], ignored);
    let reply = helper.request("removeWorktree", json!({"path": built, "ignored": ignored}));
    assert_eq!(reply["result"], Value::Null, "{reply}");
    assert!(!built.exists());

    // A confirm of 9 entries said "and more", so nothing is compared.
    let shown: Vec<String> = (0..9).map(|n| format!("cache-{n}/")).collect();
    let reply = helper.request("removeWorktree", json!({"path": many, "ignored": shown}));
    assert_eq!(reply["result"], Value::Null, "{reply}");
    assert!(!many.exists());
    helper.shutdown();
}

#[test]
fn every_worktree_job_ends_with_an_event_saying_how_it_went() {
    let f = DriveFixture::new();
    let main = &f.main;
    let wt = |name: &str| f.root.join(format!("harbor-notes-{name}"));
    let ready = wt("ready");
    pushed_worktree(main, &ready, "ready");
    let dirty = wt("dirty");
    pushed_worktree(main, &dirty, "dirty");
    write(&dirty.join("notes/todo.txt"), "todo\n");
    let gone = wt("gone");
    pushed_worktree(main, &gone, "gone");
    std::fs::remove_dir_all(&gone).unwrap();
    let lost = wt("lost");
    git(
        main,
        &["worktree", "add", "-q", "--detach", lost.to_str().unwrap()],
    );
    commit_file(&lost, "src/lost.rs", "// lost\n", "Add lost");
    std::fs::remove_dir_all(&lost).unwrap();

    let mut helper = f.spawn();
    helper.events.clear();
    let reply = helper.request("removeWorktree", json!({"path": ready}));
    assert_eq!(reply["result"], Value::Null, "{reply}");
    // The event comes after the answer.
    assert!(
        !helper
            .events
            .iter()
            .any(|e| e["event"] == "worktreeJobDone"),
        "{:?}",
        helper.events
    );
    assert_eq!(
        helper.refused("removeWorktree", json!({"path": dirty})),
        "This worktree has uncommitted changes."
    );
    let reply = helper.request("pruneWorktree", json!({"repo": main, "path": gone}));
    assert_eq!(reply["result"], Value::Null, "{reply}");
    assert_eq!(
        helper.refused("pruneWorktree", json!({"repo": main, "path": lost})),
        "Its commits are on no branch, tag or remote, so they would be lost."
    );
    // Refused before any job ran: answered at once, with no event.
    assert_eq!(
        helper.refused("removeWorktree", json!({"path": main})),
        "Not a linked worktree."
    );
    helper.paths();
    let done: Vec<&Value> = helper
        .events
        .iter()
        .filter(|e| e["event"] == "worktreeJobDone")
        .map(|e| &e["data"])
        .collect();
    assert_eq!(
        done,
        [
            &json!({"path": ready}),
            &json!({"path": dirty, "error": "This worktree has uncommitted changes."}),
            &json!({"path": gone}),
            &json!({
                "path": lost,
                "error": "Its commits are on no branch, tag or remote, so they would be lost."
            }),
        ]
    );
    helper.shutdown();
}
