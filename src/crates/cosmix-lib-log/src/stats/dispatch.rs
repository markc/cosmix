//! Substrate-side handler for the `<svc>.stats.snapshot` Bus verb
//! (plan §4.2).
//!
//! This module is the **pure** half of the verb — it operates on an
//! already-parsed [`SnapshotRequest`] and returns either the snapshot
//! shape from `types.rs` (with sensitivity-correct label representation)
//! or a [`SnapshotError`] capturing the cap-required / parse-rejected
//! cases. The wire-side Bus-message parser + response serializer is
//! slice 5b's concern; keeping the dispatch logic Bus-agnostic means
//! the cap-gating contract — the load-bearing security piece — is
//! unit-testable without standing up a noded broker.
//!
//! # The cap-gating contract (plan §4.2)
//!
//! Three caps shape the verb's behavior:
//!
//! 1. **`stats.snapshot:<svc>`** (operator-class default) — required
//!    to call the verb at all. Enforcement happens at the wire layer
//!    (slice 5b) and is *not* this module's concern — it gates verb
//!    admission, not response shape. This module assumes the caller
//!    has cleared that bar.
//!
//! 2. **`stats.snapshot:raw-labels:<svc>`** (host-class default) —
//!    unlocks raw label values on `LabelSensitivity::Restricted`
//!    families. Without it, the response replaces every Restricted
//!    family's `SeriesLabels::Raw` with `SeriesLabels::Hash` (the
//!    same FxHash digest that appears in JSONL records and
//!    cardinality-drop warnings, so an operator can correlate
//!    across surfaces). Tracked here by [`SnapshotRequest::has_raw_labels_cap`].
//!
//! # The probe-oracle defense
//!
//! The plan calls out that a `labels?` filter that binds a key to a
//! literal value is a side-channel on a Restricted family: a caller
//! without the raw-labels cap could guess values and observe which
//! hashed responses come back. The dispatch enforces this gate
//! WITHOUT touching the live registry — the decision is made from
//! a classification view held under the classify read lock for the
//! duration of the call (compile-time bounded family names), so
//! timing on the gate cannot leak whether a Restricted family
//! currently has any matching series, AND concurrent `classify()`
//! calls cannot land a `Safe → Restricted` reclassification between
//! the audit and the recorder snapshot. (Codex slice-5a round-2
//! MAJOR — round-1 cloned the map and released the lock, which left
//! the reclassification window open.)
//!
//! Concretely, a [`LabelFilter::KeyEquals`] without
//! `has_raw_labels_cap` is **rejected unless** the request's
//! [`MetricPattern`] is `Exact(name)` AND the classification view
//! resolves `name` to `Safe`. Prefix / Suffix globs and the no-glob
//! case both potentially admit unclassified (= default Restricted)
//! families, so a conservative reject is the only way to keep the
//! cap-gate independent of the live registry. (Codex slice-5a
//! round-1 BLOCKERs + MAJOR fix.)
//!
//! [`LabelFilter::HasKey`] (key-presence only, no value bind) is
//! permitted on Restricted families at the operator-class cap because
//! key names are bounded by compile-time metric definitions and carry
//! no user-controlled bytes (plan §4.2).
//!
//! [`labels_hash?`](SnapshotRequest::labels_hash) filters accept the
//! canonical 16-hex-char FxHash strings produced by
//! [`crate::stats::labels_hash`] and need no cap upgrade — the hash is
//! the redacted form already shipped on disk.
//!
//! # What is intentionally NOT here
//!
//! The 1 MB response-size cap (plan §4.2) lives at the wire layer —
//! this module returns the structured `Snapshot`; size enforcement
//! requires the serialized JSON bytes and lives in the Bus-response
//! serializer in slice 5b. Same for the `metric?`/`labels?` /
//! `labels_hash?` *string parser* — only the parsed enum form crosses
//! into this module.

use crate::stats::classify::with_classifications;
use crate::stats::labels_hash::labels_hash as compute_labels_hash;
use crate::stats::recorder::StatsRecorder;
use crate::stats::snapshot::snapshot_from_inner;
use crate::stats::types::{LabelSensitivity, MetricFamily, Series, SeriesLabels, Snapshot};
use std::collections::HashMap;

