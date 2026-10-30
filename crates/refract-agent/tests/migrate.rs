//! Moving a repo "on a Windows drive" into WSL, over stdin and stdout. The drive is a
//! temporary folder the helper treats as one ([`state::TEST_SLOW_PREFIX`]); the source
//! snapshot the app would write is built with
//! [`refract_agent::migrate::source::build`], Linux git standing in for `git.exe`.
//! Faults are injected through [`TEST_HOOK`], a script the helper runs at fixed points.

mod common;

use std::fs;
use std::io::{BufRead, BufReader, Write};
use std::os::unix::fs::PermissionsExt;
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStdin, Command, Stdio};
use std::sync::mpsc::{self, Receiver};
use std::time::{Duration, Instant};

use chrono::{TimeDelta, Utc};
use refract_agent::migrate::worker::TEST_HOOK;
use refract_agent::state;
use serde_json::{json, Value};

use common::*;

const SESSION_ID: &str = "5e2a7c10-0000-4000-8000-0000000000bb";

struct Agent {
    child: Child,
    stdin: ChildStdin,
    lines: Receiver<String>,
    next_id: u64,
}

impl Agent {
    fn spawn(home: &Path, config: &Path, drive: &Path, hook: Option<&Path>) -> Agent {
        let mut command = Command::new(env!("CARGO_BIN_EXE_refract-agent"));
        command
            .env_remove("XDG_STATE_HOME")
            .env_remove("XDG_DATA_HOME")
            .env_remove("XDG_CONFIG_HOME")
            .env_remove("CLAUDE_CONFIG_DIR")
            .env("HOME", home)
            .env("REFRACT_AGENT_CONFIG", config)
            .env(state::TEST_SLOW_PREFIX, drive)
            .envs(GIT_ENV.iter().copied())
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null());
        match hook {
            Some(hook) => command.env(TEST_HOOK, hook),
            None => command.env_remove(TEST_HOOK),
        };
        let mut child = command.spawn().expect("spawn agent");
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

    /// Sends a request and returns its id.
    fn send(&mut self, method: &str, params: Value) -> u64 {
        let id = self.next_id;
        self.next_id += 1;
        let message = json!({"id": id, "method": method, "params": params});
        writeln!(self.stdin, "{message}").unwrap();
        self.stdin.flush().unwrap();
        id
    }

    #[track_caller]
    fn next(&self, timeout: Duration) -> Value {
        let line = self
            .lines
            .recv_timeout(timeout)
            .expect("agent wrote nothing in time");
        serde_json::from_str(&line).unwrap_or_else(|e| panic!("not JSON: {line}: {e}"))
    }

    /// The response to `id`, skipping everything else.
    #[track_caller]
    fn response(&self, id: u64) -> Value {
        let deadline = Instant::now() + Duration::from_secs(60);
        loop {
            let message = self.next(deadline.saturating_duration_since(Instant::now()));
            if message["id"] == id {
                return message;
            }
        }
    }

    #[track_caller]
    fn request(&mut self, method: &str, params: Value) -> Value {
        let id = self.send(method, params);
        self.response(id)
    }

    /// Waits for `migrateReady` and returns the report.
    #[track_caller]
    fn ready(&self) -> Value {
        let deadline = Instant::now() + Duration::from_secs(120);
        loop {
            let message = self.next(deadline.saturating_duration_since(Instant::now()));
            if message["event"] == "migrateReady" {
                return message["data"]["report"].clone();
            }
        }
    }

    fn list_when_loaded(&mut self) -> Value {
        let first = self.request("listRepos", json!(null));
        if first["result"]["loading"] == false {
            return first;
        }
        let deadline = Instant::now() + Duration::from_secs(20);
        loop {
            let message = self.next(deadline.saturating_duration_since(Instant::now()));
            if message["event"] == "loadComplete" {
                break;
            }
        }
        self.request("listRepos", json!(null))
    }
}

impl Drop for Agent {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

/// A temporary world: a home with a Linux root, a "Windows drive", and the repo on it.
struct World {
    _tmp: tempfile::TempDir,
    tmp: PathBuf,
    home: PathBuf,
    root: PathBuf,
    drive: PathBuf,
    config: PathBuf,
    src: PathBuf,
    dest: PathBuf,
}

impl World {
    fn new() -> World {
        hermetic_git();
        let tmp_dir = tempfile::tempdir().unwrap();
        let tmp = tmp_dir.path().to_owned();
        let home = tmp.join("home");
        let root = home.join("projects");
        let drive = tmp.join("drive");
        fs::create_dir_all(&root).unwrap();
        fs::create_dir_all(home.join(".claude/projects")).unwrap();
        let src = drive.join("Desktop/aurora-notes");
        let config = tmp.join("agent.toml");
        write(
            &config,
            &format!(
                "roots = [{:?}]\nextra = [{:?}]\n",
                root.to_str().unwrap(),
                src.to_str().unwrap()
            ),
        );
        build_repo(&tmp, &src);
        World {
            _tmp: tmp_dir,
            dest: root.join("aurora-notes"),
            tmp,
            home,
            root,
            drive,
            config,
            src,
        }
    }

    fn agent(&self, hook: Option<&Path>) -> Agent {
        Agent::spawn(&self.home, &self.config, &self.drive, hook)
    }

    /// Writes the source snapshot as the app would, and returns its path.
    fn snapshot(&self) -> PathBuf {
        let snapshot = refract_agent::migrate::source::build(&self.src).unwrap();
        let path = self.tmp.join("snapshot.json");
        fs::write(&path, serde_json::to_vec(&snapshot).unwrap()).unwrap();
        path
    }

    fn migrate(&self, agent: &mut Agent, dest: &Path) -> Value {
        let snapshot = self.snapshot();
        agent.request(
            "migrate",
            json!({"path": self.src, "dest": dest, "snapshot": snapshot}),
        )
    }

    /// A hook script running `body` (sh, with `$1` the point, `$2` the round, `$3` the
    /// copy and `$4` the source).
    fn hook(&self, body: &str) -> PathBuf {
        let path = self.tmp.join("hook.sh");
        write(&path, body);
        path
    }

    fn projects(&self) -> PathBuf {
        self.home.join(".claude/projects")
    }

    fn migrations_dir(&self) -> PathBuf {
        self.home.join(".local/share/refract/migrations")
    }

    /// Leftover temporary copies next to the destination.
    fn temps(&self) -> Vec<PathBuf> {
        fs::read_dir(&self.root)
            .unwrap()
            .flatten()
            .map(|e| e.path())
            .filter(|p| {
                p.file_name()
                    .unwrap()
                    .to_string_lossy()
                    .starts_with(".refract-migrate-")
            })
            .collect()
    }
}

fn crlf(text: &str) -> String {
    text.replace('\n', "\r\n")
}

/// The repo as Windows git would have left it: CRLF working tree with
/// `core.autocrlf=true`, `core.filemode=false` and friends; a commit, a branch, a
/// stash, a submodule, an executable script, staged, unstaged, untracked and ignored
/// files, and a `node_modules` to skip.
fn build_repo(tmp: &Path, src: &Path) {
    let kelp = tmp.join("seeds/kelp");
    init_repo(&kelp);
    commit_file(&kelp, "lib.rs", "pub fn kelp() {}\n", "Add lib");

    fs::create_dir_all(src).unwrap();
    git(src, &["init", "-q", "-b", "main"]);
    for (key, value) in [
        ("core.autocrlf", "true"),
        ("core.filemode", "false"),
        ("core.symlinks", "false"),
        ("core.ignorecase", "true"),
    ] {
        git(src, &["config", key, value]);
    }
    write(
        &src.join("README.md"),
        &crlf("# aurora notes\n\nTide tables.\n"),
    );
    write(
        &src.join("src/main.rs"),
        &crlf("fn main() {\n    println!(\"hi\");\n}\n"),
    );
    write(&src.join("scripts/run.sh"), &crlf("#!/bin/sh\necho run\n"));
    write(&src.join(".gitignore"), &crlf("node_modules/\n.env\n"));
    write(&src.join("assets/logo.bin"), "\u{0}\u{1}\r\n\u{2}");
    git(src, &["add", "."]);
    git(src, &["update-index", "--chmod=+x", "scripts/run.sh"]);
    git(src, &["commit", "-q", "-m", "Initial commit"]);
    git(
        src,
        &[
            "-c",
            "protocol.file.allow=always",
            "submodule",
            "add",
            "-q",
            kelp.to_str().unwrap(),
            "libs/kelp",
        ],
    );
    git(src, &["commit", "-q", "-m", "Add kelp"]);
    git(src, &["branch", "feature"]);

    write(
        &src.join("README.md"),
        &crlf("# aurora notes\n\nStashed.\n"),
    );
    git(src, &["stash", "-q"]);
    write(
        &src.join("src/main.rs"),
        &crlf("fn main() {\n    println!(\"staged\");\n}\n"),
    );
    git(src, &["add", "src/main.rs"]);
    write(
        &src.join("README.md"),
        &crlf("# aurora notes\n\nUnstaged.\n"),
    );
    write(&src.join("notes.txt"), &crlf("untracked\n"));
    write(&src.join(".env"), "TOKEN=invented\n");
    write(
        &src.join("node_modules/left-pad/index.js"),
        "module.exports = 1;\n",
    );
}

fn check<'a>(report: &'a Value, name: &str) -> &'a Value {
    report["checks"]
        .as_array()
        .unwrap()
        .iter()
        .find(|c| c["check"] == name)
        .unwrap_or_else(|| panic!("no check {name}: {report:#}"))
}

/// True when the report allows deleting the original, as `Report::can_delete` says.
fn can_delete(report: &Value) -> bool {
    let report: refract_core::migrate::report::Report =
        serde_json::from_value(report.clone()).unwrap();
    report.can_delete()
}

/// `git status`, without refreshing the index (which would change the source).
fn status(dir: &Path, config: &[&str]) -> String {
    let mut args = vec!["--no-optional-locks"];
    args.extend(config);
    args.extend([
        "status",
        "--porcelain=v2",
        "--untracked-files=all",
        "--ignored=matching",
    ]);
    git(dir, &args)
}

