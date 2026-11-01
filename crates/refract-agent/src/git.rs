//! Repo facts from the `git` command line.
//!
//! Every command runs with `--no-optional-locks`, so reading status never rewrites the
//! index. That matters because the index is watched: a write would trigger another
//! refresh.

use std::collections::BTreeSet;
use std::io;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::{AtomicBool, Ordering};

use refract_core::language::main_language;
use refract_core::protocol::{GitOperation, LastCommit};
use refract_core::sessions::is_windows_mount;
use refract_core::worktree::{derive_worktree_status, WorktreeFacts, MAX_ONLY_HERE};

use crate::scan;

/// What `git status` says: cheap, refreshed on every working-tree change.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Status {
    /// `None` when HEAD is detached.
    pub branch: Option<String>,
    /// An upstream is set, even one that is gone from the remote.
    pub upstream: bool,
    /// Ahead and behind the upstream; `None` without an upstream (or a gone one).
    pub ahead_behind: Option<(u32, u32)>,
    /// Changed, staged, conflicted and untracked paths, relative to the repo root. An
    /// untracked directory git still reports as a whole (such as a nested repo) keeps
    /// its trailing `/`.
    pub paths: Vec<String>,
    /// The original paths of the renames and copies in `paths`.
    pub origins: Vec<String>,
    /// What else a linked worktree holds; `None` for every other repo.
    pub worktree: Option<Worktree>,
}

/// Facts about a linked worktree (spec §4), read with its status.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Worktree {
    /// The main working tree's real path.
    pub main: PathBuf,
    /// The git part of its status read with the status, with `changes` from the status's
    /// `paths`. The ref facts (`only_here`, `merged`, `release`) are left empty here: they
    /// come from [`HeadInfo::worktree`] (see [`worktree_facts`]).
    pub facts: WorktreeFacts,
    /// Top-level ignored entries, at most [`MAX_IGNORED`]. Read only once the worktree
    /// could be removed (see [`finish_worktree`]); empty otherwise and on a Windows drive.
    pub ignored: Vec<String>,
}

/// The ref facts of a linked worktree (spec §4): refreshed when HEAD or refs move, not
/// on every working-tree change.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct WorktreeRefs {
    /// See [`WorktreeFacts::only_here`].
    pub only_here: Option<u32>,
    /// The default branch's name when HEAD is its ancestor.
    pub merged: Option<String>,
    /// The oldest tag containing HEAD.
    pub release: Option<String>,
}

/// The most top-level ignored entries kept: 8 to show, and one more to say "and more".
pub const MAX_IGNORED: usize = 9;

/// How deep [`holds_repo_in`] looks inside each ignored folder for a `.git`: the folder
/// itself is depth 0. `<ignored>/worktrees/<name>/.git` is found at depth 2.
const NESTED_REPO_DEPTH: usize = 4;

/// The most folders [`holds_repo_in`] reads before giving up and counting the worktree as
/// holding a repo, so a huge ignored tree (`node_modules/`) costs a bounded walk.
const NESTED_REPO_BUDGET: usize = 10_000;

/// Facts about HEAD: refreshed when HEAD or refs move.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct HeadInfo {
    pub origin_url: Option<String>,
    pub last_commit: Option<LastCommit>,
    pub language: Option<String>,
    pub tracked_bytes: u64,
    pub commits_30d: u32,
    /// 1 when HEAD has a commit that no `origin/*` branch has, else 0 (also without an
    /// origin, where it would not be used). Capped at 1: only whether there is one matters.
    pub off_origin: u32,
    /// Directories that hold tracked files, relative to the root (`""` is the root).
    pub tracked_dirs: Vec<String>,
    /// The ref facts of a linked worktree; `None` for every other repo.
    pub worktree: Option<WorktreeRefs>,
}

