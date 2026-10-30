//! The migration report (spec §2.4): every check with its result, the skipped build
//! folders, the install result and the history that moved. The helper sends it in the
//! `migrateReady` event and saves it to
//! `~/.local/share/refract/migrations/<timestamp>-<name>.json`.

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use crate::migrate::build::{InstallStep, SkippedFolder};
use crate::migrate::manifest::{DroppedStream, ManifestDiff, PathProblem, Totals};

/// Version of the [`Report`] format.
pub const REPORT_VERSION: u32 = 1;

/// One thing wrong, with the path it concerns when there is one.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Problem {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub path: Option<String>,
    pub message: String,
}

impl From<&PathProblem> for Problem {
    fn from(problem: &PathProblem) -> Problem {
        Problem {
            path: Some(problem.path().to_owned()),
            message: problem.message(),
        }
    }
}

/// The checks of spec §5, in order.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum Check {
    /// 1. Every file matches.
    Files,
    /// 2. Git state is identical.
    GitState,
    /// 3. Nothing skipped without you knowing.
    NothingSkipped,
    /// 4. Nothing touched it during the copy.
    Unchanged,
    /// 5. History copied. Does not block deleting the original.
    History,
}

impl Check {
    /// The check's name as the report shows it.
    pub fn title(self) -> &'static str {
        match self {
            Check::Files => "Every file matches",
            Check::GitState => "Git state is identical",
            Check::NothingSkipped => "Nothing skipped without you knowing",
            Check::Unchanged => "Nothing touched it during the copy",
            Check::History => "History copied",
        }
    }

    /// What to do when the check fails.
    pub fn advice(self) -> &'static str {
        match self {
            Check::Files => "Discard the copy and retry. If it fails again, check the disk.",
            Check::GitState => {
                "Discard the copy and retry. If it fails again, run git fsck on the original."
            }
            Check::NothingSkipped => {
                "Close any program using these files, then discard the copy and retry."
            }
            Check::Unchanged => {
                "The folder keeps changing. Close whatever is writing to it, then retry."
            }
            Check::History => "The old transcripts are kept, so no history is lost.",
        }
    }

    /// True for the checks that must pass before the original may be deleted.
    pub fn blocks_delete(self) -> bool {
        self != Check::History
    }
}

/// How a check went.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum CheckStatus {
    Passed,
    Failed,
    /// Not reached, because an earlier step failed or the migration was cancelled.
    NotRun,
}

/// One check's result.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CheckResult {
    pub check: Check,
    pub status: CheckStatus,
    /// What failed, by path. Empty when it passed.
    #[serde(default)]
    pub problems: Vec<Problem>,
    /// What to do; set when it failed.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub advice: Option<String>,
}

impl CheckResult {
    /// Passed when `problems` is empty, otherwise failed with the check's advice.
    pub fn from_problems(check: Check, problems: Vec<Problem>) -> CheckResult {
        let failed = !problems.is_empty();
        CheckResult {
            check,
            status: if failed {
                CheckStatus::Failed
            } else {
                CheckStatus::Passed
            },
            problems,
            advice: failed.then(|| check.advice().to_owned()),
        }
    }

    pub fn not_run(check: Check) -> CheckResult {
        CheckResult {
            check,
            status: CheckStatus::NotRun,
            problems: Vec::new(),
            advice: None,
        }
    }
}

/// Checks 1 and 3 from a manifest diff and the copy's errors: missing and
/// unrepresentable paths and copy errors fail check 3, every other difference check 1.
pub fn manifest_checks(diff: &ManifestDiff, copy_errors: &[Problem]) -> [CheckResult; 2] {
    let (skipped, files): (Vec<&PathProblem>, Vec<&PathProblem>) =
        diff.problems.iter().partition(|p| {
            matches!(
                p,
                PathProblem::Missing { .. } | PathProblem::Unrepresentable { .. }
            )
        });
    let mut nothing_skipped: Vec<Problem> = skipped.into_iter().map(Problem::from).collect();
    nothing_skipped.extend(copy_errors.iter().cloned());
    [
        CheckResult::from_problems(Check::Files, files.into_iter().map(Problem::from).collect()),
        CheckResult::from_problems(Check::NothingSkipped, nothing_skipped),
    ]
}

/// How the install went.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum InstallStatus {
    Succeeded,
    Failed,
    /// Stopped after 15 minutes.
    TimedOut,
    /// The tool is not installed in WSL; the step's `installHint` says how to get it.
    ToolMissing,
}

/// The result of one install step. A failed install never fails the migration.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct InstallResult {
    pub step: InstallStep,
    pub status: InstallStatus,
    /// Exit code, when the command ran to an end.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub exit_code: Option<i32>,
    /// Linux path of the log, next to the saved report.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub log_path: Option<String>,
}

/// The session history that moved (spec §3.4).
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct HistoryMoved {
    /// Transcripts copied.
    pub sessions: u32,
    /// Project folders under `~/.claude/projects` copied from, and to (same order).
    pub from_dirs: Vec<String>,
    pub to_dirs: Vec<String>,
}

