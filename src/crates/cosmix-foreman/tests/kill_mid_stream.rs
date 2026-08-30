//! Proves the incremental usage checkpoint (Task 6) survives a genuinely
//! hard kill of the real `foreman` binary mid-stream -- the shape a bare
//! SIGTERM produces in production: no signal handler is installed anywhere
//! in this codebase, so the process terminates immediately, no Drop guard
//! runs, and `finish_run` never executes. Before this fix a run killed this
//! way left `runs.tokens_out = 0` / `cost_usd = NULL` forever, undercounting
//! the governor's daily spend even though real usage had already streamed.
//!
//! Spawns the real `foreman` binary (via `CARGO_BIN_EXE_foreman`, so this
//! is the production code path, not a stand-in) pointed at a fake `claude`
//! CLI (`FOREMAN_CLAUDE_BIN`) that streams two usage-bearing turns and then
//! hangs -- mirroring an in-progress session with no terminal `result` line
//! yet. Polls the ledger from a second connection until the checkpoint
//! lands, SIGTERMs the child, and asserts the run row already carries that
//! usage instead of the zero/NULL defaults `start_run` inserts.
//!
//! The other tests here cover the softer kills that reach `finish_run` with
//! nothing left to write: the runner's own stall/wall/budget kill (the
//! parser's accumulation can be lost with the abandoned reader) and the
//! ledger invariant underneath it — a finish must never regress a streamed
//! checkpoint back to zeros.

use std::path::PathBuf;
use std::time::{Duration, Instant};

use cosmix_foreman::driver::claude::ClaudeDriver;
use cosmix_foreman::executor::{AgentKind, RunOutcome, StopReason, Usage};
use cosmix_foreman::ledger::Ledger;
use cosmix_foreman::runner::{RunOptions, run_task};

mod support;

/// A fake `claude` CLI: two assistant turns carrying usage (cumulative per
/// the real ClaudeParser: 12+30=42 in, 5+9=14 out), then hangs indefinitely
/// with no terminal `result` line -- an in-flight session, not a finished
/// one. `sleep infinity` rather than a fixed duration: this suite runs
/// alongside other CPU-heavy cargo jobs on a shared box, and a fixed sleep
/// can race a poll-then-kill sequence under enough scheduling contention
/// (observed: a 30s sleep exiting on its own just as a slow poll loop
/// reached the 30s mark, flipping the final disposition assertion from
/// "still running" to "failed").
///
/// `exec sleep` — NOT a bare `sleep` — is load-bearing, and the difference
/// is a leaked process per test run. `harden()` arms PR_SET_PDEATHSIG on
/// the spawned child, but the kernel delivers that signal only to the
/// DIRECT child: with a bare `sleep`, that child is `/bin/sh`, so SIGTERM
/// reaps the shell and orphans its `sleep` to init, forever. Nothing else
/// cleans up here — unlike the stall/wall paths, this test deliberately
/// kills foreman with no handler running, so there is no process-group
/// kill to catch the stray. `exec` makes the sleep itself the direct,
/// pdeathsig-carrying child (the flag survives a plain exec; it is cleared
/// only on fork or a setuid/caps exec), so it dies with foreman. An
/// earlier revision of this fixture stranded one `sleep infinity` per
/// invocation on the shared build host — 15 of them before it was caught.
///
/// Written once, before any test in this file forks: writing a script while
/// a concurrent test's fork+exec is in flight fails ETXTBSY (the forked
/// child transiently holds the write fd open). The `OnceLock` is never
/// dropped (statics aren't), so the fixture's TempDir outlives the test
/// binary; that leaves one small directory under /tmp per run of this
/// suite, which the OS reclaims — deliberate, and the price of the
/// write-once ETXTBSY guarantee above.
fn hanging_claude() -> &'static std::path::Path {
    struct Fixture {
        _dir: tempfile::TempDir,
        script: PathBuf,
    }
    static FIXTURE: std::sync::OnceLock<Fixture> = std::sync::OnceLock::new();
    &FIXTURE
        .get_or_init(|| {
            let dir = tempfile::tempdir().unwrap();
            let script = dir.path().join("fake-claude-hang");
            support::write_executable(
                &script,
                "#!/bin/sh\n\
                 echo '{\"type\":\"system\",\"subtype\":\"init\",\"session_id\":\"sess-kill\"}'\n\
                 echo '{\"type\":\"assistant\",\"message\":{\"role\":\"assistant\",\"content\":[{\"type\":\"text\",\"text\":\"a\"}],\"usage\":{\"input_tokens\":12,\"output_tokens\":5}}}'\n\
                 echo '{\"type\":\"assistant\",\"message\":{\"role\":\"assistant\",\"content\":[{\"type\":\"text\",\"text\":\"b\"}],\"usage\":{\"input_tokens\":30,\"output_tokens\":9}}}'\n\
                 exec sleep infinity\n",
            );
            Fixture { _dir: dir, script }
        })
        .script
}

