//! Higher-order function builtins.
//!
//! A parallel registry to the pure `builtins.rs`. Entries here get
//! `&mut Evaluator` and may call back into Mix code (user functions,
//! lambdas) via `Evaluator::call_function`. This split exists so the
//! 80+ pure builtins don't need to be async-converted.
//!
//! Rollback criterion (from the v0.2.0 plan, §2):
//!   If `eval_builtins` exceeds 6 entries, OR a pure builtin has to
//!   migrate here, reopen unification in v0.3.
//!
//! Phase 2 ships 12 entries — well over the 6-entry threshold, so
//! **v0.3 is committed to reopen two-registry unification**. This is
//! the tripwire firing cleanly: the threshold wasn't a hard budget
//! for v0.2, it was a signal for future planning, and the signal
//! has fired. Document it and carry on.

use indexmap::IndexMap;
use std::collections::HashSet;
use std::future::Future;
use std::pin::Pin;
use std::rc::Rc;

use crate::builtin_table;
use crate::builtins::CapabilityClass;
use crate::contract;
use crate::error::{MixError, MixResult};
use crate::evaluator::Evaluator;
use crate::scope::MixFunction;
use crate::value::Value;

/// An async builtin with evaluator access.
///
/// Returned future borrows `&'a mut Evaluator`, matching the shape of
/// `Evaluator::call_function` so HOF bodies can cleanly `.await` a
/// callback into Mix code.
pub type EvalBuiltin = for<'a> fn(
    &'a mut Evaluator,
    Vec<Value>,
) -> Pin<Box<dyn Future<Output = MixResult<Value>> + 'a>>;

/// Resolve a HOF builtin by name. Returns `None` if the name isn't a
/// HOF entry — the caller then falls through to `call_builtin` (pure).
pub fn lookup(name: &str) -> Option<EvalBuiltin> {
    match name {
        "__hof_smoke" => Some(hof_smoke),
        "sort_by" => Some(hof_sort_by),
        "filter" => Some(hof_filter),
        "map" => Some(hof_map),
        "reduce" => Some(hof_reduce),
        "any" => Some(hof_any),
        "all" => Some(hof_all),
        "count" => Some(hof_count),
        "min_by" => Some(hof_min_by),
        "max_by" => Some(hof_max_by),
        "sum_by" => Some(hof_sum_by),
        "group_by" => Some(hof_group_by),
        "unique_by" => Some(hof_unique_by),
        "require" => Some(hof_require),
        _ => None,
    }
}

// Single source of truth for HOF metadata — mirrors the pattern in
// `builtins.rs`. Each HOF here has a corresponding match arm in
// `lookup` above. Adding a new HOF means:
//   1. Add the dispatch arm in `lookup`
//   2. Add the entry in this table
//   3. Write the async body
// No third list in `meta.rs` to update.
builtin_table! {
    HOFS, HOF_NAMES,
    ("sort_by", CapabilityClass::Pure,   "hof", "Return list sorted ascending by key function (stable) (v0.2.0)", contract!((list: list, func: function) -> list)),
    ("filter", CapabilityClass::Pure,    "hof", "Return new list of items where predicate returns truthy (v0.2.0)", contract!((list: list, pred: function) -> list)),
    ("map", CapabilityClass::Pure,       "hof", "Return new list of transform(item) results (v0.2.0)", contract!((list: list, func: function) -> list)),
    ("reduce", CapabilityClass::Pure,    "hof", "Fold list left with an explicit init: reduce($xs, $init, function($a, $b) = ...) (v0.2.0)", contract!((list: list, init: any, func: function) -> any)),
    ("any", CapabilityClass::Pure,       "hof", "Short-circuit: true if any item matches predicate (v0.2.0)", contract!((list: list, pred: function) -> bool)),
    ("all", CapabilityClass::Pure,       "hof", "Short-circuit: true if every item matches predicate (v0.2.0)", contract!((list: list, pred: function) -> bool)),
    ("count", CapabilityClass::Pure,     "hof", "Count items where predicate returns truthy (v0.2.0)", contract!((list: list, pred: function) -> number)),
    ("min_by", CapabilityClass::Pure,    "hof", "Return the ITEM (not the key) with minimum key-function value (v0.2.0)", contract!((list: list, func: function) -> any)),
    ("max_by", CapabilityClass::Pure,    "hof", "Return the ITEM with maximum key-function value (v0.2.0)", contract!((list: list, func: function) -> any)),
    ("sum_by", CapabilityClass::Pure,    "hof", "Sum of key(item) across all items, returns number (v0.2.0)", contract!((list: list, func: function) -> number)),
    ("group_by", CapabilityClass::Pure,  "hof", "Map of stringified-key → list of items (first-seen key order) (v0.2.0)", contract!((list: list, func: function) -> map)),
    ("unique_by", CapabilityClass::Pure, "hof", "Dedup list by key function, first occurrence wins (v0.2.0)", contract!((list: list, func: function) -> list)),
    ("require", CapabilityClass::FsRead, "system", "Load a Mix module: evaluate the file once in an isolated scope, return its exports (map of top-level fns + $vars, or the file's top-level return value). Cached per canonical path; cycles error (v0.27.0)", contract!((path: string) -> any; failure[raises])),
}