/// Reads `git status`. Fails if git cannot run or `path` is not a repo.
pub fn read_status(path: &Path) -> io::Result<Status> {
    let mut args = status_config(path).to_vec();
    args.extend([
        "status",
        "--porcelain=v2",
        "--branch",
        "-z",
        "--ignore-submodules=dirty",
        untracked_arg(path),
    ]);
    let out = git(path, &args)?.ok_or_else(|| io::Error::other("git status failed"))?;
    let mut status = parse_status(&out);
    if let Some(linked) = scan::linked(path) {
        let windows = worktree_on_windows_drive(path);
        status.worktree = Some(Worktree {
            main: linked.main.clone(),
            facts: WorktreeFacts {
                locked: is_locked(&linked.git_dir),
                operation: operation(&linked.git_dir),
                changes: status.paths.len() as u32,
                untracked_checked: !windows,
                windows_drive: windows,
                holds_repo: has_submodules(path, &linked.git_dir),
                ..WorktreeFacts::default()
            },
            ignored: Vec::new(),
        });
    }
    Ok(status)
}

/// True when the worktree at `path` counts as on a Windows drive: there untracked files
/// are not listed and Refract does not remove it. Also under the test prefix
/// [`crate::state::TEST_SLOW_PREFIX`].
pub fn worktree_on_windows_drive(path: &Path) -> bool {
    on_windows_drive(path) || crate::state::under_test_slow_prefix(path)
}

/// Reads the ref facts of the linked worktree at `path` (spec §4). Only reads git:
/// nothing touches the network. A fact git cannot give stays empty.
fn read_worktree_refs(path: &Path) -> WorktreeRefs {
    let merged = default_branch(path).filter(|(_, full)| is_ancestor(path, "HEAD", full));
    let release = git(path, &["tag", "--contains", "HEAD", "--sort=creatordate"])
        .ok()
        .flatten()
        .and_then(|out| {
            let text = String::from_utf8_lossy(&out);
            text.lines().next().map(str::to_owned)
        })
        .filter(|t| !t.is_empty());
    WorktreeRefs {
        only_here: only_here(path),
        merged: merged.map(|(name, _)| name),
        release,
    }
}

/// The facts of a linked worktree: those read with its status, and the ref facts read
/// with HEAD (none when HEAD could not be read, so it is not offered for removal).
pub fn worktree_facts(worktree: &Worktree, head: &HeadInfo) -> WorktreeFacts {
    let mut facts = worktree.facts.clone();
    if let Some(refs) = &head.worktree {
        facts.only_here = refs.only_here;
        facts.merged = refs.merged.clone();
        facts.release = refs.release.clone();
    }
    facts
}

/// Reads what only a removable worktree needs, once its status and HEAD facts are read:
/// the ignored entries the confirm lists, and whether an ignored folder holds a repo.
/// Skipped (and the ignored entries left empty) while anything else keeps the worktree,
/// so a worktree being worked in costs no extra git call per status refresh.
pub fn finish_worktree(path: &Path, status: &mut Status, head: &HeadInfo) {
    let Some(worktree) = status.worktree.as_mut() else {
        return;
    };
    worktree.ignored.clear();
    if !derive_worktree_status(&worktree_facts(worktree, head)).1 {
        return;
    }
    let Some(ignored) = ignored_paths(path) else {
        return;
    };
    worktree.ignored = top_level(&ignored);
    worktree.facts.holds_repo = holds_repo_in(path, &ignored);
}

/// Every ignored path in the worktree at `path`, as `git status --ignored` reports them:
/// a wholly ignored folder once, with a trailing `/`. `None` when git fails.
pub fn ignored_paths(path: &Path) -> Option<Vec<String>> {
    let out = git(
        path,
        &[
            "status",
            "--porcelain",
            "-z",
            "--ignored=traditional",
            "--untracked-files=normal",
            "--ignore-submodules=all",
        ],
    )
    .ok()??;
    Some(parse_ignored_paths(&out))
}

