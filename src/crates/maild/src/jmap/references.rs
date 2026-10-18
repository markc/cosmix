//! JMAP ResultReferences (RFC 8620 §3.7).
//!
//! A method argument named with a leading `#` is a back-reference to an earlier
//! method call's result in the *same* request, of the form
//! `{ "resultOf": <callId>, "name": <method>, "path": <JSON Pointer> }`. The
//! dispatcher resolves these before invoking the handler: it evaluates the
//! pointer (RFC 6901, extended with the RFC 8620 `*` array-map token) against
//! the referenced response and substitutes the result under the de-`#`-ed
//! argument name.
//!
//! This is what lets a client chain `Email/query` → `Email/get` (or
//! `Contact/query` → `Contact/get`) in one request — fetching only the page of
//! ids the query returned, rather than pulling everything. Before this, a `#ids`
//! reference was silently ignored and `Email/get` fell back to "return all".
//!
//! Any failure — a missing or errored referenced call, a method-name mismatch,
//! both `#x` and `x` present, or a pointer that does not resolve — is the
//! method-level error `invalidResultReference` (RFC 8620 §3.7).

use super::types::{JmapMethodError, MethodResponse};
use serde_json::Value;

fn invalid(msg: impl Into<String>) -> JmapMethodError {
    JmapMethodError {
        error_type: "invalidResultReference".into(),
        description: Some(msg.into()),
    }
}

/// Resolve every `#`-prefixed ResultReference in `args` in place, against the
/// responses already produced earlier in this request. A no-op when `args` is
/// not an object or carries no `#` keys.
pub fn resolve_result_references(
    args: &mut Value,
    prior: &[MethodResponse],
) -> Result<(), JmapMethodError> {
    let ref_keys: Vec<String> = match args.as_object() {
        Some(obj) => obj.keys().filter(|k| k.starts_with('#')).cloned().collect(),
        None => return Ok(()),
    };

    for hash_key in ref_keys {
        let name = hash_key[1..].to_string();
        // Snapshot what we need under an immutable borrow, then drop it.
        let (reference, conflict) = {
            let obj = args.as_object().expect("args is an object");
            (obj.get(&hash_key).cloned(), obj.contains_key(&name))
        };
        if conflict {
            return Err(invalid(format!(
                "argument `{name}` given both literally and as a ResultReference"
            )));
        }
        let reference = reference.expect("key came from this object");
        let result_of = reference.get("resultOf").and_then(Value::as_str);
        let ref_name = reference.get("name").and_then(Value::as_str);
        let path = reference.get("path").and_then(Value::as_str);
        let (Some(result_of), Some(ref_name), Some(path)) = (result_of, ref_name, path) else {
            return Err(invalid(
                "ResultReference must have string resultOf, name and path",
            ));
        };

        // The referenced call must have produced a *success* response of the
        // named method. An errored call's response triple has method "error",
        // so it never matches `ref_name` — which is the correct outcome.
        let referenced = prior
            .iter()
            .find(|r| r.2 == result_of && r.0 == ref_name)
            .ok_or_else(|| {
                invalid(format!(
                    "no result from call `{result_of}` of method `{ref_name}`"
                ))
            })?;

        let resolved = eval_pointer(&referenced.1, path)
            .ok_or_else(|| invalid(format!("path `{path}` did not resolve")))?;

        let obj = args.as_object_mut().expect("args is an object");
        obj.remove(&hash_key);
        obj.insert(name, resolved);
    }
    Ok(())
}

/// Evaluate a JSON Pointer (RFC 6901) extended with the RFC 8620 §3.7 `*`
/// array-map token. `None` if any step fails. A `*` requires an array and maps
/// the remainder of the pointer over each item, flattening one level when an
/// item itself yields an array.
pub fn eval_pointer(value: &Value, pointer: &str) -> Option<Value> {
    if pointer.is_empty() {
        return Some(value.clone());
    }
    let tokens: Vec<String> = pointer
        .strip_prefix('/')?
        .split('/')
        .map(unescape_token)
        .collect();
    // `eval_tokens` recurses once per token, and the path string is
    // client-controlled, so cap the depth to bound the stack. Real JMAP
    // pointers are a handful of segments (`/list/*/id`); 64 is far above any
    // legitimate path, and an over-long one resolves to `invalidResultReference`.
    if tokens.len() > MAX_POINTER_TOKENS {
        return None;
    }
    eval_tokens(value, &tokens)
}

const MAX_POINTER_TOKENS: usize = 64;

