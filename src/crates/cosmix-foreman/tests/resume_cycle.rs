//! The reject → fix → re-review cycle on a fixture: one reviewer thread.
//!
//! Task 70's acceptance asks for a cycle proving a re-review continues the
//! SAME reviewer session rather than opening a cold one. This drives the
//! real `review_landing` against a fake vendor CLI that records its argv, so
//! the assertions are about what foreman actually invoked, not about what a
//! helper returned.
//!
//! The implementer half of the same acceptance clause — one implementer
//! session across a same-rung retry, with the findings riding as the next
//! turn — is proven by
//! `runner::tests::same_rung_retry_resumes_the_prior_session_with_the_finding_as_the_turn`,
//! which needs the runner's in-crate test ledger and verifier plumbing.
//!
//! The clause's ">=80% measured drop" is the third test here, so the figure
//! quoted in the CHANGELOG is reproducible by running this file rather than
//! taken on trust.

use std::os::unix::fs::PermissionsExt;
use std::path::Path;
use std::process::Command;

use cosmix_foreman::executor::{AgentKind, ResumeFailure, RunOutcome, StopReason, Usage};
use cosmix_foreman::ledger::Ledger;
use cosmix_foreman::review::{ReviewConfig, review_landing};

/// Every test here drives the fake CLI through PROCESS-wide environment
/// variables, so they must not overlap: cargo runs a test binary's cases on
/// threads of one process.
static ENV_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

fn git(repo: &Path, args: &[&str]) -> String {
    let out = Command::new("git")
        .args(args)
        .current_dir(repo)
        .env("GIT_AUTHOR_NAME", "t")
        .env("GIT_AUTHOR_EMAIL", "t@example.com")
        .env("GIT_COMMITTER_NAME", "t")
        .env("GIT_COMMITTER_EMAIL", "t@example.com")
        .output()
        .expect("git runs");
    assert!(out.status.success(), "git {args:?}: {out:?}");
    String::from_utf8(out.stdout).unwrap().trim().to_string()
}

/// A fake `claude` that appends its argv (NUL-joined) to `$ARGV_LOG` as one
/// record per invocation, then emits the stream named by `$FAKE_STREAM`.
fn fake_claude(dir: &Path) -> std::path::PathBuf {
    let script = dir.join("fake-claude");
    std::fs::write(
        &script,
        "#!/bin/sh\nprintf '%s\\0' \"$@\" >> \"$ARGV_LOG\"\nprintf '\\36' >> \"$ARGV_LOG\"\n\
         cat \"$FAKE_STREAM\"\n",
    )
    .unwrap();
    std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).unwrap();
    script
}

/// A review fixture that derives its reported input total from the bytes it
/// actually ingests: the real `-p` payload plus the Git-object reads the
/// review policy requires for this round.
fn billing_fake_claude(dir: &Path) -> std::path::PathBuf {
    let script = dir.join("billing-fake-claude");
    std::fs::write(
        &script,
        r#"#!/bin/sh
printf '%s\0' "$@" >> "$ARGV_LOG"
printf '\36' >> "$ARGV_LOG"
prompt=$2
bytes=$(printf %s "$prompt" | wc -c)
paths=$(printf '%s\n' "$prompt" | sed -n 's/^[[:space:]]*"path": "\([^"]*\)",*$/\1/p')
if printf '%s\n' "$prompt" | grep -Fq 'Do not re-read a path whose content is unchanged since your last inspection.'; then
    for path in $paths; do
        previous_blob=$(git rev-parse "$FIRST_TIP:$path")
        current_blob=$(git rev-parse "$CURRENT_TIP:$path")
        if [ "$previous_blob" != "$current_blob" ]; then
            base_bytes=$(git show "$BASE_REV:$path" | wc -c)
            tip_bytes=$(git show "$CURRENT_TIP:$path" | wc -c)
            bytes=$((bytes + base_bytes + tip_bytes))
        fi
    done
elif printf '%s\n' "$prompt" | grep -Fq 'Inspect EVERY indexed path'; then
    for path in $paths; do
        base_bytes=$(git show "$BASE_REV:$path" | wc -c)
        tip_bytes=$(git show "$CURRENT_TIP:$path" | wc -c)
        bytes=$((bytes + base_bytes + tip_bytes))
    done
elif printf '%s\n' "$prompt" | grep -Fq 'Re-read every indexed path from both revisions.'; then
    for path in $paths; do
        base_bytes=$(git show "$BASE_REV:$path" | wc -c)
        tip_bytes=$(git show "$CURRENT_TIP:$path" | wc -c)
        bytes=$((bytes + base_bytes + tip_bytes))
    done
else
    echo 'review prompt has no recognised read policy' >&2
    exit 64
fi
printf '{"type":"system","subtype":"init","session_id":"%s","model":"m","cwd":"/tmp"}\n' "$FAKE_SESSION"
printf '{"type":"assistant","message":{"usage":{"input_tokens":%s,"output_tokens":1},"content":[]}}\n' "$bytes"
printf '{"type":"result","subtype":"success","is_error":false,"duration_ms":10,"num_turns":1,"result":"Reviewed.\\n{\\"verdict\\":\\"%s\\",\\"findings\\":[],\\"files_inspected\\":[\\"src/review.rs\\",\\"src/runner.rs\\",\\"src/refinery/cargo.rs\\",\\"src/refinery/errors.rs\\",\\"src/refinery/land.rs\\",\\"src/refinery/manifest_base.rs\\",\\"src/refinery/manifest_live.rs\\",\\"src/refinery/mod.rs\\",\\"src/refinery/preflight.rs\\",\\"src/refinery/rebase.rs\\",\\"src/refinery/recovery.rs\\",\\"src/refinery/reviews.rs\\",\\"src/refinery/version.rs\\",\\"src/refinery/version_fs.rs\\",\\"src/refinery/worktree.rs\\",\\"src/driver/claude.rs\\",\\"src/driver/codex.rs\\"]}","session_id":"%s","usage":{"input_tokens":%s,"output_tokens":40}}\n' "$FAKE_VERDICT" "$FAKE_SESSION" "$bytes"
"#,
    )
    .unwrap();
    std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).unwrap();
    script
}

