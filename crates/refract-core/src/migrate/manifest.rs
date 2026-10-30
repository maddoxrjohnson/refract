//! Manifests of a repo folder and how a copy's manifest is checked against the
//! source's (spec §4.2, §5 checks 1 and 3), plus the stat snapshots that show whether
//! the source changed meanwhile (check 4).
//!
//! Paths are [`RelPath`]s: relative, `/`-separated and valid Unicode. Windows names
//! are UTF-16 and Linux names are bytes; a name that does not convert losslessly
//! (an unpaired surrogate, bytes that are not UTF-8) cannot be represented, is kept
//! in [`Manifest::unrepresentable`] instead, and fails the comparison.
//!
//! The comparison rules, by where a path lies:
//! - **Inside a git directory** (`.git`, a submodule's `.git/modules/<name>`, or any
//!   folder inside a `.git` that holds `HEAD`, `objects/` and `refs/`): byte-identical
//!   by SHA-256 and size, except `index` and `config`, which may differ (they are
//!   checked by [`super::gitstate::compare`]), and new loose objects
//!   (`objects/xx/<rest of the id>`), which the copy may add. Both are listed.
//! - **Tracked files** (the source entry has a blob id): the copy's blob id must equal
//!   it; bytes may differ, since line endings change. A tracked file may become a
//!   symlink, as a symlink Windows git checked out as a plain file does.
//! - **Everything else** (untracked and ignored files): SHA-256 and size.
//!
//! Skipped folders and alternate data streams are not entries: they are listed in the
//! source manifest and passed through to the result. Anything else missing, extra or
//! different is a problem with its path. Modification times are recorded but not
//! compared, since normalising rewrites tracked files.

use std::collections::{BTreeMap, BTreeSet};
use std::fmt;

use serde::{Deserialize, Serialize};

use crate::migrate::build::SkippedFolder;

/// A path relative to the repo root: `/`-separated, with no empty, `.` or `..`
/// components, no leading or trailing `/` and no NUL. Serialized as a plain string.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(try_from = "String", into = "String")]
pub struct RelPath(String);

impl RelPath {
    /// Checks and wraps a `/`-separated relative path.
    pub fn new(path: impl Into<String>) -> Result<RelPath, String> {
        let path = path.into();
        let bad = path.is_empty()
            || path.contains('\0')
            || path
                .split('/')
                .any(|part| part.is_empty() || part == "." || part == "..");
        if bad {
            return Err(format!("not a relative path: {path:?}"));
        }
        Ok(RelPath(path))
    }

    /// Joins Windows path components (UTF-16, as `FindFirstFileW` gives them).
    /// Fails, with a lossy rendering for messages, when a name is not valid UTF-16 or
    /// holds a `/`.
    pub fn from_utf16(components: &[&[u16]]) -> Result<RelPath, String> {
        let mut parts = Vec::with_capacity(components.len());
        for component in components {
            match String::from_utf16(component) {
                Ok(part) if !part.contains('/') => parts.push(part),
                _ => {
                    let lossy: Vec<String> = components
                        .iter()
                        .map(|c| String::from_utf16_lossy(c))
                        .collect();
                    return Err(lossy.join("/"));
                }
            }
        }
        RelPath::new(parts.join("/"))
    }

    /// Wraps a Linux relative path given as bytes. Fails, with a lossy rendering for
    /// messages, when the bytes are not UTF-8.
    pub fn from_bytes(bytes: &[u8]) -> Result<RelPath, String> {
        match std::str::from_utf8(bytes) {
            Ok(path) => RelPath::new(path),
            Err(_) => Err(String::from_utf8_lossy(bytes).into_owned()),
        }
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }

    /// The last component.
    pub fn name(&self) -> &str {
        self.0.rsplit('/').next().unwrap_or(&self.0)
    }

    /// The path without its last component; `None` at the root.
    pub fn parent(&self) -> Option<&str> {
        self.0.rsplit_once('/').map(|(parent, _)| parent)
    }

