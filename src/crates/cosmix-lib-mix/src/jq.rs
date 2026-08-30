//! Embedded jq engine — `jq()` / `jq_all()` builtins.
//!
//! Enabled by the `json` feature flag (same gate as `json_parse`;
//! the jaq crates are `optional` deps pulled by that feature).
//!
//! Design of record: `src/_doc/mix/jq-builtin-design.md` (Codex-converged
//! rev 5). The load-bearing decisions, each surviving cold review:
//!
//! * **Direct marshalling, no `serde_json` hop.** `serde_json` is linked
//!   without `preserve_order`, so routing a Mix `Map` (an `IndexMap`)
//!   through `serde_json::Value` would silently reorder object keys.
//!   We convert Mix `Value` ⇄ `jaq_json::Val` directly.
//! * **Exhaustive, lossless output.** `jaq_json::Val` is a JSON
//!   *superset* (byte strings, non-string object keys). Kinds Mix
//!   cannot faithfully hold are hard-errored, never coerced.
//! * **Non-finite numbers raise**, matching the strict-data serializer
//!   precedent (`value.rs` `write_mix_data`), not `json.rs`'s silent
//!   `unwrap_or(0.0)`.
//! * **Mode-driven, in-scope runner.** The jaq output iterator borrows
//!   function-local state (`arena`, the compiled filter, the `Ctx`),
//!   so it can never be returned. `run_jq` drives it inside one stack
//!   frame; `JqMode::Single` early-returns after the second output, so
//!   `jq()` over a huge stream stays memory-bounded.

use indexmap::IndexMap;

use crate::error::{MixError, MixResult};
use crate::value::Value;

use jaq_core::load::{Arena, File, Loader};
use jaq_core::{Compiler, Ctx, Vars, data, unwrap_valr};
use jaq_json::{Map as JaqMap, Num, Rc as JaqRc, Val};

fn err(msg: impl Into<String>) -> MixError {
    MixError::RuntimeError {
        span: None,
        msg: msg.into(),
    }
}

/// Which cardinality contract the caller wants. `Single` backs `jq()`
/// (0 or 1 output expected); `All` backs `jq_all()` (the stream case).
pub enum JqMode {
    Single,
    All,
}

/// Mix `Value` → jaq `Val` (input side). Total except for the two kinds
/// with no jq representation, which raise rather than coerce:
/// non-finite numbers and functions.
fn mix_to_jaq(v: &Value) -> MixResult<Val> {
    Ok(match v {
        Value::Nil => Val::Null,
        Value::Bool(b) => Val::Bool(*b),
        Value::Number(n) => {
            if !n.is_finite() {
                // Same policy as the strict-data serializer
                // (`value.rs` write_mix_data): a non-finite number has
                // no faithful representation, so reject before
                // marshalling — do NOT inherit `json.rs`'s silent
                // `as_f64().unwrap_or(0.0)` → 0 coercion.
                return Err(err(format!(
                    "jq(): non-finite input number {n} has no jq representation"
                )));
            }
            Val::from(*n)
        }
        // Mix `String` is always valid UTF-8 → `TStr` via `From<String>`.
        Value::String(s) => Val::from(s.clone()),
        Value::List(items) => {
            let arr: Vec<Val> = items.iter().map(mix_to_jaq).collect::<MixResult<_>>()?;
            Val::Arr(JaqRc::new(arr))
        }
        Value::Map(m) => {
            // Iterate the IndexMap in insertion order so jaq sees the
            // same key order Mix has; each key is a Mix `String` → a
            // `TStr` key.
            let mut obj = JaqMap::default();
            for (k, val) in m.iter() {
                obj.insert(Val::from(k.clone()), mix_to_jaq(val)?);
            }
            Val::obj(obj)
        }
        Value::Function(_) => {
            return Err(err(
                "jq(): a function value has no jq representation".to_string()
            ));
        }
        Value::Bytes(_) => {
            // Same policy as functions and non-finite numbers — bytes
            // have no faithful jq representation, so reject loudly
            // rather than silent-encoding through `base64_encode` here
            // and losing the call site's intent.
            return Err(err(
                "jq(): a bytes value has no jq representation".to_string()
            ));
        }
        Value::Buffer(_) => {
            // Same policy as Bytes/Function — reject loudly rather than
            // silently encode a mutable byte buffer through base64 here.
            return Err(err(
                "jq(): a buffer value has no jq representation".to_string()
            ));
        }
    })
}