/// One stream-json review session: a verdict with full inspection coverage,
/// under `session_id`.
fn review_stream(session_id: &str, verdict: &str, inspected: &[&str]) -> String {
    review_stream_with_ids(session_id, session_id, verdict, inspected)
}

fn review_stream_with_ids(
    init_session_id: &str,
    terminal_session_id: &str,
    verdict: &str,
    inspected: &[&str],
) -> String {
    let files = inspected
        .iter()
        .map(|p| format!("\"{p}\""))
        .collect::<Vec<_>>()
        .join(",");
    let reply = format!(
        "Reviewed.\n{{\"verdict\":\"{verdict}\",\"findings\":[],\"files_inspected\":[{files}]}}"
    );
    let reply = serde_json::to_string(&reply).unwrap();
    format!(
        "{{\"type\":\"system\",\"subtype\":\"init\",\"session_id\":\"{init_session_id}\",\"model\":\"m\",\"cwd\":\"/tmp\"}}\n\
         {{\"type\":\"result\",\"subtype\":\"success\",\"is_error\":false,\"duration_ms\":10,\"num_turns\":1,\
         \"result\":{reply},\"session_id\":\"{terminal_session_id}\",\"usage\":{{\"input_tokens\":900,\"output_tokens\":40}}}}\n"
    )
}

fn review_stream_without_ids(verdict: &str, inspected: &[&str]) -> String {
    let files = inspected
        .iter()
        .map(|p| format!("\"{p}\""))
        .collect::<Vec<_>>()
        .join(",");
    let reply = format!(
        "Reviewed.\n{{\"verdict\":\"{verdict}\",\"findings\":[],\"files_inspected\":[{files}]}}"
    );
    let reply = serde_json::to_string(&reply).unwrap();
    format!(
        "{{\"type\":\"system\",\"subtype\":\"init\",\"model\":\"m\",\"cwd\":\"/tmp\"}}\n\
         {{\"type\":\"result\",\"subtype\":\"success\",\"is_error\":false,\"duration_ms\":10,\"num_turns\":1,\
         \"result\":{reply},\"usage\":{{\"input_tokens\":900,\"output_tokens\":40}}}}\n"
    )
}

fn argv_records(log: &Path) -> Vec<Vec<String>> {
    std::fs::read(log)
        .unwrap_or_default()
        .split(|&b| b == 0x1e)
        .filter(|record| !record.is_empty())
        .map(|record| {
            record
                .split(|&b| b == 0)
                .filter(|f| !f.is_empty())
                .map(|f| String::from_utf8_lossy(f).into_owned())
                .collect()
        })
        .collect()
}

fn resume_ids(argv: &[String]) -> Vec<String> {
    argv.windows(2)
        .filter(|pair| pair[0] == "--resume")
        .map(|pair| pair[1].clone())
        .collect()
}

fn prompt_of(argv: &[String]) -> String {
    let at = argv.iter().position(|a| a == "-p").expect("-p in argv");
    argv[at + 1].clone()
}

fn reported_input_tokens(stdout: &[u8]) -> u64 {
    String::from_utf8_lossy(stdout)
        .lines()
        .filter_map(|line| serde_json::from_str::<serde_json::Value>(line).ok())
        .find(|event| event.get("type").and_then(serde_json::Value::as_str) == Some("result"))
        .and_then(|event| {
            event
                .pointer("/usage/input_tokens")
                .and_then(serde_json::Value::as_u64)
        })
        .expect("fake reviewer emitted terminal input usage")
}

#[test]
fn rereview_billing_counts_base_and_tip_for_each_changed_path() {
    let tmp = tempfile::tempdir().unwrap();
    let repo = tmp.path().join("wt");
    std::fs::create_dir(&repo).unwrap();
    git(&repo, &["init", "-q", "-b", "main"]);
    let path = repo.join("a.rs");
    let base_content = "base version\n";
    std::fs::write(&path, base_content).unwrap();
    git(&repo, &["add", "."]);
    git(&repo, &["commit", "-qm", "base"]);
    let base = git(&repo, &["rev-parse", "HEAD"]);

    std::fs::write(&path, "first review version\n").unwrap();
    git(&repo, &["add", "."]);
    git(&repo, &["commit", "-qm", "first review"]);
    let first_tip = git(&repo, &["rev-parse", "HEAD"]);

    let current_content = "fixed version\n";
    std::fs::write(&path, current_content).unwrap();
    git(&repo, &["add", "."]);
    git(&repo, &["commit", "-qm", "fix"]);
    let current_tip = git(&repo, &["rev-parse", "HEAD"]);

    let prompt = "Do not re-read a path whose content is unchanged since your last inspection.\n\
                  \"path\": \"a.rs\",\n";
    let output = Command::new(billing_fake_claude(tmp.path()))
        .args(["-p", prompt])
        .current_dir(&repo)
        .env("ARGV_LOG", tmp.path().join("argv.log"))
        .env("BASE_REV", &base)
        .env("FIRST_TIP", &first_tip)
        .env("CURRENT_TIP", &current_tip)
        .env("FAKE_SESSION", "thread-billing")
        .env("FAKE_VERDICT", "APPROVE")
        .output()
        .unwrap();
    assert!(output.status.success(), "{output:?}");
    assert_eq!(
        reported_input_tokens(&output.stdout),
        (prompt.len() + base_content.len() + current_content.len()) as u64,
        "a changed path requires both Git-object reads named by the re-review turn"
    );
}

