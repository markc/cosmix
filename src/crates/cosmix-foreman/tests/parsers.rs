//! Parser-level tests against captured-shape fixture streams: raw vendor
//! lines in, normalized events + outcome out. No subprocesses, no tokens.

use cosmix_foreman::driver::claude::ClaudeParser;
use cosmix_foreman::driver::codex::CodexParser;
use cosmix_foreman::executor::{AgentEvent, StopReason, StreamParser};

/// Run-time manifest directory rather than the `env!`-baked one — see the note
/// on `manifest_dir` in `tests/harness.rs` for why the two differ when one
/// `CARGO_TARGET_DIR` is shared across git worktrees.
fn manifest_dir() -> String {
    std::env::var("CARGO_MANIFEST_DIR").unwrap_or_else(|_| env!("CARGO_MANIFEST_DIR").to_string())
}

fn feed(parser: &mut dyn StreamParser, fixture: &str) -> Vec<AgentEvent> {
    let path = format!("{}/testdata/{fixture}", manifest_dir());
    let data = std::fs::read_to_string(&path).expect("fixture readable");
    data.lines()
        .filter(|l| !l.trim().is_empty())
        .flat_map(|l| parser.parse_line(l))
        .collect()
}

#[test]
fn claude_happy_path() {
    let mut parser = Box::new(ClaudeParser::default());
    let events = feed(parser.as_mut(), "claude-ok.jsonl");

    assert!(
        matches!(&events[0], AgentEvent::Started { session_ref: Some(s) } if s == "sess-1"),
        "first event should carry the session id, got {:?}",
        events[0]
    );
    assert!(
        events
            .iter()
            .any(|e| matches!(e, AgentEvent::Text { text } if text == "Working on it."))
    );
    assert!(events.iter().any(
        |e| matches!(e, AgentEvent::ToolUse { name, detail } if name == "Bash" && detail.contains("cargo test"))
    ));
    assert!(
        events
            .iter()
            .any(|e| matches!(e, AgentEvent::ToolResult { detail } if detail == "ok")),
        "the tool_result line belongs in the event stream"
    );
    assert!(
        !events.iter().any(|e| matches!(e, AgentEvent::Raw { .. })),
        "no line in the happy fixture should fall through to Raw"
    );

    let outcome = parser.finish(None, false);
    assert_eq!(
        outcome.stop,
        StopReason::Error,
        "no exit status must not read as success"
    );

    let mut parser = Box::new(ClaudeParser::default());
    feed(parser.as_mut(), "claude-ok.jsonl");
    let outcome = parser.finish(Some(exit_status(0)), false);
    assert_eq!(outcome.stop, StopReason::Done);
    assert_eq!(outcome.result.as_deref(), Some("Done: tests pass."));
    assert_eq!(outcome.session_ref.as_deref(), Some("sess-1"));
    // The result line's totals replace the per-message accumulation.
    assert_eq!(outcome.usage.input_tokens, 42);
    assert_eq!(outcome.usage.fresh_input_tokens, Some(42));
    assert_eq!(outcome.usage.cache_read_input_tokens, None);
    assert_eq!(outcome.usage.cache_creation_input_tokens, None);
    assert_eq!(outcome.usage.output_tokens, 14);
    assert_eq!(outcome.usage.cost_usd, Some(0.0421));
}

#[test]
fn claude_exit_two_is_budget_ceiling_only_when_budgeted() {
    let mut parser = Box::new(ClaudeParser::new(true));
    feed(parser.as_mut(), "claude-budget.jsonl");
    let outcome = parser.finish(Some(exit_status(2)), false);
    assert_eq!(outcome.stop, StopReason::BudgetCeiling);
    assert!(outcome.error.is_none());
    assert_eq!(
        outcome.usage.output_tokens, 3,
        "partial usage still accounted"
    );

    // Without a budget set, the same error_max result must not read as a
    // bounceable ceiling — it is the agent's own error, kept as diagnosis.
    let mut parser = Box::new(ClaudeParser::new(false));
    feed(parser.as_mut(), "claude-budget.jsonl");
    let outcome = parser.finish(Some(exit_status(2)), false);
    assert_eq!(outcome.stop, StopReason::Error);
    assert_eq!(outcome.error.as_deref(), Some("Reached max turns."));
}

