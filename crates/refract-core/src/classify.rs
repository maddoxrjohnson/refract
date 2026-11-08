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

/// The most ancestors a [`HookReport`] keeps. Claude Code runs the hook command
/// through `sh -c`, so the `claude` process that ran it is the second; a third leaves
/// room for one more shell, and a nested `claude` (run from a session's Bash tool) has
/// its parent `claude` fourth, out of reach.
pub const HOOK_PIDS: usize = 3;

/// A status hook report: the status, when the script wrote it, in Unix seconds, and
/// the script's ancestors, nearest first, one of which is the `claude` process that ran
/// the hook. `pids[i]` started `starts[i]` clock ticks after boot (field 22 of
/// `/proc/<pid>/stat`); 0 is none. `cleared` is true when the status is `Ended` with
/// reason `clear`: the person ran `/clear`, which closed the session on purpose.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct HookReport {
    pub status: HookStatus,
    pub at: Option<i64>,
    pub pids: [u32; HOOK_PIDS],
    pub starts: [u64; HOOK_PIDS],
    pub cleared: bool,
}

impl HookReport {
    /// A report without pids or a reason, as an install from before they were recorded
    /// writes.
    pub fn new(status: HookStatus, at: Option<i64>) -> Self {
        HookReport {
            status,
            at,
            pids: [0; HOOK_PIDS],
            starts: [0; HOOK_PIDS],
            cleared: false,
        }
    }

    /// True when the script that wrote it recorded pids at all; an install from before
    /// then does not.
    pub fn has_pids(&self) -> bool {
        self.pids.iter().any(|&pid| pid != 0)
    }

    /// The recorded ancestors with their start ticks, nearest first. A pid without a
    /// start is left out, since it could be a later process given the same pid.
    pub fn ancestors(&self) -> impl Iterator<Item = (u32, u64)> + '_ {
        self.pids
            .iter()
            .copied()
            .zip(self.starts.iter().copied())
            .filter(|&(pid, start)| pid != 0 && start != 0)
    }
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
/// runs gives `Live(Unknown)`. When the process is gone the transcript alone decides
/// (a status without a time cannot place a `/clear`; see [`classify_report`]).
pub fn classify(summary: &SessionSummary, live: bool, hook: Option<HookStatus>) -> SessionState {
    classify_report(
        summary,
        live,
        hook.map(|status| HookReport::new(status, None)),
        0,
    )
}

/// [`classify`] with the time of the hook status. Pressing Esc or denying a permission
/// ends a turn without a `Stop` hook, so a live session whose transcript ends in a user
/// interrupt written after a working or waiting status counts as idle.
///
/// `background` is how many background subagents and workflow runs the live session's
/// process has running. A live session that would be idle while any run counts as working: its own
/// turn ended, but work it started goes on. It is ignored when the session is not live.
///
/// A session that is not live and was [`ended_by_clear`]: the person moved on from it,
/// so a transcript that ends mid-turn gives `Finished(Completed)` rather than
/// `Interrupted`.
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
                    .is_some_and(|(at, last)| last.timestamp_millis() > at.saturating_mul(1000))
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
    let state = match summary.tail {
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
    };
    if state.is_unfinished() && ended_by_clear(summary, hook) {
        return SessionState::Finished(FinishKind::Completed);
    }
    state
}

/// True when the session's report is `cleared` (it was closed by `/clear`) and is not
/// older than the transcript's last line: `at` is whole seconds, so it counts up to a
/// second after. A resumed session writes later lines and later reports, so its old
/// `/clear` no longer counts. Without a time on either side it is false. Whether the
/// session is live is the caller's to check.
pub fn ended_by_clear(summary: &SessionSummary, hook: Option<HookReport>) -> bool {
    hook.filter(|h| h.cleared)
        .and_then(|h| h.at)
        .zip(summary.last_timestamp)
        .is_some_and(|(at, last)| {
            at.saturating_add(1).saturating_mul(1000) > last.timestamp_millis()
        })
}

/// A report older than this no longer marks its session [`cut_off`]: the status hook's
/// sweep deletes such reports anyway.
pub const CUT_OFF_MAX_AGE: i64 = 7 * 24 * 60 * 60;