#[test]
fn a_repo_moves_as_a_linux_checkout_and_finishing_deletes_the_old_history() {
    let world = World::new();
    let src = &world.src;
    let transcript_dir = project_dir(&world.projects(), src);
    write(
        &transcript_dir.join(format!("{SESSION_ID}.jsonl")),
        &transcript(
            SESSION_ID,
            src,
            &src.join("src/main.rs"),
            Utc::now() - TimeDelta::days(2),
            true,
        ),
    );
    let mut agent = world.agent(None);
    let ticket = world.migrate(&mut agent, &world.dest);
    let id = ticket["result"]["id"].as_str().expect("an id").to_owned();
    let report = agent.ready();
    assert!(can_delete(&report), "{report:#}");
    assert_eq!(report["id"], id.as_str());
    assert_eq!(check(&report, "history")["status"], "passed", "{report:#}");
    assert_eq!(report["skipped"][0]["path"], "node_modules");
    assert!(report["newObjects"].as_u64().unwrap() > 0);

    // A Linux checkout: LF, an executable script, no Windows-only config.
    let dest = &world.dest;
    assert!(world.temps().is_empty());
    assert_eq!(
        fs::read_to_string(dest.join("README.md")).unwrap(),
        "# aurora notes\n\nUnstaged.\n"
    );
    assert!(!fs::read_to_string(dest.join("src/main.rs"))
        .unwrap()
        .contains('\r'));
    assert_eq!(
        fs::read(dest.join("assets/logo.bin")).unwrap(),
        b"\0\x01\r\n\x02"
    );
    // Untracked and ignored files are copied as they are.
    assert_eq!(
        fs::read_to_string(dest.join("notes.txt")).unwrap(),
        "untracked\r\n"
    );
    assert_eq!(
        fs::read_to_string(dest.join(".env")).unwrap(),
        "TOKEN=invented\n"
    );
    assert!(!dest.join("node_modules").exists());
    let mode = |p: &str| fs::metadata(dest.join(p)).unwrap().permissions().mode() & 0o777;
    assert_eq!(mode("scripts/run.sh"), 0o755);
    assert_eq!(mode("README.md"), 0o644);
    assert_eq!(mode("src"), 0o755);
    assert_eq!(
        git(dest, &["config", "--local", "--get-regexp", "^core\\."])
            .lines()
            .filter(|l| l.contains("autocrlf")
                || l.contains("symlinks")
                || l.contains("ignorecase"))
            .count(),
        0
    );
    assert_eq!(git(dest, &["config", "core.filemode"]), "true");

    // Git sees the same repo: staged, unstaged, untracked and ignored, the stash, the
    // submodule.
    let windows = ["-c", "core.autocrlf=true", "-c", "core.filemode=false"];
    let expected = status(src, &windows).replace("\n! node_modules/", "");
    assert_eq!(status(dest, &[]), expected);
    assert!(status(dest, &[]).contains("1 M. "), "{}", status(dest, &[]));
    assert!(status(dest, &[]).contains("1 .M "), "{}", status(dest, &[]));
    assert_eq!(git(dest, &["stash", "list"]), git(src, &["stash", "list"]));
    assert_eq!(
        git(dest, &["rev-parse", "feature"]),
        git(src, &["rev-parse", "feature"])
    );
    assert_eq!(
        fs::read_to_string(dest.join("libs/kelp/lib.rs")).unwrap(),
        "pub fn kelp() {}\n"
    );
    assert_eq!(git(&dest.join("libs/kelp"), &["status", "--porcelain"]), "");

    // The report is saved; the history is copied with the new cwd, the old kept.
    let saved: Vec<PathBuf> = fs::read_dir(world.migrations_dir())
        .unwrap()
        .flatten()
        .map(|e| e.path())
        .filter(|p| p.extension().is_some_and(|e| e == "json"))
        .collect();
    assert_eq!(saved.len(), 1, "{saved:?}");
    let new_transcript = project_dir(&world.projects(), dest).join(format!("{SESSION_ID}.jsonl"));
    let moved = fs::read_to_string(&new_transcript).unwrap();
    assert!(moved.contains(&format!("\"cwd\":\"{}\"", dest.display())));
    assert!(!moved.contains(&format!("\"cwd\":\"{}\"", src.display())));
    assert!(transcript_dir.exists());

    // Pending until finished.
    let pending = agent.request("migratePending", json!(null));
    assert_eq!(pending["result"][0]["id"], id.as_str());

    // Nothing changed since: safe to delete.
    let recheck = agent.request("migrateRecheck", json!({"id": id}));
    assert_eq!(
        recheck["result"],
        json!({"changed": [], "removed": [], "unreadable": [], "copyChanged": [],
            "copyRemoved": [], "copyUnreadable": [], "copyMissing": false,
            "busyAgents": [], "sourceMissing": false}),
        "{recheck}"
    );
    write(&src.join("notes.txt"), "edited after the report\n");
    let recheck = agent.request("migrateRecheck", json!({"id": id}));
    assert_eq!(recheck["result"]["changed"], json!(["notes.txt"]));

    // Re-sync brings the edit over and sends a fresh report.
    let resync = agent.request("migrateResync", json!({"id": id}));
    assert_eq!(resync["result"], Value::Null, "{resync}");
    let report = agent.ready();
    assert!(can_delete(&report), "{report:#}");
    assert_eq!(
        fs::read_to_string(dest.join("notes.txt")).unwrap(),
        "edited after the report\n"
    );
    let recheck = agent.request("migrateRecheck", json!({"id": id}));
    assert_eq!(recheck["result"]["changed"], json!([]), "{recheck}");
    assert_eq!(recheck["result"]["copyChanged"], json!([]), "{recheck}");

    // Deleting needs migrateBeginDelete first.
    let early = agent.request("migrateFinish", json!({"id": id, "action": "delete"}));
    assert_eq!(early["error"]["code"], 6, "{early}");
    let begin = agent.request("migrateBeginDelete", json!({"id": id}));
    assert_eq!(begin["result"]["ok"], true, "{begin}");
    // The app sends the original to the Recycle Bin, then finishes.
    fs::remove_dir_all(src).unwrap();
    let finish = agent.request("migrateFinish", json!({"id": id, "action": "delete"}));
    assert_eq!(finish["result"], Value::Null, "{finish}");
    assert!(!transcript_dir.exists());
    assert!(new_transcript.exists());
    let config = fs::read_to_string(&world.config).unwrap();
    assert!(config.contains(dest.to_str().unwrap()), "{config}");
    assert!(!config.contains(src.to_str().unwrap()), "{config}");
    let pending = agent.request("migratePending", json!(null));
    assert_eq!(pending["result"], json!([]));
    let again = agent.request("migrateFinish", json!({"id": id, "action": "delete"}));
    assert_eq!(again["error"]["code"], 6);
}

#[test]
fn discard_deletes_the_copy_and_keep_hides_the_original() {
    let world = World::new();
    let src = &world.src;
    write(
        &project_dir(&world.projects(), src).join(format!("{SESSION_ID}.jsonl")),
        &transcript(SESSION_ID, src, &src.join("README.md"), Utc::now(), true),
    );
    let mut agent = world.agent(None);
    let id = world.migrate(&mut agent, &world.dest)["result"]["id"]
        .as_str()
        .unwrap()
        .to_owned();
    let report = agent.ready();
    assert!(can_delete(&report), "{report:#}");
    let new_history = project_dir(&world.projects(), &world.dest);
    assert!(new_history.exists());
    let discard = agent.request("migrateFinish", json!({"id": id, "action": "discard"}));
    assert_eq!(discard["result"], Value::Null, "{discard}");
    assert!(!world.dest.exists());
    assert!(!new_history.exists());
    assert!(project_dir(&world.projects(), src).exists());
    assert!(src.join(".git").exists());

    let dest = world.root.join("aurora-notes-2");
    let id = world.migrate(&mut agent, &dest)["result"]["id"]
        .as_str()
        .unwrap()
        .to_owned();
    let report = agent.ready();
    assert!(can_delete(&report), "{report:#}");
    let keep = agent.request("migrateFinish", json!({"id": id, "action": "keep"}));
    assert_eq!(keep["result"], Value::Null, "{keep}");
    assert!(dest.join(".git").exists() && src.join(".git").exists());
    let config = fs::read_to_string(&world.config).unwrap();
    assert!(config.contains("hidden"), "{config}");
    let parsed: toml::Value = toml::from_str(&config).unwrap();
    assert_eq!(parsed["hidden"][0].as_str(), src.to_str());
    // Both histories stay.
    assert!(project_dir(&world.projects(), src).exists());
    assert!(project_dir(&world.projects(), &dest).exists());
}

#[test]
fn a_placed_migration_comes_back_after_a_restart() {
    let world = World::new();
    let mut agent = world.agent(None);
    let id = world.migrate(&mut agent, &world.dest)["result"]["id"]
        .as_str()
        .unwrap()
        .to_owned();
    agent.ready();
    let bye = agent.request("shutdown", json!(null));
    assert_eq!(bye["result"], Value::Null);
    drop(agent);

    let mut agent = world.agent(None);
    let report = agent.ready();
    assert_eq!(report["id"], id.as_str());
    let pending = agent.request("migratePending", json!(null));
    assert_eq!(pending["result"][0]["id"], id.as_str());
    let recheck = agent.request("migrateRecheck", json!({"id": id}));
    assert_eq!(recheck["result"]["changed"], json!([]), "{recheck}");
    let discard = agent.request("migrateFinish", json!({"id": id, "action": "discard"}));
    assert_eq!(discard["result"], Value::Null, "{discard}");
    assert!(!world.dest.exists());
}

#[test]
fn leftover_temporary_copies_are_deleted_at_startup() {
    let world = World::new();
    let ours = world
        .root
        .join(".refract-migrate-aurora-notes-6f1c0000-0000-4000-8000-000000000001");
    let not_ours = world.root.join(".refract-migrate-notes");
    write(&ours.join("README.md"), "half a copy");
    write(&not_ours.join("keep.txt"), "not ours");
    let _agent = world.agent(None);
    let deadline = Instant::now() + Duration::from_secs(10);
    while ours.exists() && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(50));
    }
    assert!(!ours.exists());
    assert!(not_ours.join("keep.txt").exists());
}

#[test]
fn preflight_refusals_answer_migrate() {
    let world = World::new();
    let mut agent = world.agent(None);
    fs::create_dir_all(&world.dest).unwrap();
    let taken = world.migrate(&mut agent, &world.dest);
    assert_eq!(taken["error"]["code"], 6, "{taken}");
    assert!(taken["error"]["message"]
        .as_str()
        .unwrap()
        .contains("already exists"));
    let on_drive = world.migrate(&mut agent, &world.drive.join("elsewhere"));
    assert!(on_drive["error"]["message"]
        .as_str()
        .unwrap()
        .contains("isn't on the Linux filesystem"));
    write(
        &world.src.join(".git/MERGE_HEAD"),
        "0000000000000000000000000000000000000000\n",
    );
    let merging = world.migrate(&mut agent, &world.root.join("fresh"));
    assert!(
        merging["error"]["message"]
            .as_str()
            .unwrap()
            .contains("merge is in progress"),
        "{merging}"
    );
    let missing = agent.request(
        "migrate",
        json!({"path": world.src, "dest": world.root.join("x"), "snapshot": "/nonexistent.json"}),
    );
    assert_eq!(missing["error"]["code"], 6);
    assert!(world.temps().is_empty());
}

