//! v1 canonical-bytes serialisation — the single authoritative
//! `serde_json::Value → bytes` canonicaliser shared by every signed
//! artifact in this crate (the cross-mesh [`crate::envelope`] and the
//! SPEC 13 [`crate::inventory`]).
//!
//! # Why one shared implementation
//!
//! The cardinal risk for any detached-signature scheme is that the
//! *signer* and the *verifier* canonicalise differently and disagree
//! on the bytes a signature covers. SPEC 13 §6 names this the central
//! risk and resolves it by mandating **one authoritative
//! canonicaliser** that both sides call. This module is that
//! canonicaliser; the signer (SPEC 13 1b-b) and `noded`'s verifier
//! (1b-c) both reach the same bytes by both going through
//! [`to_canonical_bytes`].
//!
//! # v1 canonical contract
//!
//! The rules below match the cross-mesh envelope's long-standing v1
//! contract (originally documented on [`crate::envelope`]); they are
//! "RFC-8785-style" JCS and are **JCS-equivalent over the value space
//! these artifacts actually use** (ASCII object keys, base64 / IP /
//! name strings, integer epochs, bools, null, nested objects/arrays):
//!
//! - **Deterministic key order.** Object keys (recursively) are emitted
//!   in lexicographic order over their Rust `str` comparison — i.e. by
//!   Unicode scalar value. For the all-ASCII keys these artifacts use
//!   this is identical to RFC 8785's UTF-16-code-unit ordering; the two
//!   orderings can only differ on non-BMP (supplementary-plane) keys,
//!   which never appear here.
//! - **No whitespace.** Zero bytes of whitespace between tokens.
//! - **String escaping.** Only the characters JSON requires are escaped
//!   (`"`, `\`, `\n`, `\r`, `\t`, `\b`, `\f`, and C0 controls via
//!   `\u00XX`). Every other Unicode codepoint is emitted verbatim as
//!   UTF-8. No `\uXXXX` for non-control BMP characters, no surrogate
//!   pairs.
//! - **No Unicode normalisation.** Codepoints are preserved verbatim;
//!   the verifier re-canonicalises from parsed fields, so producer and
//!   verifier share the same codepoint sequence by construction.
//! - **Numbers.** Emitted via [`serde_json::Number`]'s default
//!   representation: integers in `[i64::MIN, u64::MAX]` round-trip
//!   losslessly (`1`, not `1.0`); floats round-trip as f64. The
//!   `arbitrary_precision` serde_json feature is intentionally NOT
//!   enabled — it would give the same value different lexical forms and
//!   be its own cross-producer divergence trap. NaN/Infinity cannot
//!   appear (serde_json rejects them at parse time, and
//!   [`deserialize_strict_value`] forbids non-finite floats too).
//! - **Duplicate object keys.** Rejected at parse time, recursively, by
//!   [`deserialize_strict_value`] — serde_json's default `Value` parse
//!   silently keeps the last value for a duplicate key, which would let
//!   a verifier's canonical bytes match the signature while a separate
//!   downstream consumer (audit log, jq, a different JSON lib) sees an
//!   earlier value for the same key.
//!
//! # Verifier contract
//!
//! *"Verifier MUST re-canonicalise from parsed fields, never verify
//! against bytes-as-received."* [`to_canonical_bytes`] is the only
//! signature input — call sites must not feed raw wire bytes to a
//! verify routine.

use serde_json::Value;
use std::io::Write as _;

/// Produce v1 canonical bytes for an arbitrary [`Value`] — the exact
/// byte sequence a signature is computed over. See the module-level
/// documentation for the v1 contract.
pub fn to_canonical_bytes(value: &Value) -> Vec<u8> {
    let mut out = Vec::with_capacity(256);
    emit_value(value, &mut out);
    out
}