/// True when the worktree whose own git directory is `git_dir` has submodules, which
/// `git worktree remove` refuses: `<git_dir>/modules` exists, or `.gitmodules` does
/// (which also counts submodules not checked out, where git would not refuse).
pub fn has_submodules(path: &Path, git_dir: &Path) -> bool {
    git_dir.join("modules").is_dir() || std::fs::symlink_metadata(path.join(".gitmodules")).is_ok()
}

/// True when any folder in `ignored` (paths relative to `root`, folders ending in `/`)
/// holds a `.git` (a repo or another worktree), looking [`NESTED_REPO_DEPTH`] levels
/// deep without following symlinks. Also true when the walk reads more than
/// [`NESTED_REPO_BUDGET`] folders, since then it cannot tell.
pub fn holds_repo_in(root: &Path, ignored: &[String]) -> bool {
    let mut budget = NESTED_REPO_BUDGET;
    let mut queue: std::collections::VecDeque<(PathBuf, usize)> = ignored
        .iter()
        .filter(|p| p.ends_with('/'))
        .map(|p| (root.join(p.trim_end_matches('/')), 0))
        .collect();
    while let Some((dir, depth)) = queue.pop_front() {
        if budget == 0 {
            return true;
        }
        budget -= 1;
        let Ok(entries) = std::fs::read_dir(&dir) else {
            continue;
        };
        for entry in entries.flatten() {
            if entry.file_name() == ".git" {
                return true;
            }
            if depth < NESTED_REPO_DEPTH && entry.file_type().is_ok_and(|t| t.is_dir()) {
                queue.push_back((entry.path(), depth + 1));
            }
        }
    }
    false
}

/// The tracked files in the worktree at `path` whose changes git does not show: marked
/// assume-unchanged (a lowercase tag in `git ls-files -v`), or skip-worktree (`S`) and
/// present on disk, as a sparse checkout's files are not. `None` when git fails.
pub fn hidden_edits(path: &Path) -> Option<Vec<String>> {
    let out = git(path, &["ls-files", "-v", "-z"]).ok()??;
    Some(
        out.split(|&b| b == 0)
            .filter_map(|record| {
                let (&tag, rest) = record.split_first()?;
                let file = String::from_utf8_lossy(rest.strip_prefix(b" ")?).into_owned();
                let hidden = tag.is_ascii_lowercase()
                    || (tag == b'S' && std::fs::symlink_metadata(path.join(&file)).is_ok());
                hidden.then_some(file)
            })
            .collect(),
    )
}

/// True when the worktree whose git directory is `git_dir` is locked.
pub fn is_locked(git_dir: &Path) -> bool {
    std::fs::symlink_metadata(git_dir.join("locked")).is_ok()
}

/// The operation under way in the working tree whose own git directory is `git_dir`.
pub fn operation(git_dir: &Path) -> Option<GitOperation> {
    let has = |name: &str| std::fs::symlink_metadata(git_dir.join(name)).is_ok();
    if has("rebase-merge") || has("rebase-apply") {
        Some(GitOperation::Rebase)
    } else if has("MERGE_HEAD") {
        Some(GitOperation::Merge)
    } else if has("CHERRY_PICK_HEAD") {
        Some(GitOperation::CherryPick)
    } else if has("REVERT_HEAD") {
        Some(GitOperation::Revert)
    } else if has("BISECT_LOG") {
        Some(GitOperation::Bisect)
    } else {
        None
    }
}

/// Commits on HEAD that no remote-tracking ref (of any remote) has, at most
/// [`MAX_ONLY_HERE`]. `None` when git cannot count them (no commits).
pub fn only_here(path: &Path) -> Option<u32> {
    let max = format!("--max-count={MAX_ONLY_HERE}");
    let out = git(
        path,
        &["rev-list", "--count", &max, "HEAD", "--not", "--remotes"],
    )
    .ok()??;
    String::from_utf8_lossy(&out).trim().parse().ok()
}