    /// True when this path is `dir` or lies under it, by whole components.
    pub fn within(&self, dir: &str) -> bool {
        self.0 == dir
            || self
                .0
                .strip_prefix(dir)
                .is_some_and(|rest| rest.starts_with('/'))
    }
}

impl TryFrom<String> for RelPath {
    type Error = String;
    fn try_from(path: String) -> Result<RelPath, String> {
        RelPath::new(path)
    }
}

impl From<RelPath> for String {
    fn from(path: RelPath) -> String {
        path.0
    }
}

impl fmt::Display for RelPath {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

/// What a manifest entry is.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum EntryKind {
    File,
    Dir,
    Symlink,
}

/// One file, folder or symlink.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Entry {
    pub path: RelPath,
    pub kind: EntryKind,
    /// Size in bytes; 0 for folders.
    pub size: u64,
    /// Modification time, nanoseconds since the Unix epoch.
    pub mtime_ns: i64,
    /// Windows file attributes (`FILE_ATTRIBUTE_*`); 0 on the Linux side. Recorded,
    /// not compared.
    #[serde(default)]
    pub attributes: u32,
    /// Lowercase hex SHA-256 of the content, for files.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sha256: Option<String>,
    /// For tracked files, the blob id git stores for it: on the source the clean blob
    /// from `git.exe hash-object --stdin-paths`, on the copy Linux `git hash-object
    /// --path`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub blob: Option<String>,
    /// For symlinks, the target as stored.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub link_target: Option<String>,
}

impl Entry {
    /// The entry's stat facts, for [`compare_stats`].
    pub fn stat(&self) -> StatEntry {
        StatEntry {
            path: self.path.clone(),
            size: self.size,
            mtime_ns: self.mtime_ns,
            hash: None,
        }
    }
}

/// An alternate data stream of a source file (such as `Zone.Identifier`); never copied.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct DroppedStream {
    /// The file the stream belongs to.
    pub path: RelPath,
    /// The stream's name, without the leading `:` or the `:$DATA` suffix.
    pub stream: String,
    pub size: u64,
}

/// Everything recorded about one side of a migration.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Manifest {
    /// Every file, folder and symlink outside the skipped folders, `.git` included.
    pub entries: Vec<Entry>,
    /// The folders left out (source side only).
    #[serde(default)]
    pub skipped: Vec<SkippedFolder>,
    /// Alternate data streams left out (source side only).
    #[serde(default)]
    pub streams: Vec<DroppedStream>,
    /// Lossy renderings of paths whose names could not be represented as a
    /// [`RelPath`]. Any one fails the comparison.
    #[serde(default)]
    pub unrepresentable: Vec<String>,
}

/// File and byte counts of one side, for the report.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Totals {
    pub files: u64,
    pub dirs: u64,
    pub bytes: u64,
}

impl Manifest {
    /// Counts of files (symlinks included), folders and file bytes.
    pub fn totals(&self) -> Totals {
        let mut totals = Totals::default();
        for entry in &self.entries {
            match entry.kind {
                EntryKind::Dir => totals.dirs += 1,
                EntryKind::File | EntryKind::Symlink => {
                    totals.files += 1;
                    totals.bytes += entry.size;
                }
            }
        }
        totals
    }
}

/// What is wrong with one path.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(
    tag = "kind",
    rename_all = "camelCase",
    rename_all_fields = "camelCase"
)]
pub enum PathProblem {
    /// In the source, not in the copy.
    Missing { path: RelPath },
    /// In the copy, not in the source, and not an allowed extra.
    Extra { path: RelPath },
    /// A file in one, a folder or symlink in the other.
    KindDiffers {
        path: RelPath,
        source: EntryKind,
        copy: EntryKind,
    },
    /// SHA-256 or size differ, or a symlink's target.
    ContentDiffers { path: RelPath },
    /// A tracked file whose blob ids differ, or whose copy has none.
    BlobDiffers {
        path: RelPath,
        source: String,
        copy: Option<String>,
    },
    /// A name that could not be represented (lossy rendering).
    Unrepresentable { path: String },
}