#[test]
fn sigterm_mid_stream_leaves_last_seen_usage_not_null() {
    let tmp = tempfile::tempdir().unwrap();
    let db_name = ["state", "db"].join(".");
    let db = tmp.path().join(db_name);
    let workdir = tmp.path().join("work");
    std::fs::create_dir_all(&workdir).unwrap();
    let stderr_log = tmp.path().join("foreman.stderr");

    let script = hanging_claude();

    {
        let ledger = Ledger::open(&db).unwrap();
        let task = ledger
            .add_task("kill-mid-stream", "spec", "impl", "low", &[], "none")
            .unwrap();
        assert_eq!(task, 1, "first task in a fresh ledger is id 1");
    }

    let foreman = PathBuf::from(env!("CARGO_BIN_EXE_foreman"));
    let mut child = std::process::Command::new(&foreman)
        .args([
            "--db",
            db.to_str().unwrap(),
            "run",
            "--task",
            "1",
            "--agent",
            "claude",
            "--workdir",
            workdir.to_str().unwrap(),
            // Comfortably longer than the poll+SIGTERM deadline below so
            // foreman's own stall kill cannot fire first under contention
            // and change the disposition out from under this test.
            "--stall-secs",
            "600",
            "--no-governor",
            "--no-verify",
        ])
        .env("FOREMAN_VERIFY_LANE", tmp.path().join("verify.lock"))
        .env("FOREMAN_VERIFY_LANE_WAIT_SECS", "30")
        .env("FOREMAN_CLAUDE_BIN", script)
        .stdout(std::process::Stdio::null())
        // Kept, not discarded: if foreman dies before the checkpoint lands,
        // its own diagnosis is the only thing that distinguishes a genuine
        // regression from a broken fixture, and `Stdio::null()` threw that
        // away — leaving the failure below to say only "never checkpointed".
        .stderr(std::process::Stdio::from(
            std::fs::File::create(&stderr_log).unwrap(),
        ))
        .spawn()
        .expect("spawn foreman run");

    // ONE connection for the whole poll, opened before the loop. Re-opening
    // per iteration is what made this test flaky, and it was self-inflicted:
    // `Ledger::open` runs `migrate` unconditionally, whose `execute_batch` of
    // `CREATE TABLE/INDEX IF NOT EXISTS` is a WRITE transaction taking an
    // exclusive lock. At 20 Hz that is a lock storm aimed at the very
    // database the process under test is streaming into, and foreman's
    // `update_run_usage` is fallible on purpose — a `SQLITE_BUSY` outlasting
    // its 5s `busy_timeout` aborts `drive`, so the observer was killing the
    // run it was waiting on. In WAL mode each `recent_runs` is its own read
    // transaction, so a long-lived reader still sees every commit another
    // process makes; nothing about the "how a governor query would observe
    // it mid-run" intent needs a fresh connection.
    let observer = Ledger::open(&db).unwrap();

    // No fixed sleep guessing at fork+exec+driver-startup latency. The bound
    // stays generous because this suite shares a box with other CPU-heavy
    // cargo jobs, but it is no longer the only backstop: a foreman that dies
    // early is now caught the moment it dies, so the deadline only has to
    // cover a *slow* start, not a dead one.
    //
    // The fixture emits two assistant turns: first (12, 5), then (30, 9).
    // The ClaudeParser accumulates to (42, 14). We wait for the final
    // cumulative tokens_out (14) to ensure both turns are checkpointed
    // before killing — otherwise we break after the first event (5) and
    // assert against the wrong final values.
    let deadline = Instant::now() + Duration::from_secs(120);
    let mut checkpointed = false;
    while Instant::now() < deadline {
        if let Ok(runs) = observer.recent_runs(1)
            && let Some(run) = runs.first()
            && run.tokens_out >= 14
        {
            checkpointed = true;
            break;
        }
        // A foreman that has already exited is never going to checkpoint;
        // waiting out the deadline would report a timeout and hide the real
        // cause. Fail immediately, carrying what it said on the way out.
        if let Some(status) = child.try_wait().expect("poll the foreman child") {
            panic!(
                "foreman exited ({status}) before checkpointing a streamed Usage \
                 event; its stderr:\n{}",
                std::fs::read_to_string(&stderr_log).unwrap_or_default()
            );
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    assert!(
        checkpointed,
        "foreman never checkpointed a streamed Usage event into the run row in \
         time; its stderr:\n{}",
        std::fs::read_to_string(&stderr_log).unwrap_or_default()
    );

    // The production failure mode under test: a bare SIGTERM, no handler
    // anywhere in this codebase, terminates `foreman run` immediately --
    // well short of `finish_run`. Not a graceful `interrupt()` + `wait()`;
    // the whole process is gone the instant the signal is delivered.
    unsafe {
        libc::kill(child.id() as i32, libc::SIGTERM);
    }
    let status = child.wait().expect("reap the killed foreman process");
    assert!(
        !status.success(),
        "a SIGTERM'd mid-stream run must not report success"
    );

    let ledger = Ledger::open(&db).unwrap();
    let runs = ledger.recent_runs(1).unwrap();
    assert_eq!(runs.len(), 1);
    assert_eq!(
        runs[0].tokens_in, 42,
        "run row must carry the last streamed cumulative usage, not the \
         start_run zero default"
    );
    assert_eq!(
        runs[0].tokens_out, 14,
        "run row must carry the last streamed cumulative usage, not the \
         start_run zero default"
    );
    // Honest limit of what a hard kill can preserve: the claude stream only
    // prices a session on its terminal `result` line (`total_cost_usd`), so
    // a run killed before that has TOKENS to persist and no dollars -- the
    // column stays NULL because nothing ever reported a price, not because
    // the checkpoint dropped one. The governor's daily token ceiling covers
    // this run; its dollar ceiling cannot, and no amount of checkpointing
    // changes that without inventing a price foreman was never told.
    assert_eq!(
        runs[0].cost_usd, None,
        "no usage event before the result line carries a price; a checkpoint \
         must not fabricate one"
    );
    assert_eq!(runs[0].role, "implement");
    assert_eq!(
        runs[0].delivery, "unknown",
        "a process killed before terminal disposition must remain void"
    );
    drop(ledger);
    let ledger = Ledger::open(&db).unwrap();
    assert_eq!(
        ledger.recent_runs(1).unwrap()[0].delivery,
        "unknown",
        "an idempotent migration must not promote an unfinished run"
    );
    // The task itself must not be left claimed-forever by a dead process --
    // out of scope for this fix, but worth a cheap sanity check that the
    // run row (not the task disposition) is what we changed.
    let task = ledger.task(1).unwrap().unwrap();
    assert_eq!(
        task.status, "running",
        "disposition on a hard kill is a separate concern from usage \
         persistence; this run never got the chance to finish"
    );
}

/// The checkpoint answers to the same rule as the finish: a later usage
/// event that carries no price must not null the price an earlier one
/// reported. No driver in the tree can regress that way today (claude only
/// ever sets `cost_usd`, codex never sets it, and the GLM remap clears it on
/// every event), so this holds the invariant structurally rather than by
/// driver accident -- the next driver to report a priced turn followed by an
/// unpriced one must not be able to erase real spend.
#[test]
fn checkpoint_never_nulls_a_price_it_already_reported() {
    let tmp = tempfile::tempdir().unwrap();
    let ledger = Ledger::open(&tmp.path().join("ledger.db")).unwrap();
    let task = ledger
        .add_task("coalesce", "spec", "impl", "low", &[], "none")
        .unwrap();
    let run = ledger
        .start_run(task, AgentKind::Claude, None, None)
        .unwrap();

    ledger
        .update_run_usage(
            run,
            &Usage {
                input_tokens: 10,
                fresh_input_tokens: None,
                output_tokens: 4,
                cost_usd: Some(0.0125),
                cache_read_input_tokens: None,
                cache_creation_input_tokens: None,
            },
        )
        .unwrap();
    // Later turn: more tokens, no price attached.
    ledger
        .update_run_usage(
            run,
            &Usage {
                input_tokens: 25,
                fresh_input_tokens: None,
                output_tokens: 11,
                cost_usd: None,
                cache_read_input_tokens: None,
                cache_creation_input_tokens: None,
            },
        )
        .unwrap();

    let row = ledger.recent_runs(1).unwrap().remove(0);
    assert_eq!(
        (row.tokens_in, row.tokens_out),
        (25, 11),
        "tokens are last-seen-wins: the driver's later figure supersedes"
    );
    assert_eq!(
        row.cost_usd,
        Some(0.0125),
        "an unpriced event is not a free one -- the reported price stands"
    );
}

/// The other half of the same guarantee, at the ledger boundary: once a
/// streamed checkpoint is on the row, `finish_run` must never write it back
/// down to zeros. An interrupted or errored run whose parser accumulation
/// was lost (abandoned reader, a ledger write that failed mid-stream) hands
/// `finish_run` a `Usage::default()`; taking that literally would erase real
/// spend and undercount the governor's day for work already paid for.
#[test]
fn finish_run_never_regresses_a_streamed_checkpoint() {
    let tmp = tempfile::tempdir().unwrap();
    let ledger = Ledger::open(&tmp.path().join("ledger.db")).unwrap();
    let task = ledger
        .add_task("checkpoint", "spec", "impl", "low", &[], "none")
        .unwrap();
    let streamed = Usage {
        input_tokens: 42,
        fresh_input_tokens: None,
        output_tokens: 14,
        cost_usd: Some(0.0421),
        cache_read_input_tokens: None,
        cache_creation_input_tokens: None,
    };

    // 1. Interrupted, with no usage of its own: the checkpoint stands, and
    //    the rest of the outcome still lands.
    let run = ledger
        .start_run(task, AgentKind::Claude, None, None)
        .unwrap();
    ledger.update_run_usage(run, &streamed).unwrap();
    ledger
        .finish_run(
            run,
            &RunOutcome {
                stop: StopReason::Interrupted,
                result: None,
                error: Some("session killed; reader abandoned".into()),
                usage: Usage::default(),
                session_ref: None,
                terminal_session_ref: None,
                usage_observed: false,
                output_observed: false,
                resume_failure: None,
            },
            1234,
        )
        .unwrap();
    let row = ledger.recent_runs(1).unwrap().remove(0);
    assert_eq!((row.tokens_in, row.tokens_out), (42, 14));
    assert_eq!(row.cost_usd, Some(0.0421));
    assert_eq!(row.verdict.as_deref(), Some("interrupted"));
    assert_eq!(row.duration_ms, Some(1234));

    // 2. A final figure that IS present outranks the checkpoint — the
    //    parser's accumulation is authoritative whenever it survives.
    let run = ledger
        .start_run(task, AgentKind::Claude, None, None)
        .unwrap();
    ledger.update_run_usage(run, &streamed).unwrap();
    ledger
        .finish_run(
            run,
            &RunOutcome {
                stop: StopReason::Done,
                result: Some("ok".into()),
                error: None,
                usage: Usage {
                    input_tokens: 90,
                    fresh_input_tokens: None,
                    output_tokens: 30,
                    // A driver that counts tokens but never prices them:
                    // `None` is "unpriced", not "free", so the streamed cost
                    // stays the best evidence there is.
                    cost_usd: None,
                    cache_read_input_tokens: None,
                    cache_creation_input_tokens: None,
                },
                session_ref: None,
                terminal_session_ref: None,
                usage_observed: true,
                output_observed: true,
                resume_failure: None,
            },
            9,
        )
        .unwrap();
    let row = ledger.recent_runs(1).unwrap().remove(0);
    assert_eq!((row.tokens_in, row.tokens_out), (90, 30));
    assert_eq!(row.cost_usd, Some(0.0421));

    // Both runs land in the governor's day sum, checkpoint included.
    assert!((ledger.total_spend_usd().unwrap() - 0.0842).abs() < 1e-9);
}

/// End-to-end over the runner's own kill paths (stall here; the wall-clock
/// and budget ceilings kill through the same code): the session dies
/// mid-stream, `finish_run` runs normally, and the run row carries the usage
/// that streamed before the kill rather than the `start_run` zeros.
#[test]
fn stall_killed_run_records_the_usage_it_streamed() {
    let tmp = tempfile::tempdir().unwrap();
    let ledger = Ledger::open(&tmp.path().join("ledger.db")).unwrap();
    let task = ledger
        .add_task("stalled", "spec", "impl", "low", &[], "none")
        .unwrap();

    let driver = ClaudeDriver::new().with_program(hanging_claude().to_str().unwrap());
    let opts = RunOptions {
        workdir: tmp.path().to_path_buf(),
        // The fake CLI streams its two turns at once and then never speaks
        // again: one second of silence is a stall.
        stall_secs: 1,
        verify: false,
        ..Default::default()
    };
    let report = run_task(&ledger, task, &driver, &opts).unwrap();

    assert_eq!(report.outcome.stop, StopReason::Interrupted);
    assert_eq!(report.task_status, "bounced");
    let row = ledger.recent_runs(1).unwrap().remove(0);
    assert_eq!(
        (row.tokens_in, row.tokens_out),
        (42, 14),
        "a stall-killed run must still account for what it spent"
    );
    assert_eq!(row.verdict.as_deref(), Some("interrupted"));
    assert_eq!(row.delivery, "harness_error");
}
