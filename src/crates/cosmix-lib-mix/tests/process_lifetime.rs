//! TODO-mix P2: process lifetime — deadline escalation (`grace`), descendant
//! reaping, and the option validation around them.
//!
//! The acceptance shape: start a parent that forks a child, cancel it at a
//! deadline, prove the descendant is gone, and read the reason from the
//! result map — never from terminal text.

#![cfg(target_os = "linux")]

use std::time::{Duration, Instant};

use cosmix_mix::error::MixError;
use cosmix_mix::evaluator::{Evaluator, SharedBuf};
use cosmix_mix::lexer::Lexer;
use cosmix_mix::parser::Parser;

async fn run(source: &str) -> Result<String, MixError> {
    let mut lexer = Lexer::new(source);
    let tokens = lexer.tokenize()?;
    let mut parser = Parser::new(tokens, source);
    let statements = parser.parse_program()?;
    let stdout = SharedBuf::new();
    let stderr = SharedBuf::new();
    let mut evaluator = Evaluator::with_output(Box::new(stdout.clone()), Box::new(stderr.clone()));
    evaluator.execute(&statements).await?;
    Ok(stdout.to_string_lossy())
}

async fn run_ok(source: &str) -> String {
    run(source)
        .await
        .unwrap_or_else(|error| panic!("script should succeed, got: {error}"))
}

/// A pid is gone when /proc no longer lists it, or lists it as a zombie
/// (dead, only awaiting its new parent's reap — no longer running anything).
fn pid_is_gone(pid: u32) -> bool {
    match std::fs::read_to_string(format!("/proc/{pid}/stat")) {
        Err(_) => true,
        Ok(stat) => stat
            .rsplit_once(')')
            .map(|(_, rest)| rest.trim_start().starts_with('Z'))
            .unwrap_or(false),
    }
}

fn wait_gone(pid: u32, within: Duration) -> bool {
    let deadline = Instant::now() + within;
    loop {
        if pid_is_gone(pid) {
            return true;
        }
        if Instant::now() >= deadline {
            return false;
        }
        std::thread::sleep(Duration::from_millis(20));
    }
}

fn kill_leftover(pid: u32) {
    // SAFETY: plain kill(2) on a pid this test created; a stale pid is ESRCH.
    unsafe {
        libc::kill(pid as i32, libc::SIGKILL);
    }
}

fn parse_lines(output: &str) -> Vec<String> {
    output.lines().map(str::to_string).collect()
}

/// Acceptance: a parent that IGNORES SIGTERM forks a child that ignores it
/// too. At the deadline Mix sends SIGTERM, waits the grace, then SIGKILLs the
/// group: the result says timed out + signal 9, the call took at least
/// timeout + grace, and the descendant is gone.
#[tokio::test]
async fn deadline_grace_escalates_to_sigkill_and_reaps_the_descendant() {
    let output = run_ok(
        "$r = run_argv([\"sh\", \"-c\", \"trap '' TERM; sleep 60 & echo $!; wait\"], {timeout: 0.3, grace: 0.6})\n\
         print($r.timed_out .. \" \" .. $r.signal .. \" \" .. $r.ok .. \" \" .. ($r.duration_ms >= 850))\n\
         print(trim($r.stdout))\n",
    )
    .await;
    let lines = parse_lines(&output);
    assert_eq!(lines[0], "true 9 false true", "full output: {output:?}");
    let descendant: u32 = lines[1].parse().expect("descendant pid on stdout");
    let gone = wait_gone(descendant, Duration::from_secs(5));
    if !gone {
        kill_leftover(descendant);
    }
    assert!(gone, "descendant {descendant} outlived the escalated deadline");
}

/// The grace belongs to the whole GROUP, not the leader: the leader honours
/// SIGTERM at once (signal 15 is the reported reason), but its descendant
/// ignores SIGTERM, so the call waits out the grace and the descendant is
/// SIGKILLed AT the grace deadline — never left running as an orphan.
#[tokio::test]
async fn term_ignoring_descendant_is_killed_at_the_grace_deadline() {
    let output = run_ok(
        "$r = run_argv([\"sh\", \"-c\", \"(trap '' TERM; exec sleep 60) & echo $!; wait\"], {timeout: 0.3, grace: 1})\n\
         print($r.timed_out .. \" \" .. $r.signal .. \" \" .. ($r.duration_ms >= 1250) .. \" \" .. ($r.duration_ms < 5000))\n\
         print(trim($r.stdout))\n",
    )
    .await;
    let lines = parse_lines(&output);
    assert_eq!(lines[0], "true 15 true true", "full output: {output:?}");
    let descendant: u32 = lines[1].parse().expect("descendant pid on stdout");
    let gone = wait_gone(descendant, Duration::from_secs(5));
    if !gone {
        kill_leftover(descendant);
    }
    assert!(gone, "TERM-ignoring descendant {descendant} was orphaned");
}

