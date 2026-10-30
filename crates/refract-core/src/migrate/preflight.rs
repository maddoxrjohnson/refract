//! Whether a migration may start (spec §4.1), where it goes by default and what the
//! dialog warns about (spec §2.2).
//!
//! The app and the helper gather the facts; [`refusals`] only decides. Busy VS Code
//! windows and agents are not refusals: they block **Start** in the dialog instead.

use serde::{Deserialize, Serialize};

use crate::sessions::is_windows_mount;

/// A git operation that must be finished or aborted first.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum GitOperation {
    Merge,
    Rebase,
    CherryPick,
    Revert,
    Bisect,
}

impl GitOperation {
    fn name(self) -> &'static str {
        match self {
            GitOperation::Merge => "merge",
            GitOperation::Rebase => "rebase",
            GitOperation::CherryPick => "cherry-pick",
            GitOperation::Revert => "revert",
            GitOperation::Bisect => "bisect",
        }
    }
}

/// What preflight found out, from both sides.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct PreflightFacts {
    /// The source is the top of a git working tree.
    pub is_git_repo: bool,
    /// The source is on a Windows drive (`/mnt/<drive>/...` from WSL).
    pub on_windows_drive: bool,
    /// The source is a linked worktree (its `.git` is a file pointing elsewhere).
    pub is_linked_worktree: bool,
    /// The source has linked worktrees (`.git/worktrees/*`).
    pub has_linked_worktrees: bool,
    /// A merge, rebase, cherry-pick, revert or bisect is in progress.
    pub operation: Option<GitOperation>,
    /// Paths of symlinks, junctions or other reparse points that point outside the repo.
    pub external_links: Vec<String>,
    /// Paths that could not be opened for reading.
    pub unreadable: Vec<String>,
    /// The repo uses Git LFS.
    pub uses_lfs: bool,
    /// `git-lfs` is installed in WSL.
    pub wsl_has_git_lfs: bool,
    /// The destination's parent is on the Linux filesystem (not under `/mnt/<drive>`).
    pub dest_on_linux: bool,
    /// Something already exists at the destination.
    pub dest_exists: bool,
    /// Bytes that will be copied.
    pub copy_bytes: u64,
    /// Free bytes on the destination's filesystem.
    pub free_bytes: u64,
}

/// One reason a migration can't start.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(
    tag = "kind",
    rename_all = "camelCase",
    rename_all_fields = "camelCase"
)]
pub enum Refusal {
    NotARepo,
    NotOnWindowsDrive,
    LinkedWorktree,
    HasLinkedWorktrees,
    OperationInProgress { operation: GitOperation },
    ExternalLinks { paths: Vec<String> },
    Unreadable { paths: Vec<String> },
    LfsMissing,
    DestNotLinux,
    DestExists,
    NotEnoughSpace { needed: u64, free: u64 },
}

/// The command that installs `git-lfs` in WSL.
pub const GIT_LFS_INSTALL: &str = "sudo apt install git-lfs";

impl Refusal {
    /// What the dialog shows: short, plain sentences.
    pub fn message(&self) -> String {
        match self {
            Refusal::NotARepo => "This folder isn't a git repo.".into(),
            Refusal::NotOnWindowsDrive => "This repo isn't on a Windows drive.".into(),
            Refusal::LinkedWorktree => {
                "This is a linked worktree. Move the main repo instead.".into()
            }
            Refusal::HasLinkedWorktrees => {
                "This repo has linked worktrees. Remove them first.".into()
            }
            Refusal::OperationInProgress { operation } => format!(
                "A {} is in progress. Finish or abort it first.",
                operation.name()
            ),
            Refusal::ExternalLinks { paths } => format!(
                "{} points outside the repo: {}.",
                plural(paths.len(), "link"),
                list(paths)
            ),
            Refusal::Unreadable { paths } => format!(
                "{} can't be read: {}. Close the program using it and try again.",
                plural(paths.len(), "file"),
                list(paths)
            ),
            Refusal::LfsMissing => format!(
                "This repo uses Git LFS, which isn't installed in WSL. Install it with `{GIT_LFS_INSTALL}`."
            ),
            Refusal::DestNotLinux => "The destination isn't on the Linux filesystem.".into(),
            Refusal::DestExists => {
                "The destination already exists. Pick another folder.".into()
            }
            Refusal::NotEnoughSpace { needed, free } => format!(
                "Not enough free space: the move needs {} and {} is free.",
                gigabytes(*needed),
                gigabytes(*free)
            ),
        }
    }
}

fn plural(n: usize, noun: &str) -> String {
    if n == 1 {
        format!("1 {noun}")
    } else {
        format!("{n} {noun}s")
    }
}

/// Up to three paths, then how many more.
fn list(paths: &[String]) -> String {
    let shown = paths.iter().take(3).cloned().collect::<Vec<_>>().join(", ");
    match paths.len() {
        0..=3 => shown,
        n => format!("{shown} and {} more", n - 3),
    }
}

fn gigabytes(bytes: u64) -> String {
    format!("{:.1} GB", bytes as f64 / 1e9)
}

