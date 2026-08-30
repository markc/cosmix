//! SPEC 07 §2 property surface for the `mon` sub-service (L1).
//!
//! Exposes system metrics already gathered by `gather_status()` as a
//! uniform `PropTree`. Built fresh per `props.get` from a `sysinfo`
//! refresh; transient leaves (cpu_usage, mem_used_mb, etc.) are
//! marked so a future L2/L3 publisher would suppress them from
//! change events without losing them from the snapshot.

use std::collections::BTreeMap;

use cosmix_props::{PropDescribe, PropPath, PropTree, PropType, PropValue, tree::build_snapshot};
use serde_json::Value as Json;

/// Snapshot of the mon service's observable state. Captured per
/// `props.get` from a fresh sysinfo refresh.
pub struct MonPropsSnapshot {
    pub started_at: String,
    pub uptime_s: u64,
    pub top_processes_default_limit: u64,

    pub hostname: String,
    pub system_uptime_s: u64,
    pub cpu_count: u64,
    pub cpu_usage: f64,
    pub mem_total_mb: u64,
    pub mem_used_mb: u64,
    pub mem_percent: f64,
    pub swap_total_mb: u64,
    pub swap_used_mb: u64,
    pub load_avg_one: f64,
    pub load_avg_five: f64,
    pub load_avg_fifteen: f64,

    /// Map of mount-point → `{total_gb, used_gb, percent}`. Kept as a
    /// single Object leaf so the leaf set is static even when disks
    /// come and go (mirrors the `corpus.kinds` pattern in indexd).
    pub disks: BTreeMap<String, DiskSummary>,
}

#[derive(Clone, Debug)]
pub struct DiskSummary {
    pub total_gb: f64,
    pub used_gb: f64,
    pub percent: f64,
}

impl MonPropsSnapshot {
    pub fn snapshot_value(&self) -> PropValue {
        let disks = PropValue::Object(
            self.disks
                .iter()
                .map(|(mount, d)| {
                    let mut m = std::collections::BTreeMap::new();
                    m.insert("total_gb".to_string(), PropValue::from(d.total_gb));
                    m.insert("used_gb".to_string(), PropValue::from(d.used_gb));
                    m.insert("percent".to_string(), PropValue::from(d.percent));
                    (mount.clone(), PropValue::Object(m))
                })
                .collect(),
        );

        build_snapshot([
            (
                PropPath::new("config.top_processes_default_limit").unwrap(),
                PropValue::from(self.top_processes_default_limit),
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
                PropPath::new("system.hostname").unwrap(),
                PropValue::from(self.hostname.clone()),
            ),
            (
                PropPath::new("system.uptime_s").unwrap(),
                PropValue::from(self.system_uptime_s),
            ),
            (
                PropPath::new("system.cpu_count").unwrap(),
                PropValue::from(self.cpu_count),
            ),
            (
                PropPath::new("system.cpu_usage").unwrap(),
                PropValue::from(self.cpu_usage),
            ),
            (
                PropPath::new("system.mem_total_mb").unwrap(),
                PropValue::from(self.mem_total_mb),
            ),
            (
                PropPath::new("system.mem_used_mb").unwrap(),
                PropValue::from(self.mem_used_mb),
            ),
            (
                PropPath::new("system.mem_percent").unwrap(),
                PropValue::from(self.mem_percent),
            ),
            (
                PropPath::new("system.swap_total_mb").unwrap(),
                PropValue::from(self.swap_total_mb),
            ),
            (
                PropPath::new("system.swap_used_mb").unwrap(),
                PropValue::from(self.swap_used_mb),
            ),
            (
                PropPath::new("system.load_avg_one").unwrap(),
                PropValue::from(self.load_avg_one),
            ),
            (
                PropPath::new("system.load_avg_five").unwrap(),
                PropValue::from(self.load_avg_five),
            ),
            (
                PropPath::new("system.load_avg_fifteen").unwrap(),
                PropValue::from(self.load_avg_fifteen),
            ),
            (PropPath::new("disks").unwrap(), disks),
        ])
    }
}

