//! The status of a linked git worktree (see `.plans/worktrees-spec.md` §4): whether
//! anything in it could be lost, and so whether it can be removed. The helper reads the
//! facts with git; this decides the git rows of the status table. The agent rows (an
//! agent waiting or running) come first and are put on top by the app, since sessions
//! change without a new snapshot.

use crate::protocol::{GitOperation, WorktreeStatus};

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
    /// Commits on HEAD that no remote-tracking ref has, capped at [`MAX_ONLY_HERE`].
    pub only_here: u32,
    /// The default branch's name when HEAD is its ancestor.
    pub merged: Option<String>,
    /// The oldest tag containing HEAD.
    pub release: Option<String>,
}

/// The first matching row of spec §4 rows 3-10, and whether the worktree can be removed
/// as far as git goes: rows 8-10 (in a release, merged, or pushed), and not on a Windows
/// drive. The app also needs no agent working or waiting there.
pub fn derive_worktree_status(f: &WorktreeFacts) -> (WorktreeStatus, bool) {
    let status = if f.locked {
        WorktreeStatus::Locked
    } else if let Some(op) = f.operation {
        WorktreeStatus::Operation { op }
    } else if f.changes > 0 {
        WorktreeStatus::Changes { count: f.changes }
    } else if !f.untracked_checked {
        WorktreeStatus::Unchecked
    } else if f.only_here > 0 && f.merged.is_none() {
        WorktreeStatus::OnlyHere {
            count: f.only_here.min(MAX_ONLY_HERE),
        }
    } else if let Some(tag) = &f.release {
        WorktreeStatus::Release { tag: tag.clone() }
    } else if let Some(branch) = &f.merged {
        WorktreeStatus::Merged {
            branch: branch.clone(),
        }
    } else {
        WorktreeStatus::Pushed
    };
    let removable = !f.windows_drive
        && matches!(
            status,
            WorktreeStatus::Release { .. } | WorktreeStatus::Merged { .. } | WorktreeStatus::Pushed
        );
    (status, removable)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A clean worktree whose commits are all on a remote.
    fn pushed() -> WorktreeFacts {
        WorktreeFacts {
            untracked_checked: true,
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
                    only_here: 2,
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
            assert_eq!(
                derive_worktree_status(&facts),
                (status, removable),
                "{facts:?}"
            );
        }
    }

    #[test]
    fn earlier_rows_win() {
        // Locked beats everything after it.
        let all = WorktreeFacts {
            locked: true,
            operation: Some(GitOperation::Merge),
            changes: 4,
            only_here: 7,
            ..pushed()
        };
        assert_eq!(derive_worktree_status(&all).0, WorktreeStatus::Locked);
        // An operation beats changes.
        let op = WorktreeFacts {
            locked: false,
            ..all.clone()
        };
        assert_eq!(
            derive_worktree_status(&op).0,
            WorktreeStatus::Operation {
                op: GitOperation::Merge
            }
        );
        // Changes beat unpushed commits.
        let dirty = WorktreeFacts {
            operation: None,
            ..op
        };
        assert_eq!(
            derive_worktree_status(&dirty).0,
            WorktreeStatus::Changes { count: 4 }
        );
        // Changes on a Windows drive are still changes.
        let dirty_on_windows = WorktreeFacts {
            untracked_checked: false,
            windows_drive: true,
            ..dirty
        };
        assert_eq!(
            derive_worktree_status(&dirty_on_windows),
            (WorktreeStatus::Changes { count: 4 }, false)
        );
    }

    #[test]
    fn merged_commits_are_not_only_here() {
        let merged = WorktreeFacts {
            only_here: 5,
            merged: Some("main".into()),
            ..pushed()
        };
        assert_eq!(
            derive_worktree_status(&merged),
            (
                WorktreeStatus::Merged {
                    branch: "main".into()
                },
                true
            )
        );
        // A tag containing unpushed, unmerged commits does not make them safe.
        let tagged = WorktreeFacts {
            only_here: 5,
            release: Some("v1.0.0".into()),
            ..pushed()
        };
        assert_eq!(
            derive_worktree_status(&tagged),
            (WorktreeStatus::OnlyHere { count: 5 }, false)
        );
    }

    #[test]
    fn only_here_is_capped() {
        let many = WorktreeFacts {
            only_here: 250,
            ..pushed()
        };
        assert_eq!(
            derive_worktree_status(&many).0,
            WorktreeStatus::OnlyHere { count: 100 }
        );
    }

    #[test]
    fn nothing_is_removable_on_a_windows_drive() {
        let windows = WorktreeFacts {
            windows_drive: true,
            ..pushed()
        };
        assert_eq!(
            derive_worktree_status(&windows),
            (WorktreeStatus::Pushed, false)
        );
    }
}
