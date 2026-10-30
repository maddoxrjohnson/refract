//! The source snapshot (S0 and G0, [`SourceSnapshot`]) built with Linux git: the
//! fallback of spec §4.2 when `git.exe` is missing, what the helper uses to re-read
//! files that changed during the copy (check 4), and what the tests hand the helper in
//! place of the app.
//!
//! The app builds the same thing natively with `git.exe`. To match, it must produce:
//!
//! - **`git`**: the top repo first with path `""`, then every checked-out submodule
//!   (`git submodule status --recursive`, lines not starting with `-`) by its
//!   working-tree path relative to the top, in that command's order. Each
//!   [`GitSnapshot`] holds the output of the commands listed in
//!   [`refract_core::migrate::gitstate`], run in that repo's working tree, parsed by
//!   the parsers named there.
//! - **`manifest.skipped`**: folders `git status --porcelain=v2 -z --untracked-files=all
//!   --ignored=matching` lists as `! <path>/` (in the repo or submodule holding them)
//!   that [`build::skip_rule`] accepts, not inside a `.git` or another skipped folder,
//!   with their file count and total file size.
//! - **`manifest.entries`**: every file, folder and symlink outside the skipped folders,
//!   `.git` included, parents before children. `size` is the file size (0 for folders
//!   and symlinks, whose sizes are not compared), `mtimeNs` the modification time in
//!   nanoseconds since the Unix epoch, `sha256` lowercase hex for files, `linkTarget`
//!   for symlinks as stored. `blob` is set for each path `git ls-files` lists in the
//!   repo or submodule holding it: for a file, what `git hash-object --stdin-paths`
//!   prints for it with `-c core.safecrlf=false` and the repo's `core.autocrlf`; for a
//!   symlink, the blob of its target as stored (`git hash-object --no-filters
//!   --stdin`), never of the file it points to.
//! - **`git[].autocrlf`**: the `core.autocrlf` those blobs were computed with. The app
//!   records what `git config core.autocrlf` prints in each repo for git.exe (`false`
//!   when unset), so a global or system setting counts as it does for Windows git; the
//!   helper then normalises and re-reads with that value. Left out here, which means
//!   the repo's local value, else `true` (Git for Windows' system default).
//! - **`linuxGit`**: true here; false when `git.exe` read it.
//!
//! Both sides run git with `core.quotepath=false`, `GIT_OPTIONAL_LOCKS=0` and
//! `LC_ALL=C`. Every `git` field of a [`GitSnapshot`] is filled, `indexFlags`
//! (`ls-files -v -z`) included; `config --local -l` may exit 1 (no local config).
//!
//! The helper compares the source's sizes and modification times with its own stat of
//! the source over `/mnt` when the copy starts; a difference counts as a change made
//! after the snapshot, and the path is read again (so a mismatch in how the two sides
//! see times costs a re-read, not a failure).

use std::cell::RefCell;
use std::collections::BTreeSet;
use std::os::unix::ffi::OsStrExt;
use std::path::Path;
use std::sync::atomic::AtomicBool;

use refract_core::migrate::build::{self, DirFacts, SkippedFolder};
use refract_core::migrate::gitconfig::parse_config;
use refract_core::migrate::gitstate::{GitSnapshot, StatusKind};
use refract_core::migrate::manifest::{Entry, EntryKind, RelPath, StatChanges};
use refract_core::migrate::{RepoGit, SourceSnapshot, SOURCE_SNAPSHOT_VERSION};

use super::fsx::{self, Visit};
use super::gitx;
use super::tree;

/// The Windows `-c` settings for the repo whose working tree is `dir`: `recorded`
/// when the snapshot has it, else from its local config (see
/// [`gitx::windows_config`]).
pub fn windows_config_of(dir: &Path, recorded: Option<&str>) -> Vec<String> {
    let local = gitx::run(dir, &[], &["config", "--local", "-l"], None)
        .map(|out| parse_config(&String::from_utf8_lossy(&out.stdout)))
        .unwrap_or_default();
    gitx::windows_config(&local, recorded)
}

