//! Moving Claude Code session history with the repo (spec §3.4): the transcripts of
//! sessions run in the old path or a folder under it are copied to the project folders
//! of the new path, each rewritten with [`rewrite_transcript`] and checked with
//! [`verify_transcript`]. The originals stay until `migrateFinish` with `delete`.
//!
//! Project folder names are lossy (`/mnt/c/dev/app/web` and `/mnt/c/dev/app-web` share
//! one), so each transcript is decided by its own working directory, the first `cwd`
//! in it: it moves only when that lies in the old path. A file under a session's own
//! folder (`<session>/subagents/…`, `<session>/tool-results/…`) follows its session
//! when it has no `cwd` of its own, and any other file of a project folder moves only
//! when every session in that folder moves.
//!
//! Each copy records the original's size, time and SHA-256 and its own SHA-256, so that
//! [`remove_originals`] deletes only originals unchanged since (a changed one is
//! copied again first, or kept when its copy changed too), and [`remove_copies`] only
//! copies nobody has written to since.

use std::collections::BTreeMap;
use std::fs;
use std::io::{BufRead, BufReader};
use std::path::{Path, PathBuf};

use refract_core::migrate::history::{moved_cwd, rewrite_transcript, verify_transcript};
use refract_core::migrate::report::{HistoryMoved, Problem};
use refract_core::sessions::{encode_project_dir, is_windows_mount};
use serde::{Deserialize, Serialize};

use super::fsx::{self, Visit};

/// One file copied, as it was when copied.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct HistoryFile {
    pub original: PathBuf,
    pub copy: PathBuf,
    /// The original's size, time and SHA-256 when it was read.
    pub size: u64,
    pub mtime_ns: i64,
    pub sha256: String,
    /// The SHA-256 of what was written to the copy.
    pub copy_sha256: String,
}

/// What [`copy`] did.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Copied {
    pub moved: HistoryMoved,
    /// Each file copied.
    pub files: Vec<HistoryFile>,
    pub problems: Vec<Problem>,
}

/// The project folders under `projects` that may hold sessions of `old` or a folder
/// under it: the one named for `old`, and those whose names extend it with `-`.
fn candidates(projects: &Path, old: &str) -> Vec<PathBuf> {
    let encoded = encode_project_dir(old);
    let fold = is_windows_mount(old);
    let matches = |name: &str| {
        let (name, encoded) = if fold {
            (name.to_ascii_lowercase(), encoded.to_ascii_lowercase())
        } else {
            (name.to_owned(), encoded.clone())
        };
        name == encoded || name.starts_with(&format!("{encoded}-"))
    };
    let Ok(entries) = fs::read_dir(projects) else {
        return Vec::new();
    };
    let mut dirs: Vec<PathBuf> = entries
        .flatten()
        .filter(|e| e.file_type().is_ok_and(|t| t.is_dir()))
        .filter(|e| e.file_name().to_str().is_some_and(matches))
        .map(|e| e.path())
        .collect();
    dirs.sort();
    dirs
}

/// The first `cwd` of a transcript, if it has one.
fn first_cwd(path: &Path) -> Option<String> {
    let file = fs::File::open(path).ok()?;
    for line in BufReader::new(file).lines() {
        let Ok(line) = line else {
            return None;
        };
        let Ok(value) = serde_json::from_str::<serde_json::Value>(&line) else {
            continue;
        };
        if let Some(cwd) = value.get("cwd").and_then(|c| c.as_str()) {
            return Some(cwd.to_owned());
        }
    }
    None
}

fn is_transcript(path: &Path) -> bool {
    path.extension().is_some_and(|e| e == "jsonl")
}

/// Where a file of a project folder goes: the project folder of its new working
/// directory, or nowhere.
type Decision = Option<String>;

