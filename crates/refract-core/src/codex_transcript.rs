//! Parsing of Codex CLI rollout files (spec `.plans/codex-spec.md` §3).
//!
//! A rollout is plain JSONL at
//! `$CODEX_HOME/sessions/YYYY/MM/DD/rollout-<local time>-<session id>.jsonl`, created on
//! the first message and appended to on resume. Each line is
//! `{timestamp, ordinal, type, payload}`. What Refract reads:
//!
//! - `session_meta`: `payload.id` (the session id), `cwd`, `originator` (`codex-tui` or
//!   `codex_exec`), `source` (`cli`, `vscode` or `exec`), `thread_source` (`user` for a
//!   thread the person started). It is the first line and can be tens of KiB long, as
//!   it carries the base instructions.
//! - `event_msg` with `payload.type`: `task_started` starts a turn, `task_complete`
//!   (with `last_agent_message`) finishes one, `turn_aborted` (with `reason`, such as
//!   `interrupted`) ends one on Esc. Quitting writes nothing, so a turn that started with
//!   no later complete or abort, in a session that isn't live, was cut off.
//!   `token_count` carries `rate_limits`, whose `rate_limit_reached_type` is null unless
//!   a limit was hit.
//! - `response_item` with `payload.type` `message`: `role` `user` (`input_text` blocks;
//!   the injected `<environment_context>` and similar start with `<`) or `assistant`
//!   (`output_text` blocks). `developer` messages are Codex's own instructions.
//!
//! The result is the same [`SessionSummary`] Claude transcripts give, so classify and
//! preselect take Codex sessions as they are, plus the Codex-only facts in
//! [`CodexSummary`]. Token usage and edited files are not read.
//!
//! A rollout only grows, so a [`CodexParser`] can be kept and fed the lines appended
//! since the last read, carrying the turn, rate limit and last message on.

use chrono::{DateTime, Utc};
use serde_json::Value;

use crate::brief::SessionEnd;
use crate::classify::{classify_report, LiveState, SessionState};
use crate::transcript::{asks_question, snippet, SessionSummary, Tail};

/// The longest last assistant message kept in full, in bytes, cut on a character
/// boundary. More than the brief takes ([`crate::brief::MAX_MESSAGE_BYTES`]).
pub const MAX_MESSAGE_BYTES: usize = 8 * 1024;

/// How the last turn in the parsed lines stands.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TurnEnd {
    /// `task_complete`: the turn finished.
    Complete,
    /// `turn_aborted`, with its reason (`interrupted` for Esc).
    Aborted(Option<String>),
    /// `task_started` with nothing after it: working if live, otherwise cut off.
    MidTurn,
}

/// What the first lines of a rollout say. Fields the head does not name stay `None`.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct CodexHead {
    pub session_id: Option<String>,
    pub cwd: Option<String>,
    pub originator: Option<String>,
    pub source: Option<String>,
    /// `thread_source`: `user` for a thread the person started.
    pub thread_source: Option<String>,
    /// `source` is an object rather than a name, as for a thread Codex spawned itself
    /// (a subagent).
    pub spawned: bool,
}

impl CodexHead {
    /// A non-interactive `codex exec` session. Codex hides these from its own resume
    /// picker, and Refract does too.
    pub fn is_exec(&self) -> bool {
        self.source.as_deref() == Some("exec") || self.originator.as_deref() == Some("codex_exec")
    }

    /// A thread the person did not start: `thread_source` names another source, or
    /// Codex spawned it. Older rollouts without the field count as the person's.
    pub fn is_spawned(&self) -> bool {
        self.spawned || self.thread_source.as_deref().is_some_and(|t| t != "user")
    }

    /// Left out of Refract's sessions: an exec session or a spawned thread.
    pub fn is_hidden(&self) -> bool {
        self.is_exec() || self.is_spawned()
    }
}

