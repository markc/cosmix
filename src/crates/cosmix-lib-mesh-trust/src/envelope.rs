//! Cross-mesh signed envelope (plan §8) — types, parse, and v1
//! canonical-bytes serialisation.
//!
//! The envelope is the signed payload of a cross-mesh HTTPS call. All
//! authenticated fields live inside [`Envelope`]; the outer
//! [`SignedEnvelope`] wrapper carries the Ed25519 signature over the
//! v1 canonical bytes.
//!
//! # Canonical bytes (v1)
//!
//! The signed-bytes contract is the strict-data form referenced by
//! plan §8:
//!
//! - **Top-level shape.** A JSON object with exactly the twelve
//!   documented envelope fields, no more, no less. Unknown fields at
//!   parse time are a hard error (parse-fail, not silent-drop), so a
//!   v2 envelope cannot be downgraded to v1 by a v1-only verifier.
//! - **Deterministic key order.** Object keys (including nested
//!   objects inside `headers` and `body`) are emitted in lexicographic
//!   order over their UTF-8 codepoint sequences.
//! - **No whitespace.** Zero bytes of whitespace between any tokens:
//!   `{"a":1,"b":2}`, not `{ "a": 1, "b": 2 }`.
//! - **String escaping.** Only the characters JSON requires to be
//!   escaped are escaped (`"`, `\`, `\n`, `\r`, `\t`, `\b`, `\f`, and
//!   C0 controls via `\u00XX`). All other Unicode codepoints are
//!   emitted verbatim as UTF-8 bytes. No `\uXXXX` for non-control BMP
//!   characters, no UTF-16 surrogate pairs.
//! - **No Unicode normalisation.** Codepoints are preserved verbatim.
//!   The verifier re-canonicalises from parsed fields, so producer
//!   and verifier use the same codepoint sequence by construction.
//! - **Numbers.** Emitted via [`serde_json::Number`]'s default
//!   representation. v1 intentionally inherits serde_json's parsing
//!   model: integers in the `[i64::MIN, u64::MAX]` range round-trip
//!   losslessly; floats round-trip as f64; very large numeric
//!   literals that overflow i64/u64 silently fall through to f64
//!   (potentially lossy). The `arbitrary_precision` cargo feature
//!   is intentionally not enabled — it produces different lexical
//!   representations for the same value and would itself be a
//!   cross-producer divergence trap. Producers MUST NOT include
//!   numbers in `body` / `headers` that exceed lossless f64
//!   representation; doing so does not break the signature
//!   contract (signer and verifier both parse through the same
//!   crate and produce the same canonical bytes) but it does break
//!   "what the operator typed equals what the verifier sees".
//!   NaN/Infinity are rejected by serde_json at parse time and
//!   cannot appear.
//! - **Duplicate object keys.** Rejected at parse time, recursively,
//!   for every object including nested `headers` / `body` levels.
//!   The default `serde_json::Value` deserializer silently collapses
//!   duplicate keys (last-write-wins) — a custom strict deserializer
//!   defends against the integrity gap where a verifier's parsed
//!   canonical bytes match the signature while a separate downstream
//!   consumer parsing the same wire bytes (audit log, jq filter,
//!   different JSON lib) sees a different earlier value for the same
//!   key.
//!
//! `envelope_version` is itself signed, so v2 cannot be downgraded to
//! v1: a v2 envelope changes the canonical-bytes contract and a v1
//! verifier will refuse to parse it.
//!
//! # Verifier contract
//!
//! Per plan §8: *"Verifier MUST re-canonicalise from parsed fields,
//! never verify against bytes-as-received."* [`Envelope::canonical_bytes`]
//! is the only signature input — call sites must not feed the raw
//! wire bytes to the verifier.

use crate::canonical::{emit_json_string, emit_value};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::BTreeMap;

/// Envelope schema version. v1 pins the canonicalisation rules
/// documented at the module level.
pub const ENVELOPE_VERSION_V1: &str = "v1";

/// Signed cross-mesh envelope — the JSON wire payload.
///
/// Wire form:
///
/// ```json
/// {
///   "envelope":  { ... twelve fields ... },
///   "signature": "<base64-ed25519-sig>"
/// }
/// ```
///
/// The signature is over [`Envelope::canonical_bytes`]; the wire
/// bytes themselves are never used as the signature input.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SignedEnvelope {
    pub envelope: Envelope,
    /// Base64-encoded Ed25519 signature over `envelope.canonical_bytes()`.
    pub signature: String,
}