/// Recursively emit a [`Value`] using the v1 canonical rules.
pub(crate) fn emit_value(v: &Value, out: &mut Vec<u8>) {
    match v {
        Value::Null => out.extend_from_slice(b"null"),
        Value::Bool(true) => out.extend_from_slice(b"true"),
        Value::Bool(false) => out.extend_from_slice(b"false"),
        Value::Number(n) => out.extend_from_slice(n.to_string().as_bytes()),
        Value::String(s) => emit_json_string(s, out),
        Value::Array(arr) => {
            out.push(b'[');
            let mut first = true;
            for item in arr {
                if !first {
                    out.push(b',');
                }
                first = false;
                emit_value(item, out);
            }
            out.push(b']');
        }
        Value::Object(obj) => {
            // serde_json::Map's backing is either preserve-order or
            // BTreeMap depending on cargo features. Sort explicitly so
            // canonicalisation never depends on how the input JSON was
            // parsed.
            let mut keys: Vec<&String> = obj.keys().collect();
            keys.sort();
            out.push(b'{');
            let mut first = true;
            for k in keys {
                if !first {
                    out.push(b',');
                }
                first = false;
                emit_json_string(k, out);
                out.push(b':');
                emit_value(&obj[k], out);
            }
            out.push(b'}');
        }
    }
}

/// Emit a JSON string using v1 canonical rules: minimal escaping,
/// verbatim UTF-8 for all non-control codepoints.
pub(crate) fn emit_json_string(s: &str, out: &mut Vec<u8>) {
    out.push(b'"');
    for c in s.chars() {
        match c {
            '"' => out.extend_from_slice(b"\\\""),
            '\\' => out.extend_from_slice(b"\\\\"),
            '\n' => out.extend_from_slice(b"\\n"),
            '\r' => out.extend_from_slice(b"\\r"),
            '\t' => out.extend_from_slice(b"\\t"),
            '\u{0008}' => out.extend_from_slice(b"\\b"),
            '\u{000C}' => out.extend_from_slice(b"\\f"),
            c if (c as u32) < 0x20 => {
                let _ = write!(out, "\\u{:04x}", c as u32);
            }
            c => {
                let mut buf = [0u8; 4];
                out.extend_from_slice(c.encode_utf8(&mut buf).as_bytes());
            }
        }
    }
    out.push(b'"');
}

