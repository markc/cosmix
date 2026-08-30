//! Redaction of sensitive values per SPEC 07 §7.2.

use crate::value::PropValue;

/// Redact a sensitive value. Strings → `"***"`, numbers/bools → `null`,
/// lists/objects → recurse (a sensitive subtree gets every leaf redacted).
///
/// SPEC 07 §7.2: full value access requires `reveal: true` AND a request
/// from the WG /24 trust domain — that policy is enforced by the citizen
/// layer / broker, not here. This function is the unconditional redactor;
/// callers decide whether to invoke it.
pub fn redact(v: &PropValue) -> PropValue {
    match v {
        PropValue::String(_) => PropValue::String("***".into()),
        PropValue::Int(_)
        | PropValue::UInt(_)
        | PropValue::Float(_)
        | PropValue::Bool(_)
        | PropValue::Null => PropValue::Null,
        PropValue::List(items) => PropValue::List(items.iter().map(redact).collect()),
        PropValue::Object(m) => {
            PropValue::Object(m.iter().map(|(k, v)| (k.clone(), redact(v))).collect())
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeMap;

    #[test]
    fn redacts_leaves() {
        assert_eq!(
            redact(&PropValue::String("secret".into())),
            PropValue::String("***".into())
        );
        assert_eq!(redact(&PropValue::from(42_i64)), PropValue::Null);
        assert_eq!(redact(&PropValue::Bool(true)), PropValue::Null);
        assert_eq!(redact(&PropValue::Null), PropValue::Null);
    }

    #[test]
    fn redacts_subtree() {
        let mut m = BTreeMap::new();
        m.insert("token".into(), PropValue::String("abc".into()));
        m.insert("count".into(), PropValue::from(5_i64));
        let r = redact(&PropValue::Object(m));
        let o = r.as_object().unwrap();
        assert_eq!(o["token"], PropValue::String("***".into()));
        assert_eq!(o["count"], PropValue::Null);
    }
}
