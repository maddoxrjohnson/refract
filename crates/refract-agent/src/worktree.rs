//! Removing a linked worktree (`removeWorktree`), checking one on a Windows drive
//! (`checkWorktree`), and dropping git's record of one whose folder is gone
//! (`pruneWorktree`); spec in `.plans/worktrees-spec.md` §5 and §10.
//!
//! Data safety (spec §5): every rule is checked again when the removal runs, in order,
//! whatever the app saw. The only command that changes anything is `git worktree
//! remove` without `--force`, whose own clean check is a second guard; nothing is ever
//! deleted by hand, and the branch is never deleted, moved or reset. A refusal names the
//! rule that failed in plain words and leaves everything as it was.
//!
//! The cheap rules (locked, operation, commits, agent) run before the slow ones, so a
//! worktree in use is refused at once, and again right after them, just before git: on
//! a Windows drive the slow ones take minutes, long enough for a commit or an agent to
//! come along. HEAD is read with the first run and must be the same at the second.
//!
//! Rule 7 sees an agent only through its process's working directory: one started in
//! the main checkout that works in this worktree by path is not seen.

use std::fs;
use std::path::{Path, PathBuf};

use refract_core::protocol::{code, ErrorBody, GitOperation, MissingWorktreeStatus, WorktreeCheck};
use refract_core::sessions::path_within;
use refract_core::worktree::{derive_missing_status, derive_worktree_status, WorktreeFacts};

use crate::git::{self, MAX_IGNORED};
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
    /// The repos Refract's refactors were started from, from their records.
    pub refactor_repos: Vec<PathBuf>,
    /// Every repo the agent knows, as it names them.
    pub known_repos: Vec<PathBuf>,
    /// A session of the worktree was live when the removal was asked for.
    pub agent_live: bool,
    /// Where refactor worktrees go (see [`scan::refactor_worktrees_dir`]).
    pub refactor_dir: Option<PathBuf>,
    /// The top-level ignored entries the confirm showed, when the app sent them (see
    /// [`check_ignored_shown`]).
    pub ignored: Option<Vec<String>>,
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

/// What a removal did.
#[derive(Debug)]
pub enum Removed {
    /// The worktree is gone.
    Done,
    /// Git failed partway but dropped the worktree's record: the worktree (at `path`, as
    /// the app sent it) is gone for git, and the message says what is left in the folder.
    Half { path: PathBuf, refused: ErrorBody },
    /// Refused, or git refused; nothing was changed.
    Refused(ErrorBody),
    /// Refused because ignored entries appeared that the confirm did not show (see
    /// [`check_ignored_shown`]); nothing was changed. The worktree (at `path`, as the app
    /// sent it) should be read again, ignored entries included, so the next confirm
    /// lists them.
    NewIgnored { path: PathBuf, refused: ErrorBody },
}

impl From<ErrorBody> for Removed {
    fn from(refused: ErrorBody) -> Removed {
        Removed::Refused(refused)
    }
}

/// What a check (`checkWorktree`) needs from the agent's state.
#[derive(Debug, Clone)]
pub struct Check {
    /// The worktree's path, as the app sent it.
    pub path: PathBuf,
    /// The worktrees of Refract's refactors, from their records.
    pub refactor_worktrees: Vec<PathBuf>,
    /// Every repo the agent knows, as it names them.
    pub known_repos: Vec<PathBuf>,
    /// Where refactor worktrees go (see [`scan::refactor_worktrees_dir`]).
    pub refactor_dir: Option<PathBuf>,
}

/// What dropping a missing worktree's record (`pruneWorktree`) needs.
#[derive(Debug, Clone)]
pub struct Prune {
    /// The main repo, as the agent knows it.
    pub repo: PathBuf,
    /// The entry's path, as `git worktree list` gives it and the app sent it.
    pub path: PathBuf,
    /// The worktrees of Refract's refactors, from their records.
    pub refactor_worktrees: Vec<PathBuf>,
    /// Every repo the agent knows, as it names them.
    pub known_repos: Vec<PathBuf>,
    /// Where refactor worktrees go (see [`scan::refactor_worktrees_dir`]).
    pub refactor_dir: Option<PathBuf>,
}

