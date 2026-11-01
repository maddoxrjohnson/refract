//! Removing a linked worktree (`removeWorktree`; spec in `.plans/worktrees-spec.md`).
//!
//! Data safety (spec §5): every rule is checked again when the removal runs, in order,
//! whatever the app saw. The only command that changes anything is `git worktree
//! remove` without `--force`, whose own clean check is a second guard; nothing is ever
//! deleted by hand, and the branch is never deleted, moved or reset. A refusal names the
//! rule that failed in plain words and leaves everything as it was.

use std::fs;
use std::path::{Path, PathBuf};

use refract_core::protocol::{code, ErrorBody, GitOperation};
use refract_core::sessions::path_within;

use crate::git;
use crate::procs;
use crate::refactor::{run_git, same_dir, worktrees};
use crate::scan;
use crate::state::path_str;

/// What a removal needs from the agent's state, taken when it was asked for.
#[derive(Debug, Clone)]
pub struct Removal {
    /// The worktree's path, as the app sent it.
    pub path: PathBuf,
    /// The worktrees of Refract's refactors, from their records.
    pub refactor_worktrees: Vec<PathBuf>,
    /// A session of the worktree was live when the removal was asked for.
    pub agent_live: bool,
    /// Where refactor worktrees go (see [`scan::refactor_worktrees_dir`]).
    pub refactor_dir: Option<PathBuf>,
}

/// A refusal with [`code::WORKTREE_REFUSED`].
pub fn refuse(message: impl Into<String>) -> ErrorBody {
    ErrorBody {
        code: code::WORKTREE_REFUSED,
        message: message.into(),
    }
}

fn not_linked() -> ErrorBody {
    refuse("Not a linked worktree.")
}

/// The main working tree of the linked worktree at `path`, from the path's own common
/// git directory, by its real path. Refuses (rule 1) when `path` is no linked worktree
/// or its repo is bare.
pub fn main_of(path: &Path) -> Result<PathBuf, ErrorBody> {
    let dot_git = fs::symlink_metadata(path.join(".git")).map_err(|_| not_linked())?;
    if dot_git.is_dir() {
        return Err(not_linked());
    }
    let git_dir = scan::git_dir(path).ok_or_else(not_linked)?;
    let common = scan::common_dir(&git_dir).ok_or_else(not_linked)?;
    let common = fs::canonicalize(common).map_err(|_| not_linked())?;
    if common.file_name().is_none_or(|n| n != ".git") {
        return Err(not_linked());
    }
    common.parent().map(Path::to_owned).ok_or_else(not_linked)
}

/// Checks spec §5 rules 1-7 in order, then removes the worktree (rule 8) with `git -C
/// <main> worktree remove <path>`, never forced. The branch is left alone (rule 9).
pub fn remove(removal: &Removal) -> Result<(), ErrorBody> {
    // 1. A non-main entry of the main repo's own worktree list.
    let real = fs::canonicalize(&removal.path).map_err(|_| not_linked())?;
    let main = main_of(&real)?;
    let list =
        worktrees(&main).map_err(|e| refuse(format!("Cannot list the repo's worktrees: {e}")))?;
    let Some(entry) = list.iter().skip(1).find(|w| same_dir(&w.path, &real)) else {
        return Err(not_linked());
    };

    // 2. Not one of Refract's refactor worktrees.
    let refactor_dir = removal.refactor_dir.as_deref();
    let in_refactor_dir = refactor_dir.is_some_and(|dir| {
        removal.path.starts_with(dir)
            || fs::canonicalize(dir).is_ok_and(|dir| real.starts_with(dir))
    });
    let recorded = removal
        .refactor_worktrees
        .iter()
        .any(|w| same_dir(w, &real) || same_dir(w, &removal.path));
    if in_refactor_dir || recorded {
        return Err(refuse(
            "Refract's own refactor worktrees are removed from the Refactor section.",
        ));
    }

    // 3. Not on a Windows drive (v1).
    if git::worktree_on_windows_drive(&removal.path) || git::worktree_on_windows_drive(&real) {
        return Err(refuse(
            "Worktrees on Windows drives can't be removed from Refract yet.",
        ));
    }

    // 4. Not locked, not prunable, no operation under way.
    let git_dir = scan::git_dir(&real).ok_or_else(not_linked)?;
    if entry.locked || git::is_locked(&git_dir) {
        return Err(refuse("This worktree is locked."));
    }
    if entry.prunable {
        return Err(refuse(
            "Git lists this worktree as prunable, so Refract leaves it alone.",
        ));
    }
    if let Some(op) = git::operation(&git_dir) {
        return Err(refuse(operation_message(op)));
    }

    // 5. Clean, untracked files included.
    let status = run_git(
        &real,
        &[
            "--no-optional-locks",
            "status",
            "--porcelain",
            "--untracked-files=all",
        ],
    )
    .map_err(|e| refuse(format!("Cannot run git: {e}")))?;
    if !status.ok {
        return Err(refuse(format!(
            "Cannot read the worktree's status: {}",
            status.stderr.trim()
        )));
    }
    if !status.stdout.trim().is_empty() {
        return Err(refuse("This worktree has uncommitted changes."));
    }

    // 6. Every commit on a remote, or merged into the default branch.
    let only_here = git::only_here(&real)
        .ok_or_else(|| refuse("Cannot tell whether this worktree's commits are pushed."))?;
    let merged =
        git::default_branch(&real).is_some_and(|(_, full)| git::is_ancestor(&real, "HEAD", &full));
    if only_here > 0 && !merged {
        return Err(refuse("Some commits are only on this computer."));
    }

    // 7. No agent running there.
    let worktree = path_str(&real);
    let given = path_str(&removal.path);
    let inside = procs::scan()
        .iter()
        .any(|p| path_within(&p.cwd, &worktree) || path_within(&p.cwd, &given));
    if removal.agent_live || inside {
        return Err(refuse("An agent is running in this worktree."));
    }

    // 8. Git removes it, with its own clean check; never `--force`, never `rm`.
    let out = run_git(&main, &["worktree", "remove", &worktree])
        .map_err(|e| refuse(format!("Cannot run git: {e}")))?;
    if !out.ok {
        return Err(refuse(format!(
            "Git refused to remove the worktree: {}",
            out.stderr.trim()
        )));
    }
    Ok(())
}

/// The refusal for an operation under way.
fn operation_message(op: GitOperation) -> &'static str {
    match op {
        GitOperation::Rebase => "A rebase is in progress in this worktree.",
        GitOperation::Merge => "A merge is in progress in this worktree.",
        GitOperation::CherryPick => "A cherry-pick is in progress in this worktree.",
        GitOperation::Revert => "A revert is in progress in this worktree.",
        GitOperation::Bisect => "A bisect is in progress in this worktree.",
    }
}
