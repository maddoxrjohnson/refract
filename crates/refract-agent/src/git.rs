//! Repo facts from the `git` command line.
//!
//! Every command runs with `--no-optional-locks`, so reading status never rewrites the
//! index. That matters because the index is watched: a write would trigger another
//! refresh.

use std::collections::BTreeSet;
use std::io;
use std::path::Path;
use std::process::Command;

use refract_core::language::main_language;
use refract_core::protocol::LastCommit;
use refract_core::sessions::is_windows_mount;

/// What `git status` says: cheap, refreshed on every working-tree change.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Status {
    /// `None` when HEAD is detached.
    pub branch: Option<String>,
    /// Ahead and behind the upstream; `None` without an upstream (or a gone one).
    pub ahead_behind: Option<(u32, u32)>,
    /// Changed, staged, conflicted and untracked paths, relative to the repo root. An
    /// untracked directory git still reports as a whole (such as a nested repo) keeps
    /// its trailing `/`.
    pub paths: Vec<String>,
}

/// Facts about HEAD: refreshed when HEAD or refs move.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct HeadInfo {
    pub origin_url: Option<String>,
    pub last_commit: Option<LastCommit>,
    pub language: Option<String>,
    pub tracked_bytes: u64,
    pub commits_30d: u32,
    /// Directories that hold tracked files, relative to the root (`""` is the root).
    pub tracked_dirs: Vec<String>,
}

/// Reads `git status`. Fails if git cannot run or `path` is not a repo.
pub fn read_status(path: &Path) -> io::Result<Status> {
    let out = git(
        path,
        &[
            "status",
            "--porcelain=v2",
            "--branch",
            "-z",
            "--ignore-submodules=dirty",
            untracked_arg(path),
        ],
    )?
    .ok_or_else(|| io::Error::other("git status failed"))?;
    Ok(parse_status(&out))
}

/// How `git status` reports untracked files in `repo`.
///
/// On the Linux file system every untracked file is listed (`all`), so an edit inside a
/// new directory matches a dirty entry. Repos under `/mnt/` live on a Windows drive,
/// where `git status` is slow (about 3 s per repo on `/mnt/c`, and listing untracked
/// files costs most of it), so there untracked files are not listed at all (`-uno`) and
/// only changes to tracked files count as dirty. A deliberate trade-off.
pub fn untracked_arg(repo: &Path) -> &'static str {
    if is_windows_mount(&repo.to_string_lossy()) {
        "-uno"
    } else {
        "--untracked-files=all"
    }
}

/// Reads the origin URL, last commit, tracked files and recent commit count. A repo with
/// no commits gives empty values rather than an error.
pub fn read_head(path: &Path) -> io::Result<HeadInfo> {
    let origin_url = git(path, &["config", "--get", "remote.origin.url"])?
        .map(|out| redact_userinfo(String::from_utf8_lossy(&out).trim()))
        .filter(|url| !url.is_empty());
    let last_commit =
        git(path, &["log", "-1", "--format=%ct%x00%s"])?.and_then(|out| parse_last_commit(&out));
    let tree = git(path, &["ls-tree", "-r", "-l", "-z", "HEAD"])?.unwrap_or_default();
    let files = parse_ls_tree(&tree);
    let commits_30d = git(
        path,
        &["rev-list", "--count", "--since=30.days.ago", "HEAD"],
    )?
    .and_then(|out| String::from_utf8_lossy(&out).trim().parse().ok())
    .unwrap_or(0);

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
fn git(repo: &Path, args: &[&str]) -> io::Result<Option<Vec<u8>>> {
    let out = Command::new("git")
        .arg("--no-optional-locks")
        .arg("-C")
        .arg(repo)
        .args(args)
        .env("GIT_OPTIONAL_LOCKS", "0")
        .env("GIT_TERMINAL_PROMPT", "0")
        .stdin(std::process::Stdio::null())
        .output()?;
    Ok(out.status.success().then_some(out.stdout))
}

/// Parses `git status --porcelain=v2 --branch -z`.
pub fn parse_status(out: &[u8]) -> Status {
    let mut status = Status::default();
    let mut upstream = false;
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
                "branch.upstream" => upstream = true,
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
                records.next();
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
    if !upstream {
        status.ahead_behind = None;
    }
    status
}

/// Parses `git log -1 --format=%ct%x00%s`.
fn parse_last_commit(out: &[u8]) -> Option<LastCommit> {
    let text = String::from_utf8_lossy(out);
    let (time, summary) = text.trim_end_matches('\n').split_once('\0')?;
    Some(LastCommit {
        summary: summary.to_owned(),
        time: time.trim().parse().ok()?,
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
        assert_eq!(s.ahead_behind, None);
    }

    #[test]
    fn untracked_files_are_listed_except_on_windows_drives() {
        assert_eq!(
            untracked_arg(Path::new("/home/dev/projects/tidepool-api")),
            "--untracked-files=all"
        );
        assert_eq!(
            untracked_arg(Path::new("/mnt/c/Users/dev/tidepool-api")),
            "-uno"
        );
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
            parse_last_commit(b"1773489600\0Add tide tables\n"),
            Some(LastCommit {
                summary: "Add tide tables".into(),
                time: 1_773_489_600
            })
        );
        assert_eq!(parse_last_commit(b""), None);
    }
}
