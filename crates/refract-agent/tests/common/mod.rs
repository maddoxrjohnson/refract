//! Helpers for building temporary repos and Claude Code transcripts. All names are
//! invented.

#![allow(dead_code)]

use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::Once;

use chrono::{DateTime, Utc};

/// Environment that makes git hermetic: no user or system config, fixed identity.
pub const GIT_ENV: &[(&str, &str)] = &[
    ("GIT_CONFIG_GLOBAL", "/dev/null"),
    ("GIT_CONFIG_NOSYSTEM", "1"),
    ("GIT_AUTHOR_NAME", "Test Dev"),
    ("GIT_AUTHOR_EMAIL", "dev@example.com"),
    ("GIT_COMMITTER_NAME", "Test Dev"),
    ("GIT_COMMITTER_EMAIL", "dev@example.com"),
];

/// Applies [`GIT_ENV`] to this test process, so the agent's own git calls see it too.
pub fn hermetic_git() {
    static ONCE: Once = Once::new();
    ONCE.call_once(|| {
        for (key, value) in GIT_ENV {
            std::env::set_var(key, value);
        }
    });
}

/// Runs git in `dir` and returns trimmed stdout, panicking on failure.
pub fn git(dir: &Path, args: &[&str]) -> String {
    let out = Command::new("git")
        .arg("-C")
        .arg(dir)
        .args(args)
        .envs(GIT_ENV.iter().copied())
        .output()
        .expect("run git");
    assert!(
        out.status.success(),
        "git {args:?} failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8_lossy(&out.stdout).trim().to_owned()
}

pub fn write(path: &Path, text: &str) {
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    fs::write(path, text).unwrap();
}

/// Creates a repo on `main` with one commit containing `src/main.rs` and `README.md`.
pub fn init_repo(dir: &Path) {
    fs::create_dir_all(dir).unwrap();
    git(dir, &["init", "-q", "-b", "main"]);
    write(&dir.join("src/main.rs"), "fn main() {}\n");
    write(&dir.join("README.md"), "# demo\n");
    git(dir, &["add", "."]);
    git(dir, &["commit", "-q", "-m", "Initial commit"]);
}

pub fn commit_file(dir: &Path, file: &str, text: &str, message: &str) {
    write(&dir.join(file), text);
    git(dir, &["add", file]);
    git(dir, &["commit", "-q", "-m", message]);
}

/// A bare "remote" seeded from a fresh repo; returns (bare path, seed working copy).
pub fn remote(tmp: &Path, name: &str) -> (PathBuf, PathBuf) {
    let seed = tmp.join("seeds").join(name);
    init_repo(&seed);
    let bare = tmp.join("remotes").join(format!("{name}.git"));
    fs::create_dir_all(bare.parent().unwrap()).unwrap();
    git(
        tmp,
        &[
            "clone",
            "-q",
            "--bare",
            seed.to_str().unwrap(),
            bare.to_str().unwrap(),
        ],
    );
    git(&seed, &["remote", "add", "origin", bare.to_str().unwrap()]);
    git(&seed, &["fetch", "-q", "origin"]);
    git(&seed, &["branch", "-q", "--set-upstream-to=origin/main"]);
    (bare, seed)
}

pub fn clone(bare: &Path, dest: &Path) {
    fs::create_dir_all(dest.parent().unwrap()).unwrap();
    git(
        bare.parent().unwrap(),
        &[
            "clone",
            "-q",
            bare.to_str().unwrap(),
            dest.to_str().unwrap(),
        ],
    );
}

/// Claude Code's project directory name for `cwd`.
pub fn project_dir(projects: &Path, cwd: &Path) -> PathBuf {
    projects.join(refract_core::sessions::encode_project_dir(
        cwd.to_str().unwrap(),
    ))
}

/// An invented transcript in which the assistant edits `edited` in `cwd`. A finished
/// one ends with a text reply; an unfinished one ends with a tool call that never got
/// a result.
pub fn transcript(
    id: &str,
    cwd: &Path,
    edited: &Path,
    at: DateTime<Utc>,
    finished: bool,
) -> String {
    let cwd = cwd.to_str().unwrap();
    let edited = edited.to_str().unwrap();
    let ts = |s: i64| (at + chrono::TimeDelta::seconds(s)).to_rfc3339();
    let common = |n: u32, s: i64| {
        format!(
            r#""uuid":"9b0d0000-0000-4000-8000-{n:012}","timestamp":"{}","cwd":"{cwd}","sessionId":"{id}","isSidechain":false,"userType":"external""#,
            ts(s)
        )
    };
    let mut lines = vec![
        format!(r#"{{"type":"mode","mode":"default","sessionId":"{id}"}}"#),
        format!(
            r#"{{"type":"user",{},"message":{{"role":"user","content":"Tidy up the entry point"}}}}"#,
            common(1, -4)
        ),
        format!(
            r#"{{"type":"assistant",{},"requestId":"req_1","message":{{"id":"msg_1","role":"assistant","content":[{{"type":"tool_use","id":"toolu_edit","name":"Edit","input":{{"file_path":"{edited}","old_string":"a","new_string":"b"}}}}],"usage":{{"input_tokens":5,"output_tokens":7}}}}}}"#,
            common(2, -3)
        ),
        format!(
            r#"{{"type":"user",{},"message":{{"role":"user","content":[{{"tool_use_id":"toolu_edit","type":"tool_result","content":"Edited."}}]}}}}"#,
            common(3, -2)
        ),
    ];
    if finished {
        lines.push(format!(
            r#"{{"type":"assistant",{},"requestId":"req_2","message":{{"id":"msg_2","role":"assistant","content":[{{"type":"text","text":"The entry point is tidy."}}],"usage":{{"input_tokens":5,"output_tokens":7}}}}}}"#,
            common(4, 0)
        ));
    } else {
        lines.push(format!(
            r#"{{"type":"assistant",{},"requestId":"req_2","message":{{"id":"msg_2","role":"assistant","content":[{{"type":"tool_use","id":"toolu_test","name":"Bash","input":{{"command":"cargo test"}}}}],"usage":{{"input_tokens":5,"output_tokens":7}}}}}}"#,
            common(4, 0)
        ));
    }
    lines.join("\n") + "\n"
}