/// Signed payload (plan §8). All fields are part of the signature
/// input; nothing outside this struct is authenticated.
///
/// Field order in this struct does not affect canonical bytes:
/// [`canonical_bytes`](Self::canonical_bytes) sorts keys
/// lexicographically. Order is preserved here for readability.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Envelope {
    pub envelope_version: String,
    pub target_mesh: String,
    pub target_service: String,
    pub target_endpoint: String,
    pub caller_mesh: String,
    pub caller_selector: String,
    pub caller_node_id: String,
    pub command: String,
    /// Verb structured headers (e.g. `{"namespace": "accounts"}`).
    /// Must be a JSON object; may be empty (`{}`).
    #[serde(deserialize_with = "crate::canonical::deserialize_strict_value")]
    pub headers: Value,
    /// Verb body; arbitrary JSON value (including `null`).
    #[serde(deserialize_with = "crate::canonical::deserialize_strict_value")]
    pub body: Value,
    /// ISO-8601 UTC timestamp (e.g. `2026-05-20T03:14:00Z`).
    pub ts: String,
    /// Base64-encoded 32-byte random nonce.
    pub nonce: String,
}

/// Parse / structural errors from the envelope layer.
///
/// Signature verification, freshness, and capability math live in
/// sibling modules (P0-C3) and produce their own error types.
#[derive(Debug, thiserror::Error)]
pub enum EnvelopeError {
    #[error("JSON parse failure: {0}")]
    Json(#[from] serde_json::Error),

    #[error(
        "unsupported envelope_version: {0:?} (this crate accepts only {})",
        ENVELOPE_VERSION_V1
    )]
    UnsupportedVersion(String),

    #[error("required field {0} is empty")]
    EmptyField(&'static str),

    #[error("headers must be a JSON object, got {0}")]
    HeadersNotObject(&'static str),
}

impl SignedEnvelope {
    /// Parse a wire JSON payload into a [`SignedEnvelope`] and
    /// validate the envelope's structural invariants (version,
    /// non-empty required fields, headers shape).
    ///
    /// Returns the parsed value *before* signature verification —
    /// signature verify is the caller's next step and lives in
    /// P0-C3's sig module. The verifier must pass the result of
    /// [`Envelope::canonical_bytes`] to its verify routine, never the
    /// wire bytes.
    pub fn parse(wire: &[u8]) -> Result<Self, EnvelopeError> {
        let parsed: SignedEnvelope = serde_json::from_slice(wire)?;
        parsed.envelope.validate()?;
        Ok(parsed)
    }
}

impl Envelope {
    /// Validate structural invariants documented on each field. Called
    /// automatically by [`SignedEnvelope::parse`]; exposed so
    /// constructors building envelopes from non-wire sources can run
    /// the same checks.
    pub fn validate(&self) -> Result<(), EnvelopeError> {
        if self.envelope_version != ENVELOPE_VERSION_V1 {
            return Err(EnvelopeError::UnsupportedVersion(
                self.envelope_version.clone(),
            ));
        }
        let required = [
            ("target_mesh", &self.target_mesh),
            ("target_service", &self.target_service),
            ("target_endpoint", &self.target_endpoint),
            ("caller_mesh", &self.caller_mesh),
            ("caller_selector", &self.caller_selector),
            ("caller_node_id", &self.caller_node_id),
            ("command", &self.command),
            ("ts", &self.ts),
            ("nonce", &self.nonce),
        ];
        for (name, val) in required {
            if val.is_empty() {
                return Err(EnvelopeError::EmptyField(name));
            }
        }
        match &self.headers {
            Value::Object(_) => {}
            Value::Null => return Err(EnvelopeError::HeadersNotObject("null")),
            Value::Bool(_) => return Err(EnvelopeError::HeadersNotObject("bool")),
            Value::Number(_) => return Err(EnvelopeError::HeadersNotObject("number")),
            Value::String(_) => return Err(EnvelopeError::HeadersNotObject("string")),
            Value::Array(_) => return Err(EnvelopeError::HeadersNotObject("array")),
        }
        Ok(())
    }

    /// Produce v1 canonical bytes — the exact byte sequence the
    /// signature is computed over. See the module-level
    /// documentation for the v1 contract.
    pub fn canonical_bytes(&self) -> Vec<u8> {
        // Build a BTreeMap of the top-level fields so the sorted-key
        // emission below sees the same lexicographic order regardless
        // of how the struct itself is ordered.
        let mut map: BTreeMap<&'static str, Value> = BTreeMap::new();
        map.insert("body", self.body.clone());
        map.insert("caller_mesh", Value::String(self.caller_mesh.clone()));
        map.insert("caller_node_id", Value::String(self.caller_node_id.clone()));
        map.insert(
            "caller_selector",
            Value::String(self.caller_selector.clone()),
        );
        map.insert("command", Value::String(self.command.clone()));
        map.insert(
            "envelope_version",
            Value::String(self.envelope_version.clone()),
        );
        map.insert("headers", self.headers.clone());
        map.insert("nonce", Value::String(self.nonce.clone()));
        map.insert(
            "target_endpoint",
            Value::String(self.target_endpoint.clone()),
        );
        map.insert("target_mesh", Value::String(self.target_mesh.clone()));
        map.insert("target_service", Value::String(self.target_service.clone()));
        map.insert("ts", Value::String(self.ts.clone()));

        let mut out = Vec::with_capacity(256);
        out.push(b'{');
        let mut first = true;
        for (k, v) in &map {
            if !first {
                out.push(b',');
            }
            first = false;
            emit_json_string(k, &mut out);
            out.push(b':');
            emit_value(v, &mut out);
        }
        out.push(b'}');
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn sample_envelope() -> Envelope {
        Envelope {
            envelope_version: ENVELOPE_VERSION_V1.into(),
            target_mesh: "example.com".into(),
            target_service: "maild".into(),
            target_endpoint: "/v1/mesh-call".into(),
            caller_mesh: "channie.net".into(),
            caller_selector: "2026q3".into(),
            caller_node_id: "a1b2".into(),
            command: "maild.props.list".into(),
            headers: json!({ "namespace": "accounts" }),
            body: Value::Null,
            ts: "2026-05-20T03:14:00Z".into(),
            nonce: "AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA=".into(),
        }
    }

    fn sample_signed(envelope: Envelope) -> SignedEnvelope {
        SignedEnvelope {
            envelope,
            signature: "AAAA".into(), // placeholder — not verified at parse time
        }
    }

    #[test]
    fn canonical_bytes_are_deterministic() {
        let e = sample_envelope();
        let a = e.canonical_bytes();
        let b = e.canonical_bytes();
        assert_eq!(a, b);
    }

    #[test]
    fn canonical_bytes_are_alphabetical_top_level() {
        let bytes = sample_envelope().canonical_bytes();
        let s = std::str::from_utf8(&bytes).unwrap();
        // Lexicographic order: body, caller_mesh, caller_node_id, ...
        let order = [
            "\"body\":",
            "\"caller_mesh\":",
            "\"caller_node_id\":",
            "\"caller_selector\":",
            "\"command\":",
            "\"envelope_version\":",
            "\"headers\":",
            "\"nonce\":",
            "\"target_endpoint\":",
            "\"target_mesh\":",
            "\"target_service\":",
            "\"ts\":",
        ];
        let mut cursor = 0;
        for key in order {
            let found = s[cursor..]
                .find(key)
                .unwrap_or_else(|| panic!("missing key {key:?} after offset {cursor}"));
            cursor += found + key.len();
        }
    }

    #[test]
    fn canonical_bytes_have_no_whitespace() {
        let bytes = sample_envelope().canonical_bytes();
        for &b in &bytes {
            assert!(
                !(b == b' ' || b == b'\n' || b == b'\r' || b == b'\t'),
                "found whitespace byte 0x{:02x} in canonical bytes",
                b
            );
        }
    }

    #[test]
    fn nested_objects_are_sorted() {
        let mut e = sample_envelope();
        e.headers = json!({ "z": 1, "a": 2, "m": 3 });
        let bytes = e.canonical_bytes();
        let s = std::str::from_utf8(&bytes).unwrap();
        let headers_start = s.find("\"headers\":").unwrap();
        let headers_slice = &s[headers_start..];
        let a_pos = headers_slice.find("\"a\":").unwrap();
        let m_pos = headers_slice.find("\"m\":").unwrap();
        let z_pos = headers_slice.find("\"z\":").unwrap();
        assert!(a_pos < m_pos && m_pos < z_pos, "headers keys not sorted");
    }

    #[test]
    fn round_trip_preserves_canonical_bytes() {
        // Producer: build envelope, sign canonical_bytes.
        let envelope = sample_envelope();
        let canonical_a = envelope.canonical_bytes();

        // Wire: serialise the SignedEnvelope wrapper. The serialised
        // wire bytes are *not* the canonical bytes — they include the
        // signature and may use a different field order.
        let signed = sample_signed(envelope);
        let wire = serde_json::to_vec(&signed).unwrap();

        // Verifier: parse, then re-canonicalise from parsed fields.
        let parsed = SignedEnvelope::parse(&wire).unwrap();
        let canonical_b = parsed.envelope.canonical_bytes();

        assert_eq!(canonical_a, canonical_b);
    }

    #[test]
    fn whitespace_in_wire_does_not_affect_canonical_bytes() {
        let signed = sample_signed(sample_envelope());
        let pretty = serde_json::to_vec_pretty(&signed).unwrap();
        let compact = serde_json::to_vec(&signed).unwrap();
        assert_ne!(pretty, compact, "sanity: pretty != compact");

        let from_pretty = SignedEnvelope::parse(&pretty)
            .unwrap()
            .envelope
            .canonical_bytes();
        let from_compact = SignedEnvelope::parse(&compact)
            .unwrap()
            .envelope
            .canonical_bytes();
        assert_eq!(from_pretty, from_compact);
    }

    #[test]
    fn field_reordering_in_wire_does_not_affect_canonical_bytes() {
        let envelope = sample_envelope();
        let canonical = envelope.canonical_bytes();

        // Hand-roll wire JSON with an unusual top-level order. The
        // wrapper is `signature` then `envelope`; the inner envelope
        // is also scrambled.
        let scrambled = r#"{
            "signature": "AAAA",
            "envelope": {
                "ts": "2026-05-20T03:14:00Z",
                "nonce": "AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA=",
                "envelope_version": "v1",
                "target_endpoint": "/v1/mesh-call",
                "target_service": "maild",
                "target_mesh": "example.com",
                "headers": {"namespace": "accounts"},
                "body": null,
                "command": "maild.props.list",
                "caller_selector": "2026q3",
                "caller_node_id": "a1b2",
                "caller_mesh": "channie.net"
            }
        }"#;
        let parsed = SignedEnvelope::parse(scrambled.as_bytes()).unwrap();
        assert_eq!(parsed.envelope.canonical_bytes(), canonical);
    }

    #[test]
    fn tampering_with_a_field_changes_canonical_bytes() {
        let original = sample_envelope().canonical_bytes();

        let mut tampered = sample_envelope();
        tampered.target_mesh = "evil.example".into();
        assert_ne!(tampered.canonical_bytes(), original);

        let mut tampered = sample_envelope();
        tampered.body = json!({ "extra": true });
        assert_ne!(tampered.canonical_bytes(), original);

        let mut tampered = sample_envelope();
        tampered.headers = json!({ "namespace": "rules" });
        assert_ne!(tampered.canonical_bytes(), original);
    }

    #[test]
    fn unsupported_version_rejected() {
        let mut e = sample_envelope();
        e.envelope_version = "v2".into();
        let wire = serde_json::to_vec(&sample_signed(e)).unwrap();
        let err = SignedEnvelope::parse(&wire).unwrap_err();
        assert!(matches!(err, EnvelopeError::UnsupportedVersion(_)));
    }

    #[test]
    fn empty_required_field_rejected() {
        let mut e = sample_envelope();
        e.target_mesh.clear();
        let wire = serde_json::to_vec(&sample_signed(e)).unwrap();
        let err = SignedEnvelope::parse(&wire).unwrap_err();
        assert!(matches!(err, EnvelopeError::EmptyField("target_mesh")));
    }

    #[test]
    fn headers_must_be_object() {
        let mut e = sample_envelope();
        e.headers = json!([1, 2, 3]);
        let wire = serde_json::to_vec(&sample_signed(e)).unwrap();
        let err = SignedEnvelope::parse(&wire).unwrap_err();
        assert!(matches!(err, EnvelopeError::HeadersNotObject("array")));
    }

    #[test]
    fn unknown_top_level_envelope_field_rejected() {
        // deny_unknown_fields defends against forward-compatible
        // field smuggling: a future v2 field appearing in a v1
        // envelope would silently bypass the signature contract.
        let wire = br#"{
            "signature": "AAAA",
            "envelope": {
                "envelope_version": "v1",
                "target_mesh": "example.com",
                "target_service": "maild",
                "target_endpoint": "/v1/mesh-call",
                "caller_mesh": "channie.net",
                "caller_selector": "2026q3",
                "caller_node_id": "a1b2",
                "command": "maild.props.list",
                "headers": {"namespace": "accounts"},
                "body": null,
                "ts": "2026-05-20T03:14:00Z",
                "nonce": "AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA=",
                "future_field": "smuggled"
            }
        }"#;
        let err = SignedEnvelope::parse(wire).unwrap_err();
        assert!(matches!(err, EnvelopeError::Json(_)));
    }

    #[test]
    fn unicode_strings_emit_verbatim_utf8() {
        let mut e = sample_envelope();
        e.caller_node_id = "ノード".into();
        let bytes = e.canonical_bytes();
        // Expect the raw UTF-8 bytes for the three katakana characters,
        // not `\uXXXX` escapes.
        let s = std::str::from_utf8(&bytes).unwrap();
        assert!(
            s.contains("ノード"),
            "canonical bytes should contain verbatim UTF-8"
        );
        assert!(
            !s.contains("\\u30ce"),
            "non-control codepoints must not be \\u-escaped"
        );
    }

    #[test]
    fn duplicate_object_keys_in_headers_rejected() {
        // serde_json's default `Value` parse silently keeps the last
        // value for duplicate keys, opening a wire/canonical-bytes
        // mismatch path. The strict deserializer must reject these.
        let wire = br#"{
            "signature": "AAAA",
            "envelope": {
                "envelope_version": "v1",
                "target_mesh": "example.com",
                "target_service": "maild",
                "target_endpoint": "/v1/mesh-call",
                "caller_mesh": "channie.net",
                "caller_selector": "2026q3",
                "caller_node_id": "a1b2",
                "command": "maild.props.list",
                "headers": {"namespace": "admin", "namespace": "accounts"},
                "body": null,
                "ts": "2026-05-20T03:14:00Z",
                "nonce": "AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA="
            }
        }"#;
        let err = SignedEnvelope::parse(wire).unwrap_err();
        match err {
            EnvelopeError::Json(e) => {
                assert!(
                    e.to_string().contains("duplicate JSON object key"),
                    "expected duplicate-key error, got: {e}"
                );
            }
            other => panic!("expected EnvelopeError::Json, got: {other:?}"),
        }
    }

    #[test]
    fn duplicate_object_keys_nested_in_body_rejected() {
        let wire = br#"{
            "signature": "AAAA",
            "envelope": {
                "envelope_version": "v1",
                "target_mesh": "example.com",
                "target_service": "maild",
                "target_endpoint": "/v1/mesh-call",
                "caller_mesh": "channie.net",
                "caller_selector": "2026q3",
                "caller_node_id": "a1b2",
                "command": "maild.props.set",
                "headers": {"namespace": "accounts"},
                "body": {"outer": {"k": 1, "k": 2}},
                "ts": "2026-05-20T03:14:00Z",
                "nonce": "AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA="
            }
        }"#;
        let err = SignedEnvelope::parse(wire).unwrap_err();
        match err {
            EnvelopeError::Json(e) => {
                assert!(
                    e.to_string().contains("duplicate JSON object key"),
                    "expected duplicate-key error, got: {e}"
                );
            }
            other => panic!("expected EnvelopeError::Json, got: {other:?}"),
        }
    }

    #[test]
    fn duplicate_object_keys_inside_array_rejected() {
        let wire = br#"{
            "signature": "AAAA",
            "envelope": {
                "envelope_version": "v1",
                "target_mesh": "example.com",
                "target_service": "maild",
                "target_endpoint": "/v1/mesh-call",
                "caller_mesh": "channie.net",
                "caller_selector": "2026q3",
                "caller_node_id": "a1b2",
                "command": "maild.props.list",
                "headers": {"namespace": "accounts"},
                "body": [{"k": 1, "k": 2}],
                "ts": "2026-05-20T03:14:00Z",
                "nonce": "AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA="
            }
        }"#;
        let err = SignedEnvelope::parse(wire).unwrap_err();
        assert!(matches!(err, EnvelopeError::Json(_)));
    }

    #[test]
    fn integers_within_lossless_range_round_trip() {
        // Document the v1 number contract: i64/u64 within range
        // round-trip cleanly through serde_json without arbitrary
        // precision.
        let mut e = sample_envelope();
        e.body = json!({
            "i64_min": i64::MIN,
            "i64_max": i64::MAX,
            "u64_max": u64::MAX,
        });
        let canonical_a = e.canonical_bytes();
        let wire = serde_json::to_vec(&sample_signed(e)).unwrap();
        let parsed = SignedEnvelope::parse(&wire).unwrap();
        assert_eq!(parsed.envelope.canonical_bytes(), canonical_a);
    }

    #[test]
    fn control_characters_are_escaped() {
        let mut e = sample_envelope();
        // bell (0x07) is a non-named control character → 
        e.caller_node_id = "a\u{0007}b".into();
        let bytes = e.canonical_bytes();
        let s = std::str::from_utf8(&bytes).unwrap();
        assert!(s.contains("a\\u0007b"));
    }
}