/// The files of one project folder to copy: (original, the project folder name it goes
/// to, its path relative to that folder).
fn plan_dir(dir: &Path, old: &str, new: &str) -> Vec<(PathBuf, String, PathBuf)> {
    let mut files: Vec<(PathBuf, PathBuf)> = Vec::new();
    fsx::walk(dir, &|_| false, &mut |visit| {
        if let Visit::Entry { rel, abs, meta } = visit {
            if meta.is_file() {
                files.push((abs.to_owned(), rel.to_owned()));
            }
        }
        true
    });
    // Each transcript by its own working directory; `None` when it has no `cwd`.
    let mut own: BTreeMap<PathBuf, Option<Decision>> = BTreeMap::new();
    for (abs, rel) in &files {
        if is_transcript(abs) {
            let decision =
                first_cwd(abs).map(|cwd| moved_cwd(&cwd, old, new).map(|m| encode_project_dir(&m)));
            own.insert(rel.clone(), decision);
        }
    }
    let decided: Vec<&Decision> = own.values().flatten().collect();
    // Every session here moves, and to one folder: the folder's other files follow.
    let whole: Decision = match decided.first() {
        Some(Some(first)) if decided.iter().all(|d| d.as_ref() == Some(first)) => {
            Some(first.clone())
        }
        _ => None,
    };
    let session_of = |rel: &Path| -> Option<Decision> {
        let first = rel.components().next()?;
        if rel.components().count() < 2 {
            return None;
        }
        let session = PathBuf::from(format!("{}.jsonl", first.as_os_str().to_string_lossy()));
        own.get(&session).cloned().flatten()
    };
    files
        .into_iter()
        .filter_map(|(abs, rel)| {
            let decision = match own.get(&rel) {
                Some(Some(decision)) => decision.clone(),
                _ => session_of(&rel).unwrap_or_else(|| whole.clone()),
            };
            decision.map(|to| (abs, to, rel))
        })
        .collect()
}

/// Copies the history of `old` to `new` in `projects`. `progress` gets (files done,
/// files in all). Never overwrites: a file already at the copy's place is a problem.
pub fn copy(projects: &Path, old: &str, new: &str, progress: &mut dyn FnMut(u64, u64)) -> Copied {
    let mut copied = Copied::default();
    let mut files: Vec<(PathBuf, PathBuf)> = Vec::new();
    let mut pairs: Vec<(String, String)> = Vec::new();
    for dir in candidates(projects, old) {
        for (original, to, rel) in plan_dir(&dir, old, new) {
            let to_dir = projects.join(&to);
            let pair = (
                dir.to_string_lossy().into_owned(),
                to_dir.to_string_lossy().into_owned(),
            );
            if !pairs.contains(&pair) {
                pairs.push(pair);
            }
            files.push((original, to_dir.join(rel)));
        }
    }
    for (from, to) in pairs {
        copied.moved.from_dirs.push(from);
        copied.moved.to_dirs.push(to);
    }
    let total = files.len() as u64;
    progress(0, total);
    for (n, (from, to)) in files.into_iter().enumerate() {
        match copy_one(&from, &to, old, new, false) {
            Ok(file) => {
                if is_transcript(&from) {
                    copied.moved.sessions += 1;
                }
                copied.files.push(file);
            }
            Err(message) => copied.problems.push(Problem {
                path: Some(from.to_string_lossy().into_owned()),
                message,
            }),
        }
        progress(n as u64 + 1, total);
    }
    copied
}

