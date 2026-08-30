//! SPEC 07 §2 property surface for the `log` sub-service (L1).
//!
//! The log module taps `noded.tap` and writes every Bus message to
//! `~/.local/log/cosmix/bus.log`. State exposed:
//! - config: log_path
//! - lifecycle: started_at, uptime_s, props_level, health, tap_subscribed
//! - runtime: events_seen, bytes_logged (since module start)

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};

use cosmix_props::{PropDescribe, PropPath, PropTree, PropType, PropValue, tree::build_snapshot};
use serde_json::Value as Json;

/// Counters maintained by the logger's tap loop. Wrapped in `Arc`
/// so the props-dispatch arm can read live totals without going
/// through the loop.
#[derive(Default)]
pub struct LogStats {
    pub events_seen: AtomicU64,
    pub bytes_logged: AtomicU64,
    pub tap_subscribed: AtomicBool,
}

impl LogStats {
    pub fn new() -> Arc<Self> {
        Arc::new(Self::default())
    }

    pub fn record(&self, bytes: u64) {
        self.events_seen.fetch_add(1, Ordering::Relaxed);
        self.bytes_logged.fetch_add(bytes, Ordering::Relaxed);
    }
}

/// Snapshot of the log service's observable state.
pub struct LogPropsSnapshot {
    pub log_path: String,
    pub started_at: String,
    pub uptime_s: u64,
    pub tap_subscribed: bool,
    pub events_seen: u64,
    pub bytes_logged: u64,
}

impl LogPropsSnapshot {
    pub fn snapshot_value(&self) -> PropValue {
        build_snapshot([
            (
                PropPath::new("config.log_path").unwrap(),
                PropValue::from(self.log_path.clone()),
            ),
            (
                PropPath::new("lifecycle.started_at").unwrap(),
                PropValue::from(self.started_at.clone()),
            ),
            (
                PropPath::new("lifecycle.uptime_s").unwrap(),
                PropValue::from(self.uptime_s),
            ),
            (
                PropPath::new("lifecycle.health").unwrap(),
                PropValue::from("ok"),
            ),
            (
                PropPath::new("lifecycle.props_level").unwrap(),
                PropValue::from("L1"),
            ),
            (
                PropPath::new("lifecycle.tap_subscribed").unwrap(),
                PropValue::Bool(self.tap_subscribed),
            ),
            (
                PropPath::new("runtime.events_seen").unwrap(),
                PropValue::from(self.events_seen),
            ),
            (
                PropPath::new("runtime.bytes_logged").unwrap(),
                PropValue::from(self.bytes_logged),
            ),
        ])
    }
}

impl PropTree for LogPropsSnapshot {
    fn snapshot(&self) -> PropValue {
        self.snapshot_value()
    }
    fn list(&self) -> Vec<PropPath> {
        all_paths()
    }
    fn describe(&self, path: &PropPath) -> Option<PropDescribe> {
        describe_path(path)
    }
}

fn all_paths() -> Vec<PropPath> {
    [
        "config.log_path",
        "lifecycle.started_at",
        "lifecycle.uptime_s",
        "lifecycle.health",
        "lifecycle.props_level",
        "lifecycle.tap_subscribed",
        "runtime.events_seen",
        "runtime.bytes_logged",
    ]
    .into_iter()
    .map(|s| PropPath::new(s).unwrap())
    .collect()
}

fn describe_path(path: &PropPath) -> Option<PropDescribe> {
    use PropType::*;
    match path.as_str() {
        "config.log_path" => Some(PropDescribe::leaf(
            path.clone(),
            String,
            "Filesystem path the log module appends every observed Bus message to.",
        )),
        "lifecycle.started_at" => Some(
            PropDescribe::leaf(
                path.clone(),
                String,
                "RFC 3339 timestamp of log module start.",
            )
            .with_format("rfc3339"),
        ),
        "lifecycle.uptime_s" => Some(
            PropDescribe::leaf(path.clone(), Number, "Seconds since log module start.")
                .with_transient(true),
        ),
        "lifecycle.health" => Some(PropDescribe::leaf(
            path.clone(),
            String,
            "Coarse health classification (ok | degraded | failing).",
        )),
        "lifecycle.props_level" => Some(PropDescribe::leaf(
            path.clone(),
            String,
            "SPEC 07 conformance level (L0 | L1 | L2 | L3).",
        )),
        "lifecycle.tap_subscribed" => Some(PropDescribe::leaf(
            path.clone(),
            Bool,
            "Whether the log module is currently subscribed to noded.tap. False indicates a degraded state.",
        )),
        "runtime.events_seen" => Some(
            PropDescribe::leaf(
                path.clone(),
                Number,
                "Bus messages observed since module start.",
            )
            .with_transient(true),
        ),
        "runtime.bytes_logged" => Some(
            PropDescribe::leaf(
                path.clone(),
                Number,
                "Bytes appended to the log file since module start.",
            )
            .with_transient(true),
        ),
        _ => None,
    }
}

/// Parse the optional `args` JSON header into a `serde_json::Value`.
pub fn parse_args(s: Option<&str>) -> Option<Json> {
    s.and_then(|raw| serde_json::from_str(raw).ok())
}