// ---------- shared helpers ----------

fn arg_error(name: &str, msg: impl Into<String>) -> MixError {
    MixError::RuntimeError {
        span: None,
        msg: format!("{}: {}", name, msg.into()),
    }
}

// Both expect_* take the payload via `&mut`/`&` instead of a
// move-destructure: `Value` implements `Drop`, so moving the payload
// out in a pattern is E0509.
fn expect_list(name: &str, mut v: Value) -> MixResult<Vec<Value>> {
    match &mut v {
        // CoW: O(1) when this arg is the sole owner; a SHALLOW
        // (element-Rc-bump) copy when the scope still shares it —
        // strictly cheaper than the deep clone eval_args used to pay.
        // Never deep-clone here.
        Value::List(items) => Ok(Rc::unwrap_or_clone(std::mem::take(items))),
        other => Err(arg_error(
            name,
            format!("expected list, got {}", other.type_name()),
        )),
    }
}

fn expect_function(name: &str, v: Value) -> MixResult<Rc<MixFunction>> {
    match &v {
        Value::Function(rc) => Ok(Rc::clone(rc)),
        other => Err(arg_error(
            name,
            format!("expected function, got {}", other.type_name()),
        )),
    }
}

fn expect_two_args(name: &str, args: &[Value]) -> MixResult<()> {
    if args.len() != 2 {
        return Err(arg_error(
            name,
            format!("expects 2 args, got {}", args.len()),
        ));
    }
    Ok(())
}

fn expect_three_args(name: &str, args: &[Value]) -> MixResult<()> {
    if args.len() != 3 {
        return Err(arg_error(
            name,
            format!("expects 3 args, got {}", args.len()),
        ));
    }
    Ok(())
}

/// Compare two key values for ordering. Numbers compare numerically,
/// strings lexicographically, bools false<true, nil sorts before
/// everything. Mixed types fall back to string comparison so sort
/// remains total on heterogeneous keys rather than panicking.
fn cmp_values(a: &Value, b: &Value) -> std::cmp::Ordering {
    use std::cmp::Ordering;
    match (a, b) {
        (Value::Number(x), Value::Number(y)) => x.partial_cmp(y).unwrap_or(Ordering::Equal),
        (Value::String(x), Value::String(y)) => x.cmp(y),
        (Value::Bool(x), Value::Bool(y)) => x.cmp(y),
        (Value::Nil, Value::Nil) => Ordering::Equal,
        (Value::Nil, _) => Ordering::Less,
        (_, Value::Nil) => Ordering::Greater,
        _ => a.to_mix_string().cmp(&b.to_mix_string()),
    }
}