/// Builds the [`SourceSnapshot`] of the repo at `repo` as the module docs describe.
pub fn build(repo: &Path) -> Result<SourceSnapshot, String> {
    let git = git_snapshots(repo)?;
    let skipped = skipped_folders(repo, &git);
    let paths: Vec<RelPath> = skipped.iter().map(|s| s.path.clone()).collect();
    let mut manifest = tree::manifest(
        repo,
        &paths,
        &[],
        &[],
        &AtomicBool::new(false),
        &mut |_, _| {},
    )
    .map_err(|_| "cancelled".to_string())??;
    for repo_git in &git {
        let config = windows_config_of(&join(repo, &repo_git.path), None);
        tree::add_blobs(
            repo,
            &mut manifest,
            std::slice::from_ref(&repo_git.path),
            &config,
            None,
        )?;
    }
    manifest.skipped = skipped;
    Ok(SourceSnapshot {
        version: SOURCE_SNAPSHOT_VERSION,
        manifest,
        git,
        linux_git: true,
    })
}

fn join(root: &Path, rel: &str) -> std::path::PathBuf {
    if rel.is_empty() {
        root.to_owned()
    } else {
        root.join(rel)
    }
}

/// G0 of the top repo and each checked-out submodule, read with the Windows settings.
pub fn git_snapshots(repo: &Path) -> Result<Vec<RepoGit>, String> {
    let config = windows_config_of(repo, None);
    let mut paths = vec![String::new()];
    paths.extend(gitx::submodule_paths(repo, &config)?);
    paths
        .into_iter()
        .map(|path| {
            let dir = join(repo, &path);
            let snapshot = gitx::snapshot(&dir, &windows_config_of(&dir, None))?;
            Ok(RepoGit {
                path,
                snapshot,
                autocrlf: None,
            })
        })
        .collect()
}

/// The folders to skip (spec §3.2), from the ignored folders in each repo's status.
fn skipped_folders(root: &Path, git: &[RepoGit]) -> Vec<SkippedFolder> {
    let ignored: BTreeSet<String> = git
        .iter()
        .flat_map(|repo| {
            ignored_dirs(&repo.snapshot)
                .into_iter()
                .map(move |dir| match repo.path.as_str() {
                    "" => dir,
                    prefix => format!("{prefix}/{dir}"),
                })
        })
        .collect();
    let found: RefCell<Vec<SkippedFolder>> = RefCell::new(Vec::new());
    let skip = |rel: &Path| {
        let rel = rel.as_os_str().as_bytes();
        found
            .borrow()
            .iter()
            .any(|s| s.path.as_str().as_bytes() == rel)
            || rel.split(|b| *b == b'/').any(|part| part == b".git")
    };
    fsx::walk(root, &skip, &mut |visit| {
        let Visit::Entry { rel, abs, meta } = visit else {
            return true;
        };
        if !meta.is_dir() {
            return true;
        }
        let Ok(path) = RelPath::from_bytes(rel.as_os_str().as_bytes()) else {
            return true;
        };
        let facts = DirFacts {
            name: path.name(),
            ignored: ignored.contains(path.as_str()),
            has_pyvenv_cfg: abs.join("pyvenv.cfg").is_file(),
            has_cachedir_tag: abs.join("CACHEDIR.TAG").is_file(),
            sibling_cargo_toml: abs.parent().is_some_and(|p| p.join("Cargo.toml").is_file()),
        };
        if let Some(rule) = build::skip_rule(&facts) {
            let (mut bytes, mut files) = (0, 0);
            fsx::walk(abs, &|_| false, &mut |inner| {
                if let Visit::Entry { meta, .. } = inner {
                    if !meta.is_dir() {
                        files += 1;
                        bytes += meta.len();
                    }
                }
                true
            });
            found.borrow_mut().push(SkippedFolder {
                path,
                rule,
                bytes,
                files,
            });
        }
        true
    });
    found.into_inner()
}

