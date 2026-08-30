//! Cross-pillar event-counter `tracing-subscriber::Layer`
//! (plan §3.3, "the cross-pillar bit").
//!
//! The layer increments
//! `cosmix_log_events_total{level=<l>, target_root=<crate>}`
//! for every `tracing::Event` that reaches it. Installed AFTER
//! the registry-root `EnvFilter` in `init.rs`, so it counts
//! *admitted* events (the ones operators actually see); events
//! the filter rejects never traverse any layer. Plan §3.3
//! discusses why pre-filter counting is not in scope for v0.
//!
//! # Cardinality
//!
//! `target_root` is the **first `::`-separated segment** of the
//! event's `target`, which for Rust code is the crate name (the
//! plan's wording "first underscore-separated segment" is
//! informal — the examples `cosmix_maild`, `cosmix_dnsd`, `mix`
//! are the crate names, not pre-underscore-split fragments).
//! Bounded by workspace crate count (~25), independent of
//! submodule depth.
//!
//! `level` is the five `metrics::Level` strings `trace` / `debug`
//! / `info` / `warn` / `error` (lowercase, plan §3.3). Five
//! values × 25 crates = 125 series ceiling, well under any cap.
//!
//! # Cost
//!
//! Per admitted event: one `metrics::counter!` call. With the
//! recorder installed that resolves to one `Counter::from_arc`
//! lookup + one `AtomicU64` fetch-add. With **no** recorder
//! installed (the `--stats=off` standalone case) the
//! `metrics` facade short-circuits to a no-op `Counter`, so the
//! layer is safe to install unconditionally. The plan still
//! prefers omitting the layer entirely under `--stats=off`;
//! `init.rs` honours that.
//!
//! The layer holds no state — it is a unit struct.
//!
//! # Reserved-name interaction
//!
//! `cosmix_log_events_total` is NOT in `BUILTIN_GAUGE_RESERVED_NAMES`;
//! reserving it would block this layer's own writes. The
//! reservation list covers only the side-map process gauges
//! (`process_gauges.rs`). This counter is a normal Registry
//! counter, governed by the same cardinality cap as any
//! app-side counter.
//!
//! # No `register_*` race
//!
//! The first event arriving for a `(level, target_root)` pair
//! creates the Registry entry via `metrics::counter!()`. Concurrent
//! arrivals for the same pair coalesce into one entry per
//! `Registry`'s `get_or_create_counter` semantics. Concurrent
//! arrivals for *different* pairs each consume one admission
//! slot under the per-metric cardinality cap — at 125 ceiling
//! and a cap default of 1024 this is never the binding constraint.
//!
//! # Why a separate module
//!
//! The recorder is core stats infrastructure; the layer is the
//! single point at which `tracing` ↔ `metrics` cross-pillar
//! coupling lives. Keeping it in its own module (rather than
//! grafted onto `recorder.rs`) makes the coupling boundary
//! explicit and lets future cross-pillar layers (e.g. span-time
//! histograms) join here without bloating the recorder module.
//!
//! Both labels reach the hot path via `Label::from_static_parts`,
//! so the label **string values** are allocation-free per
//! admitted event: `tracing::Metadata::target()` is documented as
//! `&'static str` (it derives from the macro-call-site target
//! literal, see `tracing::Metadata::new`), and
//! `str::split("::").next()` preserves the `'static` lifetime, so
//! the segment slice is itself `&'static str` and can be passed
//! to `Label::from_static_parts` directly. The `vec![..]` that
//! holds the two `Label`s still allocates per event — the
//! `metrics::counter!` macro accepts a label sequence and there
//! is no zero-alloc shape exposed by the facade today. A counter
//! handle cache (one `Counter` per `(level, target_root)` pair
//! held in a static map) would remove that allocation; out of
//! scope for slice 4c — the 125-series ceiling and per-event
//! cost are both negligible at admitted-event rates.
//! Codex slice-4c round-2 NIT.

use metrics::{Label, counter};
use tracing::{Event, Subscriber};
use tracing_subscriber::Layer;
use tracing_subscriber::layer::Context;