/// Invoke a Mix function value with a single-item argument list and
/// return its result. Tries the sync fast path first — many HOF
/// lambdas are simple sync expressions like `function($x) = $x * 2`,
/// and skipping `call_function`'s `Box::pin` is worth the eligibility
/// check on a hot per-element path. Falls back to the async path when
/// the body needs awaiting (calls, loops, etc.).
async fn call1(eval: &mut Evaluator, func: &MixFunction, arg: Value) -> MixResult<Value> {
    let args = [arg];
    if let Some(r) = eval.try_call_function_sync(func, &args) {
        return r;
    }
    eval.call_function_public(func, &args).await
}

// ---------- smoke entry (kept from Phase 1) ----------

fn hof_smoke<'a>(
    eval: &'a mut Evaluator,
    args: Vec<Value>,
) -> Pin<Box<dyn Future<Output = MixResult<Value>> + 'a>> {
    Box::pin(async move {
        expect_two_args("__hof_smoke", &args)?;
        let mut it = args.into_iter();
        let func = expect_function("__hof_smoke", it.next().unwrap())?;
        let x = it.next().unwrap();
        call1(eval, &func, x).await
    })
}

// ---------- core four ----------

fn hof_sort_by<'a>(
    eval: &'a mut Evaluator,
    args: Vec<Value>,
) -> Pin<Box<dyn Future<Output = MixResult<Value>> + 'a>> {
    Box::pin(async move {
        expect_two_args("sort_by", &args)?;
        let mut it = args.into_iter();
        let items = expect_list("sort_by", it.next().unwrap())?;
        let key_fn = expect_function("sort_by", it.next().unwrap())?;

        // Pre-compute keys once (cached sort). The alternative is
        // calling key_fn inside the comparator, which would run it
        // O(n log n) times and — more importantly — would require
        // `.await` inside a sync comparator closure, which doesn't
        // compose. Caching is both faster and simpler.
        let mut keyed: Vec<(Value, Value)> = Vec::with_capacity(items.len());
        for item in items {
            let k = call1(eval, &key_fn, item.clone()).await?;
            keyed.push((k, item));
        }
        keyed.sort_by(|a, b| cmp_values(&a.0, &b.0));
        Ok(Value::list(keyed.into_iter().map(|(_, v)| v).collect()))
    })
}

fn hof_filter<'a>(
    eval: &'a mut Evaluator,
    args: Vec<Value>,
) -> Pin<Box<dyn Future<Output = MixResult<Value>> + 'a>> {
    Box::pin(async move {
        expect_two_args("filter", &args)?;
        let mut it = args.into_iter();
        let items = expect_list("filter", it.next().unwrap())?;
        let pred = expect_function("filter", it.next().unwrap())?;

        let mut out = Vec::new();
        for item in items {
            let keep = call1(eval, &pred, item.clone()).await?;
            if keep.is_truthy() {
                out.push(item);
            }
        }
        Ok(Value::list(out))
    })
}

fn hof_map<'a>(
    eval: &'a mut Evaluator,
    args: Vec<Value>,
) -> Pin<Box<dyn Future<Output = MixResult<Value>> + 'a>> {
    Box::pin(async move {
        expect_two_args("map", &args)?;
        let mut it = args.into_iter();
        let items = expect_list("map", it.next().unwrap())?;
        let f = expect_function("map", it.next().unwrap())?;

        let mut out = Vec::with_capacity(items.len());
        for item in items {
            out.push(call1(eval, &f, item).await?);
        }
        Ok(Value::list(out))
    })
}