/// The folders a status lists as ignored as a whole (`! <path>/`), without the `/`.
fn ignored_dirs(snapshot: &GitSnapshot) -> Vec<String> {
    snapshot
        .status
        .iter()
        .filter(|e| e.kind == StatusKind::Ignored)
        .filter_map(|e| e.path.strip_suffix('/').map(str::to_owned))
        .collect()
}

/// Brings `snapshot` up to date with the source at `root` for the paths in `changes`
/// (see [`tree::recopy`]): removed paths leave the manifest, added and changed ones are
/// read again (hashes and blob ids with the Windows settings), and when anything but a
/// folder's time changed, G is read again for every repo, with Linux git and the
/// Windows settings.
pub fn refresh(
    snapshot: &mut SourceSnapshot,
    root: &Path,
    changes: &StatChanges,
) -> Result<(), String> {
    let gone: BTreeSet<&str> = changes.removed.iter().map(RelPath::as_str).collect();
    let touched: BTreeSet<String> = changes
        .added
        .iter()
        .chain(&changes.changed)
        .map(|p| p.as_str().to_owned())
        .collect();
    let manifest = &mut snapshot.manifest;
    manifest
        .entries
        .retain(|e| !gone.contains(e.path.as_str()) && !touched.contains(e.path.as_str()));
    let mut fresh = Vec::new();
    for path in &touched {
        let Ok(rel) = RelPath::new(path.clone()) else {
            continue;
        };
        let abs = root.join(path);
        let Ok(meta) = std::fs::symlink_metadata(&abs) else {
            continue;
        };
        let kind = if meta.file_type().is_symlink() {
            EntryKind::Symlink
        } else if meta.is_dir() {
            EntryKind::Dir
        } else {
            EntryKind::File
        };
        fresh.push(Entry {
            path: rel,
            kind,
            size: if kind == EntryKind::File {
                meta.len()
            } else {
                0
            },
            mtime_ns: fsx::mtime_ns(&meta),
            attributes: 0,
            sha256: (kind == EntryKind::File)
                .then(|| fsx::sha256_file(&abs).ok())
                .flatten(),
            blob: None,
            link_target: (kind == EntryKind::Symlink)
                .then(|| std::fs::read_link(&abs).ok())
                .flatten()
                .map(|t| t.to_string_lossy().into_owned()),
        });
    }
    let mut part = refract_core::migrate::manifest::Manifest {
        entries: fresh,
        ..Default::default()
    };
    for repo_git in &snapshot.git {
        let config = windows_config_of(&join(root, &repo_git.path), repo_git.autocrlf.as_deref());
        tree::add_blobs(
            root,
            &mut part,
            std::slice::from_ref(&repo_git.path),
            &config,
            Some(&touched),
        )?;
    }
    manifest.entries.extend(part.entries);
    manifest.entries.sort_by(|a, b| a.path.cmp(&b.path));

    let files_changed = changes
        .removed
        .iter()
        .chain(&changes.added)
        .chain(&changes.changed)
        .any(|p| !root.join(p.as_str()).is_dir());
    if files_changed {
        let repos: Vec<(String, Option<String>)> = snapshot
            .git
            .iter()
            .map(|g| (g.path.clone(), g.autocrlf.clone()))
            .collect();
        let mut fresh_git = Vec::new();
        for (path, autocrlf) in repos {
            let dir = join(root, &path);
            let git = gitx::snapshot(&dir, &windows_config_of(&dir, autocrlf.as_deref()))?;
            fresh_git.push(RepoGit {
                path,
                snapshot: git,
                autocrlf,
            });
        }
        snapshot.git = fresh_git;
    }
    Ok(())
}
