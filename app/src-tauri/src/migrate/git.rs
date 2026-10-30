//! The git snapshot G0 and the clean blob ids of tracked files (spec §4.2), read with
//! `git.exe`, or with Linux git through `wsl.exe` when `git.exe` is missing.
//!
//! Each [`GitSnapshot`] field comes from the command refract-core's `gitstate` module
//! documents, parsed with its parser, so the helper can build the copy's the same way.
//! Every command runs with `-c core.quotepath=false` and with `GIT_OPTIONAL_LOCKS=0`,
//! so `git status` never refreshes (and so rewrites) the index while it is recorded,
//! and with `LC_ALL=C`, so fsck's lines are the English ones `parse_fsck` filters.
//! Linux git also gets `-c core.autocrlf=true -c core.filemode=false`, so it sees the
//! files as Windows git does.
//!
//! git runs behind [`GitRun`], so the rules here are tested without git.

use std::collections::BTreeSet;
use std::io::{self, Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::thread;
use std::time::{Duration, Instant};

use refract_core::migrate::gitconfig::parse_config;
use refract_core::migrate::gitstate::{
    parse_fsck, parse_head, parse_lines, parse_nul_records, parse_refs, parse_status,
    parse_submodules, GitSnapshot, StatusKind, FOR_EACH_REF_FORMAT,
};

use crate::agent::{neutral_dir, no_window};

/// How long one git command gets. fsck of a large repo is the slow one.
const GIT_TIMEOUT: Duration = Duration::from_secs(30 * 60);
/// How often a running git is checked for cancellation.
const POLL: Duration = Duration::from_millis(50);

/// What one git command printed, and whether it exited 0.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct GitOutput {
    pub success: bool,
    pub stdout: Vec<u8>,
    pub stderr: Vec<u8>,
}

/// Runs git in a working tree of the source.
pub trait GitRun: Sync {
    /// Runs `git <args>` in the working tree at `repo` (relative to the source, `/`-
    /// separated; `""` for the source itself), feeding it `stdin`.
    fn run(&self, repo: &str, args: &[&str], stdin: Option<&[u8]>) -> io::Result<GitOutput>;
    /// True when this is Linux git reading the `/mnt` path.
    fn linux(&self) -> bool;
}

/// Why the git snapshot failed.
pub type GitError = String;

fn text<'a>(command: &str, bytes: &'a [u8]) -> Result<&'a str, GitError> {
    std::str::from_utf8(bytes).map_err(|_| {
        format!(
            "git {command} printed a path that isn't valid UTF-8, which the move can't represent"
        )
    })
}

/// Runs `args` and returns stdout as text; fails when git exits non-zero, unless
/// `allow_failure`.
fn run_text(
    git: &dyn GitRun,
    repo: &str,
    args: &[&str],
    allow_failure: bool,
) -> Result<String, GitError> {
    let name = args.first().copied().unwrap_or("");
    let out = git
        .run(repo, args, None)
        .map_err(|err| format!("cannot run git {name}: {err}"))?;
    if !out.success && !allow_failure {
        return Err(format!(
            "git {} failed in {}: {}",
            args.join(" "),
            if repo.is_empty() { "the repo" } else { repo },
            String::from_utf8_lossy(&out.stderr).trim()
        ));
    }
    Ok(text(name, &out.stdout)?.to_owned())
}

/// The status command, shared by the snapshot and the preflight.
const STATUS: &[&str] = &[
    "status",
    "--porcelain=v2",
    "-z",
    "--untracked-files=all",
    "--ignored=matching",
];