fn marker_path(label: &str) -> std::path::PathBuf {
    let nonce = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    std::env::temp_dir().join(format!("mix-grace-{label}-{}-{nonce}", std::process::id()))
}

/// The review's `pg_dump | gzip` case: the leader (`sh`) dies on SIGTERM at
/// once, but its child traps SIGTERM, takes a second to clean up and writes a
/// marker. With grace 5 it must be allowed to finish — the call returns once
/// the group is empty, well before the grace runs out.
#[tokio::test]
async fn term_honouring_descendant_gets_the_grace_to_finish() {
    let marker = marker_path("honour");
    let m = marker.display();
    let output = run_ok(&format!(
        "$r = run_argv([\"sh\", \"-c\", \"(trap 'sleep 1; echo done > {m}; exit 0' TERM; while true; do sleep 0.1; done) & wait\"], {{timeout: 0.3, grace: 5}})\n\
         print($r.timed_out .. \" \" .. $r.signal .. \" \" .. ($r.duration_ms >= 1200) .. \" \" .. ($r.duration_ms < 4500))\n",
    ))
    .await;
    let finished = marker.exists();
    let _ = std::fs::remove_file(&marker);
    assert_eq!(output, "true 15 true true\n", "full output: {output:?}");
    assert!(finished, "the TERM-honouring descendant was killed before its cleanup");
}

/// The capture-drain deadline uses the same escalation: the leader has
/// already exited, a TERM-honouring descendant still holds stdout, and when
/// the deadline fires it gets SIGTERM and the grace — not an instant SIGKILL.
#[tokio::test]
async fn drain_deadline_honours_grace_for_a_pipe_holding_descendant() {
    let marker = marker_path("drain");
    let m = marker.display();
    let output = run_ok(&format!(
        "$r = run_argv([\"sh\", \"-c\", \"(trap 'sleep 0.5; echo done > {m}; exit 0' TERM; while true; do sleep 0.1; done) &\"], {{timeout: 0.3, grace: 5}})\n\
         print($r.timed_out .. \" \" .. $r.exit_code .. \" \" .. ($r.duration_ms < 4500))\n",
    ))
    .await;
    let finished = marker.exists();
    let _ = std::fs::remove_file(&marker);
    assert_eq!(output, "true 0 true\n", "full output: {output:?}");
    assert!(finished, "the drain deadline SIGKILLed a TERM-honouring descendant");
}

/// The default is unchanged: no grace means SIGKILL at the deadline.
#[tokio::test]
async fn deadline_without_grace_still_kills_at_once() {
    let output = run_ok(
        "$r = run_argv([\"sh\", \"-c\", \"trap '' TERM; sleep 60\"], {timeout: 0.3})\n\
         print($r.timed_out .. \" \" .. $r.signal .. \" \" .. ($r.duration_ms < 2000))\n",
    )
    .await;
    assert_eq!(output, "true 9 true\n");
}

/// grace is validated like timeout, and a grace with no deadline — a silent
/// no-op — is refused.
#[tokio::test]
async fn grace_option_is_validated() {
    let output = run_ok(
        "try\n  run_argv([\"true\"], {timeout: 0, grace: 1})\ncatch $m, $e\n  print($e.code)\nend\n\
         try\n  run_argv([\"true\"], {grace: \"1\"})\ncatch $m, $e\n  print($e.code)\nend\n\
         try\n  run_argv([\"true\"], {grace: -1})\ncatch $m, $e\n  print($e.code)\nend\n\
         print(run_argv([\"true\"], {grace: 0}).ok)\n\
         print(run_argv_must([\"printf\", \"x\"], {timeout: 5, grace: 1}))\n",
    )
    .await;
    assert_eq!(output, "OPTION_INVALID\nOPTION_INVALID\nOPTION_INVALID\ntrue\nx\n");
}

