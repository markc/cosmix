//! Data layer for the wgd-backed `TrustGrantsCache` subscriber:
//!
//! - [`WgdClient`] trait — the abstraction the subscriber (P0-C5b) drives.
//!   Production implementation wires SPEC-12 `props.list` / `props.watch`
//!   against `cosmix-wgd`; tests implement it directly.
//! - [`Listing`] / [`WatchEvent`] / [`WgdError`] — value types on the
//!   trait surface.
//! - Record → core converters: [`parse_trusted_mesh`], [`parse_grant`],
//!   [`parse_exposable_entry`]. These translate SPEC-12 `Record`s as
//!   they come off the wire into the core [`crate::caps::TrustedMesh`] /
//!   [`crate::caps::Grant`] / [`crate::caps::Cap`] values the
//!   combinator consumes.
//!
//! The subscriber loop, ready-gating, and disconnect-reseed live in
//! the sibling `subscriber` module (P0-C5b).
//!
//! ## Why convert eagerly?
//!
//! Plan §3 places the conversion seam at the cache boundary so the
//! resolver (`resolve_cross_mesh_caps`) operates on core types only.
//! That keeps the hot path — every cross-mesh verb dispatch reads
//! through the resolver — free of SPEC-12 schema knowledge.

use std::collections::BTreeMap;
use std::pin::Pin;

use async_trait::async_trait;
use chrono::{DateTime, Utc};
use cosmix_props::{Nseq, PropValue, Record, RecordKey};
use futures_util::stream::Stream;

use crate::caps::{Cap, CapabilitySet, Grant, TrustedMesh};
use crate::freshness::parse_rfc3339;

/// Transport-level / serialisation errors returned by a [`WgdClient`].
#[derive(Debug, thiserror::Error)]
pub enum WgdError {
    /// `props.list` / `props.watch` failed at the Bus / transport layer.
    #[error("wgd transport error: {0}")]
    Transport(String),

    /// The watch stream closed without an explicit error. The subscriber
    /// treats this the same as an `Err` — it will back off and reseed.
    #[error("wgd watch stream ended")]
    StreamEnded,

    /// A record could not be parsed into a core type. Surfaced from the
    /// subscriber loop as a per-record skip rather than a fatal error;
    /// callers can `log` and continue.
    #[error("wgd record parse error: {0}")]
    Parse(#[from] WgdParseError),
}

/// Per-field parse failures when translating a SPEC-12 [`Record`] into
/// a core [`TrustedMesh`] / [`Grant`] / exposable entry.
///
/// Carries the offending field name so the subscriber can log a
/// targeted message instead of dropping the whole namespace on a
/// single bad row.
#[derive(Debug, Clone, thiserror::Error, PartialEq, Eq)]
pub enum WgdParseError {
    #[error("record value is not an object")]
    NotAnObject,

    #[error("missing field {field:?}")]
    MissingField { field: String },

    #[error("field {field:?} has wrong type: expected {expected}, got {got}")]
    WrongType {
        field: String,
        expected: &'static str,
        /// Owned so error paths needing a dynamic message (e.g. base64
        /// decode failures with the underlying error text) do not leak
        /// memory by `Box::leak`-ing a fresh `&'static str` per
        /// occurrence.
        got: String,
    },

    #[error("field {field:?}: invalid RFC 3339 timestamp: {reason}")]
    BadTimestamp { field: String, reason: String },

    #[error(
        "field {field:?}: list item at index {index} has wrong type: expected {expected}, got {got}"
    )]
    BadListItem {
        field: String,
        index: usize,
        expected: &'static str,
        got: String,
    },
}

/// One snapshot of a wgd namespace plus the watermark to subscribe from.
///
/// Plan §3.1 step 1: the `nseq` value is the SPEC-12 §5.2 high-water
/// mark at the moment the list ran, and the subscriber passes it to
/// `props.watch since_nseq=<this>` so the replay window covers every
/// commit at or after the list-observed state.
pub struct Listing {
    pub records: Vec<Record>,
    pub nseq: Nseq,
}