/// G0 of the repo at `repo` (spec §4.2).
pub fn git_snapshot(git: &dyn GitRun, repo: &str) -> Result<GitSnapshot, GitError> {
    let refs_format = format!("--format={FOR_EACH_REF_FORMAT}");
    // An unborn HEAD fails and prints `HEAD`, which parse_head turns into None.
    let head = parse_head(&run_text(git, repo, &["rev-parse", "HEAD"], true)?);
    let refs = parse_refs(&run_text(
        git,
        repo,
        &["for-each-ref", &refs_format],
        false,
    )?)?;
    let stash = parse_lines(&run_text(
        git,
        repo,
        &["stash", "list", "--format=%H"],
        false,
    )?);
    let config = parse_config(&run_text(git, repo, &["config", "--local", "-l"], false)?);
    let status = parse_status(&run_text(git, repo, STATUS, false)?)?;
    let index = parse_nul_records(&run_text(git, repo, &["ls-files", "-s", "-z"], false)?);
    let submodules = parse_submodules(&run_text(
        git,
        repo,
        &["submodule", "status", "--recursive"],
        false,
    )?)?;
    // fsck exits non-zero when it finds problems; those are what it records.
    let out = git
        .run(repo, &["fsck", "--full"], None)
        .map_err(|err| format!("cannot run git fsck: {err}"))?;
    let mut fsck_text = String::from_utf8_lossy(&out.stdout).into_owned();
    fsck_text.push('\n');
    fsck_text.push_str(&String::from_utf8_lossy(&out.stderr));
    Ok(GitSnapshot {
        head,
        refs,
        stash,
        config,
        status,
        index,
        submodules,
        fsck: parse_fsck(&fsck_text),
    })
}

/// What the preflight needs from git, cheaper than [`git_snapshot`]: the status of
/// the repo and each checked-out submodule, for the folders git ignores.
pub fn ignored_dirs_quick(git: &dyn GitRun) -> Result<BTreeSet<String>, GitError> {
    let subs = parse_submodules(&run_text(
        git,
        "",
        &["submodule", "status", "--recursive"],
        false,
    )?)?;
    let mut repos = vec![String::new()];
    repos.extend(subs.into_iter().filter(|s| s.state != "-").map(|s| s.path));
    let mut ignored = BTreeSet::new();
    for repo in repos {
        let status = parse_status(&run_text(git, &repo, STATUS, false)?)?;
        ignored.extend(ignored_dirs(&repo, &status));
    }
    Ok(ignored)
}

/// The working-tree paths of the submodules that are checked out, from the top repo's
/// `submodule status --recursive` (nested ones included), relative to the top repo.
pub fn submodule_repos(top: &GitSnapshot) -> Vec<String> {
    top.submodules
        .iter()
        .filter(|s| s.state != "-")
        .map(|s| s.path.trim_end_matches('/').to_owned())
        .collect()
}

/// The folders git ignores itself (`! dir/` in the status of the repo at `repo`),
/// relative to the top repo, without the trailing `/`.
pub fn ignored_dirs(
    repo: &str,
    status: &[refract_core::migrate::gitstate::StatusEntry],
) -> Vec<String> {
    status
        .iter()
        .filter(|e| e.kind == StatusKind::Ignored)
        .filter_map(|e| e.path.strip_suffix('/'))
        .map(|dir| prefixed(repo, dir))
        .collect()
}

fn prefixed(repo: &str, path: &str) -> String {
    if repo.is_empty() {
        path.to_owned()
    } else {
        format!("{repo}/{path}")
    }
}

/// The tracked paths of a repo from its `ls-files -s -z` records, relative to that
/// repo, once each (an unmerged path has several stages), submodules (mode 160000)
/// left out.
pub fn tracked_paths(index: &[String]) -> Vec<String> {
    let mut seen = BTreeSet::new();
    index
        .iter()
        .filter_map(|record| {
            let (meta, path) = record.split_once('\t')?;
            (!meta.starts_with("160000 ")).then_some(path)
        })
        .filter(|path| seen.insert(*path))
        .map(str::to_owned)
        .collect()
}

