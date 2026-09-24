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

/// A leader that honours SIGTERM ends inside the grace window: the result
/// reports signal 15 (the reason) and the call does NOT wait out the grace.
/// Its descendant ignores SIGTERM — and is still reaped, because the group is
/// SIGKILLed once the leader has gone. Before this, that descendant was left
/// running as an orphan.
#[tokio::test]
async fn cooperative_leader_ends_early_and_its_term_ignoring_descendant_is_swept() {
    let output = run_ok(
        "$r = run_argv([\"sh\", \"-c\", \"(trap '' TERM; exec sleep 60) & echo $!; wait\"], {timeout: 0.3, grace: 10})\n\
         print($r.timed_out .. \" \" .. $r.signal .. \" \" .. ($r.duration_ms < 5000))\n\
         print(trim($r.stdout))\n",
    )
    .await;
    let lines = parse_lines(&output);
    assert_eq!(lines[0], "true 15 true", "full output: {output:?}");
    let descendant: u32 = lines[1].parse().expect("descendant pid on stdout");
    let gone = wait_gone(descendant, Duration::from_secs(5));
    if !gone {
        kill_leftover(descendant);
    }
    assert!(gone, "TERM-ignoring descendant {descendant} was orphaned");
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

/// spawn's lifetime option: validated, contradictory with detach, and the
/// child it starts leads its own process group (so its tree is addressable).
#[tokio::test]
async fn spawn_die_with_parent_leads_its_own_group_and_refuses_detach() {
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