/// What Refract needs to know about one rollout.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct CodexSummary {
    /// The Claude-shaped summary: id, cwd, times, snippets, `asks_user` and a `tail`
    /// that [`classify_report`] reads as it does Claude's (see [`CodexSummary::state`]).
    pub summary: SessionSummary,
    pub head: CodexHead,
    /// `None` when the parsed lines have no turn marker.
    pub end: Option<TurnEnd>,
    /// The latest `token_count` says a rate limit was hit.
    pub rate_limit_reached: bool,
    /// The assistant's last message in full, up to [`MAX_MESSAGE_BYTES`], for a brief.
    pub last_message: Option<String>,
    /// The parsed lines start partway through the rollout, so a turn may have started
    /// before them: with no turn marker among them, the turn is unknown.
    pub partial: bool,
}

impl CodexSummary {
    /// The session's state. Codex has no status hook yet, so a live session is working
    /// while its last turn is open, or when the lines read hold no turn marker and do not
    /// reach the start of the rollout (the turn began before them), and idle once a turn
    /// ended. A live session with no conversation read yet (just opened, or no rollout
    /// yet) is unknown: the person may be typing its first prompt. One that isn't live
    /// is classified from its tail like a Claude session: a cut-off turn is interrupted,
    /// a finished one completed, an aborted one interrupted by the person.
    pub fn state(&self, live: bool) -> SessionState {
        if live {
            return SessionState::Live(match &self.end {
                Some(TurnEnd::MidTurn) => LiveState::Working,
                Some(_) => LiveState::Idle,
                None if self.partial => LiveState::Working,
                None if self.summary.tail.is_none() => LiveState::Unknown,
                None => LiveState::Idle,
            });
        }
        classify_report(&self.summary, false, None, 0)
    }

    /// How the session ended, for a brief, when it is not live.
    pub fn session_end(&self) -> SessionEnd {
        if self.rate_limit_reached {
            return SessionEnd::RateLimited;
        }
        match self.end {
            Some(TurnEnd::MidTurn) => SessionEnd::CutOff,
            Some(TurnEnd::Aborted(_)) => SessionEnd::Interrupted,
            Some(TurnEnd::Complete) | None => SessionEnd::Finished,
        }
    }

    /// Fills what a tail did not see from the head: the session id, cwd, originator and
    /// source.
    pub fn with_head(mut self, head: &CodexHead) -> Self {
        let fill = |field: &mut Option<String>, from: &Option<String>| {
            if field.is_none() {
                field.clone_from(from);
            }
        };
        fill(&mut self.head.session_id, &head.session_id);
        fill(&mut self.head.cwd, &head.cwd);
        fill(&mut self.head.originator, &head.originator);
        fill(&mut self.head.source, &head.source);
        fill(&mut self.head.thread_source, &head.thread_source);
        self.head.spawned |= head.spawned;
        fill(&mut self.summary.session_id, &head.session_id);
        fill(&mut self.summary.cwd, &head.cwd);
        self
    }
}

/// Parses a sequence of lines, such as a whole rollout. Lines that are not JSON objects
/// count as malformed.
pub fn parse_lines<I, S>(lines: I) -> CodexSummary
where
    I: IntoIterator<Item = S>,
    S: AsRef<str>,
{
    let mut parser = CodexParser::default();
    for line in lines {
        parser.push_line(line.as_ref());
    }
    parser.summary()
}

/// Parses the complete lines at the start of a rollout, as read from its first bytes.
/// A last line cut off by the read is ignored.
pub fn parse_head(bytes: &[u8]) -> CodexHead {
    let end = bytes.iter().rposition(|&b| b == b'\n').map_or(0, |i| i + 1);
    let text = String::from_utf8_lossy(&bytes[..end]);
    let head = parse_lines(text.lines()).head;
    if head.session_id.is_some() || end == bytes.len() {
        return head;
    }
    // No newline yet: the whole read may still be one complete line.
    parse_lines([String::from_utf8_lossy(bytes)]).head
}

/// Parses the end of a rollout, as read from `bytes` on. Unless `at_start` (the read
/// began at the start of the file), the first line is taken as cut off and skipped.
/// Combine with [`parse_head`] through [`CodexSummary::with_head`].
pub fn parse_tail(bytes: &[u8], at_start: bool) -> CodexSummary {
    let start = if at_start {
        0
    } else {
        bytes
            .iter()
            .position(|&b| b == b'\n')
            .map_or(bytes.len(), |i| i + 1)
    };
    parse_lines(String::from_utf8_lossy(&bytes[start..]).lines())
}

