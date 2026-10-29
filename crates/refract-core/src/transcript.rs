//! Parsing of Claude Code session transcripts.
//!
//! A transcript is a JSONL file at `~/.claude/projects/<encoded-cwd>/<session-id>.jsonl`.
//! Each line is one JSON object with a `type`. The conversation itself is carried by
//! `user` and `assistant` entries; everything else is bookkeeping.
//!
//! Facts about the format, checked against real transcripts (Claude Code 2.x, 2026):
//!
//! - Besides `user`, `assistant` and `system`, files contain many bookkeeping types
//!   (`attachment`, `file-history-snapshot`, `last-prompt`, `ai-title`, `mode`,
//!   `permission-mode`, `cost-state`, `queue-operation`, `api-request`, ...). They never
//!   say anything about whether a turn finished and are ignored for the tail.
//! - One API response is written as several `assistant` lines, one per content block
//!   (thinking, text, tool_use). Every line repeats the same `message.id`, the same
//!   `requestId` and an identical `message.usage`. Summing usage per line therefore
//!   over-counts; usage is deduplicated by `message.id` (falling back to `requestId`).
//! - Tool calls are `tool_use` blocks with an `id`; their results arrive in a later `user`
//!   entry as `tool_result` blocks with a matching `tool_use_id`.
//! - API errors are synthetic `assistant` entries with `"isApiErrorMessage": true`
//!   (model `<synthetic>`, the error text as a text block, zero usage).
//! - A user interrupt (Esc) is a `user` entry whose text block starts with
//!   `[Request interrupted by user` (either `]` or ` for tool use]`). Rejecting a tool call
//!   writes an error `tool_result` starting with "The user doesn't want to ...".
//! - Local slash commands and `!` shell input (`<command-name>`, `<local-command-stdout>`,
//!   `<local-command-caveat>`, `<bash-input>`, `<bash-stdout>`, ...) are written as `user`
//!   entries with string content but get no assistant reply. Many sessions end with
//!   `/exit` this way, so these count as noise. Prompt-type commands
//!   (`<command-message>`) do get a reply and count as prompts.
//! - `isMeta` user entries (skill bodies, caveats) and `isCompactSummary` entries are
//!   injected by Claude Code, not typed by the user: noise.
//! - Subagent conversations live in separate files under `<session-id>/subagents/`;
//!   `isSidechain` entries in the main file are treated as noise for the tail.
//! - A `tool_result` entry may carry `"toolEndsTurn": true`, meaning the tool itself
//!   ended the turn and no assistant reply follows.

use std::collections::HashSet;
use std::io::{self, BufRead};

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use serde_json::Value;

/// Maximum length, in characters, of the prompt and reply snippets in a summary.
pub const SNIPPET_MAX_CHARS: usize = 120;

/// Prefix of the text Claude Code writes when the user interrupts a turn.
const INTERRUPT_MARKER: &str = "[Request interrupted by user";

/// Prefix of the error result written when the user rejects a tool call.
const REJECTION_MARKER: &str = "The user doesn't want to";

/// Tags of `user` string entries that come from local commands and get no reply.
const LOCAL_COMMAND_TAGS: &[&str] = &[
    "<command-name>",
    "<local-command-stdout>",
    "<local-command-stderr>",
    "<local-command-caveat>",
    "<bash-input>",
    "<bash-stdout>",
    "<bash-stderr>",
];

/// Token usage summed over a session's API responses.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct TokenUsage {
    pub input: u64,
    pub output: u64,
    pub cache_creation_input: u64,
    pub cache_read_input: u64,
}

impl TokenUsage {
    fn add(&mut self, usage: &Value) {
        let get = |key: &str| usage.get(key).and_then(Value::as_u64).unwrap_or(0);
        self.input += get("input_tokens");
        self.output += get("output_tokens");
        self.cache_creation_input += get("cache_creation_input_tokens");
        self.cache_read_input += get("cache_read_input_tokens");
    }
}