/// The default branch, as its name and the full ref to compare with: the target of
/// `refs/remotes/origin/HEAD` (named without `origin/`), else local `main`, else local
/// `master`.
pub fn default_branch(path: &Path) -> Option<(String, String)> {
    let origin_head = git(path, &["symbolic-ref", "-q", "refs/remotes/origin/HEAD"])
        .ok()
        .flatten()
        .map(|out| String::from_utf8_lossy(&out).trim().to_owned())
        .filter(|r| r.starts_with("refs/remotes/origin/"));
    if let Some(full) = origin_head {
        let name = full["refs/remotes/origin/".len()..].to_owned();
        return Some((name, full));
    }
    ["main", "master"].iter().find_map(|name| {
        let full = format!("refs/heads/{name}");
        git(path, &["rev-parse", "--verify", "-q", &full])
            .ok()
            .flatten()
            .map(|_| ((*name).to_owned(), full))
    })
}

/// True when `ancestor` is an ancestor of (or the same commit as) `of`.
pub fn is_ancestor(path: &Path, ancestor: &str, of: &str) -> bool {
    git(path, &["merge-base", "--is-ancestor", ancestor, of]).is_ok_and(|out| out.is_some())
}

/// The ignored paths in `git status --porcelain -z --ignored`, in order.
pub fn parse_ignored_paths(out: &[u8]) -> Vec<String> {
    out.split(|&b| b == 0)
        .filter_map(|record| record.strip_prefix(b"!! "))
        .map(|path| String::from_utf8_lossy(path).into_owned())
        .collect()
}

/// The top-level entries of ignored paths, in order, at most [`MAX_IGNORED`]. A
/// directory, or a path inside one, is its first component with a trailing `/`.
pub fn top_level(paths: &[String]) -> Vec<String> {
    let mut entries: Vec<String> = Vec::new();
    for path in paths {
        let top = match path.split_once('/') {
            Some((first, _)) => format!("{first}/"),
            None => path.clone(),
        };
        if !entries.contains(&top) {
            entries.push(top);
            if entries.len() == MAX_IGNORED {
                break;
            }
        }
    }
    entries
}

/// Config for `git status` in `repo`.
///
/// On a Windows drive the index is usually written by Git for Windows, which leaves
/// inode, device and owner at zero. Linux git then takes every file for touched and
/// reads it in full, which on `/mnt/c` can nearly double the time `git status` takes.
/// There, files are compared by size and whole-second modification time only
/// (`core.checkStat=minimal`), which is what Git for Windows compares anyway. Contents
/// are still read for files changed in the second the index was written.
fn status_config(repo: &Path) -> &'static [&'static str] {
    if on_windows_drive(repo) {
        &["-c", "core.checkStat=minimal"]
    } else {
        &[]
    }
}

/// True when `path` is under `/mnt/` on a Windows drive mounted through drvfs (9p with
/// `aname=drvfs`, or `drvfs` itself on WSL 1), by the deepest mount in `/proc/mounts`
/// that holds it. Disks attached with `wsl --mount` (under `/mnt/wsl/`) are not. Without
/// `/proc/mounts`, any path under `/mnt/` counts.
pub fn on_windows_drive(path: &Path) -> bool {
    if !is_windows_mount(&path.to_string_lossy()) {
        return false;
    }
    match std::fs::read_to_string("/proc/mounts") {
        Ok(mounts) => drvfs_holds(&mounts, path),
        Err(_) => true,
    }
}

/// Whether the deepest mount in `mounts` (as `/proc/mounts` lists them) that holds
/// `path` is a drvfs one. The last of equally deep mounts wins, as it covers the others.
fn drvfs_holds(mounts: &str, path: &Path) -> bool {
    mounts
        .lines()
        .filter_map(|line| {
            let mut fields = line.split(' ');
            let (_, point, kind, options) = (
                fields.next()?,
                fields.next()?,
                fields.next()?,
                fields.next()?,
            );
            let point = unescape_mount(point);
            let drvfs = kind == "drvfs"
                || (kind == "9p" && options.split(',').any(|o| o.starts_with("aname=drvfs")));
            path.starts_with(&point).then_some((point.len(), drvfs))
        })
        .max_by_key(|(depth, _)| *depth)
        .is_some_and(|(_, drvfs)| drvfs)
}