/// One change observed by `props.watch`. The subscriber's loop applies
/// these to the in-memory snapshot.
///
/// `Delete` carries only the [`RecordKey`] because SPEC-12 §5.5
/// `Pointer` payload mode (the default, mandatory whenever any field
/// is `secret`) does not echo the deleted value.
///
/// `CaughtUp` is the **replay-caught-up** marker that closes plan
/// §3.1 step 3. Server emits it once after draining every record with
/// `nseq > since` and before forwarding live events; the subscriber
/// uses the first `CaughtUp` per namespace to flip its readiness
/// gate. The wire contract lives in SPEC-12 §5.5 (v0.2.1+,
/// 2026-05-21): the `<svc>.props.watch` reply stream carries one
/// `{ "event_type": "caught_up", "namespace": "<ns>", "nseq": <N> }`
/// control message per watch, after replay drains and before live
/// events. Without it, a verifier could admit traffic between
/// `list` capturing `nseq` and the replay queue being applied,
/// silently losing changes.
#[derive(Debug, Clone)]
pub enum WatchEvent {
    Upsert(Record),
    Delete(RecordKey),
    /// Replay-caught-up marker. `nseq` is the namespace high-water
    /// mark the server observed at install time (SPEC-12 §5.5
    /// v0.2.1); subscribers may compare against subsequent
    /// `caught_up` redeliveries on the same watch — same `nseq` is
    /// a redelivery to ignore, a different `nseq` is a protocol
    /// violation and the watch should be closed and reseeded.
    CaughtUp {
        nseq: Nseq,
    },
}

/// Boxed `Send` stream of [`WatchEvent`]s. Erased here so the trait
/// stays object-safe and concrete clients don't leak their stream
/// types into the subscriber.
pub type WatchStream = Pin<Box<dyn Stream<Item = Result<WatchEvent, WgdError>> + Send>>;

/// Minimal SPEC-12 §5.5 surface the cache subscriber needs.
///
/// Real implementation: wraps a `cosmix-lib-client` Bus handle to
/// `cosmix-wgd`. Test implementation: in-process fake.
///
/// Only `props.list` + `props.watch` are needed; the cache is
/// read-only against wgd.
#[async_trait]
pub trait WgdClient: Send + Sync + 'static {
    /// Plan §3.1 step 1 — `wgd.props.list namespace=<ns>`. Returns the
    /// current record set and the namespace's `nseq` high-water mark.
    async fn list_records(&self, namespace: &str) -> Result<Listing, WgdError>;

    /// Plan §3.1 step 2 — `wgd.props.watch namespace=<ns>
    /// since_nseq=<since>`. The stream replays every commit with
    /// `nseq > since`, then stays open for new commits.
    async fn watch_records(&self, namespace: &str, since: Nseq) -> Result<WatchStream, WgdError>;
}

// --- Conversion seam -----------------------------------------------------

fn as_object(value: &PropValue) -> Result<&BTreeMap<String, PropValue>, WgdParseError> {
    value.as_object().ok_or(WgdParseError::NotAnObject)
}

fn require<'a>(
    obj: &'a BTreeMap<String, PropValue>,
    field: &str,
) -> Result<&'a PropValue, WgdParseError> {
    obj.get(field).ok_or_else(|| WgdParseError::MissingField {
        field: field.to_owned(),
    })
}

fn as_string(field: &str, v: &PropValue) -> Result<String, WgdParseError> {
    match v {
        PropValue::String(s) => Ok(s.clone()),
        other => Err(WgdParseError::WrongType {
            field: field.to_owned(),
            expected: "string",
            got: other.type_name().to_owned(),
        }),
    }
}

fn as_bool(field: &str, v: &PropValue) -> Result<bool, WgdParseError> {
    match v {
        PropValue::Bool(b) => Ok(*b),
        other => Err(WgdParseError::WrongType {
            field: field.to_owned(),
            expected: "bool",
            got: other.type_name().to_owned(),
        }),
    }
}

