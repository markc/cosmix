//! JSON ↔ Mix Value conversion.
//!
//! Enabled by the `json` feature flag. Provides bidirectional conversion
//! between `serde_json::Value` and Mix `Value` types.

use indexmap::IndexMap;
use serde_json;

use crate::value::Value;

/// Convert a JSON value to a Mix value.
pub fn json_to_mix(v: serde_json::Value) -> Value {
    match v {
        serde_json::Value::Null => Value::Nil,
        serde_json::Value::Bool(b) => Value::Bool(b),
        serde_json::Value::Number(n) => Value::Number(n.as_f64().unwrap_or(0.0)),
        serde_json::Value::String(s) => Value::String(s),
        serde_json::Value::Array(arr) => Value::list(arr.into_iter().map(json_to_mix).collect()),
        serde_json::Value::Object(obj) => {
            let mut map = IndexMap::new();
            for (k, v) in obj {
                map.insert(k, json_to_mix(v));
            }
            Value::map(map)
        }
    }
}

/// Convert a Mix value to a JSON value.
///
/// Fallible: a non-finite number (NaN/±inf) has no JSON representation and
/// returns an error naming the offender — the old code silently coerced it
/// to `0`, the exact opposite of the strict-data/jq policy ("no faithful
/// representation → reject, never invent a value"). Functions and Bytes
/// still map to Null (a lambda or an `http_*` bytes payload inside a map
/// shouldn't abort the whole encode; bytes callers `base64_encode` first).
pub fn mix_to_json(v: &Value) -> Result<serde_json::Value, String> {
    Ok(match v {
        Value::Nil => serde_json::Value::Null,
        Value::Bool(b) => serde_json::Value::Bool(*b),
        Value::Number(n) => {
            if !n.is_finite() {
                return Err(format!(
                    "non-finite number {n} has no JSON representation (JSON has no NaN/Infinity)"
                ));
            }
            // Preserve integer representation for whole numbers. Range check:
            // `i64::MAX as f64` rounds UP to exactly 2^63 (one past i64::MAX),
            // so the upper bound must be EXCLUSIVE — an f64 of 2^63 would
            // otherwise saturate to i64::MAX (silent off-by-one) instead of
            // taking the real path. The lower bound -2^63 is exactly
            // representable, so >= is correct there.
            if *n == n.floor() && *n >= i64::MIN as f64 && *n < i64::MAX as f64 {
                serde_json::Value::Number(serde_json::Number::from(*n as i64))
            } else {
                serde_json::Value::Number(
                    // Finiteness checked above, so from_f64 cannot fail.
                    serde_json::Number::from_f64(*n)
                        .expect("finite f64 is always a valid JSON number"),
                )
            }
        }
        Value::String(s) => serde_json::Value::String(s.clone()),
        Value::List(items) => {
            serde_json::Value::Array(items.iter().map(mix_to_json).collect::<Result<_, _>>()?)
        }
        Value::Map(map) => {
            let obj: serde_json::Map<String, serde_json::Value> = map
                .iter()
                .map(|(k, v)| Ok((k.clone(), mix_to_json(v)?)))
                .collect::<Result<_, String>>()?;
            serde_json::Value::Object(obj)
        }
        // Functions are not serializable. Emit Null so `json_encode`
        // on a map containing a lambda produces valid JSON rather
        // than panicking.
        Value::Function(_) => serde_json::Value::Null,
        // Bytes have no native JSON type — emit Null so an unintended
        // `json_encode(http_response)` (which now carries a `bytes`
        // key) doesn't panic, but callers who want the payload across
        // a JSON boundary must `base64_encode($bytes)` explicitly.
        Value::Bytes(_) => serde_json::Value::Null,
        // Same policy as Bytes — a mutable byte buffer has no native JSON
        // type; `base64_encode(freeze($buf))` crosses a JSON boundary.
        Value::Buffer(_) => serde_json::Value::Null,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_json_to_mix_primitives() {
        assert_eq!(json_to_mix(serde_json::json!(null)), Value::Nil);
        assert_eq!(json_to_mix(serde_json::json!(true)), Value::Bool(true));
        assert_eq!(json_to_mix(serde_json::json!(42)), Value::Number(42.0));
        assert_eq!(
            json_to_mix(serde_json::json!("hello")),
            Value::String("hello".to_string())
        );
    }

    #[test]
    fn test_json_to_mix_array() {
        let v = json_to_mix(serde_json::json!([1, "two", true]));
        match &v {
            Value::List(items) => {
                assert_eq!(items.len(), 3);
                assert_eq!(items[0], Value::Number(1.0));
                assert_eq!(items[1], Value::String("two".to_string()));
                assert_eq!(items[2], Value::Bool(true));
            }
            _ => panic!("expected list"),
        }
    }

    #[test]
    fn test_json_to_mix_object() {
        let v = json_to_mix(serde_json::json!({"name": "Mark", "age": 42}));
        match &v {
            Value::Map(map) => {
                assert_eq!(map.get("name"), Some(&Value::String("Mark".to_string())));
                assert_eq!(map.get("age"), Some(&Value::Number(42.0)));
            }
            _ => panic!("expected map"),
        }
    }

    #[test]
    fn test_roundtrip() {
        let original = serde_json::json!({
            "name": "test",
            "count": 42,
            "active": true,
            "tags": ["a", "b"],
            "meta": null
        });
        let mix_val = json_to_mix(original.clone());
        let back = mix_to_json(&mix_val).unwrap();
        assert_eq!(original, back);
    }
}