/// The session id in a rollout's file name, `rollout-<time>-<id>.jsonl`: the last 36
/// characters before `.jsonl`, when they have a UUID's shape.
pub fn file_session_id(file_name: &str) -> Option<&str> {
    let stem = file_name.strip_prefix("rollout-")?.strip_suffix(".jsonl")?;
    let id = stem.get(stem.len().checked_sub(36)?..)?;
    let shaped = id.char_indices().all(|(i, c)| {
        if matches!(i, 8 | 13 | 18 | 23) {
            c == '-'
        } else {
            c.is_ascii_hexdigit()
        }
    });
    shaped.then_some(id)
}

/// Parses rollout lines one at a time; [`CodexParser::summary`] says what the lines so
/// far add up to, so a parser can be kept and fed a rollout's new lines as it grows.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct CodexParser {
    out: CodexSummary,
    saw_user: bool,
    saw_assistant: bool,
    /// The last assistant message, up to [`MAX_MESSAGE_BYTES`].
    reply: Option<String>,
    /// The last assistant message, in full, asks the person something.
    reply_asks: bool,
}

impl CodexParser {
    /// A parser for lines that start partway through a rollout (see
    /// [`CodexSummary::partial`]).
    pub fn partial() -> CodexParser {
        let mut parser = CodexParser::default();
        parser.out.partial = true;
        parser
    }

    /// Takes in one line. A line that is not a JSON object counts as malformed.
    pub fn push_line(&mut self, line: &str) {
        let line = line.trim();
        if line.is_empty() {
            return;
        }
        match serde_json::from_str::<Value>(line) {
            Ok(entry) if entry.is_object() => self.push_entry(&entry),
            _ => self.out.summary.malformed_lines += 1,
        }
    }

    fn push_entry(&mut self, entry: &Value) {
        let s = &mut self.out.summary;
        s.entry_count += 1;
        if let Some(ts) = str_field(entry, "timestamp").and_then(parse_timestamp) {
            if s.first_timestamp.is_none_or(|first| ts < first) {
                s.first_timestamp = Some(ts);
            }
            if s.last_timestamp.is_none_or(|last| ts > last) {
                s.last_timestamp = Some(ts);
            }
        }
        let Some(payload) = entry.get("payload") else {
            return;
        };
        match str_field(entry, "type") {
            Some("session_meta") => self.push_meta(payload),
            Some("event_msg") => self.push_event(payload),
            Some("response_item") if str_field(payload, "type") == Some("message") => {
                self.push_message(payload)
            }
            _ => {}
        }
    }

    fn push_meta(&mut self, payload: &Value) {
        let head = &mut self.out.head;
        let set = |field: &mut Option<String>, key: &str| {
            if field.is_none() {
                *field = str_field(payload, key).map(str::to_owned);
            }
        };
        set(&mut head.session_id, "id");
        if head.session_id.is_none() {
            set(&mut head.session_id, "session_id");
        }
        set(&mut head.cwd, "cwd");
        set(&mut head.originator, "originator");
        set(&mut head.source, "source");
        set(&mut head.thread_source, "thread_source");
        head.spawned |= payload.get("source").is_some_and(Value::is_object);
        let s = &mut self.out.summary;
        if s.session_id.is_none() {
            s.session_id.clone_from(&head.session_id);
        }
        if s.cwd.is_none() {
            s.cwd.clone_from(&head.cwd);
        }
    }

