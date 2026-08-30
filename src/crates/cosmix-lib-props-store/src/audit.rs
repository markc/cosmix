//! Per-row keyed-HMAC audit digest computation (SPEC 12 §10).
//!
//! Every record event carries a single keyed HMAC-SHA256 of the record's
//! canonical serialisation concatenated with the row's `nseq` (big-endian
//! 8 bytes). The key is a 32-byte secret generated per namespace at
//! registration and persisted alongside the namespace's storage. There is
//! **no `prev_digest` chain** — each row's digest is independent (§10,
//! §16.5).
//!
//! This module is storage-backend-agnostic: it exposes
//! [`AuditKey`] + [`canonical_serialise`] + [`compute_digest`]. The backend
//! holds the key (in-memory for `MemoryStore`, persisted in
//! `__props_audit_keys` for `SqliteStore`) and calls `compute_digest`
//! inside the same transaction as the record write.
//!
//! Tamper-detection is a privileged operation: a verifier with the
//! namespace audit key can re-derive `audit_digest` against the
//! authoritative current record state (via `<svc>.props.get`) and compare.
//! Passive subscribers to `<svc>.props.audit` cannot verify, by design
//! (§16.5).

use hmac::{Hmac, Mac};
use rand::RngCore;
use sha2::Sha256;

use crate::record::Nseq;
use cosmix_props_core::value::PropValue;

/// Per-namespace 32-byte audit key. The raw bytes never leave the storage
/// backend; the [`compute_digest`] function consumes the key by reference
/// and emits only the hex-encoded HMAC.
#[derive(Clone)]
pub struct AuditKey([u8; 32]);

impl AuditKey {
    /// Generate a fresh 32-byte key from the OS entropy source. Called
    /// once per namespace at registration time.
    pub fn generate() -> Self {
        let mut bytes = [0u8; 32];
        rand::rngs::OsRng.fill_bytes(&mut bytes);
        Self(bytes)
    }

    /// Construct a key from raw bytes. Used by persistent backends when
    /// reloading the key from disk on startup.
    pub fn from_bytes(bytes: [u8; 32]) -> Self {
        Self(bytes)
    }

    /// Raw bytes for persistence. Callers MUST store this only inside the
    /// owning service's private storage area — never in any verb response
    /// or audit body.
    pub fn as_bytes(&self) -> &[u8; 32] {
        &self.0
    }
}

/// SPEC 12 §10 canonical serialisation. The audit digest is computed over
/// `canonical_serialise(record) || nseq.to_be_bytes()`. The spec leaves
/// the exact byte form to the storage backend; this implementation emits
/// a deterministic UTF-8 JSON encoding (object keys sorted) of the
/// post-commit record value. For delete events the value is logically
/// gone; we encode that as a fixed `null` literal so the digest is
/// well-defined and distinguishable from an empty object.
///
/// The function is intentionally pure so verifiers (operators with the
/// audit key) can re-derive a digest from a `<svc>.props.get` response.
pub fn canonical_serialise(value: &PropValue) -> Vec<u8> {
    fn write_value(out: &mut Vec<u8>, v: &PropValue) {
        match v {
            PropValue::Null => out.extend_from_slice(b"null"),
            PropValue::Bool(b) => out.extend_from_slice(if *b { b"true" } else { b"false" }),
            // Signedness collapses via the same canonical decimal that
            // serde_json emits — `Int(3)` and `UInt(3)` map to the same
            // bytes. Verifiers re-deriving against `<svc>.props.get`
            // observe this canonical form (see `value::tests::
            // json_signedness_collapse_pinned`).
            PropValue::Int(n) => out.extend_from_slice(n.to_string().as_bytes()),
            PropValue::UInt(n) => out.extend_from_slice(n.to_string().as_bytes()),
            PropValue::Float(f) => {
                // Non-finite floats (NaN/±Inf) wire-encode to JSON
                // `null` in `From<&PropValue> for Json` (value.rs).
                // Mirror that here so verifiers re-deriving the digest
                // from a `<svc>.props.get` response — which observes
                // `null` for the field — compute the same input bytes.
                // Coercing to `0.0` silently would collide with a real
                // `Float(0.0)` and diverge from the wire form.
                match serde_json::Number::from_f64(*f) {
                    Some(n) => out.extend_from_slice(n.to_string().as_bytes()),
                    None => out.extend_from_slice(b"null"),
                }
            }
            PropValue::String(s) => {
                let s_json = serde_json::to_string(s).expect("String always serialises");
                out.extend_from_slice(s_json.as_bytes());
            }
            PropValue::List(items) => {
                out.push(b'[');
                for (i, item) in items.iter().enumerate() {
                    if i > 0 {
                        out.push(b',');
                    }
                    write_value(out, item);
                }
                out.push(b']');
            }
            PropValue::Object(map) => {
                out.push(b'{');
                // BTreeMap iterates in key order; emit verbatim.
                for (i, (k, v)) in map.iter().enumerate() {
                    if i > 0 {
                        out.push(b',');
                    }
                    let k_json = serde_json::to_string(k).expect("Key always serialises");
                    out.extend_from_slice(k_json.as_bytes());
                    out.push(b':');
                    write_value(out, v);
                }
                out.push(b'}');
            }
        }
    }
    let mut out = Vec::new();
    write_value(&mut out, value);
    out
}