fn as_u64(field: &str, v: &PropValue) -> Result<u64, WgdParseError> {
    match v {
        PropValue::UInt(n) => Ok(*n),
        // Tolerate Int for the JSON-ambiguity case noted in
        // cosmix-lib-props-core's value.rs module docs: small non-negative
        // values round-trip as `Int` through JSON even when the
        // schema says u64. Negative Ints are still rejected as
        // wrong-type.
        PropValue::Int(n) if *n >= 0 => Ok(*n as u64),
        other => Err(WgdParseError::WrongType {
            field: field.to_owned(),
            expected: "u64",
            got: other.type_name().to_owned(),
        }),
    }
}

fn as_bytes(field: &str, v: &PropValue) -> Result<Vec<u8>, WgdParseError> {
    // SPEC-12 §4.3 bytes are base64-encoded on the wire and arrive as
    // `PropValue::String`. The conversion seam matches that wire
    // shape; non-string variants are rejected.
    match v {
        PropValue::String(s) => {
            use base64::Engine as _;
            use base64::engine::general_purpose::STANDARD;
            STANDARD
                .decode(s.as_bytes())
                .map_err(|e| WgdParseError::WrongType {
                    field: field.to_owned(),
                    expected: "bytes (base64)",
                    got: format!("invalid base64: {e}"),
                })
        }
        other => Err(WgdParseError::WrongType {
            field: field.to_owned(),
            expected: "bytes (base64 string)",
            got: other.type_name().to_owned(),
        }),
    }
}

fn as_timestamp(field: &str, v: &PropValue) -> Result<DateTime<Utc>, WgdParseError> {
    let s = as_string(field, v)?;
    parse_rfc3339(&s).map_err(|e| WgdParseError::BadTimestamp {
        field: field.to_owned(),
        reason: e.to_string(),
    })
}

fn as_list_of_strings(field: &str, v: &PropValue) -> Result<Vec<String>, WgdParseError> {
    let list = match v {
        PropValue::List(l) => l,
        other => {
            return Err(WgdParseError::WrongType {
                field: field.to_owned(),
                expected: "list<string>",
                got: other.type_name().to_owned(),
            });
        }
    };
    let mut out = Vec::with_capacity(list.len());
    for (i, item) in list.iter().enumerate() {
        match item {
            PropValue::String(s) => out.push(s.clone()),
            other => {
                return Err(WgdParseError::BadListItem {
                    field: field.to_owned(),
                    index: i,
                    expected: "string",
                    got: other.type_name().to_owned(),
                });
            }
        }
    }
    Ok(out)
}

fn optional<'a>(obj: &'a BTreeMap<String, PropValue>, field: &str) -> Option<&'a PropValue> {
    match obj.get(field) {
        Some(PropValue::Null) | None => None,
        Some(v) => Some(v),
    }
}

/// Plan §4 — convert a `wgd.trusted_meshes` record into the core
/// [`TrustedMesh`] fixture the resolver consumes.
pub fn parse_trusted_mesh(record: &Record) -> Result<TrustedMesh, WgdParseError> {
    let obj = as_object(&record.value)?;
    let mesh_fqdn = as_string("mesh_fqdn", require(obj, "mesh_fqdn")?)?;
    let current_pubkey = as_bytes("current_pubkey", require(obj, "current_pubkey")?)?;
    let current_selector = as_string("current_selector", require(obj, "current_selector")?)?;
    let previous_pubkey = optional(obj, "previous_pubkey")
        .map(|v| as_bytes("previous_pubkey", v))
        .transpose()?;
    let previous_selector = optional(obj, "previous_selector")
        .map(|v| as_string("previous_selector", v))
        .transpose()?;
    let previous_pubkey_valid_until = optional(obj, "previous_pubkey_valid_until")
        .map(|v| as_timestamp("previous_pubkey_valid_until", v))
        .transpose()?;
    let last_verified_at = as_timestamp("last_verified_at", require(obj, "last_verified_at")?)?;
    let max_pubkey_age_secs = as_u64("max_pubkey_age_secs", require(obj, "max_pubkey_age_secs")?)?;
    let enabled = as_bool("enabled", require(obj, "enabled")?)?;

    Ok(TrustedMesh {
        mesh_fqdn,
        current_pubkey,
        current_selector,
        previous_pubkey,
        previous_selector,
        previous_pubkey_valid_until,
        last_verified_at,
        max_pubkey_age_secs,
        enabled,
    })
}