#[test]
fn reject_fix_rereview_uses_one_reviewer_thread() {
    let _guard = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let tmp = tempfile::tempdir().unwrap();
    let repo = tmp.path().join("wt");
    std::fs::create_dir(&repo).unwrap();
    git(&repo, &["init", "-q", "-b", "main"]);
    std::fs::write(repo.join("a.rs"), "fn a() {}\n").unwrap();
    git(&repo, &["add", "."]);
    git(&repo, &["commit", "-qm", "base"]);
    let base = git(&repo, &["rev-parse", "HEAD"]);

    std::fs::write(repo.join("a.rs"), "fn a() { todo!() }\n").unwrap();
    git(&repo, &["add", "."]);
    git(&repo, &["commit", "-qm", "work"]);
    let first_tip = git(&repo, &["rev-parse", "HEAD"]);

    let ledger = Ledger::open(&tmp.path().join("ledger.db")).unwrap();
    let task_id = ledger
        .add_task("resume cycle", "spec", "impl", "high", &[], "none")
        .unwrap();
    let task = ledger.task(task_id).unwrap().unwrap();
    let profile = cosmix_foreman::verify::lookup_profile("none").unwrap();
    let bin = fake_claude(tmp.path());
    let argv_log = tmp.path().join("argv.log");
    let stream = tmp.path().join("stream.jsonl");
    unsafe {
        std::env::set_var("ARGV_LOG", &argv_log);
        std::env::set_var("FAKE_STREAM", &stream);
        std::env::set_var("FOREMAN_QUIET", "1");
    }

    let config = |tip: &str, resume: Option<&str>| ReviewConfig {
        base: &base,
        tip: Box::leak(tip.to_string().into_boxed_str()),
        touches_foreman: true,
        reviewer: AgentKind::Claude,
        model: "opus",
        claude_bin: bin.to_str().unwrap(),
        codex_bin: "codex",
        sibling_repos: None,
        reserve_usd: 5.0,
        reserve_tokens: 100_000,
        stall_secs: 60,
        verify_subdir: ".",
        profile: &profile,
        project_pack: "",
        resume_session_ref: resume.map(|r| Box::leak(r.to_string().into_boxed_str()) as &str),
    };

    // Round 1: cold review, REJECT.
    std::fs::write(&stream, review_stream("thread-A", "REJECT", &["a.rs"])).unwrap();
    let run1 = ledger
        .start_review_run(task_id, AgentKind::Claude, Some("opus"))
        .unwrap();
    let cold = review_landing(&ledger, run1, &repo, &task, config(&first_tip, None)).unwrap();
    assert!(!cold.approve, "{}", cold.report);
    assert_eq!(cold.session_ref.as_deref(), Some("thread-A"));

    // The fix lands.
    std::fs::write(repo.join("a.rs"), "fn a() -> u8 { 1 }\n").unwrap();
    git(&repo, &["add", "."]);
    git(&repo, &["commit", "-qm", "fix"]);
    let fixed_tip = git(&repo, &["rev-parse", "HEAD"]);

    // Round 2: the SAME thread is resumed and re-judges.
    std::fs::write(&stream, review_stream("thread-A", "APPROVE", &["a.rs"])).unwrap();
    let run2 = ledger
        .start_review_run(task_id, AgentKind::Claude, Some("opus"))
        .unwrap();
    let resumed = review_landing(
        &ledger,
        run2,
        &repo,
        &task,
        config(&fixed_tip, cold.session_ref.as_deref()),
    )
    .unwrap();
    assert!(resumed.approve, "{}", resumed.report);

    let records = argv_records(&argv_log);
    assert_eq!(records.len(), 2, "exactly two reviewer invocations");

    // Round 1 opened a conversation; round 2 continued THAT one.
    assert!(
        resume_ids(&records[0]).is_empty(),
        "the first review must be cold: {:?}",
        records[0]
    );
    assert_eq!(
        resume_ids(&records[1]),
        vec!["thread-A"],
        "the re-review must resume the recorded thread exactly once: {:?}",
        records[1]
    );

    // ... and it re-sent the re-review turn, not the whole cold prompt.
    let cold_prompt = prompt_of(&records[0]);
    let turn = prompt_of(&records[1]);
    assert!(cold_prompt.contains("You are the merge-authority reviewer"));
    assert!(cold_prompt.contains("HARNESS INVARIANT CHECKLIST"));
    assert!(turn.contains(&format!("Fixes for this task landed at tip {fixed_tip}")));
    assert!(
        turn.contains("\"path\": \"a.rs\""),
        "the resumed arm must still be shown the current index: {turn}"
    );
    assert!(
        turn.contains("HARNESS INVARIANT CHECKLIST"),
        "a current Foreman diff must repeat the checklist: {turn}"
    );
    assert!(
        turn.len() < cold_prompt.len(),
        "cold {} bytes vs resumed turn {} bytes",
        cold_prompt.len(),
        turn.len()
    );
}

#[test]
fn reviewer_resume_intent_survives_crash_before_started() {
    let tmp = tempfile::tempdir().unwrap();
    let repo = tmp.path().join("wt");
    std::fs::create_dir(&repo).unwrap();
    git(&repo, &["init", "-q", "-b", "main"]);
    std::fs::write(repo.join("a.rs"), "fn a() {}\n").unwrap();
    git(&repo, &["add", "."]);
    git(&repo, &["commit", "-qm", "base"]);
    let base = git(&repo, &["rev-parse", "HEAD"]);
    std::fs::write(repo.join("a.rs"), "fn a() -> u8 { 1 }\n").unwrap();
    git(&repo, &["add", "."]);
    git(&repo, &["commit", "-qm", "work"]);
    let tip = git(&repo, &["rev-parse", "HEAD"]);

    let ledger = Ledger::open(&tmp.path().join("ledger.db")).unwrap();
    let task_id = ledger
        .add_task("review spawn crash", "spec", "impl", "high", &[], "none")
        .unwrap();
    let task = ledger.task(task_id).unwrap().unwrap();
    let prior = ledger
        .start_review_run(task_id, AgentKind::Claude, Some("opus"))
        .unwrap();
    ledger
        .finish_run(
            prior,
            &RunOutcome {
                stop: StopReason::Done,
                result: Some("rejected".into()),
                error: None,
                usage: Usage::default(),
                session_ref: Some("review-thread-before-crash".into()),
                terminal_session_ref: Some("review-thread-before-crash".into()),
                usage_observed: true,
                output_observed: true,
                resume_failure: None,
            },
            1,
        )
        .unwrap();
    let current = ledger
        .start_review_run(task_id, AgentKind::Claude, Some("opus"))
        .unwrap();
    let profile = cosmix_foreman::verify::lookup_profile("none").unwrap();
    let missing = tmp.path().join("missing-claude");
    let error = review_landing(
        &ledger,
        current,
        &repo,
        &task,
        ReviewConfig {
            base: &base,
            tip: &tip,
            touches_foreman: false,
            reviewer: AgentKind::Claude,
            model: "opus",
            claude_bin: missing.to_str().unwrap(),
            codex_bin: "codex",
            sibling_repos: None,
            reserve_usd: 5.0,
            reserve_tokens: 100_000,
            stall_secs: 60,
            verify_subdir: ".",
            profile: &profile,
            project_pack: "",
            resume_session_ref: Some("review-thread-before-crash"),
        },
    )
    .unwrap_err();
    assert!(error.to_string().contains("starting Claude review session"));

    let next = ledger
        .start_review_run(task_id, AgentKind::Claude, Some("opus"))
        .unwrap();
    let resumable = ledger
        .last_run_ref(task_id, "review", Some("claude"), next)
        .unwrap()
        .unwrap();
    assert_eq!(resumable.id, current);
    assert_eq!(
        resumable.session_ref.as_deref(),
        Some("review-thread-before-crash")
    );
}