/// Refuses (spec §5 rule 2) when `path` (real path `real`) is one of Refract's refactor
/// worktrees: in `refactor_dir`, or named by a refactor record.
fn check_not_refactor(
    path: &Path,
    real: &Path,
    refactor_dir: Option<&Path>,
    refactor_worktrees: &[PathBuf],
) -> Result<(), ErrorBody> {
    let in_refactor_dir = refactor_dir.is_some_and(|dir| {
        path.starts_with(dir) || fs::canonicalize(dir).is_ok_and(|dir| real.starts_with(dir))
    });
    let recorded = refactor_worktrees
        .iter()
        .any(|w| same_dir(w, real) || same_dir(w, path));
    if in_refactor_dir || recorded {
        return Err(refuse(
            "Refract's own refactor worktrees are removed from the Refactor section.",
        ));
    }
    Ok(())
}

/// True when the worktree at `path` names its git dir by a Windows path in its `.git`
/// file, or its admin dir's `gitdir` names it by one (`X:/` or `X:\`): Git for Windows
/// made or repaired it, and Linux git would leave its record pointing nowhere.
pub(crate) fn made_by_git_for_windows(path: &Path) -> bool {
    let windows = |text: &str| {
        let text = text.trim();
        scan::from_windows_path(text) != Path::new(text)
    };
    let dot_git = fs::read_to_string(path.join(".git")).ok();
    let target = dot_git
        .as_deref()
        .and_then(|t| t.trim().strip_prefix("gitdir:"))
        .map(str::trim);
    if target.is_some_and(windows) {
        return true;
    }
    scan::git_dir(path)
        .and_then(|dir| fs::read_to_string(dir.join("gitdir")).ok())
        .is_some_and(|t| windows(&t))
}

const MADE_BY_GIT_FOR_WINDOWS: &str =
    "This worktree was made by Git for Windows; remove it from Windows.";