/// Plan §5 — convert a `wgd.grants` record into the core [`Grant`]
/// fixture the resolver consumes.
pub fn parse_grant(record: &Record) -> Result<Grant, WgdParseError> {
    let obj = as_object(&record.value)?;
    let grant_id = as_string("grant_id", require(obj, "grant_id")?)?;
    let mesh_fqdn = as_string("mesh_fqdn", require(obj, "mesh_fqdn")?)?;
    let capabilities = as_list_of_strings("capabilities", require(obj, "capabilities")?)?
        .into_iter()
        .map(Cap::new)
        .collect::<CapabilitySet>();
    let valid_from = as_timestamp("valid_from", require(obj, "valid_from")?)?;
    let valid_until = as_timestamp("valid_until", require(obj, "valid_until")?)?;
    let enabled = as_bool("enabled", require(obj, "enabled")?)?;

    Ok(Grant {
        grant_id,
        mesh_fqdn,
        capabilities,
        valid_from,
        valid_until,
        enabled,
    })
}

/// Plan §9 — convert a `wgd.cross_mesh_exposable` record into the
/// `(namespace_id, cap)` pair the cache buckets by SPEC-12 namespace.
///
/// The subscriber accumulates these into the
/// `HashMap<String, CapabilitySet>` keyed by `cap_namespace_prefix`
/// that `TrustGrantsCacheHandle::exposable_for_namespace` reads.
pub fn parse_exposable_entry(record: &Record) -> Result<(String, Cap), WgdParseError> {
    let obj = as_object(&record.value)?;
    let cap_namespace_prefix = as_string(
        "cap_namespace_prefix",
        require(obj, "cap_namespace_prefix")?,
    )?;
    let cap_token = as_string("cap_token", require(obj, "cap_token")?)?;
    Ok((cap_namespace_prefix, Cap::new(cap_token)))
}

#[cfg(test)]
mod tests {
    use super::*;
    use base64::Engine as _;
    use base64::engine::general_purpose::STANDARD;
    use chrono::TimeZone as _;
    use cosmix_props::{NamespaceName, Version};

    fn rk(ns: &str, key: &str) -> RecordKey {
        RecordKey::collection(NamespaceName::new(ns).unwrap(), key)
    }

    fn obj(pairs: &[(&str, PropValue)]) -> PropValue {
        let mut m = BTreeMap::new();
        for (k, v) in pairs {
            m.insert((*k).to_owned(), v.clone());
        }
        PropValue::Object(m)
    }

    fn record(ns: &str, key: &str, value: PropValue) -> Record {
        Record {
            key: rk(ns, key),
            value,
            version: Version(1),
            nseq: Nseq(1),
            lifecycle: None,
        }
    }

    fn trusted_record_fields() -> Vec<(&'static str, PropValue)> {
        vec![
            ("mesh_fqdn", PropValue::String("channie.net".into())),
            (
                "current_pubkey",
                PropValue::String(STANDARD.encode([0u8; 32])),
            ),
            ("current_selector", PropValue::String("2026q3".into())),
            (
                "last_verified_at",
                PropValue::String("2026-06-01T11:00:00Z".into()),
            ),
            ("max_pubkey_age_secs", PropValue::UInt(7 * 86400)),
            ("enabled", PropValue::Bool(true)),
        ]
    }

    #[test]
    fn parse_trusted_mesh_happy_path() {
        let r = record(
            "trusted_meshes",
            "channie.net",
            obj(&trusted_record_fields()),
        );
        let t = parse_trusted_mesh(&r).unwrap();
        assert_eq!(t.mesh_fqdn, "channie.net");
        assert_eq!(t.current_pubkey, vec![0u8; 32]);
        assert_eq!(t.current_selector, "2026q3");
        assert!(t.previous_pubkey.is_none());
        assert!(t.previous_selector.is_none());
        assert!(t.previous_pubkey_valid_until.is_none());
        assert_eq!(t.max_pubkey_age_secs, 7 * 86400);
        assert!(t.enabled);
        assert_eq!(
            t.last_verified_at,
            Utc.with_ymd_and_hms(2026, 6, 1, 11, 0, 0).unwrap()
        );
    }