/// Counter name written by this layer (plan §3 "built-in
/// counters" group).
pub(crate) const EVENT_COUNTER_NAME: &str = "cosmix_log_events_total";

/// Stateless event-counter layer. Constructed via
/// [`EventCounterLayer::new`]; install via
/// `tracing_subscriber::registry().with(...)`. The unit struct
/// has no allocations and no per-binary configuration — the
/// label dimensions are derived from each event.
#[derive(Default, Clone, Copy)]
pub(crate) struct EventCounterLayer;

impl EventCounterLayer {
    pub(crate) fn new() -> Self {
        Self
    }
}

impl<S> Layer<S> for EventCounterLayer
where
    S: Subscriber,
{
    fn on_event(&self, event: &Event<'_>, _ctx: Context<'_, S>) {
        let metadata = event.metadata();
        let level = level_str(metadata.level());
        let target_root = root_of(metadata.target());
        // `counter!` takes a `&str` for the metric name and a
        // sequence of `Label` for label pairs. The `Label`s
        // themselves hold `&'static str` views into the level
        // table and into `metadata.target()`, so the label
        // *string values* never allocate; the surrounding
        // `vec![..]` still allocates per admitted event — see
        // the module docstring for why a handle cache is out of
        // scope for slice 4c.
        counter!(
            EVENT_COUNTER_NAME,
            vec![
                Label::from_static_parts("level", level),
                // `target_root` slices `metadata.target()`, which is
                // `&'static str` (see `tracing::Metadata`'s
                // documentation — the target is supplied at the
                // macro call site as a string literal and stored
                // verbatim). `str::split("::").next()` preserves the
                // `'static` lifetime, so the slice we hand to
                // `Label::from_static_parts` is itself `&'static str`
                // and the label *value* avoids allocation (Codex
                // round-1 NIT + round-3 NIT narrowing).
                Label::from_static_parts("target_root", target_root),
            ]
        )
        .increment(1);
    }
}

/// Map a `tracing::Level` to the lowercase string the plan
/// commits to for the `level` label (plan §3.3). Returns
/// `'static` slices so the `Label::from_static_parts` fast path
/// is available — no allocation per event.
fn level_str(level: &tracing::Level) -> &'static str {
    match *level {
        tracing::Level::TRACE => "trace",
        tracing::Level::DEBUG => "debug",
        tracing::Level::INFO => "info",
        tracing::Level::WARN => "warn",
        tracing::Level::ERROR => "error",
    }
}