/// A recorded session the vendor has since pruned SPAWNS fine and then
/// errors, so the failure cannot be caught at spawn time. The arm must fall
/// back to a full fresh review rather than fail closed into a rejection that
/// repeats every sweep.
#[test]
fn a_dead_reviewer_session_falls_back_to_a_fresh_review() {
    let _guard = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let tmp = tempfile::tempdir().unwrap();
    let repo = tmp.path().join("wt");
    std::fs::create_dir(&repo).unwrap();
    git(&repo, &["init", "-q", "-b", "main"]);
    std::fs::write(repo.join("a.rs"), "fn a() {}\n").unwrap();
    git(&repo, &["add", "."]);
    git(&repo, &["commit", "-qm", "base"]);
    let base = git(&repo, &["rev-parse", "HEAD"]);
    std::fs::write(repo.join("a.rs"), "fn a() -> u8 { 1 }\n").unwrap();
    git(&repo, &["add", "."]);
    git(&repo, &["commit", "-qm", "work"]);
    let tip = git(&repo, &["rev-parse", "HEAD"]);

    let ledger = Ledger::open(&tmp.path().join("ledger.db")).unwrap();
    let task_id = ledger
        .add_task("resume cycle", "spec", "impl", "high", &[], "none")
        .unwrap();
    let task = ledger.task(task_id).unwrap().unwrap();
    let profile = cosmix_foreman::verify::lookup_profile("none").unwrap();
    let prior_run = ledger
        .start_review_run(task_id, AgentKind::Claude, Some("opus"))
        .unwrap();
    ledger
        .finish_run(
            prior_run,
            &RunOutcome {
                stop: StopReason::Error,
                result: None,
                error: Some("old review failed".into()),
                usage: Usage::default(),
                session_ref: Some("dead".into()),
                terminal_session_ref: None,
                usage_observed: false,
                output_observed: false,
                resume_failure: None,
            },
            1,
        )
        .unwrap();

    // Fails on the FIRST call (the resume), succeeds on the second (fresh).
    let script = tmp.path().join("fake-claude");
    let argv_log = tmp.path().join("argv.log");
    let ok = tmp.path().join("ok.jsonl");
    std::fs::write(&ok, review_stream("thread-B", "APPROVE", &["a.rs"])).unwrap();
    std::fs::write(
        &script,
        format!(
            "#!/bin/sh\nprintf '%s\\0' \"$@\" >> \"$ARGV_LOG\"\nprintf '\\36' >> \"$ARGV_LOG\"\n\
             if [ -f \"$TMP/used\" ]; then cat '{ok}'; else : > \"$TMP/used\"; \
             echo 'No conversation found with session ID: dead' >&2; exit 1; fi\n",
            ok = ok.display()
        ),
    )
    .unwrap();
    std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).unwrap();
    unsafe {
        std::env::set_var("ARGV_LOG", &argv_log);
        std::env::set_var("TMP", tmp.path());
        std::env::set_var("FOREMAN_QUIET", "1");
    }

    let current_run = ledger
        .start_review_run(task_id, AgentKind::Claude, Some("opus"))
        .unwrap();
    let outcome = review_landing(
        &ledger,
        current_run,
        &repo,
        &task,
        ReviewConfig {
            base: &base,
            tip: &tip,
            touches_foreman: true,
            reviewer: AgentKind::Claude,
            model: "opus",
            claude_bin: script.to_str().unwrap(),
            codex_bin: "codex",
            sibling_repos: None,
            reserve_usd: 5.0,
            reserve_tokens: 100_000,
            stall_secs: 60,
            verify_subdir: ".",
            profile: &profile,
            project_pack: "",
            resume_session_ref: Some("dead"),
        },
    )
    .unwrap();

    assert!(
        outcome.approve,
        "the fresh fallback review must be the verdict: {}",
        outcome.report
    );
    let records = argv_records(&argv_log);
    assert_eq!(records.len(), 2, "one failed resume, then one fresh review");
    assert_eq!(resume_ids(&records[0]), vec!["dead"]);
    assert!(
        resume_ids(&records[1]).is_empty(),
        "the fallback must be a fresh session: {:?}",
        records[1]
    );
    assert!(
        prompt_of(&records[1]).contains("You are the merge-authority reviewer"),
        "the fallback must carry the FULL cold prompt, not the short turn"
    );
    assert!(
        ledger
            .run_event_kinds(current_run)
            .unwrap()
            .iter()
            .any(|kind| kind == "resume_fallback"),
        "the controlled boundary must be journalled"
    );
    let retired = ledger
        .recent_runs(10)
        .unwrap()
        .into_iter()
        .find(|run| run.id == prior_run)
        .unwrap();
    assert_eq!(retired.session_ref, None, "the dead id must be retired");
    let next_run = ledger
        .start_review_run(task_id, AgentKind::Claude, Some("opus"))
        .unwrap();
    let next = ledger
        .last_run_ref(task_id, "review", Some("claude"), next_run)
        .unwrap()
        .unwrap();
    assert_ne!(next.session_ref.as_deref(), Some("dead"));
}