/// Copies one file of a project folder, rewriting it when it is a transcript, and
/// returns what it copied. With `replace`, an existing copy is replaced (atomically);
/// without, it is a problem.
fn copy_one(
    from: &Path,
    to: &Path,
    old: &str,
    new: &str,
    replace: bool,
) -> Result<HistoryFile, String> {
    if !replace && fs::symlink_metadata(to).is_ok() {
        return Err(format!("{} already exists.", to.display()));
    }
    let meta = fs::metadata(from).map_err(|e| format!("Could not read it: {e}."))?;
    let bytes = fs::read(from).map_err(|e| format!("Could not read it: {e}."))?;
    let sha256 = fsx::sha256_bytes(&bytes);
    let size = bytes.len() as u64;
    let out = if is_transcript(from) {
        let text = String::from_utf8(bytes).map_err(|_| "It is not UTF-8.".to_string())?;
        let rewritten = rewrite_transcript(&text, old, new)?;
        verify_transcript(&text, &rewritten, old, new)?;
        rewritten.into_bytes()
    } else {
        bytes
    };
    if let Some(parent) = to.parent() {
        fs::create_dir_all(parent).map_err(|e| format!("Could not create its folder: {e}."))?;
    }
    let written = if replace {
        fsx::write_atomic(to, &out)
    } else {
        fs::write(to, &out)
    };
    let check = written
        .map_err(|e| format!("Could not write the copy: {e}."))
        .and_then(|()| {
            // Read back, so a short write is caught.
            let back = fs::read(to).map_err(|e| format!("Could not read the copy back: {e}."))?;
            if back != out {
                return Err("The copy differs from what was written.".into());
            }
            Ok(())
        });
    if let Err(message) = check {
        if !replace {
            let _ = fs::remove_file(to);
        }
        return Err(message);
    }
    let _ = fsx::set_mtime(to, fsx::mtime_ns(&meta));
    Ok(HistoryFile {
        original: from.to_owned(),
        copy: to.to_owned(),
        size,
        mtime_ns: fsx::mtime_ns(&meta),
        sha256,
        copy_sha256: fsx::sha256_bytes(&out),
    })
}

/// True when the file at `path` has SHA-256 `sha256`.
fn holds(path: &Path, sha256: &str) -> bool {
    fsx::sha256_file(path).is_ok_and(|h| h == sha256)
}

/// Deletes the originals of `files` (spec §6 step 4): each one unchanged since it was
/// copied goes. One that changed since (a session that went on in the old path) is
/// copied again over its copy, when nobody wrote to the copy, and then goes; otherwise
/// it stays. Returns the originals kept, with why. Folders under `projects` left empty
/// go too.
pub fn remove_originals(
    projects: &Path,
    files: &[HistoryFile],
    old: &str,
    new: &str,
) -> Vec<Problem> {
    let mut kept = Vec::new();
    let mut gone = Vec::new();
    for file in files {
        if fs::symlink_metadata(&file.original).is_err() {
            continue;
        }
        let unchanged = fs::metadata(&file.original).is_ok_and(|m| m.len() == file.size)
            && holds(&file.original, &file.sha256);
        let keep = |message: String| Problem {
            path: Some(file.original.to_string_lossy().into_owned()),
            message,
        };
        if !unchanged {
            if !holds(&file.copy, &file.copy_sha256) {
                kept.push(keep(
                    "It changed after it was copied, and so did its copy, so it was kept.".into(),
                ));
                continue;
            }
            match copy_one(&file.original, &file.copy, old, new, true) {
                // Still what was copied again: it may go.
                Ok(again) if holds(&file.original, &again.sha256) => {}
                Ok(_) => {
                    kept.push(keep("It kept changing while it was copied again.".into()));
                    continue;
                }
                Err(message) => {
                    kept.push(keep(format!(
                        "It changed after it was copied, and copying it again failed: {message}"
                    )));
                    continue;
                }
            }
        }
        if let Err(e) = fs::remove_file(&file.original) {
            kept.push(keep(format!("Could not delete it: {e}.")));
            continue;
        }
        gone.push(file.original.clone());
    }
    remove_empty_dirs(projects, &gone);
    kept
}