/// First `::`-separated segment of a tracing target. For Rust
/// code the segment is the crate name (`cosmix_maild`, `mix`,
/// etc.) — Rust crate names are underscored, but the separator
/// between crate and module path is `::`. Empty target degrades
/// to `"unknown"` rather than `""` so cardinality stays bounded
/// to a known label set.
fn root_of(target: &str) -> &str {
    if target.is_empty() {
        return "unknown";
    }
    target.split("::").next().unwrap_or(target)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::stats::recorder::StatsRecorderBuilder;
    use crate::stats::snapshot::snapshot_from_inner;
    use crate::stats::types::{SeriesLabels, SeriesValue};
    use std::sync::Arc;
    use tracing::Level;
    use tracing_subscriber::layer::SubscriberExt;

    #[test]
    fn level_str_is_stable_lowercase() {
        // Plan §3.3 commits the values; an upstream rename would
        // be a wire break for shipped JSONL/Prometheus output.
        assert_eq!(level_str(&Level::TRACE), "trace");
        assert_eq!(level_str(&Level::DEBUG), "debug");
        assert_eq!(level_str(&Level::INFO), "info");
        assert_eq!(level_str(&Level::WARN), "warn");
        assert_eq!(level_str(&Level::ERROR), "error");
    }

    #[test]
    fn root_of_extracts_crate_segment() {
        assert_eq!(root_of("cosmix_maild::scoring::run"), "cosmix_maild");
        assert_eq!(root_of("cosmix_dnsd"), "cosmix_dnsd");
        assert_eq!(root_of("mix"), "mix");
        // Empty degrades to "unknown" rather than "".
        assert_eq!(root_of(""), "unknown");
    }

    #[test]
    fn event_counter_is_classified_safe_after_recorder_build() {
        // Anti-regression for Codex slice-4c round-1 MAJOR: without
        // an explicit Safe classification, JSONL/Bus would hash the
        // `level` / `target_root` labels (Restricted is the default
        // per `classify.rs`). The builder is responsible for
        // registering the built-in family classifications.
        let _rec = StatsRecorderBuilder::new("classify-test").build();
        assert_eq!(
            crate::stats::classify::sensitivity_of(EVENT_COUNTER_NAME),
            crate::stats::types::LabelSensitivity::Safe,
            "{EVENT_COUNTER_NAME} must be Safe so level/target_root labels reach JSONL/Bus verbatim"
        );
        // The other built-ins also need to be Safe.
        for name in [
            "cosmix_process_uptime_seconds",
            "cosmix_process_memory_kb",
            "cosmix_process_open_fds",
        ] {
            assert_eq!(
                crate::stats::classify::sensitivity_of(name),
                crate::stats::types::LabelSensitivity::Safe,
                "built-in family {name} must be Safe per plan §3.3.2"
            );
        }
        // The cardinality-drops counter is INTENTIONALLY left at
        // the Restricted default — `metrics` permits dynamic metric
        // names so `rejected.name()` is not statically bounded
        // (Codex slice-4c round-2 MAJOR). Anti-regression: if a
        // future maintainer re-adds it to the Safe list,
        // `classify_built_in_metrics` flips this assertion.
        assert_eq!(
            crate::stats::classify::sensitivity_of("cosmix_stats_cardinality_drops_total"),
            crate::stats::types::LabelSensitivity::Restricted,
            "drops counter must NOT be Safe — rejected.name() is not compile-time bounded"
        );
    }

    #[test]
    fn events_increment_counter_per_level_and_target_root() {
        // Build a recorder; install via `metrics::with_local_recorder`
        // so the layer's `counter!()` calls land here and not in any
        // process-global recorder (which is per-process one-shot and
        // would race other tests). The Arc handle is the metrics-rs
        // accepted shape for scoped recorder swaps.
        let recorder = Arc::new(StatsRecorderBuilder::new("event-counter-test").build());
        let registry = tracing_subscriber::registry().with(EventCounterLayer::new());

        metrics::with_local_recorder(&*recorder, || {
            tracing::subscriber::with_default(registry, || {
                tracing::error!(target: "cosmix_maild::scoring", "boom");
                tracing::error!(target: "cosmix_maild::imap", "boom2");
                tracing::info!(target: "mix", "tick");
                tracing::info!(target: "mix", "tick2");
            });
        });

        let snap = snapshot_from_inner(&recorder.inner);
        let family = snap
            .metrics
            .iter()
            .find(|m| m.name == EVENT_COUNTER_NAME)
            .expect("event counter family present after emitted events");
        assert_eq!(
            family.series.len(),
            2,
            "two series for two distinct (level, target_root) tuples"
        );

        let by_labels: std::collections::HashMap<_, _> = family
            .series
            .iter()
            .map(|s| {
                let labels = match &s.labels {
                    SeriesLabels::Raw(m) => m.clone(),
                    SeriesLabels::Hash(_) => panic!("local snapshot returns Raw labels"),
                };
                let value = match &s.value {
                    SeriesValue::Counter(v) => *v,
                    other => panic!("expected counter, got {other:?}"),
                };
                (
                    (
                        labels.get("level").cloned().unwrap_or_default(),
                        labels.get("target_root").cloned().unwrap_or_default(),
                    ),
                    value,
                )
            })
            .collect();
        assert_eq!(
            by_labels
                .get(&("error".into(), "cosmix_maild".into()))
                .copied(),
            Some(2)
        );
        assert_eq!(
            by_labels.get(&("info".into(), "mix".into())).copied(),
            Some(2)
        );
    }
}
