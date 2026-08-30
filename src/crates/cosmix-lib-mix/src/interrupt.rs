//! Process-wide SIGINT cooperation for blocking Mix builtins.
//!
//! ## Why this exists
//!
//! Every Mix entry point (`cosmix-mix` REPL, runner, future embedders)
//! constructs a tokio current-thread runtime and runs the evaluator on
//! it. The evaluator owns an `Arc<AtomicBool>` interrupt flag that
//! statement-level evaluation polls at expression boundaries — pressing
//! Ctrl-C sets it, and the next poll converts it into a Mix runtime
//! error that unwinds the script.
//!
//! That works while the runtime is being polled. It does NOT work while
//! a *blocking* builtin like `ssh_run` is sitting inside `Command`'s
//! synchronous `try_wait()` / `wait_with_output()` — under a
//! current-thread runtime, the `tokio::signal::ctrl_c()` future cannot
//! make progress until control returns to the runtime. The flag is
//! never set, so the builtin's poll loop sees a steady `false` and the
//! user's Ctrl-C is silently swallowed until the child exits naturally.
//!
//! The fix is to install a process-level signal handler that runs in
//! signal context (independent of any runtime) and stores `true` into
//! the same `Arc<AtomicBool>` the evaluator already polls. The blocking
//! builtin reads the flag through the static `INTERRUPT_FLAG` published
//! here and treats absence (no host has wired anything) as "no
//! cooperation available, behave as before."
//!
//! ## Lifecycle
//!
//! 1. The host (REPL, runner, library embedder) constructs an
//!    `Evaluator` and calls [`init`] with `eval.interrupt_flag()`.
//! 2. [`init`] publishes the flag into [`INTERRUPT_FLAG`] and registers
//!    a `signal-hook` SIGINT handler bound to the *same* `Arc`. If
//!    publication fails because some earlier caller already published a
//!    different flag, no handler is registered — guaranteeing that the
//!    handler always writes into exactly the flag that builtins read
//!    from.
//! 3. The handler stays registered for the rest of the process; there
//!    is no `deinit`. Re-using one flag across re-entries is the
//!    intended behaviour.
//!
//! ## Coexistence with tokio's `ctrl_c`
//!
//! `signal-hook` and tokio install their SIGINT receivers via
//! `signal_hook_registry`-compatible mechanisms, which chain handlers
//! in registration order. Both fire on each SIGINT: ours stores into
//! the flag; tokio's wakes the `ctrl_c()` future so the script-mode
//! `tokio::select!` race in `cosmix-mix/src/main.rs` still sees it.

use std::sync::OnceLock;
use std::sync::atomic::{AtomicBool, Ordering};

use std::sync::Arc;

/// Process-wide interrupt flag, populated by [`init`]. Blocking
/// builtins poll this in their inner loops to cooperate with Ctrl-C.
///
/// Returns `None` if no host has called [`init`] yet — embedders of
/// the library that don't want SIGINT cooperation simply don't call
/// it, and consumers reading via `INTERRUPT_FLAG.get()` see `None`
/// and behave as if no flag exists.
pub static INTERRUPT_FLAG: OnceLock<Arc<AtomicBool>> = OnceLock::new();

/// Convenience for builtins: returns `true` if the published flag is
/// set, `false` otherwise (including when no host has wired one).
#[inline]
pub fn is_interrupted() -> bool {
    INTERRUPT_FLAG
        .get()
        .map(|f| f.load(Ordering::Relaxed))
        .unwrap_or(false)
}

/// Wire `flag` as the process-wide interrupt flag and register a
/// SIGINT handler that atomically stores `true` into it.
///
/// Single-shot: the first successful call publishes the flag and
/// registers exactly one handler bound to that flag. Every subsequent
/// call (regardless of whether it would have been the same `Arc` or a
/// different one) returns `false` and registers nothing. This trades
/// the convenience of "re-init does the right thing" for the safety
/// guarantee that the handler is always bound to the flag builtins
/// read from — there is no path that registers a handler against a
/// flag that won't be polled.
///
/// Returns `true` if this call published the flag (and registered the
/// handler), `false` if the flag was already published.
pub fn init(flag: Arc<AtomicBool>) -> bool {
    // `set` returns Err(_) if the cell is already initialized. We hand
    // it `flag.clone()` so the original `Arc` (owned by the caller's
    // evaluator) is unaffected by the publication outcome.
    if INTERRUPT_FLAG.set(flag.clone()).is_err() {
        return false;
    }
    // signal-hook stores `true` into the supplied AtomicBool from a
    // signal handler. The work it performs is async-signal-safe by
    // construction; we don't add any other code in the handler path.
    //
    // Errors here are exceptional (out of FDs, SIGINT permanently
    // blocked, etc.) and we deliberately swallow them — without the
    // handler the flag still works for the explicit-set path used by
    // the REPL's tokio task, and the pre-existing "blocking builtin
    // cannot be interrupted" behaviour is what we'd fall back to.
    let _ = signal_hook::flag::register(signal_hook::consts::SIGINT, flag);
    true
}

/// Test-only escape hatch: clear the published flag (if any). Not
/// exposed to non-test code — production has no path that wants to
/// "uninstall" the handler, and exposing one would create races
/// against in-flight builtin polls. Test code uses this to avoid
/// inter-test pollution from `init`.
#[cfg(test)]
pub(crate) fn _test_clear() {
    if let Some(f) = INTERRUPT_FLAG.get() {
        f.store(false, Ordering::SeqCst);
    }
}

/// Crate-wide serial guard for tests that touch the process-global
/// `INTERRUPT_FLAG`. Both `interrupt::tests` and `builtins::ssh_helpers_tests`
/// must lock this before calling `init()`, `_test_clear()`, or storing
/// into the canonical flag — otherwise parallel test runs interleave
/// and one test sees another's pre-armed `true`.
#[cfg(test)]
pub(crate) static TEST_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn is_interrupted_returns_false_when_unwired() {
        let _g = TEST_LOCK.lock().unwrap();
        _test_clear();
        // Whether or not init has been called, after _test_clear the
        // helper must report false.
        assert!(!is_interrupted());
    }

    #[test]
    fn init_then_set_propagates_through_helper() {
        let _g = TEST_LOCK.lock().unwrap();
        let f = Arc::new(AtomicBool::new(false));
        let _ = init(f.clone());
        // After init, INTERRUPT_FLAG.get() is some flag — it may not
        // be `f` if a previous test already published a different one,
        // so we read through the canonical path.
        let canonical = INTERRUPT_FLAG.get().expect("flag published").clone();
        canonical.store(true, Ordering::SeqCst);
        assert!(is_interrupted());
        canonical.store(false, Ordering::SeqCst);
        assert!(!is_interrupted());
    }
}