#[test]
fn cancel_deletes_the_temporary_copy() {
    let world = World::new();
    let reached = world.tmp.join("reached");
    let hold = world.tmp.join("hold");
    write(&hold, "");
    let hook = world.hook(&format!(
        "[ \"$1\" = copied ] || exit 0\ntouch {reached:?}\nwhile [ -e {hold:?} ]; do sleep 0.05; done\n"
    ));
    let mut agent = world.agent(Some(&hook));
    let id = world.migrate(&mut agent, &world.dest)["result"]["id"]
        .as_str()
        .unwrap()
        .to_owned();
    let deadline = Instant::now() + Duration::from_secs(60);
    while !reached.exists() && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(20));
    }
    assert_eq!(world.temps().len(), 1);
    let cancel = agent.send("migrateCancel", json!({"id": id}));
    fs::remove_file(&hold).unwrap();
    let answer = agent.response(cancel);
    assert_eq!(answer["result"], json!({"ended": true}), "{answer}");
    assert!(world.temps().is_empty());
    assert!(!world.dest.exists());
    let again = agent.request("migrateCancel", json!({"id": id}));
    assert_eq!(again["error"]["code"], 6);
}

/// Review 2 #8: a request sent right after a cancel's answer is answered (the
/// migration is forgotten before the cancel is answered).
#[test]
fn a_request_right_after_a_cancel_is_answered() {
    for _ in 0..5 {
        let world = World::new();
        let reached = world.tmp.join("reached");
        let hold = world.tmp.join("hold");
        write(&hold, "");
        let hook = world.hook(&format!(
            "[ \"$1\" = copied ] || exit 0\ntouch {reached:?}\nwhile [ -e {hold:?} ]; do sleep 0.05; done\n"
        ));
        let mut agent = world.agent(Some(&hook));
        let id = world.migrate(&mut agent, &world.dest)["result"]["id"]
            .as_str()
            .unwrap()
            .to_owned();
        wait_for(&reached);
        let cancel = agent.send("migrateCancel", json!({"id": id}));
        fs::remove_file(&hold).unwrap();
        agent.response(cancel);
        let again = agent.send("migrateCancel", json!({"id": id}));
        let answer = agent.response(again);
        assert_eq!(answer["error"]["code"], 6, "{answer}");
    }
}

fn wait_for(path: &Path) {
    let deadline = Instant::now() + Duration::from_secs(60);
    while !path.exists() && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(20));
    }
    assert!(path.exists(), "the hook never reached {}", path.display());
}

/// Runs a migration with `hook` and returns its report, which must not allow deleting.
fn failed_migration(world: &World, hook: Option<&Path>) -> Value {
    let mut agent = world.agent(hook);
    let ticket = world.migrate(&mut agent, &world.dest);
    assert!(ticket["result"]["id"].is_string(), "{ticket}");
    let report = agent.ready();
    assert!(!can_delete(&report), "{report:#}");
    // A failed copy is never placed, and its temporary folder is gone.
    assert!(!world.dest.exists());
    assert!(world.temps().is_empty());
    let keep = agent.request(
        "migrateFinish",
        json!({"id": report["id"], "action": "keep"}),
    );
    assert_eq!(keep["error"]["code"], 6);
    // Nothing to recheck or delete against a failed report.
    for method in ["migrateRecheck", "migrateBeginDelete"] {
        let refused = agent.request(method, json!({"id": report["id"]}));
        assert_eq!(refused["error"]["code"], 6, "{method}: {refused}");
    }
    let delete = agent.request(
        "migrateFinish",
        json!({"id": report["id"], "action": "delete"}),
    );
    assert_eq!(delete["error"]["code"], 6);
    report
}

#[test]
fn a_corrupted_byte_fails_every_file_matches() {
    let world = World::new();
    let hook = world.hook("[ \"$1\" = copied ] || exit 0\nprintf X >> \"$3/notes.txt\"\n");
    let report = failed_migration(&world, Some(&hook));
    let files = check(&report, "files");
    assert_eq!(files["status"], "failed", "{report:#}");
    assert_eq!(files["problems"][0]["path"], "notes.txt");
}

#[test]
fn a_dropped_ref_fails_git_state() {
    let world = World::new();
    let hook = world.hook("[ \"$1\" = copied ] || exit 0\nrm \"$3/.git/refs/heads/feature\"\n");
    let report = failed_migration(&world, Some(&hook));
    let git_state = check(&report, "gitState");
    assert_eq!(git_state["status"], "failed", "{report:#}");
    assert!(git_state["problems"]
        .as_array()
        .unwrap()
        .iter()
        .any(|p| p["path"] == "refs/heads/feature"));
    assert_eq!(check(&report, "nothingSkipped")["status"], "failed");
}

#[test]
fn an_unreadable_file_fails_nothing_skipped() {
    let world = World::new();
    let secret = world.src.join("secret.txt");
    write(&secret, "locked\n");
    let snapshot = world.snapshot();
    fs::set_permissions(&secret, fs::Permissions::from_mode(0o000)).unwrap();
    if fs::read(&secret).is_ok() {
        // Running as root: nothing is unreadable.
        return;
    }
    let mut agent = world.agent(None);
    let ticket = agent.request(
        "migrate",
        json!({"path": world.src, "dest": world.dest, "snapshot": snapshot}),
    );
    assert!(ticket["result"]["id"].is_string(), "{ticket}");
    let report = agent.ready();
    fs::set_permissions(&secret, fs::Permissions::from_mode(0o644)).unwrap();
    assert!(!can_delete(&report), "{report:#}");
    let skipped = check(&report, "nothingSkipped");
    assert_eq!(skipped["status"], "failed", "{report:#}");
    assert!(skipped["problems"]
        .as_array()
        .unwrap()
        .iter()
        .any(|p| p["path"] == "secret.txt"));
    assert!(!world.dest.exists());
}

#[test]
fn an_edit_during_the_copy_is_copied_again() {
    let world = World::new();
    let hook = world.hook(
        "[ \"$1\" = copied ] || exit 0\nprintf 'more\\n' >> \"$4/notes.txt\"\nprintf 'new\\n' > \"$4/later.txt\"\n",
    );
    let mut agent = world.agent(Some(&hook));
    world.migrate(&mut agent, &world.dest);
    let report = agent.ready();
    assert!(can_delete(&report), "{report:#}");
    assert_eq!(
        fs::read_to_string(world.dest.join("notes.txt")).unwrap(),
        "untracked\r\nmore\n"
    );
    assert_eq!(
        fs::read_to_string(world.dest.join("later.txt")).unwrap(),
        "new\n"
    );
    assert!(report["warnings"]
        .as_array()
        .unwrap()
        .iter()
        .any(|w| w.as_str().unwrap().contains("changed during the copy")));
}

#[test]
fn a_folder_that_keeps_changing_fails_check_4() {
    let world = World::new();
    let hook = world.hook("[ \"$1\" = verified ] || exit 0\nprintf 'x' >> \"$4/notes.txt\"\n");
    let report = failed_migration(&world, Some(&hook));
    let unchanged = check(&report, "unchanged");
    assert_eq!(unchanged["status"], "failed", "{report:#}");
    assert_eq!(unchanged["problems"][0]["path"], "notes.txt");
    assert!(unchanged["advice"]
        .as_str()
        .unwrap()
        .contains("keeps changing"));
}

#[test]
fn hooks_and_untracked_scripts_stay_executable() {
    let world = World::new();
    let src = &world.src;
    write(&src.join(".git/hooks/pre-commit"), "#!/bin/sh\nexit 0\n");
    write(&src.join(".git/hooks/pre-push.sample"), "#!/bin/sh\n");
    write(
        &src.join("tools/deploy"),
        "#!/usr/bin/env bash\necho deploy\n",
    );
    write(&src.join(".env.sh"), "#!/bin/sh\n");
    let mut agent = world.agent(None);
    world.migrate(&mut agent, &world.dest);
    let report = agent.ready();
    assert!(can_delete(&report), "{report:#}");
    let mode = |p: &str| {
        fs::metadata(world.dest.join(p))
            .unwrap()
            .permissions()
            .mode()
            & 0o777
    };
    assert_eq!(mode(".git/hooks/pre-commit"), 0o755);
    assert_eq!(mode(".git/hooks/pre-push.sample"), 0o644);
    assert_eq!(mode("tools/deploy"), 0o755);
    assert_eq!(mode(".env.sh"), 0o755, "an ignored script");
    assert_eq!(mode("notes.txt"), 0o644);
    // Tracked files take the index's mode.
    assert_eq!(mode("scripts/run.sh"), 0o755);
    assert_eq!(mode("README.md"), 0o644);
}

/// Marks `.gitignore` skip-worktree, `scripts/run.sh` assume-unchanged and `notes.txt`
/// intent-to-add in the source; all three are CRLF, so normalising rewrites them.
fn flag_index_entries(src: &Path) {
    git(src, &["update-index", "--skip-worktree", ".gitignore"]);
    git(
        src,
        &["update-index", "--assume-unchanged", "scripts/run.sh"],
    );
    git(src, &["add", "-N", "notes.txt"]);
}

#[test]
fn index_flags_and_intent_to_add_survive() {
    let world = World::new();
    flag_index_entries(&world.src);
    let mut agent = world.agent(None);
    world.migrate(&mut agent, &world.dest);
    let report = agent.ready();
    assert!(can_delete(&report), "{report:#}");
    let flags = |dir: &Path| {
        git(dir, &["ls-files", "-v"])
            .lines()
            .filter(|l| !l.starts_with("H "))
            .map(str::to_owned)
            .collect::<Vec<_>>()
    };
    assert_eq!(flags(&world.dest), ["S .gitignore", "h scripts/run.sh"]);
    assert_eq!(flags(&world.dest), flags(&world.src));
    let status = status(&world.dest, &[]);
    assert!(
        status
            .lines()
            .any(|l| l.starts_with("1 .A ") && l.ends_with(" notes.txt")),
        "{status}"
    );
    assert_eq!(
        fs::read_to_string(world.dest.join(".gitignore")).unwrap(),
        "node_modules/\n.env\n"
    );
}

#[test]
fn a_dropped_index_flag_fails_git_state() {
    let world = World::new();
    flag_index_entries(&world.src);
    let hook = world.hook(
        "[ \"$1\" = copied ] || exit 0\ngit -C \"$3\" update-index --no-skip-worktree .gitignore\n",
    );
    let report = failed_migration(&world, Some(&hook));
    let git_state = check(&report, "gitState");
    assert_eq!(git_state["status"], "failed", "{report:#}");
    let problems = git_state["problems"].as_array().unwrap();
    assert!(
        problems.iter().any(|p| p["path"] == ".gitignore"
            && p["message"].as_str().unwrap().contains("skip-worktree")),
        "{report:#}"
    );
}