/// Parsed `<svc>.stats.snapshot` request. The wire-layer parser
/// (slice 5b) constructs this; the dispatch never sees Bus message
/// types directly.
#[derive(Debug, Clone, Default)]
pub struct SnapshotRequest {
    /// `metric?` header: optional glob restricting which metric
    /// families appear in the response. `None` = no family filter
    /// (return everything that passes other filters).
    pub metric: Option<MetricPattern>,
    /// `labels?` header: zero or more label predicates. **All**
    /// must match for a series to appear in the response (AND
    /// semantics — there is no OR form at the Bus surface). On
    /// Restricted families, the cap-gating rules in the module
    /// docstring apply.
    pub labels: Vec<LabelFilter>,
    /// `labels_hash?` header: zero or more 16-hex-char FxHash
    /// digests. A series matches if its labels_hash (computed via
    /// [`crate::stats::labels_hash`]) is in this set. Empty list =
    /// no labels_hash filter.
    pub labels_hash: Vec<String>,
    /// Caller holds `stats.snapshot:raw-labels:<svc>`. Controls
    /// whether Restricted families return raw labels and whether
    /// `KeyEquals` filters on Restricted families are accepted.
    pub has_raw_labels_cap: bool,
}

/// Glob pattern over metric family names. The wire-layer parser
/// rejects malformed globs; the dispatch sees only the parsed form.
///
/// The supported shapes mirror the documented examples in plan §4.2
/// (`maild_*`, `*_total`, exact). No middle-`*` form — keeps the
/// matcher trivial and the wire grammar bounded.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MetricPattern {
    /// Family name must equal this string exactly.
    Exact(String),
    /// Family name must start with this prefix (the substring
    /// before the trailing `*` in `maild_*`).
    Prefix(String),
    /// Family name must end with this suffix (the substring after
    /// the leading `*` in `*_total`).
    Suffix(String),
}

/// One predicate inside a parsed `labels?` filter. See module
/// docstring for the cap-gating implications.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LabelFilter {
    /// Series must carry the label key (any value). Permitted on
    /// Restricted families at the operator-class cap — key names
    /// are compile-time bounded.
    HasKey(String),
    /// Series's label value for `key` must equal `value` exactly.
    /// On Restricted families this requires `has_raw_labels_cap`
    /// (probe-oracle defense — see module docstring).
    KeyEquals { key: String, value: String },
}

/// What the dispatch can reject with. The wire layer surfaces these
/// as the plan §4.2 `Err::*` error codes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SnapshotError {
    /// The request requires a capability the caller doesn't hold.
    /// `needed` is the capability name (e.g.
    /// `"stats.snapshot:raw-labels:<svc>"`); `reason` is the
    /// human-readable explanation surfaced to the operator.
    CapabilityRequired { needed: String, reason: String },
}

