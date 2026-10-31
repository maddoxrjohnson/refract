//! Which sessions are worth resuming, and which option the launch picker preselects
//! (spec §4.3).
//!
//! A session that isn't running is unfinished, with a [`ResumeReason`], when it was
//! interrupted, or when it is the newest session that isn't running and it asked the
//! person a question, or was active after the last commit while the repo has
//! uncommitted changes or unpushed commits. Enter resumes that newest session when it
//! is unfinished and recent; its size doesn't matter.
//!
//! A live session whose process runs in a terminal of the Refract window on exactly
//! this repo's folder is "focusable": the picker
//! offers to go to it. While one exists the picker always shows, so Enter never starts
//! a second agent beside it by accident; when the newest one is waiting on the person,
//! going to it is preselected.

use chrono::{DateTime, TimeDelta, Utc};
use serde::{Deserialize, Serialize};

use crate::classify::{LiveState, SessionState};
use crate::transcript::SessionSummary;

/// Sessions that ended this long ago or longer are not preselected for resume.
pub const MAX_RESUME_AGE: TimeDelta = TimeDelta::days(2);

/// A session the picker could offer.
#[derive(Debug, Clone, Copy)]
pub struct Candidate<'a> {
    pub summary: &'a SessionSummary,
    pub state: SessionState,
    /// Its process is known and runs in the Refract window on this repo, so a live
    /// session can be gone to.
    pub focusable: bool,
}

impl Candidate<'_> {
    fn live_and_focusable(&self) -> bool {
        self.focusable && matches!(self.state, SessionState::Live(_))
    }

    /// The transcript has entries but no conversation in them. A transcript whose last
    /// line was too long to read (no entries) is not empty.
    fn is_empty(&self) -> bool {
        self.summary.tail.is_none() && self.summary.entry_count > 0
    }

    /// The session's last activity is after `head_time`, a commit's Unix seconds.
    fn active_after(&self, head_time: i64) -> bool {
        self.summary
            .last_timestamp
            .is_some_and(|last| last.timestamp_millis() > head_time * 1000)
    }
}

/// Why a session that isn't running is worth resuming. When more than one applies, the
/// first listed wins. On the wire: `"interrupted"`, `"askedQuestion"`, ...
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum ResumeReason {
    /// It stopped mid-task (any session).
    Interrupted,
    /// Its last turn ended in a reply that asks the person something.
    AskedQuestion,
    /// The repo has uncommitted changes and the session was active after the last
    /// commit, or the repo has no commits.
    UncommittedChanges,
    /// The branch has commits to push and the session was active after the last commit.
    UnpushedCommits,
}

/// Where the current branch stands against its upstream.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Upstream {
    /// No upstream is set; HEAD has this many commits that no `origin/*` branch has.
    Missing(u32),
    /// An upstream is set but the branch is gone from the remote.
    Gone,
    /// Tracks an upstream and is this many commits ahead of it.
    Ahead(u32),
}

/// What the rule needs to know about the repo.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RepoFacts {
    /// The repo has uncommitted changes.
    pub dirty: bool,
    /// Committer time of HEAD in Unix seconds; `None` before the first commit.
    pub head_time: Option<i64>,
    /// The repo has an `origin` remote.
    pub has_origin: bool,
    /// HEAD is on a branch, not detached.
    pub on_branch: bool,
    pub upstream: Upstream,
}

