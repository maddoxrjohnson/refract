//! The git snapshot G0 of a repo and its comparison with the copy's (spec §4.2, §5
//! check 2).
//!
//! Each field is parsed from one git command's output, run in the repo's working tree
//! (on Windows with `git.exe`, in the copy with Linux git):
//!
//! | Field | Command | Parser |
//! |---|---|---|
//! | `head` | `git rev-parse HEAD` | [`parse_head`] |
//! | `refs` | `git for-each-ref --format=<`[`FOR_EACH_REF_FORMAT`]`>` | [`parse_refs`] |
//! | `stash` | `git stash list --format=%H` | [`parse_lines`] |
//! | `config` | `git config --local -l` | [`super::gitconfig::parse_config`] |
//! | `status` | `git status --porcelain=v2 -z --untracked-files=all --ignored=matching` | [`parse_status`] |
//! | `index` | `git ls-files -s -z` | [`parse_nul_records`] |
//! | `submodules` | `git submodule status --recursive` | [`parse_submodules`] |
//! | `fsck` | `git fsck --full` (stdout and stderr together) | [`parse_fsck`] |
//!
//! Outputs are taken as text: git writes paths as UTF-8 on both sides, and output that
//! is not UTF-8 means a path the migration cannot represent.
//!
//! `refs` covers every ref, `refs/stash` and remote-tracking refs included, with each
//! branch's upstream. Remotes themselves are compared through `config`.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

use crate::migrate::gitconfig::{check_config, ConfigEntry};
use crate::migrate::manifest::RelPath;
use crate::migrate::report::Problem;

/// The `--format` passed to `git for-each-ref`: object id, ref name, upstream and the
/// target of a symbolic ref, tab-separated (ref names can't hold tabs). Upstream and
/// target are empty when there is none.
pub const FOR_EACH_REF_FORMAT: &str = "%(objectname)%09%(refname)%09%(upstream)%09%(symref)";

/// Everything compared about one repo's git state.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct GitSnapshot {
    /// The commit HEAD names; `None` in a repo with no commits.
    #[serde(default)]
    pub head: Option<String>,
    #[serde(default)]
    pub refs: Vec<RefEntry>,
    /// Stash commits, newest first.
    #[serde(default)]
    pub stash: Vec<String>,
    #[serde(default)]
    pub config: Vec<ConfigEntry>,
    #[serde(default)]
    pub status: Vec<StatusEntry>,
    /// `ls-files -s` records: `<mode> <object> <stage>\t<path>`.
    #[serde(default)]
    pub index: Vec<String>,
    #[serde(default)]
    pub submodules: Vec<SubmoduleEntry>,
    /// fsck's problem lines (dangling objects and progress are not problems).
    #[serde(default)]
    pub fsck: Vec<String>,
}

impl GitSnapshot {
    /// Each remote's name and URL, from `config`.
    pub fn remotes(&self) -> Vec<(String, String)> {
        self.config
            .iter()
            .filter_map(|entry| {
                let rest = entry.key.strip_prefix("remote.")?;
                let (name, var) = rest.rsplit_once('.')?;
                if !var.eq_ignore_ascii_case("url") {
                    return None;
                }
                Some((name.to_owned(), entry.value.clone()?))
            })
            .collect()
    }
}

/// One ref.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RefEntry {
    pub name: String,
    pub object: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub upstream: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub symref: Option<String>,
}

/// What kind of `git status --porcelain=v2` record a [`StatusEntry`] is.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum StatusKind {
    /// `1`: changed.
    Changed,
    /// `2`: renamed or copied.
    Renamed,
    /// `u`: unmerged.
    Unmerged,
    /// `?`
    Untracked,
    /// `!`
    Ignored,
}

/// One `git status --porcelain=v2` record.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct StatusEntry {
    pub kind: StatusKind,
    /// The fields between the kind and the path (XY, submodule state, modes, object
    /// ids, score), space-separated as git wrote them; empty for `?` and `!`.
    pub fields: String,
    /// The path; an ignored or untracked folder ends in `/`.
    pub path: String,
    /// The original path of a rename or copy.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub orig_path: Option<String>,
}