/// Apply a parsed [`SnapshotRequest`] to the recorder's current
/// registry state and return a filtered, sensitivity-projected
/// [`Snapshot`].
///
/// Ordering of operations (all under one held classification read lock):
///
/// 1. **Hold the classification read lock** for the rest of the call
///    via [`with_classifications`]. Concurrent `classify()` calls
///    (write lock) block until the closure returns, so no
///    `Safe → Restricted` reclassification can land between the
///    audit decision and the projection — the audit's view of the
///    sensitivity landscape and the projection's view are guaranteed
///    identical (Codex slice-5a round-2 MAJOR fix; round-1 MAJOR
///    cloned-and-released the map, which left a window where a
///    classify call could land between capture and recorder snapshot).
/// 2. **Cap-gating** — reject `KeyEquals` filters without
///    `has_raw_labels_cap` unless the `MetricPattern` is `Exact(n)`
///    AND `classifications[n]` is `Safe`. Pure metadata decision —
///    NEVER scans the registry, so timing on the gate cannot leak
///    series existence (Codex slice-5a round-1 BLOCKER #1).
/// 3. **Snapshot** — read the recorder via [`snapshot_from_inner`]
///    exactly once, while still holding the classification lock. All
///    subsequent filtering/projection operates on THIS captured data;
///    no second read happens (Codex slice-5a round-1 BLOCKER #2).
/// 4. **Filter** — apply `metric` glob, `labels` predicates, and
///    `labels_hash` set membership. `labels_hash` matching computes
///    each series's hash via the canonical encoding so the same
///    digests match across JSONL and Bus surfaces.
/// 5. **Project** — for every family that the held classification
///    map resolves to `Restricted` (or that is unclassified, defaulting
///    to `Restricted`) without the raw cap, replace
///    `SeriesLabels::Raw` with `SeriesLabels::Hash`. Safe families
///    and authorized Restricted families pass through unchanged.
pub fn snapshot_dispatch(
    recorder: &StatsRecorder,
    request: &SnapshotRequest,
) -> Result<Snapshot, SnapshotError> {
    // Hold the classification read lock across audit + recorder
    // snapshot + projection so concurrent `classify()` (write lock)
    // calls block until we return. This is the security primitive
    // that makes the audit's "Safe family" view and the projection's
    // sensitivity decisions provably consistent (Codex slice-5a
    // round-2 MAJOR fix). The closure must not call `classify()` —
    // the write lock isn't reentrant and would deadlock; nothing on
    // the snapshot/filter/project paths does.
    with_classifications(|classifications| {
        // Cap-gating — pure metadata decision, no registry access.
        audit_probe_oracle(classifications, request)?;

        // Snapshot the recorder (raw labels, full registry) — once,
        // while still holding the classification read lock.
        let mut snap = snapshot_from_inner(&recorder.inner);

        // Apply metric glob.
        snap.metrics
            .retain(|family| family_matches_metric_filter(&family.name, request.metric.as_ref()));

        // Filter labels + labels_hash, then project sensitivity. The
        // same held `classifications` view drives projection so the
        // audit's view of "Safe family" stays consistent with what
        // the response says.
        for family in &mut snap.metrics {
            let sensitivity = classified_sensitivity(classifications, &family.name);
            filter_and_project_family(family, sensitivity, request);
        }

        // Drop families that emptied after filtering — operator output
        // shouldn't carry families with zero series.
        snap.metrics.retain(|f| !f.series.is_empty());

        Ok(snap)
    })
}

/// Look up `name` in a captured classification snapshot. Returns
/// `Restricted` (the safer-side default) when `name` is not present
/// in the map — same contract as `classify::sensitivity_of` but
/// reading from the caller-held snapshot instead of the live
/// registry.
fn classified_sensitivity(
    classifications: &HashMap<&'static str, LabelSensitivity>,
    name: &str,
) -> LabelSensitivity {
    classifications
        .get(name)
        .copied()
        .unwrap_or(LabelSensitivity::Restricted)
}

/// Probe-oracle defense — purely metadata, never reads the live
/// registry. The gate accepts a `KeyEquals` filter only when the
/// dispatch can *prove* every series in scope belongs to a Safe
/// family. The only request shape that supplies that proof from
/// pure metadata is `MetricPattern::Exact(name)` with `name`
/// classified `Safe` in the captured snapshot. Any other shape
/// (Prefix, Suffix, no glob, Exact on an unclassified or Restricted
/// name) gets rejected.
///
/// Why so conservative? The alternative — "Prefix is fine if every
/// classified family matching the prefix is Safe" — fails for
/// unclassified families that may appear in the registry but not
/// in classifications; those default to Restricted. Walking the
/// live registry to enumerate them would re-introduce the timing
/// channel this audit exists to close. The conservative shape
/// keeps the cap-gate dependent on nothing but the request and
/// the bounded classification surface. (Codex slice-5a round-1
/// BLOCKERs.)
fn audit_probe_oracle(
    classifications: &HashMap<&'static str, LabelSensitivity>,
    request: &SnapshotRequest,
) -> Result<(), SnapshotError> {
    if request.has_raw_labels_cap {
        return Ok(());
    }
    let has_key_equals = request
        .labels
        .iter()
        .any(|f| matches!(f, LabelFilter::KeyEquals { .. }));
    if !has_key_equals {
        return Ok(());
    }
    let safe_exact = matches!(
        &request.metric,
        Some(MetricPattern::Exact(name))
            if classifications.get(name.as_str()).copied()
                == Some(LabelSensitivity::Safe)
    );
    if safe_exact {
        return Ok(());
    }
    let reason = match &request.metric {
        Some(MetricPattern::Exact(name)) => {
            format!("raw label filter on family {name} (not classified Safe)")
        }
        Some(MetricPattern::Prefix(p)) => format!(
            "raw label filter on metric glob {p}* — Prefix admits possibly-Restricted families"
        ),
        Some(MetricPattern::Suffix(s)) => format!(
            "raw label filter on metric glob *{s} — Suffix admits possibly-Restricted families"
        ),
        None => {
            "raw label filter without a metric pin — request admits possibly-Restricted families"
                .to_string()
        }
    };
    Err(SnapshotError::CapabilityRequired {
        needed: "stats.snapshot:raw-labels:<svc>".to_string(),
        reason,
    })
}

