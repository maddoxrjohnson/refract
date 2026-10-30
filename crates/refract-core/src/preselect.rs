//! Which option the launch picker preselects (spec §4.3).
//!
//! A live session whose process runs in a terminal of the Refract window on exactly
//! this repo's folder is "focusable": the picker
//! offers to go to it. While one exists the picker always shows, so Enter never starts
//! a second agent beside it by accident; when the newest one is waiting on the person,
//! going to it is preselected.

use std::path::{Component, Path, PathBuf};

use chrono::{DateTime, TimeDelta, Utc};
use serde::{Deserialize, Serialize};

use crate::classify::{LiveState, SessionState};
use crate::sessions::{path_within, paths_match};
use crate::transcript::SessionSummary;

/// Sessions that ended this long ago or longer are not preselected for resume.
pub const MAX_RESUME_AGE: TimeDelta = TimeDelta::days(2);

/// Transcripts this large or larger are not preselected for resume, because resuming
/// reloads the whole conversation. 2 MB is a guess, to be tuned.
pub const MAX_RESUME_BYTES: u64 = 2 * 1024 * 1024;

/// A session the picker could offer.
#[derive(Debug, Clone, Copy)]
pub struct Candidate<'a> {
    pub summary: &'a SessionSummary,
    pub state: SessionState,
    /// Size of the transcript file in bytes.
    pub byte_size: u64,
    /// Its process is known and runs in the Refract window on this repo, so a live
    /// session can be gone to.
    pub focusable: bool,
}

impl Candidate<'_> {
    fn live_and_focusable(&self) -> bool {
        self.focusable && matches!(self.state, SessionState::Live(_))
    }
}

/// The launch decision. On the wire: `{"kind": "resume", "sessionId": "..."}`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", content = "sessionId", rename_all = "camelCase")]
pub enum Preselect {
    /// Clean repo, nothing unfinished and nothing to go to: launch a new
    /// agent without a picker.
    NoPicker,
    /// Show the picker with this session preselected.
    Resume(String),
    /// Show the picker with New agent preselected.
    NewAgent,
    /// Show the picker with "Go to the running agent" preselected for this live session.
    Focus(String),
}