/// The main working tree of the linked worktree at `path`, from the path's own common
/// git directory, by its real path. Refuses (rule 1) when `path` is no linked worktree
/// or its repo is bare, and first (rule 3) when Git for Windows made it: Linux git
/// cannot follow the `.git` of such a worktree, so it would read as no linked worktree
/// at all.
pub fn main_of(path: &Path) -> Result<PathBuf, ErrorBody> {
    if made_by_git_for_windows(path) {
        return Err(refuse(MADE_BY_GIT_FOR_WINDOWS));
    }
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
/// The cheap rules run again after the slow clean check (rule 5), just before the
/// removal, so the least time passes between them.
///
/// On a Windows drive every rule applies as anywhere else (the full untracked status
/// runs there, slowly); rule 3 only refuses worktrees made by Git for Windows (spec
/// §10.1). When `git worktree remove` fails, the worktree list is read again: a Windows
/// program holding files open makes git fail partway, after it dropped the record.
pub fn remove(removal: &Removal) -> Removed {
    let (real, main) = match check_removal(removal) {
        Ok(found) => found,
        Err(refused) => return refused,
    };

    // 8. Git removes it, with its own clean check; never `--force`, never `rm`. That check
    // follows `status.showUntrackedFiles`, so it is set to list untracked files, and
    // trusts an fsmonitor, which on a Windows drive misses what Windows programs write.
    let worktree = path_str(&real);
    let out = match run_git(
        &main,
        &[
            "-c",
            "status.showUntrackedFiles=normal",
            "-c",
            "core.fsmonitor=false",
            "worktree",
            "remove",
            &worktree,
        ],
    ) {
        Ok(out) => out,
        Err(e) => return Removed::Refused(refuse(format!("Cannot run git: {e}"))),
    };
    if out.ok {
        return Removed::Done;
    }
    let still_listed =
        worktrees(&main).map(|list| list.iter().skip(1).any(|w| same_dir(&w.path, &real)));
    if still_listed.is_ok_and(|listed| !listed) {
        let message = if git::worktree_on_windows_drive(&real) {
            format!(
                "Git removed the worktree, but a Windows program had files open, so some are \
                 still in {worktree}. Close it and delete the folder."
            )
        } else {
            let why = out.stderr.trim().lines().next().unwrap_or("").trim();
            format!(
                "Git removed the worktree but could not delete all of its files, so some are \
                 still in {worktree} ({why}). Delete the folder."
            )
        };
        return Removed::Half {
            path: removal.path.clone(),
            refused: refuse(message),
        };
    }
    Removed::Refused(refuse(format!(
        "Git refused to remove the worktree: {}",
        out.stderr.trim()
    )))
}

/// Spec §5 rules 1-7 for a removal, in order: the worktree's real path and its main
/// working tree when every rule passes. The cheap rules (4, 6, 7) run before the clean
/// check (5) and again after it (see [`check_unchanged`]). Within 5, the hidden edits and
/// the full status run first and the ignored entries and the search for another repo
/// last, so ignored files made while the status runs (minutes on a Windows drive) are
/// still seen.
fn check_removal(removal: &Removal) -> Result<(PathBuf, PathBuf), Removed> {
    let real = fs::canonicalize(&removal.path).map_err(|_| not_linked())?;

    // 3 first, then 1 (see `main_of`): a non-main entry of the main repo's own worktree
    // list.
    let main = main_of(&real)?;
    let list =
        worktrees(&main).map_err(|e| refuse(format!("Cannot list the repo's worktrees: {e}")))?;
    let Some(entry) = list.iter().skip(1).find(|w| same_dir(&w.path, &real)) else {
        return Err(not_linked().into());
    };

    // 2. Not one of Refract's refactor worktrees.
    check_not_refactor(
        &removal.path,
        &real,
        removal.refactor_dir.as_deref(),
        &removal.refactor_worktrees,
    )?;

    // 4. Not prunable (the rest of 4 is in the cheap rules).
    let git_dir = scan::git_dir(&real).ok_or_else(not_linked)?;
    if entry.prunable {
        return Err(
            refuse("Git lists this worktree as prunable, so Refract leaves it alone.").into(),
        );
    }

    // Not the repo a refactor was started from: removing it would leave the refactor with
    // nothing to merge into.
    let has_refactor = removal
        .refactor_repos
        .iter()
        .any(|r| same_dir(r, &real) || same_dir(r, &removal.path));
    if has_refactor {
        return Err(refuse("This worktree has a refactor; merge or discard it first.").into());
    }

    // 4, 6 and 7, with HEAD as it is now.
    let head = git::head_commit(&real);
    check_quick(removal, &real, &git_dir, entry.locked, head.as_deref())?;

    // 5. Clean, untracked files included, with nothing git does not show: no edits hidden
    // by assume-unchanged or skip-worktree, no other repo inside (another worktree, a
    // known repo, a submodule, or a `.git` in an ignored folder), which `git worktree
    // remove` would delete with the folder, and no ignored entry the confirm did not show.
    let hidden =
        git::hidden_edits(&real).ok_or_else(|| refuse("Cannot list the worktree's files."))?;
    if !hidden.is_empty() {
        return Err(refuse(
            "Some files are marked so git hides their changes (assume-unchanged or skip-worktree).",
        )
        .into());
    }
    let status = run_git(
        &real,
        &[
            "-c",
            "core.fsmonitor=false",
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
        ))
        .into());
    }
    if !status.stdout.trim().is_empty() {
        return Err(refuse("This worktree has uncommitted changes.").into());
    }
    let ignored = check_nothing_nested(&real, &main, &list, &removal.known_repos)?;
    check_ignored_shown(
        removal.ignored.as_deref(),
        &ignored,
        git::worktree_on_windows_drive(&real),
    )
    .map_err(|refused| Removed::NewIgnored {
        path: removal.path.clone(),
        refused,
    })?;

    // 4, 6 and 7 again, now that the slow part is done.
    check_unchanged(removal, &real, &git_dir, entry.locked, head.as_deref())?;
    Ok((real, main))
}