/// The last meaningful thing that happened in a transcript, ignoring noise.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum Tail {
    /// The user sent a prompt and nothing answered it.
    UserPrompt,
    /// The assistant called a tool and at least one call has no result.
    PendingToolUse,
    /// All tool calls have results but the assistant never continued.
    UnansweredToolResult,
    /// The tool itself ended the turn (`toolEndsTurn`).
    ToolEndedTurn,
    /// The assistant replied with text.
    AssistantReply,
    /// The user interrupted the turn or rejected a tool call.
    UserInterrupt,
    /// The last response was an API error.
    ApiError,
}

/// What Refract needs to know about one session transcript.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct SessionSummary {
    pub session_id: Option<String>,
    /// The first `cwd` in the parsed lines. For a tail that may be a directory Claude
    /// moved into later, not where the session started.
    pub cwd: Option<String>,
    pub first_timestamp: Option<DateTime<Utc>>,
    pub last_timestamp: Option<DateTime<Utc>>,
    /// Lines that parsed as JSON objects.
    pub entry_count: usize,
    /// Non-empty lines that were not JSON objects and were skipped.
    pub malformed_lines: usize,
    /// Transcript size in bytes, when the caller knows it.
    pub byte_size: Option<u64>,
    /// When the transcript file was last written, when the caller knows it.
    pub modified: Option<DateTime<Utc>>,
    /// First line of the last prompt the user typed, truncated.
    pub last_user_prompt: Option<String>,
    /// First line of the assistant's last text reply, truncated.
    pub last_assistant_text: Option<String>,
    /// Paths written by Edit, Write, MultiEdit and NotebookEdit, in first-edit order.
    pub files_edited: Vec<String>,
    pub usage: TokenUsage,
    /// Last meaningful event; `None` when the transcript has no conversation.
    pub tail: Option<Tail>,
}

impl SessionSummary {
    /// Returns the summary with `byte_size` set.
    pub fn with_byte_size(mut self, bytes: u64) -> Self {
        self.byte_size = Some(bytes);
        self
    }
}

/// Parses a whole transcript held in memory.
pub fn parse_str(text: &str) -> SessionSummary {
    parse_lines(text.lines())
}

/// Parses a transcript from a reader. Fails only on I/O errors; invalid UTF-8 lines
/// count as malformed.
pub fn parse_reader<R: BufRead>(mut reader: R) -> io::Result<SessionSummary> {
    let mut parser = Parser::default();
    let mut buf = Vec::new();
    loop {
        buf.clear();
        if reader.read_until(b'\n', &mut buf)? == 0 {
            break;
        }
        match std::str::from_utf8(&buf) {
            Ok(line) => parser.push_line(line),
            Err(_) => parser.summary.malformed_lines += 1,
        }
    }
    Ok(parser.finish())
}

/// Parses a sequence of lines, such as the tail of a transcript. A partial first line
/// is simply counted as malformed.
pub fn parse_lines<I, S>(lines: I) -> SessionSummary
where
    I: IntoIterator<Item = S>,
    S: AsRef<str>,
{
    let mut parser = Parser::default();
    for line in lines {
        parser.push_line(line.as_ref());
    }
    parser.finish()
}

#[derive(Default)]
struct Parser {
    summary: SessionSummary,
    seen_usage: HashSet<String>,
    pending_tools: HashSet<String>,
}

impl Parser {
    fn push_line(&mut self, line: &str) {
        let line = line.trim();
        if line.is_empty() {
            return;
        }
        match serde_json::from_str::<Value>(line) {
            Ok(entry) if entry.is_object() => self.push_entry(&entry),
            _ => self.summary.malformed_lines += 1,
        }
    }

