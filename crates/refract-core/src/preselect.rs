//! Which sessions are worth resuming, and which option the launch picker preselects
//! (spec §4.3).
//!
//! A session that isn't running is unfinished, with a [`ResumeReason`], when it was
//! interrupted or cut off by a restart, or when it is the newest session that isn't running and it asked the
//! person a question, or was active after the last commit while the repo has
//! uncommitted changes or unpushed commits. Enter resumes that newest session when it
//! is unfinished and recent; its size doesn't matter.
//!
//! A live session whose process runs in a terminal of the Refract window on exactly
//! this repo's folder is "focusable": the picker
//! offers to go to it. While one exists the picker always shows, so Enter never starts
//! a second agent beside it by accident; when the newest one is waiting on the person,
//! going to it is preselected.
//!
//! Sessions of both agents are candidates, and resume reasons apply to each whatever its
//! agent. [`plan`] adds Codex's rules (`.plans/codex-spec.md` §5): the hand-back, where
//! the other agent worked last and a new agent of this one is briefed from its session
//! instead of resuming an older own one, and the nudge, where Claude is near its usage
//! limit and New Codex agent is preselected.

use chrono::{DateTime, TimeDelta, Utc};
use serde::{Deserialize, Serialize};

use crate::agent::AgentKind;
use crate::classify::{LiveState, SessionState};
use crate::codex_ready::{LiveAgent, ReadyState};
use crate::limits::PlanLimits;
use crate::transcript::SessionSummary;

/// Sessions that ended this long ago or longer are not preselected for resume.
pub const MAX_RESUME_AGE: TimeDelta = TimeDelta::days(2);

