//! Per-evaluation cancellation (P0-J J-8).
//!
//! [`interrupt`](crate::interrupt) owns ONE process-wide flag because every
//! blocking builtin reads it through `INTERRUPT_FLAG`, and that single flag is
//! exactly why `set_interrupt_flag()` per request cannot express cancellation:
//! swapping the evaluator's flag leaves the builtins reading the first one ever
//! published, and raising the shared flag for a request that arrived late kills
//! whichever evaluation happens to be running instead of the one addressed.
//!
//! This module keeps the single shared flag as the delivery mechanism and adds
//! the missing half — an immutable per-evaluation identity with a STICKY
//! cancellation intent, and a signal-safe mapping of SIGINT onto the evaluation
//! that was active when the signal arrived:
//!
//! * [`begin`] opens an evaluation, publishes its id as the active one, and
//!   discards a signal aimed at anything else. A SIGINT delivered at an idle
//!   prompt therefore cannot trip the next evaluation.
//! * [`cancel`] resolves the exact id. An id that has already finished reports
//!   the real outcome and raises nothing, so a late request can never reach a
//!   successor evaluation.
//! * [`reassert`] re-raises the shared flag while intent is still set. The
//!   evaluator CLEARS the flag as it converts it into an error, so without this
//!   a Mix `try`/`catch` around cancelled work would swallow the intent and run
//!   on. A caught interruption must not clear cancellation intent.
//!
//! What this cannot do is documented rather than papered over: pre-emption is
//! cooperative at the evaluator's existing checkpoints and at whatever polling
//! a captured runner already performs. A builtin blocked in a synchronous
//! syscall is not interrupted by any of this, and the guarantee table in
//! `docs/mix/` says so.

use std::sync::atomic::{AtomicBool, AtomicU64, AtomicU8, Ordering};
use std::sync::{Arc, Mutex};

/// How many finished evaluations stay resolvable. A cancel that loses the race
/// with completion must be able to answer "it finished" rather than "unknown".
const RETAINED: usize = 64;

/// Why an evaluation was cancelled. Ordered so a later, stronger source can
/// overwrite a weaker one but never the reverse.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Source {
    Request,
    Signal,
}
impl Source {
    fn code(self) -> u8 {
        match self {
            Self::Request => 1,
            Self::Signal => 2,
        }
    }
    fn from_code(code: u8) -> Option<Self> {
        match code {
            1 => Some(Self::Request),
            2 => Some(Self::Signal),
            _ => None,
        }
    }
}

/// The honest answer to "did my cancellation do anything?".
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Outcome {
    /// Intent recorded against the evaluation while it was still running.
    /// Delivery is cooperative; this is not a promise that it stopped.
    Requested,
    /// The evaluation had already finished. Nothing was signalled, and no
    /// successor was affected.
    AlreadyFinished,
    /// No such evaluation is known — never started, or aged out of the
    /// retained window.
    Unknown,
}

/// Immutable identity plus sticky intent. Never reused: ids come from the
/// caller's own monotonic counter and a finished evaluation keeps its record
/// until it ages out.
#[derive(Debug)]
pub struct Evaluation {
    id: u64,
    requested: AtomicBool,
    source: AtomicU8,
    finished: AtomicBool,
}
impl Evaluation {
    pub fn id(&self) -> u64 {
        self.id
    }
    /// Sticky: set once, never cleared by a caught interruption or by the
    /// evaluator consuming the shared flag.
    pub fn cancel_requested(&self) -> bool {
        self.requested.load(Ordering::Relaxed)
    }
    pub fn source(&self) -> Option<Source> {
        Source::from_code(self.source.load(Ordering::Relaxed))
    }
    pub fn finished(&self) -> bool {
        self.finished.load(Ordering::Relaxed)
    }
    fn request(&self, source: Source) -> bool {
        // Strongest source wins; `fetch_max` keeps a Signal from being
        // downgraded by a later Request on the same evaluation.
        self.source.fetch_max(source.code(), Ordering::Relaxed);
        !self.requested.swap(true, Ordering::Relaxed)
    }
}

static REGISTRY: Mutex<Vec<Arc<Evaluation>>> = Mutex::new(Vec::new());
/// Id of the evaluation currently running, or 0 for none. Read from the SIGINT
/// handler, so it is only ever touched through relaxed atomics.
static ACTIVE: AtomicU64 = AtomicU64::new(0);
/// Set by the signal handler; `SIGNAL_TARGET` names the evaluation it was
/// aimed at (0 = the shell was idle).
static SIGNAL_LATCH: AtomicBool = AtomicBool::new(false);
static SIGNAL_TARGET: AtomicU64 = AtomicU64::new(0);