/// The input of `hash-object --stdin-paths`: one path per line. git unquotes a line
/// that starts with `"` C-style, so such a path, and one holding a newline, is quoted.
pub fn stdin_paths(paths: &[String]) -> Vec<u8> {
    let mut out = Vec::new();
    for path in paths {
        if path.starts_with('"') || path.contains('\n') {
            out.push(b'"');
            for byte in path.bytes() {
                match byte {
                    b'"' => out.extend_from_slice(b"\\\""),
                    b'\\' => out.extend_from_slice(b"\\\\"),
                    b'\n' => out.extend_from_slice(b"\\n"),
                    b'\t' => out.extend_from_slice(b"\\t"),
                    _ => out.push(byte),
                }
            }
            out.push(b'"');
        } else {
            out.extend_from_slice(path.as_bytes());
        }
        out.push(b'\n');
    }
    out
}

/// The clean blob id of each of `paths` (relative to the repo at `repo`), in order, as
/// git would store the file now: `hash-object --stdin-paths` applies the repo's
/// filters and line-ending settings.
pub fn clean_blobs(
    git: &dyn GitRun,
    repo: &str,
    paths: &[String],
) -> Result<Vec<String>, GitError> {
    if paths.is_empty() {
        return Ok(Vec::new());
    }
    let out = git
        .run(
            repo,
            &["hash-object", "--stdin-paths"],
            Some(&stdin_paths(paths)),
        )
        .map_err(|err| format!("cannot run git hash-object: {err}"))?;
    if !out.success {
        return Err(format!(
            "git hash-object failed: {}",
            String::from_utf8_lossy(&out.stderr).trim()
        ));
    }
    let ids = parse_lines(text("hash-object", &out.stdout)?);
    if ids.len() != paths.len() {
        return Err(format!(
            "git hash-object returned {} ids for {} files",
            ids.len(),
            paths.len()
        ));
    }
    Ok(ids)
}

/// `git.exe`: the first one on `path`, else the Git for Windows install under
/// `program_files`. `is_file` is the file system check.
pub fn find_git_exe(
    path: Option<&str>,
    program_files: Option<&Path>,
    is_file: impl Fn(&Path) -> bool,
) -> Option<PathBuf> {
    let on_path = path
        .map(crate::launch::path_dirs)
        .unwrap_or_default()
        .into_iter()
        .map(|dir| dir.join("git.exe"));
    let installed = program_files.map(|dir| dir.join(r"Git\cmd\git.exe"));
    on_path
        .chain(installed)
        .find(|candidate| is_file(candidate))
}

/// Environment for every git command.
const GIT_ENV: &[(&str, &str)] = &[
    ("GIT_OPTIONAL_LOCKS", "0"),
    ("LC_ALL", "C"),
    ("LANG", "C"),
    ("GIT_TERMINAL_PROMPT", "0"),
];

/// `git.exe` on the source's Windows path.
pub struct GitExe {
    pub exe: PathBuf,
    /// The source's Windows path, such as `C:\Users\dev\aurora-notes`.
    pub root: PathBuf,
    pub cancel: Arc<AtomicBool>,
}

impl GitRun for GitExe {
    fn run(&self, repo: &str, args: &[&str], stdin: Option<&[u8]>) -> io::Result<GitOutput> {
        let mut dir = self.root.clone();
        for part in repo.split('/').filter(|p| !p.is_empty()) {
            dir.push(part);
        }
        let mut command = Command::new(&self.exe);
        command
            .args(["-c", "core.quotepath=false"])
            .args(args)
            .current_dir(dir)
            .envs(GIT_ENV.iter().copied());
        no_window(&mut command);
        run_cancellable(&mut command, stdin, &self.cancel, GIT_TIMEOUT)
    }

    fn linux(&self) -> bool {
        false
    }
}

/// Linux git through `wsl.exe`, on the source's `/mnt` path.
pub struct WslGit {
    pub distro: Option<String>,
    /// The source's Linux path, such as `/mnt/c/Users/dev/aurora-notes`.
    pub root: String,
    pub cancel: Arc<AtomicBool>,
}