/// The space a copy of `copy_bytes` needs: 1.2 times its size plus 1 GB (10⁹ bytes).
pub fn space_needed(copy_bytes: u64) -> u64 {
    copy_bytes
        .saturating_add(copy_bytes / 5)
        .saturating_add(1_000_000_000)
}

/// Every reason the migration can't start, in the order of spec §4.1. Empty when it
/// may. A source that isn't a repo gets that refusal alone.
pub fn refusals(facts: &PreflightFacts) -> Vec<Refusal> {
    if !facts.is_git_repo {
        return vec![Refusal::NotARepo];
    }
    let mut out = Vec::new();
    if !facts.on_windows_drive {
        out.push(Refusal::NotOnWindowsDrive);
    }
    if facts.is_linked_worktree {
        out.push(Refusal::LinkedWorktree);
    }
    if facts.has_linked_worktrees {
        out.push(Refusal::HasLinkedWorktrees);
    }
    if let Some(operation) = facts.operation {
        out.push(Refusal::OperationInProgress { operation });
    }
    if !facts.external_links.is_empty() {
        out.push(Refusal::ExternalLinks {
            paths: facts.external_links.clone(),
        });
    }
    if !facts.unreadable.is_empty() {
        out.push(Refusal::Unreadable {
            paths: facts.unreadable.clone(),
        });
    }
    if facts.uses_lfs && !facts.wsl_has_git_lfs {
        out.push(Refusal::LfsMissing);
    }
    if !facts.dest_on_linux {
        out.push(Refusal::DestNotLinux);
    }
    if facts.dest_exists {
        out.push(Refusal::DestExists);
    }
    let needed = space_needed(facts.copy_bytes);
    if facts.dest_on_linux && facts.free_bytes < needed {
        out.push(Refusal::NotEnoughSpace {
            needed,
            free: facts.free_bytes,
        });
    }
    out
}

/// The default destination: the first scan root on the Linux filesystem, joined with
/// the folder `name`; `<name>-2`, `<name>-3` … when `exists` says that is taken. Roots
/// are absolute, already expanded paths. `None` when no root is on Linux (or, absurdly,
/// the first thousand names are taken).
pub fn default_destination(
    roots: &[String],
    name: &str,
    exists: impl Fn(&str) -> bool,
) -> Option<String> {
    let root = roots
        .iter()
        .find(|root| root.starts_with('/') && !is_windows_mount(root))?;
    let root = root.trim_end_matches('/');
    (1..=1000)
        .map(|n| match n {
            1 => format!("{root}/{name}"),
            n => format!("{root}/{name}-{n}"),
        })
        .find(|path| !exists(path))
}