/// SIGINT ingress. Called from signal context: two relaxed stores and nothing
/// else — no allocation, no locking, no reentrant library calls.
///
/// The target is sampled BEFORE the latch is raised so a reader that observes
/// the latch also observes a target that was current at or before the signal.
pub fn signal_arrived() {
    SIGNAL_TARGET.store(ACTIVE.load(Ordering::Relaxed), Ordering::Relaxed);
    SIGNAL_LATCH.store(true, Ordering::Relaxed);
}

fn raise() {
    if let Some(flag) = crate::interrupt::INTERRUPT_FLAG.get() {
        flag.store(true, Ordering::SeqCst);
    }
}
fn lower() {
    if let Some(flag) = crate::interrupt::INTERRUPT_FLAG.get() {
        flag.store(false, Ordering::SeqCst);
    }
}

/// Scope guard for one evaluation. Dropping it closes the evaluation: no later
/// cancellation can raise the shared flag on its behalf, and any flag this
/// evaluation's intent was holding up is lowered so the next prompt starts
/// clean.
pub struct Guard(Arc<Evaluation>);
impl Guard {
    pub fn evaluation(&self) -> &Arc<Evaluation> {
        &self.0
    }
}
impl Drop for Guard {
    fn drop(&mut self) {
        self.0.finished.store(true, Ordering::Relaxed);
        ACTIVE.store(0, Ordering::Relaxed);
        // A signal aimed at this evaluation dies with it rather than being
        // inherited by whatever runs next.
        if SIGNAL_TARGET.load(Ordering::Relaxed) == self.0.id {
            SIGNAL_LATCH.store(false, Ordering::Relaxed);
        }
        if self.0.cancel_requested() {
            lower();
        }
    }
}

/// Open an evaluation under `id`. Ids must be unique for the life of the
/// process; the shell uses its reducer's command id, which only ever advances.
pub fn begin(id: u64) -> Guard {
    let evaluation = Arc::new(Evaluation {
        id,
        requested: AtomicBool::new(false),
        source: AtomicU8::new(0),
        finished: AtomicBool::new(false),
    });
    {
        let mut registry = REGISTRY.lock().unwrap_or_else(|e| e.into_inner());
        if registry.len() >= RETAINED {
            // Oldest first, and never evict something still running.
            if let Some(index) = registry.iter().position(|e| e.finished()) {
                registry.remove(index);
            } else {
                registry.remove(0);
            }
        }
        registry.push(evaluation.clone());
    }
    ACTIVE.store(id, Ordering::Relaxed);
    // A latch raised while nothing was running, or while a PREVIOUS evaluation
    // was running, is not this evaluation's. Discard it together with the
    // shared flag the signal handler set, or the interrupt would land on the
    // first checkpoint of an evaluation nobody aimed at.
    if SIGNAL_LATCH.load(Ordering::Relaxed) && SIGNAL_TARGET.load(Ordering::Relaxed) != id {
        SIGNAL_LATCH.store(false, Ordering::Relaxed);
        lower();
    }
    // A signal that arrived for this id before its first checkpoint still
    // applies; adopt it as real intent so it survives a catch.
    if SIGNAL_LATCH.load(Ordering::Relaxed) && SIGNAL_TARGET.load(Ordering::Relaxed) == id {
        evaluation.request(Source::Signal);
        raise();
    }
    Guard(evaluation)
}

fn find(id: u64) -> Option<Arc<Evaluation>> {
    REGISTRY
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .iter()
        .find(|e| e.id == id)
        .cloned()
}

/// Resolve `id` and record intent against THAT evaluation only.
pub fn cancel(id: u64) -> Outcome {
    let Some(evaluation) = find(id) else {
        return Outcome::Unknown;
    };
    if evaluation.finished() {
        return Outcome::AlreadyFinished;
    }
    evaluation.request(Source::Request);
    // Raise the shared flag only while this evaluation is the active one. A
    // request that lost the race to completion has already returned above; this
    // second check closes the window where it finished in between.
    if ACTIVE.load(Ordering::Relaxed) == id && !evaluation.finished() {
        raise();
        Outcome::Requested
    } else {
        Outcome::AlreadyFinished
    }
}

/// Report a known evaluation's cancellation state without changing it.
pub fn state(id: u64) -> Option<(bool, Option<Source>, bool)> {
    find(id).map(|e| (e.cancel_requested(), e.source(), e.finished()))
}

