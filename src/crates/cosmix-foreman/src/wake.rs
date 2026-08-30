//! Best-effort ABP wake (Phase 3 slice 2 — see
//! `~/.cmctl/_plan/2026-08-17-cosmix-harness-plan.md`): when a task enters
//! the ready queue, ring the `foreman.wake` Bus verb so a `foreman-wake.mix`
//! citizen (`~/.cmctl/_etc/foreman/foreman-wake.mix` — deploy artifact, lives
//! in cmctl, not cos) kicks the supervisor unit immediately instead of
//! leaving it to its backstop timer.
//!
//! Design law: `_decisions/2026-07-20-no-poll-event-driven-amp-wake.md`
//! (cmctl) — "push is primary... a missed wake only costs latency". This
//! module is fire-and-forget by contract: every failure mode (no `mix` on
//! PATH, no broker, no citizen registered) degrades to "the backstop timer
//! covers it" and must never fail the caller's own operation (task add,
//! requeue, dispatch).

use std::process::{Command, Stdio};

/// The Bus verb the wake citizen (`~/.cmctl/_etc/foreman/foreman-wake.mix`)
/// answers. The citizen registers Bus service name "foreman"; the verb form
/// matches the fleet's other accelerator wakes (`provisiond.wake`,
/// `toolsd.wake`).
pub const WAKE_VERB: &str = "foreman.wake";

/// Outer wall-clock bound on the whole `mix -c` round-trip (mix startup +
/// broker connect + the RPC's own cooperative `timeout=` below). Bounds a
/// pathological hang so an enqueue can never block on this.
const WALL_SECS: u64 = 5;

/// The RPC's own cooperative timeout, in seconds — covers broker connect,
/// the citizen's `systemctl start`, and its reply (see foreman-wake.mix).
const RPC_TIMEOUT_SECS: f64 = 3.0;

/// The Mix one-liner fired at the broker. `send` is an RPC (waits for
/// `reply()`), so a citizen that's actually running answers `$rc == 0`; no
/// broker, no citizen, or a slow citizen all land in the `else` arm — never
/// a Mix-level error, only a boolean the caller may log and ignore.
fn wake_script(verb: &str, rpc_timeout_secs: f64) -> String {
    format!(
        "send foreman {verb} timeout={rpc_timeout_secs}\n\
         if $rc == 0 then exit(0) else exit(1) end\n"
    )
}

/// Fire the wake. Returns whether the citizen accepted it (`rc == 0`)
/// purely for optional logging — callers must never treat `false` as
/// failure of their own operation, only as "the backstop timer will cover
/// this instead".
pub fn fire(verb: &str) -> bool {
    fire_with("mix", verb, WALL_SECS)
}

fn fire_with(mix_bin: &str, verb: &str, wall_secs: u64) -> bool {
    let script = wake_script(verb, RPC_TIMEOUT_SECS);
    // `timeout` (not mix's own cooperative bound alone) covers a
    // pathological hang before mix even reaches the `send` — e.g. a
    // wedged interpreter start. `-k 2`: send TERM at the deadline, KILL 2s
    // later if it's still alive.
    Command::new("timeout")
        .arg("-k")
        .arg("2")
        .arg(wall_secs.to_string())
        .arg(mix_bin)
        .arg("-c")
        .arg(&script)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::fixture::write_executable;

    fn fake_bin(dir: &std::path::Path, name: &str, body: &str) -> std::path::PathBuf {
        let path = dir.join(name);
        write_executable(&path, body);
        path
    }

    #[test]
    fn wake_script_shape() {
        let s = wake_script("foreman.wake", 3.0);
        assert!(s.contains("send foreman foreman.wake timeout=3"));
        assert!(s.contains("if $rc == 0 then exit(0) else exit(1) end"));
    }

    #[test]
    fn fire_with_reports_acceptance() {
        let dir = tempfile::tempdir().unwrap();
        let mix = fake_bin(dir.path(), "fake-mix-ok", "#!/bin/sh\nexit 0\n");
        assert!(fire_with(mix.to_str().unwrap(), "foreman.wake", 5));
    }

    #[test]
    fn fire_with_reports_refusal_without_failing() {
        let dir = tempfile::tempdir().unwrap();
        let mix = fake_bin(dir.path(), "fake-mix-refuse", "#!/bin/sh\nexit 1\n");
        assert!(!fire_with(mix.to_str().unwrap(), "foreman.wake", 5));
    }

    #[test]
    fn fire_with_missing_binary_is_false_not_panic() {
        assert!(!fire_with(
            "/nonexistent/mix-binary-does-not-exist-xyz",
            "foreman.wake",
            5
        ));
    }
}
