use std::io::Cursor;

use refract_core::classify::{
    classify, FinishKind, HookStatus, InterruptReason, LiveState, SessionState,
};
use refract_core::transcript::{parse_lines, parse_reader, parse_str, SessionSummary, Tail};

const REPO: &str = "/home/dev/projects/aurora-notes";

fn fixture(name: &str) -> String {
    let path = format!("{}/tests/fixtures/{name}", env!("CARGO_MANIFEST_DIR"));
    std::fs::read_to_string(path).unwrap()
}

fn summary(name: &str) -> SessionSummary {
    parse_str(&fixture(name))
}

fn state(name: &str) -> SessionState {
    classify(&summary(name), false, None)
}

#[test]
fn clean_finish_summary() {
    let s = summary("clean_finish.jsonl");
    assert_eq!(
        s.session_id.as_deref(),
        Some("0d6c3b52-1e4a-4c55-9a11-6f2b7d8e9a01")
    );
    assert_eq!(s.cwd.as_deref(), Some(REPO));
    assert_eq!(s.entry_count, 15);
    assert_eq!(s.malformed_lines, 0);
    assert_eq!(s.byte_size, None);
    assert_eq!(
        s.last_user_prompt.as_deref(),
        Some("Add a search box to the notes list")
    );
    assert_eq!(
        s.last_assistant_text.as_deref(),
        Some("Done. The list now filters by title.")
    );
    assert_eq!(
        s.files_edited,
        vec![
            format!("{REPO}/src/NotesList.tsx"),
            format!("{REPO}/src/search.ts")
        ]
    );
    assert_eq!(
        s.first_timestamp.unwrap().to_rfc3339(),
        "2026-03-12T09:00:01.037+00:00"
    );
    assert!(s.last_timestamp > s.first_timestamp);
    assert_eq!(s.tail, Some(Tail::AssistantReply));
    assert!(!s.asks_user);
    assert_eq!(
        state("clean_finish.jsonl"),
        SessionState::Finished(FinishKind::Completed)
    );
}

#[test]
fn clean_finish_usage_counts_each_response_once() {
    let u = summary("clean_finish.jsonl").usage;
    assert_eq!(
        (
            u.input,
            u.output,
            u.cache_creation_input,
            u.cache_read_input
        ),
        (18, 100, 200, 3500)
    );
}

#[test]
fn usage_is_deduplicated_by_message_id() {
    // Response 1 spans three lines, response 2 spans two; each repeats its usage.
    let u = summary("usage_dedupe.jsonl").usage;
    assert_eq!(
        (
            u.input,
            u.output,
            u.cache_creation_input,
            u.cache_read_input
        ),
        (110, 220, 330, 440)
    );
}

#[test]
fn usage_falls_back_to_request_id() {
    let line = r#"{"type":"assistant","requestId":"req_x","message":{"usage":{"input_tokens":5,"output_tokens":1}}}"#;
    let s = parse_lines([line, line]);
    assert_eq!((s.usage.input, s.usage.output), (5, 1));
}

#[test]
fn crash_mid_tool_is_interrupted() {
    let s = summary("crash_mid_tool.jsonl");
    assert_eq!(s.tail, Some(Tail::PendingToolUse));
    assert_eq!(
        s.files_edited,
        vec![
            format!("{REPO}/src/replication/index.ts"),
            format!("{REPO}/notebooks/sync-stats.ipynb")
        ]
    );
    assert_eq!(
        state("crash_mid_tool.jsonl"),
        SessionState::Interrupted(InterruptReason::ToolCallWithoutResult)
    );
}

#[test]
fn live_session_is_live_whatever_the_tail() {
    let s = summary("crash_mid_tool.jsonl");
    assert_eq!(
        classify(&s, true, None),
        SessionState::Live(LiveState::Unknown)
    );
    assert_eq!(
        classify(&s, true, Some(HookStatus::Working)),
        SessionState::Live(LiveState::Working)
    );
    assert_eq!(
        classify(&s, true, Some(HookStatus::WaitingOnUser)),
        SessionState::Live(LiveState::WaitingOnUser)
    );
    assert_eq!(
        classify(&s, true, Some(HookStatus::Finished)),
        SessionState::Live(LiveState::Idle)
    );
    assert_eq!(
        classify(&s, true, Some(HookStatus::Ended)),
        SessionState::Live(LiveState::Unknown)
    );
}

#[test]
fn hook_status_does_not_override_a_dead_process() {
    let s = summary("crash_mid_tool.jsonl");
    assert_eq!(
        classify(&s, false, Some(HookStatus::Working)),
        SessionState::Interrupted(InterruptReason::ToolCallWithoutResult)
    );
}

#[test]
fn api_error_is_interrupted() {
    let s = summary("api_error.jsonl");
    assert_eq!(s.tail, Some(Tail::ApiError));
    assert_eq!(s.last_assistant_text, None);
    assert_eq!(
        state("api_error.jsonl"),
        SessionState::Interrupted(InterruptReason::ApiError)
    );
}

#[test]
fn unanswered_prompt_is_interrupted() {
    let s = summary("waiting_on_user.jsonl");
    assert_eq!(
        s.last_user_prompt.as_deref(),
        Some("Fix the first one and add a test")
    );
    assert_eq!(
        s.last_assistant_text.as_deref(),
        Some("There are three TODOs in src/.")
    );
    assert_eq!(
        state("waiting_on_user.jsonl"),
        SessionState::Interrupted(InterruptReason::UnansweredPrompt)
    );
}

#[test]
fn user_interrupt_counts_as_finished_by_user() {
    assert_eq!(
        state("user_interrupted.jsonl"),
        SessionState::Finished(FinishKind::UserInterrupted)
    );
}