    #[test]
    fn parse_trusted_mesh_with_previous_key() {
        let mut fields = trusted_record_fields();
        fields.push((
            "previous_pubkey",
            PropValue::String(STANDARD.encode([1u8; 32])),
        ));
        fields.push(("previous_selector", PropValue::String("2026q2".into())));
        fields.push((
            "previous_pubkey_valid_until",
            PropValue::String("2026-09-01T00:00:00Z".into()),
        ));
        let r = record("trusted_meshes", "channie.net", obj(&fields));
        let t = parse_trusted_mesh(&r).unwrap();
        assert_eq!(t.previous_pubkey, Some(vec![1u8; 32]));
        assert_eq!(t.previous_selector.as_deref(), Some("2026q2"));
        assert!(t.previous_pubkey_valid_until.is_some());
    }

    #[test]
    fn parse_trusted_mesh_null_optional_is_none_not_an_error() {
        let mut fields = trusted_record_fields();
        // SPEC-12 §4.3 represents an absent option<T> as either the
        // field being omitted or the wire JSON `null`. Both forms must
        // round-trip to Option::None.
        fields.push(("previous_pubkey", PropValue::Null));
        fields.push(("previous_selector", PropValue::Null));
        fields.push(("previous_pubkey_valid_until", PropValue::Null));
        let r = record("trusted_meshes", "channie.net", obj(&fields));
        let t = parse_trusted_mesh(&r).unwrap();
        assert!(t.previous_pubkey.is_none());
        assert!(t.previous_selector.is_none());
        assert!(t.previous_pubkey_valid_until.is_none());
    }

    #[test]
    fn parse_trusted_mesh_missing_field_errors() {
        let mut fields = trusted_record_fields();
        fields.retain(|(k, _)| *k != "current_pubkey");
        let r = record("trusted_meshes", "channie.net", obj(&fields));
        let err = parse_trusted_mesh(&r).unwrap_err();
        assert!(matches!(err, WgdParseError::MissingField { field } if field == "current_pubkey"));
    }

    #[test]
    fn parse_trusted_mesh_wrong_type_errors() {
        let mut fields = trusted_record_fields();
        // Replace enabled:bool with an int.
        for f in fields.iter_mut() {
            if f.0 == "enabled" {
                f.1 = PropValue::Int(1);
            }
        }
        let r = record("trusted_meshes", "channie.net", obj(&fields));
        let err = parse_trusted_mesh(&r).unwrap_err();
        assert!(matches!(
            err,
            WgdParseError::WrongType { ref field, expected: "bool", .. } if field == "enabled"
        ));
    }

    #[test]
    fn parse_trusted_mesh_u64_tolerates_int_for_jsonability() {
        let mut fields = trusted_record_fields();
        for f in fields.iter_mut() {
            if f.0 == "max_pubkey_age_secs" {
                f.1 = PropValue::Int(7 * 86400);
            }
        }
        let r = record("trusted_meshes", "channie.net", obj(&fields));
        let t = parse_trusted_mesh(&r).unwrap();
        assert_eq!(t.max_pubkey_age_secs, 7 * 86400);
    }

    #[test]
    fn parse_trusted_mesh_negative_int_for_u64_field_rejected() {
        let mut fields = trusted_record_fields();
        for f in fields.iter_mut() {
            if f.0 == "max_pubkey_age_secs" {
                f.1 = PropValue::Int(-1);
            }
        }
        let r = record("trusted_meshes", "channie.net", obj(&fields));
        assert!(matches!(
            parse_trusted_mesh(&r).unwrap_err(),
            WgdParseError::WrongType { ref field, expected: "u64", .. } if field == "max_pubkey_age_secs"
        ));
    }

    #[test]
    fn parse_trusted_mesh_bad_base64_errors() {
        let mut fields = trusted_record_fields();
        for f in fields.iter_mut() {
            if f.0 == "current_pubkey" {
                f.1 = PropValue::String("not-base64!@#".into());
            }
        }
        let r = record("trusted_meshes", "channie.net", obj(&fields));
        let err = parse_trusted_mesh(&r).unwrap_err();
        assert!(matches!(
            err,
            WgdParseError::WrongType { ref field, .. } if field == "current_pubkey"
        ));
    }