#[test]
fn what_the_install_changes_is_listed_and_allowed_in_a_resync() {
    let world = World::new();
    let src = &world.src;
    write(&src.join("package.json"), "{\"name\": \"aurora-notes\"}\n");
    git(src, &["add", "package.json"]);
    git(
        src,
        &[
            "commit",
            "-q",
            "-m",
            "Add package.json",
            "--",
            "package.json",
        ],
    );
    // A fake npm, found through the login shell's profile, that writes a lockfile and
    // rewrites package.json as `npm install` can.
    let bin = world.home.join("bin");
    write(
        &bin.join("npm"),
        "#!/bin/sh\nmkdir -p node_modules/left-pad\nprintf '{}\\n' > package-lock.json\n\
         printf '{\"name\": \"aurora-notes\", \"version\": \"1.0.0\"}\\n' > package.json\n",
    );
    fs::set_permissions(bin.join("npm"), fs::Permissions::from_mode(0o755)).unwrap();
    write(
        &world.home.join(".profile"),
        &format!("export PATH=\"{}:$PATH\"\n", bin.display()),
    );

    let mut agent = world.agent(None);
    let ticket = world.migrate(&mut agent, &world.dest);
    let id = ticket["result"]["id"].as_str().expect("an id").to_owned();
    let report = agent.ready();
    assert!(can_delete(&report), "{report:#}");
    assert_eq!(report["install"][0]["status"], "succeeded", "{report:#}");
    let expected = json!({
        "created": ["package-lock.json"],
        "changed": ["package.json"],
        "removed": [],
    });
    assert_eq!(report["installChanges"], expected, "{report:#}");
    assert!(world.dest.join("node_modules/left-pad").is_dir());
    // The original's package.json, which npm rewrote in the copy, is backed up.
    let backup = PathBuf::from(report["installBackup"].as_str().expect("a backup"));
    assert!(backup.starts_with(world.migrations_dir()), "{backup:?}");
    assert_eq!(
        fs::read_to_string(backup.join("package.json")).unwrap(),
        "{\"name\": \"aurora-notes\"}\n"
    );
    assert!(!backup.join("package-lock.json").exists());
    // The copy as checked is the one after the install.
    let recheck = agent.request("migrateRecheck", json!({"id": id}));
    assert_eq!(recheck["result"]["copyChanged"], json!([]), "{recheck}");

    // A re-sync checks the copy again: the install's files are allowed, and listed.
    write(&src.join("notes.txt"), "edited after the report\n");
    let resync = agent.request("migrateResync", json!({"id": id}));
    assert_eq!(resync["result"], Value::Null, "{resync}");
    let report = agent.ready();
    assert!(can_delete(&report), "{report:#}");
    assert_eq!(report["installChanges"], expected, "{report:#}");
    assert!(fs::read_to_string(world.dest.join("package.json"))
        .unwrap()
        .contains("1.0.0"));

    // Once the original's package.json changes, its version is copied over npm's.
    write(
        &src.join("package.json"),
        "{\"name\": \"aurora-notes-2\"}\n",
    );
    agent.request("migrateResync", json!({"id": id}));
    let report = agent.ready();
    assert_eq!(report["installChanges"]["changed"], json!([]), "{report:#}");
    assert!(can_delete(&report), "{report:#}");
}

/// A process that looks like `claude --session-id <id>` running in `cwd`, until its
/// stdin closes or it is stopped.
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

#[test]
fn stop_agent_stops_an_idle_session_and_refuses_a_working_one() {
    let world = World::new();
    let repo = world.root.join("tidepool-api");
    init_repo(&repo);
    let idle = "5e2a7c10-0000-4000-8000-0000000000c1";
    let working = "5e2a7c10-0000-4000-8000-0000000000c2";
    let dir = project_dir(&world.projects(), &repo);
    let edited = repo.join("src/main.rs");
    let at = Utc::now() - TimeDelta::seconds(30);
    write(
        &dir.join(format!("{idle}.jsonl")),
        &transcript(idle, &repo, &edited, at, true),
    );
    write(
        &dir.join(format!("{working}.jsonl")),
        &transcript(working, &repo, &edited, at, false),
    );
    let mut idle_proc = fake_claude(&repo, idle);
    let mut working_proc = fake_claude(&repo, working);
    let mut agent = world.agent(None);
    agent.list_when_loaded();

    let busy = agent.request("stopAgent", json!({"path": repo, "sessionId": working}));
    assert_eq!(busy["error"]["code"], 7, "{busy}");
    assert!(working_proc.try_wait().unwrap().is_none());

    let stopped = agent.request("stopAgent", json!({"path": repo, "sessionId": idle}));
    assert_eq!(stopped["result"], Value::Null, "{stopped}");
    let deadline = Instant::now() + Duration::from_secs(5);
    while idle_proc.try_wait().unwrap().is_none() && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(20));
    }
    assert!(idle_proc.try_wait().unwrap().is_some());

    let gone = agent.request(
        "stopAgent",
        json!({"path": repo, "sessionId": "5e2a7c10-0000-4000-8000-0000000000c9"}),
    );
    assert_eq!(gone["error"]["code"], 5, "{gone}");
    let _ = working_proc.kill();
    let _ = working_proc.wait();
}

/// Starts a migration with `hook` and waits for its report, which must allow deleting.
fn placed_migration(world: &World, hook: Option<&Path>) -> (Agent, String) {
    let mut agent = world.agent(hook);
    let ticket = world.migrate(&mut agent, &world.dest);
    let id = ticket["result"]["id"].as_str().expect("an id").to_owned();
    let report = agent.ready();
    assert!(can_delete(&report), "{report:#}");
    (agent, id)
}

fn error_message(response: &Value) -> String {
    assert_eq!(response["error"]["code"], 6, "{response}");
    response["error"]["message"].as_str().unwrap().to_owned()
}

#[test]
fn a_file_a_clean_filter_strips_is_kept_byte_for_byte_and_blocks_deleting() {
    let world = World::new();
    let src = &world.src;
    git(src, &["config", "filter.strip.clean", "sed /^OUT/d"]);
    git(src, &["config", "filter.strip.smudge", "cat"]);
    write(&src.join(".gitattributes"), "*.nb filter=strip\n");
    write(&src.join("a.nb"), "cell\n");
    git(src, &["add", ".gitattributes", "a.nb"]);
    git(
        src,
        &["commit", "-q", "-m", "nb", "--", ".gitattributes", "a.nb"],
    );
    // Uncommitted outputs the clean filter strips; git on Windows sees no change.
    write(&src.join("a.nb"), "cell\r\nOUT precious result\r\n");
    git(src, &["add", "a.nb"]);
    let mut agent = world.agent(None);
    world.migrate(&mut agent, &world.dest);
    let report = agent.ready();
    assert!(!can_delete(&report), "{report:#}");
    let files = check(&report, "files");
    assert!(
        files["problems"].as_array().unwrap().iter().any(
            |p| p["path"] == "a.nb" && p["message"].as_str().unwrap().contains("byte for byte")
        ),
        "{report:#}"
    );
    assert_eq!(
        fs::read_to_string(src.join("a.nb")).unwrap(),
        "cell\r\nOUT precious result\r\n"
    );
}

#[test]
fn staging_during_the_copy_still_passes() {
    let world = World::new();
    let hook = world.hook(
        "[ \"$1\" = copied ] || exit 0\n\
         git -C \"$4\" -c core.autocrlf=true -c core.filemode=false add notes.txt\n",
    );
    let (_agent, _) = placed_migration(&world, Some(&hook));
    let status = status(&world.dest, &[]);
    assert!(
        status
            .lines()
            .any(|l| l.starts_with("1 A. ") && l.ends_with(" notes.txt")),
        "{status}"
    );
}

#[test]
fn a_resync_after_staging_passes() {
    let world = World::new();
    let (mut agent, id) = placed_migration(&world, None);
    git(&world.src, &["add", "notes.txt"]);
    let recheck = agent.request("migrateRecheck", json!({"id": id}));
    let changed = recheck["result"]["changed"].as_array().unwrap().clone();
    assert!(changed.contains(&json!(".git/index")), "{recheck}");
    for _ in 0..2 {
        let resync = agent.request("migrateResync", json!({"id": id}));
        assert_eq!(resync["result"], Value::Null, "{resync}");
        let report = agent.ready();
        assert!(can_delete(&report), "{report:#}");
    }
    let begin = agent.request("migrateBeginDelete", json!({"id": id}));
    assert_eq!(begin["result"]["ok"], true, "{begin}");
}

#[test]
fn a_copy_that_changed_or_went_is_never_trusted_or_deleted() {
    let world = World::new();
    let (mut agent, id) = placed_migration(&world, None);
    write(&world.dest.join("README.md"), "my own edit in the copy\n");
    let recheck = agent.request("migrateRecheck", json!({"id": id}));
    assert_eq!(
        recheck["result"]["copyChanged"],
        json!(["README.md"]),
        "{recheck}"
    );
    let begin = agent.request("migrateBeginDelete", json!({"id": id}));
    assert_eq!(begin["result"]["ok"], false, "{begin}");
    let resync = agent.request("migrateResync", json!({"id": id}));
    assert!(error_message(&resync).contains("changed since it was checked"));
    let discard = agent.request("migrateFinish", json!({"id": id, "action": "discard"}));
    assert!(error_message(&discard).contains("won't delete it"));
    assert!(world.dest.join("README.md").exists());
    // A refused re-sync leaves it ready.
    let pending = agent.request("migratePending", json!(null));
    assert_eq!(pending["result"][0]["id"], id.as_str(), "{pending}");

    // The copy deleted by hand: not deletable, not even after a restart.
    fs::remove_dir_all(&world.dest).unwrap();
    let recheck = agent.request("migrateRecheck", json!({"id": id}));
    assert_eq!(recheck["result"]["copyChanged"], json!(["."]), "{recheck}");
    drop(agent);
    let mut agent = world.agent(None);
    let report = agent.ready();
    assert!(!can_delete(&report), "{report:#}");
    assert!(check(&report, "files")["problems"][0]["message"]
        .as_str()
        .unwrap()
        .contains("is gone"));
    let begin = agent.request("migrateBeginDelete", json!({"id": id}));
    assert_eq!(begin["error"]["code"], 6, "{begin}");
    let delete = agent.request("migrateFinish", json!({"id": id, "action": "delete"}));
    assert_eq!(delete["error"]["code"], 6, "{delete}");
    // With no copy left, discarding only forgets it.
    let discard = agent.request("migrateFinish", json!({"id": id, "action": "discard"}));
    assert_eq!(discard["result"], Value::Null, "{discard}");
    assert!(world.src.join(".git").exists());
}