#[test]
fn error_after_review_session_retirement_does_not_restore_the_dead_id() {
    let _guard = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let tmp = tempfile::tempdir().unwrap();
    let repo = tmp.path().join("wt");
    std::fs::create_dir(&repo).unwrap();
    git(&repo, &["init", "-q", "-b", "main"]);
    std::fs::write(repo.join("a.rs"), "fn a() {}\n").unwrap();
    git(&repo, &["add", "."]);
    git(&repo, &["commit", "-qm", "base"]);
    let base = git(&repo, &["rev-parse", "HEAD"]);
    std::fs::write(repo.join("a.rs"), "fn a() -> u8 { 1 }\n").unwrap();
    git(&repo, &["add", "."]);
    git(&repo, &["commit", "-qm", "work"]);
    let tip = git(&repo, &["rev-parse", "HEAD"]);

    let ledger = Ledger::open(&tmp.path().join("ledger.db")).unwrap();
    let task_id = ledger
        .add_task("retired review error", "spec", "impl", "high", &[], "none")
        .unwrap();
    let task = ledger.task(task_id).unwrap().unwrap();
    let profile = cosmix_foreman::verify::lookup_profile("none").unwrap();
    let prior = ledger
        .start_review_run(task_id, AgentKind::Claude, Some("opus"))
        .unwrap();
    ledger
        .finish_run(
            prior,
            &RunOutcome {
                stop: StopReason::Error,
                result: None,
                error: Some("old review failed".into()),
                usage: Usage::default(),
                session_ref: Some("dead".into()),
                terminal_session_ref: None,
                usage_observed: false,
                output_observed: false,
                resume_failure: None,
            },
            1,
        )
        .unwrap();

    // The resume proves the id dead, then removes its own executable. The
    // fresh fallback therefore fails to spawn after the retirement commit.
    let script = tmp.path().join("vanishing-claude");
    std::fs::write(
        &script,
        "#!/bin/sh\nrm -- \"$0\"\necho 'No conversation found with session ID: dead' >&2\nexit 1\n",
    )
    .unwrap();
    std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).unwrap();
    unsafe {
        std::env::set_var("FOREMAN_QUIET", "1");
    }

    let current = ledger
        .start_review_run(task_id, AgentKind::Claude, Some("opus"))
        .unwrap();
    let failure = review_landing(
        &ledger,
        current,
        &repo,
        &task,
        ReviewConfig {
            base: &base,
            tip: &tip,
            touches_foreman: true,
            reviewer: AgentKind::Claude,
            model: "opus",
            claude_bin: script.to_str().unwrap(),
            codex_bin: "codex",
            sibling_repos: None,
            reserve_usd: 5.0,
            reserve_tokens: 100_000,
            stall_secs: 60,
            verify_subdir: ".",
            profile: &profile,
            project_pack: "",
            resume_session_ref: Some("dead"),
        },
    )
    .unwrap_err();
    assert!(
        failure
            .to_string()
            .contains("starting Claude review session"),
        "{failure:#}"
    );
    assert_eq!(
        failure.session_ref, None,
        "typed failure state must not preserve a classified-dead id"
    );

    // This is the refinery wrapper's terminal write. The typed failure state
    // is the value it now persists instead of the original resume candidate.
    ledger
        .finish_run(
            current,
            &RunOutcome {
                stop: StopReason::Error,
                result: None,
                error: Some(failure.to_string()),
                usage: Usage::default(),
                session_ref: failure.session_ref,
                terminal_session_ref: None,
                usage_observed: false,
                output_observed: false,
                resume_failure: None,
            },
            1,
        )
        .unwrap();
    let next = ledger
        .start_review_run(task_id, AgentKind::Claude, Some("opus"))
        .unwrap();
    let next_ref = ledger
        .last_run_ref(task_id, "review", Some("claude"), next)
        .unwrap()
        .and_then(|run| run.session_ref);
    assert_eq!(next_ref, None, "the next review sweep must start cold");
}

#[test]
fn resumed_review_that_streams_usage_then_dies_does_not_fallback() {
    let _guard = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let tmp = tempfile::tempdir().unwrap();
    let repo = tmp.path().join("wt");
    std::fs::create_dir(&repo).unwrap();
    git(&repo, &["init", "-q", "-b", "main"]);
    std::fs::write(repo.join("a.rs"), "fn a() {}\n").unwrap();
    git(&repo, &["add", "."]);
    git(&repo, &["commit", "-qm", "base"]);
    let base = git(&repo, &["rev-parse", "HEAD"]);
    std::fs::write(repo.join("a.rs"), "fn a() -> u8 { 1 }\n").unwrap();
    git(&repo, &["add", "."]);
    git(&repo, &["commit", "-qm", "work"]);
    let tip = git(&repo, &["rev-parse", "HEAD"]);

    let ledger = Ledger::open(&tmp.path().join("ledger.db")).unwrap();
    let task_id = ledger
        .add_task(
            "usage before dead resume",
            "spec",
            "impl",
            "high",
            &[],
            "none",
        )
        .unwrap();
    let task = ledger.task(task_id).unwrap().unwrap();
    let profile = cosmix_foreman::verify::lookup_profile("none").unwrap();
    let script = tmp.path().join("usage-then-dead-claude");
    let argv_log = tmp.path().join("argv.log");
    std::fs::write(
        &script,
        "#!/bin/sh\nprintf '%s\\0' \"$@\" >> \"$ARGV_LOG\"\nprintf '\\36' >> \"$ARGV_LOG\"\n\
         echo '{\"type\":\"system\",\"subtype\":\"init\",\"session_id\":\"dead\"}'\n\
         echo '{\"type\":\"assistant\",\"message\":{\"usage\":{\"input_tokens\":77,\"output_tokens\":3},\"content\":[]}}'\n\
         echo 'No conversation found with session ID: dead' >&2\nexit 1\n",
    )
    .unwrap();
    std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).unwrap();
    unsafe {
        std::env::set_var("ARGV_LOG", &argv_log);
        std::env::set_var("FOREMAN_QUIET", "1");
    }
    let run = ledger
        .start_review_run(task_id, AgentKind::Claude, Some("opus"))
        .unwrap();
    let outcome = review_landing(
        &ledger,
        run,
        &repo,
        &task,
        ReviewConfig {
            base: &base,
            tip: &tip,
            touches_foreman: true,
            reviewer: AgentKind::Claude,
            model: "opus",
            claude_bin: script.to_str().unwrap(),
            codex_bin: "codex",
            sibling_repos: None,
            reserve_usd: 5.0,
            reserve_tokens: 100_000,
            stall_secs: 60,
            verify_subdir: ".",
            profile: &profile,
            project_pack: "",
            resume_session_ref: Some("dead"),
        },
    )
    .unwrap();
    assert!(!outcome.approve);
    assert!(outcome.usage_observed);
    assert_eq!(outcome.usage.input_tokens, 77);
    assert_eq!(
        argv_records(&argv_log).len(),
        1,
        "fallback would double-spend"
    );
    assert_eq!(
        ledger
            .recent_runs(10)
            .unwrap()
            .into_iter()
            .find(|record| record.id == run)
            .unwrap()
            .tokens_in,
        77
    );
    assert!(
        !ledger
            .run_event_kinds(run)
            .unwrap()
            .contains(&"resume_fallback".into())
    );
}