/// Called by the evaluator immediately after it converts the shared flag into
/// an error and clears it. While intent is still set on the active evaluation
/// the flag goes straight back up, so a `catch` cannot turn a cancellation into
/// a resumed script.
pub fn reassert() {
    let id = ACTIVE.load(Ordering::Relaxed);
    if id == 0 {
        return;
    }
    // Adopt a signal aimed at this evaluation before deciding, so the FIRST
    // interruption a signal causes is also recorded as sticky intent.
    if SIGNAL_LATCH.load(Ordering::Relaxed) && SIGNAL_TARGET.load(Ordering::Relaxed) == id {
        SIGNAL_LATCH.store(false, Ordering::Relaxed);
        if let Some(evaluation) = find(id) {
            evaluation.request(Source::Signal);
        }
    }
    if find(id).is_some_and(|e| e.cancel_requested() && !e.finished()) {
        raise();
    }
}

#[cfg(test)]
pub(crate) fn reset_for_test() {
    REGISTRY.lock().unwrap_or_else(|e| e.into_inner()).clear();
    ACTIVE.store(0, Ordering::Relaxed);
    SIGNAL_LATCH.store(false, Ordering::Relaxed);
    SIGNAL_TARGET.store(0, Ordering::Relaxed);
    lower();
}

#[cfg(test)]
mod tests {
    use super::*;

    fn flag() -> Arc<AtomicBool> {
        crate::interrupt::INTERRUPT_FLAG
            .get_or_init(|| Arc::new(AtomicBool::new(false)))
            .clone()
    }

    #[test]
    fn cancellation_never_reaches_a_successor_evaluation() {
        let _lock = crate::interrupt::TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let flag = flag();
        reset_for_test();
        let first = begin(1);
        assert_eq!(cancel(1), Outcome::Requested);
        assert!(flag.load(Ordering::SeqCst));
        drop(first);
        // The guard lowered the flag it was holding; a late cancel for the
        // finished id must not raise it again for whatever runs next.
        assert!(!flag.load(Ordering::SeqCst));
        let second = begin(2);
        assert_eq!(cancel(1), Outcome::AlreadyFinished);
        assert!(!flag.load(Ordering::SeqCst), "successor was affected");
        assert!(!second.evaluation().cancel_requested());
        drop(second);
        assert_eq!(cancel(9), Outcome::Unknown);
    }

    #[test]
    fn a_caught_interruption_does_not_clear_intent() {
        let _lock = crate::interrupt::TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let flag = flag();
        reset_for_test();
        let guard = begin(7);
        assert_eq!(cancel(7), Outcome::Requested);
        // The evaluator's checkpoint: consume the flag and clear it.
        assert!(flag.swap(false, Ordering::SeqCst));
        reassert();
        assert!(flag.load(Ordering::SeqCst), "intent must survive a catch");
        assert!(guard.evaluation().cancel_requested());
        assert_eq!(guard.evaluation().source(), Some(Source::Request));
        drop(guard);
    }

    #[test]
    fn a_signal_binds_to_the_evaluation_that_was_running() {
        let _lock = crate::interrupt::TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let flag = flag();
        reset_for_test();
        // Idle at the prompt: the signal has no target.
        signal_arrived();
        flag.store(true, Ordering::SeqCst);
        let next = begin(3);
        assert!(
            !flag.load(Ordering::SeqCst),
            "an idle-prompt SIGINT must not trip the next evaluation"
        );
        assert!(!next.evaluation().cancel_requested());
        // Now during evaluation 3.
        signal_arrived();
        flag.store(true, Ordering::SeqCst);
        assert!(flag.swap(false, Ordering::SeqCst));
        reassert();
        assert!(flag.load(Ordering::SeqCst));
        assert_eq!(next.evaluation().source(), Some(Source::Signal));
        drop(next);
        assert!(!flag.load(Ordering::SeqCst));
        // The signal died with its evaluation.
        let after = begin(4);
        assert!(!after.evaluation().cancel_requested());
        assert!(!flag.load(Ordering::SeqCst));
        drop(after);
    }

    #[test]
    fn a_signal_delivered_before_the_first_checkpoint_is_still_this_evaluations() {
        let _lock = crate::interrupt::TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let flag = flag();
        reset_for_test();
        ACTIVE.store(11, Ordering::Relaxed);
        signal_arrived();
        ACTIVE.store(0, Ordering::Relaxed);
        let guard = begin(11);
        assert!(guard.evaluation().cancel_requested());
        assert!(flag.load(Ordering::SeqCst));
        drop(guard);
    }

    #[test]
    fn the_retained_window_is_bounded_and_keeps_the_running_evaluation() {
        let _lock = crate::interrupt::TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        reset_for_test();
        let live = begin(1);
        for id in 2..(RETAINED as u64 + 40) {
            drop(begin(id));
        }
        let registry = REGISTRY.lock().unwrap_or_else(|e| e.into_inner());
        assert!(registry.len() <= RETAINED);
        assert!(
            registry.iter().any(|e| e.id == 1),
            "a running evaluation must never be evicted"
        );
        drop(registry);
        drop(live);
    }
}