#[test]
fn history_moves_only_the_repos_sessions_and_keeps_what_changed() {
    use refract_agent::migrate::history;
    use refract_core::sessions::encode_project_dir;
    let tmp = tempfile::tempdir().unwrap();
    let projects = tmp.path().join("projects");
    let old = "/mnt/c/dev/app";
    let new = "/home/dev/projects/app";
    // `/mnt/c/dev/app/web` and the sibling repo `/mnt/c/dev/app-web` share a folder.
    let shared = projects.join(encode_project_dir("/mnt/c/dev/app/web"));
    assert_eq!(
        shared,
        projects.join(encode_project_dir("/mnt/c/dev/app-web"))
    );
    let line = |cwd: &str| format!("{{\"type\":\"user\",\"cwd\":\"{cwd}\"}}\n");
    write(&shared.join("s1.jsonl"), &line("/mnt/c/dev/app/web"));
    write(&shared.join("s1/tool-results/t.txt"), "output");
    write(&shared.join("s2.jsonl"), &line("/mnt/c/dev/app-web"));
    write(&shared.join("s2/tool-results/t.txt"), "sibling output");
    let top = projects.join(encode_project_dir(old));
    write(&top.join("s3.jsonl"), &line(old));
    write(&top.join("s4.jsonl"), &line(old));
    write(&top.join("memory/MEMORY.md"), "notes");
    let copied = history::copy(&projects, old, new, &mut |_, _| {});
    assert_eq!(copied.problems, vec![]);
    let new_top = projects.join(encode_project_dir(new));
    let new_web = projects.join(encode_project_dir(&format!("{new}/web")));
    assert!(new_web.join("s1.jsonl").exists() && new_web.join("s1/tool-results/t.txt").exists());
    assert!(!new_web.join("s2.jsonl").exists());
    assert!(new_top.join("memory/MEMORY.md").exists());
    // s3 goes on in the old path after the copy; s4 goes on in the new one.
    let append = |path: &Path, text: &str| {
        fs::OpenOptions::new()
            .append(true)
            .open(path)
            .unwrap()
            .write_all(text.as_bytes())
            .unwrap()
    };
    append(&top.join("s3.jsonl"), &line(old));
    append(&top.join("s4.jsonl"), &line(old));
    append(&new_top.join("s4.jsonl"), &line(new));
    let kept = history::remove_originals(&projects, &copied.files, old, new);
    assert!(shared.join("s2.jsonl").exists() && shared.join("s2/tool-results/t.txt").exists());
    assert!(!shared.join("s1.jsonl").exists());
    // s3's new lines reached its copy before it went; s4 changed on both sides and stays.
    assert!(!top.join("s3.jsonl").exists());
    let s3 = fs::read_to_string(new_top.join("s3.jsonl")).unwrap();
    assert_eq!(s3, line(new).repeat(2));
    assert_eq!(kept.len(), 1, "{kept:?}");
    assert!(kept[0].path.as_ref().unwrap().ends_with("s4.jsonl"));
    assert!(top.join("s4.jsonl").exists());
    // Discarding keeps a copy that was written to since.
    let kept = history::remove_copies(&projects, &copied.files);
    assert!(new_top.join("s4.jsonl").exists());
    assert!(kept
        .iter()
        .any(|k| k.path.as_ref().unwrap().ends_with("s4.jsonl")));
}

/// Review 2 #1: once the copy is placed, a cancel never deletes it, even with the
/// original deleted by hand and the copy edited meanwhile: it ends in a report.
#[test]
fn a_cancel_after_the_copy_is_placed_keeps_it_and_ends_in_a_report() {
    let world = World::new();
    let src = &world.src;
    write(
        &project_dir(&world.projects(), src).join(format!("{SESSION_ID}.jsonl")),
        &transcript(SESSION_ID, src, &src.join("README.md"), Utc::now(), true),
    );
    let reached = world.tmp.join("reached");
    let hold = world.tmp.join("hold");
    write(&hold, "");
    let hook = world.hook(&format!(
        "[ \"$1\" = moved ] || exit 0\necho mine > \"$3/my-work.txt\"\nrm -rf \"$4\"\ntouch {reached:?}\nwhile [ -e {hold:?} ]; do sleep 0.05; done\n"
    ));
    let mut agent = world.agent(Some(&hook));
    let id = world.migrate(&mut agent, &world.dest)["result"]["id"]
        .as_str()
        .unwrap()
        .to_owned();
    wait_for(&reached);
    assert!(!world.src.exists());
    let cancel = agent.send("migrateCancel", json!({"id": id}));
    fs::remove_file(&hold).unwrap();
    let report = agent.ready();
    let answer = agent.response(cancel);
    assert_eq!(answer["result"], json!({"ended": false}), "{answer}");
    assert_eq!(report["placed"], true, "{report:#}");
    assert!(world.dest.join("my-work.txt").exists());
    // The only copy left is refused to discard; it can be kept.
    let discard = agent.request("migrateFinish", json!({"id": id, "action": "discard"}));
    assert!(
        error_message(&discard).contains("only one left"),
        "{discard}"
    );
    let keep = agent.request("migrateFinish", json!({"id": id, "action": "keep"}));
    assert_eq!(keep["result"], Value::Null, "{keep}");
    assert!(world.dest.join("my-work.txt").exists());
}

/// Cancelled before the install and the history: both are skipped and reported, the
/// copy stays, and discarding it then leaves no transcript copies behind.
#[test]
fn a_cancel_before_the_install_skips_the_rest_and_discard_cleans_up() {
    let world = World::new();
    let src = &world.src;
    write(
        &project_dir(&world.projects(), src).join(format!("{SESSION_ID}.jsonl")),
        &transcript(SESSION_ID, src, &src.join("README.md"), Utc::now(), true),
    );
    let reached = world.tmp.join("reached");
    let hold = world.tmp.join("hold");
    write(&hold, "");
    let hook = world.hook(&format!(
        "[ \"$1\" = placed ] || exit 0\ntouch {reached:?}\nwhile [ -e {hold:?} ]; do sleep 0.05; done\n"
    ));
    let mut agent = world.agent(Some(&hook));
    let id = world.migrate(&mut agent, &world.dest)["result"]["id"]
        .as_str()
        .unwrap()
        .to_owned();
    wait_for(&reached);
    let cancel = agent.send("migrateCancel", json!({"id": id}));
    fs::remove_file(&hold).unwrap();
    let report = agent.ready();
    assert_eq!(agent.response(cancel)["result"], json!({"ended": false}));
    assert!(can_delete(&report), "{report:#}");
    assert_eq!(check(&report, "history")["status"], "failed", "{report:#}");
    assert!(!project_dir(&world.projects(), &world.dest).exists());
    let discard = agent.request("migrateFinish", json!({"id": id, "action": "discard"}));
    assert_eq!(discard["result"], Value::Null, "{discard}");
    assert!(!world.dest.exists());
    assert!(project_dir(&world.projects(), src).exists());
}

/// Review 2 minor: the helper stopping while the history moved (after the copies
/// were written, before they were saved) leaks no transcript copies: they are found
/// after the restart, and a discard deletes them.
#[test]
fn a_restart_while_history_moves_finds_the_copies() {
    let world = World::new();
    let src = &world.src;
    write(
        &project_dir(&world.projects(), src).join(format!("{SESSION_ID}.jsonl")),
        &transcript(SESSION_ID, src, &src.join("README.md"), Utc::now(), true),
    );
    let reached = world.tmp.join("reached");
    let hold = world.tmp.join("hold");
    write(&hold, "");
    let hook = world.hook(&format!(
        "[ \"$1\" = moved ] || exit 0\ntouch {reached:?}\nwhile [ -e {hold:?} ]; do sleep 0.05; done\n"
    ));
    let mut agent = world.agent(Some(&hook));
    let id = world.migrate(&mut agent, &world.dest)["result"]["id"]
        .as_str()
        .unwrap()
        .to_owned();
    wait_for(&reached);
    assert!(project_dir(&world.projects(), &world.dest).exists());
    drop(agent);
    fs::remove_file(&hold).unwrap();
    let mut agent = world.agent(None);
    let report = agent.ready();
    assert!(can_delete(&report), "{report:#}");
    assert_eq!(check(&report, "history")["status"], "failed", "{report:#}");
    assert!(check(&report, "history")["problems"][0]["message"]
        .as_str()
        .unwrap()
        .contains("didn't finish"));
    let discard = agent.request("migrateFinish", json!({"id": id, "action": "discard"}));
    assert_eq!(discard["result"], Value::Null, "{discard}");
    assert!(!world.dest.exists());
    assert!(!project_dir(&world.projects(), &world.dest).exists());
    assert!(project_dir(&world.projects(), src).exists());
}

/// Review 2 minor: the helper stopping during the install leaves a report that says
/// so, with Delete and Keep possible (not only Keep both). What changed in the copy
/// meanwhile is kept apart (review 3 A), so Discard waits until it is looked at.
#[test]
fn a_restart_during_the_install_reports_it_cut_short() {
    let world = World::new();
    let reached = world.tmp.join("reached");
    let hold = world.tmp.join("hold");
    write(&hold, "");
    // What an install writes outside the build folders.
    let hook = world.hook(&format!(
        "[ \"$1\" = placed ] || exit 0\necho lock > \"$3/package-lock.json\"\ntouch {reached:?}\nwhile [ -e {hold:?} ]; do sleep 0.05; done\n"
    ));
    let mut agent = world.agent(Some(&hook));
    let id = world.migrate(&mut agent, &world.dest)["result"]["id"]
        .as_str()
        .unwrap()
        .to_owned();
    wait_for(&reached);
    drop(agent);
    fs::remove_file(&hold).unwrap();
    let mut agent = world.agent(None);
    let report = agent.ready();
    assert!(can_delete(&report), "{report:#}");
    assert_eq!(
        report["recoveredChanges"]["created"],
        json!(["package-lock.json"]),
        "{report:#}"
    );
    assert!(report["warnings"]
        .as_array()
        .unwrap()
        .iter()
        .any(|w| w.as_str().unwrap().contains("install didn't finish")));
    let recheck = agent.request("migrateRecheck", json!({"id": id}));
    assert_eq!(recheck["result"]["copyChanged"], json!([]), "{recheck}");
    let discard = agent.request("migrateFinish", json!({"id": id, "action": "discard"}));
    assert!(
        error_message(&discard).contains("package-lock.json"),
        "{discard}"
    );
    let keep = agent.request("migrateFinish", json!({"id": id, "action": "keep"}));
    assert_eq!(keep["result"], Value::Null, "{keep}");
    assert!(world.dest.join("package-lock.json").exists());
}

#[test]
fn a_submodule_pointing_outside_the_copy_stops_it_untouched() {
    let world = World::new();
    let module = world.src.join(".git/modules/libs/kelp");
    let before = (
        fs::read(module.join("index")).unwrap(),
        fs::read(module.join("config")).unwrap(),
    );
    let hook = world.hook(
        "[ \"$1\" = copied ] || exit 0\n\
         printf 'gitdir: %s/.git/modules/libs/kelp\\n' \"$4\" > \"$3/libs/kelp/.git\"\n",
    );
    let report = failed_migration(&world, Some(&hook));
    let skipped = check(&report, "nothingSkipped");
    assert!(
        skipped["problems"][0]["message"]
            .as_str()
            .unwrap()
            .contains("outside the repo"),
        "{report:#}"
    );
    let after = (
        fs::read(module.join("index")).unwrap(),
        fs::read(module.join("config")).unwrap(),
    );
    assert!(
        before == after,
        "the original's submodule git folder changed"
    );
}

#[test]
fn a_destination_reaching_the_drive_through_a_link_is_refused() {
    let world = World::new();
    std::os::unix::fs::symlink(&world.drive, world.root.join("drive-link")).unwrap();
    let mut agent = world.agent(None);
    let refused = world.migrate(&mut agent, &world.root.join("drive-link/aurora-notes"));
    assert!(
        error_message(&refused).contains("isn't on the Linux filesystem"),
        "{refused}"
    );
    let dotted = world.migrate(&mut agent, &world.root.join("x/../aurora-notes"));
    assert_eq!(dotted["error"]["code"], 6, "{dotted}");
}