/// Chooses the preselected picker option.
///
/// `dirty` holds the absolute paths of the repo's uncommitted files; an entry ending in
/// `/` is a directory git reports as a whole, and any edit inside it counts. Edited
/// paths in a transcript are absolute already; relative ones are resolved against the
/// session's cwd. Paths under `/mnt/` compare ignoring ASCII case.
///
/// The rules, in order: when the newest focusable live session waits on the person, go
/// to it; when the newest unfinished session is recent, small and touched a dirty file,
/// resume it; with no unfinished session, a clean repo and no focusable live session,
/// skip the picker; otherwise preselect New agent.
pub fn preselect(candidates: &[Candidate<'_>], dirty: &[String], now: DateTime<Utc>) -> Preselect {
    let focusable = candidates
        .iter()
        .filter(|c| c.live_and_focusable())
        .max_by_key(|c| c.summary.last_timestamp);
    if let Some(running) = focusable {
        if running.state == SessionState::Live(LiveState::WaitingOnUser) {
            if let Some(id) = &running.summary.session_id {
                return Preselect::Focus(id.clone());
            }
        }
    }
    let latest = candidates
        .iter()
        .filter(|c| c.state.is_unfinished())
        .max_by_key(|c| c.summary.last_timestamp);
    let Some(latest) = latest else {
        return if dirty.is_empty() && focusable.is_none() {
            Preselect::NoPicker
        } else {
            Preselect::NewAgent
        };
    };

    let recent = latest
        .summary
        .last_timestamp
        .is_some_and(|ended| now - ended < MAX_RESUME_AGE);
    let small = latest.byte_size < MAX_RESUME_BYTES;
    if recent && small && touched_dirty_file(latest.summary, dirty) {
        if let Some(id) = &latest.summary.session_id {
            return Preselect::Resume(id.clone());
        }
    }
    Preselect::NewAgent
}

fn touched_dirty_file(summary: &SessionSummary, dirty: &[String]) -> bool {
    let dirty: Vec<(String, bool)> = dirty
        .iter()
        .map(|d| (path_string(&normalize(Path::new(d))), d.ends_with('/')))
        .collect();
    summary.files_edited.iter().any(|edited| {
        let edited = Path::new(edited);
        let edited = match &summary.cwd {
            Some(cwd) if edited.is_relative() => normalize(&Path::new(cwd).join(edited)),
            _ => normalize(edited),
        };
        let edited = path_string(&edited);
        dirty.iter().any(|(d, is_dir)| {
            if *is_dir {
                path_within(&edited, d)
            } else {
                paths_match(&edited, d)
            }
        })
    })
}

fn path_string(path: &Path) -> String {
    path.to_string_lossy().into_owned()
}

/// Removes `.` and resolves `..` lexically, without touching the file system.
fn normalize(path: &Path) -> PathBuf {
    let mut out = PathBuf::new();
    for part in path.components() {
        match part {
            Component::CurDir => {}
            Component::ParentDir => {
                out.pop();
            }
            other => out.push(other),
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::classify::{FinishKind, InterruptReason};

    const REPO: &str = "/home/dev/projects/tidepool-api";

    fn now() -> DateTime<Utc> {
        "2026-03-14T12:00:00Z".parse().unwrap()
    }

    fn summary(id: &str, ended: DateTime<Utc>, edited: &[&str]) -> SessionSummary {
        SessionSummary {
            session_id: Some(id.to_owned()),
            cwd: Some(REPO.to_owned()),
            last_timestamp: Some(ended),
            files_edited: edited.iter().map(|p| format!("{REPO}/{p}")).collect(),
            ..SessionSummary::default()
        }
    }

    fn interrupted(summary: &SessionSummary, byte_size: u64) -> Candidate<'_> {
        Candidate {
            summary,
            state: SessionState::Interrupted(InterruptReason::ToolCallWithoutResult),
            byte_size,
            focusable: false,
        }
    }

    fn running(summary: &SessionSummary, live: LiveState, focusable: bool) -> Candidate<'_> {
        Candidate {
            summary,
            state: SessionState::Live(live),
            byte_size: 1000,
            focusable,
        }
    }

    fn dirty(paths: &[&str]) -> Vec<String> {
        paths.iter().map(|p| format!("{REPO}/{p}")).collect()
    }

    #[test]
    fn clean_repo_without_unfinished_sessions_skips_picker() {
        let s = summary("a", now(), &["src/lib.rs"]);
        let finished = Candidate {
            summary: &s,
            state: SessionState::Finished(FinishKind::Completed),
            byte_size: 10,
            focusable: false,
        };
        assert_eq!(preselect(&[finished], &[], now()), Preselect::NoPicker);
        assert_eq!(preselect(&[], &[], now()), Preselect::NoPicker);
    }

    #[test]
    fn dirty_repo_without_unfinished_sessions_offers_new_agent() {
        assert_eq!(
            preselect(&[], &dirty(&["src/lib.rs"]), now()),
            Preselect::NewAgent
        );
    }

    #[test]
    fn live_sessions_are_not_resume_candidates() {
        let s = summary("a", now(), &["src/lib.rs"]);
        let live = running(&s, LiveState::Working, false);
        assert_eq!(preselect(&[live], &[], now()), Preselect::NoPicker);
    }

    #[test]
    fn focusable_live_session_always_shows_the_picker() {
        let s = summary("a", now(), &["src/lib.rs"]);
        for live in [LiveState::Working, LiveState::Idle, LiveState::Unknown] {
            assert_eq!(
                preselect(&[running(&s, live, true)], &[], now()),
                Preselect::NewAgent,
                "{live:?}"
            );
        }
    }

    #[test]
    fn live_session_outside_vscode_does_not_stop_the_shortcut() {
        let s = summary("a", now(), &["src/lib.rs"]);
        let got = preselect(&[running(&s, LiveState::WaitingOnUser, false)], &[], now());
        assert_eq!(got, Preselect::NoPicker);
    }

    #[test]
    fn focusable_session_waiting_on_you_is_preselected() {
        let s = summary("a", now(), &["src/lib.rs"]);
        let got = preselect(&[running(&s, LiveState::WaitingOnUser, true)], &[], now());
        assert_eq!(got, Preselect::Focus("a".to_owned()));
    }

    #[test]
    fn going_to_a_waiting_agent_comes_before_resume() {
        let open = summary("open", now() - TimeDelta::hours(1), &["src/lib.rs"]);
        let waiting = summary("waiting", now() - TimeDelta::hours(2), &[]);
        let candidates = [
            interrupted(&open, 1000),
            running(&waiting, LiveState::WaitingOnUser, true),
        ];
        let got = preselect(&candidates, &dirty(&["src/lib.rs"]), now());
        assert_eq!(got, Preselect::Focus("waiting".to_owned()));
    }

    #[test]
    fn only_the_newest_focusable_session_is_considered() {
        let older = summary("older", now() - TimeDelta::hours(2), &[]);
        let newer = summary("newer", now() - TimeDelta::hours(1), &[]);
        let candidates = [
            running(&older, LiveState::WaitingOnUser, true),
            running(&newer, LiveState::Working, true),
        ];
        assert_eq!(preselect(&candidates, &[], now()), Preselect::NewAgent);
        // A newer session outside VS Code does not hide the one inside.
        let candidates = [
            running(&older, LiveState::WaitingOnUser, true),
            running(&newer, LiveState::WaitingOnUser, false),
        ];
        assert_eq!(
            preselect(&candidates, &[], now()),
            Preselect::Focus("older".to_owned())
        );
    }

    #[test]
    fn working_focusable_session_leaves_resume_alone() {
        let open = summary("open", now() - TimeDelta::hours(1), &["src/lib.rs"]);
        let busy = summary("busy", now(), &[]);
        let candidates = [
            interrupted(&open, 1000),
            running(&busy, LiveState::Working, true),
        ];
        let got = preselect(&candidates, &dirty(&["src/lib.rs"]), now());
        assert_eq!(got, Preselect::Resume("open".to_owned()));
    }

    #[test]
    fn finished_session_marked_focusable_is_ignored() {
        let s = summary("a", now(), &["src/lib.rs"]);
        let finished = Candidate {
            summary: &s,
            state: SessionState::Finished(FinishKind::Completed),
            byte_size: 10,
            focusable: true,
        };
        assert_eq!(preselect(&[finished], &[], now()), Preselect::NoPicker);
    }

    #[test]
    fn focus_goes_on_the_wire_with_its_session_id() {
        assert_eq!(
            serde_json::to_value(Preselect::Focus("a".into())).unwrap(),
            serde_json::json!({"kind": "focus", "sessionId": "a"})
        );
    }

    #[test]
    fn resumes_recent_small_session_that_touched_dirty_files() {
        let s = summary("a", now() - TimeDelta::hours(3), &["src/lib.rs"]);
        let got = preselect(&[interrupted(&s, 1000)], &dirty(&["src/lib.rs"]), now());
        assert_eq!(got, Preselect::Resume("a".to_owned()));
    }

    #[test]
    fn unfinished_session_with_clean_repo_shows_picker_with_new_agent() {
        let s = summary("a", now() - TimeDelta::hours(3), &["src/lib.rs"]);
        assert_eq!(
            preselect(&[interrupted(&s, 1000)], &[], now()),
            Preselect::NewAgent
        );
    }

    #[test]
    fn age_boundary_is_exclusive() {
        let d = dirty(&["src/lib.rs"]);
        let just_inside = summary(
            "a",
            now() - MAX_RESUME_AGE + TimeDelta::seconds(1),
            &["src/lib.rs"],
        );
        assert_eq!(
            preselect(&[interrupted(&just_inside, 1000)], &d, now()),
            Preselect::Resume("a".to_owned())
        );
        let at_limit = summary("a", now() - MAX_RESUME_AGE, &["src/lib.rs"]);
        assert_eq!(
            preselect(&[interrupted(&at_limit, 1000)], &d, now()),
            Preselect::NewAgent
        );
    }

    #[test]
    fn size_boundary_is_exclusive() {
        let d = dirty(&["src/lib.rs"]);
        let s = summary("a", now() - TimeDelta::hours(1), &["src/lib.rs"]);
        assert_eq!(
            preselect(&[interrupted(&s, MAX_RESUME_BYTES - 1)], &d, now()),
            Preselect::Resume("a".to_owned())
        );
        assert_eq!(
            preselect(&[interrupted(&s, MAX_RESUME_BYTES)], &d, now()),
            Preselect::NewAgent
        );
    }

    #[test]
    fn no_overlap_with_dirty_files_offers_new_agent() {
        let s = summary("a", now() - TimeDelta::hours(1), &["src/lib.rs"]);
        let got = preselect(&[interrupted(&s, 1000)], &dirty(&["README.md"]), now());
        assert_eq!(got, Preselect::NewAgent);
    }

    #[test]
    fn same_relative_path_in_another_repo_does_not_match() {
        let mut s = summary("a", now() - TimeDelta::hours(1), &[]);
        s.files_edited = vec!["/home/dev/projects/aurora-notes/src/lib.rs".to_owned()];
        let got = preselect(&[interrupted(&s, 1000)], &dirty(&["src/lib.rs"]), now());
        assert_eq!(got, Preselect::NewAgent);
    }

    #[test]
    fn relative_edited_paths_resolve_against_cwd() {
        let mut s = summary("a", now() - TimeDelta::hours(1), &[]);
        s.files_edited = vec!["./src/../src/lib.rs".to_owned()];
        let got = preselect(&[interrupted(&s, 1000)], &dirty(&["src/lib.rs"]), now());
        assert_eq!(got, Preselect::Resume("a".to_owned()));
    }

    #[test]
    fn picks_most_recent_unfinished_session() {
        let older = summary("older", now() - TimeDelta::hours(5), &["src/lib.rs"]);
        let newer = summary("newer", now() - TimeDelta::hours(1), &["src/lib.rs"]);
        let newest_finished = summary("done", now(), &["src/lib.rs"]);
        let candidates = [
            interrupted(&older, 1000),
            Candidate {
                summary: &newest_finished,
                state: SessionState::Finished(FinishKind::Completed),
                byte_size: 1000,
                focusable: false,
            },
            interrupted(&newer, 1000),
        ];
        let got = preselect(&candidates, &dirty(&["src/lib.rs"]), now());
        assert_eq!(got, Preselect::Resume("newer".to_owned()));
    }

    #[test]
    fn most_recent_failing_a_rule_does_not_fall_back_to_older() {
        let older = summary("older", now() - TimeDelta::hours(5), &["src/lib.rs"]);
        let newer = summary("newer", now() - TimeDelta::hours(1), &["src/lib.rs"]);
        let candidates = [
            interrupted(&older, 1000),
            interrupted(&newer, MAX_RESUME_BYTES),
        ];
        let got = preselect(&candidates, &dirty(&["src/lib.rs"]), now());
        assert_eq!(got, Preselect::NewAgent);
    }

    #[test]
    fn edits_inside_an_untracked_directory_match_it() {
        let s = summary("a", now() - TimeDelta::hours(1), &["src/tide/new.rs"]);
        let got = preselect(&[interrupted(&s, 1000)], &dirty(&["src/tide/"]), now());
        assert_eq!(got, Preselect::Resume("a".to_owned()));
        // Without the slash the entry is a file, and a sibling name is no match.
        let got = preselect(&[interrupted(&s, 1000)], &dirty(&["src/tide"]), now());
        assert_eq!(got, Preselect::NewAgent);
    }

    #[test]
    fn windows_mount_paths_match_ignoring_case() {
        let mut s = summary("a", now() - TimeDelta::hours(1), &[]);
        s.cwd = Some("/mnt/c/Users/dev/tidepool-api".to_owned());
        s.files_edited = vec!["/mnt/c/users/dev/tidepool-api/src/lib.rs".to_owned()];
        let dirty = ["/mnt/c/Users/dev/tidepool-api/src/lib.rs".to_owned()];
        let got = preselect(&[interrupted(&s, 1000)], &dirty, now());
        assert_eq!(got, Preselect::Resume("a".to_owned()));
        let dirty_dir = ["/mnt/c/Users/dev/tidepool-api/SRC/".to_owned()];
        let got = preselect(&[interrupted(&s, 1000)], &dirty_dir, now());
        assert_eq!(got, Preselect::Resume("a".to_owned()));
    }

    #[test]
    fn session_without_timestamp_is_not_resumed() {
        let mut s = summary("a", now(), &["src/lib.rs"]);
        s.last_timestamp = None;
        let got = preselect(&[interrupted(&s, 1000)], &dirty(&["src/lib.rs"]), now());
        assert_eq!(got, Preselect::NewAgent);
    }
}