/// Arguments for `wsl.exe` that run Linux git as [`WslGit`] does.
pub fn wsl_git_args(distro: Option<&str>, dir: &str, args: &[&str]) -> Vec<String> {
    let mut out = crate::agent::distro_args(distro);
    out.extend([
        "--cd".to_owned(),
        dir.to_owned(),
        "--exec".to_owned(),
        "env".to_owned(),
    ]);
    out.extend(GIT_ENV.iter().map(|(k, v)| format!("{k}={v}")));
    out.extend(
        [
            "git",
            "-c",
            "core.quotepath=false",
            "-c",
            "core.autocrlf=true",
            "-c",
            "core.filemode=false",
        ]
        .map(str::to_owned),
    );
    out.extend(args.iter().map(|a| (*a).to_owned()));
    out
}

impl GitRun for WslGit {
    fn run(&self, repo: &str, args: &[&str], stdin: Option<&[u8]>) -> io::Result<GitOutput> {
        let dir = prefixed(&self.root, repo);
        let mut command = crate::agent::wsl_command();
        command.args(wsl_git_args(self.distro.as_deref(), &dir, args));
        run_cancellable(&mut command, stdin, &self.cancel, GIT_TIMEOUT)
    }

    fn linux(&self) -> bool {
        true
    }
}

/// Runs `command` with piped stdio, writing `stdin` on its own thread, until it exits,
/// `cancel` is set or `timeout` passes; the last two kill it and fail.
pub fn run_cancellable(
    command: &mut Command,
    stdin: Option<&[u8]>,
    cancel: &AtomicBool,
    timeout: Duration,
) -> io::Result<GitOutput> {
    if command.get_current_dir().is_none() {
        command.current_dir(neutral_dir());
    }
    let mut child = command
        .stdin(if stdin.is_some() {
            Stdio::piped()
        } else {
            Stdio::null()
        })
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()?;
    let writer = match (child.stdin.take(), stdin) {
        (Some(mut pipe), Some(bytes)) => {
            let bytes = bytes.to_vec();
            // A write error means git exited early; its status says why.
            Some(thread::spawn(move || {
                let _ = pipe.write_all(&bytes);
            }))
        }
        _ => None,
    };
    let stdout = read_on_thread(child.stdout.take());
    let stderr = read_on_thread(child.stderr.take());
    let started = Instant::now();
    let status = loop {
        if let Some(status) = child.try_wait()? {
            break status;
        }
        if cancel.load(Ordering::Relaxed) {
            kill(&mut child);
            return Err(io::Error::new(io::ErrorKind::Interrupted, "cancelled"));
        }
        if started.elapsed() > timeout {
            kill(&mut child);
            return Err(io::Error::new(
                io::ErrorKind::TimedOut,
                format!("timed out after {timeout:?}"),
            ));
        }
        thread::sleep(POLL);
    };
    if let Some(writer) = writer {
        let _ = writer.join();
    }
    Ok(GitOutput {
        success: status.success(),
        stdout: stdout.join().unwrap_or_default(),
        stderr: stderr.join().unwrap_or_default(),
    })
}

fn kill(child: &mut Child) {
    // Errors mean it already exited.
    let _ = child.kill();
    let _ = child.wait();
}

fn read_on_thread(pipe: Option<impl Read + Send + 'static>) -> thread::JoinHandle<Vec<u8>> {
    thread::spawn(move || {
        let mut bytes = Vec::new();
        if let Some(mut pipe) = pipe {
            // A read error keeps what was read before it.
            let _ = pipe.read_to_end(&mut bytes);
        }
        bytes
    })
}

#[cfg(test)]
pub mod tests {
    use super::*;
    use std::collections::BTreeMap;
    use std::sync::Mutex;

    /// Answers git commands from a table keyed by `<repo>|<args>`.
    #[derive(Default)]
    pub struct FakeGit {
        pub answers: BTreeMap<String, GitOutput>,
        pub calls: Mutex<Vec<String>>,
    }