#[test]
fn nested_migrations_do_not_run_together() {
    let world = World::new();
    let hold = world.tmp.join("hold");
    write(&hold, "");
    let hook = world.hook(&format!(
        "[ \"$1\" = copied ] || exit 0\nwhile [ -e {hold:?} ]; do sleep 0.05; done\n"
    ));
    let mut agent = world.agent(Some(&hook));
    let first = world.migrate(&mut agent, &world.dest);
    let id = first["result"]["id"].as_str().expect("an id").to_owned();
    let snapshot = world.snapshot();
    for (path, dest) in [
        (world.src.join("src"), world.root.join("elsewhere")),
        (world.drive.join("Desktop"), world.root.join("elsewhere")),
        (world.drive.join("other"), world.dest.join("inner")),
    ] {
        let refused = agent.request(
            "migrate",
            json!({"path": path, "dest": dest, "snapshot": snapshot}),
        );
        assert!(
            error_message(&refused).contains("Another migration"),
            "{refused}"
        );
    }
    let cancel = agent.send("migrateCancel", json!({"id": id}));
    fs::remove_file(&hold).unwrap();
    agent.response(cancel);
}

#[test]
fn an_unreadable_folder_in_the_original_is_never_taken_for_removed() {
    let world = World::new();
    let docs = world.src.join("docs");
    write(&docs.join("plan.md"), "the plan\n");
    let (mut agent, id) = placed_migration(&world, None);
    fs::set_permissions(&docs, fs::Permissions::from_mode(0o000)).unwrap();
    if fs::read_dir(&docs).is_ok() {
        // Running as root: nothing is unreadable.
        fs::set_permissions(&docs, fs::Permissions::from_mode(0o755)).unwrap();
        return;
    }
    let recheck = agent.request("migrateRecheck", json!({"id": id}));
    let begin = agent.request("migrateBeginDelete", json!({"id": id}));
    let resync = agent.request("migrateResync", json!({"id": id}));
    let report = agent.ready();
    fs::set_permissions(&docs, fs::Permissions::from_mode(0o755)).unwrap();
    assert_eq!(
        recheck["result"]["unreadable"],
        json!(["docs"]),
        "{recheck}"
    );
    assert_eq!(recheck["result"]["removed"], json!([]), "{recheck}");
    assert_eq!(recheck["result"]["changed"], json!([]), "{recheck}");
    assert_eq!(begin["result"]["ok"], false, "{begin}");
    assert_eq!(resync["result"], Value::Null, "{resync}");
    assert!(!can_delete(&report), "{report:#}");
    assert_eq!(check(&report, "unchanged")["problems"][0]["path"], "docs");
    assert_eq!(
        fs::read_to_string(world.dest.join("docs/plan.md")).unwrap(),
        "the plan\n"
    );
}

#[test]
fn a_missing_original_leaves_only_delete() {
    let world = World::new();
    let (mut agent, id) = placed_migration(&world, None);
    let away = world.tmp.join("away");
    fs::rename(&world.src, &away).unwrap();
    let recheck = agent.request("migrateRecheck", json!({"id": id}));
    assert_eq!(recheck["result"]["sourceMissing"], true, "{recheck}");
    assert_eq!(recheck["result"]["removed"], json!([]), "{recheck}");
    let resync = agent.request("migrateResync", json!({"id": id}));
    assert!(error_message(&resync).contains("is gone"));
    let discard = agent.request("migrateFinish", json!({"id": id, "action": "discard"}));
    assert!(error_message(&discard).contains("only one left"));
    assert!(world.dest.join(".git").exists());
    // Unless the app says Windows doesn't see it either, it may be a hidden drive.
    let begin = agent.request("migrateBeginDelete", json!({"id": id}));
    assert_eq!(begin["result"]["ok"], false, "{begin}");
    let begin = agent.request("migrateBeginDelete", json!({"id": id, "sourceGone": true}));
    assert_eq!(begin["result"]["ok"], true, "{begin}");
    let finish = agent.request("migrateFinish", json!({"id": id, "action": "delete"}));
    assert_eq!(finish["result"], Value::Null, "{finish}");
    assert!(world.dest.join(".git").exists());
}

#[test]
fn deleting_is_saved_and_refuses_everything_but_finishing() {
    let world = World::new();
    let (mut agent, id) = placed_migration(&world, None);
    let begin = agent.request("migrateBeginDelete", json!({"id": id}));
    assert_eq!(begin["result"]["ok"], true, "{begin}");
    let refusals = |agent: &mut Agent| {
        for (method, params) in [
            ("migrateResync", json!({"id": id})),
            ("migrateCancel", json!({"id": id})),
            ("migrateFinish", json!({"id": id, "action": "keep"})),
            ("migrateFinish", json!({"id": id, "action": "discard"})),
        ] {
            let refused = agent.request(method, params);
            assert_eq!(
                error_message(&refused),
                "The original is being deleted.",
                "{method}"
            );
        }
        let pending = agent.request("migratePending", json!(null));
        assert_eq!(pending["result"][0]["deleting"], true, "{pending}");
    };
    refusals(&mut agent);
    drop(agent);
    let mut agent = world.agent(None);
    agent.ready();
    refusals(&mut agent);
    // Part of the original is gone: the rest may still go.
    fs::remove_file(world.src.join("notes.txt")).unwrap();
    fs::remove_dir_all(world.src.join("src")).unwrap();
    let again = agent.request("migrateBeginDelete", json!({"id": id}));
    assert_eq!(again["result"]["ok"], true, "{again}");
    let removed = again["result"]["recheck"]["removed"].as_array().unwrap();
    assert!(removed.contains(&json!("notes.txt")) && removed.contains(&json!("src/main.rs")));
    assert_eq!(again["result"]["recheck"]["changed"], json!([]), "{again}");
    fs::remove_dir_all(&world.src).unwrap();
    let finish = agent.request("migrateFinish", json!({"id": id, "action": "delete"}));
    assert_eq!(finish["result"], Value::Null, "{finish}");
    assert_eq!(
        fs::read_to_string(world.dest.join("notes.txt")).unwrap(),
        "untracked\r\n"
    );
}

/// Review 2 #2: while deleting, the copy in use (a build file added, a file changed
/// whose original is already gone) doesn't block the rest; a change to a path the
/// original still has does, unless forced, and the copy keeps its version.
#[test]
fn deleting_ignores_what_the_rest_cannot_lose_and_force_deletes_the_rest() {
    let world = World::new();
    let (mut agent, id) = placed_migration(&world, None);
    let begin = agent.request("migrateBeginDelete", json!({"id": id}));
    assert_eq!(begin["result"]["ok"], true, "{begin}");
    // A partial removal, then the user works in the copy.
    fs::remove_dir_all(world.src.join("src")).unwrap();
    write(&world.dest.join("tsconfig.tsbuildinfo"), "{}\n");
    write(
        &world.dest.join("src/main.rs"),
        "fn main() { /* mine */ }\n",
    );
    let again = agent.request("migrateBeginDelete", json!({"id": id}));
    assert_eq!(again["result"]["ok"], true, "{again}");
    assert_eq!(
        again["result"]["recheck"]["copyChanged"],
        json!([]),
        "{again}"
    );
    // A file the original still has, changed in the copy: blocks, and is listed.
    write(&world.dest.join("README.md"), "my edit in the copy\n");
    let blocked = agent.request("migrateBeginDelete", json!({"id": id}));
    assert_eq!(blocked["result"]["ok"], false, "{blocked}");
    assert_eq!(
        blocked["result"]["recheck"]["copyChanged"],
        json!(["README.md"]),
        "{blocked}"
    );
    let forced = agent.request(
        "migrateBeginDelete",
        json!({"id": id, "force": {"changed": ["README.md"]}}),
    );
    assert_eq!(forced["result"]["ok"], true, "{forced}");
    fs::remove_dir_all(&world.src).unwrap();
    let finish = agent.request("migrateFinish", json!({"id": id, "action": "delete"}));
    assert_eq!(finish["result"], Value::Null, "{finish}");
    assert_eq!(
        fs::read_to_string(world.dest.join("README.md")).unwrap(),
        "my edit in the copy\n"
    );
}

/// `force` never enters the deleting state, and never skips an original that
/// changed.
#[test]
fn force_only_skips_copy_changes_while_deleting() {
    let world = World::new();
    let (mut agent, id) = placed_migration(&world, None);
    write(&world.dest.join("README.md"), "my edit in the copy\n");
    let forced = agent.request(
        "migrateBeginDelete",
        json!({"id": id, "force": {"changed": ["README.md", "notes.txt", "."], "removed": ["README.md", "notes.txt", "."]}}),
    );
    assert_eq!(forced["result"]["ok"], false, "{forced}");
    write(
        &world.dest.join("README.md"),
        &fs::read_to_string(world.src.join("README.md")).unwrap(),
    );
    let keep = agent.request("migrateFinish", json!({"id": id, "action": "keep"}));
    assert_eq!(keep["result"], Value::Null, "{keep}");

    let world = World::new();
    let (mut agent, id) = placed_migration(&world, None);
    let begin = agent.request("migrateBeginDelete", json!({"id": id}));
    assert_eq!(begin["result"]["ok"], true, "{begin}");
    std::thread::sleep(Duration::from_millis(20));
    write(&world.src.join("notes.txt"), "edited while deleting\n");
    let forced = agent.request(
        "migrateBeginDelete",
        json!({"id": id, "force": {"changed": ["README.md", "notes.txt", "."], "removed": ["README.md", "notes.txt", "."]}}),
    );
    assert_eq!(forced["result"]["ok"], false, "{forced}");
    assert_eq!(
        forced["result"]["recheck"]["changed"],
        json!(["notes.txt"]),
        "{forced}"
    );
}

/// Review 2 #3: a drive WSL can't see never enters the deleting state unless the
/// app says Windows doesn't see the original either; the move stays answerable.
#[test]
fn a_hidden_drive_never_enters_deleting() {
    let world = World::new();
    let (mut agent, id) = placed_migration(&world, None);
    let away = world.tmp.join("away");
    fs::rename(&world.src, &away).unwrap();
    let begin = agent.request("migrateBeginDelete", json!({"id": id, "sourceGone": false}));
    assert_eq!(begin["result"]["ok"], false, "{begin}");
    assert_eq!(begin["result"]["recheck"]["sourceMissing"], true, "{begin}");
    let pending = agent.request("migratePending", json!(null));
    assert_eq!(pending["result"][0]["deleting"], false, "{pending}");
    // Back, with an edit made while WSL couldn't see it: the recheck catches it.
    fs::rename(&away, &world.src).unwrap();
    std::thread::sleep(Duration::from_millis(20));
    write(
        &world.src.join("notes.txt"),
        "edited on Windows meanwhile\n",
    );
    let again = agent.request("migrateBeginDelete", json!({"id": id}));
    assert_eq!(again["result"]["ok"], false, "{again}");
    assert_eq!(
        again["result"]["recheck"]["changed"],
        json!(["notes.txt"]),
        "{again}"
    );
    let keep = agent.request("migrateFinish", json!({"id": id, "action": "keep"}));
    assert_eq!(keep["result"], Value::Null, "{keep}");
}

