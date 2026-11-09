//! The opening prompt of a briefed launch (spec `.plans/codex-spec.md` §8): a new agent
//! picking up a repo from the other agent's session. The helper builds it when a
//! `LaunchMode::New` names a `brief` session, and passes it after `--` as the prompt,
//! for Claude and Codex alike.

use crate::agent::AgentKind;
use crate::classify::{LiveState, SessionState};

/// The longest last message a brief quotes, in bytes, cut on a character boundary.
pub const MAX_MESSAGE_BYTES: usize = 2048;

/// The marker of a branch Codex left for Claude to review, as written in the handoff.
pub const NEEDS_REVIEW: &str = "Needs review:";

/// How the other agent's session ended, as the brief tells it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SessionEnd {
    /// It stopped mid-turn without being closed (a restart, a crash, a closed terminal).
    CutOff,
    /// It hit its usage limit.
    RateLimited,
    /// It stopped mid-task or the person interrupted it.
    Interrupted,
    /// Its last turn finished (also an idle session stopped for the pick-up).
    Finished,
}

impl SessionEnd {
    /// How a Claude session ended, from its state, whether a restart cut it off
    /// ([`crate::classify::cut_off`]) and whether it hit its usage limit (the helper's
    /// plan limits at 100%, say). For a Codex session use
    /// [`CodexSummary::session_end`](crate::codex_transcript::CodexSummary::session_end).
    pub fn of(state: SessionState, cut_off: bool, rate_limited: bool) -> SessionEnd {
        if rate_limited {
            return SessionEnd::RateLimited;
        }
        if cut_off {
            return SessionEnd::CutOff;
        }
        match state {
            SessionState::Interrupted(_) => SessionEnd::Interrupted,
            SessionState::Finished(crate::classify::FinishKind::UserInterrupted) => {
                SessionEnd::Interrupted
            }
            SessionState::Finished(_) | SessionState::Live(LiveState::Idle) => SessionEnd::Finished,
            SessionState::Live(_) => SessionEnd::Interrupted,
        }
    }

    fn words(self) -> &'static str {
        match self {
            SessionEnd::CutOff => "was cut off mid-turn",
            SessionEnd::RateLimited => "stopped at its usage limit",
            SessionEnd::Interrupted => "was interrupted",
            SessionEnd::Finished => "finished its last turn",
        }
    }
}

/// What a brief is built from.
#[derive(Debug, Clone, Copy)]
pub struct BriefInput<'a> {
    /// The agent being launched.
    pub target: AgentKind,
    /// The agent whose session it picks up from.
    pub from: AgentKind,
    pub end: SessionEnd,
    /// That session's last assistant message, in full; it is capped here.
    pub last_message: Option<&'a str>,
    /// The handoff file's text, read for [`NEEDS_REVIEW`] lines when briefing Claude.
    pub handoff: Option<&'a str>,
}

/// The brief prompt.
pub fn brief(input: &BriefInput<'_>) -> String {
    let from = input.from.name();
    let mut text = format!(
        "You are picking up this repository from {from}. Follow AGENTS.md and the handoff it \
names, and continue with its next item.\n{from}'s last session {}.",
        input.end.words()
    );
    if let Some(message) = input.last_message.map(str::trim).filter(|m| !m.is_empty()) {
        text.push_str(" Its last message was:\n\n");
        for line in cap(message, MAX_MESSAGE_BYTES).lines() {
            text.push_str(if line.is_empty() { ">" } else { "> " });
            text.push_str(line);
            text.push('\n');
        }
    } else {
        text.push('\n');
    }
    text.push_str(
        "\nRun `git status` and `git diff` before continuing: it may have left work uncommitted.",
    );
    if input.target == AgentKind::Claude {
        let lines = needs_review(input.handoff.unwrap_or_default());
        if !lines.is_empty() {
            text.push_str(
                "\n\nThe handoff lists branches to review, merge and push, then remove from it:",
            );
            for line in lines {
                text.push_str("\n- ");
                text.push_str(line);
            }
        }
    }
    // A NUL can't go in an argument, and the extension refuses a launch with one.
    text.retain(|c| c != '\0');
    text
}

/// `text` cut to at most `max_bytes` on a character boundary, ending in `…` when cut.
pub fn cap(text: &str, max_bytes: usize) -> String {
    if text.len() <= max_bytes {
        return text.to_owned();
    }
    let mut end = max_bytes.saturating_sub('…'.len_utf8());
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    format!("{}…", &text[..end])
}