#[test]
fn mismatched_resume_that_rendered_a_verdict_fails_closed_without_fallback() {
    let _guard = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let tmp = tempfile::tempdir().unwrap();
    let repo = tmp.path().join("wt");
    std::fs::create_dir(&repo).unwrap();
    git(&repo, &["init", "-q", "-b", "main"]);
    std::fs::write(repo.join("a.rs"), "fn a() {}\n").unwrap();
    git(&repo, &["add", "."]);
    git(&repo, &["commit", "-qm", "base"]);
    let base = git(&repo, &["rev-parse", "HEAD"]);
    std::fs::write(repo.join("a.rs"), "fn a() -> u8 { 1 }\n").unwrap();
    git(&repo, &["add", "."]);
    git(&repo, &["commit", "-qm", "work"]);
    let tip = git(&repo, &["rev-parse", "HEAD"]);

    let ledger = Ledger::open(&tmp.path().join("ledger.db")).unwrap();
    let task_id = ledger
        .add_task("mismatched resume", "spec", "impl", "high", &[], "none")
        .unwrap();
    let task = ledger.task(task_id).unwrap().unwrap();
    let profile = cosmix_foreman::verify::lookup_profile("none").unwrap();
    let script = tmp.path().join("mismatch-claude");
    let argv_log = tmp.path().join("argv.log");
    let first = tmp.path().join("first.jsonl");
    let second = tmp.path().join("second.jsonl");
    let used = tmp.path().join("used");
    std::fs::write(
        &first,
        review_stream_with_ids("requested", "wrong", "APPROVE", &["a.rs"])
            .replace(",\"usage\":{\"input_tokens\":900,\"output_tokens\":40}", ""),
    )
    .unwrap();
    std::fs::write(
        &second,
        review_stream_with_ids("fresh", "fresh", "APPROVE", &["a.rs"]),
    )
    .unwrap();
    std::fs::write(
        &script,
        format!(
            "#!/bin/sh\nprintf '%s\\0' \"$@\" >> \"$ARGV_LOG\"\nprintf '\\36' >> \"$ARGV_LOG\"\n\
             if [ -f '{used}' ]; then cat '{second}'; else : > '{used}'; cat '{first}'; fi\n",
            first = first.display(),
            second = second.display(),
            used = used.display(),
        ),
    )
    .unwrap();
    std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).unwrap();
    unsafe {
        std::env::set_var("ARGV_LOG", &argv_log);
        std::env::set_var("TMP", tmp.path());
        std::env::set_var("FOREMAN_QUIET", "1");
    }
    let run = ledger
        .start_review_run(task_id, AgentKind::Claude, Some("opus"))
        .unwrap();
    let outcome = review_landing(
        &ledger,
        run,
        &repo,
        &task,
        ReviewConfig {
            base: &base,
            tip: &tip,
            touches_foreman: true,
            reviewer: AgentKind::Claude,
            model: "opus",
            claude_bin: script.to_str().unwrap(),
            codex_bin: "codex",
            sibling_repos: None,
            reserve_usd: 5.0,
            reserve_tokens: 100_000,
            stall_secs: 60,
            verify_subdir: ".",
            profile: &profile,
            project_pack: "",
            resume_session_ref: Some("requested"),
        },
    )
    .unwrap();
    assert!(!outcome.approve, "a mismatched resume rendered work");
    assert_eq!(outcome.session_ref, None, "mismatched id must be retired");
    assert_eq!(
        outcome.resume_failure,
        Some(ResumeFailure::SessionIdMismatch)
    );
    assert!(!outcome.usage_observed, "missing telemetry stays unknown");
    assert!(
        outcome.output_observed,
        "the rendered verdict is consumed work"
    );
    assert_eq!(outcome.usage.cost_usd, None, "unknown spend is not zero");
    assert_eq!(argv_records(&argv_log).len(), 1, "no fresh fallback");
    assert!(
        !ledger
            .run_event_kinds(run)
            .unwrap()
            .contains(&"resume_fallback".into()),
        "worked resume must not cross the fallback boundary"
    );
    assert!(
        ledger
            .run_event_kinds(run)
            .unwrap()
            .contains(&"review_usage_unknown".into()),
        "unknown usage must be recorded as unknown"
    );

    ledger
        .finish_run(
            run,
            &RunOutcome {
                stop: StopReason::Error,
                result: None,
                error: Some(outcome.report.clone()),
                usage: outcome.usage.clone(),
                session_ref: outcome.session_ref.clone(),
                terminal_session_ref: None,
                usage_observed: outcome.usage_observed,
                output_observed: outcome.output_observed,
                resume_failure: outcome.resume_failure,
            },
            1,
        )
        .unwrap();
    let next_run = ledger
        .start_review_run(task_id, AgentKind::Claude, Some("opus"))
        .unwrap();
    let next_resume = ledger
        .last_run_ref(task_id, "review", Some("claude"), next_run)
        .unwrap()
        .and_then(|prior| prior.session_ref);
    assert_eq!(next_resume, None, "the next sweep must start cold");

    let recovered = review_landing(
        &ledger,
        next_run,
        &repo,
        &task,
        ReviewConfig {
            base: &base,
            tip: &tip,
            touches_foreman: true,
            reviewer: AgentKind::Claude,
            model: "opus",
            claude_bin: script.to_str().unwrap(),
            codex_bin: "codex",
            sibling_repos: None,
            reserve_usd: 5.0,
            reserve_tokens: 100_000,
            stall_secs: 60,
            verify_subdir: ".",
            profile: &profile,
            project_pack: "",
            resume_session_ref: next_resume.as_deref(),
        },
    )
    .unwrap();
    assert!(recovered.approve, "{}", recovered.report);
    let records = argv_records(&argv_log);
    assert_eq!(records.len(), 2, "one failed resume, then one cold sweep");
    assert_eq!(resume_ids(&records[0]), vec!["requested"]);
    assert!(resume_ids(&records[1]).is_empty(), "{records:?}");
}