/// Review 2 #4: with the copy gone, discarding only forgets the migration, whether
/// the original is gone too or the delete had begun; nothing else is deleted.
#[test]
fn a_copy_that_is_gone_can_always_be_forgotten() {
    // Not deleting, the original gone too.
    let world = World::new();
    let (mut agent, id) = placed_migration(&world, None);
    let away = world.tmp.join("away");
    fs::rename(&world.src, &away).unwrap();
    fs::remove_dir_all(&world.dest).unwrap();
    let pending = agent.request("migratePending", json!(null));
    assert_eq!(pending["result"][0]["copyMissing"], true, "{pending}");
    let keep = agent.request("migrateFinish", json!({"id": id, "action": "keep"}));
    assert!(error_message(&keep).contains("nothing to keep"), "{keep}");
    let discard = agent.request("migrateFinish", json!({"id": id, "action": "discard"}));
    assert_eq!(discard["result"], Value::Null, "{discard}");
    assert!(away.join(".git").exists());
    let pending = agent.request("migratePending", json!(null));
    assert_eq!(pending["result"], json!([]), "{pending}");

    // Deleting, then the copy deleted by hand: the rest of the original is never
    // deleted, even forced, and the move can be ended.
    let world = World::new();
    let (mut agent, id) = placed_migration(&world, None);
    let begin = agent.request("migrateBeginDelete", json!({"id": id}));
    assert_eq!(begin["result"]["ok"], true, "{begin}");
    fs::remove_dir_all(&world.dest).unwrap();
    let forced = agent.request(
        "migrateBeginDelete",
        json!({"id": id, "force": {"changed": ["README.md", "notes.txt", "."], "removed": ["README.md", "notes.txt", "."]}}),
    );
    assert_eq!(forced["result"]["ok"], false, "{forced}");
    assert_eq!(
        forced["result"]["recheck"]["copyChanged"],
        json!(["."]),
        "{forced}"
    );
    let discard = agent.request("migrateFinish", json!({"id": id, "action": "discard"}));
    assert_eq!(discard["result"], Value::Null, "{discard}");
    assert!(world.src.join(".git").exists());
}

/// Review 2 #5: a file written to the copy while a re-sync runs is not taken for the
/// re-sync's, so a discard doesn't delete it.
#[test]
fn an_edit_in_the_copy_during_a_resync_is_not_the_resyncs() {
    let world = World::new();
    let dest = world.dest.to_str().unwrap();
    let hook = world.hook(&format!(
        "[ \"$1\" = verified ] && [ \"$3\" = {dest:?} ] || exit 0\necho mine > \"$3/mine.txt\"\n"
    ));
    let (mut agent, id) = placed_migration(&world, Some(&hook));
    std::thread::sleep(Duration::from_millis(20));
    write(&world.src.join("notes.txt"), "edited after the report\n");
    let resync = agent.request("migrateResync", json!({"id": id}));
    assert_eq!(resync["result"], Value::Null, "{resync}");
    agent.ready();
    let recheck = agent.request("migrateRecheck", json!({"id": id}));
    assert_eq!(
        recheck["result"]["copyChanged"],
        json!(["mine.txt"]),
        "{recheck}"
    );
    let discard = agent.request("migrateFinish", json!({"id": id, "action": "discard"}));
    assert!(
        error_message(&discard).contains("won't delete it"),
        "{discard}"
    );
    assert!(world.dest.join("mine.txt").exists());
}

/// Review 2 #6 and minor: a re-sync cut short by a restart, or stopped by a cancel,
/// is reported as such; re-syncing again finishes it.
#[test]
fn a_resync_cut_short_is_reported_and_can_be_run_again() {
    let world = World::new();
    let dest = world.dest.to_str().unwrap();
    let reached = world.tmp.join("reached");
    let hold = world.tmp.join("hold");
    let hook = world.hook(&format!(
        "[ \"$1\" = verified ] && [ \"$3\" = {dest:?} ] && [ -e {hold:?} ] || exit 0\ntouch {reached:?}\nwhile [ -e {hold:?} ]; do sleep 0.05; done\n"
    ));
    let (mut agent, id) = placed_migration(&world, Some(&hook));
    std::thread::sleep(Duration::from_millis(20));
    write(&world.src.join("notes.txt"), "edited after the report\n");

    // Cancelled.
    write(&hold, "");
    let resync = agent.request("migrateResync", json!({"id": id}));
    assert_eq!(resync["result"], Value::Null, "{resync}");
    wait_for(&reached);
    let cancel = agent.send("migrateCancel", json!({"id": id}));
    fs::remove_file(&hold).unwrap();
    let report = agent.ready();
    assert_eq!(agent.response(cancel)["result"], json!({"ended": false}));
    assert!(!can_delete(&report), "{report:#}");
    assert!(check(&report, "unchanged")["problems"][0]["message"]
        .as_str()
        .unwrap()
        .contains("You stopped the re-sync"));

    // Cut short by a restart.
    fs::remove_file(&reached).unwrap();
    write(&hold, "");
    let resync = agent.request("migrateResync", json!({"id": id}));
    assert_eq!(resync["result"], Value::Null, "{resync}");
    wait_for(&reached);
    drop(agent);
    fs::remove_file(&hold).unwrap();
    let mut agent = world.agent(Some(&hook));
    let report = agent.ready();
    assert!(!can_delete(&report), "{report:#}");
    assert!(check(&report, "unchanged")["problems"][0]["message"]
        .as_str()
        .unwrap()
        .contains("didn't finish"));
    let recheck = agent.request("migrateRecheck", json!({"id": id}));
    assert_eq!(recheck["error"]["code"], 6, "{recheck}");
    let resync = agent.request("migrateResync", json!({"id": id}));
    assert_eq!(resync["result"], Value::Null, "{resync}");
    let report = agent.ready();
    assert!(can_delete(&report), "{report:#}");
    assert_eq!(
        fs::read_to_string(world.dest.join("notes.txt")).unwrap(),
        "edited after the report\n"
    );
    let begin = agent.request("migrateBeginDelete", json!({"id": id}));
    assert_eq!(begin["result"]["ok"], true, "{begin}");
}

/// Review 2 #9: when the config can't be written, the migration stays, and the same
/// finish again goes through once it can.
#[test]
fn a_finish_whose_config_fails_can_be_sent_again() {
    let world = World::new();
    let (mut agent, id) = placed_migration(&world, None);
    let saved = fs::read_to_string(&world.config).unwrap();
    fs::remove_file(&world.config).unwrap();
    fs::create_dir(&world.config).unwrap();
    let keep = agent.request("migrateFinish", json!({"id": id, "action": "keep"}));
    assert_eq!(keep["error"]["code"], 6, "{keep}");
    let pending = agent.request("migratePending", json!(null));
    assert_eq!(pending["result"][0]["id"], id.as_str(), "{pending}");
    fs::remove_dir(&world.config).unwrap();
    write(&world.config, &saved);
    let keep = agent.request("migrateFinish", json!({"id": id, "action": "keep"}));
    assert_eq!(keep["result"], Value::Null, "{keep}");
    assert!(fs::read_to_string(&world.config)
        .unwrap()
        .contains("hidden"));
    let pending = agent.request("migratePending", json!(null));
    assert_eq!(pending["result"], json!([]), "{pending}");
    assert!(world.dest.exists());
}

/// Review 3 A: the helper stops during the install and the user edits the copy before
/// it restarts. What changed is kept apart from the install's (it may be the user's
/// work), backed up from the original, and never discarded or re-synced over; the
/// original may still go, and both may be kept.
#[test]
fn edits_after_an_interrupted_install_are_kept_apart_and_never_discarded() {
    let world = World::new();
    let reached = world.tmp.join("reached");
    let hold = world.tmp.join("hold");
    write(&hold, "");
    let hook = world.hook(&format!(
        "[ \"$1\" = placed ] || exit 0\ntouch {reached:?}\nwhile [ -e {hold:?} ]; do sleep 0.05; done\n"
    ));
    let mut agent = world.agent(Some(&hook));
    let id = world.migrate(&mut agent, &world.dest)["result"]["id"]
        .as_str()
        .unwrap()
        .to_owned();
    wait_for(&reached);
    drop(agent);
    fs::remove_file(&hold).unwrap();
    let readme = fs::read_to_string(world.src.join("README.md")).unwrap();
    write(&world.dest.join("my-notes.md"), "work done in the copy\n");
    write(&world.dest.join("README.md"), "edited in the copy\n");
    let mut agent = world.agent(None);
    let report = agent.ready();
    assert_eq!(
        report["recoveredChanges"]["created"],
        json!(["my-notes.md"]),
        "{report}"
    );
    assert_eq!(
        report["recoveredChanges"]["changed"],
        json!(["README.md"]),
        "{report}"
    );
    assert_eq!(report["installChanges"]["created"], json!([]), "{report}");
    let backup = PathBuf::from(report["installBackup"].as_str().expect("a backup"));
    assert_eq!(
        fs::read_to_string(backup.join("README.md")).unwrap(),
        readme
    );

    let discard = agent.request("migrateFinish", json!({"id": id, "action": "discard"}));
    assert!(error_message(&discard).contains("my-notes.md"), "{discard}");
    let resync = agent.request("migrateResync", json!({"id": id}));
    assert!(error_message(&resync).contains("README.md"), "{resync}");
    assert!(world.dest.join("my-notes.md").exists());
    let recheck = agent.request("migrateRecheck", json!({"id": id}));
    assert_eq!(recheck["result"]["copyChanged"], json!([]), "{recheck}");
    let keep = agent.request("migrateFinish", json!({"id": id, "action": "keep"}));
    assert_eq!(keep["result"], Value::Null, "{keep}");
    assert_eq!(
        fs::read_to_string(world.dest.join("README.md")).unwrap(),
        "edited in the copy\n"
    );
}

/// Review 3 A: the same, ending in Delete original, which is allowed.
#[test]
fn edits_after_an_interrupted_install_still_let_the_original_go() {
    let world = World::new();
    let reached = world.tmp.join("reached");
    let hold = world.tmp.join("hold");
    write(&hold, "");
    let hook = world.hook(&format!(
        "[ \"$1\" = placed ] || exit 0\ntouch {reached:?}\nwhile [ -e {hold:?} ]; do sleep 0.05; done\n"
    ));
    let mut agent = world.agent(Some(&hook));
    let id = world.migrate(&mut agent, &world.dest)["result"]["id"]
        .as_str()
        .unwrap()
        .to_owned();
    wait_for(&reached);
    drop(agent);
    fs::remove_file(&hold).unwrap();
    write(&world.dest.join("README.md"), "edited in the copy\n");
    let mut agent = world.agent(None);
    let report = agent.ready();
    assert!(can_delete(&report), "{report}");
    let begin = agent.request("migrateBeginDelete", json!({"id": id}));
    assert_eq!(begin["result"]["ok"], true, "{begin}");
    fs::remove_dir_all(&world.src).unwrap();
    let finish = agent.request("migrateFinish", json!({"id": id, "action": "delete"}));
    assert_eq!(finish["result"], Value::Null, "{finish}");
}

