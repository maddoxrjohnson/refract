//! The files of a repo folder: listing them as a [`Manifest`] or as stat facts
//! ([`StatEntry`]), and copying them (spec §3.1).
//!
//! Paths are relative to the repo and `/`-separated. Skipped build folders are left out
//! of everything here: they are neither listed, statted nor copied.

use std::collections::{BTreeMap, BTreeSet};
use std::fs::{self, File, OpenOptions};
use std::io::{self, Read, Write};
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
use std::path::Path;

use refract_core::migrate::manifest::{
    Entry, EntryKind, Manifest, RelPath, StatChanges, StatEntry,
};
use refract_core::migrate::report::Problem;

use super::fsx::{self, Visit};
use super::gitx;

/// Copied files' mode.
const FILE_MODE: u32 = 0o644;
/// Copied files' mode when they look executable ([`executable`]).
const EXEC_MODE: u32 = 0o755;
/// Copied folders' mode.
const DIR_MODE: u32 = 0o755;

/// A predicate for [`fsx::walk`] leaving out the `skipped` folders.
pub fn skipper(skipped: &[RelPath]) -> impl Fn(&Path) -> bool + '_ {
    move |rel: &Path| {
        let bytes = rel.as_os_str().as_bytes();
        skipped.iter().any(|s| s.as_str().as_bytes() == bytes)
    }
}

/// Stops a long job between files when it is cancelled.
pub trait Cancel {
    fn cancelled(&self) -> bool;
}

impl Cancel for std::sync::atomic::AtomicBool {
    fn cancelled(&self) -> bool {
        self.load(std::sync::atomic::Ordering::Relaxed)
    }
}

/// Returned when a job was cancelled.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Cancelled;

/// Lists the tree under `root` without hashing. A name that cannot be represented is
/// listed in `unrepresentable`, and so is each path under it.
fn list(root: &Path, skipped: &[RelPath], cancel: &dyn Cancel) -> Result<Manifest, Cancelled> {
    let mut manifest = Manifest::default();
    let skip = skipper(skipped);
    let finished = fsx::walk(root, &skip, &mut |visit| {
        if cancel.cancelled() {
            return false;
        }
        let Visit::Entry { rel, abs, meta } = visit else {
            return true;
        };
        let path = match RelPath::from_bytes(rel.as_os_str().as_bytes()) {
            Ok(path) => path,
            Err(lossy) => {
                manifest.unrepresentable.push(lossy);
                return true;
            }
        };
        let file_type = meta.file_type();
        let kind = if file_type.is_symlink() {
            EntryKind::Symlink
        } else if file_type.is_dir() {
            EntryKind::Dir
        } else {
            EntryKind::File
        };
        let link_target = (kind == EntryKind::Symlink)
            .then(|| fs::read_link(abs).ok())
            .flatten()
            .map(|t| t.to_string_lossy().into_owned());
        manifest.entries.push(Entry {
            path,
            kind,
            size: if kind == EntryKind::File {
                meta.len()
            } else {
                0
            },
            mtime_ns: fsx::mtime_ns(meta),
            attributes: 0,
            sha256: None,
            blob: None,
            link_target,
        });
        true
    });
    if !finished {
        return Err(Cancelled);
    }
    Ok(manifest)
}

/// Builds the manifest of the tree under `root`: every entry outside `skipped` with its
/// SHA-256 (files), and the blob id of each tracked file of each repo in `repos`
/// (working-tree paths relative to `root`, `""` for the top), computed with `config`.
/// `progress` gets (files hashed, files in all).
pub fn manifest(
    root: &Path,
    skipped: &[RelPath],
    repos: &[String],
    config: &[String],
    cancel: &dyn Cancel,
    progress: &mut dyn FnMut(u64, u64),
) -> Result<Result<Manifest, String>, Cancelled> {
    let mut manifest = list(root, skipped, cancel)?;
    let total = manifest
        .entries
        .iter()
        .filter(|e| e.kind == EntryKind::File)
        .count() as u64;
    let mut done = 0;
    progress(0, total);
    for entry in &mut manifest.entries {
        if entry.kind != EntryKind::File {
            continue;
        }
        if cancel.cancelled() {
            return Err(Cancelled);
        }
        entry.sha256 = fsx::sha256_file(&root.join(entry.path.as_str())).ok();
        done += 1;
        progress(done, total);
    }
    if let Err(e) = add_blobs(root, &mut manifest, repos, config, None) {
        return Ok(Err(e));
    }
    Ok(Ok(manifest))
}