impl PathProblem {
    /// The path, as shown.
    pub fn path(&self) -> &str {
        match self {
            PathProblem::Missing { path }
            | PathProblem::Extra { path }
            | PathProblem::KindDiffers { path, .. }
            | PathProblem::ContentDiffers { path }
            | PathProblem::BlobDiffers { path, .. } => path.as_str(),
            PathProblem::Unrepresentable { path } => path,
        }
    }

    /// A short sentence for the report.
    pub fn message(&self) -> String {
        match self {
            PathProblem::Missing { .. } => "Missing from the copy.".into(),
            PathProblem::Extra { .. } => "In the copy but not in the original.".into(),
            PathProblem::KindDiffers { source, copy, .. } => format!(
                "A {} in the original but a {} in the copy.",
                kind_name(*source),
                kind_name(*copy)
            ),
            PathProblem::ContentDiffers { .. } => "Its content differs in the copy.".into(),
            PathProblem::BlobDiffers { copy: None, .. } => {
                "Git could not hash it in the copy.".into()
            }
            PathProblem::BlobDiffers { .. } => "Git sees different content in the copy.".into(),
            PathProblem::Unrepresentable { .. } => {
                "Its name can't be represented on the other side.".into()
            }
        }
    }
}

fn kind_name(kind: EntryKind) -> &'static str {
    match kind {
        EntryKind::File => "file",
        EntryKind::Dir => "folder",
        EntryKind::Symlink => "symlink",
    }
}

/// The result of [`diff`].
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ManifestDiff {
    /// Failures, sorted by path. Empty when the copy matches.
    pub problems: Vec<PathProblem>,
    /// Loose objects the copy added (allowed; they must still pass fsck).
    pub new_objects: Vec<RelPath>,
    /// Git `index` and `config` files that differ (allowed; checked separately).
    pub changed_git_files: Vec<RelPath>,
    /// The source's skipped folders (allowed missing).
    pub skipped: Vec<SkippedFolder>,
    /// The source's dropped alternate data streams (allowed missing).
    pub dropped_streams: Vec<DroppedStream>,
    pub source_totals: Totals,
    pub copy_totals: Totals,
}

impl ManifestDiff {
    /// True when nothing failed.
    pub fn passed(&self) -> bool {
        self.problems.is_empty()
    }
}