    fn push_entry(&mut self, entry: &Value) {
        let s = &mut self.summary;
        s.entry_count += 1;
        if s.session_id.is_none() {
            s.session_id = str_field(entry, "sessionId").map(str::to_owned);
        }
        if s.cwd.is_none() {
            s.cwd = str_field(entry, "cwd").map(str::to_owned);
        }
        if let Some(ts) = str_field(entry, "timestamp").and_then(parse_timestamp) {
            if s.first_timestamp.is_none_or(|first| ts < first) {
                s.first_timestamp = Some(ts);
            }
            if s.last_timestamp.is_none_or(|last| ts > last) {
                s.last_timestamp = Some(ts);
            }
        }

        let message = entry.get("message");
        match str_field(entry, "type") {
            Some("assistant") => self.push_assistant(entry, message),
            Some("user") => self.push_user(entry, message),
            _ => {}
        }
    }

    fn push_assistant(&mut self, entry: &Value, message: Option<&Value>) {
        let Some(message) = message else { return };
        let blocks = content_blocks(message);

        // Edits and usage count even in sidechains: they are real work and real cost.
        for block in &blocks {
            if block_type(block) == Some("tool_use") {
                if let Some(path) = edited_path(block) {
                    if !self.summary.files_edited.iter().any(|p| p == path) {
                        self.summary.files_edited.push(path.to_owned());
                    }
                }
            }
        }
        if let Some(usage) = message.get("usage") {
            let key = str_field(message, "id").or_else(|| str_field(entry, "requestId"));
            let fresh = match key {
                Some(key) => self.seen_usage.insert(key.to_owned()),
                None => true,
            };
            if fresh {
                self.summary.usage.add(usage);
            }
        }

        if is_sidechain(entry) {
            return;
        }
        if bool_field(entry, "isApiErrorMessage") {
            self.pending_tools.clear();
            self.summary.tail = Some(Tail::ApiError);
            return;
        }
        let mut replied = false;
        for block in &blocks {
            match block_type(block) {
                Some("tool_use") => {
                    if let Some(id) = str_field(block, "id") {
                        self.pending_tools.insert(id.to_owned());
                    }
                }
                Some("text") => {
                    if let Some(text) = str_field(block, "text").and_then(snippet) {
                        self.summary.last_assistant_text = Some(text);
                        replied = true;
                    }
                }
                _ => {}
            }
        }
        // A thinking-only line is not a reply yet and leaves the tail as it was.
        if !self.pending_tools.is_empty() {
            self.summary.tail = Some(Tail::PendingToolUse);
        } else if replied {
            self.summary.tail = Some(Tail::AssistantReply);
        }
    }

    fn push_user(&mut self, entry: &Value, message: Option<&Value>) {
        if is_sidechain(entry)
            || bool_field(entry, "isMeta")
            || bool_field(entry, "isCompactSummary")
        {
            return;
        }
        let Some(message) = message else { return };

        if let Some(text) = message.get("content").and_then(Value::as_str) {
            self.push_prompt_text(text);
            return;
        }

        let blocks = content_blocks(message);
        let mut had_result = false;
        let mut rejected = false;
        let mut prompt: Option<&str> = None;
        for block in &blocks {
            match block_type(block) {
                Some("tool_result") => {
                    had_result = true;
                    if let Some(id) = str_field(block, "tool_use_id") {
                        self.pending_tools.remove(id);
                    }
                    if bool_field(block, "is_error")
                        && result_text(block).is_some_and(|t| t.starts_with(REJECTION_MARKER))
                    {
                        rejected = true;
                    }
                }
                Some("text") => {
                    let text = str_field(block, "text").unwrap_or("");
                    if text.starts_with(INTERRUPT_MARKER) {
                        rejected = true;
                    } else if prompt.is_none() {
                        prompt = Some(text);
                    }
                }
                Some("image") => prompt = prompt.or(Some("")),
                _ => {}
            }
        }

        if rejected {
            self.pending_tools.clear();
            self.summary.tail = Some(Tail::UserInterrupt);
        } else if let Some(text) = prompt.filter(|_| !had_result) {
            self.push_prompt_text(text);
        } else if had_result {
            self.summary.tail = Some(if !self.pending_tools.is_empty() {
                Tail::PendingToolUse
            } else if bool_field(entry, "toolEndsTurn") {
                Tail::ToolEndedTurn
            } else {
                Tail::UnansweredToolResult
            });
        }
    }