#[test]
fn plain_interrupt_after_thinking_is_finished_by_user() {
    let lines = [
        r#"{"type":"user","message":{"role":"user","content":"Explain the cache"}}"#,
        r#"{"type":"assistant","message":{"id":"m1","content":[{"type":"thinking","thinking":"..."}]}}"#,
        r#"{"type":"user","message":{"role":"user","content":[{"type":"text","text":"[Request interrupted by user]"}]}}"#,
    ];
    let s = parse_lines(lines);
    assert_eq!(s.tail, Some(Tail::UserInterrupt));
    assert_eq!(
        classify(&s, false, None),
        SessionState::Finished(FinishKind::UserInterrupted)
    );
}

#[test]
fn thinking_without_output_leaves_prompt_unanswered() {
    let lines = [
        r#"{"type":"user","message":{"role":"user","content":"Explain the cache"}}"#,
        r#"{"type":"assistant","message":{"id":"m1","content":[{"type":"thinking","thinking":"..."}]}}"#,
    ];
    assert_eq!(parse_lines(lines).tail, Some(Tail::UserPrompt));
}

#[test]
fn tool_result_without_reply_is_interrupted() {
    let lines = [
        r#"{"type":"user","message":{"role":"user","content":"Run the tests"}}"#,
        r#"{"type":"assistant","message":{"id":"m1","content":[{"type":"tool_use","id":"t1","name":"Bash","input":{}}]}}"#,
        r#"{"type":"user","message":{"role":"user","content":[{"type":"tool_result","tool_use_id":"t1","content":"ok"}]}}"#,
    ];
    let s = parse_lines(lines);
    assert_eq!(
        classify(&s, false, None),
        SessionState::Interrupted(InterruptReason::ToolResultWithoutReply)
    );
}

#[test]
fn tool_that_ends_the_turn_is_finished() {
    let lines = [
        r#"{"type":"user","message":{"role":"user","content":"Plan it"}}"#,
        r#"{"type":"assistant","message":{"id":"m1","content":[{"type":"tool_use","id":"t1","name":"ExitPlanMode","input":{}}]}}"#,
        r#"{"type":"user","toolEndsTurn":true,"message":{"role":"user","content":[{"type":"tool_result","tool_use_id":"t1","content":"ok"}]}}"#,
    ];
    assert_eq!(
        classify(&parse_lines(lines), false, None),
        SessionState::Finished(FinishKind::Completed)
    );
}

#[test]
fn partially_answered_parallel_tool_calls_are_pending() {
    let lines = [
        r#"{"type":"user","message":{"role":"user","content":"Read both"}}"#,
        r#"{"type":"assistant","message":{"id":"m1","content":[{"type":"tool_use","id":"t1","name":"Read","input":{}}]}}"#,
        r#"{"type":"assistant","message":{"id":"m1","content":[{"type":"tool_use","id":"t2","name":"Read","input":{}}]}}"#,
        r#"{"type":"user","message":{"role":"user","content":[{"type":"tool_result","tool_use_id":"t1","content":"a"}]}}"#,
    ];
    assert_eq!(parse_lines(lines).tail, Some(Tail::PendingToolUse));
}

#[test]
fn malformed_lines_are_skipped_and_counted() {
    let s = summary("malformed_line.jsonl");
    assert_eq!(s.malformed_lines, 3);
    assert_eq!(s.entry_count, 6);
    assert_eq!(s.last_assistant_text.as_deref(), Some("Bumped to 0.3.0."));
    assert_eq!(
        state("malformed_line.jsonl"),
        SessionState::Finished(FinishKind::Completed)
    );
}

#[test]
fn trailing_noise_is_ignored() {
    let s = summary("trailing_noise.jsonl");
    assert_eq!(s.tail, Some(Tail::AssistantReply));
    assert!(!s.asks_user);
    assert_eq!(s.last_user_prompt.as_deref(), Some("Format the codebase"));
    assert_eq!(
        s.last_assistant_text.as_deref(),
        Some("Formatted 14 files.")
    );
    // Sidechain usage is real cost and is counted.
    assert_eq!((s.usage.input, s.usage.output), (6, 12));
    assert_eq!(
        state("trailing_noise.jsonl"),
        SessionState::Finished(FinishKind::Completed)
    );
}

#[test]
fn reader_matches_str_and_tolerates_invalid_utf8() {
    let text = fixture("clean_finish.jsonl");
    let from_reader = parse_reader(Cursor::new(text.as_bytes())).unwrap();
    assert_eq!(from_reader, parse_str(&text));

    let mut bytes = text.into_bytes();
    bytes.extend_from_slice(b"\xff\xfe broken\n");
    let s = parse_reader(Cursor::new(bytes)).unwrap();
    assert_eq!(s.malformed_lines, 1);
    assert_eq!(s.tail, Some(Tail::AssistantReply));
    assert!(!s.asks_user);
}

#[test]
fn tail_of_a_file_is_enough_to_classify() {
    let text = fixture("crash_mid_tool.jsonl");
    let lines: Vec<&str> = text.lines().collect();
    // Start mid-line, as a byte-offset tail read would.
    let mut tail = vec![&lines[5][10..]];
    tail.extend_from_slice(&lines[6..]);
    let s = parse_lines(tail);
    assert_eq!(s.malformed_lines, 1);
    assert_eq!(
        classify(&s, false, None),
        SessionState::Interrupted(InterruptReason::ToolCallWithoutResult)
    );
}

#[test]
fn empty_input_is_finished_and_byte_size_can_be_set() {
    let s = parse_str("").with_byte_size(0);
    assert_eq!(s.byte_size, Some(0));
    assert_eq!(s.tail, None);
    assert_eq!(
        classify(&s, false, None),
        SessionState::Finished(FinishKind::Completed)
    );
}
