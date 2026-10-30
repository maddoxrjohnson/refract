//! Moving a repo from a Windows drive onto the WSL Linux filesystem ("Move to WSL",
//! `.plans/wsl-migration-spec.md`): the rules and comparisons, with no I/O.
//!
//! The work is split between the Windows app and the WSL helper:
//!
//! 1. **Preflight.** Both sides gather facts ([`preflight::PreflightFacts`]);
//!    [`preflight::refusals`] says whether the migration may start, and
//!    [`preflight::default_destination`], [`build::skip_rule`],
//!    [`build::install_plan`] and [`preflight::windows_tool_warnings`] fill the dialog.
//! 2. **Snapshot.** The app reads the source natively with `git.exe` and builds a
//!    [`SourceSnapshot`]: the manifest S0 ([`manifest::Manifest`]) and the git snapshot
//!    G0 ([`gitstate::GitSnapshot`], one per repo, submodules included).
//! 3. **Copy, normalise, verify.** The helper copies, rewrites tracked files as Linux git
//!    checks them out, applies [`gitconfig::config_changes`], builds the copy's manifest
//!    and git snapshot the same way, and compares them with [`manifest::diff`] and
//!    [`gitstate::compare`]. The source is re-statted and compared with
//!    [`manifest::compare_stats`] until it holds still.
//! 4. **History.** Transcripts are copied with [`history::rewrite_transcript`] and
//!    checked with [`history::verify_transcript`].
//! 5. **Report.** Everything ends up in a [`report::Report`], sent in the `migrateReady`
//!    event and saved next to the install log.
//!
//! # How S0 and G0 reach the helper
//!
//! The app writes the [`SourceSnapshot`] as JSON (field names camelCase) to a file and
//! passes that file's Linux path as the `snapshot` param of `migrate` (normally under
//! `/mnt/c/...`, in the app's local data folder). It writes the file completely, under a
//! temporary name renamed into place, *before* it sends `migrate`: the helper reads it
//! once, at the start, because it needs the skipped folders before it copies and the
//! manifest before it verifies. The helper never writes to it; the app deletes it after
//! `migrateFinish` or `migrateCancel`. [`SOURCE_SNAPSHOT_VERSION`] guards the format.

pub mod build;
pub mod gitconfig;
pub mod gitstate;
pub mod history;
pub mod manifest;
pub mod preflight;
pub mod report;

use serde::{Deserialize, Serialize};

use crate::migrate::gitstate::GitSnapshot;
use crate::migrate::manifest::Manifest;

/// Version of the [`SourceSnapshot`] file format.
pub const SOURCE_SNAPSHOT_VERSION: u32 = 1;

/// What the app records about the source before the copy (spec §4.2), handed to the
/// helper as a file (see the module docs).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SourceSnapshot {
    /// [`SOURCE_SNAPSHOT_VERSION`].
    pub version: u32,
    /// S0: every file and folder outside the skipped folders, with the skipped folders
    /// and dropped alternate data streams listed.
    pub manifest: Manifest,
    /// G0: the repo first (path `""`), then each submodule by its working-tree path
    /// relative to the repo, nested ones included.
    pub git: Vec<RepoGit>,
    /// True when `git.exe` was missing and Linux git read the `/mnt` path instead, with
    /// `-c core.autocrlf=true -c core.filemode=false`. The report says so.
    #[serde(default)]
    pub linux_git: bool,
}

/// The git snapshot of one repo or submodule.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RepoGit {
    /// Working-tree path relative to the top repo, `/`-separated; `""` for the top repo.
    pub path: String,
    pub snapshot: GitSnapshot,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::migrate::manifest::{Entry, EntryKind, RelPath};
    use serde_json::json;

    #[test]
    fn source_snapshot_round_trips_in_camel_case() {
        let snapshot = SourceSnapshot {
            version: SOURCE_SNAPSHOT_VERSION,
            manifest: Manifest {
                entries: vec![Entry {
                    path: RelPath::new("README.md").unwrap(),
                    kind: EntryKind::File,
                    size: 12,
                    mtime_ns: 1_700_000_000_000_000_000,
                    attributes: 0x20,
                    sha256: Some("ab".repeat(32)),
                    blob: Some("cd".repeat(20)),
                    link_target: None,
                }],
                ..Manifest::default()
            },
            git: vec![RepoGit {
                path: String::new(),
                snapshot: GitSnapshot::default(),
            }],
            linux_git: false,
        };
        let value = serde_json::to_value(&snapshot).unwrap();
        assert_eq!(value["linuxGit"], json!(false));
        assert_eq!(value["manifest"]["entries"][0]["path"], "README.md");
        assert_eq!(
            value["manifest"]["entries"][0]["mtimeNs"],
            1_700_000_000_000_000_000_i64
        );
        let back: SourceSnapshot = serde_json::from_value(value).unwrap();
        assert_eq!(back, snapshot);
    }
}
