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
/// reason `clear`: the person ran `/clear`, which closed the session on purpose. `boot`
/// is the kernel's boot id that the script read ([`parse_boot_id`]) and `init` when PID
/// 1 started, in clock ticks after boot; 0 is unknown for either.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct HookReport {
    pub status: HookStatus,
    pub at: Option<i64>,
    pub pids: [u32; HOOK_PIDS],
    pub starts: [u64; HOOK_PIDS],
    pub cleared: bool,
    pub boot: u128,
    pub init: u64,
}

impl HookReport {
    /// A report without pids, a reason or a boot, as an install from before they were
    /// recorded writes.
    pub fn new(status: HookStatus, at: Option<i64>) -> Self {
        HookReport {
            status,
            at,
            pids: [0; HOOK_PIDS],
            starts: [0; HOOK_PIDS],
            cleared: false,
            boot: 0,
            init: 0,
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

/// A report this old or older no longer marks its session [`cut_off`]. The helper
/// deletes reports untouched this long when it reads the whole status directory.
pub const CUT_OFF_MAX_AGE: i64 = 7 * 24 * 60 * 60;

/// Clock ticks per second in `/proc/<pid>/stat` start times, taken as 100 as on every
/// Linux build Refract runs on.
pub const TICKS_PER_SECOND: u64 = 100;

/// The boot the helper runs in: the kernel's boot id ([`parse_boot_id`]), when PID 1
/// started in clock ticks after boot (0 for either is unknown), and the boot time in
/// Unix seconds (`btime` from `/proc/stat`). A WSL distro restarted inside a running VM
/// keeps the boot id and the boot time but gets a new PID 1.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Boot {
    pub id: u128,
    pub init: u64,
    pub btime: Option<i64>,
}

/// A boot id as `/proc/sys/kernel/random/boot_id` holds it: a UUID, case and a final
/// newline aside. `None` for anything else.
pub fn parse_boot_id(text: &str) -> Option<u128> {
    let text = text.trim_end();
    let hex: String = text.chars().filter(|&c| c != '-').collect();
    (text.len() == 36 && hex.len() == 32 && hex.chars().all(|c| c.is_ascii_hexdigit()))
        .then(|| u128::from_str_radix(&hex, 16).ok())
        .flatten()
}

/// True when the session's latest report does not say `ended` and was written before a
/// restart: Claude Code never ran `SessionEnd`, so its process died with the machine or
/// the distro rather than being closed.
///
/// A report that names its boot id or PID 1 start was written before a restart when
/// either differs from `boot`'s (a side that doesn't know one doesn't compare it). The
/// clock plays no part, as a WSL VM can keep its boot over a distro restart, and its
/// clock (so the boot time a helper reads) moves after the host sleeps. A report from
/// a script before those were recorded falls back to its time: before `btime` plus
/// PID 1's start.
///
/// It is false for a report without a time or [`CUT_OFF_MAX_AGE`] old or older (at
/// `now`, Unix seconds), so it lasts as long as the report. Resuming the session
/// writes a report from this boot, and closing it writes `ended`, so either ends it.
/// Whether the session is live is the caller's to check.
pub fn cut_off(hook: Option<HookReport>, boot: Boot, now: i64) -> bool {
    let Some((hook, at)) = hook
        .filter(|h| h.status != HookStatus::Ended)
        .and_then(|h| Some((h, h.at?)))
    else {
        return false;
    };
    if now.saturating_sub(at) >= CUT_OFF_MAX_AGE {
        return false;
    }
    let differs =
        |recorded: u128, current: u128| recorded != 0 && current != 0 && recorded != current;
    if hook.boot != 0 || hook.init != 0 {
        return differs(hook.boot, boot.id) || differs(hook.init.into(), boot.init.into());
    }
    let init_secs = i64::try_from(boot.init / TICKS_PER_SECOND).unwrap_or(i64::MAX);
    boot.btime
        .is_some_and(|btime| at < btime.saturating_add(init_secs))
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
    fn a_report_from_another_boot_or_init_that_does_not_say_ended_is_cut_off() {
        let btime = 1_773_489_600;
        let now = btime + 3600;
        let id = parse_boot_id("5346b8e5-5fc0-4caf-b9ca-2e442e237168\n").unwrap();
        let boot = Boot {
            id,
            init: 5068,
            btime: Some(btime),
        };
        let report = |status, at, boot_id, init| {
            let mut report = HookReport::new(status, Some(at));
            report.boot = boot_id;
            report.init = init;
            Some(report)
        };
        for status in [
            HookStatus::Working,
            HookStatus::WaitingOnUser,
            HookStatus::Finished,
        ] {
            // This boot and init: not cut off, whatever the clock says.
            assert!(!cut_off(report(status, btime - 60, id, 5068), boot, now));
            // Another boot, or the same VM with the distro restarted.
            assert!(cut_off(report(status, btime + 60, id + 1, 5068), boot, now));
            assert!(cut_off(report(status, btime + 60, id, 9000), boot, now));
            // Only what both sides know is compared.
            assert!(cut_off(report(status, btime + 60, 0, 9000), boot, now));
            assert!(!cut_off(report(status, btime - 60, 0, 5068), boot, now));
            let unknown = Boot {
                id: 0,
                init: 0,
                ..boot
            };
            assert!(!cut_off(
                report(status, btime - 60, id + 1, 9000),
                unknown,
                now
            ));
            // Up to the 7-day limit, exclusive.
            let old = now - CUT_OFF_MAX_AGE;
            assert!(cut_off(report(status, old + 1, id + 1, 5068), boot, now));
            assert!(!cut_off(report(status, old, id + 1, 5068), boot, now));
        }
        // A script from before boot ids: before the boot time plus PID 1's start (50 s).
        let old_script = |at| Some(HookReport::new(HookStatus::Finished, Some(at)));
        assert!(cut_off(old_script(btime + 49), boot, now));
        assert!(!cut_off(old_script(btime + 50), boot, now));
        let no_btime = Boot {
            btime: None,
            ..boot
        };
        assert!(!cut_off(old_script(btime - 60), no_btime, now));
        // Closed, or nothing to go on.
        assert!(!cut_off(
            report(HookStatus::Ended, btime + 60, id + 1, 5068),
            boot,
            now
        ));
        let mut untimed = report(HookStatus::Finished, 0, id + 1, 5068).unwrap();
        untimed.at = None;
        assert!(!cut_off(Some(untimed), boot, now));
        assert!(!cut_off(None, boot, now));
    }

    #[test]
    fn boot_ids_parse_as_uuids_only() {
        assert_eq!(
            parse_boot_id("5346B8E5-5fc0-4caf-b9ca-2e442e237168"),
            Some(0x5346b8e5_5fc0_4caf_b9ca_2e442e237168)
        );
        for bad in [
            "",
            "5346b8e5",
            "5346b8e5-5fc0-4caf-b9ca-2e442e23716g",
            "5346b8e55fc04cafb9ca2e442e237168",
        ] {
            assert_eq!(parse_boot_id(bad), None, "{bad}");
        }
    }
}