    #[test]
    fn parse_trusted_mesh_bad_timestamp_errors() {
        let mut fields = trusted_record_fields();
        for f in fields.iter_mut() {
            if f.0 == "last_verified_at" {
                f.1 = PropValue::String("not a timestamp".into());
            }
        }
        let r = record("trusted_meshes", "channie.net", obj(&fields));
        assert!(matches!(
            parse_trusted_mesh(&r).unwrap_err(),
            WgdParseError::BadTimestamp { ref field, .. } if field == "last_verified_at"
        ));
    }

    #[test]
    fn parse_grant_happy_path() {
        let r = record(
            "grants",
            "01HXEXAMPLE",
            obj(&[
                ("grant_id", PropValue::String("01HXEXAMPLE".into())),
                ("mesh_fqdn", PropValue::String("channie.net".into())),
                (
                    "capabilities",
                    PropValue::List(vec![
                        PropValue::String("props.read:maild.accounts".into()),
                        PropValue::String("props.read:maild.engine_config".into()),
                    ]),
                ),
                (
                    "valid_from",
                    PropValue::String("2026-05-31T12:00:00Z".into()),
                ),
                (
                    "valid_until",
                    PropValue::String("2026-07-01T12:00:00Z".into()),
                ),
                ("enabled", PropValue::Bool(true)),
            ]),
        );
        let g = parse_grant(&r).unwrap();
        assert_eq!(g.grant_id, "01HXEXAMPLE");
        assert_eq!(g.mesh_fqdn, "channie.net");
        assert_eq!(g.capabilities.len(), 2);
        assert!(g.enabled);
    }

    #[test]
    fn parse_grant_bad_capability_item_errors() {
        let r = record(
            "grants",
            "01H",
            obj(&[
                ("grant_id", PropValue::String("01H".into())),
                ("mesh_fqdn", PropValue::String("channie.net".into())),
                (
                    "capabilities",
                    // capabilities is list<string>; a non-string item
                    // surfaces as BadListItem with the offending index.
                    PropValue::List(vec![
                        PropValue::String("props.read:maild.accounts".into()),
                        PropValue::Int(42),
                    ]),
                ),
                (
                    "valid_from",
                    PropValue::String("2026-05-31T12:00:00Z".into()),
                ),
                (
                    "valid_until",
                    PropValue::String("2026-07-01T12:00:00Z".into()),
                ),
                ("enabled", PropValue::Bool(true)),
            ]),
        );
        assert!(matches!(
            parse_grant(&r).unwrap_err(),
            WgdParseError::BadListItem { ref field, index: 1, .. } if field == "capabilities"
        ));
    }

    #[test]
    fn parse_exposable_entry_happy_path() {
        let r = record(
            "cross_mesh_exposable",
            "maild:props.read:maild.accounts",
            obj(&[
                (
                    "pk",
                    PropValue::String("maild:props.read:maild.accounts".into()),
                ),
                ("owning_service", PropValue::String("maild".into())),
                (
                    "cap_token",
                    PropValue::String("props.read:maild.accounts".into()),
                ),
                (
                    "cap_namespace_prefix",
                    PropValue::String("maild.accounts".into()),
                ),
                (
                    "registered_at",
                    PropValue::String("2026-05-31T12:00:00Z".into()),
                ),
                ("lifetime_kind", PropValue::String("ephemeral".into())),
            ]),
        );
        let (ns, cap) = parse_exposable_entry(&r).unwrap();
        assert_eq!(ns, "maild.accounts");
        assert_eq!(cap.as_str(), "props.read:maild.accounts");
    }

    #[test]
    fn record_value_not_object_errors() {
        // Defensive: a malformed wgd record where value isn't an
        // Object at all must surface a clean NotAnObject error before
        // any field access.
        let r = record("trusted_meshes", "channie.net", PropValue::Null);
        assert_eq!(
            parse_trusted_mesh(&r).unwrap_err(),
            WgdParseError::NotAnObject
        );
    }
}