/// `jaq_json::Num` → `f64`. f64 is Mix's only number type; magnitudes
/// outside f64-exact range are documented lossy (a pre-existing Mix
/// constraint, same as `json.rs`). `BigInt`/`Dec` go through their
/// textual form rather than depending on `num-bigint` internals.
fn num_to_f64(n: &Num) -> f64 {
    match n {
        Num::Int(i) => *i as f64,
        Num::Float(f) => *f,
        // BigInt magnitude beyond f64 → infinity (documented loss).
        Num::BigInt(b) => b.to_string().parse::<f64>().unwrap_or(f64::INFINITY),
        Num::Dec(s) => s.parse::<f64>().unwrap_or(f64::NAN),
    }
}

/// jaq `Val` → Mix `Value` (output side). EXHAUSTIVE per real
/// `jaq_json 2.0` variant, no catch-all. Kinds Mix cannot faithfully
/// hold hard-error rather than coerce, and every message names the
/// offending variant.
fn jaq_to_mix(v: Val) -> MixResult<Value> {
    Ok(match v {
        Val::Null => Value::Nil,
        Val::Bool(b) => Value::Bool(b),
        Val::Num(n) => {
            let f = num_to_f64(&n);
            if !f.is_finite() {
                return Err(err(format!(
                    "jq(): result number {n} is non-finite and has no Mix representation"
                )));
            }
            Value::Number(f)
        }
        // `TStr` is *interpreted* as UTF-8 but its bytes are not
        // guaranteed valid; reject invalid UTF-8 rather than lossily
        // replacing it (which would silently corrupt data).
        Val::TStr(b) => match core::str::from_utf8(&b[..]) {
            Ok(s) => Value::String(s.to_string()),
            Err(e) => {
                return Err(err(format!(
                    "jq(): result text string is not valid UTF-8 ({e}); Mix strings are UTF-8 only"
                )));
            }
        },
        Val::BStr(_) => {
            return Err(err(
                "jq(): result is a byte string, which Mix cannot represent (Mix strings are UTF-8 text only)"
                    .to_string(),
            ));
        }
        Val::Arr(a) => {
            let items: Vec<Value> = a
                .iter()
                .cloned()
                .map(jaq_to_mix)
                .collect::<MixResult<_>>()?;
            Value::list(items)
        }
        Val::Obj(o) => {
            let mut map: IndexMap<String, Value> = IndexMap::new();
            for (k, val) in o.iter() {
                let key = match k {
                    Val::TStr(b) => core::str::from_utf8(&b[..]).map_err(|e| {
                        err(format!(
                            "jq(): result object has a non-UTF-8 string key ({e}); Mix map keys are UTF-8 text"
                        ))
                    })?,
                    other => {
                        return Err(err(format!(
                            "jq(): result object has a non-string key ({}); Mix map keys must be strings",
                            other_kind(other)
                        )));
                    }
                };
                map.insert(key.to_string(), jaq_to_mix(val.clone())?);
            }
            Value::map(map)
        }
    })
}

/// Name the jaq value kind for a diagnostic (non-string object keys).
fn other_kind(v: &Val) -> &'static str {
    match v {
        Val::Null => "null",
        Val::Bool(_) => "boolean",
        Val::Num(_) => "number",
        Val::BStr(_) => "byte string",
        Val::TStr(_) => "string",
        Val::Arr(_) => "array",
        Val::Obj(_) => "object",
    }
}