/// Warnings that the repo looks made for Windows tools, which will be slow or broken
/// on `\\wsl.localhost`: a `.sln` or `.vcxproj` file anywhere, or a Unity
/// `ProjectSettings/` folder at the root. `paths` are the source's relative paths
/// (`/`-separated, folders included). One warning per kind.
pub fn windows_tool_warnings<'a>(paths: impl IntoIterator<Item = &'a str>) -> Vec<String> {
    let (mut visual_studio, mut unity) = (false, false);
    for path in paths {
        let lower = path.to_ascii_lowercase();
        if lower.ends_with(".sln") || lower.ends_with(".vcxproj") {
            visual_studio = true;
        }
        if path == "ProjectSettings" || path.starts_with("ProjectSettings/") {
            unity = true;
        }
    }
    let mut warnings = Vec::new();
    if visual_studio {
        warnings.push(
            "This looks like a Visual Studio project. Visual Studio will be slow or broken on the WSL copy."
                .to_string(),
        );
    }
    if unity {
        warnings.push(
            "This looks like a Unity project. Unity will be slow or broken on the WSL copy."
                .to_string(),
        );
    }
    warnings
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ready() -> PreflightFacts {
        PreflightFacts {
            is_git_repo: true,
            on_windows_drive: true,
            dest_on_linux: true,
            copy_bytes: 2_000_000_000,
            free_bytes: 10_000_000_000,
            ..PreflightFacts::default()
        }
    }

    #[test]
    fn a_plain_repo_may_move() {
        assert_eq!(refusals(&ready()), vec![]);
    }

    #[test]
    fn each_fact_gives_its_refusal() {
        let cases: Vec<(PreflightFacts, Refusal)> = vec![
            (
                PreflightFacts {
                    on_windows_drive: false,
                    ..ready()
                },
                Refusal::NotOnWindowsDrive,
            ),
            (
                PreflightFacts {
                    is_linked_worktree: true,
                    ..ready()
                },
                Refusal::LinkedWorktree,
            ),
            (
                PreflightFacts {
                    has_linked_worktrees: true,
                    ..ready()
                },
                Refusal::HasLinkedWorktrees,
            ),
            (
                PreflightFacts {
                    operation: Some(GitOperation::Rebase),
                    ..ready()
                },
                Refusal::OperationInProgress {
                    operation: GitOperation::Rebase,
                },
            ),
            (
                PreflightFacts {
                    external_links: vec!["vendor/shared".into()],
                    ..ready()
                },
                Refusal::ExternalLinks {
                    paths: vec!["vendor/shared".into()],
                },
            ),
            (
                PreflightFacts {
                    unreadable: vec!["data/tide.db".into()],
                    ..ready()
                },
                Refusal::Unreadable {
                    paths: vec!["data/tide.db".into()],
                },
            ),
            (
                PreflightFacts {
                    uses_lfs: true,
                    ..ready()
                },
                Refusal::LfsMissing,
            ),
            (
                PreflightFacts {
                    dest_exists: true,
                    ..ready()
                },
                Refusal::DestExists,
            ),
        ];
        for (facts, refusal) in cases {
            assert_eq!(refusals(&facts), vec![refusal]);
        }
        let lfs_ok = PreflightFacts {
            uses_lfs: true,
            wsl_has_git_lfs: true,
            ..ready()
        };
        assert_eq!(refusals(&lfs_ok), vec![]);
        let not_repo = PreflightFacts {
            is_git_repo: false,
            on_windows_drive: false,
            ..ready()
        };
        assert_eq!(refusals(&not_repo), vec![Refusal::NotARepo]);
    }

    #[test]
    fn free_space_needs_a_fifth_more_plus_a_gigabyte() {
        assert_eq!(space_needed(0), 1_000_000_000);
        assert_eq!(space_needed(10_000_000_000), 13_000_000_000);
        assert_eq!(space_needed(u64::MAX), u64::MAX);
        let tight = PreflightFacts {
            copy_bytes: 10_000_000_000,
            free_bytes: 12_999_999_999,
            ..ready()
        };
        assert_eq!(
            refusals(&tight),
            vec![Refusal::NotEnoughSpace {
                needed: 13_000_000_000,
                free: 12_999_999_999
            }]
        );
        let enough = PreflightFacts {
            free_bytes: 13_000_000_000,
            ..tight
        };
        assert_eq!(refusals(&enough), vec![]);
        // A destination on Windows is refused for that, not for its space.
        let windows = PreflightFacts {
            dest_on_linux: false,
            free_bytes: 0,
            ..ready()
        };
        assert_eq!(refusals(&windows), vec![Refusal::DestNotLinux]);
    }

    #[test]
    fn messages_are_plain_sentences() {
        assert_eq!(
            Refusal::OperationInProgress {
                operation: GitOperation::CherryPick
            }
            .message(),
            "A cherry-pick is in progress. Finish or abort it first."
        );
        assert_eq!(
            Refusal::Unreadable {
                paths: vec!["a".into(), "b".into(), "c".into(), "d".into(), "e".into()]
            }
            .message(),
            "5 files can't be read: a, b, c and 2 more. Close the program using it and try again."
        );
        assert_eq!(
            Refusal::ExternalLinks {
                paths: vec!["vendor/shared".into()]
            }
            .message(),
            "1 link points outside the repo: vendor/shared."
        );
        assert_eq!(
            Refusal::NotEnoughSpace {
                needed: 13_000_000_000,
                free: 2_500_000_000
            }
            .message(),
            "Not enough free space: the move needs 13.0 GB and 2.5 GB is free."
        );
        assert!(Refusal::LfsMissing.message().contains(GIT_LFS_INSTALL));
        let value = serde_json::to_value(Refusal::OperationInProgress {
            operation: GitOperation::CherryPick,
        })
        .unwrap();
        assert_eq!(
            value,
            serde_json::json!({"kind": "operationInProgress", "operation": "cherryPick"})
        );
    }

    #[test]
    fn the_default_destination_skips_windows_roots_and_taken_names() {
        let roots = vec![
            "/mnt/c/Users/dev/Desktop".to_string(),
            "/home/dev/projects/".to_string(),
            "/home/dev/code".to_string(),
        ];
        let taken = [
            "/home/dev/projects/aurora-notes",
            "/home/dev/projects/aurora-notes-2",
        ];
        assert_eq!(
            default_destination(&roots, "aurora-notes", |p| taken.contains(&p)).as_deref(),
            Some("/home/dev/projects/aurora-notes-3")
        );
        assert_eq!(
            default_destination(&roots, "tidepool-api", |_| false).as_deref(),
            Some("/home/dev/projects/tidepool-api")
        );
        assert_eq!(
            default_destination(&roots[..1], "aurora-notes", |_| false),
            None
        );
    }

    #[test]
    fn windows_tool_projects_are_warned_about() {
        assert_eq!(
            windows_tool_warnings(["src/main.rs", "README.md", "Assets"]),
            Vec::<String>::new()
        );
        let vs = windows_tool_warnings(["tidepool.SLN", "native/engine.vcxproj"]);
        assert_eq!(vs.len(), 1);
        assert!(vs[0].contains("Visual Studio"));
        let unity = windows_tool_warnings([
            "Assets",
            "ProjectSettings",
            "ProjectSettings/ProjectVersion.txt",
        ]);
        assert_eq!(unity.len(), 1);
        assert!(unity[0].contains("Unity"));
        assert!(windows_tool_warnings(["docs/ProjectSettings"]).is_empty());
    }
}
