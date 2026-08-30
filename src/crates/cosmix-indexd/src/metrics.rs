//! Per-request observability for cosmix-indexd (2026-07-24 arc).
//!
//! Fixed-action counter blocks, independent of the `AppState` mutex.
//! Each action's counters live behind their own tiny `std::sync::Mutex`
//! so a `snapshot()` is **internally consistent** — `requests` always
//! equals `ok + queued + errors + invalid`, and phase totals come from
//! the same completion set (independent atomics could tear across a
//! concurrent completion, corrupting external sampler deltas).
//! Contention is negligible at indexd request rates; the lock is held
//! for a dozen integer adds.
//!
//! Times are accumulated in **microseconds** so the many
//! sub-millisecond DB operations don't vanish into zero. Snapshots feed
//! the `stats` action's `request_metrics` object so external samplers
//! can attribute CPU to workload without scraping logs.
//!
//! `bytes` totals are **per execution action**, not per ingress: a
//! `background=true` index_file counts its payload once under
//! `index_file` (the queue ack) and once under `index_file_job` (the
//! execution). Summing bytes across those two buckets double-counts by
//! design — they answer "what did this bucket process".
//!
//! `internal.world_snapshot` covers the 1 Hz world-publisher tick
//! (`collect_props` → `db.stats()` COUNT + GROUP BY under the global
//! mutex), which bypasses `process_request` entirely — without it,
//! request logs can report zero workload while the daemon burns CPU.
//! Flat `props.get`-style reads of the same snapshot are charged to
//! `props.get` instead, so internal burn and external callers stay
//! distinguishable.

use std::sync::Mutex;
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};

/// Wall-clock phase timings + flags accumulated across one request.
/// Threaded explicitly through `compute_embeddings` and the handlers
/// (a tracing span can't feed the cumulative `stats` counters, and
/// task-local state is hidden coupling — explicit params win).
#[derive(Default)]
pub struct ReqTiming {
    pub lock_wait_us: u64,
    pub model_load_us: u64,
    pub embed_us: u64,
    pub db_us: u64,
    /// In-memory vector scan time (search top-k) — distinct from
    /// db_us so SQL and distance work stay separable.
    pub vector_us: u64,
    /// Time a background job spent waiting in the queue before the
    /// worker dequeued it (index_file_job only).
    pub queue_us: u64,
    /// A model load was attempted on demand during this request —
    /// including FAILED loads, whose burn is exactly what this
    /// instrumentation must expose.
    pub cold_model: bool,
    /// Embed served entirely from the embedding cache.
    pub cache_hit: bool,
    /// Bounded work dimension: results returned / chunks stored /
    /// sections indexed / rows affected — meaning depends on action.
    pub work: u64,
}

/// Who sent the request, resolved once per connection (unix) or per
/// command (Bus). `peer` is best-effort display data, never authority.
pub struct RequestContext {
    pub transport: &'static str,
    pub peer: String,
}

#[derive(Default)]
struct Counters {
    requests: u64,
    ok: u64,
    queued: u64,
    errors: u64,
    invalid: u64,
    bytes: u64,
    cold_loads: u64,
    /// Embed cache full hits (embed/search buckets); for
    /// `internal.corpus_reconcile` this counts DISCARDED reconciles
    /// (aggregate succeeded but a concurrent mutation nudge was
    /// fresher, so the swap was skipped).
    cache_hits: u64,
    work: u64,
    elapsed_us: u64,
    model_load_us: u64,
    embed_us: u64,
    db_us: u64,
    vector_us: u64,
    lock_wait_us: u64,
    queue_us: u64,
}

pub struct ActionMetrics {
    inner: Mutex<Counters>,
}

impl ActionMetrics {
    pub const fn new() -> Self {
        Self {
            inner: Mutex::new(Counters {
                requests: 0,
                ok: 0,
                queued: 0,
                errors: 0,
                invalid: 0,
                bytes: 0,
                cold_loads: 0,
                cache_hits: 0,
                work: 0,
                elapsed_us: 0,
                model_load_us: 0,
                embed_us: 0,
                db_us: 0,
                vector_us: 0,
                lock_wait_us: 0,
                queue_us: 0,
            }),
        }
    }

    pub fn record(&self, t: &ReqTiming, elapsed_us: u64, bytes: u64, outcome: Outcome) {
        let mut c = self.inner.lock().unwrap();
        c.requests += 1;
        match outcome {
            Outcome::Ok => c.ok += 1,
            Outcome::Queued => c.queued += 1,
            Outcome::Error => c.errors += 1,
            Outcome::Invalid => c.invalid += 1,
        }
        c.bytes += bytes;
        if t.cold_model {
            c.cold_loads += 1;
        }
        if t.cache_hit {
            c.cache_hits += 1;
        }
        c.work += t.work;
        c.elapsed_us += elapsed_us;
        c.model_load_us += t.model_load_us;
        c.embed_us += t.embed_us;
        c.db_us += t.db_us;
        c.vector_us += t.vector_us;
        c.lock_wait_us += t.lock_wait_us;
        c.queue_us += t.queue_us;
    }