/// The whole pipeline in one stack frame. The jaq output iterator
/// borrows `arena` / `filter` / `ctx` (all locals), so it is consumed
/// here and never escapes — that is what makes the `impl Iterator`
/// shape impossible and this mode-driven shape necessary.
pub fn run_jq(value: &Value, filter: &str, mode: JqMode) -> MixResult<Value> {
    // Input-side marshalling and program load/compile happen before
    // anything runs: failures here are `MixError` (the `json_parse`
    // raise-on-malformed precedent), not in-band values.
    let input = mix_to_jaq(value)?;

    let program = File {
        code: filter,
        path: (),
    };
    let defs = jaq_core::defs()
        .chain(jaq_std::defs())
        .chain(jaq_json::defs());
    let funs = jaq_core::funs()
        .chain(jaq_std::funs())
        .chain(jaq_json::funs());

    let loader = Loader::new(defs);
    let arena = Arena::default();
    let modules = loader.load(&arena, program).map_err(|errs| {
        err(format!(
            "jq(): cannot parse filter {filter:?}: {}",
            fmt_load_errors(&errs)
        ))
    })?;
    let filter_prog = Compiler::default()
        .with_funs(funs)
        .compile(modules)
        .map_err(|errs| {
            err(format!(
                "jq(): cannot compile filter {filter:?}: {}",
                fmt_load_errors(&errs)
            ))
        })?;

    let ctx = Ctx::<data::JustLut<Val>>::new(&filter_prog.lut, Vars::new([]));
    // `ValR<Val>` = `Result<Val, jaq_core::Error<Val>>`; the runtime
    // error type is `Display` (via `Val: Display`).
    let mut outs = filter_prog.id.run((ctx, input)).map(unwrap_valr);

    match mode {
        JqMode::Single => {
            // Pull at most two outputs. A 2nd `Err` is a runtime
            // error (NOT a second output); a 2nd `Ok` is the
            // cardinality error. Either way we `return` immediately —
            // the still-borrowing iterator is dropped undrained, so at
            // most two `Value`s are ever marshalled regardless of how
            // long the stream is.
            match outs.next() {
                None => Ok(Value::Nil),
                Some(Err(e)) => Err(err(format!("jq(): {e}"))),
                Some(Ok(first)) => {
                    // Keep `first` as a raw `Val` and decide cardinality
                    // BEFORE marshalling: if there is a 2nd output the
                    // result is the wrong-tool error regardless of
                    // whether `first` is Mix-representable, and a huge
                    // first output must not be converted just to be
                    // discarded. Only the confirmed-sole output is ever
                    // marshalled (≤1 `Value`, tighter than the design's
                    // ≤2 note).
                    match outs.next() {
                        None => jaq_to_mix(first),
                        Some(Err(e)) => Err(err(format!("jq(): {e}"))),
                        Some(Ok(_)) => Err(err(
                            "jq(): filter produced more than one output; use jq_all(), \
                             or wrap the filter as `[ … ]` / `map(...)` / add `first` \
                             to make it a single value"
                                .to_string(),
                        )),
                    }
                }
            }
        }
        JqMode::All => {
            // Materialising the whole stream is this builtin's
            // contract; short-circuit on the first runtime error.
            let mut items = Vec::new();
            for r in outs {
                match r {
                    Ok(v) => items.push(jaq_to_mix(v)?),
                    Err(e) => return Err(err(format!("jq_all(): {e}"))),
                }
            }
            Ok(Value::list(items))
        }
    }
}