#[test]
fn claude_exit_two_without_budget_result_is_not_a_ceiling() {
    // Auth/permission failures also exit 2; on a budgeted run they emit no
    // error_max result line, and must NOT be bounced as a budget event.
    let mut parser = Box::new(ClaudeParser::new(true));
    parser.parse_line(r#"{"type":"system","subtype":"init","session_id":"s"}"#);
    let outcome = parser.finish(Some(exit_status(2)), false);
    assert_eq!(outcome.stop, StopReason::Error);
    assert!(
        outcome
            .error
            .as_deref()
            .unwrap_or("")
            .contains("blocking error")
    );
}

#[test]
fn claude_error_result_outranks_interruption() {
    // An is_error result followed by a hang + kill keeps its diagnosis.
    let mut parser = Box::new(ClaudeParser::default());
    parser.parse_line(
        r#"{"type":"result","subtype":"error_during_execution","is_error":true,"result":"tool exploded","session_id":"s"}"#,
    );
    let outcome = parser.finish(None, true);
    assert_eq!(outcome.stop, StopReason::Error);
    assert_eq!(outcome.error.as_deref(), Some("tool exploded"));
}

#[test]
fn claude_kill_after_result_line_is_done() {
    // A stall/grace kill that lands after the result already streamed is
    // finished, paid-for work — bouncing it would duplicate the spend.
    let mut parser = Box::new(ClaudeParser::default());
    feed(parser.as_mut(), "claude-ok.jsonl");
    let outcome = parser.finish(None, true);
    assert_eq!(outcome.stop, StopReason::Done);
    assert_eq!(outcome.result.as_deref(), Some("Done: tests pass."));
}

#[test]
fn claude_teardown_kill_of_background_bash_is_abandonment() {
    let mut parser = Box::new(ClaudeParser::default());
    parser.parse_line(
        r#"{"type":"system","subtype":"task_started","task_id":"bash-1","task_type":"local_bash","is_backgrounded":false}"#,
    );
    parser.parse_line(
        r#"{"type":"system","subtype":"task_updated","task_id":"bash-1","patch":{"is_backgrounded":true}}"#,
    );
    parser
        .parse_line(r#"{"type":"result","subtype":"success","is_error":false,"result":"waiting"}"#);
    parser.parse_line(
        r#"{"type":"system","subtype":"task_updated","task_id":"bash-1","patch":{"status":"killed"}}"#,
    );

    let outcome = parser.finish(Some(exit_status(0)), false);

    assert_eq!(outcome.stop, StopReason::Error);
    assert!(
        outcome
            .error
            .as_deref()
            .unwrap_or_default()
            .starts_with("agent_abandoned_background")
    );
}

#[test]
fn claude_background_snapshot_without_completion_is_abandonment() {
    let mut parser = Box::new(ClaudeParser::default());
    parser.parse_line(
        r#"{"type":"system","subtype":"background_tasks_changed","tasks":[{"task_id":"bash-2","task_type":"local_bash"}]}"#,
    );
    parser
        .parse_line(r#"{"type":"result","subtype":"success","is_error":false,"result":"waiting"}"#);

    let outcome = parser.finish(Some(exit_status(0)), false);

    assert_eq!(outcome.stop, StopReason::Error);
    assert!(
        outcome
            .error
            .as_deref()
            .unwrap_or_default()
            .contains("bash-2")
    );
}

#[test]
fn claude_killed_after_result_is_abandonment_with_legacy_start_bookend() {
    let mut parser = Box::new(ClaudeParser::default());
    parser.parse_line(
        r#"{"type":"system","subtype":"task_started","task_id":"bash-3","task_type":"local_bash"}"#,
    );
    parser
        .parse_line(r#"{"type":"result","subtype":"success","is_error":false,"result":"waiting"}"#);
    parser.parse_line(
        r#"{"type":"system","subtype":"task_updated","task_id":"bash-3","patch":{"status":"killed"}}"#,
    );

    let outcome = parser.finish(Some(exit_status(0)), false);

    assert_eq!(outcome.stop, StopReason::Error);
    assert!(
        outcome
            .error
            .as_deref()
            .unwrap_or_default()
            .contains("bash-3")
    );
}

#[test]
fn claude_missing_background_flag_is_not_assumed_live() {
    let mut parser = Box::new(ClaudeParser::default());
    parser.parse_line(
        r#"{"type":"system","subtype":"task_started","task_id":"bash-unknown","task_type":"local_bash"}"#,
    );
    parser.parse_line(r#"{"type":"result","subtype":"success","is_error":false,"result":"done"}"#);

    let outcome = parser.finish(Some(exit_status(0)), false);

    assert_eq!(outcome.stop, StopReason::Done);
    assert_eq!(outcome.error, None);
}

#[test]
fn claude_background_task_does_not_mask_an_error_result() {
    let mut parser = Box::new(ClaudeParser::default());
    parser.parse_line(
        r#"{"type":"system","subtype":"task_started","task_id":"bash-error","task_type":"local_bash","is_backgrounded":true}"#,
    );
    parser.parse_line(
        r#"{"type":"result","subtype":"error_during_execution","is_error":true,"result":"tool exploded"}"#,
    );

    let outcome = parser.finish(Some(exit_status(1)), false);

    assert_eq!(outcome.stop, StopReason::Error);
    assert_eq!(outcome.error.as_deref(), Some("tool exploded"));
}

#[test]
fn claude_success_result_with_nonzero_exit_is_error() {
    let mut parser = Box::new(ClaudeParser::default());
    feed(parser.as_mut(), "claude-ok.jsonl");
    let outcome = parser.finish(Some(exit_status(1)), false);
    assert_eq!(outcome.stop, StopReason::Error);
    assert!(
        outcome
            .error
            .as_deref()
            .unwrap_or("")
            .contains("code Some(1)")
    );
}

#[test]
fn codex_happy_path_ignores_exit_code() {
    let mut parser = Box::new(CodexParser::default());
    let events = feed(parser.as_mut(), "codex-ok.jsonl");

    assert!(matches!(&events[0], AgentEvent::Started { session_ref: Some(s) } if s == "thread-9"));
    assert!(
        events
            .iter()
            .any(|e| matches!(e, AgentEvent::Text { text } if text == "All good."))
    );
    assert!(events.iter().any(
        |e| matches!(e, AgentEvent::ToolUse { name, detail } if name == "command" && detail == "ls")
    ));
    assert!(!events.iter().any(|e| matches!(e, AgentEvent::Raw { .. })));

    // Exit codes are undocumented: a nonzero exit after turn.completed is
    // still Done — the events are ground truth.
    let outcome = parser.finish(Some(exit_status(3)), false);
    assert_eq!(outcome.stop, StopReason::Done);
    assert_eq!(outcome.result.as_deref(), Some("All good."));
    // 100 input total (cached is a subset, not additive). The old fold was
    // buggy and double-counted; the correct value is just input_tokens.
    assert_eq!(outcome.usage.input_tokens, 100);
    assert_eq!(outcome.usage.fresh_input_tokens, Some(60));
    assert_eq!(outcome.usage.cache_read_input_tokens, Some(40));
    assert_eq!(outcome.usage.output_tokens, 50);
    assert_eq!(outcome.session_ref.as_deref(), Some("thread-9"));
}

#[test]
fn codex_multi_turn_usage_is_cumulative_not_summed() {
    // turn.completed usage is cumulative session totals (openai/codex#17539):
    // two completions must replace, not add.
    let mut parser = CodexParser::default();
    parser
        .parse_line(r#"{"type":"turn.completed","usage":{"input_tokens":100,"output_tokens":50}}"#);
    parser.parse_line(r#"{"type":"turn.started"}"#);
    parser
        .parse_line(r#"{"type":"turn.completed","usage":{"input_tokens":300,"output_tokens":80}}"#);
    let outcome = Box::new(parser).finish(Some(exit_status(0)), false);
    assert_eq!(outcome.usage.input_tokens, 300);
    assert_eq!(outcome.usage.output_tokens, 80);
    assert_eq!(outcome.stop, StopReason::Done);
}

#[test]
fn codex_retry_after_failed_turn_can_succeed() {
    // turn.started clears the failure latch: a failed turn followed by a
    // successful retry is a successful session.
    let mut parser = CodexParser::default();
    parser.parse_line(r#"{"type":"turn.failed","error":{"message":"overloaded"}}"#);
    parser.parse_line(r#"{"type":"turn.started"}"#);
    parser.parse_line(r#"{"type":"turn.completed","usage":{"input_tokens":10,"output_tokens":5}}"#);
    let outcome = Box::new(parser).finish(Some(exit_status(0)), false);
    assert_eq!(outcome.stop, StopReason::Done);
}

#[test]
fn codex_truncation_inside_second_turn_is_error() {
    // turn.started reopens the run: turn 1's completion must not vouch for a
    // stream that died inside turn 2.
    let mut parser = CodexParser::default();
    parser.parse_line(r#"{"type":"turn.completed","usage":{"input_tokens":10,"output_tokens":5}}"#);
    parser.parse_line(r#"{"type":"turn.started"}"#);
    let outcome = Box::new(parser).finish(Some(exit_status(0)), false);
    assert_eq!(outcome.stop, StopReason::Error);
}

#[test]
fn codex_turn_failed_wins_over_exit_zero() {
    let mut parser = Box::new(CodexParser::default());
    feed(parser.as_mut(), "codex-failed.jsonl");
    let outcome = parser.finish(Some(exit_status(0)), false);
    assert_eq!(outcome.stop, StopReason::Error);
    assert_eq!(outcome.error.as_deref(), Some("model overloaded"));
}

#[test]
fn codex_truncated_stream_is_error() {
    let mut parser = Box::new(CodexParser::default());
    parser.parse_line(r#"{"type":"thread.started","thread_id":"t"}"#);
    let outcome = parser.finish(Some(exit_status(0)), false);
    assert_eq!(outcome.stop, StopReason::Error);
    assert!(
        outcome
            .error
            .as_deref()
            .unwrap_or("")
            .contains("without turn.completed")
    );
}

#[test]
fn unparseable_lines_surface_as_raw() {
    let mut claude = ClaudeParser::default();
    let evs = claude.parse_line("not json at all");
    assert!(matches!(&evs[..], [AgentEvent::Raw { line }] if line == "not json at all"));

    let mut codex = CodexParser::default();
    let evs = codex.parse_line(r#"{"type":"something.new","x":1}"#);
    assert!(matches!(&evs[..], [AgentEvent::Raw { .. }]));
}

#[test]
fn claude_cache_breakdown_sums_to_folded_total() {
    // Verify that cache_read + cache_creation + fresh = folded input_tokens total
    // This proves we're preserving the existing fold behaviour, not reimplementing it.
    let mut parser = ClaudeParser::default();
    parser.parse_line(
        r#"{"type":"assistant","message":{
            "content":[],
            "usage":{
                "input_tokens":1000,
                "cache_read_input_tokens":4000,
                "cache_creation_input_tokens":500,
                "output_tokens":2000
            }
        }}"#,
    );

    let outcome = Box::new(parser).finish(Some(exit_status(0)), false);

    assert_eq!(outcome.usage.fresh_input_tokens, Some(1000));
    // The folded total must equal the stored sum of all three components.
    assert_eq!(
        outcome.usage.input_tokens,
        outcome.usage.fresh_input_tokens.unwrap()
            + outcome.usage.cache_read_input_tokens.unwrap()
            + outcome.usage.cache_creation_input_tokens.unwrap(),
        "folded input_tokens must equal fresh + cache_read + cache_creation"
    );

    // Individual cache components are captured
    assert_eq!(
        outcome.usage.cache_read_input_tokens,
        Some(4000),
        "cache_read_input_tokens should be captured"
    );
    assert_eq!(
        outcome.usage.cache_creation_input_tokens,
        Some(500),
        "cache_creation_input_tokens should be captured"
    );
}

#[test]
fn codex_cache_reads_are_a_subset_of_the_folded_total() {
    // Codex input_tokens is already the complete input count. The cache-read
    // count is a subset, so fresh input is their difference.
    let mut parser = CodexParser::default();
    parser.parse_line(
        r#"{"type":"turn.completed","usage":{
            "input_tokens":16785,
            "cached_input_tokens":11008,
            "cache_write_input_tokens":700,
            "output_tokens":1500
        }}"#,
    );

    let outcome = Box::new(parser).finish(Some(exit_status(0)), false);

    assert_eq!(
        outcome.usage.input_tokens, 16785,
        "cached input must not be folded into the complete total again"
    );
    assert_eq!(
        outcome.usage.cache_read_input_tokens,
        Some(11008),
        "cached_input_tokens is the cache-read component"
    );
    assert_eq!(
        outcome.usage.cache_creation_input_tokens,
        Some(700),
        "cache_write_input_tokens is the cache-creation component"
    );
    assert_eq!(
        outcome.usage.fresh_input_tokens,
        Some(5777),
        "fresh input is total input less the cached subset"
    );
}

#[test]
fn codex_omitted_cache_write_is_unknown_not_zero() {
    // A Codex build that omits `cache_write_input_tokens` still reports the
    // cache-read count. The absent field records as unknown, not as a zero.
    let mut parser = CodexParser::default();
    parser.parse_line(
        r#"{"type":"turn.completed","usage":{
            "input_tokens":1000,
            "cached_input_tokens":300,
            "output_tokens":1500
        }}"#,
    );

    let outcome = Box::new(parser).finish(Some(exit_status(0)), false);

    assert_eq!(outcome.usage.input_tokens, 1000);
    assert_eq!(outcome.usage.fresh_input_tokens, Some(700));
    assert_eq!(outcome.usage.cache_read_input_tokens, Some(300));
    assert_eq!(
        outcome.usage.cache_creation_input_tokens, None,
        "an omitted cache_write field is unknown, not zero"
    );
}

#[test]
fn codex_genuine_zeros_are_distinguishable_from_unknown() {
    let mut parser = CodexParser::default();
    parser.parse_line(
        r#"{"type":"turn.completed","usage":{
            "input_tokens":500,
            "cached_input_tokens":0,
            "cache_write_input_tokens":0,
            "output_tokens":10
        }}"#,
    );

    let outcome = Box::new(parser).finish(Some(exit_status(0)), false);

    assert_eq!(outcome.usage.input_tokens, 500);
    assert_eq!(
        outcome.usage.cache_read_input_tokens,
        Some(0),
        "a reported zero is a genuine zero, distinguishable from an absent field"
    );
    assert_eq!(outcome.usage.cache_creation_input_tokens, Some(0));

    // Same shape, but with both cache fields absent: every component unknown.
    let mut bare = CodexParser::default();
    bare.parse_line(r#"{"type":"turn.completed","usage":{"input_tokens":500,"output_tokens":10}}"#);
    let bare = Box::new(bare).finish(Some(exit_status(0)), false);
    assert_eq!(bare.usage.input_tokens, 500);
    assert_eq!(bare.usage.cache_read_input_tokens, None);
    assert_eq!(bare.usage.cache_creation_input_tokens, None);
}

#[test]
fn omitted_cache_fields_are_unknown_but_explicit_zeros_are_known() {
    let parse = |usage: &str| {
        let mut parser = ClaudeParser::default();
        parser.parse_line(&format!(
            r#"{{"type":"assistant","message":{{"content":[],"usage":{usage}}}}}"#
        ));
        Box::new(parser).finish(Some(exit_status(0)), false).usage
    };

    let unknown = parse(r#"{"input_tokens":0,"output_tokens":0}"#);
    assert_eq!(unknown.fresh_input_tokens, Some(0));
    assert_eq!(unknown.cache_read_input_tokens, None);
    assert_eq!(unknown.cache_creation_input_tokens, None);

    let zero = parse(
        r#"{"input_tokens":0,"cache_read_input_tokens":0,"cache_creation_input_tokens":0,"output_tokens":0}"#,
    );
    assert_eq!(zero.fresh_input_tokens, Some(0));
    assert_eq!(zero.cache_read_input_tokens, Some(0));
    assert_eq!(zero.cache_creation_input_tokens, Some(0));
}

fn exit_status(code: i32) -> std::process::ExitStatus {
    use std::os::unix::process::ExitStatusExt;
    std::process::ExitStatus::from_raw(code << 8)
}