/// Compares the copy's manifest with the source's under the rules in the module docs.
pub fn diff(source: &Manifest, copy: &Manifest) -> ManifestDiff {
    let git_dirs = git_dirs(source);
    let theirs: BTreeMap<&str, &Entry> =
        copy.entries.iter().map(|e| (e.path.as_str(), e)).collect();
    let ours: BTreeMap<&str, &Entry> = source
        .entries
        .iter()
        .map(|e| (e.path.as_str(), e))
        .collect();

    let mut result = ManifestDiff {
        skipped: source.skipped.clone(),
        dropped_streams: source.streams.clone(),
        source_totals: source.totals(),
        copy_totals: copy.totals(),
        ..ManifestDiff::default()
    };
    for path in source.unrepresentable.iter().chain(&copy.unrepresentable) {
        result
            .problems
            .push(PathProblem::Unrepresentable { path: path.clone() });
    }

    for (path, src) in &ours {
        let Some(dst) = theirs.get(path) else {
            result.problems.push(PathProblem::Missing {
                path: src.path.clone(),
            });
            continue;
        };
        let role = git_role(&src.path, &git_dirs);
        if matches!(role, GitRole::Index | GitRole::Config) {
            if src.kind != dst.kind {
                result.problems.push(kind_differs(src, dst));
            } else if !same_bytes(src, dst) {
                result.changed_git_files.push(src.path.clone());
            }
            continue;
        }
        if let (Some(blob), GitRole::None) = (&src.blob, role) {
            let kind_ok = src.kind == dst.kind
                || (src.kind == EntryKind::File && dst.kind == EntryKind::Symlink);
            if !kind_ok {
                result.problems.push(kind_differs(src, dst));
            } else if dst.blob.as_ref() != Some(blob) {
                result.problems.push(PathProblem::BlobDiffers {
                    path: src.path.clone(),
                    source: blob.clone(),
                    copy: dst.blob.clone(),
                });
            }
            continue;
        }
        if src.kind != dst.kind {
            result.problems.push(kind_differs(src, dst));
        } else if !same_bytes(src, dst) {
            result.problems.push(PathProblem::ContentDiffers {
                path: src.path.clone(),
            });
        }
    }

    for (path, dst) in &theirs {
        if ours.contains_key(path) {
            continue;
        }
        match git_role(&dst.path, &git_dirs) {
            GitRole::LooseObject if dst.kind == EntryKind::File => {
                result.new_objects.push(dst.path.clone())
            }
            // A fan-out folder (`objects/xx`) made for new loose objects.
            GitRole::FanOut if dst.kind == EntryKind::Dir => {}
            _ => result.problems.push(PathProblem::Extra {
                path: dst.path.clone(),
            }),
        }
    }
    result.problems.sort_by(|a, b| a.path().cmp(b.path()));
    result
}

fn kind_differs(src: &Entry, dst: &Entry) -> PathProblem {
    PathProblem::KindDiffers {
        path: src.path.clone(),
        source: src.kind,
        copy: dst.kind,
    }
}

fn same_bytes(src: &Entry, dst: &Entry) -> bool {
    match src.kind {
        EntryKind::Dir => true,
        EntryKind::Symlink => src.link_target == dst.link_target,
        EntryKind::File => src.size == dst.size && src.sha256.is_some() && src.sha256 == dst.sha256,
    }
}

/// The git directories in a manifest: folders named `.git`, and folders inside a
/// `.git` that hold a `HEAD` file and `objects` and `refs` folders (submodule git
/// directories under `.git/modules`).
fn git_dirs(manifest: &Manifest) -> BTreeSet<&str> {
    let kinds: BTreeMap<&str, EntryKind> = manifest
        .entries
        .iter()
        .map(|e| (e.path.as_str(), e.kind))
        .collect();
    let has = |dir: &str, name: &str, kind: EntryKind| {
        kinds.get(format!("{dir}/{name}").as_str()) == Some(&kind)
    };
    manifest
        .entries
        .iter()
        .filter(|e| e.kind == EntryKind::Dir)
        .map(|e| e.path.as_str())
        .filter(|dir| {
            dir.rsplit('/').next() == Some(".git")
                || (dir.split('/').any(|part| part == ".git")
                    && has(dir, "HEAD", EntryKind::File)
                    && has(dir, "objects", EntryKind::Dir)
                    && has(dir, "refs", EntryKind::Dir))
        })
        .collect()
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum GitRole {
    /// Not inside a git directory.
    None,
    /// Inside one, compared byte for byte.
    Plain,
    Index,
    Config,
    /// `objects/xx/<hex>`.
    LooseObject,
    /// `objects/xx`.
    FanOut,
}

/// What `path` is, relative to the innermost git directory holding it.
fn git_role(path: &RelPath, git_dirs: &BTreeSet<&str>) -> GitRole {
    let Some(rest) = git_dirs
        .iter()
        .filter_map(|dir| path.as_str().strip_prefix(dir)?.strip_prefix('/'))
        .min_by_key(|rest| rest.len())
    else {
        return GitRole::None;
    };
    let hex = |s: &str| {
        s.bytes()
            .all(|b| b.is_ascii_hexdigit() && !b.is_ascii_uppercase())
    };
    match rest.split('/').collect::<Vec<_>>().as_slice() {
        ["index"] => GitRole::Index,
        ["config"] => GitRole::Config,
        ["objects", fan] if fan.len() == 2 && hex(fan) => GitRole::FanOut,
        // 38 hex digits for SHA-1 ids, 62 for SHA-256.
        ["objects", fan, rest]
            if fan.len() == 2 && hex(fan) && matches!(rest.len(), 38 | 62) && hex(rest) =>
        {
            GitRole::LooseObject
        }
        _ => GitRole::Plain,
    }
}

/// Stat facts of one path, for telling whether the source changed (check 4).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct StatEntry {
    pub path: RelPath,
    pub size: u64,
    pub mtime_ns: i64,
    /// A content hash, for files whose stat alone is not trusted (refs and the index).
    /// Compared only when both sides have one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub hash: Option<String>,
}