    impl FakeGit {
        pub fn answer(&mut self, repo: &str, args: &str, stdout: &str) {
            self.answers.insert(
                format!("{repo}|{args}"),
                GitOutput {
                    success: true,
                    stdout: stdout.as_bytes().to_vec(),
                    stderr: Vec::new(),
                },
            );
        }
    }

    impl GitRun for FakeGit {
        fn run(&self, repo: &str, args: &[&str], stdin: Option<&[u8]>) -> io::Result<GitOutput> {
            let key = format!("{repo}|{}", args.join(" "));
            let mut call = key.clone();
            if let Some(stdin) = stdin {
                call.push_str(&format!(" <{}", String::from_utf8_lossy(stdin)));
            }
            self.calls.lock().unwrap().push(call);
            match self.answers.get(&key) {
                Some(out) => Ok(out.clone()),
                None if args[0] == "rev-parse" => Ok(GitOutput {
                    success: false,
                    stdout: b"HEAD\n".to_vec(),
                    stderr: b"fatal: ambiguous argument 'HEAD'".to_vec(),
                }),
                None => Ok(GitOutput {
                    success: true,
                    ..GitOutput::default()
                }),
            }
        }

        fn linux(&self) -> bool {
            false
        }
    }

    const ID: &str = "1111111111111111111111111111111111111111";

    pub fn repo_with_submodule() -> FakeGit {
        let mut git = FakeGit::default();
        git.answer("", "rev-parse HEAD", &format!("{ID}\n"));
        git.answer(
            "",
            &format!("for-each-ref --format={FOR_EACH_REF_FORMAT}"),
            &format!("{ID}\trefs/heads/main\trefs/remotes/origin/main\t\n"),
        );
        git.answer("", "config --local -l", "core.autocrlf=true\n");
        git.answer(
            "",
            &STATUS.join(" "),
            "# branch.oid x\0? notes.txt\0! node_modules/\0! .env\0! web/dist/\0",
        );
        git.answer(
            "",
            "ls-files -s -z",
            &format!("100644 {ID} 0\tREADME.md\0160000 {ID} 0\tlib/sub\0"),
        );
        git.answer(
            "",
            "submodule status --recursive",
            &format!(" {ID} lib/sub (heads/main)\n-{ID} lib/unused\n"),
        );
        git.answer("lib/sub", &STATUS.join(" "), "! target/\0");
        git.answer("", "fsck --full", "dangling blob 2222\nChecking objects\n");
        git
    }

    #[test]
    fn reads_g0_with_the_documented_commands() {
        let git = repo_with_submodule();
        let snapshot = git_snapshot(&git, "").unwrap();
        assert_eq!(snapshot.head.as_deref(), Some(ID));
        assert_eq!(snapshot.refs.len(), 1);
        assert_eq!(
            snapshot.refs[0].upstream.as_deref(),
            Some("refs/remotes/origin/main")
        );
        assert_eq!(snapshot.status.len(), 4);
        assert_eq!(snapshot.index.len(), 2);
        assert_eq!(snapshot.submodules.len(), 2);
        assert!(snapshot.fsck.is_empty());
        assert_eq!(submodule_repos(&snapshot), ["lib/sub"]);
        assert_eq!(
            ignored_dirs("", &snapshot.status),
            ["node_modules", "web/dist"]
        );
        assert_eq!(
            ignored_dirs("lib/sub", &parse_status("! target/\0").unwrap()),
            ["lib/sub/target"]
        );
        // An unborn HEAD is no commit, not an error.
        let empty = git_snapshot(&git, "lib/sub").unwrap();
        assert_eq!(empty.head, None);
    }

    #[test]
    fn quick_ignored_dirs_cover_checked_out_submodules() {
        let git = repo_with_submodule();
        let dirs: Vec<String> = ignored_dirs_quick(&git).unwrap().into_iter().collect();
        assert_eq!(dirs, ["lib/sub/target", "node_modules", "web/dist"]);
    }

