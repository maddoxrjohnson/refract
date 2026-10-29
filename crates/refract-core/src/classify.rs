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
    if live {
        let state = match hook {
            Some(HookStatus::Working) => LiveState::Working,
            Some(HookStatus::WaitingOnUser) => LiveState::WaitingOnUser,
            Some(HookStatus::Finished) => LiveState::Idle,
            Some(HookStatus::Ended) | None => LiveState::Unknown,
        };
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