/// Sets the blob id of each tracked file of each repo (see [`manifest`]); with `only`,
/// just of those paths. Entries git does not track are left as they are.
pub fn add_blobs(
    root: &Path,
    manifest: &mut Manifest,
    repos: &[String],
    config: &[String],
    only: Option<&BTreeSet<String>>,
) -> Result<(), String> {
    let mut blobs: BTreeMap<String, String> = BTreeMap::new();
    for repo in repos {
        let dir = if repo.is_empty() {
            root.to_owned()
        } else {
            root.join(repo)
        };
        let prefix = if repo.is_empty() {
            String::new()
        } else {
            format!("{repo}/")
        };
        let tracked = gitx::tracked(&dir, config)?;
        let paths: Vec<String> = tracked
            .into_iter()
            .filter(|p| only.is_none_or(|only| only.contains(&format!("{prefix}{p}"))))
            .collect();
        for (path, blob) in gitx::blobs(&dir, config, &paths, false)? {
            blobs.insert(format!("{prefix}{path}"), blob);
        }
    }
    for entry in &mut manifest.entries {
        if only.is_some_and(|only| !only.contains(entry.path.as_str())) {
            continue;
        }
        if matches!(entry.kind, EntryKind::File | EntryKind::Symlink) {
            if let Some(blob) = blobs.get(entry.path.as_str()) {
                entry.blob = Some(blob.clone());
            }
        }
    }
    Ok(())
}

/// True for the git files whose stat alone is not trusted to show a change: `HEAD`,
/// `index`, `packed-refs` and loose refs, in any git directory.
fn hashed_for_stat(path: &str) -> bool {
    let parts: Vec<&str> = path.split('/').collect();
    let Some(git) = parts.iter().position(|p| *p == ".git") else {
        return false;
    };
    let rest = &parts[git + 1..];
    matches!(rest.last(), Some(&("HEAD" | "index" | "packed-refs"))) || rest.contains(&"refs")
}

/// The stat facts of every entry under `root` outside `skipped`: size (0 for folders
/// and symlinks) and modification time, and a SHA-256 for refs, `HEAD`, `packed-refs`
/// and the index.
pub fn stats(
    root: &Path,
    skipped: &[RelPath],
    cancel: &dyn Cancel,
) -> Result<Vec<StatEntry>, Cancelled> {
    let listed = list(root, skipped, cancel)?;
    Ok(listed
        .entries
        .iter()
        .map(|e| StatEntry {
            path: e.path.clone(),
            size: e.size,
            mtime_ns: e.mtime_ns,
            hash: (e.kind == EntryKind::File && hashed_for_stat(e.path.as_str()))
                .then(|| fsx::sha256_file(&root.join(e.path.as_str())).ok())
                .flatten(),
        })
        .collect())
}

/// The stat facts of a source manifest's entries, sized as [`stats`] sizes them.
pub fn manifest_stats(manifest: &Manifest) -> Vec<StatEntry> {
    manifest
        .entries
        .iter()
        .map(|e| StatEntry {
            size: if e.kind == EntryKind::File { e.size } else { 0 },
            ..e.stat()
        })
        .collect()
}

/// True when a copied file at `rel` that starts with `head` gets mode 755 rather than
/// 644 (spec §3.1): drvfs cannot tell the Windows side's executable bit reliably, so a
/// hook (a file right in a git folder's `hooks`, not ending in `.sample`) and any file
/// outside git folders starting with `#!` count as executable. Tracked files get their
/// index mode afterwards when normalised.
fn executable(rel: &Path, head: &[u8]) -> bool {
    let parts: Vec<&[u8]> = rel.as_os_str().as_bytes().split(|b| *b == b'/').collect();
    match parts.iter().position(|p| *p == b".git") {
        Some(git) => {
            let n = parts.len();
            n >= git + 3 && parts[n - 2] == b"hooks" && !parts[n - 1].ends_with(b".sample")
        }
        None => head.starts_with(b"#!"),
    }
}

/// Copies the tree under `from` into the existing folder `to` (spec §3.1): everything
/// outside `skipped`, files as mode 644 (755 when [`executable`]) and folders as 755
/// with their modification times, symlinks as they are. Special files and anything that fails are reported as
/// problems, and the copy goes on. `progress` gets the bytes copied so far.
pub fn copy_tree(
    from: &Path,
    to: &Path,
    skipped: &[RelPath],
    cancel: &dyn Cancel,
    progress: &mut dyn FnMut(u64),
) -> Result<Vec<Problem>, Cancelled> {
    let skip = skipper(skipped);
    let mut problems = Vec::new();
    let mut bytes = 0u64;
    let finished = fsx::walk(from, &skip, &mut |visit| {
        if cancel.cancelled() {
            return false;
        }
        match visit {
            Visit::Entry { rel, abs, meta } => {
                let dest = to.join(rel);
                if let Err(e) =
                    copy_entry(abs, rel, &dest, meta, &mut bytes, &mut *progress, cancel)
                {
                    problems.push(problem(rel, &e));
                }
            }
            Visit::DirDone { rel, meta, .. } => {
                let _ = fsx::set_mtime(&to.join(rel), fsx::mtime_ns(meta));
            }
            Visit::Error { rel, error } => problems.push(problem(rel, &error)),
        }
        true
    });
    if !finished {
        return Err(Cancelled);
    }
    Ok(problems)
}