    fn push_prompt_text(&mut self, text: &str) {
        let trimmed = text.trim_start();
        if LOCAL_COMMAND_TAGS
            .iter()
            .any(|tag| trimmed.starts_with(tag))
        {
            return;
        }
        if trimmed.starts_with(INTERRUPT_MARKER) {
            self.pending_tools.clear();
            self.summary.tail = Some(Tail::UserInterrupt);
            return;
        }
        // Tagged prompts (`<command-message>`, `<task-notification>`) need a reply but
        // make poor snippets, so only plain text updates the displayed prompt.
        if !trimmed.starts_with('<') {
            if let Some(line) = snippet(trimmed) {
                self.summary.last_user_prompt = Some(line);
            }
        }
        self.pending_tools.clear();
        self.summary.tail = Some(Tail::UserPrompt);
    }

    fn finish(self) -> SessionSummary {
        self.summary
    }
}

fn str_field<'a>(value: &'a Value, key: &str) -> Option<&'a str> {
    value.get(key).and_then(Value::as_str)
}

fn bool_field(value: &Value, key: &str) -> bool {
    value.get(key).and_then(Value::as_bool).unwrap_or(false)
}

fn is_sidechain(entry: &Value) -> bool {
    bool_field(entry, "isSidechain")
}

fn block_type(block: &Value) -> Option<&str> {
    str_field(block, "type")
}

fn content_blocks(message: &Value) -> Vec<&Value> {
    match message.get("content") {
        Some(Value::Array(blocks)) => blocks.iter().filter(|b| b.is_object()).collect(),
        _ => Vec::new(),
    }
}

/// The text of a `tool_result`, whose content is a string or a list of text blocks.
fn result_text(block: &Value) -> Option<&str> {
    match block.get("content")? {
        Value::String(text) => Some(text),
        Value::Array(parts) => parts.iter().find_map(|p| str_field(p, "text")),
        _ => None,
    }
}

fn edited_path(block: &Value) -> Option<&str> {
    let input = block.get("input")?;
    match str_field(block, "name")? {
        "Edit" | "Write" | "MultiEdit" => str_field(input, "file_path"),
        "NotebookEdit" => str_field(input, "notebook_path"),
        _ => None,
    }
}

fn parse_timestamp(text: &str) -> Option<DateTime<Utc>> {
    DateTime::parse_from_rfc3339(text)
        .ok()
        .map(|ts| ts.with_timezone(&Utc))
}

/// First non-empty line of `text`, truncated to [`SNIPPET_MAX_CHARS`].
fn snippet(text: &str) -> Option<String> {
    let line = text.lines().map(str::trim).find(|l| !l.is_empty())?;
    if line.chars().count() <= SNIPPET_MAX_CHARS {
        return Some(line.to_owned());
    }
    let mut cut: String = line.chars().take(SNIPPET_MAX_CHARS - 1).collect();
    cut.push('…');
    Some(cut)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn snippet_takes_first_non_empty_line() {
        assert_eq!(snippet("\n  hello  \nworld"), Some("hello".to_owned()));
        assert_eq!(snippet("  \n "), None);
    }

    #[test]
    fn snippet_truncates_long_lines() {
        let long = "x".repeat(SNIPPET_MAX_CHARS + 10);
        let cut = snippet(&long).unwrap();
        assert_eq!(cut.chars().count(), SNIPPET_MAX_CHARS);
        assert!(cut.ends_with('…'));
    }
}