    fn snapshot(&self) -> Option<serde_json::Value> {
        let c = self.inner.lock().unwrap();
        if c.requests == 0 {
            return None;
        }
        Some(serde_json::json!({
            "requests": c.requests,
            "ok": c.ok,
            "queued": c.queued,
            "errors": c.errors,
            "invalid": c.invalid,
            "bytes": c.bytes,
            "cold_loads": c.cold_loads,
            "cache_hits": c.cache_hits,
            "work": c.work,
            "elapsed_us": c.elapsed_us,
            "model_load_us": c.model_load_us,
            "embed_us": c.embed_us,
            "db_us": c.db_us,
            "vector_us": c.vector_us,
            "lock_wait_us": c.lock_wait_us,
            "queue_us": c.queue_us,
        }))
    }
}

macro_rules! action_table {
    ($($field:ident => $name:literal),+ $(,)?) => {
        pub struct RequestMetrics {
            $(pub $field: ActionMetrics,)+
        }

        impl RequestMetrics {
            pub const fn new() -> Self {
                Self { $($field: ActionMetrics::new(),)+ }
            }

            /// Counters for a known action name; `invalid` for anything
            /// unparseable (so nothing is ever silently uncounted).
            pub fn for_action(&self, action: &str) -> &ActionMetrics {
                match action {
                    $($name => &self.$field,)+
                    _ => &self.invalid,
                }
            }

            /// Only actions with traffic, keyed by wire name. Each
            /// action's block is internally consistent (single lock);
            /// cross-action consistency is not claimed.
            pub fn snapshot(&self) -> serde_json::Value {
                let mut map = serde_json::Map::new();
                $(
                    if let Some(snap) = self.$field.snapshot() {
                        map.insert($name.to_string(), snap);
                    }
                )+
                serde_json::Value::Object(map)
            }
        }
    };
}

action_table! {
    embed => "embed",
    store => "store",
    search => "search",
    update => "update",
    delete => "delete",
    list => "list",
    feedback => "feedback",
    supersede => "supersede",
    stale => "stale",
    index_file => "index_file",
    index_file_job => "index_file_job",
    stats => "stats",
    stats_snapshot => "stats.snapshot",
    props_get => "props.get",
    props_set => "props.set",
    props_delete => "props.delete",
    props_list => "props.list",
    props_describe => "props.describe",
    props_watch => "props.watch",
    invalid => "invalid",
    world_snapshot => "internal.world_snapshot",
    corpus_reconcile => "internal.corpus_reconcile",
}

pub static METRICS: RequestMetrics = RequestMetrics::new();

/// Live memory/admission gauges. Atomics keep these observable while a model
/// forward is running off the async state lock (including after its caller has
/// timed out).
pub static QUEUE_DEPTH: AtomicUsize = AtomicUsize::new(0);
pub static QUEUED_BYTES: AtomicU64 = AtomicU64::new(0);
pub static INFERENCE_IN_FLIGHT: AtomicUsize = AtomicUsize::new(0);
pub static TIMED_OUT_IN_FLIGHT: AtomicUsize = AtomicUsize::new(0);
pub static MODEL_GENERATION: AtomicU64 = AtomicU64::new(0);
pub static RSS_BEFORE_LOAD_BYTES: AtomicU64 = AtomicU64::new(0);
pub static RSS_AFTER_LOAD_BYTES: AtomicU64 = AtomicU64::new(0);
pub static RSS_AFTER_UNLOAD_BYTES: AtomicU64 = AtomicU64::new(0);

#[derive(serde::Serialize)]
pub struct RuntimeSnapshot {
    pub queue_depth: usize,
    pub queued_bytes: u64,
    pub inference_in_flight: usize,
    pub timed_out_still_running: usize,
    pub model_generation: u64,
    pub rss_before_load_bytes: u64,
    pub rss_after_load_bytes: u64,
    pub rss_after_unload_bytes: u64,
}

pub fn runtime_snapshot() -> RuntimeSnapshot {
    RuntimeSnapshot {
        queue_depth: QUEUE_DEPTH.load(Ordering::Relaxed),
        queued_bytes: QUEUED_BYTES.load(Ordering::Relaxed),
        inference_in_flight: INFERENCE_IN_FLIGHT.load(Ordering::Relaxed),
        timed_out_still_running: TIMED_OUT_IN_FLIGHT.load(Ordering::Relaxed),
        model_generation: MODEL_GENERATION.load(Ordering::Relaxed),
        rss_before_load_bytes: RSS_BEFORE_LOAD_BYTES.load(Ordering::Relaxed),
        rss_after_load_bytes: RSS_AFTER_LOAD_BYTES.load(Ordering::Relaxed),
        rss_after_unload_bytes: RSS_AFTER_UNLOAD_BYTES.load(Ordering::Relaxed),
    }
}

