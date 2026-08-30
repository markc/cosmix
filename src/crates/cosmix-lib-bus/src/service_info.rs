//! `ServiceInfo` / `NodeInfo` — the Bus service & node discovery wire
//! types (SPEC 02 §4.1).
//!
//! These live in `cosmix-lib-bus` (the bottom of `bus ← mix ← cos`) so
//! both the broker (`cosmix-noded`, in cos) that *emits* them and
//! `cosmix-lib-client` (in bus) that *parses* them share one definition.
//!
//! ## Open struct = additive-safe
//!
//! Every `ServiceInfo`/`NodeInfo` field except `name` is `Option` with
//! `#[serde(default)]`, and neither is `deny_unknown_fields`. So a future
//! typed field is non-breaking: a new emitter's extra field is skipped by
//! an old parser, and a new parser defaults a field an old emitter omits.
//! Truly experimental data rides in `meta` with no schema change. This is
//! the mechanism that makes the §9 breaking `noded.list` reshape the LAST
//! breaking change to this surface.
//!
//! ## Dual-parse (transition)
//!
//! `ServiceInfo`'s `Deserialize` accepts **either** a bare service-name
//! string (the legacy `noded.list` element shape) **or** a full object.
//! A bare string becomes `ServiceInfo { name, ..Default }`. This lets a
//! new (object-aware) client tolerate an old broker still emitting
//! `["name", …]` during the client-first fleet rollout (§9). The string
//! arm is dropped a release after every broker emits objects.

use serde::de::{self, MapAccess, Visitor};
use serde::{Deserialize, Deserializer, Serialize};
use std::fmt;

/// The current `ServiceInfo`/`NodeInfo` record-format version. Bumped
/// only on a genuinely breaking reshape (the open-struct rule means
/// additive fields do NOT bump it).
pub const SCHEMA_VERSION: u16 = 1;

fn default_schema_version() -> u16 {
    SCHEMA_VERSION
}

fn is_empty_map(m: &serde_json::Map<String, serde_json::Value>) -> bool {
    m.is_empty()
}

/// Per-service build provenance + registry binding metadata. One per
/// registered service; returned by `noded.list` and nested in
/// [`NodeInfo`]. Immutable for the life of the registered process — see
/// the contract's "immutable facts only" rule.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize)]
pub struct ServiceInfo {
    /// Registered Bus service name (the broker registry key). Required.
    pub name: String,
    /// Binary / package name when it differs from the logical service
    /// name (e.g. `"cosmix-jmap"` for service `"maild"`). Disambiguates
    /// inventory + Prometheus labels.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub binary: Option<String>,
    /// `CARGO_PKG_VERSION` semver of the registered binary.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub version: Option<String>,
    /// Git sha of the binary's source at build (the truthful fingerprint
    /// that catches a forgotten semver bump).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub git_sha: Option<String>,
    /// Whether the build tree was dirty.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub git_dirty: Option<bool>,
    /// RFC3339 UTC build timestamp (catches a stale binary).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub build_time: Option<String>,
    /// Citizen process id.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pid: Option<u32>,
    /// RFC3339 UTC process start (→ uptime).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub started_at: Option<String>,
    /// RFC3339 UTC when this name bound — **registry binding** metadata
    /// (a same-name refresh re-stamps it), not process provenance.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub registered_at: Option<String>,
    /// Record-format version (see [`SCHEMA_VERSION`]).
    #[serde(default = "default_schema_version")]
    pub schema_version: u16,
    /// Open forward field — experimental / node-specific JSON values.
    #[serde(default, skip_serializing_if = "is_empty_map")]
    pub meta: serde_json::Map<String, serde_json::Value>,
}

impl ServiceInfo {
    /// A name-only record (all provenance defaulted) — the shape a bare
    /// legacy string deserialises to, and a convenient constructor for
    /// an old citizen that registered without provenance.
    pub fn from_name(name: impl Into<String>) -> Self {
        Self {
            name: name.into(),
            schema_version: SCHEMA_VERSION,
            ..Default::default()
        }
    }
}