/// A session the picker could offer.
#[derive(Debug, Clone, Copy)]
pub struct Candidate<'a> {
    pub agent: AgentKind,
    pub summary: &'a SessionSummary,
    pub state: SessionState,
    /// Its process is known and runs in the Refract window on this repo, so a live
    /// session can be gone to.
    pub focusable: bool,
    /// It is not running and was closed by `/clear`
    /// ([`ended_by_clear`](crate::classify::ended_by_clear)): the person moved on from
    /// it, so it is never the last conversation and never preselected.
    pub cleared: bool,
    /// It is not running and its latest status report, from before a restart, does not
    /// say it was closed ([`cut_off`](crate::classify::cut_off)). One with no
    /// conversation in it has nothing to resume, so it gets no reason.
    pub cut_off: bool,
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
/// first listed wins. On the wire: `"interrupted"`, `"cutOff"`, `"askedQuestion"`, ...
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum ResumeReason {
    /// It stopped mid-task (any session).
    Interrupted,
    /// A restart killed it without it being closed (any session).
    CutOff,
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
/// sessions. Only an interrupt or a cut-off counts for any session, also while another
/// one runs; the other reasons count only for the repo's last conversation, and only
/// when it isn't running: a running agent's own edits aren't another session's
/// unfinished work. When the last conversation was closed by `/clear`, the person moved
/// on: no session gets those reasons, not even an older one. The age of a session
/// doesn't matter here.
pub fn resume_reasons(candidates: &[Candidate<'_>], repo: &RepoFacts) -> Vec<Option<ResumeReason>> {
    let last = newest(candidates, |c| !c.is_empty()).filter(|&i| {
        !matches!(candidates[i].state, SessionState::Live(_)) && !candidates[i].cleared
    });
    candidates
        .iter()
        .enumerate()
        .map(|(i, c)| {
            if matches!(c.state, SessionState::Interrupted(_)) {
                return Some(ResumeReason::Interrupted);
            }
            if c.cut_off && !c.is_empty() {
                return Some(ResumeReason::CutOff);
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

/// Index of the newest candidate with a conversation that isn't running. One closed by
/// `/clear` has no reason, so it keeps older sessions from being preselected.
fn newest_stopped(candidates: &[Candidate<'_>]) -> Option<usize> {
    newest(candidates, |c| {
        !c.is_empty() && !matches!(c.state, SessionState::Live(_))
    })
}

/// The `codexThreshold` setting's default: Claude's use, as a percentage, from which the
/// nudge preselects New Codex agent.
pub const DEFAULT_CODEX_THRESHOLD: f64 = 95.0;

/// What the hand-back and nudge rules need beyond the sessions. The default (no limits
/// known, Codex not installed) gives today's Claude-only rules.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct AgentFacts {
    /// Claude's highest use of its 5-hour and weekly windows, as a percentage
    /// ([`claude_peak`]); `None` when unknown.
    pub claude_used_percent: Option<f64>,
    /// The `codexThreshold` setting.
    pub codex_threshold: f64,
    /// The repo's Codex readiness state; `None` when Codex is not installed.
    pub codex: Option<ReadyState>,
}

impl Default for AgentFacts {
    fn default() -> Self {
        AgentFacts {
            claude_used_percent: None,
            codex_threshold: DEFAULT_CODEX_THRESHOLD,
            codex: None,
        }
    }
}

/// Claude's highest use, as a percentage, of the plan windows that haven't reset by
/// `now`; `None` when none is left.
pub fn claude_peak(limits: &PlanLimits, now: DateTime<Utc>) -> Option<f64> {
    [limits.five_hour, limits.seven_day]
        .into_iter()
        .flatten()
        .filter(|w| w.resets_at > now)
        .map(|w| w.used_percent)
        .reduce(f64::max)
}

/// Why New is preselected for a particular agent. On the wire: `{"kind": "handBack"}` or
/// `{"kind": "nudge", "usedPercent": 97}`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(
    tag = "kind",
    rename_all = "camelCase",
    rename_all_fields = "camelCase"
)]
pub enum NewAgentReason {
    /// The other agent worked here last.
    HandBack,
    /// Claude is at `used_percent` (rounded down) of a limit, at or over the threshold.
    Nudge { used_percent: u32 },
}

/// What a [`Preselect::NewAgent`] means beyond the old shape: which agent the
/// preselected New row starts, the other agent's session it is briefed from, and why.
/// The default is a plain New Claude agent. On the wire as `newAgent` beside
/// `preselect`: `{"agent": "codex", "briefFrom": "...", "reason": {"kind": "nudge",
/// "usedPercent": 97}}`, each field left out when default.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct NewAgentHint {
    #[serde(default, skip_serializing_if = "AgentKind::is_claude")]
    pub agent: AgentKind,
    /// The session of the other agent to brief from: pass it as `LaunchMode::New`'s
    /// `brief`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub brief_from: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reason: Option<NewAgentReason>,
}

impl NewAgentHint {
    /// A plain New Claude agent, as before Codex.
    pub fn is_plain(&self) -> bool {
        *self == NewAgentHint::default()
    }

    /// The reason as the picker shows it: "Claude at 97%" or "Codex worked here last".
    pub fn reason_text(&self) -> Option<String> {
        Some(match self.reason.as_ref()? {
            NewAgentReason::Nudge { used_percent } => format!("Claude at {used_percent}%"),
            NewAgentReason::HandBack => format!("{} worked here last", self.agent.other().name()),
        })
    }
}

/// The launch decision with its New row.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Plan {
    pub preselect: Preselect,
    /// Not plain only with [`Preselect::NewAgent`].
    pub new_agent: NewAgentHint,
}

/// Chooses the preselected picker option for Claude-only rules: [`plan`] with default
/// [`AgentFacts`].
pub fn preselect(candidates: &[Candidate<'_>], repo: &RepoFacts, now: DateTime<Utc>) -> Preselect {
    plan(candidates, repo, &AgentFacts::default(), now).preselect
}

/// Chooses the preselected picker option and its New row.
///
/// The agent to launch is Codex when the nudge applies (Codex is installed and Ready or
/// with warnings, and Claude's use is at or over the threshold), otherwise Claude. The
/// rules, in order:
/// 1. when the newest focusable live session waits on the person, go to it;
/// 2. hand-back: when the newest session with a conversation is the other agent's, is
///    not closed by `/clear`, and is either live and idle or stopped less than
///    [`MAX_RESUME_AGE`] ago (and, for the nudge, unfinished), preselect a new agent
///    briefed from it. A busy one leaves the rules below;
/// 3. when the newest session that isn't running is this agent's, has a
///    [`ResumeReason`] and ended less than [`MAX_RESUME_AGE`] ago, resume it;
/// 4. with the nudge, preselect New Codex agent;
/// 5. with no session to resume, a clean repo and no focusable live session, skip the
///    picker; otherwise preselect New agent.
pub fn plan(
    candidates: &[Candidate<'_>],
    repo: &RepoFacts,
    agents: &AgentFacts,
    now: DateTime<Utc>,
) -> Plan {
    let plain = |preselect| Plan {
        preselect,
        new_agent: NewAgentHint::default(),
    };
    let focusable = candidates
        .iter()
        .filter(|c| c.live_and_focusable())
        .max_by_key(|c| c.summary.last_timestamp);
    if let Some(running) = focusable {
        if running.state == SessionState::Live(LiveState::WaitingOnUser) {
            if let Some(id) = &running.summary.session_id {
                return plain(Preselect::Focus(id.clone()));
            }
        }
    }
    let nudge = nudge_percent(agents);
    let target = if nudge.is_some() {
        AgentKind::Codex
    } else {
        AgentKind::Claude
    };
    let reason = match nudge {
        Some(used_percent) => NewAgentReason::Nudge { used_percent },
        None => NewAgentReason::HandBack,
    };
    let reasons = resume_reasons(candidates, repo);
    let recent = |c: &Candidate<'_>| {
        c.summary
            .last_timestamp
            .is_some_and(|ended| now - ended < MAX_RESUME_AGE)
    };
    if let Some(i) = newest(candidates, |c| !c.is_empty()) {
        let last = &candidates[i];
        let briefable = match last.state {
            SessionState::Live(state) => state == LiveState::Idle,
            _ => !last.cleared && recent(last) && (nudge.is_none() || reasons[i].is_some()),
        };
        if last.agent != target && briefable {
            if let Some(id) = &last.summary.session_id {
                return Plan {
                    preselect: Preselect::NewAgent,
                    new_agent: NewAgentHint {
                        agent: target,
                        brief_from: Some(id.clone()),
                        reason: Some(reason),
                    },
                };
            }
        }
    }
    if let Some(i) = newest_stopped(candidates) {
        let newest = &candidates[i];
        if newest.agent == target && reasons[i].is_some() && recent(newest) {
            if let Some(id) = &newest.summary.session_id {
                return plain(Preselect::Resume(id.clone()));
            }
        }
    }
    if nudge.is_some() {
        return Plan {
            preselect: Preselect::NewAgent,
            new_agent: NewAgentHint {
                agent: AgentKind::Codex,
                brief_from: None,
                reason: Some(reason),
            },
        };
    }
    if reasons.iter().all(Option::is_none) && !repo.dirty && focusable.is_none() {
        plain(Preselect::NoPicker)
    } else {
        plain(Preselect::NewAgent)
    }
}

/// Claude's use, rounded down, when the nudge applies.
fn nudge_percent(agents: &AgentFacts) -> Option<u32> {
    let used = agents.claude_used_percent?;
    let ready = agents.codex.is_some_and(ReadyState::launchable);
    (ready && used >= agents.codex_threshold).then(|| used.floor().clamp(0.0, 1000.0) as u32)
}

/// The live sessions among the candidates, newest first, as
/// [`codex_ready::readiness`](crate::codex_ready::readiness) and
/// [`gate`](crate::codex_ready::gate) take them.
pub fn live_agents(candidates: &[Candidate<'_>]) -> Vec<LiveAgent> {
    let mut live: Vec<(&Candidate<'_>, LiveAgent)> = candidates
        .iter()
        .filter_map(|c| match (c.state, &c.summary.session_id) {
            (SessionState::Live(state), Some(id)) => Some((
                c,
                LiveAgent {
                    agent: c.agent,
                    session_id: id.clone(),
                    state,
                },
            )),
            _ => None,
        })
        .collect();
    live.sort_by_key(|(c, _)| std::cmp::Reverse(c.summary.last_timestamp));
    live.into_iter().map(|(_, l)| l).collect()
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
            agent: AgentKind::Claude,
            summary,
            state: SessionState::Interrupted(InterruptReason::ToolCallWithoutResult),
            focusable: false,
            cleared: false,
            cut_off: false,
        }
    }

    fn finished(summary: &SessionSummary) -> Candidate<'_> {
        Candidate {
            agent: AgentKind::Claude,
            summary,
            state: SessionState::Finished(FinishKind::Completed),
            focusable: false,
            cleared: false,
            cut_off: false,
        }
    }

    fn running(summary: &SessionSummary, live: LiveState, focusable: bool) -> Candidate<'_> {
        Candidate {
            agent: AgentKind::Claude,
            summary,
            state: SessionState::Live(live),
            focusable,
            cleared: false,
            cut_off: false,
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
    fn a_session_cut_off_by_a_restart_is_offered_like_an_interrupted_one() {
        let cut = |summary| Candidate {
            cut_off: true,
            ..finished(summary)
        };
        // Older than a finished session: not the last conversation, still offered.
        let (older, newer) = (ended("older", 2), ended("newer", 1));
        let candidates = [cut(&older), finished(&newer)];
        assert_eq!(
            resume_reasons(&candidates, &pushed()),
            [Some(ResumeReason::CutOff), None]
        );
        assert_eq!(
            preselect(&candidates, &pushed(), now()),
            Preselect::NewAgent
        );
        // The newest stopped one, recent: resumed.
        assert_eq!(preselect(&[cut(&older)], &pushed(), now()), resume("older"));
        // Stopped mid-task too: the interrupt wins. Asked a question too: the cut-off
        // wins.
        let both = Candidate {
            cut_off: true,
            ..interrupted(&older)
        };
        assert_eq!(reason(both, &pushed()), Some(ResumeReason::Interrupted));
        let asked = asking("asked", 1);
        assert_eq!(reason(cut(&asked), &dirty()), Some(ResumeReason::CutOff));
        // Another session running.
        let live = ended("live", 0);
        let candidates = [running(&live, LiveState::Working, true), cut(&older)];
        assert_eq!(
            resume_reasons(&candidates, &pushed()),
            [None, Some(ResumeReason::CutOff)]
        );
        // The newest conversation closed by /clear.
        let cleared = Candidate {
            cleared: true,
            ..finished(&newer)
        };
        assert_eq!(
            resume_reasons(&[cut(&older), cleared], &pushed()),
            [Some(ResumeReason::CutOff), None]
        );
        // A transcript with no conversation in it has nothing to resume.
        let empty = SessionSummary {
            entry_count: 2,
            ..ended("empty", 1)
        };
        assert_eq!(reason(cut(&empty), &pushed()), None);
        assert_eq!(
            preselect(&[cut(&empty)], &pushed(), now()),
            Preselect::NoPicker
        );
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
        assert_eq!(
            serde_json::to_value(ResumeReason::CutOff).unwrap(),
            serde_json::json!("cutOff")
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
    fn a_session_closed_by_clear_is_not_the_last_conversation() {
        // `claude --resume x` then /clear: x stopped, the new session has no
        // conversation yet.
        let x = ended("x", 1);
        let new = SessionSummary {
            entry_count: 2,
            ..ended("new", 0)
        };
        let cleared = Candidate {
            cleared: true,
            ..finished(&x)
        };
        let fresh = finished(&new);
        for repo in [dirty(), ahead(2)] {
            let candidates = [cleared, fresh];
            assert_eq!(resume_reasons(&candidates, &repo), [None, None]);
            assert_ne!(preselect(&candidates, &repo, now()), resume("x"));
            // Without the /clear, x is the last conversation.
            let candidates = [finished(&x), fresh];
            assert!(resume_reasons(&candidates, &repo)[0].is_some());
            assert_eq!(preselect(&candidates, &repo, now()), resume("x"));
        }
        // An older session stopped mid-task is still offered, but not preselected: the
        // newest stopped conversation was closed on purpose.
        let older = ended("older", 2);
        let candidates = [interrupted(&older), cleared, fresh];
        assert_eq!(
            resume_reasons(&candidates, &pushed()),
            [Some(ResumeReason::Interrupted), None, None]
        );
        assert_eq!(
            preselect(&candidates, &pushed(), now()),
            Preselect::NewAgent
        );
        // Nor does an older finished session inherit the repo's uncommitted changes.
        let candidates = [finished(&older), cleared, fresh];
        assert_eq!(resume_reasons(&candidates, &dirty()), [None, None, None]);
        assert_eq!(preselect(&candidates, &dirty(), now()), Preselect::NewAgent);
    }

    // Codex: hand-back and nudge.

    fn codex(candidate: Candidate<'_>) -> Candidate<'_> {
        Candidate {
            agent: AgentKind::Codex,
            ..candidate
        }
    }

    /// Codex installed and ready, Claude at `used` percent.
    fn claude_at(used: f64) -> AgentFacts {
        AgentFacts {
            claude_used_percent: Some(used),
            codex: Some(ReadyState::Ready),
            ..AgentFacts::default()
        }
    }

    fn briefed(agent: AgentKind, from: &str, reason: NewAgentReason) -> Plan {
        Plan {
            preselect: Preselect::NewAgent,
            new_agent: NewAgentHint {
                agent,
                brief_from: Some(from.into()),
                reason: Some(reason),
            },
        }
    }

    #[test]
    fn a_newer_codex_session_hands_back_to_a_briefed_claude() {
        let stale = ended("stale", 3);
        let worked = ended("worked", 1);
        let candidates = [interrupted(&stale), codex(finished(&worked))];
        let got = plan(&candidates, &pushed(), &AgentFacts::default(), now());
        assert_eq!(
            got,
            briefed(AgentKind::Claude, "worked", NewAgentReason::HandBack)
        );
        assert_eq!(
            got.new_agent.reason_text().as_deref(),
            Some("Codex worked here last")
        );
        // Resume reasons stay per session, whatever the agent.
        assert_eq!(
            resume_reasons(&candidates, &dirty()),
            [
                Some(ResumeReason::Interrupted),
                Some(ResumeReason::UncommittedChanges)
            ]
        );
        // Even with nothing unfinished, the picker shows.
        let got = plan(
            &[codex(finished(&worked))],
            &pushed(),
            &AgentFacts::default(),
            now(),
        );
        assert_eq!(got.new_agent.brief_from.as_deref(), Some("worked"));
        // An old one doesn't.
        let older = ended("older", 80);
        let old = ended("old", 72);
        let candidates = [interrupted(&older), codex(finished(&old))];
        let got = plan(&candidates, &pushed(), &AgentFacts::default(), now());
        assert_eq!(got.preselect, Preselect::NewAgent);
        assert!(got.new_agent.is_plain());
    }

    #[test]
    fn an_older_codex_session_leaves_claude_s_resume_alone() {
        let worked = ended("worked", 3);
        let mine = ended("mine", 1);
        let candidates = [codex(finished(&worked)), interrupted(&mine)];
        let got = plan(&candidates, &pushed(), &AgentFacts::default(), now());
        assert_eq!(got.preselect, resume("mine"));
        assert!(got.new_agent.is_plain());
    }

    #[test]
    fn an_idle_live_codex_session_is_briefed_from_and_a_busy_one_is_not() {
        let mine = ended("mine", 2);
        let open = ended("open", 0);
        let candidates = [
            interrupted(&mine),
            codex(running(&open, LiveState::Idle, true)),
        ];
        assert_eq!(
            plan(&candidates, &pushed(), &AgentFacts::default(), now()),
            briefed(AgentKind::Claude, "open", NewAgentReason::HandBack)
        );
        let candidates = [
            interrupted(&mine),
            codex(running(&open, LiveState::Working, true)),
        ];
        assert_eq!(
            plan(&candidates, &pushed(), &AgentFacts::default(), now()).preselect,
            resume("mine")
        );
        assert_eq!(
            live_agents(&candidates),
            [LiveAgent {
                agent: AgentKind::Codex,
                session_id: "open".into(),
                state: LiveState::Working,
            }]
        );
    }

    #[test]
    fn claude_near_its_limit_nudges_to_codex() {
        let s = ended("a", 1);
        let got = plan(&[finished(&s)], &pushed(), &claude_at(97.6), now());
        assert_eq!(
            got,
            Plan {
                preselect: Preselect::NewAgent,
                new_agent: NewAgentHint {
                    agent: AgentKind::Codex,
                    brief_from: None,
                    reason: Some(NewAgentReason::Nudge { used_percent: 97 }),
                },
            }
        );
        assert_eq!(
            got.new_agent.reason_text().as_deref(),
            Some("Claude at 97%")
        );
        // At the threshold exactly, and with warnings.
        let facts = AgentFacts {
            codex: Some(ReadyState::Warnings),
            ..claude_at(95.0)
        };
        assert_eq!(
            plan(&[], &pushed(), &facts, now()).new_agent.agent,
            AgentKind::Codex
        );
        // Below it, not ready, busy or not installed: today's rules.
        for facts in [
            claude_at(94.9),
            AgentFacts {
                codex: Some(ReadyState::NotReady),
                ..claude_at(99.0)
            },
            AgentFacts {
                codex: Some(ReadyState::Busy),
                ..claude_at(99.0)
            },
            AgentFacts {
                codex: None,
                ..claude_at(99.0)
            },
            AgentFacts {
                claude_used_percent: None,
                ..claude_at(99.0)
            },
        ] {
            let got = plan(&[finished(&s)], &pushed(), &facts, now());
            assert_eq!(got.preselect, Preselect::NoPicker, "{facts:?}");
            assert!(got.new_agent.is_plain(), "{facts:?}");
        }
    }

    #[test]
    fn the_nudge_briefs_codex_from_an_unfinished_claude_session() {
        let cut = ended("cut", 1);
        let got = plan(&[interrupted(&cut)], &pushed(), &claude_at(100.0), now());
        assert_eq!(
            got,
            briefed(
                AgentKind::Codex,
                "cut",
                NewAgentReason::Nudge { used_percent: 100 }
            )
        );
        // A finished one is not briefed from.
        let done = ended("done", 1);
        let got = plan(&[finished(&done)], &pushed(), &claude_at(100.0), now());
        assert_eq!(got.new_agent.brief_from, None);
        assert_eq!(got.new_agent.agent, AgentKind::Codex);
        // Codex's own unfinished session, newest: resumed.
        let mine = ended("mine", 0);
        let candidates = [interrupted(&cut), codex(interrupted(&mine))];
        assert_eq!(
            plan(&candidates, &pushed(), &claude_at(100.0), now()).preselect,
            resume("mine")
        );
        // Going to a waiting agent still comes first.
        let waiting = ended("waiting", 0);
        let candidates = [
            interrupted(&cut),
            running(&waiting, LiveState::WaitingOnUser, true),
        ];
        assert_eq!(
            plan(&candidates, &pushed(), &claude_at(100.0), now()).preselect,
            Preselect::Focus("waiting".into())
        );
    }

    #[test]
    fn claude_peak_takes_the_highest_window_that_has_not_reset() {
        use crate::limits::LimitWindow;
        let window = |used_percent, hours| {
            Some(LimitWindow {
                used_percent,
                resets_at: now() + TimeDelta::hours(hours),
            })
        };
        let limits = PlanLimits {
            five_hour: window(40.0, 2),
            seven_day: window(96.0, 50),
            updated_at: now(),
        };
        assert_eq!(claude_peak(&limits, now()), Some(96.0));
        let reset = PlanLimits {
            seven_day: window(96.0, -1),
            ..limits.clone()
        };
        assert_eq!(claude_peak(&reset, now()), Some(40.0));
        let none = PlanLimits {
            five_hour: None,
            ..reset
        };
        assert_eq!(claude_peak(&none, now()), None);
    }

    #[test]
    fn the_new_agent_hint_goes_on_the_wire() {
        assert_eq!(
            serde_json::to_value(NewAgentHint::default()).unwrap(),
            serde_json::json!({})
        );
        let hint = briefed(
            AgentKind::Codex,
            "a",
            NewAgentReason::Nudge { used_percent: 97 },
        )
        .new_agent;
        assert_eq!(
            serde_json::to_value(&hint).unwrap(),
            serde_json::json!({"agent": "codex", "briefFrom": "a",
                "reason": {"kind": "nudge", "usedPercent": 97}})
        );
        assert_eq!(
            serde_json::to_value(NewAgentReason::HandBack).unwrap(),
            serde_json::json!({"kind": "handBack"})
        );
    }

    #[test]
    fn focus_goes_on_the_wire_with_its_session_id() {
        assert_eq!(
            serde_json::to_value(Preselect::Focus("a".into())).unwrap(),
            serde_json::json!({"kind": "focus", "sessionId": "a"})
        );
    }
}