/// Monotone id tying a request's log line to any handler-level events.
pub static REQUEST_SEQ: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

/// Structural outcome classification. Object member order is not part of the
/// JSON contract, so inspect parsed top-level members rather than serialised
/// prefixes. `Invalid` is assigned by the caller for parse failures, never
/// here.
pub fn classify_response(response: &str) -> Outcome {
    match serde_json::from_str::<serde_json::Value>(response) {
        Ok(serde_json::Value::Object(object)) if object.contains_key("error") => Outcome::Error,
        Ok(serde_json::Value::Object(object))
            if object.get("accepted").and_then(serde_json::Value::as_bool) == Some(true) =>
        {
            Outcome::Queued
        }
        _ => Outcome::Ok,
    }
}

/// Best-effort action extraction from an unparseable request, so a
/// malformed `search` (say) is charged to `search.invalid` rather than
/// vanishing into the catch-all bucket and understating that action's
/// real traffic. Falls back to "invalid" when even the action field is
/// missing or the input isn't JSON.
pub fn action_of_invalid(input: &str) -> String {
    serde_json::from_str::<serde_json::Value>(input)
        .ok()
        .and_then(|v| {
            v.get("action")
                .and_then(|a| a.as_str())
                .map(|s| s.to_string())
        })
        .unwrap_or_else(|| "invalid".to_string())
}

#[derive(PartialEq, Eq, Clone, Copy, Debug)]
pub enum Outcome {
    Ok,
    Queued,
    Error,
    Invalid,
}

impl Outcome {
    pub fn as_str(self) -> &'static str {
        match self {
            Outcome::Ok => "ok",
            Outcome::Queued => "queued",
            Outcome::Error => "error",
            Outcome::Invalid => "invalid",
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn classify_response_uses_top_level_members() {
        assert_eq!(classify_response("{\"error\":\"boom\"}"), Outcome::Error);
        assert_eq!(
            classify_response(
                "{\"code\":\"duplicate_vanished\",\"error\":\"store transaction rolled back\"}"
            ),
            Outcome::Error,
            "duplicate-race exhaustion must stay an error regardless of key order"
        );
        // A substring check would misclassify this successful payload
        // whose *content* mentions an error key.
        assert_eq!(
            classify_response("{\"items\":[{\"content\":\"set \\\"error\\\" handler\"}]}"),
            Outcome::Ok
        );
        assert_eq!(
            classify_response("{\"accepted\":true,\"queued\":true,\"file\":\"x.md\"}"),
            Outcome::Queued
        );
    }

    #[test]
    fn invalid_requests_keep_their_action() {
        assert_eq!(
            action_of_invalid("{\"action\":\"search\",\"limit\":\"x\"}"),
            "search"
        );
        assert_eq!(action_of_invalid("{\"action\":\"nonsense\"}"), "nonsense");
        assert_eq!(action_of_invalid("not json at all"), "invalid");
        assert_eq!(action_of_invalid("{\"no_action\":true}"), "invalid");
    }

    #[test]
    fn for_action_falls_back_to_invalid() {
        let m = RequestMetrics::new();
        let t = ReqTiming::default();
        m.for_action("nonsense").record(&t, 5, 10, Outcome::Invalid);
        let snap = m.snapshot();
        let inv = snap.get("invalid").expect("invalid bucket present");
        assert_eq!(inv["requests"], 1);
        assert_eq!(inv["invalid"], 1);
        assert_eq!(inv["errors"], 0);
        assert_eq!(inv["bytes"], 10);
    }

    #[test]
    fn record_accumulates_and_outcomes_sum_to_requests() {
        let m = RequestMetrics::new();
        let t = ReqTiming {
            lock_wait_us: 3,
            model_load_us: 1000,
            embed_us: 2000,
            db_us: 40,
            vector_us: 0,
            queue_us: 0,
            cold_model: true,
            cache_hit: false,
            work: 2,
        };
        m.for_action("search").record(&t, 3100, 64, Outcome::Ok);
        m.for_action("search")
            .record(&ReqTiming::default(), 100, 32, Outcome::Error);
        m.for_action("search")
            .record(&ReqTiming::default(), 10, 8, Outcome::Queued);
        let snap = m.snapshot();
        let s = snap.get("search").expect("search present");
        assert_eq!(s["requests"], 3);
        assert_eq!(
            s["requests"].as_u64().unwrap(),
            s["ok"].as_u64().unwrap()
                + s["queued"].as_u64().unwrap()
                + s["errors"].as_u64().unwrap()
                + s["invalid"].as_u64().unwrap()
        );
        assert_eq!(s["cold_loads"], 1);
        assert_eq!(s["work"], 2);
        assert_eq!(s["elapsed_us"], 3210);
        assert_eq!(s["bytes"], 104);
        // Idle actions are omitted entirely.
        assert!(snap.get("embed").is_none());
    }
}