/// Strict `Value` deserializer — rejects duplicate object keys
/// recursively. Used via `#[serde(deserialize_with = ...)]` on any
/// opaque-`Value` field of a signed artifact to defend the canonical
/// bytes contract against wire JSON that would otherwise collapse under
/// serde_json's default last-write-wins duplicate-key semantics. Also
/// rejects non-finite floats (NaN/Infinity), which have no canonical
/// representation.
pub fn deserialize_strict_value<'de, D>(d: D) -> Result<Value, D::Error>
where
    D: serde::Deserializer<'de>,
{
    use serde::de::{Error as _, MapAccess, SeqAccess, Visitor};
    use std::fmt;

    struct StrictVisitor;

    impl<'de> Visitor<'de> for StrictVisitor {
        type Value = Value;

        fn expecting(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
            f.write_str("any JSON value with no duplicate object keys")
        }

        fn visit_unit<E>(self) -> Result<Value, E> {
            Ok(Value::Null)
        }
        fn visit_bool<E>(self, v: bool) -> Result<Value, E> {
            Ok(Value::Bool(v))
        }
        fn visit_i64<E>(self, v: i64) -> Result<Value, E> {
            Ok(Value::Number(v.into()))
        }
        fn visit_u64<E>(self, v: u64) -> Result<Value, E> {
            Ok(Value::Number(v.into()))
        }
        fn visit_f64<E: serde::de::Error>(self, v: f64) -> Result<Value, E> {
            serde_json::Number::from_f64(v)
                .map(Value::Number)
                .ok_or_else(|| E::custom("non-finite float (NaN/Infinity) is not permitted"))
        }
        fn visit_str<E: serde::de::Error>(self, v: &str) -> Result<Value, E> {
            Ok(Value::String(v.to_owned()))
        }
        fn visit_string<E>(self, v: String) -> Result<Value, E> {
            Ok(Value::String(v))
        }
        fn visit_none<E>(self) -> Result<Value, E> {
            Ok(Value::Null)
        }
        fn visit_some<D2: serde::Deserializer<'de>>(self, d: D2) -> Result<Value, D2::Error> {
            deserialize_strict_value(d)
        }
        fn visit_seq<A: SeqAccess<'de>>(self, mut seq: A) -> Result<Value, A::Error> {
            // Read each element through this same strict path so nested
            // objects also reject duplicate keys.
            struct StrictWrapper(Value);
            impl<'de> serde::Deserialize<'de> for StrictWrapper {
                fn deserialize<D2: serde::Deserializer<'de>>(d: D2) -> Result<Self, D2::Error> {
                    deserialize_strict_value(d).map(StrictWrapper)
                }
            }
            let mut arr = Vec::new();
            while let Some(StrictWrapper(item)) = seq.next_element::<StrictWrapper>()? {
                arr.push(item);
            }
            Ok(Value::Array(arr))
        }
        fn visit_map<A: MapAccess<'de>>(self, mut map: A) -> Result<Value, A::Error> {
            struct StrictWrapper(Value);
            impl<'de> serde::Deserialize<'de> for StrictWrapper {
                fn deserialize<D2: serde::Deserializer<'de>>(d: D2) -> Result<Self, D2::Error> {
                    deserialize_strict_value(d).map(StrictWrapper)
                }
            }
            let mut obj = serde_json::Map::new();
            while let Some(k) = map.next_key::<String>()? {
                let StrictWrapper(v) = map.next_value::<StrictWrapper>()?;
                if obj.contains_key(&k) {
                    return Err(A::Error::custom(format!(
                        "duplicate JSON object key: {k:?}"
                    )));
                }
                obj.insert(k, v);
            }
            Ok(Value::Object(obj))
        }
    }

    d.deserialize_any(StrictVisitor)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn empty_object_and_array() {
        assert_eq!(to_canonical_bytes(&json!({})), b"{}");
        assert_eq!(to_canonical_bytes(&json!([])), b"[]");
    }

    #[test]
    fn object_keys_sorted_recursively() {
        let v = json!({ "z": 1, "a": { "y": 2, "b": 3 }, "m": [3, 2, 1] });
        let s = String::from_utf8(to_canonical_bytes(&v)).unwrap();
        assert_eq!(s, r#"{"a":{"b":3,"y":2},"m":[3,2,1],"z":1}"#);
    }

    #[test]
    fn no_whitespace_emitted() {
        let bytes = to_canonical_bytes(&json!({ "a": 1, "b": [2, 3] }));
        for &b in &bytes {
            assert!(
                !(b == b' ' || b == b'\n' || b == b'\r' || b == b'\t'),
                "found whitespace byte 0x{b:02x}"
            );
        }
    }

    #[test]
    fn integers_emit_without_decimal_point() {
        let v = json!({ "epoch": 7u64, "min": i64::MIN, "max": u64::MAX });
        let s = String::from_utf8(to_canonical_bytes(&v)).unwrap();
        assert_eq!(
            s,
            r#"{"epoch":7,"max":18446744073709551615,"min":-9223372036854775808}"#
        );
    }

    #[test]
    fn unicode_strings_emit_verbatim_utf8() {
        let s = String::from_utf8(to_canonical_bytes(&json!({ "k": "ノード" }))).unwrap();
        assert!(s.contains("ノード"));
        assert!(!s.contains("\\u30ce"));
    }

    #[test]
    fn control_characters_are_escaped() {
        // The bell (0x07) is a non-named C0 control → escaped as the
        // 6-char sequence backslash-u-0-0-0-7, not stripped.
        let s = String::from_utf8(to_canonical_bytes(&json!("a\u{0007}b"))).unwrap();
        // Build expected via format! so no \u escape appears in a source
        // literal: format! yields the 6-char sequence backslash-u-0007.
        let expected = format!("\"a\\u{:04x}b\"", 0x07u32);
        assert_eq!(s, expected);
    }

    #[test]
    fn named_escapes() {
        let s = String::from_utf8(to_canonical_bytes(&json!("\"\\\n\r\t\u{08}\u{0c}"))).unwrap();
        assert_eq!(s, r#""\"\\\n\r\t\b\f""#);
    }

    #[test]
    fn strict_deserializer_rejects_duplicate_keys() {
        // Top-level object, and nested inside an array — both recurse
        // through the strict path and must reject the collision.
        for input in [r#"{"k":1,"k":2}"#, r#"[{"k":1,"k":2}]"#] {
            let mut de = serde_json::Deserializer::from_str(input);
            let err = deserialize_strict_value(&mut de).unwrap_err();
            assert!(
                err.to_string().contains("duplicate JSON object key"),
                "input {input:?} should reject duplicate keys, got: {err}"
            );
        }
    }

    #[test]
    fn strict_deserializer_accepts_distinct_keys() {
        let mut de = serde_json::Deserializer::from_str(r#"{"a":1,"b":2}"#);
        let v = deserialize_strict_value(&mut de).unwrap();
        assert_eq!(v, json!({ "a": 1, "b": 2 }));
    }
}