    fn push_event(&mut self, payload: &Value) {
        match str_field(payload, "type") {
            Some("task_started") => {
                self.out.end = Some(TurnEnd::MidTurn);
                // A new turn: a limit hit in an earlier one is behind it.
                self.out.rate_limit_reached = false;
            }
            Some("task_complete") => {
                self.out.end = Some(TurnEnd::Complete);
                if let Some(text) = str_field(payload, "last_agent_message") {
                    self.push_reply(text);
                }
            }
            Some("turn_aborted") => {
                self.out.end = Some(TurnEnd::Aborted(
                    str_field(payload, "reason").map(str::to_owned),
                ));
            }
            Some("token_count") => {
                if let Some(limits) = payload.get("rate_limits").filter(|l| l.is_object()) {
                    self.out.rate_limit_reached = limits
                        .get("rate_limit_reached_type")
                        .is_some_and(|v| !v.is_null());
                }
            }
            _ => {}
        }
    }

    fn push_message(&mut self, payload: &Value) {
        let text = message_text(payload);
        match str_field(payload, "role") {
            Some("assistant") => {
                if let Some(text) = text.filter(|t| !t.trim().is_empty()) {
                    self.push_reply(&text);
                }
            }
            Some("user") => {
                let Some(text) = text else { return };
                let trimmed = text.trim_start();
                if trimmed.is_empty()
                    || trimmed.starts_with('<')
                    || trimmed.starts_with("# AGENTS.md instructions")
                {
                    return;
                }
                self.saw_user = true;
                if let Some(line) = snippet(trimmed) {
                    self.out.summary.last_user_prompt = Some(line);
                }
            }
            _ => {}
        }
    }

    fn push_reply(&mut self, text: &str) {
        if let Some(line) = snippet(text) {
            self.saw_assistant = true;
            self.out.summary.last_assistant_text = Some(line);
            self.reply = Some(crate::brief::cap(text, MAX_MESSAGE_BYTES));
            self.reply_asks = asks_question(text);
        }
    }

    /// What the lines so far add up to.
    pub fn summary(&self) -> CodexSummary {
        let mut out = self.out.clone();
        out.summary.tail = match &out.end {
            Some(TurnEnd::MidTurn) if out.rate_limit_reached => Some(Tail::ApiError),
            Some(TurnEnd::MidTurn) => Some(Tail::UserPrompt),
            Some(TurnEnd::Complete) => Some(Tail::AssistantReply),
            Some(TurnEnd::Aborted(_)) => Some(Tail::UserInterrupt),
            None if self.saw_assistant => Some(Tail::AssistantReply),
            None if self.saw_user => Some(Tail::UserPrompt),
            None => None,
        };
        out.summary.asks_user = out.summary.tail == Some(Tail::AssistantReply) && self.reply_asks;
        out.last_message.clone_from(&self.reply);
        out
    }
}

/// The text blocks of a message joined by newlines; `None` when it has none.
fn message_text(payload: &Value) -> Option<String> {
    let blocks = payload.get("content")?.as_array()?;
    let parts: Vec<&str> = blocks
        .iter()
        .filter(|b| {
            matches!(
                str_field(b, "type"),
                Some("input_text" | "output_text" | "text")
            )
        })
        .filter_map(|b| str_field(b, "text"))
        .collect();
    (!parts.is_empty()).then(|| parts.join("\n"))
}

fn str_field<'a>(value: &'a Value, key: &str) -> Option<&'a str> {
    value.get(key).and_then(Value::as_str)
}