fn hof_reduce<'a>(
    eval: &'a mut Evaluator,
    args: Vec<Value>,
) -> Pin<Box<dyn Future<Output = MixResult<Value>> + 'a>> {
    Box::pin(async move {
        expect_three_args("reduce", &args)?;
        let mut it = args.into_iter();
        let items = expect_list("reduce", it.next().unwrap())?;
        let init = it.next().unwrap();
        let combine = expect_function("reduce", it.next().unwrap())?;

        let mut acc = init;
        for item in items {
            let args = [acc, item];
            acc = match eval.try_call_function_sync(&combine, &args) {
                Some(r) => r?,
                None => eval.call_function_public(&combine, &args).await?,
            };
        }
        Ok(acc)
    })
}

// ---------- short-circuit folds ----------

fn hof_any<'a>(
    eval: &'a mut Evaluator,
    args: Vec<Value>,
) -> Pin<Box<dyn Future<Output = MixResult<Value>> + 'a>> {
    Box::pin(async move {
        expect_two_args("any", &args)?;
        let mut it = args.into_iter();
        let items = expect_list("any", it.next().unwrap())?;
        let pred = expect_function("any", it.next().unwrap())?;

        for item in items {
            if call1(eval, &pred, item).await?.is_truthy() {
                return Ok(Value::Bool(true));
            }
        }
        Ok(Value::Bool(false))
    })
}

fn hof_all<'a>(
    eval: &'a mut Evaluator,
    args: Vec<Value>,
) -> Pin<Box<dyn Future<Output = MixResult<Value>> + 'a>> {
    Box::pin(async move {
        expect_two_args("all", &args)?;
        let mut it = args.into_iter();
        let items = expect_list("all", it.next().unwrap())?;
        let pred = expect_function("all", it.next().unwrap())?;

        for item in items {
            if !call1(eval, &pred, item).await?.is_truthy() {
                return Ok(Value::Bool(false));
            }
        }
        Ok(Value::Bool(true))
    })
}

fn hof_count<'a>(
    eval: &'a mut Evaluator,
    args: Vec<Value>,
) -> Pin<Box<dyn Future<Output = MixResult<Value>> + 'a>> {
    Box::pin(async move {
        expect_two_args("count", &args)?;
        let mut it = args.into_iter();
        let items = expect_list("count", it.next().unwrap())?;
        let pred = expect_function("count", it.next().unwrap())?;

        let mut n = 0i64;
        for item in items {
            if call1(eval, &pred, item).await?.is_truthy() {
                n += 1;
            }
        }
        Ok(Value::Number(n as f64))
    })
}

// ---------- _by family ----------

/// Shared body for `min_by` / `max_by`. `want_min` picks the
/// comparison direction. Returns the ITEM (not the key) so callers
/// don't need a second lookup — that's the defining choice for the
/// `_by` suffix per the plan.
async fn min_max_by(
    eval: &mut Evaluator,
    name: &str,
    args: Vec<Value>,
    want_min: bool,
) -> MixResult<Value> {
    expect_two_args(name, &args)?;
    let mut it = args.into_iter();
    let items = expect_list(name, it.next().unwrap())?;
    let key_fn = expect_function(name, it.next().unwrap())?;

    if items.is_empty() {
        return Ok(Value::Nil);
    }

    let mut best_idx: usize = 0;
    let mut best_key = call1(eval, &key_fn, items[0].clone()).await?;
    for (i, item) in items.iter().enumerate().skip(1) {
        let k = call1(eval, &key_fn, item.clone()).await?;
        let ord = cmp_values(&k, &best_key);
        let take = if want_min {
            ord == std::cmp::Ordering::Less
        } else {
            ord == std::cmp::Ordering::Greater
        };
        if take {
            best_idx = i;
            best_key = k;
        }
    }
    Ok(items[best_idx].clone())
}

fn hof_min_by<'a>(
    eval: &'a mut Evaluator,
    args: Vec<Value>,
) -> Pin<Box<dyn Future<Output = MixResult<Value>> + 'a>> {
    Box::pin(async move { min_max_by(eval, "min_by", args, true).await })
}