/// The cheap rules for the worktree at `real` (own git dir `git_dir`), with HEAD at
/// `head` (`None` when it has no commit): 4, not locked (`listed_locked` from the
/// worktree list) and no operation under way; 6, every commit on a remote or merged
/// into the default branch; 7, no agent running there.
fn check_quick(
    removal: &Removal,
    real: &Path,
    git_dir: &Path,
    listed_locked: bool,
    head: Option<&str>,
) -> Result<(), ErrorBody> {
    // 4. Not locked, no operation under way.
    if listed_locked || git::is_locked(git_dir) {
        return Err(refuse("This worktree is locked."));
    }
    if let Some(op) = git::operation(git_dir) {
        return Err(refuse(operation_message(op)));
    }

    // 6. Every commit on a remote, or merged into the default branch.
    let rev = head.unwrap_or("HEAD");
    let merged =
        git::default_branch(real).is_some_and(|(_, full)| git::is_ancestor(real, rev, &full));
    let only_here = git::only_here_at(real, rev);
    if only_here.is_none() && !merged {
        return Err(refuse(
            "Cannot tell whether this worktree's commits are pushed.",
        ));
    }
    if only_here.is_some_and(|n| n > 0) && !merged {
        return Err(refuse("Some commits are only on this computer."));
    }

    // 7. No agent running there.
    let worktree = path_str(real);
    let given = path_str(&removal.path);
    let inside = procs::scan()
        .iter()
        .any(|p| path_within(&p.cwd, &worktree) || path_within(&p.cwd, &given));
    if removal.agent_live || inside {
        return Err(refuse("An agent is running in this worktree."));
    }
    Ok(())
}

/// The cheap rules again (see [`check_quick`]), right before git removes the worktree,
/// with HEAD still at `head`, as it was when they first ran: a commit made meanwhile
/// was not looked at by the clean check.
fn check_unchanged(
    removal: &Removal,
    real: &Path,
    git_dir: &Path,
    listed_locked: bool,
    head: Option<&str>,
) -> Result<(), ErrorBody> {
    if git::head_commit(real).as_deref() != head {
        return Err(refuse(
            "This worktree's HEAD moved while it was being checked. Check again.",
        ));
    }
    check_quick(removal, real, git_dir, listed_locked, head)
}

/// Refuses when the worktree's ignored paths (`paths`, as [`git::ignored_paths`] gives
/// them) have a top-level entry that the confirm did not show (`shown`): it would be
/// deleted unseen. Nothing is compared when the app sent no list (an older app), or one
/// of [`MAX_IGNORED`] entries or more, where the confirm said "and more" and so did not
/// show them all. On a Windows drive (`windows`) the list comes from a check, so the
/// refusal asks for another; elsewhere the worktree is read again after the refusal.
fn check_ignored_shown(
    shown: Option<&[String]>,
    paths: &[String],
    windows: bool,
) -> Result<(), ErrorBody> {
    let Some(shown) = shown.filter(|s| s.len() < MAX_IGNORED) else {
        return Ok(());
    };
    let mut new: Vec<String> = Vec::new();
    for path in paths {
        let top = git::top_entry(path);
        if !shown.contains(&top) && !new.contains(&top) {
            new.push(top);
        }
    }
    if new.is_empty() {
        return Ok(());
    }
    let mut named = new.iter().take(3).cloned().collect::<Vec<_>>().join(", ");
    if new.len() > 3 {
        named.push_str(" and more");
    }
    let next = if windows {
        "Check again."
    } else {
        "Review the list and remove again."
    };
    Err(refuse(format!(
        "New ignored files appeared since you confirmed ({named}). {next}"
    )))
}