fn family_matches_metric_filter(name: &str, pattern: Option<&MetricPattern>) -> bool {
    match pattern {
        None => true,
        Some(MetricPattern::Exact(s)) => name == s.as_str(),
        Some(MetricPattern::Prefix(p)) => name.starts_with(p.as_str()),
        Some(MetricPattern::Suffix(s)) => name.ends_with(s.as_str()),
    }
}

fn filter_and_project_family(
    family: &mut MetricFamily,
    sensitivity: LabelSensitivity,
    request: &SnapshotRequest,
) {
    // Apply labels + labels_hash filters first; project sensitivity
    // last so the hash computation in `labels_hash` matching sees
    // the raw labels (which is what JSONL hashes too).
    family
        .series
        .retain(|s| series_matches_labels_filter(s, &request.labels));
    if !request.labels_hash.is_empty() {
        family
            .series
            .retain(|s| series_matches_labels_hash_filter(s, &request.labels_hash));
    }

    if sensitivity == LabelSensitivity::Restricted && !request.has_raw_labels_cap {
        for series in &mut family.series {
            if let SeriesLabels::Raw(map) = &series.labels {
                let digest = compute_labels_hash(map);
                series.labels = SeriesLabels::Hash(digest);
            }
        }
    }
}

fn series_matches_labels_filter(series: &Series, filters: &[LabelFilter]) -> bool {
    let SeriesLabels::Raw(map) = &series.labels else {
        // snapshot_from_inner produces only Raw; defensive guard
        // for a future caller that hands in a pre-projected snap.
        return filters.is_empty();
    };
    filters.iter().all(|f| match f {
        LabelFilter::HasKey(k) => map.contains_key(k.as_str()),
        LabelFilter::KeyEquals { key, value } => {
            map.get(key.as_str()).map(|s| s.as_str()) == Some(value.as_str())
        }
    })
}