/// Review 3 B: `force` skips only the copy changes it names, never others, never an
/// unreadable path; removals in the copy are listed apart.
#[test]
fn force_skips_only_the_copy_changes_it_names() {
    let world = World::new();
    let (mut agent, id) = placed_migration(&world, None);
    let begin = agent.request("migrateBeginDelete", json!({"id": id}));
    assert_eq!(begin["result"]["ok"], true, "{begin}");
    write(&world.dest.join("README.md"), "my edit in the copy\n");
    fs::remove_file(world.dest.join("notes.txt")).unwrap();
    let blocked = agent.request("migrateBeginDelete", json!({"id": id}));
    let recheck = &blocked["result"]["recheck"];
    assert_eq!(blocked["result"]["ok"], false, "{blocked}");
    assert_eq!(recheck["copyChanged"], json!(["README.md", "notes.txt"]));
    assert_eq!(recheck["copyRemoved"], json!(["notes.txt"]), "{blocked}");
    let some = agent.request(
        "migrateBeginDelete",
        json!({"id": id, "force": {"changed": ["README.md"]}}),
    );
    assert_eq!(some["result"]["ok"], false, "{some}");
    // A copy path that can't be read is never forced.
    let scripts = world.dest.join("scripts");
    fs::set_permissions(&scripts, fs::Permissions::from_mode(0o000)).unwrap();
    let unreadable = agent.request(
        "migrateBeginDelete",
        json!({"id": id, "force": {"changed": ["README.md", "scripts"], "removed": ["notes.txt"]}}),
    );
    fs::set_permissions(&scripts, fs::Permissions::from_mode(0o755)).unwrap();
    assert_eq!(unreadable["result"]["ok"], false, "{unreadable}");
    assert_eq!(
        unreadable["result"]["recheck"]["copyUnreadable"],
        json!(["scripts"]),
        "{unreadable}"
    );
    // Each path only under how it changed.
    let swapped = agent.request(
        "migrateBeginDelete",
        json!({"id": id, "force": {"changed": ["notes.txt"], "removed": ["README.md"]}}),
    );
    assert_eq!(swapped["result"]["ok"], false, "{swapped}");
    let all = agent.request(
        "migrateBeginDelete",
        json!({"id": id, "force": {"changed": ["README.md"], "removed": ["notes.txt"]}}),
    );
    assert_eq!(all["result"]["ok"], true, "{all}");
}

/// Review 3 F: a copy that can't be read is not taken for one that is gone.
#[test]
fn an_unreadable_copy_is_not_a_copy_that_is_gone() {
    let world = World::new();
    let (mut agent, id) = placed_migration(&world, None);
    fs::set_permissions(&world.dest, fs::Permissions::from_mode(0o000)).unwrap();
    let recheck = agent.request("migrateRecheck", json!({"id": id}));
    fs::set_permissions(&world.dest, fs::Permissions::from_mode(0o755)).unwrap();
    assert_eq!(recheck["result"]["copyMissing"], false, "{recheck}");
    assert_eq!(
        recheck["result"]["copyUnreadable"],
        json!(["."]),
        "{recheck}"
    );
    fs::remove_dir_all(&world.dest).unwrap();
    let recheck = agent.request("migrateRecheck", json!({"id": id}));
    assert_eq!(recheck["result"]["copyMissing"], true, "{recheck}");
    assert_eq!(recheck["result"]["copyChanged"], json!(["."]), "{recheck}");
}

/// Review 3 C: a finish delete whose config write failed is retried the app's way,
/// `migrateBeginDelete` first, and ends the move.
#[test]
fn a_finish_delete_whose_config_failed_is_retried_the_apps_way() {
    let world = World::new();
    let (mut agent, id) = placed_migration(&world, None);
    let begin = agent.request("migrateBeginDelete", json!({"id": id}));
    assert_eq!(begin["result"]["ok"], true, "{begin}");
    fs::remove_dir_all(&world.src).unwrap();
    let saved = fs::read_to_string(&world.config).unwrap();
    fs::remove_file(&world.config).unwrap();
    fs::create_dir(&world.config).unwrap();
    let finish = agent.request("migrateFinish", json!({"id": id, "action": "delete"}));
    assert!(finish["error"].is_object(), "{finish}");
    fs::remove_dir(&world.config).unwrap();
    write(&world.config, &saved);
    let recheck = agent.request("migrateRecheck", json!({"id": id}));
    assert!(recheck["result"].is_object(), "{recheck}");
    let begin = agent.request("migrateBeginDelete", json!({"id": id, "sourceGone": true}));
    assert_eq!(begin["result"]["ok"], true, "{begin}");
    let finish = agent.request("migrateFinish", json!({"id": id, "action": "delete"}));
    assert_eq!(finish["result"], Value::Null, "{finish}");
    let pending = agent.request("migratePending", json!(null));
    assert_eq!(pending["result"], json!([]), "{pending}");
}

/// Review 3 D: finishing a delete with the copy gone is refused; the retry finds the
/// copy gone, and the move can be ended.
#[test]
fn a_finish_delete_with_the_copy_gone_is_refused() {
    let world = World::new();
    let (mut agent, id) = placed_migration(&world, None);
    let begin = agent.request("migrateBeginDelete", json!({"id": id}));
    assert_eq!(begin["result"]["ok"], true, "{begin}");
    fs::remove_dir_all(&world.src).unwrap();
    fs::remove_dir_all(&world.dest).unwrap();
    let finish = agent.request("migrateFinish", json!({"id": id, "action": "delete"}));
    assert!(error_message(&finish).contains("gone"), "{finish}");
    let begin = agent.request("migrateBeginDelete", json!({"id": id, "sourceGone": true}));
    assert_eq!(begin["result"]["ok"], false, "{begin}");
    assert_eq!(begin["result"]["recheck"]["copyMissing"], true, "{begin}");
    let discard = agent.request("migrateFinish", json!({"id": id, "action": "discard"}));
    assert_eq!(discard["result"], Value::Null, "{discard}");
}

/// Review 4: finish delete removes the old transcripts, its config write fails, the
/// helper restarts and the copy goes. Ending the move then only forgets it: the
/// transcript copies, now the only ones, stay.
#[test]
fn ending_the_move_after_a_half_finished_delete_keeps_the_history() {
    let world = World::new();
    let src = world.src.clone();
    let old = project_dir(&world.projects(), &src).join(format!("{SESSION_ID}.jsonl"));
    write(
        &old,
        &transcript(
            SESSION_ID,
            &src,
            &src.join("README.md"),
            Utc::now() - TimeDelta::days(2),
            true,
        ),
    );
    let (mut agent, id) = placed_migration(&world, None);
    let new = project_dir(&world.projects(), &world.dest).join(format!("{SESSION_ID}.jsonl"));
    assert!(new.exists());
    let begin = agent.request("migrateBeginDelete", json!({"id": id}));
    assert_eq!(begin["result"]["ok"], true, "{begin}");
    fs::remove_dir_all(&world.src).unwrap();
    let saved = fs::read_to_string(&world.config).unwrap();
    fs::remove_file(&world.config).unwrap();
    fs::create_dir(&world.config).unwrap();
    let finish = agent.request("migrateFinish", json!({"id": id, "action": "delete"}));
    assert!(finish["error"].is_object(), "{finish}");
    assert!(!old.exists() && new.exists());
    fs::remove_dir(&world.config).unwrap();
    write(&world.config, &saved);
    drop(agent);
    fs::remove_dir_all(&world.dest).unwrap();
    let mut agent = world.agent(None);
    agent.ready();
    let begin = agent.request("migrateBeginDelete", json!({"id": id, "sourceGone": true}));
    assert_eq!(begin["result"]["recheck"]["copyMissing"], true, "{begin}");
    let discard = agent.request("migrateFinish", json!({"id": id, "action": "discard"}));
    assert_eq!(discard["result"], Value::Null, "{discard}");
    assert!(new.exists(), "the only transcript was deleted");
}

/// Review 4: the config write of finish delete fails, then the copy goes. End the
/// move (discard) is accepted, as a forget, without a restart.
#[test]
fn ending_the_move_while_finishing_a_delete_whose_copy_went() {
    let world = World::new();
    let src = world.src.clone();
    let old = project_dir(&world.projects(), &src).join(format!("{SESSION_ID}.jsonl"));
    write(
        &old,
        &transcript(
            SESSION_ID,
            &src,
            &src.join("README.md"),
            Utc::now() - TimeDelta::days(2),
            true,
        ),
    );
    let (mut agent, id) = placed_migration(&world, None);
    let new = project_dir(&world.projects(), &world.dest).join(format!("{SESSION_ID}.jsonl"));
    let begin = agent.request("migrateBeginDelete", json!({"id": id}));
    assert_eq!(begin["result"]["ok"], true, "{begin}");
    fs::remove_dir_all(&world.src).unwrap();
    let saved = fs::read_to_string(&world.config).unwrap();
    fs::remove_file(&world.config).unwrap();
    fs::create_dir(&world.config).unwrap();
    let finish = agent.request("migrateFinish", json!({"id": id, "action": "delete"}));
    assert!(finish["error"].is_object(), "{finish}");
    fs::remove_dir(&world.config).unwrap();
    write(&world.config, &saved);
    fs::remove_dir_all(&world.dest).unwrap();
    let begin = agent.request("migrateBeginDelete", json!({"id": id, "sourceGone": true}));
    assert_eq!(begin["result"]["recheck"]["copyMissing"], true, "{begin}");
    let discard = agent.request("migrateFinish", json!({"id": id, "action": "discard"}));
    assert_eq!(
        discard["result"],
        Value::Null,
        "End the move refused: {discard}"
    );
    assert!(new.exists(), "the only transcript was deleted");
    let pending = agent.request("migratePending", json!(null));
    assert!(!pending.to_string().contains(&id), "{pending}");
}

/// Review 4: a path the user saw edited in the copy and forced, deleted in the copy
/// since, still blocks: forcing it again needs the user to see it as removed.
#[test]
fn force_of_an_edit_does_not_skip_it_once_removed() {
    let world = World::new();
    let (mut agent, id) = placed_migration(&world, None);
    let begin = agent.request("migrateBeginDelete", json!({"id": id}));
    assert_eq!(begin["result"]["ok"], true, "{begin}");
    write(&world.dest.join("README.md"), "my edit in the copy\n");
    let seen = agent.request("migrateBeginDelete", json!({"id": id}));
    assert_eq!(
        seen["result"]["recheck"]["copyRemoved"],
        json!([]),
        "{seen}"
    );
    fs::remove_file(world.dest.join("README.md")).unwrap();
    let forced = agent.request(
        "migrateBeginDelete",
        json!({"id": id, "force": {"changed": ["README.md"]}}),
    );
    assert_eq!(forced["result"]["ok"], false, "{forced}");
    assert_eq!(
        forced["result"]["recheck"]["copyRemoved"],
        json!(["README.md"]),
        "{forced}"
    );
    let forced = agent.request(
        "migrateBeginDelete",
        json!({"id": id, "force": {"removed": ["README.md"]}}),
    );
    assert_eq!(forced["result"]["ok"], true, "{forced}");
}