// Object form: a private mirror with the derived `Deserialize`, so the
// manual `Deserialize` below can delegate the map case to it without
// re-listing every field. Kept in lockstep with `ServiceInfo`.
#[derive(Deserialize)]
struct ServiceInfoObj {
    name: String,
    #[serde(default)]
    binary: Option<String>,
    #[serde(default)]
    version: Option<String>,
    #[serde(default)]
    git_sha: Option<String>,
    #[serde(default)]
    git_dirty: Option<bool>,
    #[serde(default)]
    build_time: Option<String>,
    #[serde(default)]
    pid: Option<u32>,
    #[serde(default)]
    started_at: Option<String>,
    #[serde(default)]
    registered_at: Option<String>,
    #[serde(default = "default_schema_version")]
    schema_version: u16,
    #[serde(default)]
    meta: serde_json::Map<String, serde_json::Value>,
}

impl From<ServiceInfoObj> for ServiceInfo {
    fn from(o: ServiceInfoObj) -> Self {
        Self {
            name: o.name,
            binary: o.binary,
            version: o.version,
            git_sha: o.git_sha,
            git_dirty: o.git_dirty,
            build_time: o.build_time,
            pid: o.pid,
            started_at: o.started_at,
            registered_at: o.registered_at,
            schema_version: o.schema_version,
            meta: o.meta,
        }
    }
}

impl<'de> Deserialize<'de> for ServiceInfo {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        struct ServiceInfoVisitor;

        impl<'de> Visitor<'de> for ServiceInfoVisitor {
            type Value = ServiceInfo;

            fn expecting(&self, f: &mut fmt::Formatter) -> fmt::Result {
                f.write_str("a service-name string or a ServiceInfo object")
            }

            // Legacy `noded.list` element: a bare service name.
            fn visit_str<E: de::Error>(self, v: &str) -> Result<ServiceInfo, E> {
                Ok(ServiceInfo::from_name(v))
            }

            // Object form: delegate to the derived `ServiceInfoObj`.
            fn visit_map<A: MapAccess<'de>>(self, map: A) -> Result<ServiceInfo, A::Error> {
                ServiceInfoObj::deserialize(de::value::MapAccessDeserializer::new(map))
                    .map(Into::into)
            }
        }

        deserializer.deserialize_any(ServiceInfoVisitor)
    }
}

/// The build provenance a citizen sends in the `noded.register` body.
/// The broker merges it with the registry key (`name`) and a
/// broker-stamped `registered_at` to form the stored [`ServiceInfo`].
/// All fields optional — an old citizen sends an empty body. Built once
/// per process (so `started_at` is the true process start and survives
/// reconnects) and cloned into each (re)connect.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct RegisterProvenance {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub binary: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub version: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub git_sha: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub git_dirty: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub build_time: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pid: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub started_at: Option<String>,
    #[serde(default, skip_serializing_if = "is_empty_map")]
    pub meta: serde_json::Map<String, serde_json::Value>,
}

impl RegisterProvenance {
    /// Assemble from a `cosmix-lib-buildinfo` `build_info!()`'s parts
    /// (passed individually so this crate stays free of a buildinfo dep)
    /// plus the binary name and a process-start timestamp. Stamps the
    /// current pid. `started_at` should be captured ONCE at process start
    /// and reused across reconnects.
    #[allow(clippy::too_many_arguments)]
    pub fn from_parts(
        binary: &str,
        version: &str,
        git_sha: &str,
        git_dirty: bool,
        build_time: &str,
        started_at: String,
    ) -> Self {
        Self {
            binary: Some(binary.to_string()),
            version: Some(version.to_string()),
            git_sha: Some(git_sha.to_string()),
            git_dirty: Some(git_dirty),
            build_time: Some(build_time.to_string()),
            pid: Some(std::process::id()),
            started_at: Some(started_at),
            meta: serde_json::Map::new(),
        }
    }
}