fn problem(rel: &Path, error: &io::Error) -> Problem {
    let path = rel.to_string_lossy().into_owned();
    Problem {
        path: (!path.is_empty()).then_some(path),
        message: format!("Could not copy it: {error}."),
    }
}

/// Copies one entry (at `rel` in the tree), replacing what is at `dest` unless it is a
/// folder already.
fn copy_entry(
    src: &Path,
    rel: &Path,
    dest: &Path,
    meta: &fs::Metadata,
    bytes: &mut u64,
    progress: &mut dyn FnMut(u64),
    cancel: &dyn Cancel,
) -> io::Result<()> {
    let file_type = meta.file_type();
    let existing = fs::symlink_metadata(dest).ok();
    if file_type.is_dir() {
        match existing {
            Some(m) if m.is_dir() => {}
            Some(_) => {
                fs::remove_file(dest)?;
                fs::create_dir(dest)?;
            }
            None => fs::create_dir(dest)?,
        }
        fs::set_permissions(dest, fs::Permissions::from_mode(DIR_MODE))?;
        // Set again once its contents are in.
        return fsx::set_mtime(dest, fsx::mtime_ns(meta));
    }
    if existing.as_ref().is_some_and(|m| m.is_dir()) {
        fs::remove_dir_all(dest)?;
    }
    if file_type.is_symlink() {
        let target = fs::read_link(src)?;
        if existing.is_some() {
            let _ = fs::remove_file(dest);
        }
        std::os::unix::fs::symlink(target, dest)?;
        return fsx::set_mtime(dest, fsx::mtime_ns(meta));
    }
    if !file_type.is_file() {
        return Err(io::Error::other(
            "it is a special file (a pipe, socket or device)",
        ));
    }
    if existing
        .as_ref()
        .is_some_and(|m| m.file_type().is_symlink())
    {
        fs::remove_file(dest)?;
    }
    let mut input = File::open(src)?;
    let mut output = OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .mode(FILE_MODE)
        .open(dest)?;
    let mut buf = vec![0; 1 << 20];
    let mut head = Vec::with_capacity(2);
    loop {
        if cancel.cancelled() {
            return Ok(());
        }
        let n = match input.read(&mut buf) {
            Ok(0) => break,
            Ok(n) => n,
            Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
            Err(e) => return Err(e),
        };
        output.write_all(&buf[..n])?;
        let want = 2usize.saturating_sub(head.len()).min(n);
        head.extend_from_slice(&buf[..want]);
        *bytes += n as u64;
        progress(*bytes);
    }
    drop(output);
    let mode = if executable(rel, &head) {
        EXEC_MODE
    } else {
        FILE_MODE
    };
    fs::set_permissions(dest, fs::Permissions::from_mode(mode))?;
    fsx::set_mtime(dest, fsx::mtime_ns(meta))
}

