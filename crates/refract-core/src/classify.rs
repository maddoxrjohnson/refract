//! Session state: live, interrupted or finished (spec §6).

use serde::{Deserialize, Serialize};

use crate::transcript::{SessionSummary, Tail};

/// Status reported by the optional status hook (spec §6.2). The file format is defined
/// later; this is the decoded value.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum HookStatus {
    Working,
    WaitingOnUser,
    Finished,
    Ended,
}

/// A status hook report: the status and when the script wrote it, in Unix seconds.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct HookReport {
    pub status: HookStatus,
    pub at: Option<i64>,
}

/// What a running session is doing.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum LiveState {
    /// The agent is working on a turn.
    Working,
    /// The agent is blocked on permission or input.
    WaitingOnUser,
    /// The last turn finished and the agent waits for a prompt.
    Idle,
    /// Running, but no hook status says more.
    Unknown,
}

/// Why a session that is no longer running counts as interrupted.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum InterruptReason {
    /// The user's last prompt got no reply.
    UnansweredPrompt,
    /// A tool call has no result.
    ToolCallWithoutResult,
    /// Tool results arrived but the assistant never continued the turn.
    ToolResultWithoutReply,
    /// The last response was an API error.
    ApiError,
}

/// How a finished session ended.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum FinishKind {
    /// The last turn completed normally.
    Completed,
    /// The user interrupted the last turn or rejected a tool call.
    UserInterrupted,
}

/// The state of one session. On the wire: `{"kind": "interrupted", "detail": "apiError"}`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", content = "detail", rename_all = "camelCase")]
pub enum SessionState {
    Live(LiveState),
    Interrupted(InterruptReason),
    Finished(FinishKind),
}

impl SessionState {
    /// True for sessions worth offering to resume.
    pub fn is_unfinished(self) -> bool {
        matches!(self, SessionState::Interrupted(_))
    }
}

/// Classifies a session from its transcript summary, whether a `claude` process is
/// running it, and the hook status if there is one.
///
/// Process detection decides liveness. A hook saying `Ended` while the process still
/// runs gives `Live(Unknown)`. When the process is gone the transcript alone decides.
pub fn classify(summary: &SessionSummary, live: bool, hook: Option<HookStatus>) -> SessionState {
    classify_report(
        summary,
        live,
        hook.map(|status| HookReport { status, at: None }),
        0,
    )
}

/// [`classify`] with the time of the hook status. Pressing Esc or denying a permission
/// ends a turn without a `Stop` hook, so a live session whose transcript ends in a user
/// interrupt written after a working or waiting status counts as idle.
///
/// `background` is how many background subagents the live session's process has
/// running. A live session that would be idle while any run counts as working: its own
/// turn ended, but work it started goes on. It is ignored when the session is not live.
pub fn classify_report(
    summary: &SessionSummary,
    live: bool,
    hook: Option<HookReport>,
    background: u32,
) -> SessionState {
    if live {
        let interrupted_since = |at: Option<i64>| {
            summary.tail == Some(Tail::UserInterrupt)
                && at
                    .zip(summary.last_timestamp)
                    .is_some_and(|(at, last)| last.timestamp_millis() > at * 1000)
        };
        let state = match hook.map(|h| (h.status, h.at)) {
            Some((HookStatus::Working | HookStatus::WaitingOnUser, at))
                if interrupted_since(at) =>
            {
                LiveState::Idle
            }
            Some((HookStatus::Working, _)) => LiveState::Working,
            Some((HookStatus::WaitingOnUser, _)) => LiveState::WaitingOnUser,
            Some((HookStatus::Finished, _)) => LiveState::Idle,
            Some((HookStatus::Ended, _)) | None => LiveState::Unknown,
        };
        if state == LiveState::Idle && background > 0 {
            return SessionState::Live(LiveState::Working);
        }
        return SessionState::Live(state);
    }
    match summary.tail {
        Some(Tail::UserPrompt) => SessionState::Interrupted(InterruptReason::UnansweredPrompt),
        Some(Tail::PendingToolUse) => {
            SessionState::Interrupted(InterruptReason::ToolCallWithoutResult)
        }
        Some(Tail::UnansweredToolResult) => {
            SessionState::Interrupted(InterruptReason::ToolResultWithoutReply)
        }
        Some(Tail::ApiError) => SessionState::Interrupted(InterruptReason::ApiError),
        Some(Tail::UserInterrupt) => SessionState::Finished(FinishKind::UserInterrupted),
        Some(Tail::AssistantReply | Tail::ToolEndedTurn) | None => {
            SessionState::Finished(FinishKind::Completed)
        }
    }
}