/// The `Needs review: <branch>` entries in a handoff, from the marker to the end of the
/// line, trimmed, in order. A marker with nothing after it doesn't count.
pub fn needs_review(handoff: &str) -> Vec<&str> {
    handoff
        .lines()
        .filter_map(|line| {
            let at = line.find(NEEDS_REVIEW)?;
            let entry = line[at..].trim_end().trim_end_matches('`').trim_end();
            (!entry[NEEDS_REVIEW.len()..].trim().is_empty()).then_some(entry)
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::classify::{FinishKind, InterruptReason};

    fn input<'a>(
        target: AgentKind,
        message: Option<&'a str>,
        handoff: Option<&'a str>,
    ) -> BriefInput<'a> {
        BriefInput {
            target,
            from: target.other(),
            end: SessionEnd::RateLimited,
            last_message: message,
            handoff,
        }
    }

    #[test]
    fn a_brief_names_the_agent_its_end_and_its_last_message() {
        let text = brief(&input(
            AgentKind::Codex,
            Some("Tide tables are in.\n\nNext: the CLI."),
            None,
        ));
        assert_eq!(
            text,
            "You are picking up this repository from Claude. Follow AGENTS.md and the handoff it \
names, and continue with its next item.\nClaude's last session stopped at its usage limit. Its \
last message was:\n\n> Tide tables are in.\n>\n> Next: the CLI.\n\nRun `git status` and `git \
diff` before continuing: it may have left work uncommitted."
        );
        let text = brief(&BriefInput {
            end: SessionEnd::CutOff,
            ..input(AgentKind::Claude, Some("  "), None)
        });
        assert!(text.contains("Codex's last session was cut off mid-turn.\n\nRun `git status`"));
        // A NUL in the message is dropped.
        let text = brief(&input(AgentKind::Claude, Some("Tide\0 tables"), None));
        assert!(text.contains("> Tide tables\n") && !text.contains('\0'));
    }

    #[test]
    fn a_hand_back_to_claude_lists_branches_to_review() {
        let handoff = "# Handoff\n\nNeeds review: codex/tide-tables\n- `Needs review: codex/cli`\nNeeds review:\nNext: wire it up.\n";
        let text = brief(&input(AgentKind::Claude, None, Some(handoff)));
        assert!(text.ends_with(
            "The handoff lists branches to review, merge and push, then remove from it:\n\
- Needs review: codex/tide-tables\n- Needs review: codex/cli"
        ));
        // Codex is not asked to review.
        let text = brief(&input(AgentKind::Codex, None, Some(handoff)));
        assert!(!text.contains(NEEDS_REVIEW));
        assert!(needs_review("Nothing to review.").is_empty());
    }

    #[test]
    fn the_message_is_capped_on_a_character_boundary() {
        assert_eq!(cap("short", 10), "short");
        assert_eq!(cap("abcdef", 6), "abcdef");
        // "é" is two bytes: a cut inside it backs off to the boundary before.
        let text = "é".repeat(10);
        let cut = cap(&text, 8);
        assert_eq!(cut, "éé…");
        assert!(cut.len() <= 8);
        let long = "x".repeat(MAX_MESSAGE_BYTES * 2);
        let text = brief(&input(AgentKind::Codex, Some(&long), None));
        let quoted = text.lines().find(|l| l.starts_with("> ")).unwrap();
        assert_eq!(quoted.len(), 2 + MAX_MESSAGE_BYTES);
        assert!(quoted.ends_with('…'));
    }

    #[test]
    fn claude_session_ends_follow_their_state() {
        let interrupted = SessionState::Interrupted(InterruptReason::ApiError);
        let finished = SessionState::Finished(FinishKind::Completed);
        assert_eq!(
            SessionEnd::of(interrupted, false, false),
            SessionEnd::Interrupted
        );
        assert_eq!(SessionEnd::of(interrupted, true, false), SessionEnd::CutOff);
        assert_eq!(
            SessionEnd::of(interrupted, true, true),
            SessionEnd::RateLimited
        );
        assert_eq!(SessionEnd::of(finished, false, false), SessionEnd::Finished);
        assert_eq!(
            SessionEnd::of(
                SessionState::Finished(FinishKind::UserInterrupted),
                false,
                false
            ),
            SessionEnd::Interrupted
        );
        assert_eq!(
            SessionEnd::of(SessionState::Live(LiveState::Idle), false, false),
            SessionEnd::Finished
        );
    }
}