/// Per-node identity + the local broker's own build, returned by
/// `noded.info`. Unlike [`ServiceInfo`] (a stored, immutable registry
/// record) this is **computed on read**, so its dynamic fields
/// (`uptime_s`, `service_count`) are legitimately live.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct NodeInfo {
    /// Mesh node name (from `node.conf.mix`).
    pub node: String,
    /// WireGuard IP (from `node.conf.mix`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub wg_ip: Option<String>,
    /// Cross-mesh FQDN this node belongs to, if any.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub mesh: Option<String>,
    /// The local `cosmix-noded`'s own build provenance.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub noded: Option<ServiceInfo>,
    /// Node uptime in seconds (live).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub uptime_s: Option<u64>,
    /// Count of currently registered services (live).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub service_count: Option<u16>,
    /// Record-format version.
    #[serde(default = "default_schema_version")]
    pub schema_version: u16,
    /// Open forward field.
    #[serde(default, skip_serializing_if = "is_empty_map")]
    pub meta: serde_json::Map<String, serde_json::Value>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bare_string_deserialises_to_name_only() {
        let s: ServiceInfo = serde_json::from_str("\"maild\"").unwrap();
        assert_eq!(s.name, "maild");
        assert_eq!(s.version, None);
        assert_eq!(s.schema_version, SCHEMA_VERSION);
    }

    #[test]
    fn object_deserialises_fully() {
        let j = r#"{"name":"maild","binary":"cosmix-jmap","version":"0.2.1",
                    "git_sha":"abc123","pid":4242,"schema_version":1}"#;
        let s: ServiceInfo = serde_json::from_str(j).unwrap();
        assert_eq!(s.name, "maild");
        assert_eq!(s.binary.as_deref(), Some("cosmix-jmap"));
        assert_eq!(s.pid, Some(4242));
    }

    #[test]
    fn mixed_array_dual_parses() {
        // Exactly the client-first transition case: an old broker could
        // even mix shapes; both arms must coexist in one array.
        let j = r#"["webd", {"name":"maild","version":"0.2.1"}]"#;
        let v: Vec<ServiceInfo> = serde_json::from_str(j).unwrap();
        assert_eq!(v[0].name, "webd");
        assert_eq!(v[0].version, None);
        assert_eq!(v[1].name, "maild");
        assert_eq!(v[1].version.as_deref(), Some("0.2.1"));
    }

    #[test]
    fn unknown_fields_are_ignored() {
        // Additive-safe: a future field an old parser doesn't know.
        let j = r#"{"name":"x","some_future_field":42}"#;
        let s: ServiceInfo = serde_json::from_str(j).unwrap();
        assert_eq!(s.name, "x");
    }

    #[test]
    fn serialize_omits_empty_optionals() {
        let s = ServiceInfo::from_name("noded");
        let j = serde_json::to_string(&s).unwrap();
        // name + schema_version always present; no null version/pid/etc.
        assert!(j.contains("\"name\":\"noded\""));
        assert!(j.contains("\"schema_version\":1"));
        assert!(!j.contains("\"version\"")); // the `version` field (not schema_version)
        assert!(!j.contains("\"pid\""));
        assert!(!j.contains("null"));
    }

    #[test]
    fn round_trip_full() {
        let mut s = ServiceInfo::from_name("indexd");
        s.version = Some("0.2.2".into());
        s.git_sha = Some("deadbeef".into());
        s.pid = Some(99);
        let j = serde_json::to_string(&s).unwrap();
        let back: ServiceInfo = serde_json::from_str(&j).unwrap();
        assert_eq!(s, back);
    }

    #[test]
    fn node_info_round_trips() {
        let n = NodeInfo {
            node: "alpha".into(),
            wg_ip: Some("192.0.2.5".into()),
            mesh: Some("example.org".into()),
            noded: Some(ServiceInfo::from_name("noded")),
            uptime_s: Some(3600),
            service_count: Some(5),
            schema_version: SCHEMA_VERSION,
            meta: serde_json::Map::new(),
        };
        let j = serde_json::to_string(&n).unwrap();
        let back: NodeInfo = serde_json::from_str(&j).unwrap();
        assert_eq!(n, back);
    }
}