/// Undoes the octal escapes (`\040` for a space) of a `/proc/mounts` field.
fn unescape_mount(field: &str) -> String {
    let bytes = field.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        let code = bytes
            .get(i + 1..i + 4)
            .filter(|_| bytes[i] == b'\\')
            .and_then(|d| u8::from_str_radix(std::str::from_utf8(d).ok()?, 8).ok());
        match code {
            Some(byte) => {
                out.push(byte);
                i += 4;
            }
            None => {
                out.push(bytes[i]);
                i += 1;
            }
        }
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// How `git status` reports untracked files in `repo`.
///
/// On a Linux file system every untracked file is listed (`all`), so an edit inside a
/// new directory matches a dirty entry. On a Windows drive ([`on_windows_drive`])
/// `git status` is slow (about 3 s per repo on `/mnt/c`, and listing untracked files
/// costs most of it), so there untracked files are not listed at all (`-uno`) and only
/// changes to tracked files count as dirty. A deliberate trade-off. Linux disks attached
/// under `/mnt/wsl/` list them as usual.
pub fn untracked_arg(repo: &Path) -> &'static str {
    untracked_arg_on(on_windows_drive(repo))
}

fn untracked_arg_on(windows_drive: bool) -> &'static str {
    if windows_drive {
        "-uno"
    } else {
        "--untracked-files=all"
    }
}

/// Reads the origin URL, last commit, tracked files, and the recent and unpushed commit
/// counts. A repo with no commits gives empty values rather than an error.
pub fn read_head(path: &Path) -> io::Result<HeadInfo> {
    read_head_unless(path, &AtomicBool::new(false))
}

/// Like [`read_head`], but fails instead of starting the next git call once `cancel` is
/// set.
pub fn read_head_unless(path: &Path, cancel: &AtomicBool) -> io::Result<HeadInfo> {
    let git = |path: &Path, args: &[&str]| {
        if cancel.load(Ordering::Relaxed) {
            return Err(io::Error::new(io::ErrorKind::Interrupted, "cancelled"));
        }
        git(path, args)
    };
    let origin_url = git(path, &["config", "--get", "remote.origin.url"])?
        .map(|out| redact_userinfo(String::from_utf8_lossy(&out).trim()))
        .filter(|url| !url.is_empty());
    let last_commit = git(path, &["log", "-1", "--format=%ct%x00%h%x00%s"])?
        .and_then(|out| parse_last_commit(&out));
    let tree = git(path, &["ls-tree", "-r", "-l", "-z", "HEAD"])?.unwrap_or_default();
    let files = parse_ls_tree(&tree);
    let commits_30d = git(
        path,
        &["rev-list", "--count", "--since=30.days.ago", "HEAD"],
    )?
    .and_then(|out| String::from_utf8_lossy(&out).trim().parse().ok())
    .unwrap_or(0);
    // Without an origin the count is unused, and with nothing to exclude it would walk
    // the whole history.
    let off_origin = match origin_url {
        Some(_) => git(
            path,
            &[
                "rev-list",
                "--count",
                "--max-count=1",
                "HEAD",
                "--not",
                "--remotes=origin",
            ],
        )?
        .and_then(|out| String::from_utf8_lossy(&out).trim().parse().ok())
        .unwrap_or(0),
        None => 0,
    };

    let language = main_language(files.iter().map(|(p, _)| p.as_str())).map(str::to_owned);
    let tracked_bytes = files.iter().map(|(_, size)| size).sum();
    let mut dirs = BTreeSet::from([String::new()]);
    for (file, _) in &files {
        let mut rest = file.as_str();
        while let Some((parent, _)) = rest.rsplit_once('/') {
            if !dirs.insert(parent.to_owned()) {
                break;
            }
            rest = parent;
        }
    }
    Ok(HeadInfo {
        origin_url,
        last_commit,
        language,
        tracked_bytes,
        commits_30d,
        off_origin,
        tracked_dirs: dirs.into_iter().collect(),
        worktree: scan::linked(path).map(|_| read_worktree_refs(path)),
    })
}