#[test]
fn resumed_verdict_without_any_session_id_is_never_approval_capable() {
    let _guard = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let tmp = tempfile::tempdir().unwrap();
    let repo = tmp.path().join("wt");
    std::fs::create_dir(&repo).unwrap();
    git(&repo, &["init", "-q", "-b", "main"]);
    std::fs::write(repo.join("a.rs"), "fn a() {}\n").unwrap();
    git(&repo, &["add", "."]);
    git(&repo, &["commit", "-qm", "base"]);
    let base = git(&repo, &["rev-parse", "HEAD"]);
    std::fs::write(repo.join("a.rs"), "fn a() -> u8 { 1 }\n").unwrap();
    git(&repo, &["add", "."]);
    git(&repo, &["commit", "-qm", "work"]);
    let tip = git(&repo, &["rev-parse", "HEAD"]);

    let ledger = Ledger::open(&tmp.path().join("ledger.db")).unwrap();
    let task_id = ledger
        .add_task("missing resume id", "spec", "impl", "high", &[], "none")
        .unwrap();
    let task = ledger.task(task_id).unwrap().unwrap();
    let profile = cosmix_foreman::verify::lookup_profile("none").unwrap();
    let bin = fake_claude(tmp.path());
    let argv_log = tmp.path().join("argv.log");
    let stream = tmp.path().join("stream.jsonl");
    std::fs::write(&stream, review_stream_without_ids("APPROVE", &["a.rs"])).unwrap();
    unsafe {
        std::env::set_var("ARGV_LOG", &argv_log);
        std::env::set_var("FAKE_STREAM", &stream);
        std::env::set_var("FOREMAN_QUIET", "1");
    }
    let run = ledger
        .start_review_run(task_id, AgentKind::Claude, Some("opus"))
        .unwrap();
    let outcome = review_landing(
        &ledger,
        run,
        &repo,
        &task,
        ReviewConfig {
            base: &base,
            tip: &tip,
            touches_foreman: true,
            reviewer: AgentKind::Claude,
            model: "opus",
            claude_bin: bin.to_str().unwrap(),
            codex_bin: "codex",
            sibling_repos: None,
            reserve_usd: 5.0,
            reserve_tokens: 100_000,
            stall_secs: 60,
            verify_subdir: ".",
            profile: &profile,
            project_pack: "",
            resume_session_ref: Some("requested"),
        },
    )
    .unwrap();
    assert!(!outcome.approve, "an unproven resume rendered APPROVE");
    assert_eq!(outcome.session_ref, None);
    assert_eq!(
        outcome.resume_failure,
        Some(ResumeFailure::SessionIdMissing)
    );
    assert!(
        outcome.output_observed,
        "the rendered verdict is consumed work"
    );
    assert_eq!(argv_records(&argv_log).len(), 1, "no fresh fallback");
    assert!(
        !ledger
            .run_event_kinds(run)
            .unwrap()
            .contains(&"resume_fallback".into()),
        "missing identity must not authorise fallback"
    );
}