/// Everything the report shows.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Report {
    /// [`REPORT_VERSION`].
    pub version: u32,
    /// The migration's id.
    pub id: String,
    /// Linux path of the original (`/mnt/...`).
    pub source: String,
    /// Linux path of the copy.
    pub dest: String,
    pub started_at: DateTime<Utc>,
    pub finished_at: DateTime<Utc>,
    /// The five checks, in order.
    pub checks: Vec<CheckResult>,
    pub skipped: Vec<SkippedFolder>,
    pub dropped_streams: Vec<DroppedStream>,
    /// Loose objects normalising added to the copy.
    #[serde(default)]
    pub new_objects: u64,
    pub source_totals: Totals,
    pub copy_totals: Totals,
    /// Empty when nothing needed installing, or the checks failed first.
    #[serde(default)]
    pub install: Vec<InstallResult>,
    #[serde(default)]
    pub history: HistoryMoved,
    /// Linux git read the source because `git.exe` was missing.
    #[serde(default)]
    pub linux_git: bool,
    /// Dialog warnings carried over, such as a Windows-tool project.
    #[serde(default)]
    pub warnings: Vec<String>,
}

impl Report {
    /// True when every check that guards the original passed, so **Delete original**
    /// may be offered.
    pub fn can_delete(&self) -> bool {
        [
            Check::Files,
            Check::GitState,
            Check::NothingSkipped,
            Check::Unchanged,
        ]
        .iter()
        .all(|check| self.passed(*check))
    }

    /// True when the history check passed, so the old transcripts may be deleted.
    pub fn history_ok(&self) -> bool {
        self.passed(Check::History)
    }

    fn passed(&self, check: Check) -> bool {
        self.checks
            .iter()
            .any(|c| c.check == check && c.status == CheckStatus::Passed)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::migrate::manifest::{EntryKind, RelPath};
    use serde_json::json;

    fn p(path: &str) -> RelPath {
        RelPath::new(path).unwrap()
    }

    fn report(checks: Vec<CheckResult>) -> Report {
        Report {
            version: REPORT_VERSION,
            id: "m-1".into(),
            source: "/mnt/c/Users/dev/Desktop/aurora-notes".into(),
            dest: "/home/dev/projects/aurora-notes".into(),
            started_at: "2026-10-30T10:00:00Z".parse().unwrap(),
            finished_at: "2026-10-30T10:05:00Z".parse().unwrap(),
            checks,
            skipped: vec![],
            dropped_streams: vec![],
            new_objects: 3,
            source_totals: Totals::default(),
            copy_totals: Totals::default(),
            install: vec![],
            history: HistoryMoved::default(),
            linux_git: false,
            warnings: vec![],
        }
    }

    fn all(status: CheckStatus) -> Vec<CheckResult> {
        [
            Check::Files,
            Check::GitState,
            Check::NothingSkipped,
            Check::Unchanged,
            Check::History,
        ]
        .into_iter()
        .map(|check| CheckResult {
            check,
            status,
            problems: vec![],
            advice: None,
        })
        .collect()
    }

    #[test]
    fn delete_needs_checks_one_to_four() {
        assert!(report(all(CheckStatus::Passed)).can_delete());
        assert!(!report(all(CheckStatus::NotRun)).can_delete());
        assert!(!report(vec![]).can_delete());
        let mut history_failed = all(CheckStatus::Passed);
        history_failed[4].status = CheckStatus::Failed;
        let r = report(history_failed);
        assert!(r.can_delete());
        assert!(!r.history_ok());
        let mut git_failed = all(CheckStatus::Passed);
        git_failed[1].status = CheckStatus::Failed;
        assert!(!report(git_failed).can_delete());
        assert!(!Check::History.blocks_delete());
    }

    #[test]
    fn manifest_problems_split_between_checks_one_and_three() {
        let diff = ManifestDiff {
            problems: vec![
                PathProblem::ContentDiffers { path: p(".env") },
                PathProblem::Missing {
                    path: p("data/tide.csv"),
                },
                PathProblem::Unrepresentable {
                    path: "docs/\u{fffd}.md".into(),
                },
                PathProblem::KindDiffers {
                    path: p("notes"),
                    source: EntryKind::Dir,
                    copy: EntryKind::File,
                },
            ],
            ..ManifestDiff::default()
        };
        let copy_error = Problem {
            path: Some("big.bin".into()),
            message: "Copying failed: disk full.".into(),
        };
        let [files, skipped] = manifest_checks(&diff, std::slice::from_ref(&copy_error));
        assert_eq!(files.status, CheckStatus::Failed);
        assert_eq!(files.problems.len(), 2);
        assert_eq!(files.advice.as_deref(), Some(Check::Files.advice()));
        assert_eq!(skipped.problems.len(), 3);
        assert_eq!(skipped.problems[2], copy_error);
        assert_eq!(skipped.problems[0].path.as_deref(), Some("data/tide.csv"));

        let [files, skipped] = manifest_checks(&ManifestDiff::default(), &[]);
        assert_eq!(files.status, CheckStatus::Passed);
        assert_eq!(skipped.advice, None);
    }

    #[test]
    fn reports_round_trip_in_camel_case() {
        let mut checks = all(CheckStatus::Passed);
        checks[3] = CheckResult::from_problems(
            Check::Unchanged,
            vec![Problem {
                path: Some("src/main.rs".into()),
                message: "Changed during the copy.".into(),
            }],
        );
        let r = report(checks);
        let value = serde_json::to_value(&r).unwrap();
        assert_eq!(value["checks"][3]["check"], "unchanged");
        assert_eq!(value["checks"][3]["status"], "failed");
        assert_eq!(value["checks"][3]["problems"][0]["path"], "src/main.rs");
        assert_eq!(
            value["checks"][0],
            json!({"check": "files", "status": "passed", "problems": []})
        );
        assert_eq!(value["startedAt"], "2026-10-30T10:00:00Z");
        assert_eq!(value["newObjects"], 3);
        let back: Report = serde_json::from_value(value).unwrap();
        assert_eq!(back, r);
    }
}