/// Deletes the copies in `files` that nobody wrote to since (a session resumed in the
/// new path appends to its copy, which must then stay). Returns the copies kept.
pub fn remove_copies(projects: &Path, files: &[HistoryFile]) -> Vec<Problem> {
    let mut kept = Vec::new();
    let mut gone = Vec::new();
    for file in files {
        if fs::symlink_metadata(&file.copy).is_err() {
            continue;
        }
        if !holds(&file.copy, &file.copy_sha256) {
            kept.push(Problem {
                path: Some(file.copy.to_string_lossy().into_owned()),
                message: "It changed after it was copied, so it was kept.".into(),
            });
            continue;
        }
        if fs::remove_file(&file.copy).is_ok() {
            gone.push(file.copy.clone());
        }
    }
    remove_empty_dirs(projects, &gone);
    kept
}

/// Deletes the folders under `projects` holding `paths` that are left empty.
fn remove_empty_dirs(projects: &Path, paths: &[PathBuf]) {
    let mut dirs: Vec<PathBuf> = paths
        .iter()
        .flat_map(|path| {
            path.ancestors()
                .skip(1)
                .take_while(|dir| dir.starts_with(projects) && *dir != projects)
                .map(Path::to_owned)
                .collect::<Vec<_>>()
        })
        .collect();
    dirs.sort();
    dirs.dedup();
    // Deepest first; remove_dir fails, harmlessly, on folders still holding something.
    for dir in dirs.iter().rev() {
        let _ = fs::remove_dir(dir);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn line(cwd: &str) -> String {
        format!(r#"{{"type":"user","cwd":"{cwd}","message":{{"content":"hi"}}}}"#)
    }

    #[test]
    fn copies_the_folders_of_the_path_and_its_subfolders() {
        let tmp = tempfile::tempdir().unwrap();
        let projects = tmp.path().join("projects");
        let old = "/mnt/c/Users/dev/Desktop/aurora-notes";
        let new = "/home/dev/projects/aurora-notes";
        let top = projects.join(encode_project_dir(old));
        let sub = projects.join(encode_project_dir(&format!("{old}/docs")));
        let other = projects.join(encode_project_dir(&format!("{old}-old")));
        for (dir, cwd) in [
            (&top, old.to_owned()),
            (&sub, format!("{old}/docs")),
            (&other, format!("{old}-old")),
        ] {
            fs::create_dir_all(dir).unwrap();
            fs::write(dir.join("s.jsonl"), line(&cwd) + "\n").unwrap();
        }
        fs::create_dir_all(top.join("s/subagents")).unwrap();
        fs::write(top.join("s/subagents/a.jsonl"), line(old) + "\n").unwrap();

        let copied = copy(&projects, old, new, &mut |_, _| {});
        assert_eq!(copied.problems, vec![]);
        assert_eq!(copied.moved.sessions, 3);
        let new_top = projects.join(encode_project_dir(new));
        let new_sub = projects.join(encode_project_dir(&format!("{new}/docs")));
        assert_eq!(
            fs::read_to_string(new_top.join("s.jsonl")).unwrap(),
            line(new) + "\n"
        );
        assert_eq!(
            fs::read_to_string(new_top.join("s/subagents/a.jsonl")).unwrap(),
            line(new) + "\n"
        );
        assert_eq!(
            fs::read_to_string(new_sub.join("s.jsonl")).unwrap(),
            line(&format!("{new}/docs")) + "\n"
        );
        // A neighbour whose name merely starts the same is not moved.
        assert!(!copied
            .moved
            .from_dirs
            .contains(&other.to_string_lossy().into_owned()));

        // Copying again finds the copies in the way.
        let again = copy(&projects, old, new, &mut |_, _| {});
        assert_eq!(again.problems.len(), 3);

        assert_eq!(remove_originals(&projects, &copied.files, old, new), vec![]);
        assert!(!top.exists() && !sub.exists());
        assert!(other.exists() && new_top.exists());
        assert_eq!(remove_copies(&projects, &copied.files), vec![]);
        assert!(!new_top.exists() && !new_sub.exists());
        assert!(projects.exists());
    }
}