/// True when the session's latest report was written before the machine last booted
/// (`boot`, Unix seconds) and does not say `ended`: Claude Code never ran `SessionEnd`,
/// so its process died with the machine rather than being closed. A report without a
/// time, an unknown boot time and a report [`CUT_OFF_MAX_AGE`] old or older (at `now`,
/// Unix seconds) give false. Resuming the session writes a report after the boot, and
/// closing it writes `ended`, so either ends it. Whether the session is live is the
/// caller's to check.
pub fn cut_off(hook: Option<HookReport>, boot: Option<i64>, now: i64) -> bool {
    hook.filter(|h| h.status != HookStatus::Ended)
        .and_then(|h| h.at)
        .zip(boot)
        .is_some_and(|(at, boot)| at < boot && now.saturating_sub(at) < CUT_OFF_MAX_AGE)
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
        let report = |status, at| Some(HookReport::new(status, at));
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
        let report = |status| Some(HookReport::new(status, Some(at)));
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

    #[test]
    fn a_session_ended_by_clear_is_not_interrupted() {
        let at = 1_773_489_600;
        let cleared = |at| {
            let mut report = HookReport::new(HookStatus::Ended, Some(at));
            report.cleared = true;
            Some(report)
        };
        let summary = |tail, millis| SessionSummary {
            tail: Some(tail),
            last_timestamp: Utc.timestamp_millis_opt(millis).single(),
            ..SessionSummary::default()
        };
        let tails = [
            (Tail::UserPrompt, InterruptReason::UnansweredPrompt),
            (Tail::PendingToolUse, InterruptReason::ToolCallWithoutResult),
            (
                Tail::UnansweredToolResult,
                InterruptReason::ToolResultWithoutReply,
            ),
            (Tail::ApiError, InterruptReason::ApiError),
        ];
        for (tail, reason) in tails {
            let interrupted = SessionState::Interrupted(reason);
            // Cleared at or after the last line, within the same second too.
            for last in [at * 1000 - 5000, at * 1000, at * 1000 + 999] {
                assert_eq!(
                    classify_report(&summary(tail, last), false, cleared(at), 0),
                    SessionState::Finished(FinishKind::Completed),
                    "{tail:?} {last}"
                );
            }
            // A report older than the last line: the session was resumed since.
            let later = summary(tail, at * 1000 + 1000);
            assert_eq!(classify_report(&later, false, cleared(at), 0), interrupted);
            // Ended for another reason, or with no time to place it.
            let now = summary(tail, at * 1000);
            assert_eq!(
                classify_report(
                    &now,
                    false,
                    Some(HookReport::new(HookStatus::Ended, Some(at))),
                    0
                ),
                interrupted
            );
            let mut untimed = cleared(at).unwrap();
            untimed.at = None;
            assert_eq!(classify_report(&now, false, Some(untimed), 0), interrupted);
            let unplaced = SessionSummary {
                last_timestamp: None,
                ..summary(tail, at * 1000)
            };
            assert_eq!(
                classify_report(&unplaced, false, cleared(at), 0),
                interrupted
            );
            // A live session is the process's to say.
            assert_eq!(
                classify_report(&now, true, cleared(at), 0),
                SessionState::Live(LiveState::Unknown)
            );
        }
        // A finished transcript stays as it is.
        let esc = summary(Tail::UserInterrupt, at * 1000);
        assert_eq!(
            classify_report(&esc, false, cleared(at), 0),
            SessionState::Finished(FinishKind::UserInterrupted)
        );
    }

    #[test]
    fn a_report_from_before_the_boot_that_does_not_say_ended_is_cut_off() {
        let boot = 1_773_489_600;
        let now = boot + 3600;
        let report = |status, at| Some(HookReport::new(status, Some(at)));
        for status in [
            HookStatus::Working,
            HookStatus::WaitingOnUser,
            HookStatus::Finished,
        ] {
            assert!(cut_off(report(status, boot - 1), Some(boot), now));
            // Written after the boot: resumed since, or still running.
            assert!(!cut_off(report(status, boot), Some(boot), now));
            // Up to the 7-day sweep, exclusive.
            let old = now - CUT_OFF_MAX_AGE;
            assert!(cut_off(report(status, old + 1), Some(boot), now));
            assert!(!cut_off(report(status, old), Some(boot), now));
        }
        // Closed, or nothing to go on.
        assert!(!cut_off(
            report(HookStatus::Ended, boot - 1),
            Some(boot),
            now
        ));
        let untimed = Some(HookReport::new(HookStatus::Finished, None));
        assert!(!cut_off(untimed, Some(boot), now));
        assert!(!cut_off(report(HookStatus::Finished, boot - 1), None, now));
        assert!(!cut_off(None, Some(boot), now));
    }
}