fn hof_max_by<'a>(
    eval: &'a mut Evaluator,
    args: Vec<Value>,
) -> Pin<Box<dyn Future<Output = MixResult<Value>> + 'a>> {
    Box::pin(async move { min_max_by(eval, "max_by", args, false).await })
}

fn hof_sum_by<'a>(
    eval: &'a mut Evaluator,
    args: Vec<Value>,
) -> Pin<Box<dyn Future<Output = MixResult<Value>> + 'a>> {
    Box::pin(async move {
        expect_two_args("sum_by", &args)?;
        let mut it = args.into_iter();
        let items = expect_list("sum_by", it.next().unwrap())?;
        let key_fn = expect_function("sum_by", it.next().unwrap())?;

        let mut acc = 0.0f64;
        for item in items {
            let k = call1(eval, &key_fn, item).await?;
            acc += k.to_number().ok_or_else(|| {
                arg_error(
                    "sum_by",
                    format!("key function returned non-number: {}", k.type_name()),
                )
            })?;
        }
        Ok(Value::Number(acc))
    })
}

fn hof_group_by<'a>(
    eval: &'a mut Evaluator,
    args: Vec<Value>,
) -> Pin<Box<dyn Future<Output = MixResult<Value>> + 'a>> {
    Box::pin(async move {
        expect_two_args("group_by", &args)?;
        let mut it = args.into_iter();
        let items = expect_list("group_by", it.next().unwrap())?;
        let key_fn = expect_function("group_by", it.next().unwrap())?;

        // IndexMap so group insertion order matches first-seen key
        // order. Scripts that iterate with `for each` then get a
        // stable traversal — important for the digest acceptance
        // test where grouping drives presentation order.
        let mut groups: IndexMap<String, Vec<Value>> = IndexMap::new();
        for item in items {
            let k = call1(eval, &key_fn, item.clone()).await?;
            let key_str = k.to_mix_string();
            groups.entry(key_str).or_default().push(item);
        }

        let mut out = IndexMap::new();
        for (k, v) in groups {
            out.insert(k, Value::list(v));
        }
        Ok(Value::map(out))
    })
}

// ---------- module loader ----------

fn hof_require<'a>(
    eval: &'a mut Evaluator,
    args: Vec<Value>,
) -> Pin<Box<dyn Future<Output = MixResult<Value>> + 'a>> {
    Box::pin(async move {
        if args.len() != 1 {
            return Err(arg_error(
                "require",
                format!("expects 1 arg (path), got {}", args.len()),
            ));
        }
        // `&mut` + mem::take instead of a move-destructure: `Value`
        // implements `Drop`, so moving the payload out in a pattern is
        // E0509 (same shape as expect_list above).
        let mut v = args.into_iter().next().unwrap();
        let path = match &mut v {
            Value::String(s) => std::mem::take(s),
            other => {
                return Err(arg_error(
                    "require",
                    format!("expected string path, got {}", other.type_name()),
                ));
            }
        };
        eval.exec_require(&path).await
    })
}

fn hof_unique_by<'a>(
    eval: &'a mut Evaluator,
    args: Vec<Value>,
) -> Pin<Box<dyn Future<Output = MixResult<Value>> + 'a>> {
    Box::pin(async move {
        expect_two_args("unique_by", &args)?;
        let mut it = args.into_iter();
        let items = expect_list("unique_by", it.next().unwrap())?;
        let key_fn = expect_function("unique_by", it.next().unwrap())?;

        // Membership via a HashSet (O(1) per probe). A `Vec` +
        // `contains` here was O(n²) — a real stall on multi-thousand-
        // element inputs. `out` still preserves first-seen order.
        let mut seen: HashSet<String> = HashSet::new();
        let mut out: Vec<Value> = Vec::new();
        for item in items {
            let k = call1(eval, &key_fn, item.clone()).await?;
            let key_str = k.to_mix_string();
            if seen.insert(key_str) {
                out.push(item);
            }
        }
        Ok(Value::list(out))
    })
}