impl RepoFacts {
    /// The branch has commits to push: it is ahead of its upstream (on any remote), or
    /// has no upstream but an origin and commits no `origin/*` branch has. A gone
    /// upstream and a detached HEAD don't count.
    fn unpushed(&self) -> bool {
        self.on_branch
            && match self.upstream {
                Upstream::Missing(n) => self.has_origin && n > 0,
                Upstream::Gone => false,
                Upstream::Ahead(n) => n > 0,
            }
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

/// Each candidate's reason to resume it, in order; `None` for running and finished
/// sessions. Only an interrupt counts for any session; the other reasons count only for
/// the repo's last conversation, and only when it isn't running: a running agent's own
/// edits aren't another session's unfinished work. The age of a session doesn't matter
/// here.
pub fn resume_reasons(candidates: &[Candidate<'_>], repo: &RepoFacts) -> Vec<Option<ResumeReason>> {
    let last = newest(candidates, |c| !c.is_empty())
        .filter(|&i| !matches!(candidates[i].state, SessionState::Live(_)));
    candidates
        .iter()
        .enumerate()
        .map(|(i, c)| {
            if matches!(c.state, SessionState::Interrupted(_)) {
                return Some(ResumeReason::Interrupted);
            }
            if last != Some(i) {
                return None;
            }
            let after_commit = repo.head_time.is_some_and(|t| c.active_after(t));
            if c.summary.asks_user {
                Some(ResumeReason::AskedQuestion)
            } else if repo.dirty && (repo.head_time.is_none() || after_commit) {
                Some(ResumeReason::UncommittedChanges)
            } else if repo.unpushed() && after_commit {
                Some(ResumeReason::UnpushedCommits)
            } else {
                None
            }
        })
        .collect()
}

/// Index of the newest candidate that `keep` accepts.
fn newest(candidates: &[Candidate<'_>], keep: impl Fn(&Candidate<'_>) -> bool) -> Option<usize> {
    candidates
        .iter()
        .enumerate()
        .filter(|(_, c)| keep(c))
        .max_by_key(|(_, c)| c.summary.last_timestamp)
        .map(|(i, _)| i)
}

/// Index of the newest candidate with a conversation that isn't running.
fn newest_stopped(candidates: &[Candidate<'_>]) -> Option<usize> {
    newest(candidates, |c| {
        !c.is_empty() && !matches!(c.state, SessionState::Live(_))
    })
}

/// Chooses the preselected picker option.
///
/// The rules, in order: when the newest focusable live session waits on the person, go
/// to it; when the newest session that isn't running has a [`ResumeReason`] and ended
/// less than [`MAX_RESUME_AGE`] ago, resume it; with no session to resume, a clean repo
/// and no focusable live session, skip the picker; otherwise preselect New agent.
pub fn preselect(candidates: &[Candidate<'_>], repo: &RepoFacts, now: DateTime<Utc>) -> Preselect {
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
    let reasons = resume_reasons(candidates, repo);
    if let Some(i) = newest_stopped(candidates) {
        let newest = candidates[i].summary;
        let recent = newest
            .last_timestamp
            .is_some_and(|ended| now - ended < MAX_RESUME_AGE);
        if reasons[i].is_some() && recent {
            if let Some(id) = &newest.session_id {
                return Preselect::Resume(id.clone());
            }
        }
    }
    if reasons.iter().all(Option::is_none) && !repo.dirty && focusable.is_none() {
        Preselect::NoPicker
    } else {
        Preselect::NewAgent
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::classify::{FinishKind, InterruptReason};

    fn now() -> DateTime<Utc> {
        "2026-03-14T12:00:00Z".parse().unwrap()
    }

    /// HEAD's committer time: four hours before [`now`].
    fn commit_time() -> i64 {
        (now() - TimeDelta::hours(4)).timestamp()
    }

    fn summary(id: &str, ended: DateTime<Utc>) -> SessionSummary {
        SessionSummary {
            session_id: Some(id.to_owned()),
            last_timestamp: Some(ended),
            ..SessionSummary::default()
        }
    }

    /// A session that ended `hours` before [`now`]: after the commit when under 4.
    fn ended(id: &str, hours: i64) -> SessionSummary {
        summary(id, now() - TimeDelta::hours(hours))
    }

    fn asking(id: &str, hours: i64) -> SessionSummary {
        SessionSummary {
            asks_user: true,
            ..ended(id, hours)
        }
    }

    fn interrupted(summary: &SessionSummary) -> Candidate<'_> {
        Candidate {
            summary,
            state: SessionState::Interrupted(InterruptReason::ToolCallWithoutResult),
            focusable: false,
        }
    }

    fn finished(summary: &SessionSummary) -> Candidate<'_> {
        Candidate {
            summary,
            state: SessionState::Finished(FinishKind::Completed),
            focusable: false,
        }
    }

    fn running(summary: &SessionSummary, live: LiveState, focusable: bool) -> Candidate<'_> {
        Candidate {
            summary,
            state: SessionState::Live(live),
            focusable,
        }
    }

    /// A clean, pushed repo on a branch with an origin.
    fn pushed() -> RepoFacts {
        RepoFacts {
            dirty: false,
            head_time: Some(commit_time()),
            has_origin: true,
            on_branch: true,
            upstream: Upstream::Ahead(0),
        }
    }

    fn dirty() -> RepoFacts {
        RepoFacts {
            dirty: true,
            ..pushed()
        }
    }

    fn ahead(n: u32) -> RepoFacts {
        RepoFacts {
            upstream: Upstream::Ahead(n),
            ..pushed()
        }
    }

    fn resume(id: &str) -> Preselect {
        Preselect::Resume(id.to_owned())
    }

    fn reason(candidate: Candidate<'_>, repo: &RepoFacts) -> Option<ResumeReason> {
        resume_reasons(&[candidate], repo)[0]
    }

    // The case table in spec §4.3, row by row.

    #[test]
    fn clean_pushed_and_finished_skips_the_picker() {
        let s = ended("a", 1);
        assert_eq!(
            preselect(&[finished(&s)], &pushed(), now()),
            Preselect::NoPicker
        );
        assert_eq!(preselect(&[], &pushed(), now()), Preselect::NoPicker);
    }

    #[test]
    fn a_reply_asking_a_question_is_resumed() {
        let s = asking("a", 1);
        assert_eq!(
            reason(finished(&s), &pushed()),
            Some(ResumeReason::AskedQuestion)
        );
        assert_eq!(preselect(&[finished(&s)], &pushed(), now()), resume("a"));
        // Even from before the last commit.
        let s = asking("a", 5);
        assert_eq!(preselect(&[finished(&s)], &pushed(), now()), resume("a"));
    }

    #[test]
    fn an_interrupted_session_is_resumed() {
        let s = ended("a", 5);
        assert_eq!(
            reason(interrupted(&s), &pushed()),
            Some(ResumeReason::Interrupted)
        );
        assert_eq!(preselect(&[interrupted(&s)], &pushed(), now()), resume("a"));
    }

    #[test]
    fn dirty_repo_resumes_a_session_active_after_the_last_commit() {
        let s = ended("a", 1);
        assert_eq!(
            reason(finished(&s), &dirty()),
            Some(ResumeReason::UncommittedChanges)
        );
        assert_eq!(preselect(&[finished(&s)], &dirty(), now()), resume("a"));
    }

    #[test]
    fn dirty_repo_offers_new_agent_when_the_session_ended_before_the_commit() {
        let s = ended("a", 5);
        assert_eq!(reason(finished(&s), &dirty()), None);
        assert_eq!(
            preselect(&[finished(&s)], &dirty(), now()),
            Preselect::NewAgent
        );
        assert_eq!(preselect(&[], &dirty(), now()), Preselect::NewAgent);
    }

    #[test]
    fn commits_ahead_resume_a_session_active_after_the_last_commit() {
        let s = ended("a", 1);
        assert_eq!(
            reason(finished(&s), &ahead(2)),
            Some(ResumeReason::UnpushedCommits)
        );
        assert_eq!(preselect(&[finished(&s)], &ahead(2), now()), resume("a"));
    }

    #[test]
    fn a_branch_without_upstream_counts_as_unpushed() {
        let repo = RepoFacts {
            upstream: Upstream::Missing(2),
            ..pushed()
        };
        let s = ended("a", 1);
        assert_eq!(
            reason(finished(&s), &repo),
            Some(ResumeReason::UnpushedCommits)
        );
        assert_eq!(preselect(&[finished(&s)], &repo, now()), resume("a"));
    }

    #[test]
    fn a_branch_without_upstream_and_nothing_off_origin_has_nothing_to_push() {
        let repo = RepoFacts {
            upstream: Upstream::Missing(0),
            ..pushed()
        };
        let s = ended("a", 1);
        assert_eq!(reason(finished(&s), &repo), None);
        assert_eq!(
            preselect(&[finished(&s)], &repo, now()),
            Preselect::NoPicker
        );
    }

    #[test]
    fn ahead_of_an_upstream_counts_without_an_origin() {
        let repo = RepoFacts {
            has_origin: false,
            ..ahead(2)
        };
        let s = ended("a", 1);
        assert_eq!(
            reason(finished(&s), &repo),
            Some(ResumeReason::UnpushedCommits)
        );
    }

    #[test]
    fn commits_ahead_from_before_the_session_skip_the_picker() {
        let s = ended("a", 5);
        assert_eq!(reason(finished(&s), &ahead(2)), None);
        assert_eq!(
            preselect(&[finished(&s)], &ahead(2), now()),
            Preselect::NoPicker
        );
    }

    #[test]
    fn an_old_unfinished_session_is_shown_but_not_preselected() {
        let s = ended("a", 72);
        assert_eq!(
            reason(interrupted(&s), &pushed()),
            Some(ResumeReason::Interrupted)
        );
        assert_eq!(
            preselect(&[interrupted(&s)], &pushed(), now()),
            Preselect::NewAgent
        );
        let s = asking("a", 72);
        assert_eq!(
            reason(finished(&s), &pushed()),
            Some(ResumeReason::AskedQuestion)
        );
        assert_eq!(
            preselect(&[finished(&s)], &pushed(), now()),
            Preselect::NewAgent
        );
    }

    // Order and edges.

    #[test]
    fn the_first_reason_in_the_table_wins() {
        let repo = RepoFacts {
            dirty: true,
            ..ahead(2)
        };
        let s = asking("a", 1);
        assert_eq!(
            reason(interrupted(&s), &repo),
            Some(ResumeReason::Interrupted)
        );
        assert_eq!(
            reason(finished(&s), &repo),
            Some(ResumeReason::AskedQuestion)
        );
        let s = ended("a", 1);
        assert_eq!(
            reason(finished(&s), &repo),
            Some(ResumeReason::UncommittedChanges)
        );
    }

    #[test]
    fn a_repo_without_origin_has_nothing_to_push() {
        let repo = RepoFacts {
            has_origin: false,
            upstream: Upstream::Missing(1),
            ..pushed()
        };
        let s = ended("a", 1);
        assert_eq!(reason(finished(&s), &repo), None);
        assert_eq!(
            preselect(&[finished(&s)], &repo, now()),
            Preselect::NoPicker
        );
    }

    #[test]
    fn a_detached_head_has_nothing_to_push() {
        let repo = RepoFacts {
            on_branch: false,
            upstream: Upstream::Missing(1),
            ..pushed()
        };
        let s = ended("a", 1);
        assert_eq!(reason(finished(&s), &repo), None);
    }

    #[test]
    fn a_gone_upstream_has_nothing_to_push() {
        let repo = RepoFacts {
            upstream: Upstream::Gone,
            ..pushed()
        };
        let s = ended("a", 1);
        assert_eq!(reason(finished(&s), &repo), None);
        assert_eq!(
            preselect(&[finished(&s)], &repo, now()),
            Preselect::NoPicker
        );
    }

    #[test]
    fn a_repo_without_commits_counts_changes_but_not_pushes() {
        let unborn = RepoFacts {
            head_time: None,
            upstream: Upstream::Missing(0),
            ..pushed()
        };
        let s = ended("a", 30);
        assert_eq!(reason(finished(&s), &unborn), None);
        let repo = RepoFacts {
            dirty: true,
            ..unborn
        };
        assert_eq!(
            reason(finished(&s), &repo),
            Some(ResumeReason::UncommittedChanges)
        );
        assert_eq!(preselect(&[finished(&s)], &repo, now()), resume("a"));
    }

    #[test]
    fn a_newer_running_session_takes_the_last_three_reasons_from_older_ones() {
        let live = ended("live", 0);
        let stopped = asking("stopped", 1);
        let interrupted_one = ended("cut", 2);
        // Working in the Refract window: the picker shows, with New agent preselected.
        let candidates = [
            running(&live, LiveState::Working, true),
            finished(&stopped),
            interrupted(&interrupted_one),
        ];
        assert_eq!(
            resume_reasons(&candidates, &dirty()),
            [None, None, Some(ResumeReason::Interrupted)]
        );
        let candidates = [running(&live, LiveState::Working, true), finished(&stopped)];
        assert_eq!(resume_reasons(&candidates, &dirty()), [None, None]);
        assert_eq!(preselect(&candidates, &dirty(), now()), Preselect::NewAgent);
        // Running elsewhere, in a clean repo: nothing to offer.
        let candidates = [
            running(&live, LiveState::Working, false),
            finished(&stopped),
        ];
        assert_eq!(resume_reasons(&candidates, &ahead(2)), [None, None]);
        assert_eq!(
            preselect(&candidates, &pushed(), now()),
            Preselect::NoPicker
        );
    }

    #[test]
    fn an_empty_transcript_is_not_the_last_conversation() {
        let empty = SessionSummary {
            entry_count: 3,
            ..ended("empty", 0)
        };
        let asked = SessionSummary {
            tail: Some(crate::transcript::Tail::AssistantReply),
            entry_count: 5,
            ..asking("asked", 1)
        };
        let candidates = [finished(&empty), finished(&asked)];
        assert_eq!(
            resume_reasons(&candidates, &pushed()),
            [None, Some(ResumeReason::AskedQuestion)]
        );
        assert_eq!(preselect(&candidates, &pushed(), now()), resume("asked"));
        // An empty running one doesn't hide it either.
        let candidates = [running(&empty, LiveState::Unknown, false), finished(&asked)];
        assert_eq!(preselect(&candidates, &pushed(), now()), resume("asked"));
        // A transcript whose last line was too long to read still counts.
        let unread = ended("unread", 0);
        let candidates = [finished(&unread), finished(&asked)];
        assert_eq!(
            resume_reasons(&candidates, &dirty()),
            [Some(ResumeReason::UncommittedChanges), None]
        );
    }

    #[test]
    fn only_the_last_conversation_gets_the_other_reasons() {
        let older = asking("older", 2);
        let newer = ended("newer", 1);
        let candidates = [finished(&older), finished(&newer)];
        assert_eq!(
            resume_reasons(&candidates, &dirty()),
            [None, Some(ResumeReason::UncommittedChanges)]
        );
        assert_eq!(resume_reasons(&candidates, &pushed()), [None, None]);
        assert_eq!(
            preselect(&candidates, &pushed(), now()),
            Preselect::NoPicker
        );
    }

    #[test]
    fn interrupted_older_and_finished_newer_shows_the_picker_with_new_agent() {
        let older = ended("older", 2);
        let newer = ended("newer", 1);
        let candidates = [interrupted(&older), finished(&newer)];
        assert_eq!(
            resume_reasons(&candidates, &pushed()),
            [Some(ResumeReason::Interrupted), None]
        );
        assert_eq!(
            preselect(&candidates, &pushed(), now()),
            Preselect::NewAgent
        );
    }

    #[test]
    fn age_boundary_is_exclusive() {
        let just_inside = summary("a", now() - MAX_RESUME_AGE + TimeDelta::seconds(1));
        assert_eq!(
            preselect(&[interrupted(&just_inside)], &pushed(), now()),
            resume("a")
        );
        let at_limit = summary("a", now() - MAX_RESUME_AGE);
        assert_eq!(
            preselect(&[interrupted(&at_limit)], &pushed(), now()),
            Preselect::NewAgent
        );
    }

    #[test]
    fn a_session_must_be_active_strictly_after_the_commit() {
        let at_commit = summary("a", DateTime::from_timestamp(commit_time(), 0).unwrap());
        assert_eq!(reason(finished(&at_commit), &dirty()), None);
        let just_after = summary(
            "a",
            DateTime::from_timestamp(commit_time(), 1_000_000).unwrap(),
        );
        assert_eq!(
            reason(finished(&just_after), &dirty()),
            Some(ResumeReason::UncommittedChanges)
        );
    }

    #[test]
    fn session_without_timestamp_is_not_resumed() {
        let mut s = ended("a", 1);
        s.last_timestamp = None;
        assert_eq!(reason(finished(&s), &dirty()), None);
        assert_eq!(
            reason(interrupted(&s), &dirty()),
            Some(ResumeReason::Interrupted)
        );
        assert_eq!(
            preselect(&[interrupted(&s)], &dirty(), now()),
            Preselect::NewAgent
        );
    }

    #[test]
    fn reasons_go_on_the_wire_in_camel_case() {
        assert_eq!(
            serde_json::to_value(ResumeReason::UncommittedChanges).unwrap(),
            serde_json::json!("uncommittedChanges")
        );
        assert_eq!(
            serde_json::to_value(ResumeReason::AskedQuestion).unwrap(),
            serde_json::json!("askedQuestion")
        );
    }

    // Running agents.

    #[test]
    fn live_sessions_are_not_resume_candidates() {
        let s = asking("a", 0);
        let live = running(&s, LiveState::Working, false);
        assert_eq!(reason(live, &dirty()), None);
        assert_eq!(preselect(&[live], &pushed(), now()), Preselect::NoPicker);
    }

    #[test]
    fn focusable_live_session_always_shows_the_picker() {
        let s = ended("a", 0);
        for live in [LiveState::Working, LiveState::Idle, LiveState::Unknown] {
            assert_eq!(
                preselect(&[running(&s, live, true)], &pushed(), now()),
                Preselect::NewAgent,
                "{live:?}"
            );
        }
    }

    #[test]
    fn live_session_outside_vscode_does_not_stop_the_shortcut() {
        let s = ended("a", 0);
        let got = preselect(
            &[running(&s, LiveState::WaitingOnUser, false)],
            &pushed(),
            now(),
        );
        assert_eq!(got, Preselect::NoPicker);
    }

    #[test]
    fn focusable_session_waiting_on_you_is_preselected() {
        let s = ended("a", 0);
        let got = preselect(
            &[running(&s, LiveState::WaitingOnUser, true)],
            &pushed(),
            now(),
        );
        assert_eq!(got, Preselect::Focus("a".to_owned()));
    }

    #[test]
    fn going_to_a_waiting_agent_comes_before_resume() {
        let open = ended("open", 1);
        let waiting = ended("waiting", 2);
        let candidates = [
            interrupted(&open),
            running(&waiting, LiveState::WaitingOnUser, true),
        ];
        let got = preselect(&candidates, &dirty(), now());
        assert_eq!(got, Preselect::Focus("waiting".to_owned()));
    }

    #[test]
    fn only_the_newest_focusable_session_is_considered() {
        let older = ended("older", 2);
        let newer = ended("newer", 1);
        let candidates = [
            running(&older, LiveState::WaitingOnUser, true),
            running(&newer, LiveState::Working, true),
        ];
        assert_eq!(
            preselect(&candidates, &pushed(), now()),
            Preselect::NewAgent
        );
        // A newer session outside VS Code does not hide the one inside.
        let candidates = [
            running(&older, LiveState::WaitingOnUser, true),
            running(&newer, LiveState::WaitingOnUser, false),
        ];
        assert_eq!(
            preselect(&candidates, &pushed(), now()),
            Preselect::Focus("older".to_owned())
        );
    }

    #[test]
    fn working_focusable_session_leaves_resume_alone() {
        let open = ended("open", 1);
        let busy = ended("busy", 0);
        let candidates = [interrupted(&open), running(&busy, LiveState::Working, true)];
        assert_eq!(preselect(&candidates, &pushed(), now()), resume("open"));
    }

    #[test]
    fn finished_session_marked_focusable_is_ignored() {
        let s = ended("a", 0);
        let done = Candidate {
            focusable: true,
            ..finished(&s)
        };
        assert_eq!(preselect(&[done], &pushed(), now()), Preselect::NoPicker);
    }

    #[test]
    fn focus_goes_on_the_wire_with_its_session_id() {
        assert_eq!(
            serde_json::to_value(Preselect::Focus("a".into())).unwrap(),
            serde_json::json!({"kind": "focus", "sessionId": "a"})
        );
    }
}
