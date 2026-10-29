//! Finding repos under the configured roots.

use std::collections::BTreeSet;
use std::fs;
use std::path::{Path, PathBuf};

use crate::config::Config;

/// Directory names never searched: build output and dependencies.
const SKIPPED_DIRS: &[&str] = &["node_modules", "target"];

/// Returns every repo under the roots, plus the extra repos, minus hidden ones, sorted.
///
/// A repo is a directory containing `.git` (a directory, or a file for worktrees and
/// submodules). The search does not descend into repos, hidden directories, symlinks or
/// [`SKIPPED_DIRS`], and goes at most `config.depth` levels below each root.
pub fn find_repos(config: &Config) -> Vec<PathBuf> {
    walk_roots(config).repos
}

/// What one search of the roots saw.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct Walk {
    /// Repos found, as [`find_repos`] returns them.
    pub repos: Vec<PathBuf>,
    /// Directories searched that are not repos, with whether their subdirectories are
    /// searched too (false at the depth limit). A repo can appear in any of them.
    pub dirs: Vec<(PathBuf, bool)>,
}

/// Searches the roots like [`find_repos`], also returning the directories searched, so
/// they can be watched for repos appearing.
pub fn walk_roots(config: &Config) -> Walk {
    let mut found = BTreeSet::new();
    let mut dirs = Vec::new();
    for root in &config.roots {
        walk(root, config.depth, &mut found, &mut dirs);
    }
    for extra in &config.extra {
        if is_repo(extra) {
            found.insert(extra.clone());
        }
    }
    let repos = found
        .into_iter()
        .filter(|repo| !config.hidden.contains(repo))
        .collect();
    Walk { repos, dirs }
}

/// True for directory names the search never enters.
pub fn is_skipped(name: &str) -> bool {
    name.starts_with('.') || SKIPPED_DIRS.contains(&name)
}

fn walk(
    dir: &Path,
    depth_left: usize,
    found: &mut BTreeSet<PathBuf>,
    dirs: &mut Vec<(PathBuf, bool)>,
) {
    if is_repo(dir) {
        found.insert(dir.to_owned());
        return;
    }
    if !dir.is_dir() {
        return;
    }
    dirs.push((dir.to_owned(), depth_left > 0));
    if depth_left == 0 {
        return;
    }
    let Ok(entries) = fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let Ok(kind) = entry.file_type() else {
            continue;
        };
        let name = entry.file_name();
        let name = name.to_string_lossy();
        if !kind.is_dir() || is_skipped(&name) {
            continue;
        }
        walk(&entry.path(), depth_left - 1, found, dirs);
    }
}

/// True when `dir` has a `.git` directory or file.
pub fn is_repo(dir: &Path) -> bool {
    fs::symlink_metadata(dir.join(".git")).is_ok()
}

/// The repo's git directory: `.git`, or where a `.git` file points.
pub fn git_dir(repo: &Path) -> Option<PathBuf> {
    let dot_git = repo.join(".git");
    let meta = fs::symlink_metadata(&dot_git).ok()?;
    if meta.is_dir() {
        return Some(dot_git);
    }
    let text = fs::read_to_string(&dot_git).ok()?;
    let target = text.trim().strip_prefix("gitdir:")?.trim();
    Some(repo.join(target))
}

/// The common git directory of a linked worktree, where its refs live: what the
/// `commondir` file in its git directory points to. `None` for ordinary repos.
pub fn common_dir(git_dir: &Path) -> Option<PathBuf> {
    let text = fs::read_to_string(git_dir.join("commondir")).ok()?;
    let target = text.trim();
    (!target.is_empty()).then(|| git_dir.join(target))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn mkrepo(path: &Path) {
        fs::create_dir_all(path.join(".git")).unwrap();
    }

    fn config(roots: Vec<PathBuf>, depth: usize) -> Config {
        Config {
            roots,
            extra: vec![],
            hidden: vec![],
            depth,
            claude_projects: PathBuf::from("/nonexistent"),
        }
    }

    #[test]
    fn finds_repos_within_depth_and_skips_the_rest() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        mkrepo(&root.join("tidepool-api"));
        mkrepo(&root.join("tidepool-api/nested-inside-repo"));
        mkrepo(&root.join("group/aurora-notes"));
        mkrepo(&root.join("a/b/c/too-deep"));
        mkrepo(&root.join("node_modules/dep"));
        mkrepo(&root.join(".hidden/secret"));
        let worktree = root.join("kelp-cli");
        fs::create_dir_all(&worktree).unwrap();
        fs::write(
            worktree.join(".git"),
            "gitdir: ../elsewhere/.git/worktrees/kelp\n",
        )
        .unwrap();

        let repos = find_repos(&config(vec![root.to_owned()], 3));
        assert_eq!(
            repos,
            [
                root.join("group/aurora-notes"),
                root.join("kelp-cli"),
                root.join("tidepool-api"),
            ]
        );
        assert_eq!(
            git_dir(&worktree),
            Some(worktree.join("../elsewhere/.git/worktrees/kelp"))
        );
    }

    #[test]
    fn extra_and_hidden_apply() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().join("root");
        mkrepo(&root.join("tidepool-api"));
        mkrepo(&root.join("aurora-notes"));
        let outside = tmp.path().join("outside/kelp-cli");
        mkrepo(&outside);
        let mut c = config(vec![root.clone(), tmp.path().join("missing")], 2);
        c.extra = vec![outside.clone(), tmp.path().join("not-a-repo")];
        c.hidden = vec![root.join("aurora-notes")];
        assert_eq!(find_repos(&c), [outside, root.join("tidepool-api")]);
    }

    #[test]
    fn walk_reports_searched_dirs_up_to_the_depth() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        mkrepo(&root.join("tidepool-api"));
        fs::create_dir_all(root.join("group/sub")).unwrap();
        fs::create_dir_all(root.join("node_modules/dep")).unwrap();
        let walk = walk_roots(&config(vec![root.to_owned()], 2));
        assert_eq!(walk.repos, [root.join("tidepool-api")]);
        let mut dirs = walk.dirs;
        dirs.sort();
        assert_eq!(
            dirs,
            [
                (root.to_owned(), true),
                (root.join("group"), true),
                (root.join("group/sub"), false),
            ]
        );
    }

    #[test]
    fn common_dir_follows_commondir() {
        let tmp = tempfile::tempdir().unwrap();
        let wt = tmp.path().join("main/.git/worktrees/kelp");
        fs::create_dir_all(&wt).unwrap();
        assert_eq!(common_dir(&wt), None);
        fs::write(wt.join("commondir"), "../..\n").unwrap();
        assert_eq!(common_dir(&wt), Some(wt.join("../..")));
    }

    #[test]
    fn a_root_that_is_a_repo_is_reported() {
        let tmp = tempfile::tempdir().unwrap();
        mkrepo(tmp.path());
        assert_eq!(
            find_repos(&config(vec![tmp.path().to_owned()], 3)),
            [tmp.path()]
        );
    }
}