impl StatusEntry {
    /// The record as git prints it without `-z`, for messages.
    fn show(&self) -> String {
        let kind = match self.kind {
            StatusKind::Changed => "1",
            StatusKind::Renamed => "2",
            StatusKind::Unmerged => "u",
            StatusKind::Untracked => "?",
            StatusKind::Ignored => "!",
        };
        let mut out = kind.to_owned();
        for part in [self.fields.as_str(), self.path.as_str()] {
            if !part.is_empty() {
                out.push(' ');
                out.push_str(part);
            }
        }
        if let Some(orig) = &self.orig_path {
            out.push_str(" <- ");
            out.push_str(orig);
        }
        out
    }
}

/// One `git submodule status --recursive` line.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SubmoduleEntry {
    /// The leading character: ` ` (in sync), `-` (not initialised), `+` (other commit
    /// checked out) or `U` (conflicts).
    pub state: String,
    pub object: String,
    pub path: String,
}

/// Reads `git rev-parse HEAD`: `None` unless it is an object id (an unborn HEAD prints
/// `HEAD`).
pub fn parse_head(text: &str) -> Option<String> {
    let head = text.trim();
    let is_id = matches!(head.len(), 40 | 64) && head.bytes().all(|b| b.is_ascii_hexdigit());
    is_id.then(|| head.to_owned())
}

/// Reads `git for-each-ref --format=`[`FOR_EACH_REF_FORMAT`]. Fails on a line without
/// four fields.
pub fn parse_refs(text: &str) -> Result<Vec<RefEntry>, String> {
    let optional = |s: &str| (!s.is_empty()).then(|| s.to_owned());
    text.lines()
        .filter(|line| !line.is_empty())
        .map(
            |line| match line.split('\t').collect::<Vec<_>>().as_slice() {
                [object, name, upstream, symref] => Ok(RefEntry {
                    name: (*name).into(),
                    object: (*object).into(),
                    upstream: optional(upstream),
                    symref: optional(symref),
                }),
                _ => Err(format!("unexpected for-each-ref line: {line:?}")),
            },
        )
        .collect()
}

/// Non-empty lines, trimmed of trailing whitespace.
pub fn parse_lines(text: &str) -> Vec<String> {
    text.lines()
        .map(str::trim_end)
        .filter(|line| !line.is_empty())
        .map(str::to_owned)
        .collect()
}

/// Non-empty NUL-terminated records.
pub fn parse_nul_records(text: &str) -> Vec<String> {
    text.split('\0')
        .filter(|record| !record.is_empty())
        .map(str::to_owned)
        .collect()
}

/// Reads `git status --porcelain=v2 -z`. Header lines (`#`) are skipped. Fails on a
/// record it does not understand.
pub fn parse_status(text: &str) -> Result<Vec<StatusEntry>, String> {
    let mut records = text.split('\0').filter(|r| !r.is_empty());
    let mut entries = Vec::new();
    while let Some(record) = records.next() {
        let bad = || format!("unexpected status record: {record:?}");
        let (kind, field_count) = match record.as_bytes()[0] {
            b'#' => continue,
            b'1' => (StatusKind::Changed, 7),
            b'2' => (StatusKind::Renamed, 8),
            b'u' => (StatusKind::Unmerged, 9),
            b'?' => (StatusKind::Untracked, 0),
            b'!' => (StatusKind::Ignored, 0),
            _ => return Err(bad()),
        };
        let rest = record
            .get(2..)
            .filter(|_| record.as_bytes().get(1) == Some(&b' '));
        let rest = rest.ok_or_else(bad)?;
        let parts: Vec<&str> = rest.splitn(field_count + 1, ' ').collect();
        if parts.len() != field_count + 1 || parts[field_count].is_empty() {
            return Err(bad());
        }
        let orig_path = if kind == StatusKind::Renamed {
            Some(records.next().ok_or_else(bad)?.to_owned())
        } else {
            None
        };
        entries.push(StatusEntry {
            kind,
            fields: parts[..field_count].join(" "),
            path: parts[field_count].to_owned(),
            orig_path,
        });
    }
    Ok(entries)
}