/// Reads the worktree at `check.path` as a Linux-side worktree is read (spec §10.1):
/// untracked files listed, and once nothing else keeps it, the ignored entries and a
/// search of the ignored folders for another repo, on any drive. Edits git hides count
/// as changes. Read-only. Refused, as the removal would be, for a path that is no
/// linked worktree, one Git for Windows made, a refactor worktree and a prunable one.
pub fn check(check: &Check) -> Result<WorktreeCheck, ErrorBody> {
    let real = fs::canonicalize(&check.path).map_err(|_| not_linked())?;
    let main = main_of(&real)?;
    let list =
        worktrees(&main).map_err(|e| refuse(format!("Cannot list the repo's worktrees: {e}")))?;
    let Some(entry) = list.iter().skip(1).find(|w| same_dir(&w.path, &real)) else {
        return Err(not_linked());
    };
    check_not_refactor(
        &check.path,
        &real,
        check.refactor_dir.as_deref(),
        &check.refactor_worktrees,
    )?;
    if entry.prunable {
        return Err(refuse(
            "Git lists this worktree as prunable, so Refract leaves it alone.",
        ));
    }
    let git_dir = scan::git_dir(&real).ok_or_else(not_linked)?;
    // Edits git hides (assume-unchanged, skip-worktree) count as changes: the removal
    // refuses them.
    let hidden =
        git::hidden_edits(&real).ok_or_else(|| refuse("Cannot list the worktree's files."))?;
    let changes =
        git::count_changes(&real).ok_or_else(|| refuse("Cannot read the worktree's status."))?;
    let changes = changes + hidden.len() as u32;
    let refs = git::read_worktree_refs(&real);
    let mut facts = WorktreeFacts {
        locked: entry.locked || git::is_locked(&git_dir),
        operation: git::operation(&git_dir),
        changes,
        untracked_checked: true,
        windows_drive: false,
        only_here: refs.only_here,
        merged: refs.merged,
        release: refs.release,
        holds_repo: git::has_submodules(&real, &git_dir)
            || repo_inside(&real, &main, &list, &check.known_repos),
        too_many_ignored: false,
        git_for_windows: false,
    };
    let mut ignored = Vec::new();
    if derive_worktree_status(&facts).1 {
        let paths = git::ignored_paths(&real)
            .ok_or_else(|| refuse("Cannot list the worktree's ignored files."))?;
        ignored = git::top_level(&paths);
        match git::nested_repo_in(&real, &paths) {
            git::Nested::None => {}
            git::Nested::Repo => facts.holds_repo = true,
            git::Nested::TooMany => facts.too_many_ignored = true,
        }
    }
    let (status, removable, _) = derive_worktree_status(&facts);
    if !removable {
        ignored.clear();
    }
    Ok(WorktreeCheck {
        status,
        removable,
        ignored,
    })
}

/// True when the main working tree `main`, another entry of its worktree list `list`,
/// or a repo of `known_repos` lies inside the worktree at `real`.
fn repo_inside(
    real: &Path,
    main: &Path,
    list: &[crate::refactor::Worktree],
    known_repos: &[PathBuf],
) -> bool {
    let inside = |other: &Path| {
        let other = fs::canonicalize(other).unwrap_or_else(|_| other.to_owned());
        other != real && other.starts_with(real)
    };
    inside(main) || list.iter().any(|w| inside(&w.path)) || known_repos.iter().any(|r| inside(r))
}