/// `audit_digest = HMAC-SHA256(audit_key, canonical_serialise(record) ||
/// nseq.to_be_bytes())`. Returns the lowercase hex encoding for storage
/// in the [`crate::record::RecordEvent::audit_digest`] field.
///
/// For delete events the canonical record value is taken to be
/// [`PropValue::Null`] (the record is gone). Callers MUST pass `&None` or
/// `Some(&PropValue::Null)` to keep digest derivation symmetric across
/// store backends.
pub fn compute_digest(key: &AuditKey, value: &PropValue, nseq: Nseq) -> String {
    let mut mac =
        Hmac::<Sha256>::new_from_slice(key.as_bytes()).expect("HMAC-SHA256 accepts any key length");
    let bytes = canonical_serialise(value);
    mac.update(&bytes);
    mac.update(&nseq.0.to_be_bytes());
    let tag = mac.finalize().into_bytes();
    hex::encode(tag)
}

/// Tombstone marker for delete-event digest computation. Centralised so
/// MemoryStore and SqliteStore agree on the canonical "record is gone"
/// value.
pub fn tombstone_value() -> PropValue {
    PropValue::Null
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeMap;

    fn obj(pairs: &[(&str, PropValue)]) -> PropValue {
        let mut m = BTreeMap::new();
        for (k, v) in pairs {
            m.insert((*k).to_string(), v.clone());
        }
        PropValue::Object(m)
    }

    #[test]
    fn canonical_form_is_key_sorted_and_stable() {
        let v1 = obj(&[("b", PropValue::from("1")), ("a", PropValue::from(7_u64))]);
        let v2 = obj(&[("a", PropValue::from(7_u64)), ("b", PropValue::from("1"))]);
        // Insertion order differs; BTreeMap collapses to the same iteration
        // order so the canonical form is byte-identical.
        assert_eq!(canonical_serialise(&v1), canonical_serialise(&v2));
        let s = String::from_utf8(canonical_serialise(&v1)).unwrap();
        assert_eq!(s, r#"{"a":7,"b":"1"}"#);
    }

    #[test]
    fn digest_changes_with_value_and_nseq() {
        let key = AuditKey::from_bytes([7u8; 32]);
        let v = obj(&[("x", PropValue::from("y"))]);
        let d1 = compute_digest(&key, &v, Nseq(1));
        let d2 = compute_digest(&key, &v, Nseq(2));
        let v2 = obj(&[("x", PropValue::from("z"))]);
        let d3 = compute_digest(&key, &v2, Nseq(1));
        assert_eq!(d1.len(), 64, "hex(SHA256) is 64 chars");
        assert_ne!(d1, d2, "different nseq must yield different digest");
        assert_ne!(d1, d3, "different value must yield different digest");
    }

    #[test]
    fn digest_changes_with_key() {
        let k1 = AuditKey::from_bytes([1u8; 32]);
        let k2 = AuditKey::from_bytes([2u8; 32]);
        let v = obj(&[("x", PropValue::from("y"))]);
        assert_ne!(
            compute_digest(&k1, &v, Nseq(1)),
            compute_digest(&k2, &v, Nseq(1))
        );
    }

    #[test]
    fn tombstone_digest_is_well_defined() {
        let key = AuditKey::from_bytes([3u8; 32]);
        let d = compute_digest(&key, &tombstone_value(), Nseq(42));
        // Tombstone canonical form is the JSON literal `null`, so the
        // digest is deterministic given the key + nseq.
        let d_again = compute_digest(&key, &PropValue::Null, Nseq(42));
        assert_eq!(d, d_again);
    }

    #[test]
    fn non_finite_floats_canonicalise_as_null() {
        // Mirrors the wire encoding in `From<&PropValue> for Json` so a
        // verifier reading back the record sees `null` and re-derives
        // an identical digest.
        let nan = canonical_serialise(&PropValue::Float(f64::NAN));
        let inf = canonical_serialise(&PropValue::Float(f64::INFINITY));
        let neg_inf = canonical_serialise(&PropValue::Float(f64::NEG_INFINITY));
        let null = canonical_serialise(&PropValue::Null);
        assert_eq!(nan, null);
        assert_eq!(inf, null);
        assert_eq!(neg_inf, null);
        // Finite 0.0 must still produce "0.0" — NOT collide with null.
        let zero = canonical_serialise(&PropValue::Float(0.0));
        assert_ne!(zero, null);
    }

    #[test]
    fn generated_keys_are_unique() {
        // OS RNG should not return all-zero keys on practical runs; this
        // is a smoke test that `generate` is wired to a real entropy
        // source, not that HMAC is secure.
        let k1 = AuditKey::generate();
        let k2 = AuditKey::generate();
        assert_ne!(k1.as_bytes(), &[0u8; 32]);
        assert_ne!(k1.as_bytes(), k2.as_bytes());
    }
}