/// Reads `git submodule status --recursive`. The trailing `(describe)` is dropped.
pub fn parse_submodules(text: &str) -> Result<Vec<SubmoduleEntry>, String> {
    text.lines()
        .filter(|line| !line.trim().is_empty())
        .map(|line| {
            let bad = || format!("unexpected submodule status line: {line:?}");
            let state = line.get(..1).ok_or_else(bad)?;
            let (object, rest) = line[1..].split_once(' ').ok_or_else(bad)?;
            // The path is followed by ` (<describe>)` when git could describe the commit.
            let path = match rest.rsplit_once(" (") {
                Some((path, describe)) if describe.ends_with(')') => path,
                _ => rest,
            };
            Ok(SubmoduleEntry {
                state: state.into(),
                object: object.into(),
                path: path.into(),
            })
        })
        .collect()
}

/// The problem lines of `git fsck --full`: everything except dangling objects,
/// progress (`Checking ...`) and blank lines. Dangling objects are normal, and
/// normalising adds dangling blobs.
pub fn parse_fsck(text: &str) -> Vec<String> {
    parse_lines(text)
        .into_iter()
        .filter(|line| !line.starts_with("dangling ") && !line.starts_with("Checking "))
        .collect()
}

/// Compares the copy's git state with the source's (spec §5 check 2). Ignored status
/// entries under `skipped` folders are left out of the source's, since those folders
/// are not copied. fsck problems already in the source are allowed; only new ones
/// fail. Returns each difference, with a path where there is one.
pub fn compare(source: &GitSnapshot, copy: &GitSnapshot, skipped: &[RelPath]) -> Vec<Problem> {
    let mut problems = Vec::new();
    let mut fail = |path: Option<&str>, message: String| {
        problems.push(Problem {
            path: path.map(str::to_owned),
            message,
        })
    };

    if source.head != copy.head {
        let show = |head: &Option<String>| head.clone().unwrap_or_else(|| "unborn".into());
        fail(
            Some("HEAD"),
            format!(
                "HEAD is {} in the original but {} in the copy.",
                show(&source.head),
                show(&copy.head)
            ),
        );
    }

    let theirs: BTreeMap<&str, &RefEntry> =
        copy.refs.iter().map(|r| (r.name.as_str(), r)).collect();
    let ours: BTreeMap<&str, &RefEntry> =
        source.refs.iter().map(|r| (r.name.as_str(), r)).collect();
    for (name, ref_) in &ours {
        match theirs.get(name) {
            None => fail(Some(name), "This ref is missing from the copy.".into()),
            Some(other) if other != ref_ => fail(
                Some(name),
                "This ref points elsewhere, or has another upstream, in the copy.".into(),
            ),
            Some(_) => {}
        }
    }
    for name in theirs.keys().filter(|name| !ours.contains_key(*name)) {
        fail(
            Some(name),
            "This ref is in the copy but not in the original.".into(),
        );
    }

    if source.stash != copy.stash {
        fail(
            Some("refs/stash"),
            "The stash list differs in the copy.".into(),
        );
    }

    if let Err(config) = check_config(&source.config, &copy.config) {
        for message in config {
            fail(Some(".git/config"), message);
        }
    }

    let under_skipped = |entry: &StatusEntry| {
        let path = entry.path.trim_end_matches('/');
        entry.kind == StatusKind::Ignored
            && skipped.iter().any(|dir| {
                path == dir.as_str()
                    || path
                        .strip_prefix(dir.as_str())
                        .is_some_and(|rest| rest.starts_with('/'))
            })
    };
    let source_status: Vec<StatusEntry> = source
        .status
        .iter()
        .filter(|e| !under_skipped(e))
        .cloned()
        .collect();
    let (missing, extra) = multiset_diff(&source_status, &copy.status);
    for entry in missing {
        fail(
            Some(&entry.path),
            format!("git status in the copy lacks `{}`.", entry.show()),
        );
    }
    for entry in extra {
        fail(
            Some(&entry.path),
            format!("git status in the copy adds `{}`.", entry.show()),
        );
    }

    let (missing, extra) = multiset_diff(&source.index, &copy.index);
    for record in missing.iter().chain(&extra) {
        let path = record.split_once('\t').map_or(record.as_str(), |(_, p)| p);
        fail(Some(path), "The index entry differs in the copy.".into());
    }

    let (missing, extra) = multiset_diff(&source.submodules, &copy.submodules);
    for entry in missing.iter().chain(&extra) {
        fail(
            Some(&entry.path),
            "The submodule's state differs in the copy.".into(),
        );
    }

    for line in copy.fsck.iter().filter(|line| !source.fsck.contains(line)) {
        fail(None, format!("git fsck reports a new problem: {line}"));
    }
    problems
}