/// Drops git's record of the missing worktree `prune.path` of `prune.repo` (spec
/// §10.2): checks rules 1-4 again, in order, then that the folder is still gone, and
/// runs `git -C <repo> worktree remove <path>` without `--force` (rule 5). On a missing
/// folder git only deletes that worktree's admin dir. Never `git worktree prune`, which
/// drops every such record at once, and never `rm`; the branch is left alone (rule 6).
pub fn prune(prune: &Prune) -> Result<(), ErrorBody> {
    // 1. A known non-bare main repo, and a non-main entry of its list that git would
    // prune (or that is locked), matched by the listed path.
    let repo = &prune.repo;
    let common = repo.join(".git");
    if !fs::symlink_metadata(&common).is_ok_and(|m| m.is_dir()) {
        return Err(refuse("Not a main checkout of a repo."));
    }
    let list =
        worktrees(repo).map_err(|e| refuse(format!("Cannot list the repo's worktrees: {e}")))?;
    let Some(entry) = list
        .iter()
        .skip(1)
        .find(|w| w.path == prune.path && (w.prunable || w.locked))
    else {
        return Err(refuse("Git does not list this worktree as missing."));
    };

    // 2. Not one of Refract's refactor worktrees.
    check_not_refactor(
        &prune.path,
        &prune.path,
        prune.refactor_dir.as_deref(),
        &prune.refactor_worktrees,
    )?;

    // 3. The folder is still gone.
    check_still_gone(&prune.path)?;

    // 4. Nothing would be lost: m1-m5 of spec §10.2 all false.
    let mounts = fs::read_to_string("/proc/mounts").ok();
    let mut missing = git::missing_entry(repo, &common, entry, mounts.as_deref())
        .ok_or_else(|| refuse("Cannot find git's record of this worktree."))?;
    missing.facts.moved_to =
        git::moved_to(&missing.admin, &prune.known_repos).map(|p| path_str(&p));
    let (status, _) = derive_missing_status(&missing.facts);
    let why = match status {
        MissingWorktreeStatus::Gone => None,
        MissingWorktreeStatus::Locked => Some("This worktree is locked.".to_owned()),
        MissingWorktreeStatus::DriveNotAttached => {
            Some("The drive this worktree was on is not attached.".to_owned())
        }
        MissingWorktreeStatus::Moved { to } => Some(format!(
            "This worktree was moved to {to}; run `git worktree repair` there."
        )),
        MissingWorktreeStatus::OwnRefs => Some(
            "This worktree has refs of its own (such as a bisect), which would go with it."
                .to_owned(),
        ),
        MissingWorktreeStatus::CommitsOnlyHere => {
            Some("Its commits are on no branch, tag or remote, so they would be lost.".to_owned())
        }
    };
    if let Some(why) = why {
        return Err(refuse(why));
    }

    // 5. Git drops the record; never `--force`, never `worktree prune`, never `rm`. The
    // folder is looked for again first, as the checks above take a while: had it come
    // back, git would delete it, its ignored files with it. Git's clean check is set as
    // for a removal, should it come back even so.
    check_still_gone(&prune.path)?;
    let listed = path_str(&entry.path);
    let out = run_git(
        repo,
        &[
            "-c",
            "status.showUntrackedFiles=normal",
            "-c",
            "core.fsmonitor=false",
            "worktree",
            "remove",
            &listed,
        ],
    )
    .map_err(|e| refuse(format!("Cannot run git: {e}")))?;
    if !out.ok {
        return Err(refuse(format!(
            "Git refused to clean up the worktree: {}",
            out.stderr.trim()
        )));
    }
    Ok(())
}

/// Refuses (rule 3 of spec §10.2) unless nothing is at `path`. Only "not found" counts
/// as gone: any other error (a folder above it that cannot be read, a network drive
/// that stopped answering) leaves it unknown.
fn check_still_gone(path: &Path) -> Result<(), ErrorBody> {
    match fs::symlink_metadata(path) {
        Ok(_) => Err(refuse("The folder is back.")),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(refuse(format!(
            "Can't tell whether the folder is there: {e}."
        ))),
    }
}