#[cfg(test)]
mod tests {
    use chrono::{TimeZone, Utc};

    use super::*;

    #[test]
    fn a_user_interrupt_after_the_hook_status_means_idle() {
        let at = 1_773_489_600;
        let summary = SessionSummary {
            tail: Some(Tail::UserInterrupt),
            last_timestamp: Utc.timestamp_opt(at + 5, 0).single(),
            ..SessionSummary::default()
        };
        let report = |status, at| Some(HookReport { status, at });
        for status in [HookStatus::Working, HookStatus::WaitingOnUser] {
            assert_eq!(
                classify_report(&summary, true, report(status, Some(at)), 0),
                SessionState::Live(LiveState::Idle)
            );
            // An interrupt from before the status (an earlier turn) does not count, nor
            // one that cannot be placed in time.
            assert_eq!(
                classify_report(&summary, true, report(status, Some(at + 10)), 0),
                classify(&summary, true, Some(status))
            );
            assert_eq!(
                classify_report(&summary, true, report(status, None), 0),
                classify(&summary, true, Some(status))
            );
        }
        assert_eq!(
            classify_report(&summary, true, report(HookStatus::Ended, Some(at)), 0),
            SessionState::Live(LiveState::Unknown)
        );
        let prompt = SessionSummary {
            tail: Some(Tail::UserPrompt),
            ..summary
        };
        assert_eq!(
            classify_report(&prompt, true, report(HookStatus::Working, Some(at)), 0),
            SessionState::Live(LiveState::Working)
        );
    }

    #[test]
    fn running_background_agents_keep_an_idle_live_session_working() {
        let at = 1_773_489_600;
        let report = |status| {
            Some(HookReport {
                status,
                at: Some(at),
            })
        };
        let done = SessionSummary {
            tail: Some(Tail::AssistantReply),
            ..SessionSummary::default()
        };
        assert_eq!(
            classify_report(&done, true, report(HookStatus::Finished), 2),
            SessionState::Live(LiveState::Working)
        );
        assert_eq!(
            classify_report(&done, true, report(HookStatus::Finished), 0),
            SessionState::Live(LiveState::Idle)
        );
        assert_eq!(
            classify_report(&done, true, report(HookStatus::WaitingOnUser), 1),
            SessionState::Live(LiveState::WaitingOnUser)
        );
        // No hook says the turn ended, so nothing is known either way.
        assert_eq!(
            classify_report(&done, true, None, 1),
            SessionState::Live(LiveState::Unknown)
        );
        assert_eq!(
            classify_report(&done, false, report(HookStatus::Finished), 1),
            classify_report(&done, false, report(HookStatus::Finished), 0)
        );
        // Interrupted by Esc after the status, with agents still running.
        let interrupted = SessionSummary {
            tail: Some(Tail::UserInterrupt),
            last_timestamp: Utc.timestamp_opt(at + 5, 0).single(),
            ..SessionSummary::default()
        };
        assert_eq!(
            classify_report(&interrupted, true, report(HookStatus::Working), 1),
            SessionState::Live(LiveState::Working)
        );
        assert_eq!(
            classify_report(&interrupted, true, report(HookStatus::Working), 0),
            SessionState::Live(LiveState::Idle)
        );
    }
}
