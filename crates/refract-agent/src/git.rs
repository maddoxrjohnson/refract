//! Repo facts from the `git` command line.
//!
//! Every command runs with `--no-optional-locks`, so reading status never rewrites the
//! index. That matters because the index is watched: a write would trigger another
//! refresh.

use std::collections::BTreeSet;
use std::io;
use std::path::Path;
use std::process::Command;
use std::sync::atomic::{AtomicBool, Ordering};

use refract_core::language::main_language;
use refract_core::protocol::LastCommit;
use refract_core::sessions::is_windows_mount;

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
}

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
    Ok(parse_status(&out))
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