/// Refuses when another repo lies in the worktree at `real`: an entry of the main repo's
/// worktree list (`list`, from `main`) or a known repo inside it, a submodule, or a
/// `.git` in an ignored folder. Returns the ignored paths it read.
fn check_nothing_nested(
    real: &Path,
    main: &Path,
    list: &[crate::refactor::Worktree],
    known_repos: &[PathBuf],
) -> Result<Vec<String>, ErrorBody> {
    if repo_inside(real, main, list, known_repos) {
        return Err(refuse(
            "Another worktree or repo is inside this worktree's folder.",
        ));
    }
    let git_dir = scan::git_dir(real).ok_or_else(not_linked)?;
    if git::has_submodules(real, &git_dir) {
        return Err(refuse("This worktree has submodules."));
    }
    let ignored = git::ignored_paths(real)
        .ok_or_else(|| refuse("Cannot list the worktree's ignored files."))?;
    match git::nested_repo_in(real, &ignored) {
        git::Nested::None => {}
        git::Nested::Repo => {
            return Err(refuse(
                "An ignored folder in this worktree holds another repo.",
            ))
        }
        git::Nested::TooMany => {
            return Err(refuse(
                "This worktree has too many ignored files to check for other repos.",
            ))
        }
    }
    Ok(ignored)
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

#[cfg(test)]
mod tests {
    use super::*;
    use std::process::Command;

    fn git_in(dir: &Path, args: &[&str]) {
        let out = Command::new("git")
            .arg("-C")
            .arg(dir)
            .args(args)
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .env("GIT_AUTHOR_NAME", "Test Dev")
            .env("GIT_AUTHOR_EMAIL", "dev@example.com")
            .env("GIT_COMMITTER_NAME", "Test Dev")
            .env("GIT_COMMITTER_EMAIL", "dev@example.com")
            .output()
            .unwrap();
        assert!(
            out.status.success(),
            "git {args:?}: {}",
            String::from_utf8_lossy(&out.stderr)
        );
    }

    fn removal(path: &Path) -> Removal {
        Removal {
            path: path.to_owned(),
            refactor_worktrees: Vec::new(),
            refactor_repos: Vec::new(),
            known_repos: Vec::new(),
            agent_live: false,
            refactor_dir: None,
            ignored: None,
        }
    }

    #[test]
    fn a_head_that_moved_during_the_checks_is_refused() {
        let tmp = tempfile::tempdir().unwrap();
        let base = tmp.path().canonicalize().unwrap();
        let main = base.join("harbor-notes");
        std::fs::create_dir_all(&main).unwrap();
        git_in(&main, &["init", "-q", "-b", "main"]);
        git_in(&main, &["commit", "-q", "--allow-empty", "-m", "Start"]);
        let wt = base.join("harbor-notes-spike");
        git_in(
            &main,
            &["worktree", "add", "-q", "-b", "spike", wt.to_str().unwrap()],
        );
        let git_dir = scan::git_dir(&wt).unwrap();
        let head = git::head_commit(&wt);
        assert!(head.is_some());
        let removal = removal(&wt);
        // Merged into `main` and unchanged: the cheap rules pass again.
        check_unchanged(&removal, &wt, &git_dir, false, head.as_deref()).unwrap();
        // A commit while the slow rules ran was not looked at by them.
        git_in(&wt, &["commit", "-q", "--allow-empty", "-m", "Meanwhile"]);
        let refused = check_unchanged(&removal, &wt, &git_dir, false, head.as_deref());
        assert_eq!(
            refused.unwrap_err().message,
            "This worktree's HEAD moved while it was being checked. Check again."
        );
    }

    #[test]
    fn ignored_entries_the_confirm_did_not_show_are_refused() {
        let paths: Vec<String> = ["target/debug/app", ".env", "notes.log", "node_modules/"]
            .iter()
            .map(|p| (*p).to_owned())
            .collect();
        let shown = |list: &[&str]| list.iter().map(|p| (*p).to_owned()).collect::<Vec<_>>();
        let all = shown(&["target/", ".env", "notes.log", "node_modules/"]);
        assert!(check_ignored_shown(Some(&all), &paths, true).is_ok());
        // An older app sends nothing to compare.
        assert!(check_ignored_shown(None, &paths, true).is_ok());
        let refused = check_ignored_shown(Some(&shown(&["target/"])), &paths, true).unwrap_err();
        assert_eq!(refused.code, code::WORKTREE_REFUSED);
        assert_eq!(
            refused.message,
            "New ignored files appeared since you confirmed (.env, notes.log, node_modules/). \
             Check again."
        );
        // Off a Windows drive the worktree is read again, so the list is reviewed.
        let refused = check_ignored_shown(Some(&[]), &paths, false).unwrap_err();
        assert_eq!(
            refused.message,
            "New ignored files appeared since you confirmed (target/, .env, notes.log and \
             more). Review the list and remove again."
        );
        // The confirm said "and more", so it did not show them all: nothing to compare.
        let many: Vec<String> = (0..MAX_IGNORED).map(|n| format!("cache-{n}/")).collect();
        assert!(check_ignored_shown(Some(&many), &paths, false).is_ok());
    }
}