/// Brings the copy at `to` in line with `from` for the paths in `changes`: removed paths
/// go, added and changed ones are copied again (a folder only itself, not what is under
/// it), and the folders holding them get the source's modification times back. Returns
/// the problems, by path.
pub fn recopy(
    from: &Path,
    to: &Path,
    changes: &StatChanges,
    cancel: &dyn Cancel,
) -> Result<Vec<Problem>, Cancelled> {
    let mut problems = Vec::new();
    let mut parents: BTreeSet<String> = BTreeSet::new();
    // Deepest first, so a removed folder's children go before it.
    for path in changes.removed.iter().rev() {
        if let Err(e) = fsx::remove_any(&to.join(path.as_str())) {
            problems.push(problem(Path::new(path.as_str()), &e));
        }
        parents.extend(path.parent().map(str::to_owned));
    }
    let mut touched: Vec<&RelPath> = changes.added.iter().chain(&changes.changed).collect();
    touched.sort();
    let mut bytes = 0;
    for path in touched {
        if cancel.cancelled() {
            return Err(Cancelled);
        }
        let src = from.join(path.as_str());
        let dest = to.join(path.as_str());
        let result = fs::symlink_metadata(&src).and_then(|meta| {
            if let Some(parent) = dest.parent() {
                fs::create_dir_all(parent)?;
            }
            let rel = Path::new(path.as_str());
            copy_entry(&src, rel, &dest, &meta, &mut bytes, &mut |_| {}, cancel)
        });
        match result {
            // Removed again since it was statted: removed here too.
            Err(e) if e.kind() == io::ErrorKind::NotFound => {
                let _ = fsx::remove_any(&dest);
            }
            Err(e) => problems.push(problem(Path::new(path.as_str()), &e)),
            Ok(()) => {}
        }
        parents.extend(path.parent().map(str::to_owned));
        if fs::symlink_metadata(&src).is_ok_and(|m| m.is_dir()) {
            parents.insert(path.as_str().to_owned());
        }
    }
    for parent in parents.iter().rev() {
        if let Ok(meta) = fs::symlink_metadata(from.join(parent)) {
            let _ = fsx::set_mtime(&to.join(parent), fsx::mtime_ns(&meta));
        }
    }
    Ok(problems)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::AtomicBool;

    #[test]
    fn stat_hashes_only_refs_and_the_index() {
        assert!(hashed_for_stat(".git/index"));
        assert!(hashed_for_stat(".git/HEAD"));
        assert!(hashed_for_stat(".git/packed-refs"));
        assert!(hashed_for_stat(".git/refs/heads/main"));
        assert!(hashed_for_stat(".git/modules/libs/kelp/refs/heads/main"));
        assert!(!hashed_for_stat(".git/objects/pack/pack-1.pack"));
        assert!(!hashed_for_stat("src/index"));
    }

    #[test]
    fn hooks_and_scripts_are_executable() {
        let exec = |rel: &str, head: &[u8]| executable(Path::new(rel), head);
        assert!(exec(".git/hooks/pre-commit", b""));
        assert!(exec(".git/modules/libs/kelp/hooks/post-checkout", b""));
        assert!(!exec(".git/hooks/pre-commit.sample", b"#!"));
        assert!(!exec(".git/config", b"#!"));
        assert!(!exec(".git/hooks", b""));
        assert!(exec("tools/build", b"#!"));
        assert!(exec("hooks/x", b"#!"));
        assert!(!exec("hooks/x", b"# "));
        assert!(!exec("notes.txt", b"#"));
    }

    #[test]
    fn copies_modes_times_and_links_and_skips() {
        let tmp = tempfile::tempdir().unwrap();
        let (from, to) = (tmp.path().join("from"), tmp.path().join("to"));
        fs::create_dir_all(from.join("src")).unwrap();
        fs::create_dir_all(from.join("node_modules/x")).unwrap();
        fs::write(from.join("src/run.sh"), "#!/bin/sh\r\n").unwrap();
        fs::set_permissions(from.join("src/run.sh"), fs::Permissions::from_mode(0o700)).unwrap();
        std::os::unix::fs::symlink("src/run.sh", from.join("run")).unwrap();
        fsx::set_mtime(&from.join("src/run.sh"), 1_500_000_000_000_000_001).unwrap();
        fsx::set_mtime(&from.join("src"), 1_500_000_000_000_000_002).unwrap();
        fs::create_dir(&to).unwrap();
        let skipped = [RelPath::new("node_modules").unwrap()];
        let cancel = AtomicBool::new(false);
        let mut last = 0;
        let problems = copy_tree(&from, &to, &skipped, &cancel, &mut |b| last = b).unwrap();
        assert_eq!(problems, vec![]);
        assert_eq!(last, 11);
        assert!(!to.join("node_modules").exists());
        let meta = fs::metadata(to.join("src/run.sh")).unwrap();
        assert_eq!(
            meta.permissions().mode() & 0o777,
            0o755,
            "a script starting with #!"
        );
        assert_eq!(fsx::mtime_ns(&meta), 1_500_000_000_000_000_001);
        let dir = fs::metadata(to.join("src")).unwrap();
        assert_eq!(dir.permissions().mode() & 0o777, 0o755);
        assert_eq!(fsx::mtime_ns(&dir), 1_500_000_000_000_000_002);
        assert_eq!(
            fs::read_link(to.join("run")).unwrap(),
            Path::new("src/run.sh")
        );

        let before = stats(&from, &skipped, &cancel).unwrap();
        assert_eq!(before, stats(&to, &skipped, &cancel).unwrap());
        fs::write(from.join("src/new.txt"), "n").unwrap();
        fs::remove_file(from.join("run")).unwrap();
        let after = stats(&from, &skipped, &cancel).unwrap();
        let changes = refract_core::migrate::manifest::compare_stats(&before, &after);
        assert_eq!(changes.added, vec![RelPath::new("src/new.txt").unwrap()]);
        let problems = recopy(&from, &to, &changes, &cancel).unwrap();
        assert_eq!(problems, vec![]);
        assert_eq!(stats(&to, &skipped, &cancel).unwrap(), after);
    }
}
