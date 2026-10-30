//! Moving Claude Code session history with the repo (spec §3.4): the project folders
//! of the old path and its subfolders are copied to the folders of the new path, each
//! transcript rewritten with [`rewrite_transcript`] and checked with
//! [`verify_transcript`]. The originals stay until `migrateFinish` with `delete`.

use std::fs;
use std::path::{Path, PathBuf};

use refract_core::migrate::history::{moved_cwd, rewrite_transcript, verify_transcript};
use refract_core::migrate::report::{HistoryMoved, Problem};
use refract_core::sessions::{encode_project_dir, is_windows_mount};
use serde::{Deserialize, Serialize};

use super::fsx::{self, Visit};

/// What [`copy`] did.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Copied {
    pub moved: HistoryMoved,
    /// Each file copied: (original, copy).
    pub files: Vec<(PathBuf, PathBuf)>,
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

/// The working directory a project folder's transcripts were written in, when it is
/// `old` or under it: the first `cwd` in its transcripts that is.
fn moved_dir(dir: &Path, old: &str, new: &str) -> Option<String> {
    let mut found = None;
    fsx::walk(dir, &|_| false, &mut |visit| {
        let Visit::Entry { abs, meta, .. } = visit else {
            return true;
        };
        if !meta.is_file() || abs.extension().is_none_or(|e| e != "jsonl") {
            return true;
        }
        let Ok(text) = fs::read_to_string(abs) else {
            return true;
        };
        for line in text.lines() {
            let Ok(value) = serde_json::from_str::<serde_json::Value>(line) else {
                continue;
            };
            if let Some(moved) = value
                .get("cwd")
                .and_then(|c| c.as_str())
                .and_then(|cwd| moved_cwd(cwd, old, new))
            {
                found = Some(moved);
                return false;
            }
        }
        true
    });
    found
}

/// Copies the history of `old` to `new` in `projects`. `progress` gets (files done,
/// files in all). Never overwrites: a file already at the copy's place is a problem.
pub fn copy(projects: &Path, old: &str, new: &str, progress: &mut dyn FnMut(u64, u64)) -> Copied {
    let mut copied = Copied::default();
    let plan: Vec<(PathBuf, PathBuf)> = candidates(projects, old)
        .into_iter()
        .filter_map(|dir| {
            let cwd = moved_dir(&dir, old, new)?;
            Some((dir, projects.join(encode_project_dir(&cwd))))
        })
        .collect();
    let mut files: Vec<(PathBuf, PathBuf)> = Vec::new();
    for (from, to) in &plan {
        fsx::walk(from, &|_| false, &mut |visit| {
            if let Visit::Entry { rel, meta, .. } = visit {
                if meta.is_file() {
                    files.push((from.join(rel), to.join(rel)));
                }
            }
            true
        });
        copied
            .moved
            .from_dirs
            .push(from.to_string_lossy().into_owned());
        copied.moved.to_dirs.push(to.to_string_lossy().into_owned());
    }
    let total = files.len() as u64;
    progress(0, total);
    for (n, (from, to)) in files.into_iter().enumerate() {
        match copy_one(&from, &to, old, new) {
            Ok(transcript) => {
                if transcript {
                    copied.moved.sessions += 1;
                }
                copied.files.push((from, to));
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

/// Copies one file of a project folder, rewriting it when it is a transcript. Returns
/// whether it was one.
fn copy_one(from: &Path, to: &Path, old: &str, new: &str) -> Result<bool, String> {
    if fs::symlink_metadata(to).is_ok() {
        return Err(format!("{} already exists.", to.display()));
    }
    let meta = fs::metadata(from).map_err(|e| format!("Could not read it: {e}."))?;
    let bytes = fs::read(from).map_err(|e| format!("Could not read it: {e}."))?;
    let transcript = from.extension().is_some_and(|e| e == "jsonl");
    let out = if transcript {
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
    fs::write(to, &out).map_err(|e| format!("Could not write the copy: {e}."))?;
    // Read back, so a short write is caught.
    let back = fs::read(to).map_err(|e| format!("Could not read the copy back: {e}."))?;
    if back != out {
        return Err("The copy differs from what was written.".into());
    }
    let _ = fsx::set_mtime(to, fsx::mtime_ns(&meta));
    Ok(transcript)
}

/// Deletes the `files`' originals (`old`) or copies (`!old`), then the folders under
/// `projects` left empty. Problems are ignored: history is never worth failing a
/// finish over.
pub fn remove(projects: &Path, files: &[(PathBuf, PathBuf)], old: bool) {
    let mut dirs: Vec<PathBuf> = Vec::new();
    for (original, copy) in files {
        let path = if old { original } else { copy };
        let _ = fs::remove_file(path);
        dirs.extend(
            path.ancestors()
                .skip(1)
                .take_while(|dir| dir.starts_with(projects) && *dir != projects)
                .map(Path::to_owned),
        );
    }
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

        remove(&projects, &copied.files, true);
        assert!(!top.exists() && !sub.exists());
        assert!(other.exists() && new_top.exists());
        remove(&projects, &copied.files, false);
        assert!(!new_top.exists() && !new_sub.exists());
        assert!(projects.exists());
    }
}