/// Drops credentials from a remote URL so they never go on the wire.
///
/// In a `scheme://` URL the user part is removed when it carries a password
/// (`user:token@`), and for `http`/`https` always, since a bare user there is usually a
/// token. A plain ssh user (`ssh://git@host/...`) and scp-like `git@host:path` URLs
/// are kept: they name no secret and are needed to clone.
pub fn redact_userinfo(url: &str) -> String {
    let Some((scheme, rest)) = url.split_once("://") else {
        return url.to_owned();
    };
    let authority_end = rest.find(['/', '?', '#']).unwrap_or(rest.len());
    let (authority, path) = rest.split_at(authority_end);
    let Some((user, host)) = authority.rsplit_once('@') else {
        return url.to_owned();
    };
    let web = scheme.eq_ignore_ascii_case("http") || scheme.eq_ignore_ascii_case("https");
    if user.contains(':') || web {
        format!("{scheme}://{host}{path}")
    } else {
        url.to_owned()
    }
}

/// Runs git in `repo`. `Ok(None)` means git ran and exited non-zero.
///
/// Git looks no higher than `repo`: past a `.git` it cannot read yet, it would otherwise
/// report the work tree around it, when a root lies inside one.
fn git(repo: &Path, args: &[&str]) -> io::Result<Option<Vec<u8>>> {
    let mut command = Command::new("git");
    command
        .arg("--no-optional-locks")
        .arg("-C")
        .arg(repo)
        .args(args)
        .env("GIT_OPTIONAL_LOCKS", "0")
        .env("GIT_TERMINAL_PROMPT", "0")
        .stdin(std::process::Stdio::null());
    if let Some(parent) = repo.parent() {
        command.env("GIT_CEILING_DIRECTORIES", parent);
    }
    let out = command.output()?;
    Ok(out.status.success().then_some(out.stdout))
}

/// Parses `git status --porcelain=v2 --branch -z`.
pub fn parse_status(out: &[u8]) -> Status {
    let mut status = Status::default();
    let mut records = out
        .split(|&b| b == 0)
        .map(|r| String::from_utf8_lossy(r).into_owned());
    while let Some(record) = records.next() {
        if let Some(header) = record.strip_prefix("# ") {
            let (key, value) = header.split_once(' ').unwrap_or((header, ""));
            match key {
                "branch.head" if value != "(detached)" => {
                    status.branch = Some(value.to_owned());
                }
                "branch.upstream" => status.upstream = true,
                "branch.ab" => {
                    let mut parts = value.split(' ');
                    let ahead = parts.next().and_then(|a| a.strip_prefix('+')?.parse().ok());
                    let behind = parts.next().and_then(|b| b.strip_prefix('-')?.parse().ok());
                    if let (Some(a), Some(b)) = (ahead, behind) {
                        status.ahead_behind = Some((a, b));
                    }
                }
                _ => {}
            }
            continue;
        }
        let path = match record.as_bytes().first() {
            Some(b'1') => record.splitn(9, ' ').nth(8),
            Some(b'2') => {
                // A rename or copy; the original path is the next record.
                if let Some(origin) = records.next().filter(|o| !o.is_empty()) {
                    status.origins.push(origin);
                }
                record.splitn(10, ' ').nth(9)
            }
            Some(b'u') => record.splitn(11, ' ').nth(10),
            Some(b'?') => record.get(2..),
            _ => None,
        };
        if let Some(path) = path {
            status.paths.push(path.to_owned());
        }
    }
    if !status.upstream {
        status.ahead_behind = None;
    }
    status
}

