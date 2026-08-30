//! Property values per SPEC 07 §2.4 + SPEC 12 §4.3 type set.
//!
//! The numeric split (`Int`/`UInt`/`Float`) preserves the i64/u64/f64
//! distinctions SPEC 12 §4.3 declares as separate field types, which
//! the §10 HMAC canonicalisation relies on for round-trip identity —
//! folding everything into `f64` would lose precision on large
//! integers (timestamps, IDs) and break audit verification.
//!
//! **JSON wire form is signedness-ambiguous for small non-negative
//! integers.** JSON has no syntactic distinction between signed and
//! unsigned integers; `42` is just `42`. A `PropValue::UInt(42)`
//! serialises to the same JSON token as `PropValue::Int(42)` and
//! deserialises back as `Int(42)` (Int is tried first by serde's
//! untagged enum). Round-trip identity is therefore preserved only
//! for values that JSON itself distinguishes: negative ints (Int),
//! ints above `i64::MAX` (UInt), and fractional/exponential numbers
//! (Float).
//!
//! This is acceptable because SPEC 12 §10 HMAC canonicalisation
//! does **not** go through JSON — `canonical_serialise(record)` is
//! defined per backend (SQLite emits typed column tuples; Toml /
//! MixData emit fields in schema order). The schema-typed boundary
//! is the source of truth for i64/u64 signedness; callers that need
//! to preserve UInt-ness across JSON re-hydration consult the
//! namespace schema and normalise accordingly.

use serde::{Deserialize, Serialize};
use serde_json::Value as Json;
use std::collections::BTreeMap;

/// A property value. Object children are ordered for deterministic
/// serialization (insertion order would require indexmap; sorted is
/// simpler and stable across daemon restarts).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum PropValue {
    Null,
    Bool(bool),
    /// Signed 64-bit integer. Maps to SPEC 12 §4.3 `i64`.
    Int(i64),
    /// Unsigned 64-bit integer. Maps to SPEC 12 §4.3 `u64`. Carried
    /// separately from `Int` so the canonical serialisation can
    /// emit values above `i64::MAX` without overflow.
    UInt(u64),
    /// 64-bit float. Maps to SPEC 12 §4.3 `f64`.
    Float(f64),
    String(String),
    List(Vec<PropValue>),
    Object(BTreeMap<String, PropValue>),
}

impl PropValue {
    pub fn type_name(&self) -> &'static str {
        match self {
            Self::Null => "null",
            Self::Bool(_) => "bool",
            Self::Int(_) => "i64",
            Self::UInt(_) => "u64",
            Self::Float(_) => "f64",
            Self::String(_) => "string",
            Self::List(_) => "list",
            Self::Object(_) => "object",
        }
    }

    pub fn is_object(&self) -> bool {
        matches!(self, Self::Object(_))
    }

    pub fn as_object(&self) -> Option<&BTreeMap<String, PropValue>> {
        match self {
            Self::Object(m) => Some(m),
            _ => None,
        }
    }
}

impl From<bool> for PropValue {
    fn from(b: bool) -> Self {
        Self::Bool(b)
    }
}
impl From<i64> for PropValue {
    fn from(n: i64) -> Self {
        Self::Int(n)
    }
}
impl From<u64> for PropValue {
    fn from(n: u64) -> Self {
        Self::UInt(n)
    }
}
impl From<f64> for PropValue {
    fn from(n: f64) -> Self {
        Self::Float(n)
    }
}
impl From<String> for PropValue {
    fn from(s: String) -> Self {
        Self::String(s)
    }
}
impl From<&str> for PropValue {
    fn from(s: &str) -> Self {
        Self::String(s.to_string())
    }
}
impl<T: Into<PropValue>> From<Vec<T>> for PropValue {
    fn from(v: Vec<T>) -> Self {
        Self::List(v.into_iter().map(Into::into).collect())
    }
}

/// Convert to `serde_json::Value` for Bus body serialization.
impl From<&PropValue> for Json {
    fn from(v: &PropValue) -> Self {
        match v {
            PropValue::Null => Json::Null,
            PropValue::Bool(b) => Json::Bool(*b),
            PropValue::Int(n) => Json::Number((*n).into()),
            PropValue::UInt(n) => Json::Number((*n).into()),
            PropValue::Float(n) => serde_json::Number::from_f64(*n)
                .map(Json::Number)
                .unwrap_or(Json::Null),
            PropValue::String(s) => Json::String(s.clone()),
            PropValue::List(l) => Json::Array(l.iter().map(Into::into).collect()),
            PropValue::Object(m) => {
                let mut o = serde_json::Map::with_capacity(m.len());
                for (k, v) in m {
                    o.insert(k.clone(), v.into());
                }
                Json::Object(o)
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn type_names() {
        assert_eq!(PropValue::Null.type_name(), "null");
        assert_eq!(PropValue::Bool(true).type_name(), "bool");
        assert_eq!(PropValue::from(42_i64).type_name(), "i64");
        assert_eq!(PropValue::from(42_u64).type_name(), "u64");
        assert_eq!(PropValue::from(1.5_f64).type_name(), "f64");
        assert_eq!(PropValue::from("x").type_name(), "string");
        assert_eq!(PropValue::List(vec![]).type_name(), "list");
        assert_eq!(PropValue::Object(BTreeMap::new()).type_name(), "object");
    }

    #[test]
    fn json_round_trip() {
        let mut o = BTreeMap::new();
        o.insert("a".into(), PropValue::from(1_i64));
        o.insert("b".into(), PropValue::from("two"));
        let pv = PropValue::Object(o);
        let j: Json = (&pv).into();
        assert_eq!(j["a"], 1);
        assert_eq!(j["b"], "two");
    }

    #[test]
    fn large_integers_preserved_exactly() {
        // SPEC 12 §4.3 + §10 — i64/u64 must round-trip without
        // precision loss for HMAC canonicalisation.
        let big_i: i64 = i64::MAX;
        let big_u: u64 = u64::MAX;
        let pv_i = PropValue::from(big_i);
        let pv_u = PropValue::from(big_u);
        let j_i: Json = (&pv_i).into();
        let j_u: Json = (&pv_u).into();
        assert_eq!(j_i.as_i64(), Some(big_i));
        assert_eq!(j_u.as_u64(), Some(big_u));
    }

    #[test]
    fn json_signedness_collapse_pinned() {
        // JSON has no syntactic int/uint distinction. PropValue::UInt(42)
        // serialises identically to PropValue::Int(42) and re-parses as
        // Int(42). This is intrinsic to JSON; SPEC 12 §10
        // canonicalisation is backend-typed, not JSON, so this does not
        // affect HMAC verification. Pinning the behaviour here so any
        // future change is intentional.
        let s = serde_json::to_string(&PropValue::UInt(42)).unwrap();
        assert_eq!(s, "42");
        let back: PropValue = serde_json::from_str(&s).unwrap();
        assert_eq!(back, PropValue::Int(42));
        // Above i64::MAX is unambiguous and DOES round-trip.
        let big = (i64::MAX as u64) + 1;
        let s = serde_json::to_string(&PropValue::UInt(big)).unwrap();
        let back: PropValue = serde_json::from_str(&s).unwrap();
        assert_eq!(back, PropValue::UInt(big));
    }
}