fn series_matches_labels_hash_filter(series: &Series, accept: &[String]) -> bool {
    let SeriesLabels::Raw(map) = &series.labels else {
        return false;
    };
    let digest = compute_labels_hash(map);
    accept.iter().any(|h| h == &digest)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::stats::classify::classify;
    use crate::stats::recorder::StatsRecorderBuilder;
    use crate::stats::types::SeriesValue;
    use metrics::{Counter, Key, Label, Recorder, SharedString};

    /// Build a recorder, register a counter under (name, labels),
    /// increment by `n`, and return the recorder. The helper avoids
    /// the `metrics::with_local_recorder` thread-local seam since
    /// the dispatch operates on the recorder directly.
    fn build_with_counter(
        service: &str,
        name: &'static str,
        labels: &[(&'static str, &'static str)],
        n: u64,
    ) -> crate::stats::recorder::StatsRecorder {
        let recorder = StatsRecorderBuilder::new(service).build();
        let label_vec: Vec<Label> = labels
            .iter()
            .map(|(k, v)| Label::from_static_parts(k, v))
            .collect();
        let key = Key::from_parts(SharedString::const_str(name), label_vec);
        let counter: Counter = recorder.register_counter(
            &key,
            &metrics::Metadata::new(name, metrics::Level::INFO, None),
        );
        counter.increment(n);
        recorder
    }

    fn req_default() -> SnapshotRequest {
        SnapshotRequest::default()
    }

    #[test]
    fn safe_family_returns_raw_labels_without_cap() {
        // Safe family + no raw-labels cap = raw labels passthrough.
        classify("dispatch_safe_passthrough_metric", LabelSensitivity::Safe);
        let inner = build_with_counter(
            "dispatch-test",
            "dispatch_safe_passthrough_metric",
            &[("k", "v")],
            5,
        );
        let snap = snapshot_dispatch(&inner, &req_default()).expect("ok");
        let family = snap
            .metrics
            .iter()
            .find(|m| m.name == "dispatch_safe_passthrough_metric")
            .expect("family present");
        assert_eq!(family.series.len(), 1);
        match &family.series[0].labels {
            SeriesLabels::Raw(m) => assert_eq!(m.get("k").map(String::as_str), Some("v")),
            SeriesLabels::Hash(_) => panic!("Safe family must return raw labels"),
        }
    }

    #[test]
    fn restricted_family_returns_hash_without_cap() {
        // Default classification is Restricted; build a non-built-in
        // counter and confirm dispatch projects Raw → Hash.
        let inner = build_with_counter(
            "dispatch-test",
            "dispatch_restricted_projection_metric",
            &[("user", "alice")],
            1,
        );
        let snap = snapshot_dispatch(&inner, &req_default()).expect("ok");
        let family = snap
            .metrics
            .iter()
            .find(|m| m.name == "dispatch_restricted_projection_metric")
            .expect("family present");
        match &family.series[0].labels {
            SeriesLabels::Hash(h) => assert_eq!(h.len(), 16, "FxHash digest is 16 hex chars"),
            SeriesLabels::Raw(_) => panic!("Restricted family without cap must hash labels"),
        }
    }

    #[test]
    fn restricted_family_returns_raw_with_cap() {
        let inner = build_with_counter(
            "dispatch-test",
            "dispatch_restricted_with_cap_metric",
            &[("user", "alice")],
            1,
        );
        let req = SnapshotRequest {
            has_raw_labels_cap: true,
            ..Default::default()
        };
        let snap = snapshot_dispatch(&inner, &req).expect("ok");
        let family = snap
            .metrics
            .iter()
            .find(|m| m.name == "dispatch_restricted_with_cap_metric")
            .expect("family present");
        match &family.series[0].labels {
            SeriesLabels::Raw(m) => assert_eq!(m.get("user").map(String::as_str), Some("alice")),
            SeriesLabels::Hash(_) => panic!("raw-labels cap must pass raw"),
        }
    }

    #[test]
    fn key_equals_filter_without_metric_pin_is_rejected_without_cap() {
        // Probe-oracle defense (plan §4.2, slice-5a round-1 BLOCKERs).
        // A KeyEquals filter with no metric pin admits possibly-
        // Restricted (unclassified) families and is rejected purely
        // on request shape — no registry scan, no timing channel.
        let inner = build_with_counter(
            "dispatch-test",
            "dispatch_probe_oracle_metric",
            &[("user", "alice")],
            1,
        );
        let req = SnapshotRequest {
            labels: vec![LabelFilter::KeyEquals {
                key: "user".to_string(),
                value: "alice".to_string(),
            }],
            ..Default::default()
        };
        let err = snapshot_dispatch(&inner, &req).unwrap_err();
        match err {
            SnapshotError::CapabilityRequired { needed, reason } => {
                assert_eq!(needed, "stats.snapshot:raw-labels:<svc>");
                assert!(
                    reason.contains("without a metric pin"),
                    "reason cites the no-metric-pin shape: {reason}"
                );
            }
        }
    }

    #[test]
    fn key_equals_filter_with_prefix_glob_is_rejected_without_cap() {
        // Prefix glob admits unclassified (= default Restricted)
        // families, so the conservative gate refuses to allow
        // KeyEquals through.
        let inner = build_with_counter(
            "dispatch-test",
            "dispatch_prefix_probe_metric",
            &[("user", "alice")],
            1,
        );
        let req = SnapshotRequest {
            metric: Some(MetricPattern::Prefix("dispatch_prefix_".to_string())),
            labels: vec![LabelFilter::KeyEquals {
                key: "user".to_string(),
                value: "alice".to_string(),
            }],
            ..Default::default()
        };
        let err = snapshot_dispatch(&inner, &req).unwrap_err();
        match err {
            SnapshotError::CapabilityRequired { needed, reason } => {
                assert_eq!(needed, "stats.snapshot:raw-labels:<svc>");
                assert!(
                    reason.contains("Prefix admits"),
                    "reason cites prefix-glob shape: {reason}"
                );
            }
        }
    }

    #[test]
    fn key_equals_filter_with_exact_unclassified_is_rejected_without_cap() {
        // Exact glob targeting an unclassified family — sensitivity
        // defaults to Restricted, so the audit must reject.
        let inner = build_with_counter(
            "dispatch-test",
            "dispatch_exact_unclassified_metric",
            &[("user", "alice")],
            1,
        );
        let req = SnapshotRequest {
            metric: Some(MetricPattern::Exact(
                "dispatch_exact_unclassified_metric".to_string(),
            )),
            labels: vec![LabelFilter::KeyEquals {
                key: "user".to_string(),
                value: "alice".to_string(),
            }],
            ..Default::default()
        };
        let err = snapshot_dispatch(&inner, &req).unwrap_err();
        match err {
            SnapshotError::CapabilityRequired { needed, reason } => {
                assert_eq!(needed, "stats.snapshot:raw-labels:<svc>");
                assert!(
                    reason.contains("not classified Safe"),
                    "reason cites unclassified-Exact shape: {reason}"
                );
            }
        }
    }

    #[test]
    fn key_equals_filter_on_restricted_with_cap_is_accepted() {
        let inner = build_with_counter(
            "dispatch-test",
            "dispatch_keyequals_with_cap_metric",
            &[("user", "alice")],
            1,
        );
        let req = SnapshotRequest {
            labels: vec![LabelFilter::KeyEquals {
                key: "user".to_string(),
                value: "alice".to_string(),
            }],
            has_raw_labels_cap: true,
            ..Default::default()
        };
        let snap = snapshot_dispatch(&inner, &req).expect("ok with cap");
        let family = snap
            .metrics
            .iter()
            .find(|m| m.name == "dispatch_keyequals_with_cap_metric")
            .expect("family present");
        assert_eq!(family.series.len(), 1, "matching series survives filter");
    }

    #[test]
    fn key_equals_with_exact_safe_pin_passes_without_cap() {
        // The only request shape that admits a KeyEquals filter
        // without `has_raw_labels_cap`: `MetricPattern::Exact(name)`
        // where `name` is classified Safe in the captured snapshot.
        // This is the load-bearing positive case for the cap-gate
        // contract (Codex slice-5a round-1 BLOCKERs).
        classify("dispatch_glob_scope_safe_metric", LabelSensitivity::Safe);
        let inner_safe = build_with_counter(
            "dispatch-test",
            "dispatch_glob_scope_safe_metric",
            &[("verdict", "ham")],
            1,
        );
        let req = SnapshotRequest {
            metric: Some(MetricPattern::Exact(
                "dispatch_glob_scope_safe_metric".to_string(),
            )),
            labels: vec![LabelFilter::KeyEquals {
                key: "verdict".to_string(),
                value: "ham".to_string(),
            }],
            ..Default::default()
        };
        let snap = snapshot_dispatch(&inner_safe, &req).expect("Exact-Safe pins admit KeyEquals");
        assert_eq!(snap.metrics.len(), 1);
    }

    #[test]
    fn has_key_filter_on_restricted_without_cap_is_accepted() {
        // Key-presence-only filters are bounded by compile-time
        // metric definitions (plan §4.2) — no probe-oracle gating.
        let inner = build_with_counter(
            "dispatch-test",
            "dispatch_haskey_restricted_metric",
            &[("user", "alice"), ("region", "us")],
            1,
        );
        let req = SnapshotRequest {
            labels: vec![LabelFilter::HasKey("region".to_string())],
            ..Default::default()
        };
        let snap = snapshot_dispatch(&inner, &req).expect("HasKey is operator-class");
        let family = snap
            .metrics
            .iter()
            .find(|m| m.name == "dispatch_haskey_restricted_metric")
            .expect("family present");
        assert_eq!(family.series.len(), 1);
        // Restricted projection still applies — labels come back hashed.
        assert!(matches!(family.series[0].labels, SeriesLabels::Hash(_)));
    }

    #[test]
    fn metric_pattern_prefix_and_suffix_match() {
        classify("dispatch_prefix_match_total", LabelSensitivity::Safe);
        let inner = build_with_counter("dispatch-test", "dispatch_prefix_match_total", &[], 1);
        let req_prefix = SnapshotRequest {
            metric: Some(MetricPattern::Prefix("dispatch_prefix_".to_string())),
            ..Default::default()
        };
        let snap = snapshot_dispatch(&inner, &req_prefix).expect("ok");
        assert!(
            snap.metrics
                .iter()
                .any(|m| m.name == "dispatch_prefix_match_total")
        );

        let req_suffix = SnapshotRequest {
            metric: Some(MetricPattern::Suffix("_match_total".to_string())),
            ..Default::default()
        };
        let snap = snapshot_dispatch(&inner, &req_suffix).expect("ok");
        assert!(
            snap.metrics
                .iter()
                .any(|m| m.name == "dispatch_prefix_match_total")
        );

        let req_exclude = SnapshotRequest {
            metric: Some(MetricPattern::Prefix("does_not_exist_".to_string())),
            ..Default::default()
        };
        let snap = snapshot_dispatch(&inner, &req_exclude).expect("ok");
        assert!(snap.metrics.is_empty());
    }

    #[test]
    fn labels_hash_filter_matches_canonical_digest() {
        // The dispatch's labels_hash matching must agree byte-for-byte
        // with the canonical labels_hash helper — operators correlate
        // Bus responses to JSONL records by exact hash. A
        // Safe-classified family is convenient here because we want to
        // assert the digest computed against raw labels (the labels
        // the recorder holds), regardless of projection.
        classify("dispatch_labels_hash_metric", LabelSensitivity::Safe);
        let inner = build_with_counter(
            "dispatch-test",
            "dispatch_labels_hash_metric",
            &[("verdict", "spam")],
            1,
        );
        // Compute the expected digest the way the helper does.
        let mut map = std::collections::BTreeMap::new();
        map.insert("verdict".to_string(), "spam".to_string());
        let expected = crate::stats::labels_hash::labels_hash(&map);
        let req = SnapshotRequest {
            labels_hash: vec![expected.clone()],
            ..Default::default()
        };
        let snap = snapshot_dispatch(&inner, &req).expect("ok");
        let family = snap
            .metrics
            .iter()
            .find(|m| m.name == "dispatch_labels_hash_metric")
            .expect("family present");
        assert_eq!(family.series.len(), 1);

        let req_no_match = SnapshotRequest {
            labels_hash: vec!["0000000000000000".to_string()],
            ..Default::default()
        };
        let snap = snapshot_dispatch(&inner, &req_no_match).expect("ok");
        // Family drops entirely when no series survive filtering.
        assert!(
            !snap
                .metrics
                .iter()
                .any(|m| m.name == "dispatch_labels_hash_metric")
        );
    }

    #[test]
    fn empty_filter_returns_full_snapshot_with_sensitivity_projection() {
        // Sanity check: a default request returns every family with
        // sensitivity-correct projection. Anti-regression for a future
        // refactor that might accidentally pre-filter on Default.
        let inner = build_with_counter(
            "dispatch-test",
            "dispatch_empty_filter_full_metric",
            &[("k", "v")],
            42,
        );
        let snap = snapshot_dispatch(&inner, &req_default()).expect("ok");
        let family = snap
            .metrics
            .iter()
            .find(|m| m.name == "dispatch_empty_filter_full_metric")
            .expect("family present");
        match &family.series[0].value {
            SeriesValue::Counter(v) => assert_eq!(*v, 42),
            other => panic!("expected counter, got {other:?}"),
        }
    }
}