/// The acceptance clause's measurement, made executable.
///
/// Task 70 asks for a re-review ROUND whose token cost drops by >=80%. The
/// round's ingest is not the harness-authored payload alone: it is that
/// payload PLUS the `git show` reads the prompt instructs the reviewer to
/// perform. A cold arm is told to inspect every indexed path at both
/// revisions; a resumed arm is told not to re-read a path whose content is
/// unchanged since its last inspection, so it reads only what the fix
/// touched. The fake process performs those reads itself, streams the
/// derived total as usage, and the assertion reads both checkpointed totals
/// from the ledger. It is deterministic fixture accounting, not an estimate
/// of vendor cache billing for a resumed conversation.
#[test]
fn rereview_round_ingest_drops_by_at_least_80_percent() {
    let _guard = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let tmp = tempfile::tempdir().unwrap();
    let repo = tmp.path().join("wt");
    std::fs::create_dir(&repo).unwrap();
    git(&repo, &["init", "-q", "-b", "main"]);

    // Copy actual Foreman sources. Their sizes are whatever the reviewed code
    // honestly is; the fixture has no padding knob that can manufacture the
    // threshold.
    let source_root = Path::new(env!("CARGO_MANIFEST_DIR"));
    let paths = [
        "src/review.rs",
        "src/runner.rs",
        "src/refinery/cargo.rs",
        "src/refinery/errors.rs",
        "src/refinery/land.rs",
        "src/refinery/manifest_base.rs",
        "src/refinery/manifest_live.rs",
        "src/refinery/mod.rs",
        "src/refinery/preflight.rs",
        "src/refinery/rebase.rs",
        "src/refinery/recovery.rs",
        "src/refinery/reviews.rs",
        "src/refinery/version.rs",
        "src/refinery/version_fs.rs",
        "src/refinery/worktree.rs",
        "src/driver/claude.rs",
        "src/driver/codex.rs",
    ];
    for path in paths {
        let destination = repo.join(path);
        std::fs::create_dir_all(destination.parent().unwrap()).unwrap();
        std::fs::copy(source_root.join(path), destination).unwrap();
    }
    git(&repo, &["add", "."]);
    git(&repo, &["commit", "-qm", "base"]);
    let base = git(&repo, &["rev-parse", "HEAD"]);

    for path in paths {
        let path = repo.join(path);
        let mut content = std::fs::read_to_string(&path).unwrap();
        content.push_str("\n// first-review fixture revision\n");
        std::fs::write(path, content).unwrap();
    }
    git(&repo, &["add", "."]);
    git(&repo, &["commit", "-qm", "work"]);
    let first_tip = git(&repo, &["rev-parse", "HEAD"]);

    let ledger = Ledger::open(&tmp.path().join("ledger.db")).unwrap();
    let task_id = ledger
        .add_task(
            "resume cycle measurement",
            "spec",
            "impl",
            "high",
            &[],
            "none",
        )
        .unwrap();
    let task = ledger.task(task_id).unwrap().unwrap();
    let profile = cosmix_foreman::verify::lookup_profile("none").unwrap();

    let bin = billing_fake_claude(tmp.path());
    let argv_log = tmp.path().join("argv.log");
    unsafe {
        std::env::set_var("ARGV_LOG", &argv_log);
        std::env::set_var("BASE_REV", &base);
        std::env::set_var("FIRST_TIP", &first_tip);
        std::env::set_var("CURRENT_TIP", &first_tip);
        std::env::set_var("FAKE_SESSION", "thread-M");
        std::env::set_var("FAKE_VERDICT", "REJECT");
        std::env::set_var("FOREMAN_QUIET", "1");
    }

    let config = |tip: &str, resume: Option<&str>| ReviewConfig {
        base: &base,
        tip: Box::leak(tip.to_string().into_boxed_str()),
        touches_foreman: true,
        reviewer: AgentKind::Claude,
        model: "opus",
        claude_bin: bin.to_str().unwrap(),
        codex_bin: "codex",
        sibling_repos: None,
        reserve_usd: 5.0,
        reserve_tokens: 100_000,
        stall_secs: 60,
        verify_subdir: ".",
        profile: &profile,
        project_pack: "",
        resume_session_ref: resume.map(|r| Box::leak(r.to_string().into_boxed_str()) as &str),
    };

    let run1 = ledger
        .start_review_run(task_id, AgentKind::Claude, Some("opus"))
        .unwrap();
    let cold = review_landing(&ledger, run1, &repo, &task, config(&first_tip, None)).unwrap();
    assert!(!cold.approve, "{}", cold.report);
    let thread = cold
        .session_ref
        .clone()
        .expect("the cold arm reported a thread");

    // The fix touches ONE indexed path; the others are byte-identical to the
    // revision this thread already inspected.
    let fixed_path = repo.join("src/driver/codex.rs");
    let mut fixed_content = std::fs::read_to_string(&fixed_path).unwrap();
    fixed_content.push_str("// fix-round fixture revision\n");
    std::fs::write(fixed_path, fixed_content).unwrap();
    git(&repo, &["add", "."]);
    git(&repo, &["commit", "-qm", "fix"]);
    let fixed_tip = git(&repo, &["rev-parse", "HEAD"]);

    unsafe {
        std::env::set_var("CURRENT_TIP", &fixed_tip);
        std::env::set_var("FAKE_VERDICT", "APPROVE");
    }
    let run2 = ledger
        .start_review_run(task_id, AgentKind::Claude, Some("opus"))
        .unwrap();
    let resumed = review_landing(
        &ledger,
        run2,
        &repo,
        &task,
        config(&fixed_tip, Some(&thread)),
    )
    .unwrap();
    assert!(resumed.approve, "{}", resumed.report);

    let records = argv_records(&argv_log);
    assert_eq!(
        records.len(),
        2,
        "exactly two reviewer invocations: {records:?}"
    );
    assert_eq!(
        resume_ids(&records[1]),
        vec![thread.clone()],
        "the measured second round must be a resume: {:?}",
        records[1]
    );

    let runs = ledger.recent_runs(10).unwrap();
    let cold_round = runs.iter().find(|run| run.id == run1).unwrap().tokens_in;
    let resumed_round = runs.iter().find(|run| run.id == run2).unwrap().tokens_in;
    assert!(
        cold_round > 0 && resumed_round > 0,
        "streamed usage was not checkpointed"
    );
    let round_drop = 100.0 - (resumed_round as f64 / cold_round as f64) * 100.0;
    println!(
        "ledger-measured re-review ingest: {cold_round} -> {resumed_round} \
         derived byte-tokens ({round_drop:.1}% drop)"
    );

    assert!(
        round_drop >= 80.0,
        "the acceptance clause asks for >=80%: round {cold_round} -> {resumed_round} \
         bytes is {round_drop:.1}%"
    );

    // Prove the threshold is coupled to the instruction, not to `--resume`
    // or a hand-selected changed-path list. Removing the unchanged-path rule
    // makes the same fake reviewer conservatively re-read every indexed blob
    // and must lose the acceptance margin.
    let resumed_prompt = prompt_of(&records[1]);
    let regressed_prompt = resumed_prompt.replace(
        "Do not re-read a path whose content is unchanged since your last inspection.",
        "Re-read every indexed path from both revisions.",
    );
    assert_ne!(
        regressed_prompt, resumed_prompt,
        "policy sentence was not found"
    );
    let regression_log = tmp.path().join("regression-argv.log");
    let regression = Command::new(&bin)
        .args(["-p", &regressed_prompt, "--resume", &thread])
        .current_dir(&repo)
        .env("ARGV_LOG", regression_log)
        .env("BASE_REV", &base)
        .env("FIRST_TIP", &first_tip)
        .env("CURRENT_TIP", &fixed_tip)
        .env("FAKE_SESSION", &thread)
        .env("FAKE_VERDICT", "APPROVE")
        .output()
        .unwrap();
    assert!(regression.status.success(), "{regression:?}");
    let regressed_round = reported_input_tokens(&regression.stdout);
    let regressed_drop = 100.0 - (regressed_round as f64 / cold_round as f64) * 100.0;
    assert!(
        regressed_drop < 80.0,
        "re-reading unchanged paths must fail acceptance: {cold_round} -> {regressed_round} \
         was {regressed_drop:.1}%"
    );
}
