//! The status of a linked git worktree (see `.plans/worktrees-spec.md` §4): whether
//! anything in it could be lost, and so whether it can be removed. The helper reads the
//! facts with git; this decides the git rows of the status table. The agent rows (an
//! agent waiting or running) come first and are put on top by the app, since sessions
//! change without a new snapshot.

use crate::protocol::{GitOperation, MissingWorktreeStatus, WorktreeStatus};

/// The most commits counted as only on this computer; that many reads as "100+".
pub const MAX_ONLY_HERE: u32 = 100;

/// What git says about a linked worktree.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct WorktreeFacts {
    /// `<worktree git dir>/locked` exists.
    pub locked: bool,
    pub operation: Option<GitOperation>,
    /// Changed, staged, conflicted and untracked (not ignored) paths.
    pub changes: u32,
    /// Untracked files were listed, so `changes` counts them. False on a Windows drive.
    pub untracked_checked: bool,
    /// The worktree is on a Windows drive, where Refract does not remove worktrees yet.
    pub windows_drive: bool,
    /// Commits on HEAD that no remote-tracking ref has, capped at [`MAX_ONLY_HERE`];
    /// `None` when git cannot count them (no commits, or git failed).
    pub only_here: Option<u32>,
    /// The default branch's name when HEAD is its ancestor.
    pub merged: Option<String>,
    /// The oldest tag containing HEAD.
    pub release: Option<String>,
    /// Another repo lies in the folder, so removing it could delete that repo's work: a
    /// submodule, another worktree or known repo inside it, or a `.git` in an ignored
    /// folder.
    pub holds_repo: bool,
    /// The ignored folders hold too many folders to check them all for a `.git`.
    pub too_many_ignored: bool,
    /// Git for Windows made or repaired it (a Windows path in its `.git` or its admin
    /// dir's `gitdir`), so Refract leaves it to Windows: never checkable.
    pub git_for_windows: bool,
}

/// The first matching row of spec §4 rows 3-10; whether the worktree can be removed as
/// far as git goes: rows 8-10 (in a release, merged, or pushed), and not on a Windows
/// drive; and whether it is checkable (spec §10.1): on a Windows drive with untracked
/// files not listed, where the same facts with them listed give rows 8-10. The app also
/// needs no agent working or waiting there.
///
/// Three rows come before rows 8-10 to keep them from being offered: commits that cannot
/// be counted (and are not merged), another repo in the folder, and ignored folders too
/// big to check for one. The helper only looks
/// for repos in ignored folders when nothing earlier matched, so that row comes last.
/// On a Windows drive those two are not looked for, so `checkable` leaves them false.
///
/// With untracked files not listed, commits that are only here (or cannot be counted)
/// come before "Changes not checked" (row 6): listing the files could not make the
/// worktree removable, so the row says what really keeps it.
pub fn derive_worktree_status(f: &WorktreeFacts) -> (WorktreeStatus, bool, bool) {
    let status = status_of(f);
    let removable = !f.windows_drive && offered(&status);
    let checkable = f.windows_drive
        && !f.untracked_checked
        && !f.git_for_windows
        && offered(&status_of(&WorktreeFacts {
            untracked_checked: true,
            holds_repo: false,
            too_many_ignored: false,
            ..f.clone()
        }));
    (status, removable, checkable)
}

/// Rows 8-10: in a release, merged, or pushed.
fn offered(status: &WorktreeStatus) -> bool {
    matches!(
        status,
        WorktreeStatus::Release { .. } | WorktreeStatus::Merged { .. } | WorktreeStatus::Pushed
    )
}

fn status_of(f: &WorktreeFacts) -> WorktreeStatus {
    if f.locked {
        WorktreeStatus::Locked
    } else if let Some(op) = f.operation {
        WorktreeStatus::Operation { op }
    } else if f.changes > 0 {
        WorktreeStatus::Changes { count: f.changes }
    } else if f.only_here.is_none() && f.merged.is_none() {
        WorktreeStatus::OnlyHereUnknown
    } else if f.only_here.is_some_and(|n| n > 0) && f.merged.is_none() {
        WorktreeStatus::OnlyHere {
            count: f.only_here.unwrap_or(0).min(MAX_ONLY_HERE),
        }
    } else if !f.untracked_checked {
        WorktreeStatus::Unchecked
    } else if f.holds_repo {
        WorktreeStatus::HoldsRepo
    } else if f.too_many_ignored {
        WorktreeStatus::TooBigToCheck
    } else if let Some(tag) = &f.release {
        WorktreeStatus::Release { tag: tag.clone() }
    } else if let Some(branch) = &f.merged {
        WorktreeStatus::Merged {
            branch: branch.clone(),
        }
    } else {
        WorktreeStatus::Pushed
    }
}