impl PropTree for MonPropsSnapshot {
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
        "config.top_processes_default_limit",
        "lifecycle.started_at",
        "lifecycle.uptime_s",
        "lifecycle.health",
        "lifecycle.props_level",
        "system.hostname",
        "system.uptime_s",
        "system.cpu_count",
        "system.cpu_usage",
        "system.mem_total_mb",
        "system.mem_used_mb",
        "system.mem_percent",
        "system.swap_total_mb",
        "system.swap_used_mb",
        "system.load_avg_one",
        "system.load_avg_five",
        "system.load_avg_fifteen",
        "disks",
    ]
    .into_iter()
    .map(|s| PropPath::new(s).unwrap())
    .collect()
}

fn describe_path(path: &PropPath) -> Option<PropDescribe> {
    use PropType::*;
    match path.as_str() {
        "config.top_processes_default_limit" => Some(PropDescribe::leaf(
            path.clone(),
            Number,
            "Default top-N limit for mon.processes when caller omits `limit` arg.",
        )),
        "lifecycle.started_at" => Some(
            PropDescribe::leaf(
                path.clone(),
                String,
                "RFC 3339 timestamp of mon module start.",
            )
            .with_format("rfc3339"),
        ),
        "lifecycle.uptime_s" => Some(
            PropDescribe::leaf(path.clone(), Number, "Seconds since mon module start.")
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
        "system.hostname" => Some(PropDescribe::leaf(
            path.clone(),
            String,
            "Kernel hostname of the host the mon module is running on.",
        )),
        "system.uptime_s" => Some(
            PropDescribe::leaf(path.clone(), Number, "Seconds since system boot.")
                .with_transient(true),
        ),
        "system.cpu_count" => Some(PropDescribe::leaf(
            path.clone(),
            Number,
            "Number of logical CPUs reported by sysinfo.",
        )),
        "system.cpu_usage" => Some(
            PropDescribe::leaf(
                path.clone(),
                Number,
                "Aggregate CPU usage % across all cores (0–100).",
            )
            .with_transient(true),
        ),
        "system.mem_total_mb" => Some(PropDescribe::leaf(
            path.clone(),
            Number,
            "Total physical memory in MB.",
        )),
        "system.mem_used_mb" => Some(
            PropDescribe::leaf(
                path.clone(),
                Number,
                "Resident memory currently in use in MB.",
            )
            .with_transient(true),
        ),
        "system.mem_percent" => Some(
            PropDescribe::leaf(path.clone(), Number, "Memory utilisation % (0–100).")
                .with_transient(true),
        ),
        "system.swap_total_mb" => Some(PropDescribe::leaf(
            path.clone(),
            Number,
            "Total swap in MB.",
        )),
        "system.swap_used_mb" => Some(
            PropDescribe::leaf(path.clone(), Number, "Swap currently in use in MB.")
                .with_transient(true),
        ),
        "system.load_avg_one" => Some(
            PropDescribe::leaf(path.clone(), Number, "1-minute load average.").with_transient(true),
        ),
        "system.load_avg_five" => Some(
            PropDescribe::leaf(path.clone(), Number, "5-minute load average.").with_transient(true),
        ),
        "system.load_avg_fifteen" => Some(
            PropDescribe::leaf(path.clone(), Number, "15-minute load average.")
                .with_transient(true),
        ),
        "disks" => Some(PropDescribe::leaf(
            path.clone(),
            Object,
            "Map of mount-point → { total_gb, used_gb, percent } for / and /home/data partitions.",
        )),
        _ => None,
    }
}

/// Parse the optional `args` JSON header into a `serde_json::Value`.
pub fn parse_args(s: Option<&str>) -> Option<Json> {
    s.and_then(|raw| serde_json::from_str(raw).ok())
}