/// Parses `git log -1 --format=%ct%x00%h%x00%s`.
fn parse_last_commit(out: &[u8]) -> Option<LastCommit> {
    let text = String::from_utf8_lossy(out);
    let (time, rest) = text.trim_end_matches('\n').split_once('\0')?;
    let (sha, summary) = rest.split_once('\0')?;
    Some(LastCommit {
        summary: summary.to_owned(),
        time: time.trim().parse().ok()?,
        sha: Some(sha.to_owned()).filter(|s| !s.is_empty()),
    })
}

/// Parses `git ls-tree -r -l -z` into (path, size) for every blob.
pub fn parse_ls_tree(out: &[u8]) -> Vec<(String, u64)> {
    out.split(|&b| b == 0)
        .filter_map(|record| {
            let record = String::from_utf8_lossy(record);
            let (meta, path) = record.split_once('\t')?;
            let mut fields = meta.split_whitespace();
            let kind = fields.nth(1)?;
            let size = fields.nth(1)?;
            (kind == "blob").then(|| (path.to_owned(), size.parse().unwrap_or(0)))
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn z(records: &[&str]) -> Vec<u8> {
        let mut out = records.join("\0").into_bytes();
        out.push(0);
        out
    }

    const OID: &str = "4e1243bd22c66e76c2ba9eddc1f91394e57f9f83";

    #[test]
    fn clean_branch_with_upstream() {
        let s = parse_status(&z(&[
            &format!("# branch.oid {OID}"),
            "# branch.head main",
            "# branch.upstream origin/main",
            "# branch.ab +2 -1",
        ]));
        assert_eq!(s.branch.as_deref(), Some("main"));
        assert!(s.upstream);
        assert_eq!(s.ahead_behind, Some((2, 1)));
        assert!(s.paths.is_empty());
    }

    #[test]
    fn changed_renamed_conflicted_and_untracked_paths() {
        let s = parse_status(&z(&[
            "# branch.oid (initial)",
            "# branch.head main",
            &format!("1 .M N... 100644 100644 100644 {OID} {OID} src/tide tables.rs"),
            &format!("2 R. N... 100644 100644 100644 {OID} {OID} R100 src/new.rs"),
            "src/old.rs",
            &format!("u UU N... 100644 100644 100644 100644 {OID} {OID} {OID} Cargo.toml"),
            "? notes/",
            "? scratch.txt",
        ]));
        assert_eq!(
            s.paths,
            [
                "src/tide tables.rs",
                "src/new.rs",
                "Cargo.toml",
                "notes/",
                "scratch.txt"
            ]
        );
        assert_eq!(s.origins, ["src/old.rs"]);
        assert_eq!(s.ahead_behind, None);
    }

    #[test]
    fn ignored_entries_are_top_level_and_capped() {
        let out = z(&[
            "?? notes.txt",
            "!! .env",
            "!! target/",
            "!! sub/build.log",
            "!! sub/cache/",
            "!! a b.tmp",
        ]);
        assert_eq!(
            top_level(&parse_ignored_paths(&out)),
            [".env", "target/", "sub/", "a b.tmp"]
        );
        let many: Vec<String> = (0..12).map(|i| format!("!! f{i}.log")).collect();
        let many: Vec<&str> = many.iter().map(String::as_str).collect();
        assert_eq!(
            top_level(&parse_ignored_paths(&z(&many))).len(),
            MAX_IGNORED
        );
    }

    #[test]
    fn detached_head_has_no_branch() {
        let s = parse_status(&z(&[
            &format!("# branch.oid {OID}"),
            "# branch.head (detached)",
        ]));
        assert_eq!(s.branch, None);
    }

    #[test]
    fn gone_upstream_has_no_counts() {
        let s = parse_status(&z(&["# branch.head main", "# branch.upstream origin/main"]));
        assert!(s.upstream);
        assert_eq!(s.ahead_behind, None);
        // Counts without an upstream header are ignored.
        let s = parse_status(&z(&["# branch.head main", "# branch.ab +1 -0"]));
        assert!(!s.upstream);
        assert_eq!(s.ahead_behind, None);
    }

    #[test]
    fn untracked_files_are_listed_except_on_windows_drives() {
        assert_eq!(
            untracked_arg(Path::new("/home/dev/projects/tidepool-api")),
            "--untracked-files=all"
        );
        assert_eq!(untracked_arg_on(true), "-uno");
        assert_eq!(untracked_arg_on(false), "--untracked-files=all");
    }

    #[test]
    fn stat_checks_are_minimal_only_on_windows_drives() {
        assert!(status_config(Path::new("/home/dev/projects/tidepool-api")).is_empty());
    }

    #[test]
    fn windows_drives_are_drvfs_mounts() {
        let mounts = "\
none / ext4 rw,relatime 0 0
C:\\134 /mnt/c 9p rw,noatime,aname=drvfs;path=C:\\;uid=1000;gid=1000,cache=0x5 0 0
D:\\134 /mnt/d drvfs rw,noatime 0 0
/dev/sdd1 /mnt/wsl/kelp\\040disk ext4 rw,relatime 0 0
none /mnt/wsl tmpfs rw,relatime 0 0
";
        for (path, drvfs) in [
            ("/mnt/c/Users/dev/tidepool-api", true),
            ("/mnt/d/aurora-notes", true),
            ("/mnt/wsl/kelp disk/kelp-cli", false),
            ("/mnt/wsl/aurora-notes", false),
            ("/mnt/cache/tidepool-api", false),
            ("/home/dev/projects/kelp-cli", false),
        ] {
            assert_eq!(drvfs_holds(mounts, Path::new(path)), drvfs, "{path}");
        }
        assert!(!on_windows_drive(Path::new("/home/dev/projects/kelp-cli")));
    }

    #[test]
    fn credentials_are_removed_from_origin_urls() {
        assert_eq!(
            redact_userinfo("https://dev:ghp_inventedtoken@github.com/dev/tidepool-api.git"),
            "https://github.com/dev/tidepool-api.git"
        );
        assert_eq!(
            redact_userinfo("https://ghp_inventedtoken@github.com/dev/tidepool-api.git"),
            "https://github.com/dev/tidepool-api.git"
        );
        assert_eq!(
            redact_userinfo("ssh://dev:secret@git.example.com:2222/dev/kelp-cli.git"),
            "ssh://git.example.com:2222/dev/kelp-cli.git"
        );
        for kept in [
            "ssh://git@github.com/dev/tidepool-api.git",
            "git@github.com:dev/tidepool-api.git",
            "https://github.com/dev/tidepool-api.git",
            "/home/dev/remotes/tidepool-api.git",
            "https://github.com/dev/a@b.git",
        ] {
            assert_eq!(redact_userinfo(kept), kept);
        }
    }

    #[test]
    fn ls_tree_keeps_blobs_with_sizes() {
        let files = parse_ls_tree(&z(&[
            &format!("100644 blob {OID}     120\tREADME.md"),
            &format!("100755 blob {OID}  48213\tsrc/main file.rs"),
            &format!("160000 commit {OID}       -\tvendor/lib"),
        ]));
        assert_eq!(
            files,
            [
                ("README.md".to_owned(), 120),
                ("src/main file.rs".to_owned(), 48213)
            ]
        );
    }

    #[test]
    fn last_commit_parses() {
        assert_eq!(
            parse_last_commit(b"1773489600\0a1b2c3d\0Add tide tables\n"),
            Some(LastCommit {
                summary: "Add tide tables".into(),
                time: 1_773_489_600,
                sha: Some("a1b2c3d".into()),
            })
        );
        assert_eq!(parse_last_commit(b""), None);
    }
}