fn unescape_token(tok: &str) -> String {
    // RFC 6901: `~1` → `/`, `~0` → `~` (in that order).
    tok.replace("~1", "/").replace("~0", "~")
}

fn eval_tokens(value: &Value, tokens: &[String]) -> Option<Value> {
    let Some((head, rest)) = tokens.split_first() else {
        return Some(value.clone());
    };
    if head == "*" {
        let arr = value.as_array()?;
        let mut out = Vec::with_capacity(arr.len());
        for item in arr {
            match eval_tokens(item, rest)? {
                Value::Array(inner) => out.extend(inner), // flatten one level
                other => out.push(other),
            }
        }
        Some(Value::Array(out))
    } else {
        let next = match value {
            Value::Object(map) => map.get(head.as_str())?,
            Value::Array(arr) => arr.get(head.parse::<usize>().ok()?)?,
            _ => return None,
        };
        eval_tokens(next, rest)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn resp(method: &str, value: Value, call_id: &str) -> MethodResponse {
        MethodResponse(method.into(), value, call_id.into())
    }

    #[test]
    fn resolves_plain_ids_path() {
        let prior = vec![resp("Email/query", json!({"ids": ["a", "b"]}), "0")];
        let mut args = json!({"#ids": {"resultOf": "0", "name": "Email/query", "path": "/ids"}});
        resolve_result_references(&mut args, &prior).unwrap();
        assert_eq!(args, json!({"ids": ["a", "b"]}));
    }

    #[test]
    fn resolves_star_map_over_objects() {
        let v = json!({"list": [{"id": "x"}, {"id": "y"}]});
        assert_eq!(eval_pointer(&v, "/list/*/id"), Some(json!(["x", "y"])));
    }

    #[test]
    fn star_flattens_one_level() {
        let v = json!({"list": [{"ids": ["a", "b"]}, {"ids": ["c"]}]});
        assert_eq!(
            eval_pointer(&v, "/list/*/ids"),
            Some(json!(["a", "b", "c"]))
        );
    }

    #[test]
    fn missing_reference_is_invalid() {
        let prior = vec![resp("Email/query", json!({"ids": []}), "0")];
        let mut args = json!({"#ids": {"resultOf": "9", "name": "Email/query", "path": "/ids"}});
        let e = resolve_result_references(&mut args, &prior).unwrap_err();
        assert_eq!(e.error_type, "invalidResultReference");
    }

    #[test]
    fn method_name_mismatch_is_invalid() {
        let prior = vec![resp("Email/query", json!({"ids": []}), "0")];
        let mut args = json!({"#ids": {"resultOf": "0", "name": "Mailbox/query", "path": "/ids"}});
        assert!(resolve_result_references(&mut args, &prior).is_err());
    }

    #[test]
    fn errored_call_does_not_match() {
        // An errored prior call has method "error", so a reference naming the
        // original method must not resolve against it.
        let prior = vec![resp("error", json!({"type": "serverFail"}), "0")];
        let mut args = json!({"#ids": {"resultOf": "0", "name": "Email/query", "path": "/ids"}});
        assert!(resolve_result_references(&mut args, &prior).is_err());
    }

    #[test]
    fn both_literal_and_reference_is_invalid() {
        let prior = vec![resp("Email/query", json!({"ids": ["a"]}), "0")];
        let mut args = json!({
            "ids": ["literal"],
            "#ids": {"resultOf": "0", "name": "Email/query", "path": "/ids"}
        });
        assert!(resolve_result_references(&mut args, &prior).is_err());
    }

    #[test]
    fn no_hash_keys_is_noop() {
        let prior = vec![];
        let mut args = json!({"accountId": "2", "ids": ["a"]});
        let before = args.clone();
        resolve_result_references(&mut args, &prior).unwrap();
        assert_eq!(args, before);
    }

    #[test]
    fn over_deep_pointer_is_rejected() {
        // A path with more segments than MAX_POINTER_TOKENS resolves to None
        // (→ invalidResultReference) rather than recursing arbitrarily deep.
        let v = json!({"a": 1});
        let deep = "/a".repeat(MAX_POINTER_TOKENS + 1);
        assert_eq!(eval_pointer(&v, &deep), None);
    }

    #[test]
    fn bad_pointer_is_invalid() {
        let prior = vec![resp("Email/query", json!({"ids": ["a"]}), "0")];
        let mut args = json!({"#ids": {"resultOf": "0", "name": "Email/query", "path": "/nope"}});
        assert!(resolve_result_references(&mut args, &prior).is_err());
    }
}