/// The result of [`compare_stats`]; each list sorted.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct StatChanges {
    pub changed: Vec<RelPath>,
    pub added: Vec<RelPath>,
    pub removed: Vec<RelPath>,
}

impl StatChanges {
    /// True when nothing changed.
    pub fn is_empty(&self) -> bool {
        self.changed.is_empty() && self.added.is_empty() && self.removed.is_empty()
    }
}

/// What changed between two stat snapshots of the source: a different size or mtime,
/// or a different hash when both have one.
pub fn compare_stats(before: &[StatEntry], after: &[StatEntry]) -> StatChanges {
    let old: BTreeMap<&RelPath, &StatEntry> = before.iter().map(|e| (&e.path, e)).collect();
    let new: BTreeMap<&RelPath, &StatEntry> = after.iter().map(|e| (&e.path, e)).collect();
    let mut changes = StatChanges::default();
    for (path, a) in &old {
        match new.get(path) {
            None => changes.removed.push((*path).clone()),
            Some(b) => {
                let hash_differs = matches!((&a.hash, &b.hash), (Some(x), Some(y)) if x != y);
                if a.size != b.size || a.mtime_ns != b.mtime_ns || hash_differs {
                    changes.changed.push((*path).clone());
                }
            }
        }
    }
    changes.added = new
        .keys()
        .filter(|path| !old.contains_key(*path))
        .map(|path| (*path).clone())
        .collect();
    changes
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::migrate::build::BuildFolder;

    fn p(path: &str) -> RelPath {
        RelPath::new(path).unwrap()
    }

    fn dir(path: &str) -> Entry {
        Entry {
            path: p(path),
            kind: EntryKind::Dir,
            size: 0,
            mtime_ns: 1,
            attributes: 0,
            sha256: None,
            blob: None,
            link_target: None,
        }
    }

    fn file(path: &str, content: &str) -> Entry {
        Entry {
            kind: EntryKind::File,
            size: content.len() as u64,
            sha256: Some(format!("sha-of-{content}")),
            ..dir(path)
        }
    }

    fn tracked(path: &str, content: &str, blob: &str) -> Entry {
        Entry {
            blob: Some(blob.into()),
            ..file(path, content)
        }
    }

    /// A small repo, aurora-notes, as the source manifest lists it.
    fn source() -> Manifest {
        Manifest {
            entries: vec![
                dir(".git"),
                file(".git/HEAD", "ref: refs/heads/main\n"),
                file(".git/config", "[core]\n\tautocrlf = true\n"),
                file(".git/index", "DIRC-windows"),
                dir(".git/objects"),
                dir(".git/objects/pack"),
                file(".git/objects/pack/pack-1.pack", "packed"),
                dir(".git/objects/ab"),
                file(&format!(".git/objects/ab/{}", "c".repeat(38)), "loose"),
                dir(".git/refs"),
                tracked("README.md", "hello\r\n", "blob-readme"),
                dir("src"),
                tracked("src/main.rs", "fn main() {}\r\n", "blob-main"),
                file(".env", "TOKEN=1\n"),
                file("notes.txt", "untracked\r\n"),
            ],
            skipped: vec![SkippedFolder {
                path: p("node_modules"),
                rule: BuildFolder::NodeModules,
                bytes: 4096,
                files: 3,
            }],
            streams: vec![DroppedStream {
                path: p("README.md"),
                stream: "Zone.Identifier".into(),
                size: 26,
            }],
            unrepresentable: vec![],
        }
    }

    /// The copy after normalising: LF tracked files with the same blobs, a new config
    /// and index, and new loose objects.
    fn copy() -> Manifest {
        let mut entries: Vec<Entry> = source()
            .entries
            .into_iter()
            .map(|mut e| {
                match e.path.as_str() {
                    "README.md" => e = tracked("README.md", "hello\n", "blob-readme"),
                    "src/main.rs" => e = tracked("src/main.rs", "fn main() {}\n", "blob-main"),
                    ".git/config" => e = file(".git/config", "[core]\n\tfilemode = true\n"),
                    ".git/index" => e = file(".git/index", "DIRC-linux"),
                    _ => {}
                }
                e
            })
            .collect();
        entries.push(dir(".git/objects/3f"));
        entries.push(file(&format!(".git/objects/3f/{}", "1".repeat(38)), "new"));
        entries.push(file(&format!(".git/objects/ab/{}", "d".repeat(38)), "new"));
        Manifest {
            entries,
            ..Manifest::default()
        }
    }

    #[test]
    fn a_normalised_copy_passes_and_lists_what_was_allowed() {
        let result = diff(&source(), &copy());
        assert_eq!(result.problems, vec![]);
        assert!(result.passed());
        assert_eq!(
            result.new_objects,
            vec![
                p(&format!(".git/objects/3f/{}", "1".repeat(38))),
                p(&format!(".git/objects/ab/{}", "d".repeat(38))),
            ]
        );
        assert_eq!(
            result.changed_git_files,
            vec![p(".git/config"), p(".git/index")]
        );
        assert_eq!(result.skipped[0].path, p("node_modules"));
        assert_eq!(result.dropped_streams[0].stream, "Zone.Identifier");
        assert_eq!(result.source_totals.files, 9);
        assert_eq!(result.copy_totals.files, 11);
    }

    #[test]
    fn crlf_is_fine_when_blobs_match_but_a_blob_mismatch_fails() {
        let mut copy = copy();
        let main = copy
            .entries
            .iter_mut()
            .find(|e| e.path.as_str() == "src/main.rs")
            .unwrap();
        main.blob = Some("blob-other".into());
        let result = diff(&source(), &copy);
        assert_eq!(
            result.problems,
            vec![PathProblem::BlobDiffers {
                path: p("src/main.rs"),
                source: "blob-main".into(),
                copy: Some("blob-other".into()),
            }]
        );
        // A copy that could not be hashed fails too.
        let main = copy
            .entries
            .iter_mut()
            .find(|e| e.path.as_str() == "src/main.rs")
            .unwrap();
        main.blob = None;
        let result = diff(&source(), &copy);
        assert_eq!(
            result.problems[0].message(),
            "Git could not hash it in the copy."
        );
    }

    #[test]
    fn untracked_files_must_match_byte_for_byte() {
        let mut copy = copy();
        for e in &mut copy.entries {
            if e.path.as_str() == "notes.txt" {
                *e = file("notes.txt", "untracked\n");
            }
        }
        let result = diff(&source(), &copy);
        assert_eq!(
            result.problems,
            vec![PathProblem::ContentDiffers {
                path: p("notes.txt")
            }]
        );
    }

    #[test]
    fn a_changed_pack_file_fails() {
        let mut copy = copy();
        for e in &mut copy.entries {
            if e.path.as_str() == ".git/objects/pack/pack-1.pack" {
                *e = file(".git/objects/pack/pack-1.pack", "packeD");
            }
        }
        let result = diff(&source(), &copy);
        assert_eq!(
            result.problems,
            vec![PathProblem::ContentDiffers {
                path: p(".git/objects/pack/pack-1.pack")
            }]
        );
    }

    #[test]
    fn a_changed_existing_loose_object_fails() {
        let mut copy = copy();
        let name = format!(".git/objects/ab/{}", "c".repeat(38));
        for e in &mut copy.entries {
            if e.path.as_str() == name {
                *e = file(&name, "LOOSE");
            }
        }
        assert_eq!(diff(&source(), &copy).problems.len(), 1);
    }

    #[test]
    fn extras_outside_loose_objects_fail() {
        let mut copy = copy();
        copy.entries
            .push(file(".git/objects/pack/pack-2.pack", "x"));
        copy.entries.push(file(".git/refs/stray", "x"));
        copy.entries.push(file("stray.txt", "x"));
        copy.entries.push(dir("node_modules"));
        // Not a loose-object name: wrong length, uppercase hex.
        copy.entries.push(file(".git/objects/ab/123", "x"));
        copy.entries
            .push(file(&format!(".git/objects/ab/{}", "C".repeat(38)), "x"));
        let result = diff(&source(), &copy);
        let paths: Vec<&str> = result.problems.iter().map(|p| p.path()).collect();
        assert_eq!(
            paths,
            vec![
                ".git/objects/ab/123",
                &format!(".git/objects/ab/{}", "C".repeat(38)),
                ".git/objects/pack/pack-2.pack",
                ".git/refs/stray",
                "node_modules",
                "stray.txt",
            ]
        );
        assert!(result
            .problems
            .iter()
            .all(|p| matches!(p, PathProblem::Extra { .. })));
    }

    #[test]
    fn missing_paths_and_kind_changes_fail() {
        let mut copy = copy();
        copy.entries.retain(|e| e.path.as_str() != ".env");
        for e in &mut copy.entries {
            if e.path.as_str() == "notes.txt" {
                *e = dir("notes.txt");
            }
        }
        let result = diff(&source(), &copy);
        assert_eq!(
            result.problems,
            vec![
                PathProblem::Missing { path: p(".env") },
                PathProblem::KindDiffers {
                    path: p("notes.txt"),
                    source: EntryKind::File,
                    copy: EntryKind::Dir,
                },
            ]
        );
        assert_eq!(
            result.problems[1].message(),
            "A file in the original but a folder in the copy."
        );
    }

    #[test]
    fn a_tracked_plain_file_may_become_a_symlink() {
        let mut source = source();
        source.entries.push(tracked("current", "src", "blob-link"));
        let mut copy = copy();
        copy.entries.push(Entry {
            kind: EntryKind::Symlink,
            link_target: Some("src".into()),
            ..tracked("current", "src", "blob-link")
        });
        assert!(diff(&source, &copy).passed());
    }

    #[test]
    fn submodule_git_dirs_follow_the_same_rules() {
        let mut source = source();
        for e in [
            dir(".git/modules"),
            dir(".git/modules/libs"),
            dir(".git/modules/libs/kelp"),
            file(".git/modules/libs/kelp/HEAD", "abc\n"),
            file(".git/modules/libs/kelp/index", "DIRC-sub"),
            file(".git/modules/libs/kelp/config", "[core]\n"),
            dir(".git/modules/libs/kelp/objects"),
            dir(".git/modules/libs/kelp/refs"),
            dir("libs"),
            dir("libs/kelp"),
            file("libs/kelp/.git", "gitdir: ../../.git/modules/libs/kelp\n"),
        ] {
            source.entries.push(e);
        }
        let mut copy = copy();
        for e in source
            .entries
            .iter()
            .filter(|e| e.path.within(".git/modules") || e.path.within("libs"))
        {
            copy.entries.push(e.clone());
        }
        for e in &mut copy.entries {
            if e.path.as_str() == ".git/modules/libs/kelp/index" {
                *e = file(".git/modules/libs/kelp/index", "DIRC-sub-linux");
            }
        }
        copy.entries.push(dir(".git/modules/libs/kelp/objects/0a"));
        let object = format!(".git/modules/libs/kelp/objects/0a/{}", "e".repeat(38));
        copy.entries.push(file(&object, "new"));
        let result = diff(&source, &copy);
        assert!(result.passed(), "{:?}", result.problems);
        assert!(result.new_objects.contains(&p(&object)));
        assert!(result
            .changed_git_files
            .contains(&p(".git/modules/libs/kelp/index")));
    }

    #[test]
    fn unrepresentable_names_fail() {
        let mut copy = copy();
        copy.unrepresentable.push("docs/caf\u{fffd}.md".into());
        let result = diff(&source(), &copy);
        assert_eq!(
            result.problems,
            vec![PathProblem::Unrepresentable {
                path: "docs/caf\u{fffd}.md".into()
            }]
        );
    }

    #[test]
    fn rel_paths_are_checked_and_converted() {
        for bad in ["", "/a", "a/", "a//b", "./a", "a/../b", "a\0b"] {
            assert!(RelPath::new(bad).is_err(), "{bad:?}");
        }
        let path = p("src/tide tables/é.rs");
        assert_eq!(path.name(), "é.rs");
        assert_eq!(path.parent(), Some("src/tide tables"));
        assert_eq!(p("README.md").parent(), None);
        assert!(path.within("src"));
        assert!(!p("srcs/a").within("src"));

        let utf16: Vec<Vec<u16>> = ["src", "é.rs"]
            .iter()
            .map(|s| s.encode_utf16().collect())
            .collect();
        let parts: Vec<&[u16]> = utf16.iter().map(Vec::as_slice).collect();
        assert_eq!(RelPath::from_utf16(&parts).unwrap(), p("src/é.rs"));
        let lone: &[u16] = &[0x61, 0xD800];
        assert_eq!(RelPath::from_utf16(&[lone]).unwrap_err(), "a\u{fffd}");
        assert!(RelPath::from_bytes(b"src/\xff.rs").is_err());
        assert_eq!(RelPath::from_bytes(b"src/a.rs").unwrap(), p("src/a.rs"));

        let parsed: Result<RelPath, _> = serde_json::from_str(r#""../etc""#);
        assert!(parsed.is_err());
        assert_eq!(serde_json::to_string(&p("a/b")).unwrap(), r#""a/b""#);
    }

    #[test]
    fn stat_changes_are_reported_by_path() {
        let stat = |path: &str, size: u64, mtime_ns: i64, hash: Option<&str>| StatEntry {
            path: p(path),
            size,
            mtime_ns,
            hash: hash.map(str::to_owned),
        };
        let before = vec![
            stat("a.txt", 1, 10, None),
            stat("b.txt", 1, 10, None),
            stat("c.txt", 1, 10, None),
            stat(".git/index", 9, 10, Some("h1")),
            stat(".git/refs/heads/main", 41, 10, Some("h2")),
        ];
        let after = vec![
            stat("a.txt", 1, 10, None),
            stat("b.txt", 1, 11, None),
            stat("d.txt", 1, 10, None),
            stat(".git/index", 9, 10, Some("h1-new")),
            stat(".git/refs/heads/main", 41, 10, None),
        ];
        let changes = compare_stats(&before, &after);
        assert_eq!(changes.changed, vec![p(".git/index"), p("b.txt")]);
        assert_eq!(changes.added, vec![p("d.txt")]);
        assert_eq!(changes.removed, vec![p("c.txt")]);
        assert!(!changes.is_empty());
        assert!(compare_stats(&before, &before).is_empty());
        assert_eq!(file("x", "y").stat().size, 1);
    }
}
