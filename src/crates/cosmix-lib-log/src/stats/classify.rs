//! Label-sensitivity classification registry (plan §3.3.2).
//!
//! Each metric family declares its `LabelSensitivity` at startup via
//! [`classify`]. The recorder, JSONL writer, and Bus snapshot verb
//! consult [`sensitivity_of`] to decide whether to write raw label
//! values or the hashed form.
//!
//! # Defaults are deliberately strict
//!
//! Unclassified families resolve to `Restricted` — the safer side.
//! The plan calls this out: under-restriction means user-controlled
//! bytes silently flow into every shipping pipeline the substrate
//! enables (Loki, Prometheus); over-restriction means an operator
//! sees `labels_hash` on a family that should have been `Safe` and
//! reclassifies it with one line of code.
//!
//! # Late re-classification is intentionally allowed
//!
//! P6 (`cosmix-lib-mix`) and P7 (daemon migration) call
//! `classify(name, Safe)` from `init_main`-equivalent paths *after*
//! the recorder is constructed but *before* any record is written.
//! Re-classifying a family after records have been written is not
//! a logic error here — the registry just publishes the new value;
//! sink correctness depends on call ordering, which is enforced at
//! the binary's init point, not by this module. The recorder logs a
//! `warn` event when a family is re-classified after the first
//! record is written (see slice 2b); the registry itself is permissive.

use crate::stats::types::LabelSensitivity;
use std::collections::HashMap;
use std::sync::RwLock;

/// Process-wide map of metric-name → sensitivity. Lazily initialised
/// on first access; classification calls take the write lock so the
/// recorder hot path (read-only `sensitivity_of`) is uncontended
/// under steady-state writes.
fn registry() -> &'static RwLock<HashMap<&'static str, LabelSensitivity>> {
    static REGISTRY: std::sync::OnceLock<RwLock<HashMap<&'static str, LabelSensitivity>>> =
        std::sync::OnceLock::new();
    REGISTRY.get_or_init(|| RwLock::new(HashMap::new()))
}

/// Declare the sensitivity classification for a metric family.
///
/// Idempotent — calling twice with the same `(name, sensitivity)`
/// pair is a no-op. Calling twice with *different* sensitivities
/// publishes the most recent value; the recorder is responsible for
/// noticing and emitting a `warn` if records have already been
/// written under the old classification (slice 2b).
///
/// `name` must be a `'static` string — every callsite is a
/// compile-time metric name registered alongside
/// `metrics::describe_*` calls, so the lifetime requirement is free.
pub fn classify(name: &'static str, sensitivity: LabelSensitivity) {
    let mut map = registry()
        .write()
        .expect("classify registry RwLock poisoned");
    map.insert(name, sensitivity);
}

/// Look up the classification for `name`. Returns `Restricted` if no
/// `classify` call has covered this metric family — the safer
/// default per plan §3.3.2.
///
/// `name` is `&str` (not `&'static str`) because lookup callers
/// hold the metric name borrowed from the `metrics::Key` (which is
/// a `metrics::SharedString`, internally a `Cow`).
pub fn sensitivity_of(name: &str) -> LabelSensitivity {
    let map = registry()
        .read()
        .expect("classify registry RwLock poisoned");
    map.get(name)
        .copied()
        .unwrap_or(LabelSensitivity::Restricted)
}

/// Returns the default sensitivity for any unclassified family.
/// Useful in tests to assert the contract without hard-coding the
/// `Restricted` literal.
pub fn classify_default() -> LabelSensitivity {
    LabelSensitivity::Restricted
}

/// Run `f` while holding the classification registry's read lock.
///
/// This is the security primitive that
/// [`crate::stats::snapshot_dispatch`] uses to make the audit + the
/// registry snapshot + sensitivity projection consistent against a
/// single policy view. Concurrent `classify()` calls (which take the
/// write lock) block until `f` returns, so no `Safe → Restricted`
/// reclassification can land between the audit decision and the
/// projection of the recorder data. (Codex slice-5a round-2 MAJOR
/// fix — releasing the lock between capture and recorder-snapshot
/// allowed a reclassification race.)
///
/// The closure receives a `&HashMap<&'static str, LabelSensitivity>`
/// containing only *classified* families. Callers must apply the
/// unclassified-default rule (`Restricted`) themselves — that rule
/// is the entire reason for the safer-side default
/// ([`classify_default`]).
///
/// `f` should not call `classify()` itself: doing so would deadlock
/// because the write lock isn't reentrant and is held to be exclusive
/// with this reader.
pub fn with_classifications<R>(f: impl FnOnce(&HashMap<&'static str, LabelSensitivity>) -> R) -> R {
    let map = registry()
        .read()
        .expect("classify registry RwLock poisoned");
    f(&map)
}

/// Built-in metric families classified `Safe` at recorder build
/// time. Every entry's labels are compile-time bounded (level
/// values, crate names) — no user-controlled bytes ever flow
/// through these labels — so the JSONL writer and Bus snapshot
/// may emit them verbatim instead of behind a `labels_hash`
/// (plan §3.3.2 + §3 "built-in counters" group; Codex slice-4c
/// round-1 MAJOR — without this, the JSONL path would have
/// hashed `cosmix_log_events_total`'s `level` / `target_root`
/// labels despite the plan marking them safe).
///
/// # Notable omission — `cosmix_stats_cardinality_drops_total`
///
/// The cardinality-drops counter is **deliberately not on the Safe
/// list** even though plan §3.3.1 leans on the recorder rejecting
/// unknown families: the `metrics` facade permits dynamic metric
/// names (any `Cow<'static, str>` through `Key::from_parts`), so
/// `rejected.name()` is not statically bounded by the binary's
/// compile-time metric vocabulary. Leaving it `Restricted` means
/// the rejected family name appears as `labels_hash` in JSONL/Bus;
/// operators correlate by hash digest, which is the cross-surface
/// pattern §3.3.1 already documents. Codex slice-4c round-2 MAJOR.
///
/// Calling more than once is harmless (`classify` is idempotent
/// for the same `(name, sensitivity)` pair).
pub(crate) fn classify_built_in_metrics() {
    let safe = &[
        // Plan §3 "built-in counters":
        "cosmix_log_events_total",          // labels: level, target_root
        "cosmix_process_uptime_seconds",    // no labels
        "cosmix_process_memory_kb",         // no labels
        "cosmix_process_open_fds",          // no labels
        "cosmix_process_cpu_seconds_total", // no labels; counter semantics
    ];
    for name in safe {
        classify(name, LabelSensitivity::Safe);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // Tests share the process-wide registry; use distinct metric
    // names per test to avoid cross-pollution.

    #[test]
    fn unknown_family_defaults_to_restricted() {
        assert_eq!(
            sensitivity_of("unknown_family_in_classify_default_test"),
            LabelSensitivity::Restricted
        );
        assert_eq!(classify_default(), LabelSensitivity::Restricted);
    }

    #[test]
    fn classify_safe_then_lookup() {
        classify("classify_safe_then_lookup_metric", LabelSensitivity::Safe);
        assert_eq!(
            sensitivity_of("classify_safe_then_lookup_metric"),
            LabelSensitivity::Safe
        );
    }

    #[test]
    fn reclassify_publishes_most_recent() {
        classify("reclassify_metric", LabelSensitivity::Safe);
        assert_eq!(sensitivity_of("reclassify_metric"), LabelSensitivity::Safe);
        classify("reclassify_metric", LabelSensitivity::Restricted);
        assert_eq!(
            sensitivity_of("reclassify_metric"),
            LabelSensitivity::Restricted
        );
    }
}
