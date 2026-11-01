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