/// jaq load/compile errors are `Vec<(File, load::Error)>`; `load::Error`
/// is `Debug` only (no `Display`), so format the structural form. The
/// filter string itself is already in the surrounding message.
fn fmt_load_errors<S: core::fmt::Debug, P, E: core::fmt::Debug>(
    errs: &[(File<S, P>, E)],
) -> String {
    errs.iter()
        .map(|(_f, e)| format!("{e:?}"))
        .collect::<Vec<_>>()
        .join("; ")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn map(pairs: &[(&str, Value)]) -> Value {
        let mut m = IndexMap::new();
        for (k, v) in pairs {
            m.insert(k.to_string(), v.clone());
        }
        Value::map(m)
    }
    fn num(n: f64) -> Value {
        Value::Number(n)
    }
    fn s(x: &str) -> Value {
        Value::String(x.to_string())
    }

    fn single(v: &Value, f: &str) -> MixResult<Value> {
        run_jq(v, f, JqMode::Single)
    }
    fn all(v: &Value, f: &str) -> MixResult<Value> {
        run_jq(v, f, JqMode::All)
    }

    /// `Value`'s own `PartialEq` (value.rs) deliberately returns
    /// `false` for any `List`/`Map` comparison, so container results
    /// need a structural compare. Scalars defer to that `PartialEq`;
    /// containers recurse and maps compare key order too (the whole
    /// point of the no-serde_json marshalling).
    fn deep_eq(a: &Value, b: &Value) -> bool {
        match (a, b) {
            (Value::List(x), Value::List(y)) => {
                x.len() == y.len() && x.iter().zip(y.iter()).all(|(p, q)| deep_eq(p, q))
            }
            (Value::Map(x), Value::Map(y)) => {
                x.len() == y.len()
                    && x.iter()
                        .zip(y.iter())
                        .all(|((xk, xv), (yk, yv))| xk == yk && deep_eq(xv, yv))
            }
            _ => a == b,
        }
    }
    fn assert_deep(got: Value, want: Value) {
        assert!(deep_eq(&got, &want), "got {got:?}, want {want:?}");
    }

    #[test]
    fn scalar_extraction() {
        let input = map(&[("a", map(&[("b", num(42.0))]))]);
        assert_eq!(single(&input, ".a.b").unwrap(), num(42.0));
    }

    #[test]
    fn zero_output_is_nil() {
        assert_eq!(single(&map(&[]), ".missing").unwrap(), Value::Nil);
    }

    #[test]
    fn more_than_one_output_raises_in_single() {
        let input = Value::list(vec![num(1.0), num(2.0), num(3.0)]);
        let e = single(&input, ".[]").unwrap_err();
        assert!(format!("{e}").contains("more than one output"), "got: {e}");
    }

    #[test]
    fn array_returning_filter_is_one_output() {
        // `map(...)` emits ONE array output → jq() unwraps it to a List.
        let input = Value::list(vec![num(1.0), num(2.0)]);
        assert_deep(
            single(&input, "map(. + 1)").unwrap(),
            Value::list(vec![num(2.0), num(3.0)]),
        );
    }

    #[test]
    fn alternative_operator_default() {
        // jq-native default idiom — no Mix-side `default` arg.
        assert_eq!(single(&map(&[]), r#".a.b // "—""#).unwrap(), s("—"));
    }

    #[test]
    fn jq_all_streams_to_list() {
        let input = Value::list(vec![num(1.0), num(2.0), num(3.0)]);
        assert_deep(
            all(&input, ".[]").unwrap(),
            Value::list(vec![num(1.0), num(2.0), num(3.0)]),
        );
    }

    #[test]
    fn jq_all_empty_stream_is_empty_list() {
        assert_deep(
            all(&Value::list(vec![]), ".[]").unwrap(),
            Value::list(vec![]),
        );
        // Zero outputs (optional iteration over absent key) → [].
        assert_deep(all(&map(&[]), ".x[]?").unwrap(), Value::list(vec![]));
    }

    #[test]
    fn jq_all_select_filter() {
        let input = Value::list(vec![
            map(&[("size", num(50.0))]),
            map(&[("size", num(150.0))]),
            map(&[("size", num(200.0))]),
        ]);
        let out = all(&input, ".[] | select(.size > 100) | .size").unwrap();
        assert_deep(out, Value::list(vec![num(150.0), num(200.0)]));
    }

    #[test]
    fn object_key_order_is_preserved() {
        // serde_json (no preserve_order) would sort/rehash these; the
        // direct marshalling must keep insertion order z, a, m.
        let input = map(&[("z", num(1.0)), ("a", num(2.0)), ("m", num(3.0))]);
        match &single(&input, ".").unwrap() {
            Value::Map(m) => {
                let keys: Vec<&str> = m.keys().map(|k| k.as_str()).collect();
                assert_eq!(keys, vec!["z", "a", "m"]);
            }
            other => panic!("expected map, got {other:?}"),
        }
    }

    #[test]
    fn non_finite_input_raises() {
        let e = single(&num(f64::INFINITY), ".").unwrap_err();
        assert!(format!("{e}").contains("non-finite"), "got: {e}");
        assert!(single(&num(f64::NAN), ".").is_err());
    }

    #[test]
    fn malformed_filter_raises() {
        let e = single(&map(&[]), ".a.[").unwrap_err();
        assert!(format!("{e}").contains("parse"), "got: {e}");
    }

    #[test]
    fn runtime_type_error_propagates() {
        // `.a + 1` where `.a` is a string is a jq runtime type error,
        // not absent data → raise, never nil.
        let input = map(&[("a", s("hello"))]);
        assert!(single(&input, ".a + 1").is_err());
    }

    #[test]
    fn jq_all_runtime_error_short_circuits() {
        let input = Value::list(vec![num(1.0), s("two"), num(3.0)]);
        // The string element makes `. + 1` fail mid-stream.
        assert!(all(&input, ".[] | . + 1").is_err());
    }

    #[test]
    fn round_trips_large_integral_floats_outside_i64_range() {
        // Mirrors the strict-data test of the same name: f64 is Mix's
        // only number type; 1e20 has no exact i64 form but survives a
        // jq identity round-trip as the same f64.
        let big = 1e20_f64;
        assert_eq!(single(&num(big), ".").unwrap(), num(big));
    }

    #[test]
    fn nested_structure_round_trips() {
        let input = map(&[
            ("name", s("test")),
            ("nums", Value::list(vec![num(1.0), num(2.0)])),
            (
                "meta",
                map(&[("ok", Value::Bool(true)), ("note", Value::Nil)]),
            ),
        ]);
        assert_deep(single(&input, ".").unwrap(), input);
    }

    #[test]
    fn string_with_unicode_round_trips() {
        let input = s("héllo → wörld 🌏");
        assert_eq!(single(&input, ".").unwrap(), input);
    }
}