/// What the helper found about a linked worktree whose folder is gone (spec §10.2).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct MissingFacts {
    /// `git worktree lock` was used on it.
    pub locked: bool,
    /// Its path is under `/mnt/`, `/media/` or `/run/media/` and no drive is mounted there.
    pub drive_not_attached: bool,
    /// A known repo whose git directory is this entry's admin dir: the folder moved there.
    pub moved_to: Option<String>,
    /// Its admin dir holds refs of its own (anything under `refs/`).
    pub own_refs: bool,
    /// Detached (or on a branch that no longer exists), and no branch, tag or remote ref
    /// holds its HEAD commit, or git could not tell.
    pub commits_only_here: bool,
}

/// The first matching row m1-m6 of spec §10.2, and whether git's record of it can be
/// dropped (only m6, `gone`).
pub fn derive_missing_status(f: &MissingFacts) -> (MissingWorktreeStatus, bool) {
    let status = if f.locked {
        MissingWorktreeStatus::Locked
    } else if f.drive_not_attached {
        MissingWorktreeStatus::DriveNotAttached
    } else if let Some(to) = &f.moved_to {
        MissingWorktreeStatus::Moved { to: to.clone() }
    } else if f.own_refs {
        MissingWorktreeStatus::OwnRefs
    } else if f.commits_only_here {
        MissingWorktreeStatus::CommitsOnlyHere
    } else {
        MissingWorktreeStatus::Gone
    };
    let cleanable = status == MissingWorktreeStatus::Gone;
    (status, cleanable)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The status and whether it is removable.
    fn derive(f: &WorktreeFacts) -> (WorktreeStatus, bool) {
        let (status, removable, _) = derive_worktree_status(f);
        (status, removable)
    }

    /// A clean worktree whose commits are all on a remote.
    fn pushed() -> WorktreeFacts {
        WorktreeFacts {
            untracked_checked: true,
            only_here: Some(0),
            ..WorktreeFacts::default()
        }
    }

    #[test]
    fn each_row_of_the_table() {
        let rows: Vec<(WorktreeFacts, WorktreeStatus, bool)> = vec![
            (
                WorktreeFacts {
                    locked: true,
                    ..pushed()
                },
                WorktreeStatus::Locked,
                false,
            ),
            (
                WorktreeFacts {
                    operation: Some(GitOperation::Rebase),
                    ..pushed()
                },
                WorktreeStatus::Operation {
                    op: GitOperation::Rebase,
                },
                false,
            ),
            (
                WorktreeFacts {
                    changes: 3,
                    ..pushed()
                },
                WorktreeStatus::Changes { count: 3 },
                false,
            ),
            (
                WorktreeFacts {
                    untracked_checked: false,
                    windows_drive: true,
                    ..pushed()
                },
                WorktreeStatus::Unchecked,
                false,
            ),
            (
                WorktreeFacts {
                    only_here: Some(2),
                    ..pushed()
                },
                WorktreeStatus::OnlyHere { count: 2 },
                false,
            ),
            (
                WorktreeFacts {
                    release: Some("v0.13.3".into()),
                    merged: Some("main".into()),
                    ..pushed()
                },
                WorktreeStatus::Release {
                    tag: "v0.13.3".into(),
                },
                true,
            ),
            (
                WorktreeFacts {
                    merged: Some("main".into()),
                    ..pushed()
                },
                WorktreeStatus::Merged {
                    branch: "main".into(),
                },
                true,
            ),
            (pushed(), WorktreeStatus::Pushed, true),
        ];
        for (facts, status, removable) in rows {
            assert_eq!(derive(&facts), (status, removable), "{facts:?}");
        }
    }

    #[test]
    fn earlier_rows_win() {
        // Locked beats everything after it.
        let all = WorktreeFacts {
            locked: true,
            operation: Some(GitOperation::Merge),
            changes: 4,
            only_here: Some(7),
            ..pushed()
        };
        assert_eq!(derive(&all).0, WorktreeStatus::Locked);
        // An operation beats changes.
        let op = WorktreeFacts {
            locked: false,
            ..all.clone()
        };
        assert_eq!(
            derive(&op).0,
            WorktreeStatus::Operation {
                op: GitOperation::Merge
            }
        );
        // Changes beat unpushed commits.
        let dirty = WorktreeFacts {
            operation: None,
            ..op
        };
        assert_eq!(derive(&dirty).0, WorktreeStatus::Changes { count: 4 });
        // Changes on a Windows drive are still changes.
        let dirty_on_windows = WorktreeFacts {
            untracked_checked: false,
            windows_drive: true,
            ..dirty
        };
        assert_eq!(
            derive(&dirty_on_windows),
            (WorktreeStatus::Changes { count: 4 }, false)
        );
    }

    #[test]
    fn merged_commits_are_not_only_here() {
        let merged = WorktreeFacts {
            only_here: Some(5),
            merged: Some("main".into()),
            ..pushed()
        };
        assert_eq!(
            derive(&merged),
            (
                WorktreeStatus::Merged {
                    branch: "main".into()
                },
                true
            )
        );
        // A tag containing unpushed, unmerged commits does not make them safe.
        let tagged = WorktreeFacts {
            only_here: Some(5),
            release: Some("v1.0.0".into()),
            ..pushed()
        };
        assert_eq!(
            derive(&tagged),
            (WorktreeStatus::OnlyHere { count: 5 }, false)
        );
    }

    #[test]
    fn only_here_is_capped() {
        let many = WorktreeFacts {
            only_here: Some(250),
            ..pushed()
        };
        assert_eq!(derive(&many).0, WorktreeStatus::OnlyHere { count: 100 });
    }

    #[test]
    fn uncounted_commits_and_nested_repos_are_kept() {
        let uncounted = WorktreeFacts {
            only_here: None,
            ..pushed()
        };
        assert_eq!(derive(&uncounted), (WorktreeStatus::OnlyHereUnknown, false));
        // Merged into the default branch is safe however many commits there are.
        let merged = WorktreeFacts {
            merged: Some("main".into()),
            ..uncounted
        };
        assert!(derive(&merged).1);
        let nested = WorktreeFacts {
            holds_repo: true,
            release: Some("v1.0.0".into()),
            ..pushed()
        };
        assert_eq!(derive(&nested), (WorktreeStatus::HoldsRepo, false));
        let too_big = WorktreeFacts {
            too_many_ignored: true,
            ..pushed()
        };
        assert_eq!(derive(&too_big), (WorktreeStatus::TooBigToCheck, false));
        // Unpushed commits say more than a nested repo.
        let both = WorktreeFacts {
            only_here: Some(3),
            ..nested
        };
        assert_eq!(derive(&both).0, WorktreeStatus::OnlyHere { count: 3 });
    }

    #[test]
    fn nothing_is_removable_on_a_windows_drive() {
        let windows = WorktreeFacts {
            windows_drive: true,
            ..pushed()
        };
        assert_eq!(derive(&windows), (WorktreeStatus::Pushed, false));
    }

    #[test]
    fn checkable_only_on_a_windows_drive_where_listing_files_could_allow_removal() {
        let unchecked = WorktreeFacts {
            untracked_checked: false,
            windows_drive: true,
            ..pushed()
        };
        let checkable = |f: &WorktreeFacts| derive_worktree_status(f).2;
        // Rows 8-10 once untracked files are listed.
        assert_eq!(
            derive_worktree_status(&unchecked),
            (WorktreeStatus::Unchecked, false, true)
        );
        for f in [
            WorktreeFacts {
                merged: Some("main".into()),
                ..unchecked.clone()
            },
            WorktreeFacts {
                release: Some("v1.4.0".into()),
                ..unchecked.clone()
            },
        ] {
            assert!(checkable(&f), "{f:?}");
        }
        // Every earlier row keeps it from being checked.
        for f in [
            WorktreeFacts {
                locked: true,
                ..unchecked.clone()
            },
            WorktreeFacts {
                operation: Some(GitOperation::Revert),
                ..unchecked.clone()
            },
            WorktreeFacts {
                changes: 1,
                ..unchecked.clone()
            },
            WorktreeFacts {
                only_here: None,
                ..unchecked.clone()
            },
            WorktreeFacts {
                only_here: Some(3),
                ..unchecked.clone()
            },
        ] {
            assert!(!checkable(&f), "{f:?}");
        }
        // Repos inside are looked for by the check itself.
        assert!(checkable(&WorktreeFacts {
            holds_repo: true,
            too_many_ignored: true,
            ..unchecked.clone()
        }));
        // Not on a Linux drive, nor on a Windows drive once untracked files are listed.
        assert!(!checkable(&pushed()));
        assert!(!checkable(&WorktreeFacts {
            untracked_checked: false,
            ..pushed()
        }));
        assert!(!checkable(&WorktreeFacts {
            untracked_checked: true,
            ..unchecked
        }));
    }

    #[test]
    fn a_known_blocker_comes_before_unchecked_files_on_a_windows_drive() {
        let unchecked = WorktreeFacts {
            untracked_checked: false,
            windows_drive: true,
            ..pushed()
        };
        let only_here = WorktreeFacts {
            only_here: Some(4),
            ..unchecked.clone()
        };
        assert_eq!(
            derive_worktree_status(&only_here),
            (WorktreeStatus::OnlyHere { count: 4 }, false, false)
        );
        let uncounted = WorktreeFacts {
            only_here: None,
            ..unchecked.clone()
        };
        assert_eq!(
            derive_worktree_status(&uncounted),
            (WorktreeStatus::OnlyHereUnknown, false, false)
        );
        // Merged commits keep nothing, so the files are what is left to check.
        let merged = WorktreeFacts {
            merged: Some("main".into()),
            ..only_here
        };
        assert_eq!(
            derive_worktree_status(&merged),
            (WorktreeStatus::Unchecked, false, true)
        );
        // Locked, an operation and changes still come first.
        let locked = WorktreeFacts {
            locked: true,
            only_here: Some(4),
            ..unchecked.clone()
        };
        assert_eq!(derive(&locked).0, WorktreeStatus::Locked);
        let dirty = WorktreeFacts {
            changes: 2,
            only_here: None,
            ..unchecked
        };
        assert_eq!(derive(&dirty).0, WorktreeStatus::Changes { count: 2 });
    }

    #[test]
    fn a_worktree_git_for_windows_made_is_never_checkable() {
        let by_windows = WorktreeFacts {
            untracked_checked: false,
            windows_drive: true,
            git_for_windows: true,
            ..pushed()
        };
        assert_eq!(
            derive_worktree_status(&by_windows),
            (WorktreeStatus::Unchecked, false, false)
        );
    }

    #[test]
    fn each_missing_row_and_precedence() {
        let derive = |f: MissingFacts| derive_missing_status(&f);
        let gone = MissingFacts::default();
        assert_eq!(derive(gone.clone()), (MissingWorktreeStatus::Gone, true));
        let all = MissingFacts {
            locked: true,
            drive_not_attached: true,
            moved_to: Some("/home/dev/projects/harbor-notes-moved".into()),
            own_refs: true,
            commits_only_here: true,
        };
        assert_eq!(derive(all.clone()), (MissingWorktreeStatus::Locked, false));
        let all = MissingFacts {
            locked: false,
            ..all
        };
        assert_eq!(
            derive(all.clone()),
            (MissingWorktreeStatus::DriveNotAttached, false)
        );
        let all = MissingFacts {
            drive_not_attached: false,
            ..all
        };
        assert_eq!(
            derive(all.clone()),
            (
                MissingWorktreeStatus::Moved {
                    to: "/home/dev/projects/harbor-notes-moved".into()
                },
                false
            )
        );
        let all = MissingFacts {
            moved_to: None,
            ..all
        };
        assert_eq!(derive(all.clone()), (MissingWorktreeStatus::OwnRefs, false));
        let all = MissingFacts {
            own_refs: false,
            ..all
        };
        assert_eq!(derive(all), (MissingWorktreeStatus::CommitsOnlyHere, false));
    }
}