fn parse_timestamp(text: &str) -> Option<DateTime<Utc>> {
    DateTime::parse_from_rfc3339(text)
        .ok()
        .map(|ts| ts.with_timezone(&Utc))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::classify::{FinishKind, InterruptReason};
    use crate::transcript::SNIPPET_MAX_CHARS;
    use serde_json::json;

    const ID: &str = "01a1bd4e-0000-7000-8000-000000000001";
    const CWD: &str = "/home/dev/projects/tidepool-api";

    fn line(n: u32, kind: &str, payload: Value) -> String {
        json!({
            "timestamp": format!("2026-11-08T17:{:02}:00.000Z", n),
            "ordinal": n,
            "type": kind,
            "payload": payload,
        })
        .to_string()
    }

    fn meta(source: &str, originator: &str) -> String {
        line(
            0,
            "session_meta",
            json!({"id": ID, "session_id": ID, "timestamp": "2026-11-08T17:00:00.000Z",
                "cwd": CWD, "originator": originator, "source": source,
                "cli_version": "0.167.0", "base_instructions": {"text": "You are Codex."}}),
        )
    }

    fn event(n: u32, payload: Value) -> String {
        line(n, "event_msg", payload)
    }

    fn message(n: u32, role: &str, text: &str) -> String {
        let block = if role == "assistant" {
            "output_text"
        } else {
            "input_text"
        };
        line(
            n,
            "response_item",
            json!({"type": "message", "role": role, "content": [{"type": block, "text": text}]}),
        )
    }

    fn token_count(n: u32, reached: Value) -> String {
        event(
            n,
            json!({"type": "token_count", "rate_limits": {"limit_id": "codex",
                "primary": {"used_percent": 12.0, "window_minutes": 300, "resets_at": 1_796_768_994},
                "secondary": null, "plan_type": "plus", "rate_limit_reached_type": reached}}),
        )
    }

    /// One finished turn: environment context, a prompt, a reply, completion.
    fn finished_turn() -> Vec<String> {
        vec![
            meta("cli", "codex-tui"),
            message(1, "developer", "You are Codex, an agent."),
            message(2, "user", "<environment_context>\n  <cwd>/home/dev</cwd>"),
            event(3, json!({"type": "task_started", "turn_id": "t1"})),
            message(4, "user", "Add tide tables"),
            message(5, "assistant", "Working on it."),
            token_count(6, Value::Null),
            message(
                7,
                "assistant",
                "Tide tables are in.\n\nShould I wire them into the CLI?",
            ),
            event(
                8,
                json!({"type": "task_complete", "turn_id": "t1",
                    "last_agent_message": "Tide tables are in.\n\nShould I wire them into the CLI?"}),
            ),
        ]
    }

    #[test]
    fn a_finished_turn_reads_like_a_claude_reply() {
        let c = parse_lines(finished_turn());
        let s = &c.summary;
        assert_eq!(s.session_id.as_deref(), Some(ID));
        assert_eq!(s.cwd.as_deref(), Some(CWD));
        assert_eq!(s.entry_count, 9);
        assert_eq!(s.last_user_prompt.as_deref(), Some("Add tide tables"));
        assert_eq!(
            s.last_assistant_text.as_deref(),
            Some("Tide tables are in.")
        );
        assert_eq!(s.tail, Some(Tail::AssistantReply));
        assert!(s.asks_user);
        assert_eq!(
            s.last_timestamp,
            Some("2026-11-08T17:08:00Z".parse().unwrap())
        );
        assert_eq!(c.end, Some(TurnEnd::Complete));
        assert!(!c.rate_limit_reached);
        assert_eq!(
            c.last_message.as_deref(),
            Some("Tide tables are in.\n\nShould I wire them into the CLI?")
        );
        assert_eq!(c.head.originator.as_deref(), Some("codex-tui"));
        assert!(!c.head.is_exec());
        assert_eq!(
            c.state(false),
            SessionState::Finished(FinishKind::Completed)
        );
        assert_eq!(c.state(true), SessionState::Live(LiveState::Idle));
        assert_eq!(c.session_end(), SessionEnd::Finished);
    }

    #[test]
    fn a_turn_with_no_end_was_cut_off_or_is_working() {
        let mut lines = finished_turn();
        lines.push(event(9, json!({"type": "task_started", "turn_id": "t2"})));
        lines.push(message(10, "user", "Now the CLI"));
        lines.push(message(11, "assistant", "Looking at main.rs."));
        let c = parse_lines(&lines);
        assert_eq!(c.end, Some(TurnEnd::MidTurn));
        assert_eq!(c.summary.tail, Some(Tail::UserPrompt));
        assert!(!c.summary.asks_user);
        assert_eq!(c.last_message.as_deref(), Some("Looking at main.rs."));
        assert_eq!(c.state(true), SessionState::Live(LiveState::Working));
        assert_eq!(
            c.state(false),
            SessionState::Interrupted(InterruptReason::UnansweredPrompt)
        );
        assert_eq!(c.session_end(), SessionEnd::CutOff);
    }

    #[test]
    fn an_aborted_turn_was_interrupted_by_the_person() {
        let mut lines = finished_turn();
        lines.push(event(9, json!({"type": "task_started"})));
        lines.push(event(
            10,
            json!({"type": "turn_aborted", "reason": "interrupted"}),
        ));
        let c = parse_lines(&lines);
        assert_eq!(c.end, Some(TurnEnd::Aborted(Some("interrupted".into()))));
        assert_eq!(c.summary.tail, Some(Tail::UserInterrupt));
        assert_eq!(
            c.state(false),
            SessionState::Finished(FinishKind::UserInterrupted)
        );
        assert_eq!(c.session_end(), SessionEnd::Interrupted);
    }

    #[test]
    fn a_limit_hit_mid_turn_is_rate_limited() {
        let mut lines = finished_turn();
        lines.push(event(9, json!({"type": "task_started"})));
        lines.push(token_count(10, json!("primary")));
        let c = parse_lines(&lines);
        assert!(c.rate_limit_reached);
        assert_eq!(c.summary.tail, Some(Tail::ApiError));
        assert_eq!(
            c.state(false),
            SessionState::Interrupted(InterruptReason::ApiError)
        );
        assert_eq!(c.session_end(), SessionEnd::RateLimited);
        // A later token count without the limit clears it, and so does a new turn.
        lines.push(token_count(11, Value::Null));
        assert!(!parse_lines(&lines).rate_limit_reached);
        lines.push(token_count(12, json!("primary")));
        lines.push(event(13, json!({"type": "task_started"})));
        assert!(!parse_lines(&lines).rate_limit_reached);
    }

    #[test]
    fn a_rollout_with_only_its_meta_has_no_conversation() {
        let c = parse_lines([meta("cli", "codex-tui")]);
        assert_eq!(c.summary.entry_count, 1);
        assert_eq!(c.summary.tail, None);
        assert_eq!(c.end, None);
        assert_eq!(c.last_message, None);
    }

    #[test]
    fn exec_sessions_are_told_apart() {
        assert!(parse_lines([meta("exec", "codex_exec")]).head.is_exec());
        assert!(parse_lines([meta("exec", "codex-tui")]).head.is_exec());
        assert!(!parse_lines([meta("cli", "codex-tui")]).head.is_exec());
    }

    #[test]
    fn spawned_threads_are_hidden() {
        let user = parse_lines([meta("cli", "codex-tui")]).head;
        assert!(!user.is_hidden());
        let mut sub = json!({"id": ID, "cwd": CWD, "originator": "codex-tui", "source": "cli",
            "thread_source": "subagent"});
        let head = |p: &Value| parse_lines([line(0, "session_meta", p.clone())]).head;
        assert!(head(&sub).is_spawned() && head(&sub).is_hidden());
        sub["thread_source"] = json!("user");
        assert!(!head(&sub).is_hidden());
        sub["source"] = json!({"subagent": {"thread_spawn": {"depth": 1}}});
        assert!(head(&sub).is_hidden());
        // The flags carry through a head merged into a tail.
        let merged = parse_lines([message(1, "user", "Hi")]).with_head(&head(&sub));
        assert!(merged.head.is_hidden());
    }

    #[test]
    fn a_partial_read_with_no_marker_is_working_while_live() {
        let mut parser = CodexParser::partial();
        parser.push_line(&message(1, "assistant", "Still reading the tide tables."));
        let c = parser.summary();
        assert_eq!(c.end, None);
        assert_eq!(c.state(true), SessionState::Live(LiveState::Working));
        // From the start of the rollout, the same lines are an idle session.
        let c = parse_lines([message(1, "assistant", "Done.")]);
        assert_eq!(c.state(true), SessionState::Live(LiveState::Idle));
        // Nothing read yet: unknown.
        let c = parse_lines([meta("cli", "codex-tui")]);
        assert_eq!(c.state(true), SessionState::Live(LiveState::Unknown));
        assert_eq!(
            CodexSummary::default().state(true),
            SessionState::Live(LiveState::Unknown)
        );
    }

    #[test]
    fn a_parser_fed_in_steps_matches_one_fed_at_once() {
        let mut lines = finished_turn();
        lines.push(event(9, json!({"type": "task_started"})));
        lines.push(token_count(10, json!("primary")));
        let mut parser = CodexParser::default();
        for (i, l) in lines.iter().enumerate() {
            parser.push_line(l);
            if i == 4 {
                assert_eq!(parser.summary().end, Some(TurnEnd::MidTurn));
            }
        }
        assert_eq!(parser.summary(), parse_lines(&lines));
        assert!(parser.summary().rate_limit_reached);
    }

    #[test]
    fn malformed_lines_are_counted_and_skipped() {
        let lines = [
            meta("cli", "codex-tui"),
            "{not json".into(),
            "[1]".into(),
            String::new(),
        ];
        let c = parse_lines(lines);
        assert_eq!(c.summary.entry_count, 1);
        assert_eq!(c.summary.malformed_lines, 2);
    }

    #[test]
    fn head_and_tail_slices_combine() {
        let text = finished_turn().join("\n") + "\n";
        let bytes = text.as_bytes();
        // The head read stops partway through the second line.
        let first_len = bytes.iter().position(|&b| b == b'\n').unwrap() + 1;
        let head = parse_head(&bytes[..first_len + 10]);
        assert_eq!(head.session_id.as_deref(), Some(ID));
        assert_eq!(head.cwd.as_deref(), Some(CWD));
        assert_eq!(head.source.as_deref(), Some("cli"));
        // The head read ends inside the first line: nothing yet.
        assert_eq!(parse_head(&bytes[..first_len / 2]), CodexHead::default());
        // A whole one-line file without its newline still reads.
        assert_eq!(
            parse_head(&bytes[..first_len - 1]).session_id.as_deref(),
            Some(ID)
        );
        // The tail starts mid-line, past the meta: the cut line is skipped.
        let tail = parse_tail(&bytes[first_len + 10..], false);
        assert_eq!(tail.summary.malformed_lines, 0);
        assert_eq!(tail.summary.session_id, None);
        let c = tail.with_head(&head);
        assert_eq!(c.summary.session_id.as_deref(), Some(ID));
        assert_eq!(c.summary.cwd.as_deref(), Some(CWD));
        assert_eq!(c.head, head);
        assert_eq!(c.summary.tail, Some(Tail::AssistantReply));
        // From the start of the file, the first line counts.
        assert_eq!(parse_tail(bytes, true), parse_lines(finished_turn()));
    }

    #[test]
    fn a_tail_without_turn_markers_reads_its_messages() {
        let c = parse_lines([
            message(1, "user", "Fix it"),
            message(2, "assistant", "Fixed."),
        ]);
        assert_eq!(c.end, None);
        assert_eq!(c.summary.tail, Some(Tail::AssistantReply));
        let c = parse_lines([message(1, "user", "Fix it")]);
        assert_eq!(c.summary.tail, Some(Tail::UserPrompt));
    }

    #[test]
    fn long_messages_are_capped() {
        let long = "é".repeat(MAX_MESSAGE_BYTES);
        let c = parse_lines([message(1, "assistant", &long)]);
        let kept = c.last_message.unwrap();
        assert!(kept.len() <= MAX_MESSAGE_BYTES);
        assert!(kept.ends_with('…'));
        assert_eq!(
            c.summary.last_assistant_text.unwrap().chars().count(),
            SNIPPET_MAX_CHARS
        );
    }

    #[test]
    fn session_ids_come_from_file_names() {
        assert_eq!(
            file_session_id(&format!("rollout-2026-11-08T17-36-09-{ID}.jsonl")),
            Some(ID)
        );
        for name in [
            "rollout-2026-11-08T17-36-09.jsonl",
            &format!("rollout-2026-11-08T17-36-09-{ID}.jsonl.zst"),
            &format!("{ID}.jsonl"),
            "rollout-2026-11-08T17-36-09-01a1bd4e-0000-7000-8000-00000000000g.jsonl",
        ] {
            assert_eq!(file_session_id(name), None, "{name}");
        }
    }
}