    #[test]
    fn a_failing_command_names_itself() {
        let mut git = repo_with_submodule();
        git.answers.insert(
            "|ls-files -s -z".into(),
            GitOutput {
                success: false,
                stdout: Vec::new(),
                stderr: b"fatal: index file corrupt\n".to_vec(),
            },
        );
        let err = git_snapshot(&git, "").unwrap_err();
        assert_eq!(
            err,
            "git ls-files -s -z failed in the repo: fatal: index file corrupt"
        );
    }

    #[test]
    fn tracked_paths_skip_submodules_and_repeat_stages() {
        let index = [
            format!("100644 {ID} 0\tREADME.md"),
            format!("160000 {ID} 0\tlib/sub"),
            format!("100644 {ID} 1\tconflict.txt"),
            format!("100644 {ID} 2\tconflict.txt"),
            format!("120000 {ID} 0\tlink"),
        ];
        assert_eq!(tracked_paths(&index), ["README.md", "conflict.txt", "link"]);
    }

    #[test]
    fn odd_paths_are_quoted_for_stdin_paths() {
        let paths = [
            "plain.txt".to_owned(),
            "\"quoted\".txt".into(),
            "new\nline".into(),
        ];
        assert_eq!(
            String::from_utf8(stdin_paths(&paths)).unwrap(),
            "plain.txt\n\"\\\"quoted\\\".txt\"\n\"new\\nline\"\n"
        );
    }

    #[test]
    fn clean_blobs_are_one_per_path() {
        let mut git = FakeGit::default();
        git.answer("", "hash-object --stdin-paths", &format!("{ID}\n{ID}\n"));
        let paths = ["a".to_owned(), "b".to_owned()];
        assert_eq!(clean_blobs(&git, "", &paths).unwrap(), [ID, ID]);
        assert_eq!(
            git.calls.lock().unwrap()[0],
            "|hash-object --stdin-paths <a\nb\n"
        );
        let one = ["a".to_owned()];
        assert!(clean_blobs(&git, "", &one)
            .unwrap_err()
            .contains("2 ids for 1 files"));
        assert_eq!(clean_blobs(&git, "", &[]).unwrap(), Vec::<String>::new());
    }

    #[test]
    fn git_exe_is_found_on_path_then_in_program_files() {
        let exists = |files: &'static [&'static str]| {
            move |p: &Path| files.iter().any(|f| Path::new(f) == p)
        };
        assert_eq!(
            find_git_exe(
                Some(r"C:\Windows;C:\Tools\Git\cmd"),
                Some(Path::new(r"C:\Program Files")),
                exists(&[
                    r"C:\Tools\Git\cmd\git.exe",
                    r"C:\Program Files\Git\cmd\git.exe"
                ]),
            ),
            Some(PathBuf::from(r"C:\Tools\Git\cmd\git.exe"))
        );
        assert_eq!(
            find_git_exe(
                Some(r"C:\Windows"),
                Some(Path::new(r"C:\Program Files")),
                exists(&[r"C:\Program Files\Git\cmd\git.exe"]),
            ),
            Some(PathBuf::from(r"C:\Program Files\Git\cmd\git.exe"))
        );
        assert_eq!(find_git_exe(None, None, exists(&[])), None);
    }

    #[test]
    fn linux_git_sees_the_files_as_windows_git_does() {
        let args = wsl_git_args(Some("Ubuntu"), "/mnt/c/dev/aurora-notes", &["status"]);
        assert_eq!(
            args[..6],
            [
                "-d",
                "Ubuntu",
                "--cd",
                "/mnt/c/dev/aurora-notes",
                "--exec",
                "env"
            ]
        );
        assert!(args.contains(&"GIT_OPTIONAL_LOCKS=0".to_owned()));
        let git = args.iter().position(|a| a == "git").unwrap();
        assert_eq!(
            args[git..],
            [
                "git",
                "-c",
                "core.quotepath=false",
                "-c",
                "core.autocrlf=true",
                "-c",
                "core.filemode=false",
                "status"
            ]
        );
    }
}