/// spawn's lifetime option: refused on a thread no host has enabled (the
/// embedder case — a pooled evaluator thread), validated, contradictory with
/// detach, and once enabled the child leads its own process group.
#[tokio::test(flavor = "current_thread")]
async fn spawn_die_with_parent_needs_a_host_leads_its_group_and_refuses_detach() {
    let refused = run_ok(
        "try\n  spawn([\"true\"], {die_with_parent: true})\ncatch $m, $e\n  print($e.code .. \" \" .. contains($m, \"owns the evaluator thread\"))\nend\n",
    )
    .await;
    assert_eq!(refused, "OPTION_INVALID true\n", "un-hosted spawn must be refused");

    assert!(cosmix_mix::builtins::owned_spawns::enable(), "this thread becomes the host");
    assert!(cosmix_mix::builtins::owned_spawns::enable(), "idempotent on the host thread");
    let other = std::thread::spawn(cosmix_mix::builtins::owned_spawns::enable)
        .join()
        .unwrap();
    assert!(!other, "a second thread cannot take over and is told so");
    let elsewhere = std::thread::spawn(|| {
        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap()
            .block_on(run_ok(
                "try\n  spawn([\"true\"], {die_with_parent: true})\ncatch $m, $e\n  print($e.code .. \" \" .. contains($m, \"another thread\"))\nend\n",
            ))
    })
    .join()
    .unwrap();
    assert_eq!(elsewhere, "OPTION_INVALID true\n", "the refusal names the other host thread");
    let output = run_ok(
        "try\n  spawn([\"true\"], {die_with_parent: true, detach: true})\ncatch $m, $e\n  print($e.code)\nend\n\
         try\n  spawn([\"true\"], {die_with_parent: 1})\ncatch $m, $e\n  print($e.code)\nend\n\
         print(spawn([\"sleep\", \"60\"], {die_with_parent: true}))\n",
    )
    .await;
    let lines = parse_lines(&output);
    assert_eq!(&lines[..2], ["OPTION_INVALID", "OPTION_INVALID"], "full output: {output:?}");
    let pid: i32 = lines[2].parse().expect("spawn returns the pid");
    // SAFETY: getpgid on a child of this process.
    let pgid = unsafe { libc::getpgid(pid) };
    kill_leftover(pid as u32);
    // SAFETY: reap our own child so the test leaves no zombie behind.
    unsafe {
        let mut status = 0;
        libc::waitpid(pid, &mut status, 0);
    }
    assert_eq!(pgid, pid, "die_with_parent child must lead its own process group");
}

/// A top-level run_parallel timeout replaces each job's own, so a job's
/// `{timeout: 0, grace: 1}` is a job WITH a deadline there, not a refused
/// grace-without-deadline; without the override it is still refused.
#[tokio::test]
async fn run_parallel_timeout_override_applies_before_grace_validation() {
    let output = run_ok(
        "$r = run_parallel([{argv: [\"true\"], timeout: 0, grace: 1}], {timeout: 5})\n\
         print($r[0].ok)\n\
         try\n  run_parallel([{argv: [\"true\"], timeout: 0, grace: 1}])\ncatch $m, $e\n  print($e.code)\nend\n",
    )
    .await;
    assert_eq!(output, "true\nOPTION_INVALID\n");
}

/// Review R6: a STOPPED member never handles SIGTERM until it is continued.
/// The escalation sends SIGCONT right after SIGTERM, so a stopped member
/// that traps SIGTERM still runs its cleanup inside the grace.
#[tokio::test]
async fn a_stopped_member_is_continued_so_it_can_honour_sigterm() {
    let marker = marker_path("stopped");
    let helper = marker_path("stopper-sh");
    std::fs::write(
        &helper,
        format!(
            "trap 'echo done > {}; exit 0' TERM\nkill -STOP $$\nwhile true; do sleep 0.1; done\n",
            marker.display()
        ),
    )
    .unwrap();
    let h = helper.display();
    let output = run_ok(&format!(
        "$r = run_argv([\"sh\", \"-c\", \"sh {h} & wait\"], {{timeout: 0.5, grace: 3}})\n\
         print($r.timed_out .. \" \" .. ($r.duration_ms < 3000))\n",
    ))
    .await;
    let finished = marker.exists();
    let _ = std::fs::remove_file(&marker);
    let _ = std::fs::remove_file(&helper);
    assert_eq!(output, "true true\n", "full output: {output:?}");
    assert!(finished, "the stopped member never ran its SIGTERM trap");
}