/// Items of `a` not matched in `b`, and items of `b` not matched in `a`, counting
/// repeats; each sorted.
fn multiset_diff<T: Ord + Clone>(a: &[T], b: &[T]) -> (Vec<T>, Vec<T>) {
    let mut a = a.to_vec();
    let mut b = b.to_vec();
    a.sort();
    b.sort();
    let (mut missing, mut extra) = (Vec::new(), Vec::new());
    let (mut i, mut j) = (0, 0);
    while i < a.len() || j < b.len() {
        match (a.get(i), b.get(j)) {
            (Some(x), Some(y)) if x == y => (i, j) = (i + 1, j + 1),
            (Some(x), Some(y)) if x < y => {
                missing.push(x.clone());
                i += 1;
            }
            (Some(_), Some(y)) => {
                extra.push(y.clone());
                j += 1;
            }
            (Some(x), None) => {
                missing.push(x.clone());
                i += 1;
            }
            (None, Some(y)) => {
                extra.push(y.clone());
                j += 1;
            }
            (None, None) => break,
        }
    }
    (missing, extra)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::migrate::gitconfig::parse_config;

    const MAIN: &str = "1f2e3d4c5b6a79881f2e3d4c5b6a79881f2e3d4c";
    const FEAT: &str = "aa11bb22cc33dd44ee55ff66aa11bb22cc33dd44";
    const STASH: &str = "0123456789abcdef0123456789abcdef01234567";

    fn refs_text() -> String {
        format!(
            "{MAIN}\trefs/heads/main\trefs/remotes/origin/main\t\n\
             {FEAT}\trefs/heads/tide-tables\t\t\n\
             {MAIN}\trefs/remotes/origin/HEAD\t\trefs/remotes/origin/main\n\
             {MAIN}\trefs/remotes/origin/main\t\t\n\
             {STASH}\trefs/stash\t\t\n"
        )
    }

    const STATUS: &str = "1 .M N... 100644 100644 100644 e69de29bb2d1d6434b8b29ae775ad8c2e48c5391 e69de29bb2d1d6434b8b29ae775ad8c2e48c5391 src/tide tables.rs\0\
        2 R. N... 100644 100644 100644 e69de29bb2d1d6434b8b29ae775ad8c2e48c5391 e69de29bb2d1d6434b8b29ae775ad8c2e48c5391 R100 docs/new.md\0docs/old.md\0\
        ? notes.txt\0\
        ! .env\0\
        ! node_modules/\0\
        ! packages/tidepool-ui/dist/\0";

    fn snapshot() -> GitSnapshot {
        GitSnapshot {
            head: parse_head(&format!("{MAIN}\n")),
            refs: parse_refs(&refs_text()).unwrap(),
            stash: parse_lines(&format!("{STASH}\n")),
            config: parse_config("core.autocrlf=true\ncore.filemode=false\nremote.origin.url=https://example.com/dev/aurora-notes.git\n"),
            status: parse_status(STATUS).unwrap(),
            index: parse_nul_records(&format!(
                "100644 {MAIN} 0\tREADME.md\0100755 {FEAT} 0\tscripts/run.sh\0"
            )),
            submodules: parse_submodules(&format!(
                " {FEAT} libs/kelp (v1.2.0)\n-{MAIN} libs/reef\n"
            ))
            .unwrap(),
            fsck: parse_fsck(&format!(
                "Checking object directories: 100% (256/256), done.\n\
                 dangling blob {FEAT}\n\
                 warning in tree {MAIN}: zeroPaddedFilemode: contains zero-padded file modes\n"
            )),
        }
    }

    /// The copy as it should be: same state, Linux config, skipped folders gone.
    fn copy() -> GitSnapshot {
        let mut copy = snapshot();
        copy.config = parse_config(
            "remote.origin.url=https://example.com/dev/aurora-notes.git\ncore.filemode=true\n",
        );
        copy.status
            .retain(|e| e.path != "node_modules/" && e.path != "packages/tidepool-ui/dist/");
        copy
    }

    fn skipped() -> Vec<RelPath> {
        vec![
            RelPath::new("node_modules").unwrap(),
            RelPath::new("packages/tidepool-ui/dist").unwrap(),
        ]
    }

    #[test]
    fn outputs_parse() {
        let snap = snapshot();
        assert_eq!(snap.head.as_deref(), Some(MAIN));
        assert_eq!(parse_head("HEAD\n"), None);
        assert_eq!(snap.refs.len(), 5);
        assert_eq!(
            snap.refs[0],
            RefEntry {
                name: "refs/heads/main".into(),
                object: MAIN.into(),
                upstream: Some("refs/remotes/origin/main".into()),
                symref: None,
            }
        );
        assert_eq!(
            snap.refs[2].symref.as_deref(),
            Some("refs/remotes/origin/main")
        );
        assert!(parse_refs("abc refs/heads/main\n").is_err());

        assert_eq!(snap.status.len(), 6);
        assert_eq!(snap.status[0].kind, StatusKind::Changed);
        assert_eq!(snap.status[0].path, "src/tide tables.rs");
        assert_eq!(snap.status[1].kind, StatusKind::Renamed);
        assert_eq!(snap.status[1].path, "docs/new.md");
        assert_eq!(snap.status[1].orig_path.as_deref(), Some("docs/old.md"));
        assert!(snap.status[1].fields.ends_with("R100"));
        assert_eq!(snap.status[2].kind, StatusKind::Untracked);
        assert_eq!(snap.status[4].path, "node_modules/");

        assert_eq!(snap.index[1], format!("100755 {FEAT} 0\tscripts/run.sh"));
        assert_eq!(snap.submodules[0].path, "libs/kelp");
        assert_eq!(snap.submodules[0].state, " ");
        assert_eq!(snap.submodules[1].state, "-");
        assert_eq!(snap.fsck.len(), 1);
        assert!(snap.fsck[0].starts_with("warning in tree"));
        assert_eq!(
            snap.remotes(),
            vec![(
                "origin".to_string(),
                "https://example.com/dev/aurora-notes.git".to_string()
            )]
        );
    }

    #[test]
    fn unmerged_records_and_headers_parse_and_junk_fails() {
        let text = format!(
            "# branch.oid {MAIN}\0u UU N... 100644 100644 100644 100644 {MAIN} {FEAT} {STASH} src/merge me.rs\0"
        );
        let entries = parse_status(&text).unwrap();
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].kind, StatusKind::Unmerged);
        assert_eq!(entries[0].path, "src/merge me.rs");
        assert!(parse_status("x weird\0").is_err());
        assert!(parse_status("1 .M N...\0").is_err());
        assert!(parse_status("?notes.txt\0").is_err());
        // A rename without its original path.
        assert!(parse_status(&STATUS[..STATUS.find("docs/new.md").unwrap() + 12]).is_err());
    }

    #[test]
    fn an_identical_copy_passes_with_skipped_folders_excluded() {
        assert_eq!(compare(&snapshot(), &copy(), &skipped()), vec![]);
        // Without the skipped list, the missing ignored folders are reported.
        let problems = compare(&snapshot(), &copy(), &[]);
        let paths: Vec<_> = problems.iter().filter_map(|p| p.path.as_deref()).collect();
        assert_eq!(paths, vec!["node_modules/", "packages/tidepool-ui/dist/"]);
    }

    #[test]
    fn an_ignored_file_outside_skipped_folders_must_stay() {
        let mut copy = copy();
        copy.status.retain(|e| e.path != ".env");
        let problems = compare(&snapshot(), &copy, &skipped());
        assert_eq!(problems.len(), 1);
        assert_eq!(problems[0].path.as_deref(), Some(".env"));
        assert_eq!(
            problems[0].message,
            "git status in the copy lacks `! .env`."
        );
    }

    #[test]
    fn a_dropped_or_moved_ref_fails() {
        let mut copy = copy();
        copy.refs.retain(|r| r.name != "refs/heads/tide-tables");
        copy.refs[0].upstream = None;
        copy.refs.push(RefEntry {
            name: "refs/heads/stray".into(),
            object: MAIN.into(),
            upstream: None,
            symref: None,
        });
        let problems = compare(&snapshot(), &copy, &skipped());
        let paths: Vec<_> = problems.iter().filter_map(|p| p.path.as_deref()).collect();
        assert_eq!(
            paths,
            vec![
                "refs/heads/main",
                "refs/heads/tide-tables",
                "refs/heads/stray"
            ]
        );
    }

    #[test]
    fn head_stash_index_and_submodules_are_compared() {
        let mut copy = copy();
        copy.head = None;
        copy.stash.clear();
        copy.index[1] = format!("100644 {FEAT} 0\tscripts/run.sh");
        copy.submodules[0].state = "+".into();
        let problems = compare(&snapshot(), &copy, &skipped());
        let paths: Vec<_> = problems.iter().filter_map(|p| p.path.as_deref()).collect();
        assert_eq!(
            paths,
            vec![
                "HEAD",
                "refs/stash",
                "scripts/run.sh",
                "scripts/run.sh",
                "libs/kelp",
                "libs/kelp"
            ]
        );
        assert!(problems[0].message.contains("unborn"));
    }

    #[test]
    fn only_new_fsck_problems_fail() {
        // The pre-existing warning is still there: fine.
        let copy = copy();
        assert!(compare(&snapshot(), &copy, &skipped()).is_empty());
        let mut broken = copy;
        broken.fsck.push(format!("missing blob {STASH}"));
        let problems = compare(&snapshot(), &broken, &skipped());
        assert_eq!(problems.len(), 1);
        assert_eq!(
            problems[0].message,
            format!("git fsck reports a new problem: missing blob {STASH}")
        );
        // A problem that went away is not a failure.
        let mut fixed = self::copy();
        fixed.fsck.clear();
        assert!(compare(&snapshot(), &fixed, &skipped()).is_empty());
    }

    #[test]
    fn an_unexpected_config_change_fails() {
        let mut copy = copy();
        copy.config.push(ConfigEntry {
            key: "user.email".into(),
            value: Some("dev@example.com".into()),
        });
        let problems = compare(&snapshot(), &copy, &skipped());
        assert_eq!(problems.len(), 1);
        assert_eq!(problems[0].path.as_deref(), Some(".git/config"));
    }

    #[test]
    fn multisets_count_repeats() {
        let (missing, extra) = multiset_diff(&[1, 1, 2], &[1, 3]);
        assert_eq!((missing, extra), (vec![1, 2], vec![3]));
    }
}
