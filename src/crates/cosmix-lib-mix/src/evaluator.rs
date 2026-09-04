use indexmap::IndexMap;
use std::cell::{Cell, Ref, RefCell, RefMut};
use std::collections::{BTreeMap, HashMap, HashSet};
use std::future::Future;
use std::io::Write;
use std::pin::Pin;
use std::rc::Rc;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

// SPEC 18 §3.4: `catch_unwind` combinator for the per-handler panic
// boundary in `run_handlers_for`. `std::panic::catch_unwind` is
// synchronous and cannot wrap an `.await`; the futures-util combinator
// is the async-aware equivalent the plan prescribes.
use futures_util::future::FutureExt;

use crate::ast::*;
use crate::builtins;
use crate::error::{MixError, MixResult, Span};
use crate::numeric::{
    InputPolicy, as_duration, as_loop_bound, as_loop_step, as_signed_index, extract_number,
};
use crate::scope::{FibOp, MixFunction, Scope};
use crate::stats::UsageStats;
use crate::token::StringPart;
use crate::value::Value;

/// Which coalescing operator a `${…}` interpolation carries as its fallback.
/// `??` (nil-only) and `?:` (falsy: nil/`""`/`0`/`false`/`[]`) mirror the
/// same-named expression operators, so a default inside a string behaves
/// exactly as it would in Mix code.
#[derive(Clone, Copy, PartialEq)]
enum InterpCoalesce {
    /// `??` — fall back only when the resolved value is nil.
    Nil,
    /// `?:` — fall back when the resolved value is falsy.
    Falsy,
}

/// Split a `${…}` interpolation spec into its dotted-path head and an optional
/// coalescing default. `x` → `("x", None)`; `x ?? ""` → `("x", Some((Nil,
/// "\"\"")))`; `a.b ?: $fallback` → `("a.b", Some((Falsy, "$fallback")))`.
///
/// A dotted path never contains `?`, so the first `??`/`?:` scanning left to
/// right is unambiguously the operator — everything before it is the path,
/// everything after is the default *expression source* (parsed + evaluated
/// lazily by the caller, only when the fallback actually fires). Both sides are
/// trimmed. A lone `?` not forming `??`/`?:` is left in the path (it will fail
/// to resolve and surface a normal error).
fn split_interp_coalesce(spec: &str) -> (&str, Option<(InterpCoalesce, &str)>) {
    let bytes = spec.as_bytes();
    let mut i = 0;
    while i + 1 < bytes.len() {
        if bytes[i] == b'?' {
            let kind = match bytes[i + 1] {
                b'?' => Some(InterpCoalesce::Nil),
                b':' => Some(InterpCoalesce::Falsy),
                _ => None,
            };
            if let Some(kind) = kind {
                return (spec[..i].trim(), Some((kind, spec[i + 2..].trim())));
            }
        }
        i += 1;
    }
    (spec, None)
}

/// The dotted-path head of an interpolation spec, ignoring any coalescing
/// default — `${a.b ?? c}` → `"a"`. Used by slot-cache collection so a spec
/// with a default doesn't register the whole `"a.b ?? c"` string as a name.
fn interp_head(spec: &str) -> &str {
    split_interp_coalesce(spec)
        .0
        .split('.')
        .next()
        .unwrap_or("")
}

/// A Rust function callable from Mix scripts via the extension API.
///
/// Extension functions are async to support I/O operations (e.g., Bus IPC).
/// For pure-sync extensions, wrap the body in `Box::pin(async move { ... })`.
pub type ExtFn = Rc<dyn Fn(Vec<Value>) -> Pin<Box<dyn Future<Output = MixResult<Value>>>>>;

/// Helper to create an ExtFn from a synchronous closure.
pub fn sync_ext(f: impl Fn(Vec<Value>) -> MixResult<Value> + 'static) -> ExtFn {
    Rc::new(move |args| {
        let result = f(args);
        Box::pin(async move { result })
    })
}

/// Build the Mix `$event` map value from an `IncomingEvent`.
///
/// Shape: `{ command: <string>, headers: <map>, body: <string>, args: <parsed
/// body | nil> }`. Handlers access fields via `$event.command`,
/// `$event.headers["topic"]`, `$event.body`, and `$event.args`. The header map
/// preserves BTreeMap ordering (stable alphabetical), which is what Mix's Map
/// iteration expects.
fn incoming_event_to_mix_value(event: &IncomingEvent) -> Value {
    let mut headers_map: IndexMap<String, Value> = IndexMap::new();
    for (k, v) in &event.headers {
        headers_map.insert(k.clone(), Value::String(v.clone()));
    }
    let mut event_map: IndexMap<String, Value> = IndexMap::new();
    event_map.insert("command".to_string(), Value::String(event.command.clone()));
    event_map.insert("headers".to_string(), Value::map(headers_map));
    event_map.insert("body".to_string(), Value::String(event.body.clone()));
    event_map.insert("args".to_string(), parse_event_args(&event.body));
    Value::map(event_map)
}

/// Best-effort structured view of the message body, exposed as `$event.args`.
///
/// The sender's JSON-body path serialises structured args into the body — a
/// positional `send svc cmd "a" "b"` becomes `{"_0":"a","_1":"b"}`, and a map
/// arg becomes its JSON object. `$event.args` exposes that PRE-PARSED so a
/// handler writes `$event.args["_0"]` / `$event.args.field` instead of
/// `json_parse($event.body)`; it is symmetric with the already-parsed
/// `$event.headers` map. `$event.body` stays the raw string regardless.
///
/// Nil when the body is empty, is not valid JSON (a raw `body="hi"` text
/// payload — a bare word is not JSON, so text bodies read back as nil), or the
/// `json` feature is compiled out. A handler distinguishes "no structured
/// args" from a real value with `??` / `if`, and reaches for `$event.body`
/// when it wants the raw bytes.
#[cfg(feature = "json")]
fn parse_event_args(body: &str) -> Value {
    if body.is_empty() {
        return Value::Nil;
    }
    match serde_json::from_str::<serde_json::Value>(body) {
        Ok(j) => crate::json::json_to_mix(j),
        Err(_) => Value::Nil,
    }
}

#[cfg(not(feature = "json"))]
fn parse_event_args(_body: &str) -> Value {
    Value::Nil
}

/// In-place `container[idx] = val`. Returns `Some(error_message)` to
/// raise, or `None` on success. Shared by the async `IndexAssignment`
/// arm and the for-loop fast path so signed-index and non-container
/// handling can't drift between the two copies.
///
/// - Map: insert (any key); enforces `max_map` if set.
/// - List: the index type mirrors the READ path (`index_value_clone`) —
///   only a `Value::Number` indexes a list. It is resolved through
///   `resolve_signed_index`, so a negative index writes from the end
///   (`$l[-1] = v` hits the last element) rather than saturating to 0. A
///   NaN/infinite index is rejected (would otherwise `as i64`→0 and
///   corrupt element 0); a non-number index, or an out-of-range one, is a
///   loud error, not a silent drop. (Reads return `nil` for a missing
///   index; a write to a missing index errors instead — a lost write is
///   data loss, a missing read has a safe default.)
/// - Anything else (number/string/nil/bool/bytes/function): a typed
///   error instead of silently discarding the write.
fn assign_index_in_place(
    v: &mut Value,
    idx: Value,
    val: Value,
    max_map: Option<usize>,
) -> Option<MixError> {
    match v {
        Value::Map(map) => {
            // Moves the inner String out when idx is already a string —
            // `to_mix_string` would clone it.
            let key = idx.into_map_key();
            // Preflight the cap: only a NEW key grows the map, and the
            // check must precede the insert — inserting first and
            // erroring after leaves the map already over the limit for
            // anyone who catches the error.
            if let Some(max) = max_map
                && !map.contains_key(&key)
                && map.len() >= max
            {
                return Some(MixError::RuntimeError {
                    span: None,
                    msg: format!("map size {} exceeds limit {}", map.len() + 1, max),
                });
            }
            // CoW: detach from any sharing binding before the write
            // (copies one level iff shared — exactly when another Mix
            // binding can still observe the old value).
            Rc::make_mut(map).insert(key, val);
            None
        }
        Value::List(list) => match &idx {
            Value::Number(n) => match as_signed_index("list assignment index", *n) {
                Ok(n) => match crate::builtins::resolve_signed_index(n, list.len()) {
                    Some(pos) => {
                        // CoW: see the Map arm.
                        Rc::make_mut(list)[pos] = val;
                        None
                    }
                    None => Some(MixError::RuntimeError {
                        span: None,
                        msg: format!("list index {n} out of range (length {})", list.len()),
                    }),
                },
                Err(err) => Some(err),
            },
            other => Some(MixError::RuntimeError {
                span: None,
                msg: format!("cannot index list with {}", other.type_name()),
            }),
        },
        other => Some(MixError::RuntimeError {
            span: None,
            msg: format!("cannot index-assign into {}", other.type_name()),
        }),
    }
}

/// In-place `container.field = val`. Returns `Some(error_message)` to
/// raise, or `None` on success. Shared by the `FieldAssignment` arm and
/// the path walker.
///
/// A non-map target is a **loud error**, mirroring `assign_index_in_place`.
/// (Before 0.33.0 the `FieldAssignment` arm matched `if let Value::Map`
/// with no else, so `$x = 5; $x.f = 1` silently discarded the write —
/// data loss with no diagnostic.)
fn assign_field_in_place(
    v: &mut Value,
    field: &str,
    val: Value,
    max_map: Option<usize>,
) -> Option<String> {
    match v {
        Value::Map(map) => {
            // Cap preflight: only a new key grows the map. See
            // `assign_index_in_place` — check before the insert.
            if let Some(max) = max_map
                && !map.contains_key(field)
                && map.len() >= max
            {
                return Some(format!("map size {} exceeds limit {}", map.len() + 1, max));
            }
            Rc::make_mut(map).insert(field.to_string(), val);
            None
        }
        other => Some(format!("cannot index-assign into {}", other.type_name())),
    }
}

/// One segment of an assignment path with its index expression already
/// evaluated — the form the mutation walker consumes.
///
/// `.k` and `["k"]` stay distinct: a `Field` can only ever address a map,
/// while an `Index` may address a map key or a list position.
#[derive(Debug, Clone)]
pub(crate) enum ResolvedSeg {
    Field(String),
    Index(Value),
}

impl ResolvedSeg {
    /// The map key this segment addresses.
    fn map_key(&self) -> String {
        match self {
            ResolvedSeg::Field(f) => f.clone(),
            ResolvedSeg::Index(v) => v.clone().into_map_key(),
        }
    }

    /// May a *missing* container be auto-created as a map so that THIS
    /// segment can address it?
    ///
    /// Only for a field or a string key. A numeric index is ambiguous —
    /// `$m["a"][0] = v` could mean a list slot or the map key `"0"` — and
    /// guessing would silently freeze one interpretation into the data.
    /// The script says which it wants by assigning an explicit `[]`/`{}`.
    fn is_map_shaped(&self) -> bool {
        matches!(
            self,
            ResolvedSeg::Field(_) | ResolvedSeg::Index(Value::String(_))
        )
    }

    fn render(&self) -> String {
        match self {
            ResolvedSeg::Field(f) => format!(".{}", f),
            ResolvedSeg::Index(v) => format!("[{}]", v.to_mix_string()),
        }
    }
}

/// Render `$root` + the first `upto` segments, for error messages.
fn render_path(root: &str, segs: &[ResolvedSeg], upto: usize) -> String {
    let mut s = format!("${}", root);
    for seg in segs.iter().take(upto) {
        s.push_str(&seg.render());
    }
    s
}

/// A path-assignment failure: an optional structured error code plus the
/// message. Only the ambiguous-auto-create case carries a code today
/// (`INDEX_MISSING`) — the others keep the plain runtime errors the
/// single-level writes have always raised, so existing `catch` code that
/// matches on them is undisturbed.
enum PathErr {
    Coded(&'static str, String),
    Runtime(String),
    Error(MixError),
}

fn no_vivify_err(root: &str, segs: &[ResolvedSeg], i: usize) -> PathErr {
    PathErr::Coded(
        "INDEX_MISSING",
        format!(
            "cannot auto-create a container at {}: the next accessor {} is not a name or string \
             key, so it could mean either a list position or a map key — assign an explicit [] or \
             {{}} first",
            render_path(root, segs, i + 1),
            segs[i + 1].render()
        ),
    )
}

/// An uncoded path failure (plain runtime error).
fn plain(msg: String) -> PathErr {
    PathErr::Runtime(msg)
}

/// Validate a nested assignment WITHOUT mutating anything.
///
/// Run before `assign_path_in_place` so a path that fails halfway (a
/// scalar intermediate, an out-of-range list index, a map cap) leaves no
/// half-vivified maps behind — the write is all-or-nothing, which matters
/// because a script can `catch` the error and carry on.
fn check_path_assign(
    root_name: &str,
    root: &Value,
    segs: &[ResolvedSeg],
    max_map: Option<usize>,
) -> Option<PathErr> {
    let last = segs.len() - 1;
    let mut cur: &Value = root;

    // Everything from a vivify site down descends into maps we would
    // CREATE. Each further intermediate must itself be auto-creatable,
    // and the fresh map at the end receives exactly one key — which a
    // `max_map` of 0 forbids. Both must be caught HERE: by the time the
    // mutator discovered them it would already have created the maps
    // above, and a caught error would leave them behind.
    let fresh_tail_ok = |from: usize| -> Option<PathErr> {
        for j in from..last {
            if !segs[j + 1].is_map_shaped() {
                return Some(no_vivify_err(root_name, segs, j));
            }
        }
        if let Some(max) = max_map
            && max < 1
        {
            return Some(plain(format!("map size 1 exceeds limit {}", max)));
        }
        None
    };

    for i in 0..last {
        // Auto-creating this intermediate is decided by the NEXT segment:
        // it is the one that has to address the container we'd create.
        let vivify_ok = segs[i + 1].is_map_shaped();

        match cur {
            Value::Map(map) => {
                let key = segs[i].map_key();
                // Exact key only — never fall through to the `"*"`
                // wildcard used on the READ path, or `$m.missing.k = v`
                // would quietly mutate `$m["*"]`.
                match map.get(&key) {
                    Some(Value::Nil) | None => {
                        if !vivify_ok {
                            return Some(no_vivify_err(root_name, segs, i));
                        }
                        if map.get(&key).is_none()
                            && let Some(max) = max_map
                            && map.len() >= max
                        {
                            return Some(plain(format!(
                                "map size {} exceeds limit {}",
                                map.len() + 1,
                                max
                            )));
                        }
                        return fresh_tail_ok(i + 1);
                    }
                    Some(v) => cur = v,
                }
            }
            Value::List(list) => match &segs[i] {
                ResolvedSeg::Index(Value::Number(n)) => {
                    let n = match as_signed_index("list assignment index", *n) {
                        Ok(n) => n,
                        Err(err) => return Some(PathErr::Error(err)),
                    };
                    match crate::builtins::resolve_signed_index(n, list.len()) {
                        // A list is never extended by assignment, at any
                        // depth — an out-of-range write is a lost write.
                        None => {
                            return Some(plain(format!(
                                "list index {} out of range (length {}) at {}",
                                n,
                                list.len(),
                                render_path(root_name, segs, i)
                            )));
                        }
                        Some(pos) => match &list[pos] {
                            Value::Nil => {
                                if !vivify_ok {
                                    return Some(no_vivify_err(root_name, segs, i));
                                }
                                return fresh_tail_ok(i + 1);
                            }
                            v => cur = v,
                        },
                    }
                }
                ResolvedSeg::Index(other) => {
                    return Some(plain(format!(
                        "cannot index list with {}",
                        other.type_name()
                    )));
                }
                ResolvedSeg::Field(f) => {
                    return Some(plain(format!(
                        "cannot read field .{} of a list at {}",
                        f,
                        render_path(root_name, segs, i)
                    )));
                }
            },
            // An existing scalar is never silently replaced by a map.
            other => {
                return Some(plain(format!(
                    "cannot index-assign into {} at {}",
                    other.type_name(),
                    render_path(root_name, segs, i)
                )));
            }
        }
    }

    // The final container exists; validate the write into it without
    // performing it. `Value::Nil` is unreachable here (the loop above
    // returns early once it decides to vivify).
    match (cur, &segs[last]) {
        (Value::Map(map), seg) => {
            let key = seg.map_key();
            if let Some(max) = max_map
                && !map.contains_key(&key)
                && map.len() >= max
            {
                return Some(plain(format!(
                    "map size {} exceeds limit {}",
                    map.len() + 1,
                    max
                )));
            }
            None
        }
        (Value::List(list), ResolvedSeg::Index(Value::Number(n))) => {
            let n = match as_signed_index("list assignment index", *n) {
                Ok(n) => n,
                Err(err) => return Some(PathErr::Error(err)),
            };
            match crate::builtins::resolve_signed_index(n, list.len()) {
                Some(_) => None,
                None => Some(plain(format!(
                    "list index {} out of range (length {})",
                    n,
                    list.len()
                ))),
            }
        }
        (Value::List(_), ResolvedSeg::Index(other)) => Some(plain(format!(
            "cannot index list with {}",
            other.type_name()
        ))),
        (Value::List(_), ResolvedSeg::Field(f)) => {
            Some(plain(format!("cannot assign field .{} on a list", f)))
        }
        (other, _) => Some(plain(format!(
            "cannot index-assign into {} at {}",
            other.type_name(),
            render_path(root_name, segs, last)
        ))),
    }
}

/// In-place nested write: `$root<segs> = val`, `segs.len() >= 2`.
///
/// Call ONLY after `check_path_assign` has approved the same
/// `(root, segs)` — the error returns here are a backstop, not the
/// primary diagnostic path.
///
/// Walks the path with `Rc::make_mut` at every level. That is the whole
/// point of the node: the get→mutate→reassign workaround reads the inner
/// container out (refcount 2), so every write deep-copies it — O(N²) when
/// building a map of maps. Descending through `make_mut` keeps the
/// refcount at 1 all the way down, so nothing is copied and the same loop
/// is linear.
fn assign_path_in_place(
    root: &mut Value,
    segs: &[ResolvedSeg],
    val: Value,
    max_map: Option<usize>,
) -> Option<String> {
    let last = segs.len() - 1;
    let mut cur: &mut Value = root;

    for seg in segs.iter().take(last) {
        cur = match cur {
            Value::Map(map) => {
                let key = seg.map_key();
                let map = Rc::make_mut(map);
                let slot = map
                    .entry(key)
                    .or_insert_with(|| Value::Map(Rc::new(IndexMap::new())));
                if matches!(slot, Value::Nil) {
                    *slot = Value::Map(Rc::new(IndexMap::new()));
                }
                slot
            }
            Value::List(list) => {
                let ResolvedSeg::Index(Value::Number(n)) = seg else {
                    return Some(format!("cannot index list with {}", seg.render()));
                };
                let Some(pos) = crate::builtins::resolve_signed_index(*n as i64, list.len()) else {
                    return Some(format!(
                        "list index {} out of range (length {})",
                        *n as i64,
                        list.len()
                    ));
                };
                let slot = &mut Rc::make_mut(list)[pos];
                if matches!(slot, Value::Nil) {
                    *slot = Value::Map(Rc::new(IndexMap::new()));
                }
                slot
            }
            other => return Some(format!("cannot index-assign into {}", other.type_name())),
        };
    }

    match &segs[last] {
        ResolvedSeg::Field(f) => assign_field_in_place(cur, f, val, max_map),
        ResolvedSeg::Index(idx) => {
            assign_index_in_place(cur, idx.clone(), val, max_map).map(|err| err.to_string())
        }
    }
}

/// A borrowed assignment-path segment, used to compare an assignment
/// TARGET against an expression without cloning either.
#[derive(Debug, Clone, Copy)]
pub(crate) enum LvSeg<'a> {
    Field(&'a str),
    Index(&'a Expr),
}

/// Read `$root.a["b"]` — an accessor chain rooted at a plain variable —
/// as `(root, segments)`. Anything else (a call, a literal, a chain
/// rooted at something other than a variable) is not an lvalue: `None`.
fn expr_as_lvalue(expr: &Expr) -> Option<(&str, Vec<LvSeg<'_>>)> {
    match expr {
        Expr::Variable(name) => Some((name.as_str(), Vec::new())),
        Expr::Index { object, index } => {
            let (root, mut segs) = expr_as_lvalue(object)?;
            segs.push(LvSeg::Index(index));
            Some((root, segs))
        }
        Expr::FieldAccess { object, field } => {
            let (root, mut segs) = expr_as_lvalue(object)?;
            segs.push(LvSeg::Field(field.as_str()));
            Some((root, segs))
        }
        _ => None,
    }
}

/// Structural equality over the `is_expr_sync` subset — enough to prove
/// two index expressions denote the same slot.
///
/// Deliberately conservative: any shape not listed compares `false` and
/// the caller falls back to the generic path. `.k` and `["k"]` are never
/// equated (different segment kinds), and numbers compare by bits so
/// `0.0`/`-0.0` don't collide.
fn expr_eq_sync(a: &Expr, b: &Expr) -> bool {
    match (a, b) {
        (Expr::NumberLiteral(x), Expr::NumberLiteral(y)) => x.to_bits() == y.to_bits(),
        (Expr::StringLiteral(x), Expr::StringLiteral(y))
        | (Expr::EscapedQuoteStringLiteral(x), Expr::EscapedQuoteStringLiteral(y))
        | (Expr::StringLiteral(x), Expr::EscapedQuoteStringLiteral(y))
        | (Expr::EscapedQuoteStringLiteral(x), Expr::StringLiteral(y)) => x == y,
        (Expr::BoolLiteral(x), Expr::BoolLiteral(y)) => x == y,
        (Expr::NilLiteral, Expr::NilLiteral) => true,
        (Expr::Variable(x), Expr::Variable(y)) => x == y,
        (
            Expr::BinaryOp {
                left: l1,
                op: o1,
                right: r1,
            },
            Expr::BinaryOp {
                left: l2,
                op: o2,
                right: r2,
            },
        ) => o1 == o2 && expr_eq_sync(l1, l2) && expr_eq_sync(r1, r2),
        (
            Expr::InterpolatedString(p1) | Expr::Heredoc(p1),
            Expr::InterpolatedString(p2) | Expr::Heredoc(p2),
        ) => {
            p1.len() == p2.len()
                && p1.iter().zip(p2.iter()).all(|(x, y)| match (x, y) {
                    (StringPart::Literal(a), StringPart::Literal(b)) => a == b,
                    (StringPart::Variable(a), StringPart::Variable(b)) => a == b,
                    (StringPart::EnvVar(a), StringPart::EnvVar(b)) => a == b,
                    _ => false,
                })
        }
        (
            Expr::Index {
                object: o1,
                index: i1,
            },
            Expr::Index {
                object: o2,
                index: i2,
            },
        ) => expr_eq_sync(o1, o2) && expr_eq_sync(i1, i2),
        (
            Expr::FieldAccess {
                object: o1,
                field: f1,
            },
            Expr::FieldAccess {
                object: o2,
                field: f2,
            },
        ) => f1 == f2 && expr_eq_sync(o1, o2),
        _ => false,
    }
}

fn lv_seg_eq(a: LvSeg<'_>, b: LvSeg<'_>) -> bool {
    match (a, b) {
        (LvSeg::Field(x), LvSeg::Field(y)) => x == y,
        (LvSeg::Index(x), LvSeg::Index(y)) => expr_eq_sync(x, y),
        _ => false,
    }
}

/// Walk an already-validated path to the value AT its end (no
/// auto-creation), for the push fast path. `None` if any step is missing
/// or the wrong type — the caller then takes the generic path.
fn path_leaf<'a>(root: &'a Value, segs: &[ResolvedSeg]) -> Option<&'a Value> {
    let mut cur = root;
    for seg in segs {
        cur = match (cur, seg) {
            // Exact key only — the `"*"` wildcard is a READ fallback for
            // missing keys and must not make the fast path think a list
            // lives at a key that doesn't exist.
            (Value::Map(map), seg) => map.get(&seg.map_key())?,
            // STRICT here on purpose, unlike the public read path: this is
            // the PREFLIGHT for the push-assign fast path, and the write it
            // guards validates its index strictly. A lenient truncating
            // probe (0.5 -> 0) would bless a target the write then refuses,
            // surfacing as "internal: push fast-path target changed under
            // it" (round-2 review). `.ok()?` DECLINES the fast path; the
            // generic path then raises the proper VALUE_OUT_OF_RANGE.
            (Value::List(list), ResolvedSeg::Index(Value::Number(n))) => {
                list.get(crate::builtins::resolve_signed_index(
                    as_signed_index("list index", *n).ok()?,
                    list.len(),
                )?)?
            }
            _ => return None,
        };
    }
    Some(cur)
}

/// `$m[$k] = push($m[$k], $v)` — append IN PLACE to a list that lives
/// inside a container.
///
/// `push` can only mutate through a bare variable slot, so given any
/// other first argument it appends to a COPY and returns it — correct,
/// but the copy makes the read-append-writeback idiom O(N²). This walks
/// the path with `Rc::make_mut` (refcount 1 all the way down, nothing
/// copied) and appends, making the idiom linear.
///
/// Mirrors the `$s = $s .. rhs` self-concat fast path: it fires only on
/// an exact structural match between the assignment target and push's
/// first argument, with every index expression sync/pure, so it cannot
/// change evaluation order or run a side effect a different number of
/// times.
/// `Err(msg)` raises. The path is re-walked here after `path_leaf`
/// approved it under the SAME synchronous borrow window — no user code
/// runs in between, so a "target vanished" case is unreachable rather
/// than merely unlikely, and is reported loudly instead of silently
/// dropping the append.
fn push_through_path(
    root: &mut Value,
    segs: &[ResolvedSeg],
    val: Value,
    max_list: Option<usize>,
) -> Result<(), String> {
    const LOST: &str = "internal: push fast-path target changed under it";
    let mut cur: &mut Value = root;
    for seg in segs {
        cur = match cur {
            Value::Map(map) => {
                let key = seg.map_key();
                match Rc::make_mut(map).get_mut(&key) {
                    Some(v) => v,
                    None => return Err(LOST.to_string()),
                }
            }
            Value::List(list) => {
                let ResolvedSeg::Index(Value::Number(n)) = seg else {
                    return Err(LOST.to_string());
                };
                match crate::builtins::resolve_signed_index(
                    as_signed_index("list index", *n).map_err(|_| LOST.to_string())?,
                    list.len(),
                ) {
                    Some(pos) => &mut Rc::make_mut(list)[pos],
                    None => return Err(LOST.to_string()),
                }
            }
            _ => return Err(LOST.to_string()),
        };
    }
    let Value::List(list) = cur else {
        return Err(LOST.to_string());
    };
    // Cap BEFORE the append: erroring after growing would leave the list
    // over its limit for anyone who catches.
    if let Some(max) = max_list
        && list.len() + 1 > max
    {
        // Same wording as the generic growth sites — the message is
        // observable through `catch`, so it must not fork by path.
        return Err(format!(
            "list length {} exceeds limit {}",
            list.len() + 1,
            max
        ));
    }
    Rc::make_mut(list).push(val);
    Ok(())
}

/// An incoming Bus message delivered to a Mix event handler.
///
/// This is the **neutral language-level event type**. Mix is a reusable
/// scripting language and not every host of `cosmix-lib-mix` will be
/// `cosmix-noded` — a future host could be a test harness, a standalone CLI
/// tool, a different message bus, or something that doesn't yet exist. The
/// trait boundary between `lib-mix` and its event source is the interface
/// that lets alternative hosts plug in their own event stream implementations,
/// so it must not name a specific host's wire type.
///
/// Hosts adapt their native event types into `IncomingEvent` at the trait
/// boundary (via an explicit field copy or a `From`/`Into` impl). This costs
/// one struct copy per message at the boundary — negligible at realistic
/// event rates — and keeps `lib-mix` reusable across different Bus
/// implementations, test harnesses, and future non-Bus transports.
///
/// Applied principle: "event source is a transport concern, not a language
/// concern, and the language's data model should not name its hosts." Same
/// shape as the topic broker's "transport metadata is just headers" rule,
/// lifted one layer up.
#[derive(Debug, Clone)]
pub struct IncomingEvent {
    pub command: String,
    pub headers: BTreeMap<String, String>,
    pub body: String,
}

/// SPEC 18 Phase 2 WS3-C.7c — opaque per-citizen invocation id.
///
/// `u64` (not `Uuid`) because the registry is per-citizen process-
/// local, never crosses the Bus wire, and `Cell<u64>` counter
/// allocation is one increment per dispatch — three orders of
/// magnitude cheaper than `Uuid::new_v4`'s OS-RNG draw. Overflow
/// after 2^64 dispatches per citizen is structurally unreachable.
pub(crate) type InvocationId = u64;

/// SPEC 18 Phase 2 WS3-C.7c — shutdown-visible per-invocation reply
/// correlation.
///
/// Replaces the stack-local `EventReplyMeta` snapshot the pre-C.7c
/// inline dispatch path threaded through `Evaluator::current_event_meta`:
/// once C.7d spawns Class C bodies on a citizen-scoped `LocalSet`, the
/// citizen's shutdown drain (C.7f) needs to enumerate every still-in-
/// flight invocation whose `reply()` was never called and synthesize
/// the SPEC 18 §3.4 error reply for each. A stack-local field is
/// unreachable from outside the spawned task; an [`InvocationReplyHandle`]
/// is registered in the per-citizen [`InvocationReplyRegistry`] and
/// shared (`Rc`) between the running invocation and the registry's
/// snapshot enumerator.
///
/// **`Rc` (not `Arc`).** `BusHandler` is `!Send` by design — the whole
/// invocation model runs on a single-thread `LocalSet`. Switching to
/// `Arc` would imply a `Send`/`Sync` direction the trait does not
/// promise and the runtime does not need.
///
/// **`Cell<ReplyState>` for the reply gate.** Single-thread `LocalSet`
/// execution means no cross-thread race; `Cell` is the cheapest
/// interior mutability primitive that suffices. The three-state
/// machine (`Unanswered → Replying → Answered`, with rollback to
/// `Unanswered` on a failed wire send) closes the cooperative-yield
/// window where two `reply_once` callers could both see
/// `Unanswered`, both reach `handler.reply(...).await`, and both
/// fire duplicate wire replies. Codex C.7c R1 MAJOR: a plain
/// `Cell<bool>` pre-await flip prevents duplicates but loses
/// post-failure retry — the `Replying` intermediate state preserves
/// both invariants without `AtomicBool`'s false implication of
/// cross-thread sharing.
///
/// **Snapshotted handler.** The `Rc<dyn BusHandler>` is captured at
/// registration time so a script that re-wires `globals.bus_handler`
/// mid-dispatch (a hypothetical reload) cannot change the wire path
/// the original requester's reply travels back along. `Option` because
/// a citizen running without Bus wiring (e.g. unit tests) still goes
/// through the registry; `reply_once` surfaces the canonical
/// "Bus not available" error rather than registering nothing and
/// turning `reply()` into "outside on handler" by accident.
pub(crate) struct InvocationReplyHandle {
    /// Per-citizen monotonically increasing id. The deregistration key
    /// the RAII [`InvocationRegistration`] guard uses on drop. Held
    /// for the registry's bookkeeping but never read off the handle
    /// itself (the guard captures its own copy); kept here so future
    /// debug-formatting of a snapshot can report which invocation a
    /// stuck reply belonged to (C.7f shutdown drain).
    #[allow(dead_code)]
    id: InvocationId,
    /// The requester — becomes the response `to`. Best-effort and
    /// cosmetic: the Cosmix transport correlates a reply back to its
    /// caller by `id` (noded's `pending_responses` map), NOT by `to`.
    /// Routinely **empty** for a correlated request — noded strips the
    /// wire `from` for anonymous callers (the normal CLI / MCP-bridge
    /// case) as its SPEC 12 §15.5 impersonation defense
    /// (`canonicalize_routed_from`). An empty `from` therefore does NOT
    /// mean "no one to answer"; `reply_once` still fires (matching the
    /// Rust `NodedClient::respond_parts` path, which sets `to=from` but
    /// never refuses on an empty `from`). The real "no caller" signal is
    /// `is_request == false` (a topic broadcast), gated separately.
    from: String,
    /// Echoed back as the response `command` for caller-side matching.
    command: String,
    /// Correlation id; `None` for an id-less inbound.
    correlation_id: Option<String>,
    /// Whether the inbound was `type=request`. Only a request expects
    /// a reply; `reply_once` on a non-request errors (defensive check
    /// inside `reply_once` per Codex C.7c R1 MINOR — C.7f's shutdown
    /// drain enumerates handles from a registry snapshot and could
    /// otherwise wire-send a reply to a topic delivery that has no
    /// caller to receive it).
    is_request: bool,
    /// Reply-once state machine. See [`ReplyState`] for the three
    /// transitions; the `Replying` intermediate state closes the
    /// cooperative-yield duplicate-reply window Codex C.7c R1 MAJOR
    /// flagged.
    state: Cell<ReplyState>,
    /// Bus handler snapshotted at registration. `None` when no
    /// `bus_handler` was wired; `reply_once` returns the
    /// "Bus not available" error in that case.
    handler: Option<Rc<dyn BusHandler>>,
}

/// SPEC 18 Phase 2 WS3-C.7c R1 — reply-once state machine.
///
/// Three-state Cell-protected gate around the wire reply:
///
/// - **`Unanswered`** — initial. `reply_once` may proceed.
/// - **`Replying`** — a `reply_once` call has read `Unanswered`,
///   claimed the slot, and is currently `.await`ing
///   `handler.reply(...)`. Other `reply_once` callers see `Replying`
///   and return `Ok(false)` (the answer is in flight; firing again
///   would race a duplicate wire send through the cooperative
///   yield window).
/// - **`Answered`** — the wire send completed successfully. Further
///   `reply_once` calls return `Ok(false)`. Terminal in the normal
///   path.
///
/// A failed wire send rolls back `Replying → Unanswered` so a later
/// caller (a retry, the §3.4 synth path, or C.7f's shutdown drain)
/// can attempt again. Without the rollback, a single transport
/// hiccup would mark the request unanswerable, leaving the
/// requester blocked forever.
///
/// A `Replying` state at C.7f shutdown time means a task was mid-
/// reply when the citizen began draining. Two-phase semantics
/// (Codex C.7c R2 MINOR — pin these now so C.7f does not re-derive
/// them):
///
/// - **During grace** (the `CLASSC_DRAIN_GRACE` window): the owning
///   task is still scheduled, so its `reply_once` will resolve
///   itself — either `Replying → Answered` on a successful wire
///   send or `Replying → Unanswered` on a failure that the
///   `Unanswered` re-entry then catches. The drain skips `Replying`
///   handles in this phase to avoid racing the owner.
/// - **After cancellation** (grace expired, `LocalSet` aborted): no
///   running task is left to make either transition. A handle that
///   is still `Replying` is a cancellation victim — the original
///   `handler.reply(...).await` was interrupted mid-flight. C.7f
///   must treat any stranded `Replying` request handle as
///   un-answered and synthesize a §3.4 error reply through a force
///   path (a dedicated drain-only `synthesize_unanswered` that
///   bypasses the reply-once gate rather than going through
///   `reply_once`, since the gate would short-circuit on the
///   `Replying` state).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ReplyState {
    Unanswered,
    Replying,
    Answered,
}

impl InvocationReplyHandle {
    /// Mark this handle answered without a wire send. Returns `true`
    /// if this call won the gate (state was `Unanswered`), `false` if
    /// the gate was already claimed (`Replying` or `Answered`). The
    /// shutdown drain uses this to suppress a synthesis on a handle
    /// some other path already owns; the normal completion path goes
    /// through [`Self::reply_once`] instead.
    #[allow(dead_code)] // wired by C.7f shutdown drain
    pub(crate) fn mark_answered(&self) -> bool {
        if self.state.get() == ReplyState::Unanswered {
            self.state.set(ReplyState::Answered);
            true
        } else {
            false
        }
    }

    /// `true` iff state == [`ReplyState::Answered`]. A `Replying`
    /// state is *not* answered yet — the wire send is mid-flight and
    /// may still roll back to `Unanswered`.
    #[allow(dead_code)] // wired by C.7f shutdown drain
    pub(crate) fn is_answered(&self) -> bool {
        self.state.get() == ReplyState::Answered
    }

    /// True iff this is a request whose state is `Unanswered`. The
    /// §3.4 fault boundary filters on this — non-request topic
    /// deliveries have no caller to answer, an in-flight `Replying`
    /// is being handled by its owning task (the fault landed AFTER
    /// the wire send started, so the caller may already have a
    /// reply on the way), and an `Answered` request needs no second
    /// reply.
    pub(crate) fn is_unanswered_request(&self) -> bool {
        self.is_request && self.state.get() == ReplyState::Unanswered
    }

    /// True iff this is a request whose state is **not** `Answered`
    /// (i.e. `Unanswered` OR `Replying`). C.7f's shutdown drain
    /// filters on this AFTER the owning Class C task has been
    /// [`tokio::task::JoinHandle::abort`]-ed: a stranded `Replying`
    /// state means the in-flight `handler.reply(...).await` was
    /// cancelled mid-flight (between `state.set(Replying)` and the
    /// `Answered`/`Unanswered` post-await transition), so we cannot
    /// tell whether the wire send completed on the broker side. The
    /// design favours a possibly-duplicate reply over a silent
    /// caller hang (cf. type-doc on `ReplyState` — "treat any
    /// stranded `Replying` request handle as un-answered and
    /// synthesize a §3.4 error reply through a force path"). The
    /// §3.4 fault boundary's narrower [`Self::is_unanswered_request`]
    /// filter is deliberately distinct so a non-cancelled in-flight
    /// reply is never duplicated by the synth path.
    // Only the `tokio-sleep`-gated `drain_class_c_for_shutdown` path
    // consumes this; gate it to match so a no-tokio-sleep lib build (e.g.
    // cos consumers) doesn't see it as dead code.
    #[cfg(feature = "tokio-sleep")]
    pub(crate) fn is_pending_request(&self) -> bool {
        self.is_request && self.state.get() != ReplyState::Answered
    }

    /// SPEC 18 Phase 2 WS3-C.7f drain-only **force-path** wire reply.
    ///
    /// Bypasses the [`Self::reply_once`] gate (which short-circuits
    /// on `Replying` and `Answered`) so a stranded post-abort handle
    /// can be replied to. Sets `state.set(Answered)` unconditionally
    /// BEFORE the wire send so that a coincident
    /// [`Self::reply_once`] from a path the drain missed becomes a
    /// no-op (returns `Ok(false)`).
    ///
    /// **Safety / when to call.** Only call after the owning Class
    /// C task has been [`tokio::task::JoinHandle::abort`]-ed and
    /// best-effort joined — i.e. inside
    /// [`Evaluator::drain_class_c_for_shutdown`] after the survivor
    /// abort step, not from concurrent dispatch. Callers MUST filter
    /// on [`Self::is_pending_request`] first; this method does not
    /// re-check state.
    ///
    /// Returns `Ok(())` on a successful wire send, `Err` on a
    /// non-request handle (defensive — drain filter should have
    /// excluded these), missing handler, or transport failure. The
    /// state transition to `Answered` happens before the wire send
    /// so a transport error still prevents a future `reply_once`
    /// from re-firing (a drain-time transport failure is terminal —
    /// the citizen is shutting down anyway).
    #[cfg(feature = "tokio-sleep")]
    pub(crate) async fn synthesize_unanswered(&self, rc: u8, body: &str) -> MixResult<()> {
        if !self.is_request {
            return Err(MixError::RuntimeError {
                span: None,
                msg: "synthesize_unanswered on non-request handle".to_string(),
            });
        }
        let handler = self
            .handler
            .as_ref()
            .ok_or_else(|| MixError::RuntimeError {
                span: None,
                msg: "Bus not available (no handler registered)".to_string(),
            })?;
        self.state.set(ReplyState::Answered);
        handler
            .reply_shutdown_synth(
                &self.from,
                &self.command,
                self.correlation_id.as_deref(),
                rc,
                body,
            )
            .await
    }

    /// Fire the wire reply at most once. Returns `Ok(true)` if this
    /// call sent the reply, `Ok(false)` if another `reply_once` call
    /// already won the gate (`Replying` or `Answered` — double-reply
    /// is a script bug, not a hard error; the second `reply()`
    /// silently no-ops instead of generating a duplicate wire
    /// message). Returns `Err` on: a non-request handle (defensive,
    /// Codex C.7c R1 MINOR — C.7f's drain enumerates from a snapshot
    /// and must not wire-reply to a topic delivery), no handler
    /// snapshotted ("Bus not available"), or the underlying
    /// `handler.reply(...).await` failing.
    ///
    /// **State transitions.** Reads `state.get()`: if not
    /// `Unanswered`, returns `Ok(false)` immediately. Otherwise
    /// claims `Replying` *before* the `.await` (the Codex C.7c R1
    /// MAJOR fix — without the pre-await claim, two cooperatively-
    /// scheduled tasks could both read `Unanswered`, both reach the
    /// await, and both fire). On `Ok(())` from the wire send,
    /// transitions to `Answered`. On `Err`, rolls back to
    /// `Unanswered` so a later attempt can retry (a transient
    /// transport error must not strand the requester).
    pub(crate) async fn reply_once(&self, rc: u8, body: &str) -> MixResult<bool> {
        if !self.is_request {
            return Err(MixError::RuntimeError {
                span: None,
                msg: "reply_once on non-request handle (topic delivery has no \
                      caller to answer)"
                    .to_string(),
            });
        }
        match self.state.get() {
            ReplyState::Replying | ReplyState::Answered => return Ok(false),
            ReplyState::Unanswered => {}
        }
        let handler = self
            .handler
            .as_ref()
            .ok_or_else(|| MixError::RuntimeError {
                span: None,
                msg: "Bus not available (no handler registered)".to_string(),
            })?;
        self.state.set(ReplyState::Replying);
        match handler
            .reply(
                &self.from,
                &self.command,
                self.correlation_id.as_deref(),
                rc,
                body,
            )
            .await
        {
            Ok(()) => {
                self.state.set(ReplyState::Answered);
                Ok(true)
            }
            Err(e) => {
                self.state.set(ReplyState::Unanswered);
                Err(e)
            }
        }
    }
}

/// SPEC 18 Phase 2 WS3-C.7c — per-citizen registry of in-flight
/// invocation reply handles.
///
/// One instance per `EvaluatorGlobals`; shared by every per-invocation
/// `Evaluator` clone (they all hold the same `Rc<EvaluatorGlobals>`).
/// C.7c uses it for the inline-dispatch path; C.7d extends the same
/// registration shape to LocalSet-spawned Class C tasks; C.7f's
/// shutdown drain enumerates the live handles to synthesize unanswered
/// replies. Interior mutability so the explicit-completion guard's
/// `complete()` (and its no-op `Drop`) does not need to borrow
/// `EvaluatorGlobals` mutably.
pub(crate) struct InvocationReplyRegistry {
    next_id: Cell<InvocationId>,
    handles: RefCell<HashMap<InvocationId, Rc<InvocationReplyHandle>>>,
}

impl InvocationReplyRegistry {
    pub(crate) fn new() -> Self {
        Self {
            next_id: Cell::new(1),
            handles: RefCell::new(HashMap::new()),
        }
    }

    /// Register an invocation. Returns the shared handle and an
    /// explicit-completion guard. Caller stores the handle in
    /// `Evaluator::current_reply` (for `reply()` to find) and binds
    /// the guard on the stack for the dispatch's lifetime. The guard
    /// deregisters **only** via [`InvocationRegistration::complete`];
    /// its `Drop` is a no-op so cancellation victims stay enumerable
    /// by C.7f's shutdown drain (Codex C.7c R1 BLOCKER).
    pub(crate) fn register(
        self: &Rc<Self>,
        from: String,
        command: String,
        correlation_id: Option<String>,
        is_request: bool,
        handler: Option<Rc<dyn BusHandler>>,
    ) -> (Rc<InvocationReplyHandle>, InvocationRegistration) {
        let id = self.next_id.get();
        self.next_id.set(id.wrapping_add(1));
        let handle = Rc::new(InvocationReplyHandle {
            id,
            from,
            command,
            correlation_id,
            is_request,
            state: Cell::new(ReplyState::Unanswered),
            handler,
        });
        self.handles.borrow_mut().insert(id, Rc::clone(&handle));
        let guard = InvocationRegistration {
            id,
            registry: Rc::clone(self),
        };
        (handle, guard)
    }

    /// Snapshot of live handles — used by C.7f's shutdown drain to
    /// synthesize unanswered replies without holding the RefCell
    /// borrow across `.await`. The caller iterates the returned
    /// `Vec<Rc<...>>` after the borrow has dropped.
    #[cfg(feature = "tokio-sleep")]
    pub(crate) fn snapshot(&self) -> Vec<Rc<InvocationReplyHandle>> {
        self.handles.borrow().values().cloned().collect()
    }

    /// Test/observability hook — number of live registrations. Not
    /// used in production hot paths.
    #[allow(dead_code)] // wired by C.7f's shutdown drain + tests
    pub(crate) fn len(&self) -> usize {
        self.handles.borrow().len()
    }

    fn remove(&self, id: InvocationId) {
        self.handles.borrow_mut().remove(&id);
    }
}

/// SPEC 18 Phase 2 WS3-C.7c — explicit-completion registration token.
///
/// Bound on the dispatch stack for the duration of one invocation.
/// **Plain `Drop` is a no-op** — it does NOT remove the entry from
/// the registry, because dropping is the *cancellation* signal and
/// cancellation is exactly when C.7f's shutdown drain needs the
/// handle still registered so it can enumerate the unanswered
/// requests and synthesize the §3.4 error reply (Codex C.7c R1
/// BLOCKER — an auto-removing Drop deletes the entry before the
/// drain can see it).
///
/// Normal completion in `run_handlers_for` calls
/// [`Self::complete`] explicitly, which consumes the guard and
/// removes the entry. A long-lived citizen therefore does not leak
/// per-dispatch entries on the happy path; the registry only grows
/// when an in-flight task is being cancelled, and C.7f's drain
/// reclaims everything still registered at shutdown.
///
/// Synthesis on cancellation is NOT a Drop concern — `Drop` cannot
/// `.await` — it is owned by the C.7f shutdown drain (single async
/// pass over the registry snapshot after `LocalSet` cancellation).
pub(crate) struct InvocationRegistration {
    id: InvocationId,
    registry: Rc<InvocationReplyRegistry>,
}

impl InvocationRegistration {
    /// Mark this invocation completed and remove its handle from the
    /// registry. Called from `run_handlers_for` after the §3.4 fault
    /// boundary has had its chance to synthesize a reply for an
    /// unanswered request. Consumes the guard to make accidental
    /// double-completion a compile error.
    ///
    /// Does NOT `mem::forget(self)` — the `Drop` impl is a no-op, so
    /// the normal field drop is harmless and must run so the guard's
    /// `Rc<InvocationReplyRegistry>` clone decrements the registry
    /// refcount (Codex C.7c R2 MAJOR — leaking the Rc on every
    /// dispatch would prevent the registry from ever being freed and
    /// pin its `Cell`/`RefCell` allocations for the life of the
    /// process).
    pub(crate) fn complete(self) {
        self.registry.remove(self.id);
        // `self` falls out of scope here: Drop runs (no-op), then
        // the `id` Copy and `registry` Rc drop normally. The Rc
        // decrement is the load-bearing operation — without it the
        // strong count grows unboundedly across normal dispatches.
    }
}

impl Drop for InvocationRegistration {
    fn drop(&mut self) {
        // No-op by design — see type-level docs. Cancellation must
        // leave the entry so C.7f's shutdown drain can enumerate
        // it. Normal completion calls `complete()` instead.
    }
}

/// SPEC 18 Phase 2 WS3-C.7d — task id for the per-citizen Class C
/// task registry. Monotonic per citizen; only used inside the registry
/// for `JoinHandle` lookup/drain so a 64-bit counter never wraps in any
/// realistic citizen lifetime.
pub(crate) type TaskId = u64;

/// SPEC 18 Phase 2 WS3-C.7d — per-citizen registry of in-flight Class C
/// task handles.
///
/// One instance per `EvaluatorGlobals`; shared by every per-invocation
/// `Evaluator` clone via the same `Rc<EvaluatorGlobals>`. The registry
/// is *task-handle* indexed (cancellation, drain) — it complements the
/// reply-handle registry ([`InvocationReplyRegistry`]) which is
/// *invocation-state* indexed (synthesise unanswered §3.4 replies).
/// The two are intentionally separate: a single Class C dispatch
/// carries one entry in each, but with different lifetimes (the task
/// outlives the reply on a `reply()` early-return; the reply outlives
/// the task on a `JoinHandle::abort` cancellation that lands before
/// the §3.4 fault arm runs). One combined registry would conflate
/// those lifetimes and force a partial-drain edge case the split
/// avoids.
///
/// Interior mutability (`Cell` + `RefCell`) so the spawn-site and the
/// task-finalize path do not need to take `&mut EvaluatorGlobals`.
/// C.7f's shutdown drain calls [`Self::drain`] to abort still-live
/// tasks; the §3.4 unanswered-reply synthesis loop walks
/// [`InvocationReplyRegistry::snapshot`] separately.
pub(crate) struct ClassCTaskRegistry {
    next_id: Cell<TaskId>,
    tasks: RefCell<HashMap<TaskId, tokio::task::JoinHandle<()>>>,
}

impl ClassCTaskRegistry {
    pub(crate) fn new() -> Self {
        Self {
            next_id: Cell::new(1),
            tasks: RefCell::new(HashMap::new()),
        }
    }

    /// Allocate the next task id without storing a handle. Paired with
    /// [`Self::attach_handle`] so `dispatch_event`'s Class C arm can
    /// bake the id into the spawned closure (for self-removal on
    /// normal completion) BEFORE the `tokio::task::spawn_local` call —
    /// avoiding an `Rc<Cell<Option<TaskId>>>` round-trip through the
    /// closure.
    pub(crate) fn reserve_id(&self) -> TaskId {
        let id = self.next_id.get();
        self.next_id.set(id.wrapping_add(1));
        id
    }

    /// Bind a previously-reserved [`TaskId`] to its
    /// [`tokio::task::JoinHandle`]. Called synchronously after
    /// `spawn_local` returns — the LocalSet cannot poll the spawned
    /// task until the next await yields control, so the handle is
    /// always attached before the task body runs. (If `attach_handle`
    /// is somehow skipped — e.g. a panic between spawn and attach —
    /// the spawned task's self-`remove` call is a HashMap miss, a
    /// no-op; no entry leaks.)
    pub(crate) fn attach_handle(&self, id: TaskId, handle: tokio::task::JoinHandle<()>) {
        self.tasks.borrow_mut().insert(id, handle);
    }

    /// Remove a task entry by id without cancelling. Called from the
    /// spawned task's finalize step after the chain body returns
    /// normally so the registry only grows while tasks are in flight.
    pub(crate) fn remove(&self, id: TaskId) {
        self.tasks.borrow_mut().remove(&id);
    }

    /// Drain every live task handle. Returns the handles so the caller
    /// (C.7f shutdown drain) can `.abort()` them and then `.await` the
    /// joins under the grace timer. Empties the registry in one borrow
    /// — no `.await` is held against the `RefCell`.
    #[cfg(feature = "tokio-sleep")]
    pub(crate) fn drain(&self) -> Vec<tokio::task::JoinHandle<()>> {
        let mut map = self.tasks.borrow_mut();
        std::mem::take(&mut *map).into_values().collect()
    }

    /// Test/observability hook — number of live Class C tasks.
    pub(crate) fn len(&self) -> usize {
        self.tasks.borrow().len()
    }
}

/// SPEC 18 Phase 2 WS3-C.7f — default bounded grace for the Class C
/// in-flight-handler drain step of the §3.5 graceful shutdown
/// sequence.
///
/// 5 s is the Phase-2 default: long enough for a Class C handler that
/// is mid-`send` to a peer service (the canonical reason to be Class
/// C) to finish its round-trip, short enough that systemd's default
/// stop timeout (90 s) plus the §3.5 deregister grace (5 s) leaves
/// generous headroom. Operator override (env / config surface) is
/// deferred; mint one when Phase-2 deployment evidence shows a real
/// citizen needs it.
///
/// Live, default-arg call site is `cosmix-mix::run_serve` (`mix
/// --serve`); library tests pass their own (typically much shorter)
/// `Duration` directly.
#[cfg(feature = "tokio-sleep")]
pub const CLASSC_DRAIN_GRACE: std::time::Duration = std::time::Duration::from_secs(5);

/// SPEC 18 Phase 2 WS3-C.7f — `rc` value for synth'd shutdown replies.
///
/// Uses the existing §3.4 fault-domain `rc=2` rather than minting a
/// new value. Callers distinguish "handler crashed" (§3.4 fault arm)
/// from "service shutting down mid-handler" (C.7f drain synth) by the
/// reply body, not the rc — minting a new rc here would be a SPEC 18
/// §3.4 amendment, which Phase 2 deliberately does not perform.
#[cfg(feature = "tokio-sleep")]
pub const SHUTDOWN_SYNTH_RC: u8 = 2;

/// SPEC 18 Phase 2 WS3-C.7f — body for synth'd shutdown replies.
///
/// Stable, non-sensitive, identifies the cause unambiguously. Does
/// not embed the service name — the caller's correlation already
/// resolves the address (the broker route returned this reply on the
/// `&lt;svc&gt;.&lt;cmd&gt;` namespace it dispatched against).
#[cfg(feature = "tokio-sleep")]
pub const SHUTDOWN_SYNTH_BODY: &str = "service shutting down; handler cancelled before replying";

/// SPEC 18 Phase 2 WS3-C.7f — accounting for one drain pass.
///
/// Returned by [`Evaluator::drain_class_c_for_shutdown`] so the
/// caller (typically `cosmix-mix::run_serve`) can log a structured
/// summary line of what the drain actually did. Used in tests to
/// assert post-conditions on each drain branch (clean drain, grace
/// expiry + abort + synth, transport-drop suppression).
///
/// `initial_tasks` and `initial_pending` are observation-only —
/// they don't sum with the action counts because some tasks both
/// drain clean AND register a pending reply that completes cleanly
/// inside the grace window; we track action counts (drained_clean,
/// aborted, synth_sent, synth_failed, synth_skipped_no_socket)
/// separately so a transport-drop branch with `initial_pending &gt; 0`
/// + `synth_skipped_no_socket == initial_pending` is recognisable.
#[cfg(feature = "tokio-sleep")]
#[derive(Debug, Default, Clone, Copy)]
pub struct ClassCDrainOutcome {
    /// Live Class C task handles at drain entry.
    pub initial_tasks: usize,
    /// Pending request handles (Unanswered + Replying) at drain entry.
    pub initial_pending: usize,
    /// Task handles that resolved naturally within the grace window.
    pub drained_clean: usize,
    /// Survivor handles that exceeded grace and were
    /// [`tokio::task::JoinHandle::abort`]-ed.
    pub aborted: usize,
    /// Pending-request handles whose synth reply was wired
    /// successfully through the broker.
    pub synth_sent: usize,
    /// Pending-request handles whose synth reply attempt returned an
    /// `Err` from the underlying `BusHandler::reply` (transport error
    /// mid-drain, or `Bus not available`).
    pub synth_failed: usize,
    /// Pending-request handles skipped because the caller set
    /// `allow_synth_replies = false` (transport-drop branch: the
    /// socket is already gone, no caller channel to reply on).
    pub synth_skipped_no_socket: usize,
}

/// A single registered handler body plus its WS1 class flag.
///
/// SPEC 18 Phase 2 WS2: the handler registry pairs each body
/// with `is_async` so WS3-C can compute the chain class
/// (Class C if ANY matching handler is `async`, else Class S).
/// Until WS3-C lands, dispatch still runs every chain Class S
/// inline; the field is consumed at registration but not yet at
/// dispatch.
#[derive(Debug, Clone)]
pub(crate) struct HandlerEntry {
    /// The handler body. `Arc<Vec<Stmt>>` lets dispatch clone a handle
    /// to the body without copying the AST.
    body: Arc<Vec<Stmt>>,
    /// Captured at parse time from `StmtKind::On { is_async, .. }`.
    /// `true` means the source declared `on <cmd> async` (Class C);
    /// `false` is a plain `on <cmd>` (Class S).
    is_async: bool,
}

/// Boxed future returned by `BusHandler` methods. `!Send` because the
/// evaluator stores the handler behind `Rc<RefCell<…>>` and runs on a
/// single-thread runtime.
pub type BusFuture<'a, T> = Pin<Box<dyn Future<Output = T> + 'a>>;

/// Boxed future returned by [`DbHandler`] methods. Same shape as
/// [`BusFuture`] — `!Send`, single-thread runtime.
pub type DbFuture<'a, T> = Pin<Box<dyn Future<Output = T> + 'a>>;

/// Host database seam: a scoped relational store injected into the
/// evaluator so Mix scripts can `db_query`/`db_exec` without ever
/// holding a connection. `None` by default (the `db_*` builtins then
/// raise a catchable error); set per-evaluator via
/// [`Evaluator::set_db_handler`].
///
/// Methods are **async** (returning a [`DbFuture`]) on purpose: an
/// in-process embedder (e.g. `cosmix-webd`'s trusted handler) binds the
/// handler to a local `Connection` and resolves immediately, while an
/// out-of-process worker can implement the same trait by RPC-ing the
/// query back to its host — **the Mix script is identical either way**.
/// Gated by [`crate::builtins::CapabilityClass::Db`], so a policy that
/// doesn't allow `Db` denies `db_query`/`db_exec` before the handler is
/// consulted.
///
/// Contract:
/// * `query(sql, params)` → a `Value::List` of `Value::Map` rows
///   (column name → value). Intended for `SELECT`.
/// * `exec(sql, params)` → a `Value::Map` `{ affected, last_insert_id }`.
///   Intended for `INSERT`/`UPDATE`/`DELETE`/DDL.
///
/// `params` are positional bind values for `?1`, `?2`, … placeholders;
/// the implementor maps Mix `Value`s to the store's parameter types and
/// MUST use bound parameters (never string-interpolate) to avoid
/// injection.
pub trait DbHandler {
    fn query<'a>(&'a self, sql: &'a str, params: &'a [Value]) -> DbFuture<'a, MixResult<Value>>;

    fn exec<'a>(&'a self, sql: &'a str, params: &'a [Value]) -> DbFuture<'a, MixResult<Value>>;
}

/// Boxed future returned by [`JmapHandler`] methods. Same shape as
/// [`DbFuture`] — `!Send`, single-thread runtime.
pub type JmapFuture<'a, T> = Pin<Box<dyn Future<Output = T> + 'a>>;

/// One JMAP method invocation, as the `jmap()` builtin hands it to a
/// [`JmapHandler`]: the method name (e.g. `"Contact/query"`), its
/// arguments (a `Value::Map`), and the caller-chosen `call_id` used to
/// correlate the response and resolve RFC 8620 §3.7 ResultReferences
/// (`#field` back-references) within a batch.
#[derive(Clone, Debug)]
pub struct JmapCall {
    pub method: String,
    pub args: Value,
    pub call_id: String,
}

/// Host JMAP seam: a scoped JMAP endpoint injected into the evaluator so
/// Mix scripts can `jmap(...)` without ever holding a network socket or
/// credentials. `None` by default (the `jmap` builtin then raises a
/// catchable error); set per-evaluator via
/// [`Evaluator::set_jmap_handler`].
///
/// Async (returning a [`JmapFuture`]) for the same reason as
/// [`DbHandler`]: an in-process embedder (e.g. `cosmix-webd`) resolves it
/// over its HTTP client to the vhost's configured maild upstream, while
/// an out-of-process worker could RPC it — **the Mix script is identical
/// either way**. Gated by [`crate::builtins::CapabilityClass::Jmap`], so
/// a policy that doesn't allow `Jmap` denies `jmap` before the handler is
/// consulted, and the network reach is *only* the embedder's configured
/// upstream — the script never names a host.
///
/// Contract: `request(calls)` runs the `calls` as one JMAP request and
/// returns the `methodResponses` as a `Value::List` of 3-element
/// `Value::List` triples `[method, result, call_id]`, in request order.
/// A JMAP **method-level** failure is a normal `["error", {…}, call_id]`
/// triple in that list (NOT an `Err`); the implementor reserves `Err`
/// for transport/protocol failures (upstream unreachable, malformed
/// response envelope, auth/credential failure). The `jmap()` builtin
/// applies the single-vs-batch policy on top of this uniform shape.
pub trait JmapHandler {
    fn request<'a>(&'a self, calls: &'a [JmapCall]) -> JmapFuture<'a, MixResult<Value>>;

    /// Upload `bytes` as a JMAP blob (RFC 8620 §6.1) with the given
    /// `content_type`, returning the minted `blobId` as a
    /// `Value::String`. Backs the `jmap_upload(body, content_type)`
    /// builtin — the *compose* half of the mail seam: maild's `Email/set`
    /// create is blob-only, so a handler that wants to send must upload
    /// the raw RFC822 message first, then reference the returned `blobId`
    /// from `Email/set { create }`.
    ///
    /// Same error discipline as [`request`](JmapHandler::request): `Err`
    /// is reserved for transport/protocol/auth failures (upstream
    /// unreachable, non-2xx, malformed response); a successful upload
    /// yields the `blobId` string. The embedder bounds *where* the upload
    /// goes (its one configured upstream); `CapabilityClass::Jmap` gates
    /// *whether* the script may, exactly as for `request`.
    ///
    /// Implementors that forward `content_type` to an HTTP header should
    /// reject control bytes (CR/LF/NUL) with an `Err` rather than letting
    /// the transport layer fail opaquely — a script may pass a
    /// user-derived `content_type`.
    fn upload<'a>(
        &'a self,
        bytes: &'a [u8],
        content_type: &'a str,
    ) -> JmapFuture<'a, MixResult<Value>>;
}

/// [`BusCallFuture`] — `!Send`, single-thread runtime (same shape as
/// [`JmapFuture`]).
pub type BusCallFuture<'a, T> = Pin<Box<dyn Future<Output = T> + 'a>>;

/// The host-injected channel backing the `bus_call(verb, args)` builtin —
/// a *scoped, mediated, DELEGATED* Bus control-plane call, injected via
/// [`Evaluator::set_bus_call_handler`].
///
/// This is deliberately NOT [`BusHandler`] (`send`/`emit`): a sandboxed web
/// handler must not get the raw broker. Instead the embedder (e.g.
/// `cosmix-webd`) bounds **which verbs** are reachable (a per-route
/// exact-verb allowlist) and injects the **delegation envelope** (the
/// authenticated actor, vhost, route_id, csrf flag) from TRUSTED request
/// state — the Mix script supplies only `verb` + `args` and can name no
/// destination/peer/actor. Gated by
/// [`crate::builtins::CapabilityClass::Bus`], so a policy that doesn't allow
/// `Bus` denies `bus_call` before the handler is consulted.
///
/// Contract: `call(verb, args)` runs the verb with `args` (a JSON-encodable
/// Mix value) and returns the reply as a [`Value`] on success. `Err` is
/// reserved for a refusal or failure the script should treat as an
/// exception — the verb is not in the route's allowlist, the broker is
/// unreachable, or the verb replied with an error rc. (A handler that wants
/// to render an error gracefully wraps the call in `try`/`catch`.) The
/// implementor never lets the script choose the destination or forge the
/// envelope.
pub trait BusCallHandler {
    fn call<'a>(&'a self, verb: &'a str, args: &'a Value) -> BusCallFuture<'a, MixResult<Value>>;
}

/// Walk a Mix value (a `jmap` call's args) and return the name of the
/// first variant that has **no JSON representation** — a `Function` or
/// raw `Bytes`. Such values would be silently rewritten to `null` by the
/// JSON encoder; for a *protocol* builtin that's a footgun (a dropped
/// arg the caller never sees), so `jmap()` rejects them up front instead.
/// `None` ⇒ the value is fully JSON-encodable.
fn jmap_unjsonable(v: &Value) -> Option<&'static str> {
    match v {
        Value::Function(_) => Some("function"),
        Value::Bytes(_) => Some("bytes"),
        Value::Buffer(_) => Some("buffer"),
        Value::List(items) => items.iter().find_map(jmap_unjsonable),
        Value::Map(m) => m.values().find_map(jmap_unjsonable),
        _ => None,
    }
}

/// Classification of one line for the [`ShellHandler`] fallback used
/// by `source`. Mirrors the REPL's `classify_input` triage but stays
/// strict: an unparseable line is an error from `classify`, not a
/// silent skip.
pub enum ShellLineKind {
    /// Line is a shell command. Caller will invoke
    /// [`ShellHandler::execute_shell`] to actually run it.
    Shell,
    /// Line is Mix code. Caller parses and executes it in the source
    /// caller's scope.
    Mix,
    /// Line is empty / whitespace / comment — skip.
    Empty,
}

/// Result of a shell line executed through a host handler.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ShellExecResult {
    Status(i32),
    ExitRequested(i32),
}

/// Variable resolver passed to [`ShellHandler::execute_shell`] so
/// that handler implementations can apply REPL-equivalent `$VAR`
/// expansion against the evaluator's live scope before parsing the
/// pipeline. The evaluator implements this over scope + process env
/// (same precedence as `cosmix-mix::shell::expand_variables`); the
/// handler stays in the binary crate but doesn't need to depend on
/// the evaluator struct directly.
pub trait ShellVarResolver {
    /// Resolve `name` to its string form for `$VAR`/`${VAR}`
    /// expansion. Scope hits win over process env (REPL parity);
    /// missing returns `None` so the caller can decide whether to
    /// expand to the empty string (current REPL behavior) or
    /// preserve the literal `$name` (current REPL behavior is
    /// expand-to-empty via env fallback).
    fn resolve(&self, name: &str) -> Option<String>;

    /// Run a `$(...)` command substitution found while tokenizing a
    /// shell-dispatch word, returning the captured stdout with trailing
    /// newlines stripped (bash semantics). `None` SUPPRESSES the
    /// substitution — the segment contributes nothing.
    ///
    /// The default is `None` so command substitution NEVER executes
    /// unless a resolver opts in: structural resolvers (classify /
    /// validate / fuzz), and any third-party `ShellVarResolver`, stay
    /// spawn-free by construction. The live `Evaluator` overrides this to
    /// execute `cmd` via `/bin/sh -c` (the same shell `run()`/`run_rc()`
    /// use), so the REPL, `mix -c`, the login shell, and sourced files —
    /// all of which resolve against an `Evaluator` — get `$(...)`. The
    /// captured text is pure DATA in phase 2: it is concatenated into the
    /// word's value and never re-lexed for operators, so a substitution
    /// can no more inject structure than a `$VAR` value can.
    fn command_subst(&self, _cmd: &str) -> Option<String> {
        None
    }
}

/// Per-line shell classifier+executor for files loaded via `source`.
///
/// `exec_source` first tries to parse the whole file as Mix
/// (preserving multi-line blocks). If that parse fails AND a
/// `ShellHandler` is registered, the evaluator runs a two-phase
/// fallback:
///
///   1. Walk every non-blank line tracking literal alias
///      definitions from earlier Mix lines. The probe enters Phase 2
///      only when it finds an **unambiguous** shell line: classified
///      as `Shell` AND not standalone-parseable as Mix. A bareword
///      that exists on `PATH` but is also a valid Mix identifier
///      (e.g. `ls`) is ambiguous — Phase 1 leaves it for Phase 2 to
///      handle, and a pure-Mix file containing such a word followed
///      by a syntax error propagates the original parse error
///      unchanged.
///   2. Otherwise the file is treated as a `.mixrc`-style mixed
///      file: walk the classifications in order, calling
///      [`execute_shell`] on Shell lines and re-parsing Mix lines
///      into the caller's scope.
///
/// The handler implementation lives in the `cosmix-mix` binary crate
/// (over the same shell primitives the REPL uses) and is registered
/// via [`Evaluator::set_shell_handler`] during REPL / script-runner
/// startup. Daemon/serve modes leave it unset and keep strict
/// pure-Mix `source` semantics.
pub trait ShellHandler {
    /// Classify `line` without executing. `aliases` is the
    /// evaluator's live alias map so aliases defined earlier in the
    /// file are visible to later lines. Returning `Err` here aborts
    /// the source with that message (REPL's "print and continue"
    /// leniency is intentionally NOT applied to sourced files).
    fn classify(
        &self,
        line: &str,
        aliases: &HashMap<String, String>,
    ) -> Result<ShellLineKind, String>;

    /// Execute a previously-classified `Shell` line. Returns the
    /// process exit code so the evaluator can publish it as the
    /// `status` scope variable (matching REPL semantics — see
    /// `cosmix-mix/src/repl.rs` ExternalCommand arm).
    ///
    /// `vars` lets the handler apply `$VAR`/`${VAR}` expansion
    /// against the evaluator's live scope before parsing the
    /// pipeline, matching the REPL's `ExternalCommand` arm which
    /// runs `shell::expand_variables(&input, &eval)` before
    /// `exec::parse_pipeline`.
    fn execute_shell(
        &self,
        line: &str,
        aliases: &HashMap<String, String>,
        vars: &dyn ShellVarResolver,
    ) -> Result<ShellExecResult, String>;

    /// Execute a shell line and return the post-alias-expansion head of each
    /// command-list branch that control flow actually selected. The default
    /// preserves compatibility for handlers that do not expose this detail.
    fn execute_shell_tracked(
        &self,
        line: &str,
        aliases: &HashMap<String, String>,
        vars: &dyn ShellVarResolver,
    ) -> Result<(ShellExecResult, Vec<String>), String> {
        self.execute_shell(line, aliases, vars)
            .map(|result| (result, Vec::new()))
    }

    /// Static dry-run validation of a `Shell`-classified line — no
    /// process spawn, no env mutation, no variable expansion. Used
    /// by tools like `mix self check` to surface lines that
    /// [`execute_shell`] would reject at run time (unsupported REPL
    /// builtins, malformed pipelines) instead of letting them slip
    /// through whole-file validation. A return of `Ok(())` means
    /// "this line would dispatch cleanly assuming runtime variable
    /// values exist"; `Err` means definitively broken.
    fn validate_shell(&self, line: &str, aliases: &HashMap<String, String>) -> Result<(), String>;
}

/// `$rc` value for a delivered+accepted `send` (RPC reply in `$result`).
pub const RC_OK: i32 = 0;
/// `$rc` for a transport failure after a route/handler existed (a lost broker
/// connection, a handler transport error). `$result` carries the error text.
pub const RC_TRANSPORT: i32 = -1;
/// `$rc` for a per-send `timeout=` budget exceeded. `$result` is the
/// `timeout: send to <target> exceeded <n>s` message.
pub const RC_TIMEOUT: i32 = -2;
/// `$rc` for Bus unavailable BEFORE delivery — no evaluator Bus handler, or a
/// broker that was never present (a bare host). Non-fatal: the statement
/// returns Nil and the script continues, but `$rc == 0` now strictly means
/// "delivered", never "no-op on a broker-less host". `$result` explains.
pub const RC_UNAVAILABLE: i32 = -3;
// A broker/app-level error is `rc >= 10` (cosmix_lib_bus::RC_ERROR); `$result`
// carries the daemon's error body. Those come straight from the handler.

/// Handler for Bus IPC operations. Implemented by the mix-bus crate.
///
/// All methods return boxed futures since the trait object is stored in the
/// Evaluator which is `!Send` (uses Rc/RefCell).
pub trait BusHandler {
    /// Send a command and wait for reply. Returns `(rc, result_value)`.
    ///
    /// `rc` is a SIGNED status (the `$rc` contract): `0` = delivered+accepted,
    /// `1..9` = delivered with a warning / sub-error status (e.g. `5` =
    /// `RC_WARNING`) — still a non-fatal success, `result` is the reply; the
    /// exact peer rc is preserved (never flattened to 10); `>= 10` = broker/app
    /// error (`result` is the error body); and a NEGATIVE band the evaluator
    /// also produces on its own transport/timeout/no-handler paths — see
    /// [`RC_OK`]/[`RC_TRANSPORT`]/[`RC_TIMEOUT`]/[`RC_UNAVAILABLE`]. A handler
    /// returns `RC_UNAVAILABLE` for its own "never had a broker" degrade (was a
    /// false `(0, Nil)` success); a hard transport failure is an `Err`, which
    /// the evaluator maps to `RC_TRANSPORT`.
    fn send<'a>(
        &'a self,
        target: &'a str,
        command: &'a str,
        args: &'a Value,
    ) -> BusFuture<'a, MixResult<(i32, Value)>>;

    /// Fire-and-forget send. Returns immediately after dispatching.
    fn emit<'a>(
        &'a self,
        target: &'a str,
        command: &'a str,
        args: &'a Value,
    ) -> BusFuture<'a, MixResult<()>>;

    /// Check if a port exists and is reachable.
    fn port_exists<'a>(&'a self, target: &'a str) -> BusFuture<'a, MixResult<bool>>;

    /// Await the next incoming Bus message for this client.
    ///
    /// Returns `None` when the underlying connection is closed, when no
    /// incoming stream is available (e.g. the handler is outbound-only), or
    /// when the handler cannot initialize its transport. Callers should treat
    /// `None` as a permanent signal to stop draining — repeated calls after
    /// `None` are allowed but will keep returning `None`.
    ///
    /// Implementations must be safe to call repeatedly from a single-thread
    /// evaluator task. Concurrent/reentrant calls from the same thread are
    /// not supported.
    fn next_incoming<'a>(&'a self) -> BusFuture<'a, Option<IncomingEvent>>;

    /// Register (or re-register) the client as a named service on the broker.
    /// This enables the broker to route messages addressed to `name` back to
    /// this connection, and sets the `from` header on future outbound messages.
    fn register_as<'a>(&'a self, _name: &'a str) -> BusFuture<'a, MixResult<()>> {
        Box::pin(async { Ok(()) })
    }

    /// Subscribe to a Ch03 topic (SPEC 18 WS2; the `subscribe(name)`
    /// builtin routes here). A serve-mode handler backed by a
    /// supervised client records the topic transactionally for §3.3
    /// reconnect replay.
    ///
    /// Unlike [`register_as`](Self::register_as), the default is a hard
    /// **error**, not a silent `Ok`: a handler that cannot subscribe
    /// must say so. A no-op default would be the silent-no-op
    /// partial-truth bug — the script believes it is subscribed while
    /// no broker subscription exists. Outbound-only / non-Bus handlers
    /// therefore fail loudly here by design.
    fn subscribe_topic<'a>(&'a self, _name: &'a str) -> BusFuture<'a, MixResult<()>> {
        Box::pin(async {
            Err(MixError::RuntimeError {
                span: None,
                msg: "subscribe() requires a Bus connection (handler does not support topics)"
                    .to_string(),
            })
        })
    }

    /// Unsubscribe from a Ch03 topic (SPEC 18 WS2; the
    /// `unsubscribe(name)` builtin routes here). Default errors for the
    /// same reason as [`subscribe_topic`](Self::subscribe_topic).
    fn unsubscribe_topic<'a>(&'a self, _name: &'a str) -> BusFuture<'a, MixResult<()>> {
        Box::pin(async {
            Err(MixError::RuntimeError {
                span: None,
                msg: "unsubscribe() requires a Bus connection (handler does not support topics)"
                    .to_string(),
            })
        })
    }

    /// Answer the request an `on` handler is currently servicing (SPEC 18
    /// WS-R; the `reply(body)` / `reply(rc, body)` builtin routes here).
    /// `to` is the requester (the inbound `from`), `command` echoes the
    /// inbound command, `id` correlates the response, `rc` is the Bus
    /// return code, `body` the payload. The host adapts these neutral
    /// parts onto its transport's correlated-response primitive (for
    /// `cosmix-mix`, `cosmix-lib-client::NodedClient::respond_parts`).
    ///
    /// No correlation is invented here: the evaluator supplies the parts
    /// it captured from the in-flight [`IncomingEvent`]; this method only
    /// transmits them. Like [`subscribe_topic`](Self::subscribe_topic),
    /// the default is a hard **error**, not a silent `Ok`: a dropped
    /// reply is the silent-no-op partial-truth bug in its most acute
    /// form — the script believes it answered while the requester blocks
    /// forever. Outbound-only / non-Bus handlers fail loudly here by
    /// design.
    fn reply<'a>(
        &'a self,
        _to: &'a str,
        _command: &'a str,
        _id: Option<&'a str>,
        _rc: u8,
        _body: &'a str,
    ) -> BusFuture<'a, MixResult<()>> {
        Box::pin(async {
            Err(MixError::RuntimeError {
                span: None,
                msg: "reply() requires a Bus connection (handler does not support replies)"
                    .to_string(),
            })
        })
    }

    /// SPEC 18 Phase 2 WS3-C.7f — answer a pending request from the
    /// shutdown drain's Phase 3 synth path. Identical contract to
    /// [`reply`] except that supervised-transport handlers MUST
    /// route this through a path that bypasses any "shutting down"
    /// outbound gate, because the drain runs *after* the citizen has
    /// transitioned into the shutting-down state by design.
    ///
    /// The default implementation delegates to [`reply`] — handlers
    /// that have no shutdown-gated outbound (test handlers, the
    /// transient [`MixBusHandler`]) need no override. The
    /// `MixServeHandler` in `cosmix-mix` *does* override, routing
    /// through `SupervisedClient::respond_parts_shutdown_synth` so a
    /// post-deregister synth reply can still reach a caller over the
    /// still-live WS socket.
    fn reply_shutdown_synth<'a>(
        &'a self,
        to: &'a str,
        command: &'a str,
        id: Option<&'a str>,
        rc: u8,
        body: &'a str,
    ) -> BusFuture<'a, MixResult<()>> {
        self.reply(to, command, id, rc, body)
    }

    /// Drop any cached broker connection so the *next* outbound call
    /// re-dials (SPEC 18 §3.3 acceptance affordance; the
    /// `bus_reconnect()` builtin routes here). A `mix --serve` citizen
    /// recovers from a `cosmix-noded` bounce on its own via the
    /// supervised client, but a plain `mix` *driver* (the WS8 acceptance
    /// harness) caches an anonymous broker client with no re-dial — when
    /// it induces the bounce it must be able to reset its own client.
    ///
    /// Unlike [`subscribe_topic`](Self::subscribe_topic) /
    /// [`reply`](Self::reply), the default is a no-op `Ok(())`, NOT a
    /// hard error. There is no silent-no-op partial-truth here: a
    /// handler with nothing cached to drop (outbound-only, socket-per-
    /// call, non-broker) genuinely has nothing to do, the next call
    /// dials fresh regardless, and "no connection to reset" is exactly
    /// the post-condition the caller asked for — so reporting success is
    /// honest, not a swallowed failure.
    fn reconnect<'a>(&'a self) -> BusFuture<'a, MixResult<()>> {
        Box::pin(async { Ok(()) })
    }
}

/// SPEC 18 WS4 — runtime-reserved Ch07 L0+ surface for `mix --serve`
/// citizens.
///
/// Installed by the serve entrypoint (`cosmix-mix` `run_serve` →
/// [`Evaluator::set_serve_runtime`]); **absent** for plain `run_source`
/// scripts — a transient client is not a registered service and owes no
/// SPEC 07 §9 conformance, so it falls straight through to author
/// handlers with no reserved-verb interception.
///
/// Consulted by [`Evaluator::run_event_pump`] **before**
/// [`Evaluator::dispatch_event`]: if a verb is reserved the runtime
/// answers it and the pump `continue`s, so an author `on HELP do` /
/// `on <svc>.props.get do` can never shadow the runtime verb
/// (SPEC 18 DECIDED §7-Q4 — *runtime wins*; an author override would
/// let §9(d)/(e) conformance be silently broken). Authors extend the
/// data surfaced through `INFO`/props via the lifecycle tree, they do
/// not replace the verbs.
///
/// The trait is intentionally `&str`/`String`-only (no `serde_json`,
/// no `cosmix_props`): the prop-tree model and JSON encoding live in
/// the citizen layer (`cosmix-mix`, which owns the citizen runtime),
/// keeping `cosmix-lib-mix` core feature-clean per the core/citizen
/// split — the same shape as [`BusHandler`].
pub trait ServeRuntime {
    /// If `command` is a runtime-reserved verb, service it and return
    /// the reply parts plus whether the runtime must now begin graceful
    /// shutdown. `None` ⇒ not a reserved verb; the pump dispatches to
    /// author handlers as normal.
    ///
    /// - `args_header` — the inbound `args` header (a JSON object string
    ///   when the request was a `send name cmd k=v` RPC); `props.get` /
    ///   `props.describe` read `path` from it (falling back to
    ///   `req_body`), mirroring the indexd reference dispatch.
    /// - `req_body` — the inbound request body.
    /// - `handler_commands` — the citizen's registered `on` command set,
    ///   surfaced by `HELP`/`INFO` as the author-defined command list.
    fn handle_reserved(
        &self,
        command: &str,
        args_header: Option<&str>,
        req_body: &str,
        handler_commands: &[&str],
    ) -> Option<ReservedOutcome>;

    /// Record an isolated handler fault (error or panic) so the citizen's
    /// health surface can report it (`lifecycle.health` → `degraded`,
    /// fault count + last-fault summary). Fault isolation keeps the
    /// citizen alive (SPEC 18 §3.4); this hook is what keeps it from
    /// looking *healthy* while blind. Default no-op for runtimes without
    /// a health surface.
    fn record_handler_fault(&self, _summary: &str) {}
}

/// Result of servicing a runtime-reserved verb (see [`ServeRuntime`]).
pub struct ReservedOutcome {
    /// Bus return code for the response.
    pub rc: u8,
    /// Response body (already JSON-encoded by the citizen layer).
    pub body: String,
    /// `QUIT` was serviced: after the reply is transmitted the pump
    /// MUST break and let the serve entrypoint run the shutdown path
    /// (SPEC 18 §3.5 — *not* a no-op; WS5 wires the deregister-before-
    /// exit sequence onto this same break).
    pub quit: bool,
}

/// Shared buffer for capturing evaluator output.
#[derive(Clone)]
pub struct SharedBuf {
    inner: Rc<RefCell<Vec<u8>>>,
}

impl Default for SharedBuf {
    fn default() -> Self {
        Self::new()
    }
}

impl SharedBuf {
    pub fn new() -> Self {
        SharedBuf {
            inner: Rc::new(RefCell::new(Vec::new())),
        }
    }

    /// Get captured output as a string.
    pub fn to_string_lossy(&self) -> String {
        String::from_utf8_lossy(&self.inner.borrow()).to_string()
    }

    /// Take the raw bytes, leaving the buffer empty.
    pub fn take(&self) -> Vec<u8> {
        std::mem::take(&mut *self.inner.borrow_mut())
    }
}

impl Write for SharedBuf {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        self.inner.borrow_mut().extend_from_slice(buf);
        Ok(buf.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

/// Single-writer scheduler primitive backing the SPEC 18 Phase 2
/// dispatch admission gate.
///
/// **Why a wrapper around `tokio::sync::RwLock<()>` instead of raw
/// lock calls in `Evaluator`:** the Class C handler runtime spawns
/// per-invocation tasks on a citizen-scoped `LocalSet` (C.7d). Those
/// tasks need to coordinate with the inline Class S dispatch path
/// and with each other through one shared admission primitive — "at
/// most one Class S chain runs at a time, and Class S excludes every
/// Class C task." Tokio's write-preferring `RwLock` is the right
/// primitive because:
///
/// * Class S takes the **writer** permit via
///   [`DispatchScheduler::acquire_writer_dispatch`] — exclusive of
///   every other dispatch, held inline for the entire chain.
/// * Class C takes the **reader** permit per async entry via
///   [`DispatchScheduler::acquire_reader_dispatch`] — multiple Class
///   C reader holders may run concurrently, but write-preference
///   guarantees a queued Class S writer is admitted before new
///   readers (no starvation).
/// * A plain entry **embedded in a Class C chain** takes the writer
///   per entry via [`DispatchScheduler::acquire_writer_class_c_entry`]
///   so it runs with the same serialization a standalone Class S
///   dispatch would have.
///
/// **Sole admission gate.** C.6b retired the legacy
/// `Evaluator::dispatch_depth` counter; C.7d retired the
/// `pending_events` FIFO queue. The three scheduler acquire methods
/// — [`Self::acquire_writer_dispatch`],
/// [`Self::acquire_writer_class_c_entry`], and
/// [`Self::acquire_reader_dispatch`] — are the only paths a citizen
/// takes through the scheduler.
///
/// **No poison story.** C.7g B1 made Class S run on a per-invocation
/// activation built by [`Evaluator::for_invocation`]; B2 then deleted
/// the scheduler-wide `poison: Cell<bool>` flag, `CleanExitGuard`,
/// `try_enter_dispatch`, and the `DispatchAdmission` enum that backed
/// them. Both arms now isolate cancellation in the activation's
/// private `InvocationCtx` / local `Scope` frames; a `dispatch_event`
/// future dropped mid-await discards the permit (RAII) and the
/// activation together. The parent evaluator is never contaminated,
/// so the scheduler does not need to refuse subsequent admissions.
///
/// **`is_dispatching()`** is backed by `try_write()` — a synchronous
/// "is anyone holding the writer right now" probe used by the sleep
/// builtin to gate the yield-point select! (so a handler-body
/// `sleep` does not re-enter the dispatch select! and self-deadlock
/// on the writer permit).
pub(crate) struct DispatchScheduler {
    /// `Arc<RwLock<()>>` so per-invocation Class C tasks can hold an
    /// **owned** read permit ([`tokio::sync::OwnedRwLockReadGuard`]) as
    /// `InvocationCtx::class_c_read_permit`, freed across every
    /// handler-body await via [`Evaluator::await_with_class_c_yield`]
    /// (C.7e). The borrowed-guard writer methods
    /// (`acquire_writer_dispatch`, `acquire_writer_class_c_entry`) keep
    /// their `&self` shape because the permit is held on a *local* stack
    /// frame for the body's non-await duration only; the owned reader
    /// method [`Self::acquire_reader_dispatch`] returns
    /// [`tokio::sync::OwnedRwLockReadGuard`] and is the one stored on
    /// `InvocationCtx` so the helper can `take()` it across an `.await`.
    lock: std::sync::Arc<tokio::sync::RwLock<()>>,
}

impl DispatchScheduler {
    pub(crate) fn new() -> Self {
        Self {
            lock: std::sync::Arc::new(tokio::sync::RwLock::new(())),
        }
    }

    /// True iff the dispatch write guard is currently held. Backs
    /// `Evaluator::is_dispatching()` after WS3-C.6b retired the
    /// legacy `Evaluator::dispatch_depth` counter.
    pub(crate) fn is_dispatching(&self) -> bool {
        self.lock.try_write().is_err()
    }

    /// SPEC 18 Phase 2 WS3-C.7g — await-based writer acquire used by
    /// the **Class S dispatch path**.
    ///
    /// Returned from `dispatch_event`'s Class S inline path: a Class S
    /// chain runs on a per-invocation activation built by
    /// [`Evaluator::for_invocation`](crate::evaluator::Evaluator::for_invocation),
    /// awaited inline under this permit. Inline-await preserves the
    /// "Class S blocks `dispatch_event` for the whole chain" atomic
    /// contract; the activation isolates cancellation damage — a
    /// `dispatch_event` future dropped mid-await discards both the
    /// permit (RAII) and the activation (its private `InvocationCtx`
    /// / local `Scope` frames). The parent evaluator is never mutated
    /// by the chain, so cancellation needs no scheduler-wide poison
    /// flag to refuse subsequent admissions (the previous CleanExitGuard
    /// + poison machinery, retired in C.7g B2).
    ///
    /// **Holding the writer permit** blocks every other admission:
    /// no other Class S writer, no new Class C reader, no Class C
    /// *resumption* of a reader released across an `.await`. Tokio's
    /// write-preferring `RwLock` (a waiting writer blocks new readers)
    /// satisfies the "Class C cannot starve a waiting Class S writer"
    /// invariant without a separate fairness implementation.
    ///
    /// **Not used by Class C** — async entries use
    /// [`Self::acquire_reader_dispatch`]; plain entries embedded in a
    /// Class C chain use [`Self::acquire_writer_class_c_entry`].
    pub(crate) async fn acquire_writer_dispatch(&self) -> tokio::sync::RwLockWriteGuard<'_, ()> {
        self.lock.write().await
    }

    /// SPEC 18 Phase 2 WS3-C.7d R2 — bare writer acquire used for
    /// **plain handlers embedded in a Class C chain**.
    ///
    /// A plain entry mixed into a Class C chain temporarily needs the
    /// same writer exclusion a standalone Class S dispatch would have
    /// — the body still mutates shared globals and must not interleave
    /// with any other dispatch. The per-entry permit is acquired
    /// inside `run_handlers_for`'s per-entry loop and released at end
    /// of iteration, letting Tokio's write-preference admit any queued
    /// writer (Class S parent or another Class C plain entry) before
    /// the next iteration's reader request.
    ///
    /// **No poison story.** Class C tasks run on per-invocation
    /// activations (`for_invocation`); cancellation discards the whole
    /// activation, no parent state to contaminate. The Class S path
    /// gained activation isolation in C.7g B1, retiring the
    /// scheduler-wide `CleanExitGuard` / `poison` machinery in C.7g B2
    /// for both arms.
    pub(crate) async fn acquire_writer_class_c_entry(
        &self,
    ) -> tokio::sync::RwLockWriteGuard<'_, ()> {
        self.lock.write().await
    }

    /// SPEC 18 Phase 2 WS3-C.7d/C.7e — await-based read permit acquire
    /// returning an [`tokio::sync::OwnedRwLockReadGuard`].
    ///
    /// Held by a Class C task during the non-await execution of an
    /// async handler body. The permit is the *only* reader form: there
    /// is no borrowed-guard variant because the permit must outlive
    /// `run_handlers_for`'s `&self` and live on
    /// [`InvocationCtx::class_c_read_permit`] so that
    /// [`Evaluator::await_with_class_c_yield`] (C.7e) can `take()` it
    /// across every handler-body `.await` and reacquire afterwards.
    /// Released across every yield point (`send`, `sleep_ms`, `reply`
    /// send, future async builtins) via that helper, and per-entry
    /// transitions are re-acquired in `run_handlers_for` for
    /// subsequent entries. Multiple Class C readers may be admitted
    /// concurrently — that is the entire point of Class C.
    ///
    /// **Writer-preference.** Tokio's `RwLock` is documented as
    /// write-preferring: a waiting writer blocks new readers. A
    /// continuous stream of Class C arrivals therefore cannot starve a
    /// waiting Class S writer; the plan's invariant is satisfied by
    /// the primitive itself, no manual fairness queue required.
    pub(crate) async fn acquire_reader_dispatch(&self) -> tokio::sync::OwnedRwLockReadGuard<()> {
        std::sync::Arc::clone(&self.lock).read_owned().await
    }
}

/// SPEC 18 Phase 2 WS3-C.7a — dispatch chain classification.
///
/// Variants use the SPEC 18 §3.7 names (`ClassS`/`ClassC`) rather
/// than Rust's `Sync`/`Async`, because the runtime concept is the
/// substrate's serialization class, not whether the body itself
/// suspends. The rule (plan §4 WS3 "Mixed-class-per-event rule —
/// DECIDED"):
///
/// > Class C if ANY matching handler is `async`, else Class S.
///
/// A Class C chain may still contain plain (Class S) bodies; those
/// run under the writer permit for their own body duration, with
/// the chain task releasing the read permit before acquiring writer
/// and re-acquiring read for any subsequent Class C bodies (no
/// read-to-write upgrade — starvation-prone, not portable across
/// `tokio::sync::RwLock` implementations; plan Codex round-3
/// MINOR).
///
/// **Routing.** [`Evaluator::dispatch_event`] classifies the chain
/// once at the top: `ClassS` awaits inline on a per-invocation
/// activation (built by [`Evaluator::for_invocation`]) under the
/// writer permit for the chain's whole duration (atomic semantics);
/// `ClassC` spawns the chain on the citizen-scoped LocalSet as a
/// separate task, running its entries sequentially with C.7e's
/// yield discipline releasing and reacquiring the reader permit
/// across each handler-body await so other dispatches may
/// interleave.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ChainClass {
    /// All handlers in the chain are plain (`on <cmd> do …`).
    /// Today's atomic semantics — the chain runs inline under the
    /// writer permit for its entire duration; no other handler
    /// invocation (Class S or Class C) progresses while the chain
    /// is on the stack.
    ClassS,
    /// At least one handler in the chain is `on <cmd> async do …`.
    /// C.7d spawns this chain on the citizen-scoped LocalSet; the
    /// task runs the chain sequentially, yielding the read permit
    /// across each `send.await` so other dispatches may interleave.
    ClassC,
}

impl ChainClass {
    /// Classify a built handler chain. ANY async handler promotes
    /// the whole chain to Class C; the empty chain is Class S
    /// (the inline-no-op path).
    pub(crate) fn from_entries(entries: &[HandlerEntry]) -> Self {
        if entries.iter().any(|e| e.is_async) {
            ChainClass::ClassC
        } else {
            ChainClass::ClassS
        }
    }
}

/// SPEC 18 Phase 2 WS3-C.7d R2 MAJOR-3 — per-entry scheduler permit
/// carrier inside `run_handlers_for`.
///
/// The Class S path (`scheduler=None` to `run_handlers_for`) means the
/// caller (`dispatch_event`'s Class S arm) already holds the writer
/// permit for the whole chain; per-entry permit logic is a no-op and
/// this variant carries nothing. The Class C path acquires per-entry
/// reader or bare writer, matching the documented mixed-chain rule
/// (an async entry runs under reader; a plain entry embedded in a
/// Class C chain runs under writer so it gets the same exclusion a
/// standalone Class S dispatch would have). Variant is dropped at end
/// of each iteration of the entries loop — `tokio::sync::RwLock`'s
/// write-preference then admits any waiting writer before the next
/// iteration's permit request.
///
/// **No CleanExitGuard on either variant.** Both Class C tasks
/// (C.7d) and Class S dispatches (C.7g B1) now run on a per-invocation
/// evaluator (`for_invocation`) whose `InvocationCtx` and local
/// `Scope` frames are private to the activation. Cancellation drops
/// the activation entirely — there is no parent state to contaminate,
/// so the scheduler-wide poison flag retired in C.7g B2 is not
/// needed on either acquire path.
enum EntryPermit<'a> {
    /// `scheduler=None` path: caller's writer permit covers the whole
    /// chain. No per-entry permit work; no drop side-effects.
    CallerHeldWriter,
    /// Class C async entry. The actual permit is an
    /// [`tokio::sync::OwnedRwLockReadGuard`] living on
    /// [`InvocationCtx::class_c_read_permit`] so
    /// [`Evaluator::await_with_class_c_yield`] (C.7e) can `take()` it
    /// across handler-body `.await` points and reacquire after. This
    /// variant is a marker; cleanup is by
    /// `self.ctx.class_c_read_permit.take()` at end-of-iteration, not
    /// by RAII on the enum value.
    Reader,
    /// Class C plain entry embedded in a mixed chain. Bare writer
    /// permit acquired via
    /// [`DispatchScheduler::acquire_writer_class_c_entry`]. Plain
    /// bodies stay serialized for their duration even when embedded
    /// in a Class C chain (they do NOT participate in the C.7e
    /// reader-yield dance); `_guard` exists only for its `Drop` side
    /// effect at end-of-iteration.
    Writer {
        _guard: tokio::sync::RwLockWriteGuard<'a, ()>,
    },
}

/// RAII guard restoring a swapped-out `globals.stdout` on drop.
///
/// Used by `exec_pipe_to_external` so that **any** exit from the piped
/// statement — normal return, panic, or future-cancellation while
/// suspended at an `.await` inside the inner statement — leaves
/// `globals.stdout` pointing at the caller's original writer rather than
/// the local capture buffer. A `mem::replace` + manual restore alone is
/// cancellation-unsafe: if the surrounding future is dropped mid-await
/// (e.g. SIGINT during `mix --serve`), the manual restore never runs and
/// every later print silently lands in a dead `SharedBuf`. SPEC 18 §3.4
/// requires that pre-handler state be restored on every yield point;
/// holding the swap in a `Drop` impl makes that automatic.
struct StdoutSwapGuard {
    globals: Rc<RefCell<EvaluatorGlobals>>,
    original: Option<Box<dyn Write>>,
}

impl Drop for StdoutSwapGuard {
    fn drop(&mut self) {
        if let Some(orig) = self.original.take() {
            self.globals.borrow_mut().stdout = orig;
        }
    }
}

/// Compiled numeric expression as a flat stack-machine bytecode for tight
/// inner-loop reuse. The For-loop top-level fast path (e.g. loop_arith's
/// `$sum + $i * 2 - 1`) walks the same `Expr` tree 1M+ times. Compiling
/// once to a `Vec<NumOpcode>` and evaluating linearly swaps:
///   - per-Variable string-keyed cache scan → single `*const f64` deref
///   - `try_eval_expr_num`'s recursive `Box`-allocated tree (one fn call
///     per node, scattered allocations) → single linear walk over a
///     contiguous `Vec` (one match dispatch per opcode, no fn calls)
///
/// The build phase resolves Variable names against a `(*const str, *const f64)`
/// slice (the f64 cache snapshot at compile time); each Variable produces a
/// `PushVarPtr` opcode holding the slot pointer directly. Unresolved variables
/// or non-numeric ops cause the build to return `None` and the caller falls
/// back to the generic path.
///
/// Both stack-machine VMs (`eval_num_program`, `eval_fib_program`) run on
/// fixed `[f64; NUM_STACK_SLOTS]` operand stacks with unchecked-by-design
/// `sp` arithmetic, so both builders track the maximum live-operand depth
/// and refuse (return `None`) any program that would need more slots — a
/// right-nested `$a - ($a - (… ))` 17+ levels deep would otherwise index
/// past the array and panic.
const NUM_STACK_SLOTS: usize = 16;

enum NumOpcode {
    PushConst(f64),
    /// Direct pointer into a live `f64` slot. Caller's safety contract: the
    /// slot must outlive every `eval_num_program` call against this program.
    /// In the For-loop fast path the slots come from `var_f64`/`assign_f64`
    /// (pointers into Value::Number payloads in the scope) which are stable
    /// for the loop's duration since the body is verified `[Assignment]`.
    PushVarPtr(*const f64),
    Add,
    Sub,
    Mul,
    Div,
    Mod,
    Pow,
    /// Fused PushConst+BinOp on the existing top-of-stack: pop replaced by
    /// in-place op against a constant, removing one dispatch + one stack
    /// store/load per fused pair. loop_arith's `$sum + $i * 2 - 1` fuses
    /// `PushConst(2); Mul` and `PushConst(1); Sub` (7 → 5 opcodes).
    AddConst(f64),
    SubConst(f64),
    MulConst(f64),
    /// Fused PushVarPtr+BinOp: same idea for `... + $x` shapes. Not used by
    /// loop_arith but cheap to support and beneficial for any expr shape
    /// `<expr> <op> $var`.
    AddVarPtr(*const f64),
    SubVarPtr(*const f64),
    MulVarPtr(*const f64),
}

/// Returns the maximum operand-stack depth the compiled (sub)program
/// needs, relative to the stack level at entry. The caller compares the
/// root's depth against [`NUM_STACK_SLOTS`] and falls back to the AST
/// path when the fixed `eval_num_program` stack would overflow.
fn build_num_program(
    expr: &Expr,
    cache: &[(*const str, *const f64)],
    out: &mut Vec<NumOpcode>,
) -> Option<usize> {
    match expr {
        Expr::NumberLiteral(n) => {
            out.push(NumOpcode::PushConst(*n));
            Some(1)
        }
        Expr::Variable(name) => {
            let needle = name.as_str();
            for (key_ptr, n_ptr) in cache.iter().rev() {
                let key_str: &str = unsafe { &**key_ptr };
                if key_str == needle {
                    out.push(NumOpcode::PushVarPtr(*n_ptr));
                    return Some(1);
                }
            }
            None
        }
        Expr::BinaryOp { left, op, right } => {
            let left_depth = build_num_program(left, cache, out)?;
            // Try to fuse `<left> <op> <const_or_var>` into a single opcode
            // by emitting the right side's atom into the BinOp itself rather
            // than as a separate Push. Saves one dispatch + one stack
            // round-trip per fused pair. Only safe for non-trapping ops
            // (Div would need rv-zero check; Mod/Pow rare in hot loops).
            match (op, &**right) {
                (BinOp::Add, Expr::NumberLiteral(n)) => {
                    out.push(NumOpcode::AddConst(*n));
                    return Some(left_depth);
                }
                (BinOp::Sub, Expr::NumberLiteral(n)) => {
                    out.push(NumOpcode::SubConst(*n));
                    return Some(left_depth);
                }
                (BinOp::Mul, Expr::NumberLiteral(n)) => {
                    out.push(NumOpcode::MulConst(*n));
                    return Some(left_depth);
                }
                (BinOp::Add, Expr::Variable(name)) => {
                    let needle = name.as_str();
                    for (key_ptr, n_ptr) in cache.iter().rev() {
                        let key_str: &str = unsafe { &**key_ptr };
                        if key_str == needle {
                            out.push(NumOpcode::AddVarPtr(*n_ptr));
                            return Some(left_depth);
                        }
                    }
                }
                (BinOp::Sub, Expr::Variable(name)) => {
                    let needle = name.as_str();
                    for (key_ptr, n_ptr) in cache.iter().rev() {
                        let key_str: &str = unsafe { &**key_ptr };
                        if key_str == needle {
                            out.push(NumOpcode::SubVarPtr(*n_ptr));
                            return Some(left_depth);
                        }
                    }
                }
                (BinOp::Mul, Expr::Variable(name)) => {
                    let needle = name.as_str();
                    for (key_ptr, n_ptr) in cache.iter().rev() {
                        let key_str: &str = unsafe { &**key_ptr };
                        if key_str == needle {
                            out.push(NumOpcode::MulVarPtr(*n_ptr));
                            return Some(left_depth);
                        }
                    }
                }
                _ => {}
            }
            // The right side evaluates with the left's result still live
            // on the stack, hence the `1 +`.
            let right_depth = build_num_program(right, cache, out)?;
            out.push(match op {
                BinOp::Add => NumOpcode::Add,
                BinOp::Sub => NumOpcode::Sub,
                BinOp::Mul => NumOpcode::Mul,
                BinOp::Div => NumOpcode::Div,
                BinOp::Mod => NumOpcode::Mod,
                BinOp::Power => NumOpcode::Pow,
                _ => return None,
            });
            Some(left_depth.max(1 + right_depth))
        }
        _ => None,
    }
}

/// Stack-machine eval. Stack is a fixed-size array; the caller refuses
/// programs whose `build_num_program` depth exceeds [`NUM_STACK_SLOTS`]
/// (falling back to the AST path). Real numeric expressions in tight
/// loops are typically depth ≤ 4.
#[inline]
fn eval_num_program(prog: &[NumOpcode]) -> MixResult<f64> {
    let mut stack: [f64; NUM_STACK_SLOTS] = [0.0; NUM_STACK_SLOTS];
    let mut sp: usize = 0;
    for op in prog {
        match op {
            NumOpcode::PushConst(n) => {
                stack[sp] = *n;
                sp += 1;
            }
            NumOpcode::PushVarPtr(p) => {
                stack[sp] = unsafe { *(*p) };
                sp += 1;
            }
            NumOpcode::Add => {
                sp -= 1;
                stack[sp - 1] += stack[sp];
            }
            NumOpcode::Sub => {
                sp -= 1;
                stack[sp - 1] -= stack[sp];
            }
            NumOpcode::Mul => {
                sp -= 1;
                stack[sp - 1] *= stack[sp];
            }
            NumOpcode::Div => {
                sp -= 1;
                let rv = stack[sp];
                if rv == 0.0 {
                    return Err(MixError::RuntimeError {
                        span: None,
                        msg: "division by zero".to_string(),
                    });
                }
                stack[sp - 1] /= rv;
            }
            NumOpcode::Mod => {
                sp -= 1;
                let rv = stack[sp];
                if rv == 0.0 {
                    return Err(MixError::RuntimeError {
                        span: None,
                        msg: "modulo by zero".to_string(),
                    });
                }
                stack[sp - 1] %= rv;
            }
            NumOpcode::Pow => {
                sp -= 1;
                stack[sp - 1] = stack[sp - 1].powf(stack[sp]);
            }
            NumOpcode::AddConst(n) => {
                stack[sp - 1] += *n;
            }
            NumOpcode::SubConst(n) => {
                stack[sp - 1] -= *n;
            }
            NumOpcode::MulConst(n) => {
                stack[sp - 1] *= *n;
            }
            NumOpcode::AddVarPtr(p) => {
                stack[sp - 1] += unsafe { *(*p) };
            }
            NumOpcode::SubVarPtr(p) => {
                stack[sp - 1] -= unsafe { *(*p) };
            }
            NumOpcode::MulVarPtr(p) => {
                stack[sp - 1] *= unsafe { *(*p) };
            }
        }
    }
    Ok(stack[0])
}

/// Stack-machine eval for fib-style bytecode. Recursion is implemented
/// via direct Rust recursion (one frame per `CallSelf` opcode); fib(22)
/// reaches depth 22, well within the default Rust stack budget. The
/// per-call cost is roughly: load `arity` args, walk a flat `Vec<FibOp>`
/// match-dispatching each opcode, and on `CallSelf` copy `arity` args
/// to a small stack array and recurse.
//
// `!(a < b)` etc. on f64 in the *JZ arms is intentional: the `JZ` opcodes
// mean "jump if the comparison is NOT strictly true", which captures NaN
// (where every ordering is false) as "jump", matching the source-level
// semantics of `if a < b then BODY end`. Using `partial_cmp` here would
// build and branch on an `Option<Ordering>` per comparison in a fib(22)-
// million-iteration hot loop for no behavioural gain.
#[allow(clippy::neg_cmp_op_on_partial_ord)]
fn eval_fib_program(
    prog: &[FibOp],
    args: &[f64],
    depth: usize,
    limit: usize,
    deadline: Option<std::time::Instant>,
    ops: &mut u64,
) -> MixResult<f64> {
    // Operand depth bounded by compile_fib_program's NUM_STACK_SLOTS gate.
    let mut stack: [f64; NUM_STACK_SLOTS] = [0.0; NUM_STACK_SLOTS];
    let mut sp: usize = 0;
    let mut pc: usize = 0;
    while pc < prog.len() {
        match &prog[pc] {
            FibOp::PushArg(i) => {
                stack[sp] = args[*i as usize];
                sp += 1;
                pc += 1;
            }
            FibOp::PushConst(n) => {
                stack[sp] = *n;
                sp += 1;
                pc += 1;
            }
            FibOp::Add => {
                sp -= 1;
                stack[sp - 1] += stack[sp];
                pc += 1;
            }
            FibOp::Sub => {
                sp -= 1;
                stack[sp - 1] -= stack[sp];
                pc += 1;
            }
            FibOp::Mul => {
                sp -= 1;
                stack[sp - 1] *= stack[sp];
                pc += 1;
            }
            FibOp::Div => {
                sp -= 1;
                let r = stack[sp];
                if r == 0.0 {
                    return Err(MixError::RuntimeError {
                        span: None,
                        msg: "division by zero".to_string(),
                    });
                }
                stack[sp - 1] /= r;
                pc += 1;
            }
            FibOp::Mod => {
                sp -= 1;
                let r = stack[sp];
                if r == 0.0 {
                    return Err(MixError::RuntimeError {
                        span: None,
                        msg: "modulo by zero".to_string(),
                    });
                }
                stack[sp - 1] %= r;
                pc += 1;
            }
            FibOp::Pow => {
                sp -= 1;
                stack[sp - 1] = stack[sp - 1].powf(stack[sp]);
                pc += 1;
            }
            FibOp::LtJZ(t) => {
                sp -= 2;
                if !(stack[sp] < stack[sp + 1]) {
                    pc = *t as usize;
                } else {
                    pc += 1;
                }
            }
            FibOp::GtJZ(t) => {
                sp -= 2;
                if !(stack[sp] > stack[sp + 1]) {
                    pc = *t as usize;
                } else {
                    pc += 1;
                }
            }
            FibOp::LeJZ(t) => {
                sp -= 2;
                if !(stack[sp] <= stack[sp + 1]) {
                    pc = *t as usize;
                } else {
                    pc += 1;
                }
            }
            FibOp::GeJZ(t) => {
                sp -= 2;
                if !(stack[sp] >= stack[sp + 1]) {
                    pc = *t as usize;
                } else {
                    pc += 1;
                }
            }
            FibOp::EqJZ(t) => {
                sp -= 2;
                if stack[sp] != stack[sp + 1] {
                    pc = *t as usize;
                } else {
                    pc += 1;
                }
            }
            FibOp::NeJZ(t) => {
                sp -= 2;
                if stack[sp] == stack[sp + 1] {
                    pc = *t as usize;
                } else {
                    pc += 1;
                }
            }
            FibOp::Jump(t) => {
                pc = *t as usize;
            }
            FibOp::CallSelf(arity) => {
                // Knob B: this bytecode VM recurses in native Rust without
                // touching `function_depth`, so bound the depth here too —
                // otherwise a runaway numeric recursion overflows the stack
                // (uncatchable SIGSEGV). `depth`/`limit` continue the
                // evaluator's shared budget (seeded from the call site).
                if depth + 1 >= limit {
                    return Err(MixError::RuntimeError {
                        span: None,
                        msg: format!("recursion depth exceeded (limit {limit})"),
                    });
                }
                // Knob C: a wide-but-shallow recursion (e.g. fib(40)) never
                // re-enters the statement poll, so check the wall-clock
                // deadline here — throttled (every 2^16 calls) to keep the
                // fib hot loop fast. Zero overhead when no deadline is set.
                if let Some(dl) = deadline {
                    *ops += 1;
                    if *ops & 0xFFFF == 0 && std::time::Instant::now() >= dl {
                        return Err(MixError::RuntimeError {
                            span: None,
                            msg: "deadline exceeded".to_string(),
                        });
                    }
                }
                let a = *arity as usize;
                sp -= a;
                let mut sub_args: [f64; 4] = [0.0; 4];
                sub_args[..a].copy_from_slice(&stack[sp..sp + a]);
                let r =
                    eval_fib_program(prog, &sub_args[..a], depth + 1, limit, deadline, &mut *ops)?;
                stack[sp] = r;
                sp += 1;
                pc += 1;
            }
            FibOp::Ret => {
                return Ok(stack[sp - 1]);
            }
        }
    }
    Ok(stack[0])
}

/// Default cap on user-function call-recursion depth (the depth knob).
///
/// Unbounded Mix recursion overflows the **native Rust stack** before
/// any cooperative check can fire — an uncatchable SIGSEGV that takes
/// the whole host process down. So unlike the other limits this one is
/// **finite by default**: it must protect even a naive embedder that
/// never calls [`Evaluator::set_limits`].
///
/// The safe ceiling scales with the stack the evaluator runs on, and
/// with how stack-hungry each call level is. Measured: the **async**
/// call path (`call_function` → `execute` → `eval_expr`, nested
/// `Box::pin` futures) burns tens of KB of native stack per recursion
/// level — on a ~2 MB thread (tokio worker / `spawn_blocking` / test
/// threads, e.g. maild's per-message inbound filter, *after* the
/// thread + runtime + block_on overhead) a deep recursion overflows
/// around depth ~32, with run-to-run variance. This default is set
/// well under that so it fails *reliably* with a clean error rather
/// than flakily overflowing. Consumers that run on a bigger stack and
/// need deeper recursion raise it via [`Evaluator::set_limits`] — the
/// `mix` binary uses 128 on the ~8 MB main thread; `mix-bench` sets its
/// own. (The sync fib fast path has far smaller frames — the
/// ~500–1000 figure in older notes was that path — but the cap must
/// hold for the worst, async, path.)
pub const DEFAULT_RECURSION_LIMIT: usize = 16;

/// Names the `Expr::FunctionCall` eval arm dispatches INLINE (special
/// forms handled in the arm body rather than through the builtin /
/// HOF registries), plus the in-place mutating trio. Two consumers
/// must stay in sync with the arm:
///   - `Parser::parse_postfix` keeps the legacy UFCS desugar for these
///     names so `expr.name(args)` still reaches the inline arm (a
///     `MethodCall` would route them to member lookup and break them);
///   - `try_inline_user_call`'s shadow-exclusion list (a subset —
///     names that can also collide with user functions).
///
/// A new inline special form added to the `FunctionCall` arm MUST be
/// added here too.
pub(crate) const INLINE_SPECIAL_FORMS: &[&str] = &[
    "push",
    "pop",
    "shift",
    "sleep",
    "readline",
    "read_stdin",
    "read_stdin_bytes",
    "printf",
    "eprintf",
    "write_stdout",
    "write_stderr",
    "print_raw",
    "eprint_raw",
    "db_query",
    "db_exec",
    "jmap",
    "jmap_upload",
    "bus_call",
    "port_exists",
    "bus_reconnect",
    "noded_register",
    "subscribe",
    "unsubscribe",
    "reply",
    "quit",
    "publish",
];

/// Per-evaluator capability gate for the builtin table (the capability
/// knob).
///
/// Injected via [`Evaluator::set_capability_policy`]. Consulted once
/// per builtin/HOF dispatch, before the builtin runs: returning
/// `Err(reason)` denies the call with a `capability denied` runtime
/// error; the default (no policy installed) is fully permissive, so
/// every existing caller is unaffected. The seam governs the builtin
/// table (`read_file`/`write_file`/`http_get`/`ssh_run`/`run`/…);
/// `send`/`emit`/`sh` are separately gated by their own handler seams.
///
/// A policy typically classifies `name` via
/// [`crate::builtins::capability_category`] and allows/denies by class.
/// NOTE: an in-process gate is a *robustness* boundary for trusted
/// embedded scripts, not a containment boundary for untrusted code —
/// a compromise owns the address space the gate runs in. Untrusted
/// multi-tenant scripts run out-of-process (see the maild/webd
/// trust-split ADR).
pub trait CapabilityPolicy {
    /// Return `Ok(())` to allow the builtin `name`, `Err(reason)` to deny.
    fn check_builtin(&self, name: &str) -> Result<(), String>;

    /// Gate a capability *class* directly, for host authority that is
    /// **not** a named builtin — Mix's shell syntax (`sh "…"`, the
    /// `$(…)` command substitution, `… | cmd` pipes, and bare
    /// shell-dispatch lines) all spawn `/bin/sh` without going through
    /// the builtin table, so a policy that only gated [`check_builtin`]
    /// would let those escape (they are [`CapabilityClass::Process`]).
    ///
    /// The default maps the class to a representative builtin name and
    /// reuses [`check_builtin`], so a policy that only implements
    /// `check_builtin` still gates shell execution correctly — override
    /// for finer control (e.g. [`crate::builtins::CategoryAllowList`]).
    fn check_class(&self, class: crate::builtins::CapabilityClass) -> Result<(), String> {
        use crate::builtins::CapabilityClass::*;
        let representative = match class {
            Pure => return Ok(()),
            FsRead => "read_file",
            FsWrite => "write_file",
            Network => "http_get",
            Process => "run",
            Env => "env",
            Db => "db_query",
            Jmap => "jmap",
            Bus => "bus_call",
        };
        self.check_builtin(representative)
    }
}

/// Native self-protection limits for a single evaluator (the depth,
/// time, and collection-size knobs).
///
/// Set via [`Evaluator::set_limits`] before execution. All limits are
/// enforced natively in the tree-walking evaluator — there is no
/// external sandbox engine or added dependency. The three categories
/// (call depth / wall-clock time / collection size) are the standard
/// self-protection knobs for an embeddable interpreter.
#[derive(Clone, Debug)]
pub struct EvalLimits {
    /// Max user-function call-recursion depth before a clean
    /// `recursion depth exceeded` error. See [`DEFAULT_RECURSION_LIMIT`].
    pub recursion_limit: usize,
    /// Optional wall-clock budget for one run. Checked at the
    /// per-statement poll, so loop-based runaways are bounded; a single
    /// blocking builtin (`run`/`ssh_run`/`http_*`) cannot be
    /// interrupted mid-syscall. `None` = no time limit.
    pub time_limit: Option<std::time::Duration>,
    /// Optional cap on any single list's length. `None` = no cap.
    pub max_list_len: Option<usize>,
    /// Optional cap on any single map's entry count. `None` = no cap.
    pub max_map_len: Option<usize>,
    /// Optional cap on any single string's byte length. `None` = no cap.
    pub max_string_len: Option<usize>,
}

impl Default for EvalLimits {
    fn default() -> Self {
        EvalLimits {
            recursion_limit: DEFAULT_RECURSION_LIMIT,
            time_limit: None,
            max_list_len: None,
            max_map_len: None,
            max_string_len: None,
        }
    }
}

/// Shared evaluator state — the **S** bucket in the SPEC 18 Phase 2
/// WS3-C field classification.
///
/// These fields are per-citizen rather than per-invocation: there is
/// one of each per `Evaluator`, and concurrent handler invocations
/// under Class C (Phase 2 WS3-C.5+) will observe the same instance.
/// Slice WS3-C.1 extracts the bucket as a direct field; later slices
/// wrap it in `Rc<RefCell<_>>` (C.5) and reshape await-surface handles
/// to clone-out form (C.3). The per-invocation **P** bucket
/// (`current_file`, `in_handler`, `function_depth`, `address_stack`,
/// `current_line`, `sync_self_func`, `var_slot_cache`,
/// `var_slot_cache_f64`) lives on its own `InvocationCtx` as of C.4a;
/// C.4b will gate the shared-global raw-pointer fast paths the slot
/// caches feed on a `can_use_shared_scope_raw_slots()` predicate. The
/// reply-correlation field became a per-invocation `Rc<InvocationReplyHandle>`
/// (`current_reply`) held in a shutdown-visible registry in C.7c. The
/// legacy `dispatch_depth` counter was retired in C.6b (the
/// scheduler RwLock is now the sole admission gate); the legacy
/// `pending_events` FIFO queue was retired in C.7d when the LocalSet-
/// spawned Class C task model + writer-permit Class S inline model
/// replaced reentrancy-queueing dispatch.
///
/// Until those later slices land this struct is just an embedded
/// grouping — behaviour, ownership, and access patterns are
/// preserved bit-for-bit (every former `self.<field>` now reads
/// `self.globals.<field>`).
pub(crate) struct EvaluatorGlobals {
    stdout: Box<dyn Write>,
    stderr: Box<dyn Write>,
    extensions: HashMap<String, ExtFn>,
    aliases: HashMap<String, String>,
    /// `include`-once dedup set: canonical paths already loaded this
    /// program run (`exec_include`). Lives here (shared globals) rather
    /// than per-`InvocationCtx` so "load at most once" holds across the
    /// whole program, including SPEC 18 Class C handler invocations that
    /// clone the same `globals`. `source` does NOT consult it.
    included_files: std::collections::HashSet<String>,
    /// `require()` once-only cache: canonical path → exports Value.
    /// Parallel to `included_files` (shared across SPEC 18 Class C
    /// invocations for the same load-at-most-once reason). Inserted
    /// only on SUCCESS — a failed require (read, parse, or runtime
    /// error) caches nothing and stays retryable; hits return a clone
    /// (`Value::Function` members are `Rc` bumps, so function-heavy
    /// exports clone cheaply).
    module_cache: HashMap<String, Value>,
    /// True once `load_prelude` has run for this program, so
    /// `exec_require` can replay the prelude into each fresh module
    /// scope (and skip the replay under `--no-prelude`).
    prelude_loaded: bool,
    /// Dedup set for one-shot diagnostics (e.g. the dead-push-into-a-
    /// by-value-parameter warning) so the same warning text is emitted at
    /// most once per program run — a helper defined inside a loop or a
    /// re-sourced file would otherwise re-warn on every definition.
    warned_diagnostics: std::collections::HashSet<String>,
    /// SPEC 18 Phase 2 WS3-C.3 — `Rc<dyn BusHandler>` (not `Box`) so a
    /// per-invocation Class C task can hold its own clone and release
    /// the shared-globals borrow before any `.await`. Every consumer
    /// uses the clone-out shape: `let h = self.globals.bus_handler
    /// .clone().ok_or_else(...)?; h.send(...).await;`. After C.5 wraps
    /// `globals` in `Rc<RefCell<_>>` the same shape becomes `let h = {
    /// self.globals.borrow().bus_handler.clone() };` — no `Ref` guard
    /// survives into the await.
    bus_handler: Option<Rc<dyn BusHandler>>,
    /// Host database seam (`db_query`/`db_exec`). `None` for plain
    /// library/REPL use (the builtins then raise "database not
    /// available"); `Some` when an embedder injects a scoped store via
    /// [`Evaluator::set_db_handler`] (e.g. webd binds it to the
    /// per-vhost connection). Same `Rc` clone-out-before-await
    /// discipline as `bus_handler`.
    db_handler: Option<Rc<dyn DbHandler>>,
    /// Host JMAP seam (`jmap`). `None` for plain library/REPL use (the
    /// builtin then raises "jmap not available"); `Some` when an embedder
    /// injects a scoped endpoint via [`Evaluator::set_jmap_handler`]
    /// (e.g. webd binds it to the per-vhost maild upstream + request
    /// auth). Same `Rc` clone-out-before-await discipline as
    /// `db_handler`.
    jmap_handler: Option<Rc<dyn JmapHandler>>,
    /// Host Bus delegated-call seam (`bus_call`). `None` for plain
    /// library/REPL use (the builtin then raises "bus_call not available");
    /// `Some` when an embedder injects a scoped, verb-allowlisted,
    /// delegation-wrapping channel via
    /// [`Evaluator::set_bus_call_handler`] (e.g. webd binds it to the
    /// broker for an admin route). Same `Rc` clone-out-before-await
    /// discipline as `jmap_handler`.
    bus_call_handler: Option<Rc<dyn BusCallHandler>>,
    /// Per-line shell fallback used only by `exec_source` when the
    /// whole-file Mix parse fails. `None` for daemon/serve modes and
    /// pure library use; `Some` for the REPL and `run_source` so that
    /// `.mixrc`-style files mixing Mix with bareword shell commands
    /// load the way the REPL treats those lines. `Rc` (not `Box`) so
    /// the per-line loop can hold a clone while also re-borrowing
    /// `&mut self` to execute Mix lines in the caller's scope.
    shell_handler: Option<Rc<dyn ShellHandler>>,
    /// SPEC 18 WS4 — runtime-reserved Ch07 L0+ surface, installed only
    /// by serve mode (`set_serve_runtime`). `None` for plain
    /// `run_source` scripts: no reserved-verb interception, no §9
    /// conformance owed. Consulted in `run_event_pump` pre-dispatch.
    ///
    /// `Rc` (not `Box`) for symmetry with `bus_handler` after WS3-C.3.
    /// The `handle_reserved` call site today is sync (no `.await`
    /// across the borrow), so this is not load-bearing in the
    /// pre-await-borrow sense — but keeping all "global trait objects
    /// invocation tasks may hold across yield points" Rc-typed in one
    /// slice lets C.5 wrap `globals` without a follow-up reshape on
    /// `serve_runtime` if a future runtime verb gains an async leg.
    serve_runtime: Option<Rc<dyn ServeRuntime>>,
    /// Per-evaluator capability gate for the builtin table. `None` =
    /// fully permissive (the default — every existing caller is
    /// unaffected). Set via [`Evaluator::set_capability_policy`];
    /// consulted at the builtin/HOF dispatch site. `Rc` (not `Box`)
    /// for symmetry with the other handler seams. See [`CapabilityPolicy`].
    capability_policy: Option<Rc<dyn CapabilityPolicy>>,
    /// Runtime arity mode source of truth (0.29.0, decision D5) —
    /// `true` = [`ArityMode::Strict`]. The hot path reads the
    /// per-invocation copy in [`InvocationCtx`]; set via
    /// [`Evaluator::set_arity_mode`].
    arity_strict: bool,
    /// Native self-protection limits (depth / time / collection size),
    /// the source of truth set via [`Evaluator::set_limits`]. The hot
    /// path reads a per-invocation **copy** in [`InvocationCtx`] (borrow-
    /// free); this shared copy is what [`Evaluator::for_invocation`]
    /// materialises into each fresh activation's ctx. See [`EvalLimits`].
    limits: EvalLimits,
    interrupted: Arc<AtomicBool>,
    /// Set by the `quit()` builtin to request a graceful shutdown of the
    /// serve event pump (SPEC 18 §3.5). Checked at the top of
    /// [`Evaluator::run_event_pump`], exactly like `interrupted`, so a
    /// script can self-terminate its citizen from an `on` handler or the
    /// init body: the pump breaks with reason `"quit"` → the serve
    /// entrypoint's deregister-before-exit path runs (same graceful path
    /// as the Ch02 `QUIT` universal). For a synchronous (Class S) handler
    /// or the init body the break is immediate (they run inline on the
    /// pump future). An `async` (Class C) handler runs as a spawned task
    /// while the pump is parked in `next_incoming().await`; a bare flag
    /// store would not wake that park (the `interrupted` flag has the
    /// same limitation but is rescued by an outer `select!` in
    /// `run_source`, which `quit()` — being self-initiated — has no
    /// counterpart to). `quit_notify` closes that gap: `quit()` both sets
    /// the flag AND fires this notify, and the pump `select!`s
    /// `next_incoming()` against `quit_notify.notified()` so a Class C
    /// `quit()` wakes the pump immediately.
    quit_requested: Arc<AtomicBool>,
    /// Paired with `quit_requested` — see its docs. `quit()` calls
    /// `notify_one()` so a Class C handler's request wakes the parked
    /// pump; the pump selects `next_incoming()` against `notified()`.
    /// `notify_one` stores at most one permit, so a fire that races ahead
    /// of the `notified()` await is not lost.
    quit_notify: Arc<tokio::sync::Notify>,
    /// Exit requested from a serve handler activation. Handler errors are
    /// normally isolated, and Class C activations run in spawned local tasks,
    /// so they cannot return `ExitRequest` directly to the event pump. The
    /// handler boundary stores the latest requested code here and wakes the
    /// pump through `quit_notify`; the pump then resumes normal ExitRequest
    /// propagation at its top-level boundary.
    exit_requested: Option<i32>,
    trace: bool,
    stats: Option<Box<UsageStats>>,
    /// Event handler registry for the `on` statement. Maps Bus command names
    /// to the ordered list of registered handler entries.
    ///
    /// Each `HandlerEntry` pairs the body (`Arc<Vec<Stmt>>` — dispatch
    /// clones a handle without copying the AST) with the WS1
    /// `is_async` flag captured at parse time. Dispatch releases the
    /// registry borrow before executing bodies so handlers that register
    /// additional handlers (or remove existing ones, once `off` lands)
    /// do not deadlock. WS3-C classifies the chain as Class C if ANY
    /// matching handler is async (otherwise Class S): Class S awaits
    /// inline on a per-invocation activation under the writer permit
    /// for the chain's whole duration; Class C spawns the chain on the
    /// citizen-scoped LocalSet and runs its entries sequentially with
    /// C.7e's release/reacquire yield discipline.
    handlers: HashMap<String, Vec<HandlerEntry>>,
    /// SPEC 18 Phase 2 WS3-C.6b — dispatch admission scheduler. Sole
    /// reentrancy gate as of C.6b. As of C.7d/C.7g B1 the only call
    /// shapes are `acquire_writer_dispatch()` (Class S inline path,
    /// awaited under the activation built by `for_invocation`) and
    /// `tokio::task::spawn_local` + `acquire_reader_dispatch()` (Class
    /// C task path). Every dispatch — Class S or Class C — blocks on
    /// the same `RwLock` here; the per-citizen event pump is the sole
    /// arrival site, so cross-evaluator FIFO preservation is naturally
    /// inherited from the pump's call order.
    ///
    /// `Rc` (not the lock alone) so future Class C spawn paths can
    /// hold a clone for the lifetime of the task without re-borrowing
    /// `EvaluatorGlobals`.
    pub(crate) scheduler: Rc<DispatchScheduler>,
    /// SPEC 18 Phase 2 WS3-C.7c — per-citizen reply handle registry.
    ///
    /// Holds an [`Rc<InvocationReplyHandle>`] for every in-flight
    /// `on`-handler dispatch (registered in `run_handlers_for` after
    /// the handler-list lookup, deregistered by the RAII guard at end
    /// of dispatch). C.7c uses it for inline dispatch; C.7d extends
    /// the same registration shape to LocalSet-spawned Class C tasks;
    /// C.7f's shutdown drain enumerates live handles via
    /// [`InvocationReplyRegistry::snapshot`] and synthesizes the
    /// SPEC 18 §3.4 unanswered-reply for each.
    ///
    /// `Rc` (single indirection, interior `Cell`/`RefCell`) so the
    /// RAII deregistration guard can hold its own clone without re-
    /// borrowing `EvaluatorGlobals` from inside `Drop`.
    pub(crate) reply_registry: Rc<InvocationReplyRegistry>,
    /// SPEC 18 Phase 2 WS3-C.7d — per-citizen Class C task registry.
    ///
    /// Holds a [`tokio::task::JoinHandle`] for every in-flight Class C
    /// chain spawned by `dispatch_event`'s Class C arm (inserted at
    /// spawn time, removed by the spawned task's own finalize step on
    /// normal completion). C.7f's shutdown drain calls
    /// [`ClassCTaskRegistry::drain`] to abort still-live handles under
    /// the `CLASSC_DRAIN_GRACE` window.
    ///
    /// Separate from [`Self::reply_registry`] by design — see
    /// [`ClassCTaskRegistry`] type docs for the cross-lifetime
    /// rationale.
    pub(crate) task_registry: Rc<ClassCTaskRegistry>,
}

impl EvaluatorGlobals {
    fn new() -> Self {
        EvaluatorGlobals {
            stdout: Box::new(std::io::stdout()),
            stderr: Box::new(std::io::stderr()),
            extensions: HashMap::new(),
            aliases: HashMap::new(),
            included_files: std::collections::HashSet::new(),
            module_cache: HashMap::new(),
            prelude_loaded: false,
            warned_diagnostics: std::collections::HashSet::new(),
            bus_handler: None,
            db_handler: None,
            jmap_handler: None,
            bus_call_handler: None,
            shell_handler: None,
            serve_runtime: None,
            capability_policy: None,
            arity_strict: false,
            limits: EvalLimits::default(),
            interrupted: Arc::new(AtomicBool::new(false)),
            quit_requested: Arc::new(AtomicBool::new(false)),
            quit_notify: Arc::new(tokio::sync::Notify::new()),
            exit_requested: None,
            trace: false,
            stats: None,
            handlers: HashMap::new(),
            scheduler: Rc::new(DispatchScheduler::new()),
            reply_registry: Rc::new(InvocationReplyRegistry::new()),
            task_registry: Rc::new(ClassCTaskRegistry::new()),
        }
    }

    fn with_output(stdout: Box<dyn Write>, stderr: Box<dyn Write>) -> Self {
        let mut g = Self::new();
        g.stdout = stdout;
        g.stderr = stderr;
        g
    }
}

/// Per-invocation activation context — the **P** bucket in the SPEC 18
/// Phase 2 WS3-C field classification.
///
/// WS3-C.2 introduces this struct as the first step of carving the
/// per-invocation activation record out of `Evaluator`. A future Class C
/// handler (spawned on a `LocalSet` task per the WS3-C.7 plan) carries
/// its own `InvocationCtx`, so source-file context and "am I executing
/// inside a handler body" remain private to the spawned task and cannot
/// be observed or mutated by another concurrently-running invocation.
///
/// **Current contents (C.4a — 8 fields):**
/// - [`current_file`](Self::current_file) (C.2): the source file the
///   active invocation is executing — moved off `Evaluator` so two
///   invocations running concurrently (under C.7) do not stomp each
///   other's file context, e.g. when one `source`s a sub-file while
///   another is executing a top-level script body.
/// - [`in_handler`](Self::in_handler) (C.2): non-zero while a handler
///   body is executing. Drives the `$event` read-only enforcement.
///   Pre-C.6b this moved in lockstep with the (now-retired)
///   `Evaluator::dispatch_depth` counter; as of C.6b the scheduler
///   RwLock is the sole reentrancy gate and `in_handler` survives
///   only as the `$event`-readonly signal.
/// - [`function_depth`](Self::function_depth) (C.4a): recursion-depth
///   counter for user-function calls.
/// - [`address_stack`](Self::address_stack) (C.4a): nested `address`
///   block targets for implicit `send`.
/// - [`current_line`](Self::current_line) (C.4a): line number of the
///   active statement for diagnostics/tracing.
/// - [`sync_self_func`](Self::sync_self_func) (C.4a): the recursing
///   function captured for `try_call_function_sync_block`'s self-
///   recursive sync fast path.
/// - [`var_slot_cache`](Self::var_slot_cache) (C.4a) and
///   [`var_slot_cache_f64`](Self::var_slot_cache_f64) (C.4a): raw-pointer
///   slot caches feeding the for-loop and sync-function fast paths.
///
/// SPEC 18 Phase 2 WS3-C.4a moved the last six fields above onto this
/// struct as a pure code-move with zero behaviour change. WS3-C.4b
/// added the `Evaluator::can_use_shared_scope_raw_slots()` audit gate
/// so the Category G call sites (for-loop top-level fast paths) cache
/// `frames[0]` slot pointers through a single named predicate; the
/// predicate returns `true` today and at every Phase 2 milestone (the
/// fast-path body is statically sync and therefore non-yielding, so
/// Class C interleaving cannot fire during the cached pointer's live
/// window — see that method's doc for the full safety model). Category
/// L pushes (numeric self-recursion args, function-param frame caching)
/// stay unconditional because their pointees are private to the active
/// sync run.
/// Runtime arity mode (0.29.0, decision D5). `Compatible` is the
/// historical default: a missing non-default parameter binds `nil`,
/// surplus arguments are silently ignored. `Strict` raises
/// `ARITY_MISMATCH` before entering the function and enforces builtin
/// contract arity. Exact arity becomes the language default only at an
/// announced major compatibility boundary (with explicit variadic
/// syntax landing first).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum ArityMode {
    #[default]
    Compatible,
    Strict,
}

/// One traceback-stack entry (see `InvocationCtx::call_frames`):
/// the callee's name, the CALLER-side line the call happened on, and
/// the CALLER's file at call time (the callee's own file is
/// `ctx.current_file` while it runs — `call_function` switches it to
/// the callee's `def_file`).
pub(crate) struct CallFrame {
    pub(crate) name: std::rc::Rc<str>,
    pub(crate) call_line: usize,
    pub(crate) caller_file: Option<std::rc::Rc<str>>,
}

pub(crate) struct InvocationCtx {
    /// Source file the active invocation is executing. `Some` while
    /// `exec_source` is on the stack (or `set_file` has been called by
    /// a runner); `None` in the REPL and in pure-library use.
    ///
    /// Plumbed through the error-reporting and trace paths in place of
    /// the former `Evaluator::current_file` field. The
    /// `exec_source` save/restore bracket is preserved exactly: take()
    /// on entry, restore on every exit path (normal return, parse
    /// error, per-line fallback, panic resume).
    ///
    /// `Rc<str>` since 0.29.0: `call_function` saves/restores it per
    /// call (switching to the callee's `MixFunction::def_file`) so the
    /// clone must be a refcount bump, not a heap copy.
    pub(crate) current_file: Option<std::rc::Rc<str>>,
    /// Non-zero while at least one handler body is executing.
    /// Incremented at `run_handlers_for` entry (the shared accounting
    /// site for both Class S inline and Class C spawn_local paths)
    /// and decremented on normal-completion exit. Used by
    /// [`Evaluator::check_event_readonly`] to refuse `$event = ...`
    /// assignments inside handler bodies.
    ///
    /// **Independence from serialization.** As of WS3-C.6b the
    /// dispatch admission gate is the `DispatchScheduler` RwLock;
    /// `in_handler` is no longer the reentrancy signal — it is solely
    /// the `$event` read-only signal. The two were lockstep through
    /// C.6a but split at C.6b. A cancellation that drops the dispatch
    /// future releases the scheduler guard (RAII) but leaves
    /// `in_handler > 0` (manual decrement after the await never runs)
    /// and similarly skips `run_handlers_for`'s scope-frame pop,
    /// `current_reply` restore, and any unpopped scope/cache state.
    /// C.7g B1 retired this hazard by isolating each invocation
    /// (Class S as well as Class C) in its own per-invocation
    /// activation built by [`Evaluator::for_invocation`] with its own
    /// [`InvocationCtx`], scoping any cancellation to that activation's
    /// private state — the parent evaluator is never the one carrying
    /// the bumped counter.
    pub(crate) in_handler: usize,
    /// Nesting depth of user-defined function calls on the current
    /// invocation. Zero at the top level / handler body entry. Used by
    /// the `Return` propagation guard (`Err(MixError::Return) if
    /// function_depth == 0` is the "return outside a function" error)
    /// and by the function-call sites to gate prelude/scope behavior.
    /// Per-invocation by construction: each Class C handler runs its
    /// own activation, so `function_depth` is private to that activation.
    pub(crate) function_depth: usize,
    /// Stack of in-flight `address "target" ... end` block targets.
    /// `.last()` is the active target consulted by the address-block
    /// unknown-name fallback in function-call dispatch. Per-invocation:
    /// address blocks are lexically scoped within a single body and
    /// must not leak across handler invocations.
    pub(crate) address_stack: Vec<String>,
    /// Source line number of the statement currently being executed.
    /// Written per-stmt before dispatch; consumed by tracing and by
    /// the error-reporting paths to attach a `line` to RuntimeErrors
    /// that lack a span. Per-invocation: tracing wants the line of
    /// whatever invocation is currently scheduled, not a shared "last
    /// line written" value that interleaving could scramble.
    pub(crate) current_line: usize,
    /// Traceback stack (0.29.0): one entry per *async* `call_function`
    /// activation. Pushed after the recursion gate, popped in the
    /// single cleanup. The sync fast paths deliberately do NOT push
    /// (they bail to the async path on anything nontrivial, and their
    /// per-call cost budget is what the fib benchmarks pin), so frames
    /// from those paths are absent — an accepted precision loss.
    /// Snapshotted into `ErrorInfo::frames` by
    /// `Evaluator::snapshot_error` at the first function/builtin
    /// boundary an error crosses, i.e. while this stack is still
    /// intact.
    pub(crate) call_frames: Vec<CallFrame>,
    /// Per-invocation copy of the arity mode (`true` = strict) — see
    /// [`Evaluator::set_arity_mode`]; copied from globals by
    /// `for_invocation`.
    pub(crate) arity_strict: bool,
    /// Set transiently inside `try_call_function_sync_block` so the
    /// recursive sync-path can dispatch back into the same function
    /// without re-resolving by name. Cleared on every exit path of the
    /// containing fast-path helper. Per-invocation by construction —
    /// it's set/cleared within a single sync run with no `.await`
    /// across the live window — but moved here so future audits don't
    /// have to remember the special case.
    pub(crate) sync_self_func: Option<Rc<MixFunction>>,
    /// Pre-resolved slot pointers for `Variable(name)` lookups that
    /// `try_eval_expr_sync` should consult before falling back to
    /// `scope.get`. Populated by:
    ///   - For-loop top-level fast path: pre-binds every Variable in
    ///     the RHS expression to its slot in `frames[0]`. The 1M-iter
    ///     hot loop reads through the cache instead of paying the
    ///     FxHash+probe per access.
    ///   - `try_call_function_sync_block` for functions with
    ///     `body_no_local_assigns`: pre-binds each param's slot in
    ///     the freshly-pushed function frame so the body's `$param`
    ///     reads (e.g. fib's three reads of `$n`) skip the probe.
    ///
    /// Key is `*const str` borrowed from a stable owner (the AST
    /// expression for the For-loop case, `MixFunction::params` for
    /// the sync-block case) — both outlive the cache entries by
    /// construction, so pushing is alloc-free (no `String::clone`
    /// per recursive call). Pointer equality vs the `Variable(name)`
    /// being resolved is by string contents, not address.
    ///
    /// SAFETY: same contract as `Scope::global_slot_ptr` /
    /// `Scope::current_frame_slot_ptr` — caller must ensure no
    /// insertions on the relevant frame while pointers are live.
    /// The hazard is **insertion into the pointee frame during the
    /// cache entry's live window**, not Class C interleaving in the
    /// abstract: a sync fast-path body cannot yield to another
    /// invocation, so even shared-`frames[0]` pointers are sound while
    /// the live window is statically sync. WS3-C.4b routes shared-
    /// global pushes through `Evaluator::can_use_shared_scope_raw_slots()`
    /// as the single audit point — see that method's doc for the full
    /// safety model and the conditions under which the predicate would
    /// need to flip to `false`.
    pub(crate) var_slot_cache: Vec<(*const str, *const Value)>,
    /// Parallel f64-typed slot cache for type-stable Number variables.
    /// Stores `*const f64` (a pointer to a live f64) so the same cache
    /// shape covers two cases:
    ///   - Numeric self-recursion: caller stack-allocates `[f64; N]`
    ///     args and pushes `&args[i] as *const f64`. Skips the 72-byte
    ///     `Value::Number(_)` wrap-then-unwrap that the Value cache
    ///     would force on every recursive call (fib hits this ~285k
    ///     times). Pointee is the caller's stack, private to the active
    ///     sync run; not subject to the C.4b shared-scope gate.
    ///   - For-loop top-level fast path: `var_f64` and `assign_f64`
    ///     are `*mut f64` into the loop counter / accumulator's
    ///     `Value::Number(_)` payload in `frames[0]`. These pushes
    ///     happen only after the call site has cleared the C.4b
    ///     shared-scope gate (`can_use_shared_scope_raw_slots()`); the
    ///     surrounding `Scope::global_slot_ptr` call is the gated
    ///     primitive that materialises the `*mut Value` they are
    ///     reinterpreted from.
    ///
    /// Walked first in `Variable` arms so the Value cache is bypassed
    /// when the variable is f64-typed. Empty-cache early-exit means
    /// non-numeric code pays nothing.
    pub(crate) var_slot_cache_f64: Vec<(*const str, *const f64)>,
    /// SPEC 18 Phase 2 WS3-C.7e — owned read permit for the in-flight
    /// Class C handler body.
    ///
    /// Set by `run_handlers_for` at top-of-iteration for `(Some(sched),
    /// true)` (async-entry-on-Class-C-chain) via
    /// [`DispatchScheduler::acquire_reader_dispatch`]; `take()`n
    /// and dropped at end-of-iteration. Between those two points, the
    /// permit lives here so that
    /// [`Evaluator::await_with_class_c_yield`] can `take()` it across
    /// every handler-body `.await` and reacquire afterwards — releasing
    /// the reader is the entire point of Class C concurrency.
    ///
    /// `None` outside the Class C async-body window (Class S inline
    /// path, Class C embedded plain entry running under the per-entry
    /// writer, REPL, library use). The helper checks `is_some()` to
    /// distinguish "we are mid-handler-body and need to yield the
    /// reader" from "no permit held; just await directly".
    pub(crate) class_c_read_permit: Option<tokio::sync::OwnedRwLockReadGuard<()>>,
    /// Per-invocation **copy** of the configured recursion-depth cap
    /// (knob B). Materialised from [`EvaluatorGlobals::limits`] so the
    /// per-call check (`function_depth >= recursion_limit`) is borrow-
    /// free on the fib-style hot path. Defaults to [`DEFAULT_RECURSION_LIMIT`].
    pub(crate) recursion_limit: usize,
    /// Per-invocation copy of the wall-clock budget (knob C). `None` =
    /// no limit. Armed lazily into [`Self::deadline`] on the first poll
    /// so the clock starts when this invocation begins executing, not
    /// when `set_limits` was called.
    pub(crate) time_limit: Option<std::time::Duration>,
    /// Lazily-armed absolute deadline for this invocation (knob C).
    /// `None` until the first statement poll computes
    /// `Instant::now() + time_limit`; thereafter compared at every poll.
    pub(crate) deadline: Option<std::time::Instant>,
    /// Throttle counter for the deadline check on the **sync** call
    /// paths (knob C). The per-statement poll covers loops and the async
    /// call path, but `try_call_function_sync*` recursion bypasses
    /// `execute`; this counts those calls so the wall-clock can be
    /// sampled every 2^16 without a clock read per call. Only advances
    /// when a `time_limit` is set.
    pub(crate) deadline_tick: u64,
    /// Per-invocation copies of the collection-size caps (knob D).
    /// `None` = no cap (zero overhead). Checked by
    /// [`Evaluator::check_collection_size`] at every growth site.
    pub(crate) max_list_len: Option<usize>,
    pub(crate) max_map_len: Option<usize>,
    pub(crate) max_string_len: Option<usize>,
    /// Canonical keys of `require()` calls currently on this
    /// invocation's stack — cycle detection + the nesting depth cap.
    /// Per-invocation so concurrent Class C handlers requiring the
    /// same module never false-positive on each other's in-flight
    /// loads.
    pub(crate) require_stack: Vec<String>,
}

impl InvocationCtx {
    fn new() -> Self {
        InvocationCtx {
            current_file: None,
            in_handler: 0,
            function_depth: 0,
            address_stack: Vec::new(),
            current_line: 0,
            call_frames: Vec::new(),
            arity_strict: false,
            sync_self_func: None,
            var_slot_cache: Vec::new(),
            var_slot_cache_f64: Vec::new(),
            class_c_read_permit: None,
            recursion_limit: DEFAULT_RECURSION_LIMIT,
            time_limit: None,
            deadline: None,
            deadline_tick: 0,
            max_list_len: None,
            max_map_len: None,
            max_string_len: None,
            require_stack: Vec::new(),
        }
    }

    /// Copy the configured limits out of [`EvaluatorGlobals`] into this
    /// per-invocation context so the hot-path checks read local fields
    /// (no `RefCell` borrow per call/op). `deadline` is left `None` to
    /// re-arm on this invocation's first poll. Called at every ctx
    /// construction site that shares already-configured globals.
    fn apply_limits(&mut self, limits: &EvalLimits) {
        self.recursion_limit = limits.recursion_limit;
        self.time_limit = limits.time_limit;
        self.deadline = None;
        self.max_list_len = limits.max_list_len;
        self.max_map_len = limits.max_map_len;
        self.max_string_len = limits.max_string_len;
    }
}

pub struct Evaluator {
    /// SPEC 18 Phase 2 WS3-C.5 — shared **S**-bucket fields grouped
    /// behind a single embedded struct, wrapped in `Rc<RefCell<_>>`
    /// so per-invocation Class C tasks (landing in C.7) can hold a
    /// clone and serialize access via short, **no-`.await`** borrows.
    ///
    /// Borrow discipline (load-bearing — Class C async cooperates by
    /// convention here, not by compiler enforcement):
    /// * Sync reads/writes use `self.globals.borrow()` /
    ///   `self.globals.borrow_mut()` in the smallest scope possible.
    /// * Any value that the caller needs across a `.await` is **cloned
    ///   out** of the borrow into a local, and the `Ref`/`RefMut`
    ///   guard is dropped (typically via `let x = { … };`) before the
    ///   `.await`. C.3 already reshaped the await-capable handles
    ///   (`bus_handler`, `shell_handler`, `serve_runtime`) to support
    ///   this — they are `Option<Rc<dyn …>>` so the clone is cheap.
    /// * Output writes (`stdout`/`stderr`) and the alias map are
    ///   accessed inside the same `borrow_mut()` window as their
    ///   surrounding state; mixed-borrow sites that cannot be expressed
    ///   that way are reshaped in C.5b.
    globals: Rc<RefCell<EvaluatorGlobals>>,
    /// Borrow-free hot-path mirror of whether `globals.stats` is attached.
    stats_attached: bool,
    /// SPEC 18 Phase 2 WS3-C.4a — per-invocation activation context.
    /// All **P**-bucket fields live here: `current_file`, `in_handler`
    /// (`$event` read-only signal), `function_depth`, `address_stack`,
    /// `current_line`, `sync_self_func`, `var_slot_cache`,
    /// `var_slot_cache_f64`. Each Class C invocation will own its own
    /// `InvocationCtx` (set up in C.7) so the raw-pointer caches and
    /// activation counters do not collide across interleaved handlers.
    /// C.4b routes the **shared-global** raw-pointer cache pushes
    /// (Category G — pointers into `Scope::frames[0]`) through
    /// `can_use_shared_scope_raw_slots()` as the single named audit
    /// point; the local-frame / stack-backed cache uses (Category L —
    /// numeric self-recursion args, function-param frame caching) stay
    /// unconditional because their pointees are private to the active
    /// sync run.
    ctx: InvocationCtx,
    scope: Scope,
    /// SPEC 18 Phase 2 WS3-C.7c — reply handle for the request the
    /// currently-running handler body is servicing. Replaces the
    /// pre-C.7c stack-local `Option<EventReplyMeta>` with a shared
    /// `Rc<InvocationReplyHandle>` registered in
    /// [`EvaluatorGlobals::reply_registry`] so the C.7f shutdown
    /// drain can enumerate it and synthesize an unanswered §3.4
    /// reply if the citizen shuts down mid-dispatch.
    ///
    /// `Some` only between the save/restore bracket in
    /// `run_handlers_for`; `None` in the main body, when idle, or
    /// while dispatching an event with no registered handlers. The
    /// `reply()` builtin reads this; a `None` here is the structural
    /// "reply() called outside an `on` handler" error. The §3.4
    /// fault-isolation and the (future) C.7f shutdown drain both call
    /// [`InvocationReplyHandle::reply_once`] through this handle, so
    /// double-reply (a script that called `reply()` then panicked, or
    /// a shutdown drain racing a normal completion) is silently
    /// no-op'd by reply-once CAS rather than firing duplicate wire
    /// messages.
    current_reply: Option<Rc<InvocationReplyHandle>>,
    /// SPEC 18 Phase 2 WS3-C.7b — γ activation role.
    ///
    /// `false` for the top-level (script-owning) evaluator;
    /// `true` for an `Evaluator` constructed by
    /// [`Self::for_invocation`] to run a single Class C handler
    /// invocation on a citizen-scoped `LocalSet` (C.7d wires the
    /// spawn). The audit gate
    /// [`Self::can_use_shared_scope_raw_slots`] flips on this flag:
    /// per-invocation evaluators cannot use the raw-`*mut Value`
    /// fast-path caches because the shared `Rc<RefCell<_>>` global
    /// frame can be rehashed by a peer invocation's insert during a
    /// yield, invalidating any cached pointer mid-handler. The
    /// `scope.uses_shared_globals()` predicate covers the same
    /// hazard from the Scope side; both checks together protect
    /// every legacy fast path that assumed "globals live in
    /// `frames[0]`."
    is_per_invocation: bool,
}

impl Default for Evaluator {
    fn default() -> Self {
        Self::new()
    }
}

impl ShellVarResolver for Evaluator {
    fn resolve(&self, name: &str) -> Option<String> {
        // SPEC 18 Phase 2 WS3-C.7b: γ-aware lookup so a per-invocation
        // (Class C) Evaluator's shell-interp `$VAR` reads still see
        // citizen-wide globals living behind `Rc<RefCell<_>>`.
        if let Some(val) = self.scope.get_value(name) {
            Some(val.to_mix_string())
        } else {
            std::env::var(name).ok()
        }
    }

    /// Execute a shell-dispatch `$(...)` substitution via `/bin/sh -c`
    /// (matching `run()`/`run_rc()`), returning captured stdout with
    /// trailing newlines stripped. A spawn failure or non-UTF-8 output is
    /// tolerated as empty (`Some("")`) rather than aborting the whole
    /// line, mirroring the lenient REPL posture. stderr is inherited so
    /// the inner command's diagnostics still reach the terminal.
    fn command_subst(&self, cmd: &str) -> Option<String> {
        let output = std::process::Command::new("/bin/sh")
            .arg("-c")
            .arg(cmd)
            .stderr(std::process::Stdio::inherit())
            .output()
            .ok()?;
        let mut s = String::from_utf8_lossy(&output.stdout).into_owned();
        while s.ends_with('\n') || s.ends_with('\r') {
            s.pop();
        }
        Some(s)
    }
}

impl Evaluator {
    pub fn new() -> Self {
        Evaluator {
            globals: Rc::new(RefCell::new(EvaluatorGlobals::new())),
            stats_attached: false,
            ctx: InvocationCtx::new(),
            scope: Scope::new(),
            current_reply: None,
            is_per_invocation: false,
        }
    }

    pub fn with_output(stdout: Box<dyn Write>, stderr: Box<dyn Write>) -> Self {
        Evaluator {
            globals: Rc::new(RefCell::new(EvaluatorGlobals::with_output(stdout, stderr))),
            stats_attached: false,
            ctx: InvocationCtx::new(),
            scope: Scope::new(),
            current_reply: None,
            is_per_invocation: false,
        }
    }

    /// SPEC 18 Phase 2 WS3-C.7b — construct a fresh per-invocation
    /// Evaluator for a Class C handler dispatch (γ activation).
    ///
    /// The new Evaluator:
    /// - **Shares** `globals: Rc<RefCell<EvaluatorGlobals>>` with the
    ///   parent (handler registry, Bus handler, output policy,
    ///   subscription state — everything in `EvaluatorGlobals`).
    /// - **Shares** the global Scope frame and the function registry
    ///   via [`Scope::shared_globals_handle`] / [`Scope::shared_functions_handle`]
    ///   on the parent's Scope (lazy-promoted on first call). Peer
    ///   invocations and the resumed parent see the same global
    ///   state.
    /// - Gets a **fresh** `InvocationCtx::new()` — the P-bucket
    ///   fields (`$event` body, `current_file`, `current_line`,
    ///   `function_depth`, `address_stack`, `var_slot_cache{,_f64}`,
    ///   `sync_self_func`, `in_handler` `$event`-readonly signal,
    ///   §3.4 fault baselines) are per-activation and never
    ///   collide across interleaved Class C invocations.
    /// - Gets a **fresh** `Scope` with shared globals/functions but
    ///   private local frames; `restore_frames_to` only truncates
    ///   the local frames, never the shared maps (the load-bearing
    ///   γ invariant — a panic in one invocation cannot erase
    ///   another's globals).
    /// - Gets `None` `current_reply`. The legacy `pending_events`
    ///   FIFO queue was retired by C.7d when the LocalSet-spawned
    ///   chain task model replaced the reentrancy-queueing dispatch
    ///   shape; there is no per-evaluator event queue any more.
    /// - Sets `is_per_invocation = true`, flipping the audit gate
    ///   [`Self::can_use_shared_scope_raw_slots`] to `false` so the
    ///   For-loop / fib-style raw-pointer fast paths are disabled
    ///   for this evaluator — the shared `RefCell` global frame
    ///   cannot hand out a `*mut Value` that survives a peer
    ///   invocation's insert.
    ///
    /// **Live call sites.** Both arms of `dispatch_event` call this
    /// per dispatch:
    ///
    /// * Class C (C.7d): one activation per `tokio::task::spawn_local`
    ///   invocation, run concurrently with peers and the outer pump.
    /// * Class S (C.7g B1): one activation per chain, awaited inline
    ///   under the writer permit. Inline-await preserves the "Class S
    ///   blocks `dispatch_event` for the chain's duration" atomic
    ///   contract while the activation isolates cancellation damage:
    ///   a `dispatch_event` future dropped mid-await discards the
    ///   activation's private ctx/local frames without touching the
    ///   parent evaluator.
    ///
    /// Takes `&mut self` because the lazy promotion of
    /// `Scope::shared_globals_handle` / `shared_functions_handle`
    /// mutates the parent Scope on first call (hoisting owned
    /// `frames[0]` / `functions` into `Rc<RefCell<_>>`). Promotion
    /// happens at dispatch setup before the handler body runs,
    /// outside any raw-slot live window (the existing fast-path AST
    /// classifiers make that window structurally non-yielding).
    pub(crate) fn for_invocation(&mut self) -> Evaluator {
        let globals = Rc::clone(&self.globals);
        let shared_globals = self.scope.shared_globals_handle();
        let shared_functions = self.scope.shared_functions_handle();
        // Materialise the configured limits into the fresh activation's
        // ctx so this invocation's hot-path checks are borrow-free and
        // inherit the embedder's policy. `deadline` re-arms on this
        // invocation's first poll (a per-dispatch wall-clock budget).
        let mut ctx = InvocationCtx::new();
        ctx.apply_limits(&self.globals.borrow().limits);
        ctx.arity_strict = self.globals.borrow().arity_strict;
        Evaluator {
            globals,
            stats_attached: self.stats_attached,
            ctx,
            scope: Scope::with_shared(shared_globals, shared_functions),
            current_reply: None,
            is_per_invocation: true,
        }
    }

    /// Set the Bus handler for send/address/emit operations.
    ///
    /// Accepts `Rc<dyn BusHandler>` (not `Box`) per SPEC 18 Phase 2
    /// WS3-C.3 so per-invocation tasks can hold their own clone and
    /// release the shared-globals borrow before each `.await`.
    pub fn set_bus_handler(&mut self, handler: Rc<dyn BusHandler>) {
        self.globals.borrow_mut().bus_handler = Some(handler);
    }

    /// Install the host database seam consulted by `db_query`/`db_exec`.
    ///
    /// Accepts `Rc<dyn DbHandler>` (not `Box`), same per-invocation
    /// clone-out discipline as [`set_bus_handler`](Self::set_bus_handler).
    /// Until set, `db_query`/`db_exec` raise a catchable "database not
    /// available" error; the capability gate (`CapabilityClass::Db`)
    /// still applies independently, so a policy can deny DB access even
    /// when a handler is present.
    pub fn set_db_handler(&mut self, handler: Rc<dyn DbHandler>) {
        self.globals.borrow_mut().db_handler = Some(handler);
    }

    /// Install the host JMAP seam consulted by the `jmap` builtin. Same
    /// clone-out discipline as [`set_db_handler`](Self::set_db_handler).
    /// Until set, `jmap` raises a catchable "jmap not available" error;
    /// the capability gate (`CapabilityClass::Jmap`) still applies
    /// independently, so a policy can deny JMAP access even when a
    /// handler is present.
    pub fn set_jmap_handler(&mut self, handler: Rc<dyn JmapHandler>) {
        self.globals.borrow_mut().jmap_handler = Some(handler);
    }

    /// Install the host Bus delegated-call seam consulted by the `bus_call`
    /// builtin. Same clone-out discipline as
    /// [`set_jmap_handler`](Self::set_jmap_handler). Until set, `bus_call`
    /// raises a catchable "bus_call not available" error; the capability
    /// gate (`CapabilityClass::Bus`) still applies independently, so a
    /// policy can deny delegated Bus even when a handler is present.
    pub fn set_bus_call_handler(&mut self, handler: Rc<dyn BusCallHandler>) {
        self.globals.borrow_mut().bus_call_handler = Some(handler);
    }

    /// Install the per-line shell fallback used by `exec_source`. See
    /// [`ShellHandler`] for the contract. `source` and top-level script files
    /// use it only after their whole-file Mix path loses, except that an
    /// explicitly continued line classified as shell must win before a
    /// coincidentally-valid arithmetic parse.
    pub fn set_shell_handler(&mut self, handler: Rc<dyn ShellHandler>) {
        self.globals.borrow_mut().shell_handler = Some(handler);
    }

    /// Parse and execute top-level script source, retaining whole-file Mix as
    /// the authoritative fast path and using the registered [`ShellHandler`]
    /// only when an unambiguous shell line makes the strict per-line fallback
    /// eligible. The lexer consumes shared continuation sites while retaining
    /// physical spans; the fallback splices those same sites before any shell
    /// classification.
    pub async fn execute_script_source(&mut self, source: &str) -> MixResult<Value> {
        let logical = crate::continuation::splice_with_metadata(source)?;
        let forced_shell_lines =
            self.continued_lines_classified_as_shell(&logical.source, &logical.continued_lines);
        let parsed = crate::lexer::Lexer::new(source)
            .tokenize()
            .and_then(|tokens| {
                let mut parser = crate::parser::Parser::new_speculative(tokens, source);
                let result = parser.parse_program();
                if result.is_ok() {
                    for warning in parser.take_deprecations() {
                        eprintln!("{}", warning);
                    }
                }
                result
            });
        match parsed {
            Ok(stmts) if forced_shell_lines.is_empty() => self.execute(&stmts).await,
            Ok(_) => {
                let marker = MixError::ParseError {
                    msg: "continued command line requires shell classification".to_string(),
                    span: Span {
                        line: forced_shell_lines[0],
                        column: 1,
                        file: None,
                    },
                };
                self.exec_source_per_line(&logical.source, marker, &forced_shell_lines)
                    .await
            }
            Err(error)
                if !logical.continued_lines.is_empty()
                    && self.globals.borrow().shell_handler.is_some() =>
            {
                self.exec_source_per_line(&logical.source, error, &forced_shell_lines)
                    .await
            }
            Err(error) => Err(error),
        }
    }

    /// Continued logical lines are the only successful-Mix parses for which a
    /// script file must still let the shell-first classifier win. Otherwise,
    /// `ls -d \\` followed by `/tmp` parses as arithmetic. Track literal
    /// aliases on the way so the decision matches the fallback's Phase 1 view.
    fn continued_lines_classified_as_shell(
        &self,
        content: &str,
        continued_lines: &[usize],
    ) -> Vec<usize> {
        if continued_lines.is_empty() {
            return Vec::new();
        }
        let Some(handler) = self.globals.borrow().shell_handler.clone() else {
            return Vec::new();
        };
        let mut aliases = self.globals.borrow().aliases.clone();
        let mut shell_lines = Vec::new();
        for (idx, raw) in content.lines().enumerate() {
            let line_no = idx + 1;
            let line = raw.trim_end_matches('\r');
            if line.trim().is_empty() {
                continue;
            }
            let parsed = crate::lexer::Lexer::new(line)
                .tokenize()
                .and_then(|tokens| {
                    crate::parser::Parser::new_speculative(tokens, line).parse_program()
                });
            if continued_lines.binary_search(&line_no).is_ok()
                && matches!(handler.classify(line, &aliases), Ok(ShellLineKind::Shell))
            {
                shell_lines.push(line_no);
            }
            if let Ok(stmts) = parsed {
                for stmt in stmts {
                    if let crate::ast::StmtKind::Alias {
                        name: Some(crate::ast::Expr::StringLiteral(name)),
                        command: Some(crate::ast::Expr::StringLiteral(command)),
                    } = stmt.kind
                    {
                        aliases.insert(name, command);
                    }
                }
            }
        }
        shell_lines
    }

    /// Install the SPEC 18 WS4 runtime-reserved Ch07 L0+ surface.
    ///
    /// Called only by the serve entrypoint. Once set, `run_event_pump`
    /// services `HELP`/`INFO`/`QUIT`/`<svc>.props.{get,list,describe}`
    /// pre-dispatch — author `on` handlers naming those verbs are
    /// shadowed by design (DECIDED §7-Q4: runtime wins).
    pub fn set_serve_runtime(&mut self, runtime: Rc<dyn ServeRuntime>) {
        self.globals.borrow_mut().serve_runtime = Some(runtime);
    }

    /// Install a per-evaluator capability gate over the builtin table.
    ///
    /// Once set, every builtin/HOF dispatch consults the policy and a
    /// denied call raises a `capability denied` runtime error. The
    /// default (no policy) is fully permissive. See [`CapabilityPolicy`].
    /// Stored in shared globals so [`Self::for_invocation`] activations
    /// inherit it.
    pub fn set_capability_policy(&mut self, policy: Rc<dyn CapabilityPolicy>) {
        self.globals.borrow_mut().capability_policy = Some(policy);
    }

    /// Set the runtime arity mode (0.29.0, decision D5). `Compatible`
    /// (the default) keeps the historical binding — missing params bind
    /// nil, surplus args are ignored. `Strict` raises a catchable
    /// `ARITY_MISMATCH` structured error BEFORE entering a user
    /// function whose call arity falls outside min..=max (min = params
    /// without defaults), and rejects builtin/HOF calls whose argument
    /// count the contract metadata does not accept. The `mix` binary
    /// exposes this as `--strict-arity`.
    pub fn set_arity_mode(&mut self, mode: ArityMode) {
        let strict = mode == ArityMode::Strict;
        self.ctx.arity_strict = strict;
        self.globals.borrow_mut().arity_strict = strict;
    }

    /// Configure the native self-protection limits (recursion depth,
    /// wall-clock time, collection sizes) for this evaluator.
    ///
    /// Writes both the shared source-of-truth (so
    /// [`Self::for_invocation`] activations inherit it) and this
    /// evaluator's own per-invocation ctx copy (so a direct
    /// `execute` on this evaluator sees it immediately). See [`EvalLimits`].
    pub fn set_limits(&mut self, limits: EvalLimits) {
        self.ctx.apply_limits(&limits);
        self.globals.borrow_mut().limits = limits;
    }

    /// Total number of registered event handlers across all commands.
    ///
    /// Used by the script runner to decide whether to enter an auto-pump
    /// loop after the main body returns: a script with no handlers exits
    /// normally; a script with one or more registered handlers stays alive
    /// to service incoming messages until interrupted.
    pub fn handler_count(&self) -> usize {
        self.globals
            .borrow()
            .handlers
            .values()
            .map(|v| v.len())
            .sum()
    }

    /// Number of distinct commands with at least one registered handler.
    pub fn handler_command_count(&self) -> usize {
        self.globals.borrow().handlers.len()
    }

    /// SPEC 18 Phase 2 WS2 — registry inspector. Returns the `is_async`
    /// class flag of the registered handler at `idx` for `command`, or
    /// `None` if the command/index is unregistered.
    ///
    /// Public so integration tests in the `tests/` crate (and future
    /// diagnostic / REPL tooling that wants to surface "which handlers
    /// are Class C") can introspect the registry without reaching into
    /// crate internals. Until WS3-C wires dispatch this is also the
    /// only reader of `HandlerEntry::is_async`; the production
    /// registration arm separately emits a `tracing::debug!` line when
    /// it stores a Class C entry (`class = "async"`) for journal
    /// observability.
    pub fn handler_is_async(&self, command: &str, idx: usize) -> Option<bool> {
        self.globals
            .borrow()
            .handlers
            .get(command)?
            .get(idx)
            .map(|e| e.is_async)
    }

    /// True when a dispatch is in progress (i.e. the scheduler's
    /// admission write guard is currently held). As of WS3-C.6b this
    /// reads the `DispatchScheduler` directly; the legacy
    /// `dispatch_depth` counter that previously backed this method
    /// was retired in the same slice.
    pub fn is_dispatching(&self) -> bool {
        self.globals.borrow().scheduler.is_dispatching()
    }

    /// SPEC 18 Phase 2 WS3-C.7d — number of live Class C task handles
    /// in the per-citizen registry. Each spawned Class C dispatch adds
    /// one entry (on `tokio::task::spawn_local`) and removes it on
    /// normal completion; a cancelled or aborted task leaves its entry
    /// for the C.7f shutdown drain.
    ///
    /// Exposed for integration tests that need to assert "all spawned
    /// Class C tasks have completed" after advancing the LocalSet. The
    /// reported count is a snapshot at the call site; callers that
    /// expect zero must have already yielded enough times for the
    /// spawned tasks to run their self-removal step.
    pub fn class_c_task_count(&self) -> usize {
        self.globals.borrow().task_registry.len()
    }

    /// SPEC 18 Phase 2 WS3-C.7e — yield discipline helper.
    ///
    /// Brackets a single handler-body `.await` with the
    /// "release-the-read-permit-then-reacquire" dance that makes Class
    /// C concurrency real:
    ///
    /// 1. `take()` the owned read permit from
    ///    [`InvocationCtx::class_c_read_permit`] (if present).
    /// 2. Drop the permit BEFORE awaiting `fut` — this lets a queued
    ///    Class S writer admit, or peer Class C readers continue
    ///    making progress past their own yield points.
    /// 3. `await` `fut` to completion.
    /// 4. Reacquire via
    ///    [`DispatchScheduler::acquire_reader_dispatch`] and install
    ///    the new owned permit on `ctx.class_c_read_permit`.
    /// 5. Return the inner future's output.
    ///
    /// **No-permit fast path.** When `ctx.class_c_read_permit` is
    /// `None` — Class S parent path (writer held; never released here),
    /// Class C embedded plain entry (writer held; never released here),
    /// REPL / library use (no scheduler dispatch at all) — the helper
    /// awaits `fut` directly and returns. This makes the helper safe
    /// to wrap unconditionally around every handler-body await without
    /// distinguishing the caller's chain class at the call site.
    ///
    /// **Cancellation contract.** If `fut.await` is cancelled, the
    /// permit is gone (we already dropped it) and the helper future
    /// itself is dropped — the partial activation is discarded by
    /// C.7d's per-invocation task isolation (Class C cancellation is
    /// task-scoped; see [`DispatchScheduler::acquire_writer_class_c_entry`]
    /// doc for why this is safe).
    ///
    /// **Reacquire is infallible.** `acquire_reader_dispatch` returns
    /// the read guard directly (no `Result`) — the scheduler-wide
    /// poison story was retired in C.7g B2 once both arms became
    /// activation-isolated.
    ///
    /// **Borrow discipline at call sites.** Callers MUST NOT hold any
    /// `RefCell` borrow or `&mut Value` across this helper — the
    /// helper's reacquire path takes
    /// `self.globals.borrow().scheduler.clone()`, and a live
    /// `globals.borrow_mut()` would panic. Clone all inputs out before
    /// invoking. The most natural pattern is:
    ///
    /// ```ignore
    /// // build `fut` from clones, then:
    /// let out = self.await_with_class_c_yield(fut).await?;
    /// ```
    pub(crate) async fn await_with_class_c_yield<F, T>(&mut self, fut: F) -> MixResult<T>
    where
        F: std::future::Future<Output = T>,
    {
        let permit = self.ctx.class_c_read_permit.take();
        if permit.is_none() {
            return Ok(fut.await);
        }
        drop(permit);

        let out = fut.await;

        let scheduler = { self.globals.borrow().scheduler.clone() };
        let guard = scheduler.acquire_reader_dispatch().await;
        self.ctx.class_c_read_permit = Some(guard);

        Ok(out)
    }

    /// SPEC 18 Phase 2 WS3-C.7f — drain in-flight Class C tasks under
    /// a bounded grace window, then synthesize §3.4 error replies for
    /// any unanswered requests left stranded by cancellation.
    ///
    /// **Sequence:**
    /// 1. Snapshot the initial task count and pending-request count
    ///    for the returned [`ClassCDrainOutcome`].
    /// 2. Drain the [`ClassCTaskRegistry`] into a local `Vec` and run
    ///    a `FuturesUnordered`-vs-sleep loop: each task that completes
    ///    naturally before the deadline counts as `drained_clean`.
    ///    (See "Why `FuturesUnordered` + `AbortHandle`" below for why
    ///    `select_all` is *not* the right primitive here.)
    /// 3. On grace expiry, call [`tokio::task::JoinHandle::abort`]
    ///    on every survivor and best-effort join them (100 ms cap)
    ///    so the LocalSet has a chance to propagate the cancellation
    ///    before we return.
    /// 4. Snapshot the reply registry, filter by
    ///    [`InvocationReplyHandle::is_pending_request`], and for each
    ///    surviving pending request fire
    ///    [`InvocationReplyHandle::synthesize_unanswered`] with
    ///    `rc=2` and a SPEC 18 §3.4 shutdown body — UNLESS
    ///    `allow_synth_replies` is false (transport-drop path: the
    ///    socket is already gone, so there is no caller channel
    ///    left to reply on).
    ///
    /// **Why `FuturesUnordered` + `AbortHandle` (and not `timeout(
    /// grace, join_all)` or `select_all`):** when `tokio::time::
    /// timeout` fires on a `join_all`, the individual `JoinHandle`s
    /// inside are dropped, which *detaches* the tasks (a dropped
    /// `JoinHandle` does not cancel its task — `.abort()` must be
    /// called explicitly). `select_all` consumes its input `Vec` on
    /// every `.await`, which would force us to rebuild the residue
    /// every iteration AND would lose the survivor handles entirely
    /// on the unselected sleep branch (the tokio::select! evaluates
    /// both branches up front, so the `select_all`-future captures
    /// the handles on construction; when the sleep branch wins, the
    /// `select_all` future is dropped and the handles with it).
    ///
    /// The fix is `FuturesUnordered<JoinHandle<()>>` for the polling
    /// surface PLUS a parallel `Vec<AbortHandle>` snapshot taken
    /// BEFORE the handles move into the stream. `AbortHandle::abort`
    /// works on completed tasks too (it's a no-op there), so we can
    /// fire it across the whole `Vec` on grace expiry without having
    /// to track which specific tasks survived.
    ///
    /// **Borrow discipline.** Holds NO `RefCell` borrow across any
    /// `.await`: registries are cloned out of `globals` at entry,
    /// the task registry's `drain()` empties its inner `RefCell` in
    /// a single non-await borrow, and the reply registry's
    /// `snapshot()` likewise collects in one borrow before the synth
    /// loop awaits.
    ///
    /// **When to call.** Strictly after the event pump has stopped
    /// accepting new inbound (i.e. the `run_event_pump` future has
    /// returned, or the citizen's shutdown signal has fired and the
    /// `select!` between the two has resolved). Calling while the
    /// pump is still admitting new chains races the task registry —
    /// a freshly-spawned chain after `drain()` is not visible to the
    /// drain loop and would survive past process exit. The §3.5
    /// shutdown sequence in `cosmix-mix::run_serve` already
    /// guarantees this ordering; library callers MUST replicate it.
    #[cfg(feature = "tokio-sleep")]
    pub async fn drain_class_c_for_shutdown(
        &self,
        grace: std::time::Duration,
        allow_synth_replies: bool,
    ) -> ClassCDrainOutcome {
        let (task_registry, reply_registry) = {
            let g = self.globals.borrow();
            (Rc::clone(&g.task_registry), Rc::clone(&g.reply_registry))
        };

        let mut outcome = ClassCDrainOutcome {
            initial_tasks: task_registry.len(),
            initial_pending: reply_registry
                .snapshot()
                .iter()
                .filter(|h| h.is_pending_request())
                .count(),
            ..Default::default()
        };

        // Phase 1: drain task handles, await each clean completion up
        // to the deadline.
        use futures_util::stream::{FuturesUnordered, StreamExt};

        let drained_handles: Vec<tokio::task::JoinHandle<()>> = task_registry.drain();
        if !drained_handles.is_empty() {
            // Snapshot AbortHandle BEFORE moving the JoinHandles into
            // the FuturesUnordered — once they move, we no longer have
            // an &JoinHandle to call .abort_handle() on. AbortHandle is
            // a no-op on completed tasks, so calling it across the
            // whole Vec on survivor-abort is safe.
            let abort_handles: Vec<tokio::task::AbortHandle> =
                drained_handles.iter().map(|h| h.abort_handle()).collect();
            let mut pending: FuturesUnordered<tokio::task::JoinHandle<()>> =
                drained_handles.into_iter().collect();

            let sleep = tokio::time::sleep(grace);
            tokio::pin!(sleep);

            loop {
                if pending.is_empty() {
                    break;
                }
                tokio::select! {
                    biased;
                    _ = &mut sleep => break,
                    Some(_join_result) = pending.next() => {
                        // One handle resolved; loop. We do NOT inspect
                        // `_join_result` (a `JoinError` on a pre-abort
                        // task body panic is logged at the §3.4 fault
                        // boundary and still counts as "drained clean"
                        // — the task is gone without our intervention).
                        outcome.drained_clean += 1;
                    }
                }
            }

            // Phase 2: abort survivors and best-effort join. Survivor
            // count = the residue still in `pending` after the deadline
            // fired. `AbortHandle::abort()` on an already-completed
            // task is a no-op, so we can fire it across the whole
            // `abort_handles` snapshot without filtering. The 100 ms
            // join cap is generous for aborted-task wind-down; a wedged
            // task that does not observe abort is the §3.5
            // process::exit's problem, not the drain's.
            outcome.aborted = pending.len();
            if outcome.aborted > 0 {
                for ah in &abort_handles {
                    ah.abort();
                }
                let _ = tokio::time::timeout(std::time::Duration::from_millis(100), async {
                    while pending.next().await.is_some() {}
                })
                .await;
            }
        }

        // Phase 3: walk reply registry for stranded pending requests
        // and synthesize §3.4 shutdown replies (when allowed).
        let snapshot = reply_registry.snapshot();
        for handle in snapshot.iter().filter(|h| h.is_pending_request()) {
            if !allow_synth_replies {
                outcome.synth_skipped_no_socket += 1;
                continue;
            }
            match handle
                .synthesize_unanswered(SHUTDOWN_SYNTH_RC, SHUTDOWN_SYNTH_BODY)
                .await
            {
                Ok(()) => outcome.synth_sent += 1,
                Err(_) => outcome.synth_failed += 1,
            }
        }

        outcome
    }

    /// Dispatch an event: classify the registered chain for
    /// `event.command`, register the reply correlation, and route to
    /// the Class S inline path or the Class C `tokio::task::spawn_local`
    /// path per [`ChainClass::from_entries`] (plan §4 WS3
    /// "Mixed-class-per-event rule — Class C if ANY matching handler is
    /// `async`").
    ///
    /// **Class S (atomic).** Acquires the scheduler writer permit via
    /// [`DispatchScheduler::acquire_writer_dispatch`], builds a
    /// per-invocation activation via [`Evaluator::for_invocation`],
    /// and awaits the chain inline on that activation (C.7g B1). No
    /// other handler invocation (Class S or Class C) progresses until
    /// the chain completes; today's atomic dispatch semantics are
    /// preserved bit-for-bit, and cancellation between admission and
    /// return discards the activation's private `InvocationCtx` /
    /// local `Scope` frames without touching `self`.
    ///
    /// **Class C (spawnable).** Builds a per-invocation `Evaluator` via
    /// [`Evaluator::for_invocation`] (γ activation — fresh
    /// `InvocationCtx`, private local Scope frames, shared globals)
    /// and `tokio::task::spawn_local`s the chain body on the ambient
    /// LocalSet. Returns *immediately* — the chain runs concurrently
    /// with other Class C invocations, gated by the scheduler reader
    /// permit acquired inside the task body so a Class S writer can
    /// preempt new starts. The task's `JoinHandle` is registered in
    /// [`ClassCTaskRegistry`] for C.7f shutdown-drain enumeration; the
    /// spawned task removes its own entry on normal completion.
    ///
    /// **Requires a `LocalSet` context for Class C events.**
    /// `tokio::task::spawn_local` panics outside a `LocalSet`. The
    /// production runners (`run_source` / serve) wrap their async
    /// blocks in `LocalSet::new().run_until(...)`. Tests that exercise
    /// only Class S handlers do not need a LocalSet; tests that fire
    /// async handlers must wrap accordingly.
    ///
    /// Error semantics: handler body errors are caught inside
    /// `run_handlers_for`, logged via `tracing::error!`, and do NOT
    /// propagate. The handler stays registered, and subsequent events
    /// continue to be servable. Main-body errors still crash the script
    /// loudly — only handler errors are soft.
    pub async fn dispatch_event(&mut self, event: IncomingEvent) -> MixResult<()> {
        // Snapshot the handler chain for this command. An empty (or
        // missing) entry list is the fast no-handler path: drop the
        // message on the floor without taking any scheduler permit and
        // without registering a reply correlation (Codex C.7c pre-impl
        // directive: do not store handles for commands with no
        // handlers). WS3-C.5 clone-out: scope the borrow inside a block
        // so the `Ref` does not survive the awaits below.
        let entries: Vec<HandlerEntry> = match self.globals.borrow().handlers.get(&event.command) {
            Some(v) if !v.is_empty() => v.clone(),
            _ => return Ok(()),
        };
        let chain_class = ChainClass::from_entries(&entries);
        tracing::trace!(
            command = %event.command,
            chain_len = entries.len(),
            chain_class = ?chain_class,
            "Phase 2 WS3-C.7d: classified handler chain (routed)"
        );

        // Register the request-correlation parts in the per-citizen
        // reply registry so a `reply()` from any of these handler
        // bodies answers *this* event AND the C.7f shutdown drain can
        // synthesize an unanswered §3.4 reply by enumerating live
        // handles. Registration happens *here* (in dispatch_event)
        // rather than inside the per-event run_handlers_for call so
        // that an accepted-but-not-yet-polled Class C task is
        // drain-visible from the moment the spawn returns — the spawn
        // future may not run its first poll for many scheduler ticks
        // after dispatch_event resolves, and a transport-drop /
        // citizen-shutdown landing in that window must still produce
        // a §3.4 reply.
        let (registry, snapshotted_handler) = {
            let g = self.globals.borrow();
            (Rc::clone(&g.reply_registry), g.bus_handler.clone())
        };
        let (handle, registration) = registry.register(
            event.headers.get("from").cloned().unwrap_or_default(),
            event.command.clone(),
            event.headers.get("id").cloned(),
            event
                .headers
                .get("type")
                .map(|t| t == "request")
                .unwrap_or(false),
            snapshotted_handler,
        );

        match chain_class {
            ChainClass::ClassS => {
                // SPEC 18 Phase 2 WS3-C.7g — Class S task isolation.
                //
                // Acquire the scheduler writer permit BEFORE building
                // the per-invocation activation so activation setup
                // (γ-clone of globals refs, lazy shared-scope promotion
                // the first time the Class S chain reads `$global`
                // state) runs inside the Class S exclusion window. No
                // Class C reader can interleave with that setup; no
                // other Class S writer can be admitted concurrently.
                //
                // Build a γ-activation Evaluator (shared globals +
                // private `InvocationCtx` + private local Scope frames)
                // and run the chain inline on the activation instead of
                // on `self`. Inline-await (NOT spawn_local + await):
                // Class S still blocks `dispatch_event` for the whole
                // chain, atomic semantics preserved bit-for-bit.
                //
                // Cancellation isolation: a `dispatch_event` future
                // dropped mid-await discards both the writer permit
                // (RAII) AND the activation (its private ctx/local
                // frames). The parent evaluator's `Scope` /
                // `InvocationCtx` / reply slot are NOT mutated by the
                // chain, so there is no parent state to contaminate.
                // That is the invariant that lets C.7g B2 retire the
                // poison flag / `CleanExitGuard` / `try_enter_dispatch`
                // / `DispatchAdmission` machinery entirely — the
                // permit's RAII drop plus the activation's RAII drop
                // are now the whole cancellation story.
                //
                // `run_handlers_for` owns the bump/decrement of the
                // per-invocation `$event` read-only signal
                // (`ctx.in_handler`) so the Class S and Class C arms
                // share one accounting site. A skipped decrement on
                // cancellation is harmless because the entire
                // activation is discarded with it. `scheduler=None`
                // tells the helper the caller already holds the writer
                // permit for the whole chain (Class S atomicity); it
                // must NOT acquire/release per entry.
                let scheduler = self.globals.borrow().scheduler.clone();
                let _lock_guard = scheduler.acquire_writer_dispatch().await;
                let mut invocation = self.for_invocation();
                invocation
                    .run_handlers_for(event, entries, handle, registration, None)
                    .await;
                Ok(())
            }
            ChainClass::ClassC => {
                // SPEC 18 Phase 2 WS3-C.7d Class C spawn path.
                //
                // Build a γ-activation per-invocation Evaluator that
                // shares globals (handler registry, Bus handler,
                // reply/task registries, shared global Scope frame)
                // with the parent but has private InvocationCtx and
                // local scope frames. `tokio::task::spawn_local` puts
                // the chain on the ambient LocalSet — the runner is
                // responsible for setting one up
                // (`LocalSet::new().run_until(...)`). dispatch_event
                // returns as soon as the task is spawned; the body
                // runs concurrently with other Class C tasks and the
                // outer pump's `next_incoming().await`.
                //
                // Permit acquisition happens *per entry* inside
                // `run_handlers_for` (Codex C.7d R2 MAJOR-3 fix): an
                // async entry acquires the reader, a plain entry
                // embedded in this Class C chain acquires the writer
                // (so it runs with the same exclusion a standalone
                // Class S dispatch would have). The chain task never
                // holds a chain-level permit between entries.
                let mut task_eval = self.for_invocation();
                let (task_registry, scheduler) = {
                    let g = self.globals.borrow();
                    (g.task_registry.clone(), g.scheduler.clone())
                };

                // Reserve the task id BEFORE the spawn so the closure
                // can bake it in (for self-removal on normal
                // completion). The LocalSet cannot poll the new task
                // until we yield, so attach_handle below always runs
                // before the closure starts executing.
                let task_id = task_registry.reserve_id();
                let task_registry_for_body = Rc::clone(&task_registry);
                let scheduler_for_body = Rc::clone(&scheduler);
                let handle_join = tokio::task::spawn_local(async move {
                    // `Some(scheduler)` tells the helper to acquire/
                    // release a per-entry permit (reader for async,
                    // writer for plain), respecting the documented
                    // "release the read permit before acquiring writer
                    // and re-acquiring read for any subsequent Class C
                    // bodies" rule (ChainClass docs).
                    task_eval
                        .run_handlers_for(
                            event,
                            entries,
                            handle,
                            registration,
                            Some(scheduler_for_body),
                        )
                        .await;
                    // Normal completion: self-remove from the task
                    // registry so the registry only grows while tasks
                    // are in flight. Cancellation skips this line — the
                    // entry survives for C.7f's shutdown drain to find.
                    task_registry_for_body.remove(task_id);
                });
                task_registry.attach_handle(task_id, handle_join);
                Ok(())
            }
        }
    }

    /// Run the event pump loop until the transport closes, the interrupt
    /// flag is set (Ctrl-C), or all handlers are unregistered.
    ///
    /// Intended to be called by the script runner AFTER `execute(&stmts)`
    /// returns `Ok` AND `handler_count() > 0`. A failed main body leaves
    /// globals in an undefined state; running handlers against that is
    /// asking for further errors, so the pump entry is the runner's
    /// responsibility, not this method's.
    ///
    /// Termination reasons logged at `tracing::info`:
    ///   - `"interrupted"`: the `self.globals.interrupted` flag was set (Ctrl-C)
    ///   - `"transport_closed"`: `next_incoming` returned `None`
    ///   - `"handlers_drained"`: all handlers were unregistered during
    ///     dispatch (future: reachable once `off` lands; today the
    ///     registry is append-only, so this branch is future-proofing)
    pub async fn run_event_pump(&mut self) -> MixResult<()> {
        tracing::info!(
            handler_count = self.handler_count(),
            command_count = self.handler_command_count(),
            "Entering event pump loop"
        );

        let reason = loop {
            {
                let mut g = self.globals.borrow_mut();
                if let Some(code) = g.exit_requested.take() {
                    return Err(MixError::ExitRequest { code });
                }
                if g.interrupted.load(Ordering::Relaxed) {
                    g.interrupted.store(false, Ordering::Relaxed);
                    break "interrupted";
                }
                // SPEC 18 §3.5 — the `quit()` builtin requests graceful
                // self-shutdown. Same terminal break as the Ch02 `QUIT`
                // universal (`run_event_pump` returns `Ok` → `PumpEnded`
                // → deregister-before-exit); unlike the QUIT verb there is
                // no correlated caller to reply rc:0 to (the request is
                // self-initiated), so we simply stop accepting inbound.
                // We reset the flag on THIS break path (mirroring
                // `interrupted`'s own reset). Note the interrupted branch
                // above returns first, so a simultaneous interrupt+quit
                // breaks `"interrupted"` and leaves `quit_requested` set —
                // immaterial for production serve, which exits the process
                // on either reason (evaluator reuse after pump termination
                // is not a supported production path; an embedder that
                // reuses one must clear both flags itself).
                if g.quit_requested.load(Ordering::Relaxed) {
                    g.quit_requested.store(false, Ordering::Relaxed);
                    break "quit";
                }
            }

            // Await next incoming message. Cancellation safety: the
            // caller (run_source) wraps this in a tokio::select! against
            // a CancellationToken, so dropping this future mid-await is
            // the normal Ctrl-C path. The ReceiverGuard in next_incoming
            // restores the receiver on drop.
            //
            // SPEC 18 Phase 2 WS3-C.5 — clone the handle out so the
            // `Ref` drops at end of statement, well before `.await`.
            // The `Rc` keeps the handler alive while the future is
            // suspended; nothing on the shared globals is held across
            // the yield point.
            let bus_handler = self.globals.borrow().bus_handler.clone();
            let handler = match bus_handler {
                Some(h) => h,
                None => break "no_handler",
            };
            // Race the inbound-message wait against a `quit()` wake. A
            // Class C (spawned-task) handler that calls `quit()` runs
            // concurrently while the pump is parked here; without this
            // select the atomic store would go unobserved until the next
            // inbound message (a lost wakeup that can hang forever on an
            // idle citizen). On the notify arm we `continue` to the top of
            // the loop, where the `quit_requested` flag check breaks
            // `"quit"`. Dropping the `next_incoming()` future is
            // cancel-safe: its `ReceiverGuard` restores the receiver on
            // drop (same property `run_source`'s Ctrl-C select! relies on).
            let quit_notify = self.globals.borrow().quit_notify.clone();
            let event = tokio::select! {
                biased;
                _ = quit_notify.notified() => continue,
                ev = handler.next_incoming() => ev,
            };

            match event {
                Some(ev) => {
                    // SPEC 18 WS4 / DECIDED §7-Q4 — runtime-reserved
                    // Ch07 L0+ verbs (HELP/INFO/QUIT,
                    // <svc>.props.{get,list,describe}) are serviced
                    // PRE-dispatch. An author `on HELP do` /
                    // `on <svc>.props.get do` must NOT shadow them, else
                    // §9(d)/(e) conformance is silently breakable. Only
                    // serve mode installs a ServeRuntime; plain
                    // `run_source` owes no §9 conformance and falls
                    // straight through to author handlers.
                    //
                    // SPEC 18 Phase 2 WS3-C.3 — `handle_reserved` is a
                    // synchronous trait method (no `.await` here), so
                    // borrowing `serve_runtime` and `handlers` across
                    // the call is sound. The Rc-typed storage is for
                    // C.5/C.7 symmetry, not because this path crosses
                    // a yield point.
                    let reserved = {
                        let g = self.globals.borrow();
                        if let Some(rt) = g.serve_runtime.as_ref() {
                            let handler_cmds: Vec<&str> =
                                g.handlers.keys().map(|s| s.as_str()).collect();
                            rt.handle_reserved(
                                &ev.command,
                                ev.headers.get("args").map(|s| s.as_str()),
                                &ev.body,
                                &handler_cmds,
                            )
                        } else {
                            None
                        }
                    };

                    if let Some(outcome) = reserved {
                        // Answer the requester with correlation lifted
                        // off the inbound headers (the same parts
                        // run_handlers_for/reply() use). The correlation
                        // key is `type=request` (the response is routed
                        // back to the caller by `id` via noded's
                        // pending_responses map; `from` is empty whenever
                        // noded anonymized the caller — see
                        // [`InvocationReplyHandle::from`]). A reserved verb that is
                        // not a request (a topic-delivered HELP/QUIT) is
                        // logged and dropped — it is still *consumed*,
                        // never handed to an author handler (reserved is
                        // reserved).
                        let from = ev.headers.get("from").cloned().unwrap_or_default();
                        let id = ev.headers.get("id").cloned();
                        let is_request = ev
                            .headers
                            .get("type")
                            .map(|t| t == "request")
                            .unwrap_or(false);
                        if is_request {
                            // WS3-C.5 clone-out: the Ref is dropped at end
                            // of the `let` statement, well before the
                            // await; the Rc keeps the handler alive across
                            // the yield point.
                            let bus_handler = self.globals.borrow().bus_handler.clone();
                            match bus_handler {
                                Some(h) => {
                                    if let Err(e) = h
                                        .reply(
                                            &from,
                                            &ev.command,
                                            id.as_deref(),
                                            outcome.rc,
                                            &outcome.body,
                                        )
                                        .await
                                    {
                                        tracing::error!(
                                            command = %ev.command,
                                            error = %e,
                                            "serve: reserved-verb reply failed"
                                        );
                                    }
                                }
                                None => tracing::error!(
                                    command = %ev.command,
                                    "serve: reserved verb but no Bus handler to reply"
                                ),
                            }
                            if outcome.quit {
                                // SPEC 18 §3.5 — QUIT is not a no-op. The
                                // rc:0 reply is already transmitted to a
                                // *correlated* requester; break so the
                                // serve entrypoint runs the shutdown path
                                // (WS5 wires deregister-before-exit onto
                                // this same break). Honoring `quit` is
                                // gated on correlation: an uncorrelated /
                                // topic-delivered QUIT cannot complete the
                                // SPEC 02 §3 "reply rc:0 THEN disconnect"
                                // handshake, so it is dropped (logged
                                // below) WITHOUT shutting the daemon down —
                                // a stray `emit QUIT` must not be an
                                // availability footgun inside the WG trust
                                // domain.
                                break "quit";
                            }
                        } else {
                            tracing::warn!(
                                command = %ev.command,
                                "serve: reserved verb arrived without request \
                                 correlation (not type=request); dropped"
                            );
                        }
                        continue;
                    }

                    self.dispatch_event(ev).await?;
                    if self.globals.borrow().handlers.is_empty() {
                        break "handlers_drained";
                    }
                }
                None => break "transport_closed",
            }
        };

        tracing::info!(reason = reason, "Event pump loop terminated");
        Ok(())
    }

    /// Run all registered handlers for a single event, in
    /// registration order. Handler errors are logged and swallowed;
    /// each handler runs independently. Called from `dispatch_event`'s
    /// Class S inline arm (`scheduler=None` — caller already holds the
    /// writer permit for the whole chain on the parent evaluator) AND
    /// from the Class C spawned task body (`scheduler=Some(_)` — this
    /// helper acquires/releases a per-entry permit on a per-invocation
    /// evaluator built by [`Self::for_invocation`]).
    ///
    /// **Per-entry permit (Class C path, Codex C.7d R2 MAJOR-3 fix).**
    /// Mixed Class C chains may contain plain (Class S) handler bodies
    /// alongside async ones. The documented model (see [`ChainClass`])
    /// is "release the read permit before acquiring writer and
    /// re-acquiring read for any subsequent Class C bodies." This
    /// helper implements that as a per-entry acquire/drop: each async
    /// entry runs under a freshly-acquired reader, each plain entry
    /// runs under a freshly-acquired writer (so it gets the same
    /// exclusion a standalone Class S dispatch would have). The chain
    /// task never holds a chain-level permit between entries — making
    /// it preemptable by a fresh writer arrival exactly as the spec's
    /// class semantics require.
    ///
    /// Caller responsibilities — `dispatch_event` owns these so a
    /// spawned Class C task is drain-visible from the instant
    /// `tokio::task::spawn_local` returns (an accepted-but-not-yet-
    /// polled task must still be enumerable by C.7f's shutdown drain):
    ///
    /// 1. Handler chain lookup (`entries`).
    /// 2. Chain-class classification (Class C if ANY async handler).
    /// 3. Reply-handle registration (`handle` + `registration` —
    ///    `registration.complete()` is called here, on the normal-
    ///    completion path; cancellation skips the call and leaves the
    ///    entry for C.7f).
    ///
    /// The caller MUST pass a non-empty `entries`; the early no-
    /// handlers exit is handled in `dispatch_event` (before any
    /// scheduler permit or reply registration).
    async fn run_handlers_for(
        &mut self,
        event: IncomingEvent,
        entries: Vec<HandlerEntry>,
        handle: Rc<InvocationReplyHandle>,
        registration: InvocationRegistration,
        scheduler: Option<Rc<DispatchScheduler>>,
    ) {
        // Convert headers (BTreeMap<String,String>) into a Mix Map once,
        // reused across all handlers for this event.
        let event_value = incoming_event_to_mix_value(&event);

        // Bump the per-invocation `$event` read-only signal.
        // Decremented on the normal-completion path below. Both Class
        // S (C.7g B1) and Class C (C.7d) call `run_handlers_for` on a
        // per-invocation activation built by [`Self::for_invocation`],
        // so a cancellation that skips the decrement only leaves
        // `in_handler > 0` on the discardable activation's private
        // `ctx` — no peer reads it, no parent state to clean up. C.7g
        // B2 retired the Class S CleanExitGuard / scheduler poison
        // belt-and-braces on the strength of that invariant.
        self.ctx.in_handler += 1;

        // Bind the reply handle onto this evaluator's `current_reply`
        // slot so the `reply()` builtin finds it. Both dispatch arms
        // now pass a freshly-built per-invocation evaluator (B1 made
        // Class S match Class C on this point), so `current_reply` is
        // normally `None` at entry. The save/restore is cheap and
        // protects any future nesting (e.g. C.7e's release/reacquire
        // dance landing a re-entered handler on top of an existing
        // bound slot).
        let prev_reply = self.current_reply.replace(Rc::clone(&handle));

        // SPEC 18 §3.4 — handler fault isolation. Each body runs inside a
        // catch_unwind boundary so a Rust panic (`unwrap` on a `nil`
        // field, an arithmetic overflow in a builtin, an explicit
        // `panic!`) in one handler cannot unwind through the event pump
        // and abort the whole `mix --serve` citizen. A `die` / uncaught
        // `MixError` already arrives as `Err(_)` from `execute` (the
        // pre-existing soft-fail path); §3.4 adds the *panic* arm plus a
        // synthetic error-reply so a faulting request-handler does not
        // leave the requester blocked forever. One malformed request
        // must not deny service to all others.
        let mut faulted = false;
        for (idx, entry) in entries.iter().enumerate() {
            // Acquire the per-entry scheduler permit for the Class C
            // path. `_entry_permit` is dropped at end of loop iteration,
            // which RAII-releases the lock — `tokio::sync::RwLock`'s
            // write-preference then admits any waiting writer before
            // the next iteration's reader request, matching the
            // documented mixed-chain semantics.
            let entry_permit = match (&scheduler, entry.is_async) {
                (None, _) => EntryPermit::CallerHeldWriter,
                (Some(sched), true) => {
                    let g = sched.acquire_reader_dispatch().await;
                    // Install on ctx so await_with_class_c_yield
                    // (C.7e) can take/reinstall across handler-body
                    // .await points. Cleared via take() at end-of-
                    // iteration below.
                    self.ctx.class_c_read_permit = Some(g);
                    EntryPermit::Reader
                }
                (Some(sched), false) => {
                    let g = sched.acquire_writer_class_c_entry().await;
                    EntryPermit::Writer { _guard: g }
                }
            };

            let body = &entry.body;
            // Baseline of the evaluator's transient execution state,
            // captured BEFORE this body runs. A caught panic skips every
            // balancing decrement/pop/restore between the panic site and
            // the catch, so on the panic arm we forcibly rewind to
            // exactly these values — otherwise a long-lived citizen would
            // leak scope frames, keep dangling raw `*const Value` entries
            // in the var-slot caches that a later handler's sync fast
            // path could dereference, retain a stale `address` target so
            // a later bare call is silently treated as an address-send,
            // or keep a stale `sync_self_func` so later sync
            // self-recursion resolves to the wrong function. The clean
            // path balances all of these itself and the baselines are
            // simply not consulted. This set MUST cover every piece of
            // execute-scoped mutable state that has a push/pop or
            // save/restore bracket inside `execute`; adding a new such
            // bracket elsewhere obliges adding it here too.
            let scope_floor = self.scope.frame_count();
            let fn_depth = self.ctx.function_depth;
            let call_frames_floor = self.ctx.call_frames.len();
            let baseline_line = self.ctx.current_line;
            let baseline_file = self.ctx.current_file.clone();
            let slot_len = self.ctx.var_slot_cache.len();
            let slot_f64_len = self.ctx.var_slot_cache_f64.len();
            let address_floor = self.ctx.address_stack.len();
            let prev_sync_self = self.ctx.sync_self_func.clone();
            let require_floor = self.ctx.require_stack.len();

            // Push a fresh scope frame, inject $event, run body, pop frame.
            // The frame isolates handler-local variables from the outer
            // script while still allowing `update_or_set` assignments to
            // walk up to outer-scope globals (intended: `$paused = true`
            // in a handler should affect the outer producer loop).
            self.scope.push_frame();
            self.scope.set("event".to_string(), event_value.clone());
            // 0.63.0 — $event has DYNAMIC EXTENT across the dispatch: a
            // fn called from the handler body must see it too. Function
            // frames only fall through to GLOBALS, so the frame-local
            // binding above is invisible one call deep — the historical
            // failure was a helper raising NAME_UNDEFINED on $event,
            // fault isolation swallowing it, and the citizen staying
            // "healthy" while its side effects silently never happened
            // (an hour of live bisection, hub 2133f4b). So the event is
            // ALSO bound into the global frame for the dispatch and
            // restored after (prior value, or Nil when there was none —
            // Scope has no remove; a post-dispatch top-level $event
            // reads nil, documented in bus.md). A fn-local $event
            // param/binding still shadows it — normal resolution order.
            // The name-based readonly guard (check_event_readonly,
            // in_handler > 0) covers this binding exactly as it covered
            // the frame-local one. Save/restore nests correctly for
            // handler-within-handler dispatch.
            let saved_global_event: Option<Value> = self.scope.get_global_only("event");
            self.scope.set_global("event", event_value.clone());

            // `AssertUnwindSafe`: the `execute` future borrows `&mut
            // self`, which is `!UnwindSafe`. The assertion is sound here
            // because we do NOT resume using the partially-mutated state
            // — the panic arm rewinds scope/caches/depth to the captured
            // baseline before the next iteration touches `self`. The
            // default panic hook still prints to stderr (journald, §3.6);
            // the structured `tracing::error!` below is the authoritative
            // record. Hook suppression is process-global and out of scope.
            let outcome = std::panic::AssertUnwindSafe(self.execute(body))
                .catch_unwind()
                .await;

            let mut requested_exit = false;
            match outcome {
                Ok(Ok(_)) => {
                    self.scope.pop_frame();
                }
                Ok(Err(MixError::ExitRequest { code })) => {
                    self.scope.pop_frame();
                    let mut g = self.globals.borrow_mut();
                    // The latest/innermost exit wins, matching nested
                    // `finally` control-flow replacement semantics.
                    g.exit_requested = Some(code);
                    g.quit_notify.notify_one();
                    requested_exit = true;
                }
                Ok(Err(e)) => {
                    self.scope.pop_frame();
                    faulted = true;
                    tracing::error!(
                        command = %event.command,
                        handler_index = idx,
                        error = %e,
                        "Handler body errored; handler remains registered"
                    );
                    // 0.63.0 — a swallowed fault must still be OBSERVABLE:
                    // tracing alone can be routed nowhere in a plain
                    // `mix --serve`, and a healthy-looking blind citizen
                    // is the worst failure shape a Bus has (noded reports
                    // delivered=1 while the handler's side effects never
                    // happen). One line to the evaluator's stderr sink +
                    // the serve runtime's health surface.
                    self.record_handler_fault(&event.command, idx, &e.to_string());
                    // Continue with subsequent handlers. Do not propagate.
                }
                Err(panic_payload) => {
                    faulted = true;
                    let msg = panic_payload
                        .downcast_ref::<&'static str>()
                        .map(|s| (*s).to_string())
                        .or_else(|| panic_payload.downcast_ref::<String>().cloned())
                        .unwrap_or_else(|| "<non-string panic payload>".to_string());
                    tracing::error!(
                        command = %event.command,
                        handler_index = idx,
                        panic = %builtins::sanitize_for_diag(&msg),
                        "Handler body PANICKED; isolated, handler remains \
                         registered (SPEC 18 §3.4)"
                    );
                    // Same observability rule as the error arm (0.63.0).
                    self.record_handler_fault(
                        &event.command,
                        idx,
                        &format!("panic: {}", builtins::sanitize_for_diag(&msg)),
                    );
                    // The unwound `execute` left scope frames, function
                    // floors, the function-depth counter, the var-slot
                    // caches, the `address` target stack and the
                    // sync-self-recursion slot in an indeterminate state.
                    // Rewind every one to the pre-body baseline so the
                    // next handler — and any subsequent event — runs
                    // against clean evaluator state. (No `pop_frame`
                    // here: `restore_frames_to` already truncates to the
                    // floor captured before the `push_frame` above.)
                    self.scope.restore_frames_to(scope_floor);
                    self.ctx.function_depth = fn_depth;
                    self.ctx.call_frames.truncate(call_frames_floor);
                    // A panic inside a function skips call_function's
                    // line/file restore bracket — rewind both so later
                    // handlers don't inherit the panicked callee's
                    // context (codex C3 review).
                    self.ctx.current_line = baseline_line;
                    self.ctx.current_file = baseline_file;
                    self.ctx.var_slot_cache.truncate(slot_len);
                    self.ctx.var_slot_cache_f64.truncate(slot_f64_len);
                    self.ctx.address_stack.truncate(address_floor);
                    self.ctx.sync_self_func = prev_sync_self;
                    self.ctx.require_stack.truncate(require_floor);
                }
            }

            // Restore the dispatch-scoped global $event (all arms,
            // including panic — the rewind above does not know about it).
            self.scope
                .set_global("event", saved_global_event.unwrap_or(Value::Nil));

            // End-of-iteration permit release.
            // - Writer: RAII via drop(entry_permit) — releases the
            //   write lock guard for the next iteration / a waiting
            //   Class S writer (write-preference).
            // - Reader: the owned permit lives on
            //   `ctx.class_c_read_permit`, take() to drop it. C.7e's
            //   await_with_class_c_yield may have take()d it already
            //   for an in-flight handler-body await and re-acquired
            //   on resume; either way, this take() lands a Some-or-None
            //   and the guard (if any) RAII-releases.
            // - CallerHeldWriter: marker only; nothing to drop here
            //   (caller's parent writer permit covers the chain).
            //
            // Per-invocation cancellation is scoped to the task's
            // private activation (`for_invocation`) for both arms;
            // there is no chain-level finalisation step.
            drop(entry_permit);
            let _ = self.ctx.class_c_read_permit.take();
            if requested_exit {
                break;
            }
        }

        // SPEC 18 §3.4 — if any body faulted (panic or error) and the
        // requester has NOT been answered by a `reply()` from some body,
        // synthesize a non-zero error reply so a request-class caller is
        // not left blocked. Gated exactly like `reply()`: only a
        // request (`type=request`) can be answered — a topic delivery
        // has no caller; `from` may be empty (noded-anonymized caller)
        // and is not a gate, the response correlates by `id`. The wire
        // body is a
        // fixed, non-sensitive string — the panic/error detail stays in
        // the server-side log and never crosses the Bus boundary (it can
        // carry request data or Trojan-Source bytes; the WG trust domain
        // does not make peers non-adversarial).
        if faulted && handle.is_unanswered_request() {
            // SPEC 18 §3.4 + WS3-C.7c: route the synthetic reply
            // through the reply handle's reply-once CAS so the
            // shutdown drain (C.7f) cannot race this path and produce
            // a duplicate wire reply on a citizen-shutdown-during-
            // fault edge. `reply_once` itself releases its
            // `RefCell`-free `Rc<dyn BusHandler>` clone across the
            // `.await` — no globals borrow survives.
            match handle.reply_once(1, "internal handler error").await {
                Ok(_) => {}
                Err(e) => tracing::error!(
                    command = %event.command,
                    error = %e,
                    "serve: §3.4 synthetic error-reply failed"
                ),
            }
        }

        // Restore the enclosing event's reply target (None at the
        // outer level). Reached unconditionally: the loop above never
        // early-returns — handler errors and panics are both isolated,
        // logged, and (for unanswered requests) answered with an error
        // reply.
        self.current_reply = prev_reply;
        // Balance the `in_handler` bump from the top of the function.
        // Reached unconditionally on the normal-return path. A
        // cancellation that drops the future before this line is
        // activation-local for both arms — the per-invocation `ctx` is
        // discarded with the activation, no peer is affected. That
        // invariant is the sole protection; the scheduler-wide
        // `CleanExitGuard` / `poison` belt-and-braces was retired in
        // C.7g B2.
        self.ctx.in_handler -= 1;
        // Explicit normal-completion deregistration (Codex C.7c R1
        // BLOCKER fix). The `InvocationRegistration` guard's plain
        // `Drop` is a no-op — it leaves cancellation victims in the
        // registry for C.7f's shutdown drain to enumerate. Reaching
        // this line means the dispatch ran to a clean end, so we
        // own the deregistration explicitly.
        registration.complete();
    }

    /// Check whether the given variable name is a write to the read-only
    /// `$event` binding inside a handler body. Returns `Err` if so.
    ///
    /// `$event` is only read-only while a handler is executing
    /// (`ctx.in_handler > 0`). As of WS3-C.6b this counter is
    /// independent of the dispatch admission gate — the scheduler
    /// RwLock alone serializes dispatch, and `in_handler` survives
    /// only as the `$event` read-only signal. Outside handlers,
    /// `event` is a plain identifier with no special semantics — a
    /// user who happened to name a variable `event` before `on`
    /// shipped is unaffected.
    ///
    /// # Soundness invariant
    ///
    /// This name-based check is sound ONLY because Mix's `Value::Map` is
    /// copy-by-value (see `value.rs` — `Value` is `#[derive(Clone)]` and
    /// every assignment deep-clones the underlying `IndexMap`). An aliased
    /// `$copy = $event` produces an independent map, so a user cannot
    /// bypass the check by mutating through an alias.
    ///
    /// If Mix ever adds reference-semantic maps (e.g., `Value::MapRc` or
    /// a COW variant for performance with large payloads), this check
    /// becomes insufficient — an aliased map would share storage with
    /// `$event` and `$copy.body = ...` would mutate the "frozen" event
    /// through the back door. At that point the readonly enforcement
    /// needs to move into the type system, not the scope name: a
    /// `Value::Frozen(Box<Value>)` variant whose mutation paths all
    /// error, or a per-scope-entry readonly flag. Don't just extend this
    /// function — the invariant has shifted and the name-based check
    /// is no longer load-bearing for soundness.
    /// Surface an isolated handler fault (0.63.0): one line to the
    /// evaluator's stderr sink (tracing can be routed nowhere in a plain
    /// `mix --serve`, and the historical failure shape was a
    /// healthy-looking blind citizen) plus the serve runtime's health
    /// hook when one is installed. Never raises — this runs on the fault
    /// path and must not displace the isolation semantics.
    fn record_handler_fault(&mut self, command: &str, handler_index: usize, detail: &str) {
        let summary = format!("handler fault: {command}[{handler_index}]: {detail}");
        {
            let mut g = self.globals.borrow_mut();
            let _ = writeln!(g.stderr, "mix: {summary} (handler remains registered)");
            let _ = g.stderr.flush();
        }
        let runtime = self.globals.borrow().serve_runtime.clone();
        if let Some(rt) = runtime {
            rt.record_handler_fault(&summary);
        }
    }

    fn check_event_readonly(&self, name: &str) -> MixResult<()> {
        if self.ctx.in_handler > 0 && name == "event" {
            return Err(self.runtime_err(
                "$event is read-only inside handlers; copy to a local if you need to mutate",
            ));
        }
        Ok(())
    }

    /// Return the RHS of the EXACT `$name = $name .. rhs` shape (the
    /// self-concat accumulator), else None. Left-associativity means a
    /// chained `$s = $s .. a .. b` does NOT match (its outer left operand
    /// is `$s .. a`, not `$s`) — that's fine, it just uses the generic path.
    fn self_concat_rhs<'e>(name: &str, value: &'e Expr) -> Option<&'e Expr> {
        let Expr::BinaryOp { left, op, right } = value else {
            return None;
        };
        if !matches!(op, BinOp::Concat) {
            return None;
        }
        let Expr::Variable(left_name) = &**left else {
            return None;
        };
        if left_name != name {
            return None;
        }
        Some(right)
    }

    /// Whether ordinary assignment to `$name` would land on an existing
    /// String slot that is safe to mutate IN PLACE. Function assignment
    /// writes the current frame (`set_in_current`) even when a same-named
    /// read resolves to an outer frame, so inside a function only a
    /// current-frame String qualifies; at top level, `update_or_set`'s
    /// outward lookup target (including a shared global) qualifies.
    /// `Value::String` is copy-by-value (value.rs) → in-place append is
    /// alias-safe. A missing or non-string target falls to the generic
    /// path, preserving undefined-variable + coercion semantics.
    fn assignment_target_is_string(&mut self, name: &str) -> bool {
        if self.ctx.function_depth > 0 {
            self.scope
                .with_mut_current_value(name, |v| matches!(v, Value::String(_)))
                .unwrap_or(false)
        } else {
            self.scope
                .with_mut_value(name, |v| matches!(v, Value::String(_)))
                .unwrap_or(false)
        }
    }

    /// Append an already-evaluated `rhs` onto the proven-String assignment
    /// target IN PLACE (write_mix reuses the String's geometric capacity →
    /// O(1) amortized, turning `$s = $s .. rhs` accumulators from O(n²) to
    /// O(n)). RHS MUST be fully evaluated before this call (no scope borrow
    /// crosses eval/await). A max_string_len violation is rolled back
    /// (truncate to the pre-append length) before raising, matching the
    /// generic `..` semantics which check the new Value before storing it.
    fn append_to_assignment_string(&mut self, name: &str, rhs: &Value) -> MixResult<()> {
        let max_string = self.ctx.max_string_len;
        let mut cap_violation: Option<usize> = None;
        let append = |v: &mut Value| {
            let Value::String(s) = v else {
                return false;
            };
            let old_len = s.len();
            rhs.write_mix(s);
            if let Some(max) = max_string
                && s.len() > max
            {
                let grown = s.len();
                s.truncate(old_len);
                cap_violation = Some(grown);
            }
            true
        };
        let appended = if self.ctx.function_depth > 0 {
            self.scope.with_mut_current_value(name, append)
        } else {
            self.scope.with_mut_value(name, append)
        }
        .unwrap_or(false);
        if !appended {
            return Err(self.runtime_err(format!(
                "self-concat target '${name}' changed during RHS evaluation"
            )));
        }
        if let Some(len) = cap_violation {
            return Err(self.runtime_err(format!(
                "string length {} exceeds limit {}",
                len,
                max_string.unwrap_or(0)
            )));
        }
        Ok(())
    }

    /// Bind a name introduced by a *binder* construct — `catch $e`,
    /// `parse … with $a $b`, loop vars — respecting function scope. Like
    /// `StmtKind::Assignment`, inside a function (`function_depth > 0`)
    /// the name binds into the current frame so it cannot clobber a
    /// same-named caller/global via `update_or_set`'s intentional
    /// globals-fallback; at the top level it stays in the single global
    /// frame (unchanged behaviour). Use this for every construct that
    /// *introduces* a name, not for ordinary re-assignment of an
    /// existing one.
    fn bind_scoped(&mut self, name: &str, value: Value) {
        if self.ctx.function_depth > 0 {
            self.scope.set_in_current(name.to_string(), value);
        } else {
            self.scope.update_or_set(name, value);
        }
    }

    /// SPEC 18 Phase 2 WS3-C.4b — environmental gate for raw-pointer
    /// fast paths that cache `scope.frames[0]` slot addresses
    /// (`Scope::global_slot_ptr` / `Scope::try_global_slot_ptr`) across
    /// multiple synchronous evaluator calls.
    ///
    /// **Safety model.** Caching a `frames[0]` slot pointer is sound
    /// when the conjunction of these conditions holds at the call site:
    /// 1. `self.scope.is_top_level()` — only one frame is live, so a
    ///    cached pointer into `frames[0]` is by definition the only
    ///    visible binding for that name.
    /// 2. `self.ctx.function_depth == 0` — no active function frame.
    /// 3. The AST statically classifies as sync (`Self::is_expr_sync`
    ///    or the equivalent per-fast-path predicate) so the cache's
    ///    live window contains no `.await` points.
    /// 4. No path inside the cached-pointer live window inserts into
    ///    `frames[0]` — `try_eval_expr_sync` only reads, and the
    ///    fast-path bodies are restricted to assignment shapes that
    ///    overwrite cached slots without inserting new keys.
    /// 5. **This predicate returns `true`** — the environmental
    ///    permission. The function exists as the single named audit
    ///    point so any future change that would invalidate the
    ///    environmental assumption (a new yielding fast-path leg; a
    ///    runtime that rehashes `frames[0]` from a background task)
    ///    must be threaded through here. The structural conditions
    ///    (1)–(4) are call-site invariants; (5) is the only knob the
    ///    rest of the evaluator can flip from above.
    ///
    /// **Why this is not keyed off `in_handler` or `function_depth`
    /// (or the scheduler's `is_dispatching()` peek) for the standalone
    /// case.** Class C interleaving between invocations is
    /// only hazardous if the cached pointer's live window can yield —
    /// LocalSet only polls another task at an `.await`. The current
    /// Category G call sites never `.await` between
    /// `Scope::global_slot_ptr` and the matching
    /// `self.ctx.var_slot_cache.truncate(...)`. Disabling these paths
    /// because the *invocation* may be Class C in the abstract would be
    /// a false negative for the *standalone* evaluator (the singleton
    /// REPL/script case where `frames[0]` is the only frame and the
    /// scope owns its `FxHashMap` outright).
    ///
    /// **WS3-C.7b — γ design flips this for per-invocation
    /// evaluators.** A per-invocation evaluator (spawned by C.7d's
    /// `for_invocation` factory) routes globals through a
    /// `Rc<RefCell<FxHashMap>>` shared with sibling invocations and
    /// the dispatcher. The "single live frame" invariant (condition
    /// one above) no longer holds — `frames[0]` is the empty local
    /// scaffold, not the global storage. Raw-pointer slot caching into
    /// `frames[0]` is *meaningless* in that mode (the slot is not
    /// where globals live) and the helper `Scope::global_slot_ptr` is
    /// hardened to panic if invoked. So this gate must report
    /// `false` whenever either:
    /// - the evaluator is per-invocation
    ///   (`self.is_per_invocation`), or
    /// - the scope has been promoted to share a global frame
    ///   (`self.scope.uses_shared_globals()`).
    ///
    /// The two checks are belt-and-suspenders: an Evaluator constructed
    /// by `for_invocation` sets both, but a future caller that bolts
    /// a shared global frame onto a standalone-flagged evaluator (e.g.
    /// a test harness, or a debugger surface) still gets the right
    /// answer.
    ///
    /// **After C.5 / C.6.** Wrapping `globals` in `Rc<RefCell<_>>`
    /// (C.5) does not change raw-pointer validity during a non-yielding
    /// window — it only constrains how `Ref`/`RefMut` guards are scoped.
    /// Replacing the `dispatch_depth` counter with the scheduler RwLock
    /// (C.6) does not change pointer validity either. The C.7b γ split
    /// above is the first milestone where this predicate's body
    /// actually toggles per evaluator instance.
    #[inline]
    pub(crate) fn can_use_shared_scope_raw_slots(&self) -> bool {
        !self.is_per_invocation && !self.scope.uses_shared_globals()
    }

    /// Get a clone of the interrupt flag (for signal handlers to set).
    pub fn interrupt_flag(&self) -> Arc<AtomicBool> {
        self.globals.borrow().interrupted.clone()
    }

    /// Replace the interrupt flag with an externally-created one.
    /// Must be called before execute() so signal handlers share
    /// the same flag as the evaluator.
    pub fn set_interrupt_flag(&mut self, flag: Arc<AtomicBool>) {
        self.globals.borrow_mut().interrupted = flag;
    }

    /// Set the current file name for error reporting.
    pub fn set_file(&mut self, file: impl Into<String>) {
        self.ctx.current_file = Some(std::rc::Rc::from(file.into().as_str()));
    }

    /// Enable or disable statement tracing.
    pub fn set_trace(&mut self, on: bool) {
        self.globals.borrow_mut().trace = on;
    }

    /// Check whether tracing is enabled.
    pub fn trace(&self) -> bool {
        self.globals.borrow().trace
    }

    /// Attach a UsageStats instance for tracking.
    pub fn attach_stats(&mut self, stats: UsageStats) {
        self.globals.borrow_mut().stats = Some(Box::new(stats));
        self.stats_attached = true;
    }

    /// Take the UsageStats out of the evaluator (transfers ownership).
    pub fn take_stats(&mut self) -> Option<UsageStats> {
        self.stats_attached = false;
        self.globals.borrow_mut().stats.take().map(|b| *b)
    }

    /// Borrow the UsageStats immutably.
    ///
    /// SPEC 18 Phase 2 WS3-C.5 — returns a `Ref` guard rather than a
    /// plain reference because `globals` lives behind `Rc<RefCell<_>>`.
    /// The guard derefs to `&UsageStats` for read-only call shapes
    /// (`eval.stats().map(|s| s.foo())`). Hold it for the shortest
    /// window possible and never across an `.await`.
    pub fn stats(&self) -> Option<Ref<'_, UsageStats>> {
        if !self.stats_attached {
            return None;
        }
        Ref::filter_map(self.globals.borrow(), |g| g.stats.as_deref()).ok()
    }

    /// Borrow the UsageStats mutably.
    ///
    /// SPEC 18 Phase 2 WS3-C.5 — returns a `RefMut` guard. Same
    /// borrow-window discipline as `stats()`.
    pub fn stats_mut(&mut self) -> Option<RefMut<'_, UsageStats>> {
        if !self.stats_attached {
            return None;
        }
        RefMut::filter_map(self.globals.borrow_mut(), |g| g.stats.as_deref_mut()).ok()
    }

    #[inline]
    fn track_builtin_attempt(&mut self, name: &str) {
        if !self.stats_attached {
            return;
        }
        let mut globals = self.globals.borrow_mut();
        if let Some(stats) = globals.stats.as_deref_mut()
            && (crate::builtins::is_builtin(name)
                || crate::builtins::EVAL_SPECIAL_BUILTINS.contains(&name)
                || crate::builtins_hof::lookup(name).is_some())
        {
            stats.track_builtin(name);
        }
    }

    #[inline]
    fn track_function_attempt(&mut self, name: &str) {
        if self.stats_attached
            && let Some(stats) = self.globals.borrow_mut().stats.as_deref_mut()
        {
            stats.track_function(name);
        }
    }

    #[inline]
    fn track_keyword_attempt(&mut self, keyword: &str) {
        if self.stats_attached
            && let Some(stats) = self.globals.borrow_mut().stats.as_deref_mut()
        {
            stats.track_keyword(keyword);
        }
    }

    #[inline]
    fn track_error_attempt(&mut self, error: &MixError) {
        if !self.stats_attached {
            return;
        }
        let message = error.to_string();
        if let Some(stats) = self.globals.borrow_mut().stats.as_deref_mut() {
            stats.track_error(&message);
        }
    }

    #[inline]
    fn track_command_attempts(&mut self, commands: &[String]) {
        if !self.stats_attached {
            return;
        }
        if let Some(stats) = self.globals.borrow_mut().stats.as_deref_mut() {
            for command in commands {
                stats.track_command(command);
            }
        }
    }

    #[inline]
    fn track_stmt_keywords(&mut self, stmt: &Stmt) {
        if !self.stats_attached {
            return;
        }
        if let Some(stats) = self.globals.borrow_mut().stats.as_deref_mut() {
            for keyword in stmt.kind.stats_keywords().into_iter().flatten() {
                stats.track_keyword(keyword);
            }
        }
    }

    /// Load the standard prelude (utility functions).
    /// Checks the user's config dir for a `prelude.mix` override first —
    /// `$COSMIX_ETC`, else `$COSMIX/etc`, else `~/.config/cosmix` — and
    /// falls back to the built-in prelude.
    pub async fn load_prelude(&mut self) {
        const BUILTIN_PRELUDE: &str = include_str!("../std/prelude.mix");
        // Record for exec_require: modules see the prelude iff the
        // program loaded it (a --no-prelude run stays prelude-free in
        // modules too).
        self.globals.borrow_mut().prelude_loaded = true;

        // Check for user override
        let source = if let Some(home) = dirs_home() {
            let etc = std::env::var_os("COSMIX_ETC")
                .map(std::path::PathBuf::from)
                .or_else(|| std::env::var_os("COSMIX").map(|r| std::path::PathBuf::from(r).join("etc")))
                .unwrap_or_else(|| home.join(".config/cosmix"));
            let user_prelude = etc.join("prelude.mix");
            if user_prelude.exists() {
                std::fs::read_to_string(&user_prelude)
                    .unwrap_or_else(|_| BUILTIN_PRELUDE.to_string())
            } else {
                BUILTIN_PRELUDE.to_string()
            }
        } else {
            BUILTIN_PRELUDE.to_string()
        };

        let mut lexer = crate::lexer::Lexer::new(&source);
        match lexer.tokenize().and_then(|tokens| {
            let mut parser = crate::parser::Parser::new(tokens, &source);
            parser.parse_program()
        }) {
            Ok(stmts) => {
                if let Err(e) = self.execute(&stmts).await {
                    eprintln!("prelude: {}", e);
                }
            }
            Err(e) => eprintln!("prelude: {}", e),
        }
    }

    /// Create a RuntimeError with the current source location.
    pub fn runtime_err(&self, msg: impl Into<String>) -> MixError {
        MixError::RuntimeError {
            msg: msg.into(),
            span: if self.ctx.current_line > 0 {
                Some(crate::error::Span {
                    line: self.ctx.current_line,
                    column: 0,
                    file: self.ctx.current_file.as_deref().map(str::to_string),
                })
            } else {
                None
            },
        }
    }

    /// Create a code-carrying structured error (0.29.0) with the current
    /// source location. The traceback is filled in later by
    /// [`snapshot_error`](Self::snapshot_error) at the first
    /// function/builtin boundary the error crosses.
    pub fn coded_err(&self, code: &str, msg: impl Into<String>) -> MixError {
        let mut info = crate::error::ErrorInfo::new(code, msg);
        info.span = self.current_span();
        MixError::Structured(Box::new(info))
    }

    /// The current statement's span, when one is known.
    fn current_span(&self) -> Option<crate::error::Span> {
        (self.ctx.current_line > 0).then(|| crate::error::Span {
            line: self.ctx.current_line,
            column: 0,
            file: self.ctx.current_file.as_deref().map(str::to_string),
        })
    }

    /// Snapshot the live call stack into traceback frames,
    /// outermost-to-innermost, final frame = failure site. `<main>`
    /// carries the line of the outermost call (or the failure line when
    /// the error is raised at top level); each subsequent frame carries
    /// the line *within* it where the next call (or the failure)
    /// happened — the Python convention. Frame k's file is the file
    /// frame k was EXECUTING in — recorded as the caller-side file on
    /// frame k+1's entry (and `ctx.current_file` for the innermost
    /// frame, which `call_function` keeps switched to the callee's
    /// `def_file`), so files stay correct across `require`d modules
    /// and `include`/`source` boundaries.
    fn snapshot_frames(&self) -> Vec<crate::error::Frame> {
        use crate::error::{Frame, FrameKind};
        let stack = &self.ctx.call_frames;
        let nonzero = |l: usize| (l > 0).then_some(l);
        let file_of = |f: &Option<std::rc::Rc<str>>| f.as_deref().map(str::to_string);
        let mut frames = Vec::with_capacity(stack.len() + 1);
        let (main_line, main_file) = match stack.first() {
            Some(cf) => (nonzero(cf.call_line), file_of(&cf.caller_file)),
            None => (
                nonzero(self.ctx.current_line),
                file_of(&self.ctx.current_file),
            ),
        };
        frames.push(Frame {
            kind: FrameKind::Script,
            function: "<main>".to_string(),
            file: main_file,
            line: main_line,
            column: None,
        });
        for (i, cf) in stack.iter().enumerate() {
            let (line, file) = match stack.get(i + 1) {
                Some(next) => (nonzero(next.call_line), file_of(&next.caller_file)),
                None => (
                    nonzero(self.ctx.current_line),
                    file_of(&self.ctx.current_file),
                ),
            };
            frames.push(Frame {
                kind: FrameKind::Script,
                function: cf.name.to_string(),
                file,
                line,
                column: None,
            });
        }
        frames
    }

    /// Convert an error into its structured form with a traceback,
    /// idempotently. Called at the first function boundary an error
    /// crosses (while `ctx.call_frames` is still intact), at the
    /// builtin dispatch sites, and when `catch` binds its optional
    /// second variable. Control-flow variants (`Return`/`Break`/
    /// `Continue`/`ExitRequest`) and lex/parse errors pass through untouched.
    pub(crate) fn snapshot_error(&self, e: MixError) -> MixError {
        use crate::error::ErrorInfo;
        match e {
            MixError::Structured(mut info) => {
                if info.frames.is_empty() {
                    info.frames = self.snapshot_frames();
                }
                if info.span.is_none() {
                    info.span = self.current_span();
                }
                MixError::Structured(info)
            }
            MixError::RuntimeError { msg, span } => {
                let mut info = ErrorInfo::new("RUNTIME_ERROR", msg);
                info.span = span.or_else(|| self.current_span());
                info.frames = self.snapshot_frames();
                MixError::Structured(Box::new(info))
            }
            MixError::DieError { msg } => {
                let mut info = ErrorInfo::new("USER_DIE", msg);
                info.span = self.current_span();
                info.frames = self.snapshot_frames();
                MixError::Structured(Box::new(info))
            }
            other => other,
        }
    }

    /// `snapshot_error` for the builtin dispatch sites: additionally
    /// records the builtin itself as the innermost frame — but only
    /// when the error had no frames yet (a HOF-propagated error from a
    /// user callback already carries deeper script frames; appending
    /// the HOF after them would misplace the failure site).
    pub(crate) fn snapshot_builtin_error(&self, name: &str, e: MixError) -> MixError {
        use crate::error::{Frame, FrameKind};
        let had_frames = matches!(&e, MixError::Structured(info) if !info.frames.is_empty());
        let e = self.snapshot_error(e);
        if had_frames {
            return e;
        }
        match e {
            MixError::Structured(mut info) => {
                info.frames.push(Frame {
                    kind: FrameKind::Builtin,
                    function: name.to_string(),
                    file: self.ctx.current_file.as_deref().map(str::to_string),
                    line: (self.ctx.current_line > 0).then_some(self.ctx.current_line),
                    column: None,
                });
                MixError::Structured(info)
            }
            other => other,
        }
    }

    /// Register a Rust function callable from Mix scripts.
    ///
    /// Extension functions are checked after builtins but before user-defined
    /// functions, so they cannot shadow builtins but will shadow user functions
    /// of the same name.
    pub fn register(&mut self, name: &str, f: ExtFn) {
        self.globals
            .borrow_mut()
            .extensions
            .insert(name.to_string(), f);
    }

    /// Set a global variable in the evaluator's scope.
    pub fn set_global(&mut self, name: &str, value: Value) {
        self.scope.update_or_set(name, value);
    }

    /// Get a variable from the evaluator's scope (any visible frame).
    /// Returns a clone so callers don't hold a borrow on the evaluator.
    ///
    /// SPEC 18 Phase 2 WS3-C.7b — uses the γ-aware lookup so per-
    /// invocation (Class C) evaluators returning to their parent /
    /// host can still read citizen-wide globals that live behind
    /// `Rc<RefCell<_>>`.
    pub fn get_global(&self, name: &str) -> Option<Value> {
        self.scope.get_value(name).map(|v| v.into_value())
    }

    /// Get the aliases map (for shell command expansion).
    ///
    /// SPEC 18 Phase 2 WS3-C.5 — returns a `Ref` guard rather than a
    /// plain reference because `globals` lives behind `Rc<RefCell<_>>`.
    /// The guard derefs to `&HashMap<String, String>`, so most call
    /// shapes (`.keys()`, `.get(name)`, `.len()`) work unchanged.
    /// Callers that pass the map to functions expecting `&HashMap`
    /// (e.g. `shell::expand_alias(line, &aliases)`) must bind the
    /// guard to a local first, then borrow.
    pub fn aliases(&self) -> Ref<'_, HashMap<String, String>> {
        Ref::map(self.globals.borrow(), |g| &g.aliases)
    }

    /// Remove an alias.
    pub fn remove_alias(&mut self, name: &str) -> bool {
        self.globals.borrow_mut().aliases.remove(name).is_some()
    }

    /// Check if a user-defined function exists.
    ///
    /// SPEC 18 Phase 2 WS3-C.7b — γ-aware: routes through the shared
    /// function registry when this evaluator is per-invocation, or
    /// when the parent's `functions` map has been promoted.
    pub fn has_function(&self, name: &str) -> bool {
        self.scope.has_function(name)
    }

    /// Call a user-defined function by name with no arguments.
    pub fn call_function_by_name<'a>(
        &'a mut self,
        name: &'a str,
    ) -> Pin<Box<dyn Future<Output = MixResult<Value>> + 'a>> {
        Box::pin(async move {
            // SPEC 18 Phase 2 WS3-C.7b — γ-aware lookup so a per-
            // invocation evaluator can still resolve user functions
            // through the shared registry.
            let func = self.scope.get_function_owned(name).ok_or_else(|| {
                MixError::structured(
                    "FUNCTION_UNDEFINED",
                    format!("undefined function '{}'", name),
                )
            })?;
            self.call_function(&func, &[]).await
        })
    }

    /// Names of all user-defined functions currently in scope, as a set —
    /// for the shell classifier's bash-style bareword function dispatch.
    pub fn function_names(&self) -> HashSet<String> {
        self.scope
            .function_names()
            .into_iter()
            .map(|(name, _params)| name)
            .collect()
    }

    /// Call a user-defined function by name, binding whitespace-split shell
    /// words as string arguments. Backs bareword dispatch: `sc restart nginx`
    /// invokes `sc("restart", "nginx")`. Missing/short args bind `nil` inside
    /// the function via the normal call ABI (extra args are ignored there).
    pub fn call_function_by_name_with_args<'a>(
        &'a mut self,
        name: &'a str,
        args: &'a [String],
    ) -> Pin<Box<dyn Future<Output = MixResult<Value>> + 'a>> {
        Box::pin(async move {
            let func = self.scope.get_function_owned(name).ok_or_else(|| {
                MixError::structured(
                    "FUNCTION_UNDEFINED",
                    format!("undefined function '{}'", name),
                )
            })?;
            let values: Vec<Value> = args.iter().map(|s| Value::String(s.clone())).collect();
            self.track_function_attempt(name);
            self.call_function(&func, &values).await
        })
    }

    /// Resolve one `${…}` interpolation part to the value to write: `${path}`
    /// or `${path ?? default}` / `${path ?: default}`. Kept as its own boxed
    /// async fn (NOT inlined into `eval_expr`) so the recursive `eval_expr`
    /// future stays small — the runaway-recursion depth cap must trip before
    /// the native stack overflows (recursion_no_crash.rs).
    ///
    /// - `path` is a dotted lookup: `$head` (scope → env), then Map keys, with
    ///   the `*` fallback at each step — identical to `$a.b.c` field access.
    /// - Head unbound in BOTH scope and env → the default if one is given, else
    ///   an ERROR (matching a bare `$head` read, not the old silent "nil"). An
    ///   explicit nil VALUE is "bound" and renders "nil"; a missing MAP KEY is
    ///   nil, as in `$a.b.c`.
    /// - `??` fires the default on a nil value, `?:` on any falsy value.
    fn resolve_interp_variable<'a>(
        &'a mut self,
        spec: &'a str,
    ) -> Pin<Box<dyn Future<Output = MixResult<Value>> + 'a>> {
        Box::pin(async move {
            let (path, coalesce) = split_interp_coalesce(spec);
            let mut parts_iter = path.split('.');
            let head = parts_iter.next().unwrap_or("");
            // SPEC 18 Phase 2 WS3-C.7b — γ-aware read, then env fallback.
            let resolved = self
                .scope
                .get_value(head)
                .map(|v| v.into_value())
                .or_else(|| std::env::var(head).ok().map(Value::String));
            let mut val = match resolved {
                None => match coalesce {
                    Some((_, src)) => return self.eval_interp_default(src).await,
                    None => {
                        return Err(MixError::RuntimeError {
                            span: None,
                            msg: format!(
                                "undefined variable '${head}' in interpolation \
                                 (use ${{{head} ?? default}} for a fallback)"
                            ),
                        });
                    }
                },
                Some(v) => v,
            };
            for field in parts_iter {
                val = match &val {
                    Value::Map(m) => m
                        .get(field)
                        .or_else(|| m.get("*"))
                        .cloned()
                        .unwrap_or(Value::Nil),
                    _ => Value::Nil,
                };
            }
            let fire = match coalesce {
                Some((InterpCoalesce::Nil, _)) => matches!(val, Value::Nil),
                Some((InterpCoalesce::Falsy, _)) => !val.is_truthy(),
                None => false,
            };
            if fire {
                self.eval_interp_default(coalesce.unwrap().1).await
            } else {
                Ok(val)
            }
        })
    }

    /// Evaluate a `${x ?? <default>}` fallback: `src` is the raw expression
    /// text after the coalescing operator. Lexed, parsed, and evaluated as
    /// ordinary Mix in the current scope, so a default is anything from a
    /// string literal (`""`, `"n/a"`) to a variable (`$fallback`) to a call
    /// (`upper($y)`). An empty default (`${x ??}`) is the empty string.
    /// Limitation: a default cannot contain a literal `}` (the interpolation
    /// scanner ends at the first `}`) — bind it to a variable first.
    fn eval_interp_default<'a>(
        &'a mut self,
        src: &'a str,
    ) -> Pin<Box<dyn Future<Output = MixResult<Value>> + 'a>> {
        Box::pin(async move {
            if src.is_empty() {
                return Ok(Value::String(String::new()));
            }
            let mut lexer = crate::lexer::Lexer::new(src);
            let stmts = lexer
                .tokenize()
                .and_then(|tokens| crate::parser::Parser::new(tokens, src).parse_program())?;
            self.execute(&stmts).await
        })
    }

    /// Get the scope (for variable name completion).
    pub fn scope(&self) -> &Scope {
        &self.scope
    }

    /// Get the current file being executed.
    pub fn current_file(&self) -> Option<&str> {
        self.ctx.current_file.as_deref()
    }

    /// Get the current line number.
    pub fn current_line(&self) -> usize {
        self.ctx.current_line
    }

    /// Get the names of registered extension functions.
    pub fn extension_names(&self) -> Vec<String> {
        self.globals.borrow().extensions.keys().cloned().collect()
    }

    /// Get the current Bus address stack.
    pub fn address_stack(&self) -> &[String] {
        &self.ctx.address_stack
    }

    /// Run a statement list as an invocation ROOT (top-level script, a
    /// function body via `call_function`, a Bus handler body). A
    /// `return` that reaches here at `function_depth == 0` is unwrapped
    /// to its value (scripts-as-messages); inside a function
    /// (`function_depth > 0`) it still propagates so `call_function`
    /// catches it.
    pub fn execute<'a>(
        &'a mut self,
        stmts: &'a [Stmt],
    ) -> Pin<Box<dyn Future<Output = MixResult<Value>> + 'a>> {
        self.execute_inner(stmts, true)
    }

    /// Run a NESTED block — an `if`/`elseif`/`else` branch, a loop body,
    /// a `select` case, a `try`/`catch` body, an `address` body. Unlike
    /// [`execute`](Self::execute) this NEVER unwraps a `Return`: it
    /// propagates `Err(MixError::Return)` so the enclosing statement
    /// bubbles it up to the invocation root (or a `call_function`
    /// boundary), where it is unwrapped exactly once. Using `execute`
    /// for a nested block would swallow a `return` inside a *top-level*
    /// control-flow block (`function_depth == 0`): the value would be
    /// discarded as `Ok(..)` and execution would wrongly continue past
    /// the block (the webd-handler `if $cond then return {..} end`
    /// fall-through bug).
    fn execute_block<'a>(
        &'a mut self,
        stmts: &'a [Stmt],
    ) -> Pin<Box<dyn Future<Output = MixResult<Value>> + 'a>> {
        self.execute_inner(stmts, false)
    }

    fn execute_inner<'a>(
        &'a mut self,
        stmts: &'a [Stmt],
        unwrap_top_return: bool,
    ) -> Pin<Box<dyn Future<Output = MixResult<Value>> + 'a>> {
        Box::pin(async move {
            // Top-level `fn` hoisting (0.63.0): bind every function
            // declared at the DIRECT top level of an invocation root
            // before the first statement runs, so a forward call works
            // (`print(f(2))` above `fn f($x)`). Top level ONLY — a `fn`
            // nested in a block (an `if` branch, a loop body) still binds
            // when its branch executes: hoisting those would make a
            // conditional definition unconditional and change meaning.
            // Safe because Mix `fn` captures nothing from the enclosing
            // scope, so an early binding cannot close over a
            // not-yet-defined value. Diagnostics stay on the in-flow
            // `FunctionDef` arm (which re-binds identically when reached),
            // so warning order and count are unchanged; for duplicate
            // top-level names, code between the two definitions still
            // sees the earlier one, exactly as before.
            if unwrap_top_return && self.ctx.function_depth == 0 {
                for stmt in stmts {
                    if let StmtKind::FunctionDef { name, params, body } = &stmt.kind {
                        let f = self.build_function(name, params, body);
                        self.scope.define_function(f);
                    }
                }
            }
            let mut last = Value::Nil;
            for stmt in stmts {
                {
                    let g = self.globals.borrow();
                    if g.interrupted.load(Ordering::Relaxed) {
                        g.interrupted.store(false, Ordering::Relaxed);
                        drop(g);
                        return Err(self.runtime_err("interrupted"));
                    }
                }
                // Knob C: wall-clock deadline. Armed lazily on the first
                // statement poll so the budget starts when this invocation
                // begins executing (not when set_limits was called); fail
                // closed once it elapses. Once-per-statement; a single
                // blocking builtin (run/ssh_run/http_*) can't be interrupted
                // mid-syscall — see EvalLimits::time_limit. No-op (one
                // Option check) when no time limit is configured.
                if let Some(lim) = self.ctx.time_limit {
                    let dl = *self
                        .ctx
                        .deadline
                        .get_or_insert_with(|| std::time::Instant::now() + lim);
                    if std::time::Instant::now() >= dl {
                        return Err(self.runtime_err("deadline exceeded"));
                    }
                }
                // Sync prefix: skip Box::pin'ing exec_stmt for stmts whose
                // body is statically known to never await. Recursion-heavy
                // function bodies (fib's `if n < 2 then return n end`) hit
                // this path and avoid the per-call exec_stmt future
                // allocation when the guard fires.
                if Self::is_stmt_sync(stmt) {
                    match self.exec_stmt_sync(stmt) {
                        Ok(v) => {
                            last = v;
                            continue;
                        }
                        Err(MixError::Return { value })
                            if unwrap_top_return && self.ctx.function_depth == 0 =>
                        {
                            return Ok(value);
                        }
                        Err(e) => {
                            let e = self.attach_span(e);
                            if !matches!(
                                e,
                                MixError::Return { .. }
                                    | MixError::Break(_)
                                    | MixError::Continue(_)
                                    | MixError::ExitRequest { .. }
                            ) {
                                self.track_error_attempt(&e);
                            }
                            return Err(e);
                        }
                    }
                }
                // Partial inline: when only the inner expression needs to
                // await but the surrounding stmt logic is trivial (Return),
                // skip `exec_stmt`'s outer Box::pin. Hot for fib's
                // `return fib($n-1) + fib($n-2)` — one less allocation per
                // recursive call (~175K calls in BENCH_FIB).
                //
                // Bypassed when trace is on so `exec_stmt` can log
                // statements at depth uniformly.
                if !self.globals.borrow().trace
                    && let StmtKind::Return(expr) = &stmt.kind
                {
                    if stmt.line > 0 {
                        self.ctx.current_line = stmt.line;
                    }
                    self.track_stmt_keywords(stmt);
                    let value = match expr {
                        Some(e) => match self.eval_expr(e).await {
                            Ok(v) => v,
                            Err(e) => {
                                let e = self.attach_span(e);
                                if !matches!(
                                    e,
                                    MixError::Return { .. }
                                        | MixError::Break(_)
                                        | MixError::Continue(_)
                                        | MixError::ExitRequest { .. }
                                ) {
                                    self.track_error_attempt(&e);
                                }
                                return Err(e);
                            }
                        },
                        None => Value::Nil,
                    };
                    if unwrap_top_return && self.ctx.function_depth == 0 {
                        return Ok(value);
                    }
                    return Err(MixError::Return { value });
                }
                match self.exec_stmt(stmt).await {
                    Ok(v) => last = v,
                    // Invocation-root return: unwrap the value (enables
                    // scripts-as-messages). Only the root unwraps — a
                    // nested block runs via `execute_block`
                    // (`unwrap_top_return == false`) so its `return`
                    // propagates here; inside a function (depth > 0)
                    // Return also propagates so call_function catches it.
                    Err(MixError::Return { value })
                        if unwrap_top_return && self.ctx.function_depth == 0 =>
                    {
                        return Ok(value);
                    }
                    Err(e) => {
                        let e = self.attach_span(e);
                        if !matches!(
                            e,
                            MixError::Return { .. }
                                | MixError::Break(_)
                                | MixError::Continue(_)
                                | MixError::ExitRequest { .. }
                        ) {
                            self.track_error_attempt(&e);
                        }
                        return Err(e);
                    }
                }
            }
            Ok(last)
        })
    }

    /// Attach the current source location to a RuntimeError or
    /// Structured error that has no span.
    fn attach_span(&self, err: MixError) -> MixError {
        match err {
            MixError::RuntimeError { msg, span: None } if self.ctx.current_line > 0 => {
                MixError::RuntimeError {
                    msg,
                    span: Some(crate::error::Span {
                        line: self.ctx.current_line,
                        column: 0,
                        file: self.ctx.current_file.as_deref().map(str::to_string),
                    }),
                }
            }
            MixError::Structured(mut info) if info.span.is_none() && self.ctx.current_line > 0 => {
                info.span = Some(crate::error::Span {
                    line: self.ctx.current_line,
                    column: 0,
                    file: self.ctx.current_file.as_deref().map(str::to_string),
                });
                MixError::Structured(info)
            }
            other => other,
        }
    }

    /// Emit a per-statement trace line when `mix trace on` is active.
    ///
    /// Self-contained on purpose: the interactive / `-c` / script paths
    /// install **no** `tracing` subscriber (only `--serve` does), so the
    /// structured event alone would be silently dropped in the REPL — the
    /// one place trace can actually be enabled. The `eprintln!` is the
    /// REPL-visible channel; the `tracing::trace!` mirror keeps the
    /// structured event flowing if a subscriber is ever present. Gated by
    /// the trace flag (REPL-only), so it is inert under `--serve` and adds
    /// just one `RefCell` read per statement when trace is off.
    ///
    /// Called from BOTH statement paths — `exec_stmt` (async) and
    /// `exec_stmt_sync` — because a statement runs through exactly one of
    /// them. Tracing only `exec_stmt` would silently miss every
    /// `is_stmt_sync` statement (a plain `$x = 1`, a bare expression),
    /// which is most of what gets typed at the REPL.
    ///
    /// For this to cover function bodies too, the sync-block fast-path
    /// specializations (`run_sync_block_body`'s fib shape,
    /// `run_sync_block_body_f64`) are gated off when trace is on, so those
    /// bodies fall back to the generic `exec_stmt_sync` loop and trace
    /// uniformly — matching the `!trace` gating already on the
    /// partial-inline and tail-return fast paths.
    #[inline]
    fn trace_stmt(&self, stmt: &Stmt) {
        if self.globals.borrow().trace {
            let file = self.ctx.current_file.as_deref().unwrap_or("<repl>");
            let kind = stmt.kind.trace_name();
            eprintln!("trace {}:{} {}", file, stmt.line, kind);
            tracing::trace!(file = file, line = stmt.line, kind = kind, "statement");
        }
    }

    fn exec_stmt<'a>(
        &'a mut self,
        stmt: &'a Stmt,
    ) -> Pin<Box<dyn Future<Output = MixResult<Value>> + 'a>> {
        Box::pin(async move {
            if stmt.line > 0 {
                self.ctx.current_line = stmt.line;
            }
            self.trace_stmt(stmt);
            self.track_stmt_keywords(stmt);
            match &stmt.kind {
                StmtKind::Expression(expr) => {
                    // Bare identifier matching a user-defined function → call it (bash-style)
                    // SPEC 18 Phase 2 WS3-C.7b — γ-aware predicate so a
                    // per-invocation evaluator (or a promoted parent)
                    // still recognises shared-registry user functions.
                    if let Expr::StringLiteral(name) = expr
                        && self.scope.has_function(name)
                    {
                        return self
                            .eval_expr(&Expr::FunctionCall {
                                name: name.clone(),
                                args: vec![],
                            })
                            .await;
                    }
                    self.eval_expr(expr).await
                }

                StmtKind::Assignment { name, value } => {
                    self.check_event_readonly(name)?;
                    // In-place self-concat fast path: `$s = $s .. rhs` where $s is
                    // already a String — append onto the slot instead of
                    // eval(clone $s) + format! (O(n²) accumulators → O(n)). Gated
                    // on is_expr_sync(rhs): generic eval clones the left operand
                    // BEFORE evaluating RHS, so a yielding/statement RHS that could
                    // rebind $s stays on the generic path to preserve left-before-
                    // right semantics.
                    if let Some(rhs_expr) = Self::self_concat_rhs(name, value)
                        && Self::is_expr_sync(rhs_expr)
                        && self.assignment_target_is_string(name)
                    {
                        let rhs_val = self.eval_expr(rhs_expr).await?;
                        self.append_to_assignment_string(name, &rhs_val)?;
                        return Ok(Value::Nil);
                    }
                    let val = self.eval_expr(value).await?;
                    if self.ctx.function_depth > 0 {
                        // Inside a function: always create/update in current frame
                        self.scope.set_in_current(name.clone(), val);
                    } else {
                        self.scope.update_or_set(name, val);
                    }
                    Ok(Value::Nil)
                }

                StmtKind::FieldAssignment {
                    object,
                    field,
                    value,
                } => {
                    // `$m.a = push($m.a, $v)` — see try_push_self_assign.
                    if let Some(r) = self
                        .try_push_self_assign(object, &[LvSeg::Field(field.as_str())], value)
                        .await?
                    {
                        return Ok(r);
                    }
                    self.check_event_readonly(object)?;
                    let val = self.eval_expr(value).await?;
                    self.ensure_map(object);
                    // SPEC 18 Phase 2 WS3-C.7b — γ-aware in-place
                    // mutation. `get_mut` returns None for shared
                    // globals (the `RefCell` can't hand out a
                    // `&mut Value` that outlives a borrow); the
                    // closure form holds the `borrow_mut` for the
                    // single synchronous insert and then releases.
                    // Knob D: `$m.foo = v` can add keys in a loop. The
                    // closure can't touch `self`, so it RETURNS the error
                    // message to raise (if any) — cap violation, or a
                    // field-assign into a non-map (which before 0.33.0
                    // silently discarded the write).
                    let max_map = self.ctx.max_map_len;
                    let assign_err = self
                        .scope
                        .with_mut_value(object, |v| assign_field_in_place(v, field, val, max_map))
                        .flatten();
                    if let Some(msg) = assign_err {
                        return Err(self.runtime_err(msg));
                    }
                    Ok(Value::Nil)
                }

                StmtKind::PathAssignment { root, path, value } => {
                    // `$m[$k].list = push($m[$k].list, $v)` — append in
                    // place instead of copy-append-writeback.
                    let lhs: Vec<LvSeg<'_>> = path
                        .iter()
                        .map(|s| match s {
                            PathSeg::Field(f) => LvSeg::Field(f.as_str()),
                            PathSeg::Index(e) => LvSeg::Index(e),
                        })
                        .collect();
                    if let Some(r) = self.try_push_self_assign(root, &lhs, value).await? {
                        return Ok(r);
                    }

                    // Order matters and is fixed: readonly check, then the
                    // LHS index expressions left-to-right, then the RHS.
                    // Every one of them can run arbitrary user code, so all
                    // of it must finish BEFORE the root binding is borrowed
                    // — `with_mut_value` holds a `borrow_mut` on the shared
                    // globals `RefCell`, and re-entering the evaluator under
                    // that borrow would panic.
                    self.check_event_readonly(root)?;
                    let mut segs: Vec<ResolvedSeg> = Vec::with_capacity(path.len());
                    for seg in path {
                        segs.push(match seg {
                            PathSeg::Field(f) => ResolvedSeg::Field(f.clone()),
                            PathSeg::Index(e) => ResolvedSeg::Index(self.eval_expr(e).await?),
                        });
                    }
                    let val = self.eval_expr(value).await?;
                    self.ensure_map(root);

                    // Validate the whole path against the CURRENT value
                    // before touching it, so a failure halfway down can't
                    // leave auto-created maps behind (a script can `catch`
                    // and keep running). Then mutate, knowing it will land.
                    let max_map = self.ctx.max_map_len;
                    let check = self
                        .scope
                        .with_value(root, |v| check_path_assign(root, v, &segs, max_map))
                        .flatten();
                    if let Some(error) = check {
                        // The ambiguous-auto-create refusal is catchable by
                        // CODE (`catch $m, $e` → `$e.code == "INDEX_MISSING"`);
                        // the rest keep the plain runtime errors the
                        // single-level writes have always raised.
                        return Err(match error {
                            PathErr::Coded(code, msg) => {
                                self.attach_span(MixError::structured(code, msg))
                            }
                            PathErr::Runtime(msg) => self.runtime_err(msg),
                            PathErr::Error(err) => self.attach_span(err),
                        });
                    }
                    let assign_err = self
                        .scope
                        .with_mut_value(root, |v| assign_path_in_place(v, &segs, val, max_map))
                        .flatten();
                    if let Some(err) = assign_err {
                        return Err(self.runtime_err(err));
                    }
                    Ok(Value::Nil)
                }

                StmtKind::IndexAssignment {
                    object,
                    index,
                    value,
                } => {
                    // `$m[$k] = push($m[$k], $v)` — the map-of-lists append
                    // idiom. See try_push_self_assign.
                    if let Some(r) = self
                        .try_push_self_assign(object, &[LvSeg::Index(index)], value)
                        .await?
                    {
                        return Ok(r);
                    }
                    self.check_event_readonly(object)?;
                    let idx = self.eval_expr(index).await?;
                    let val = self.eval_expr(value).await?;
                    self.ensure_map(object);

                    // SPEC 18 Phase 2 WS3-C.7b — γ-aware in-place
                    // mutation, same shape as FieldAssignment above.
                    // Knob D: `$m[k] = v` can add keys in a loop. The
                    // with_mut_value closure can't touch `self`, so it
                    // RETURNS the error message to raise (if any) — cap
                    // violation, out-of-range/typed list index, or an
                    // index-assign into a non-container. `None` = success.
                    let max_map = self.ctx.max_map_len;
                    let assign_err = self
                        .scope
                        .with_mut_value(object, |v| assign_index_in_place(v, idx, val, max_map))
                        .flatten();
                    if let Some(err) = assign_err {
                        return Err(self.attach_span(err));
                    }
                    Ok(Value::Nil)
                }

                StmtKind::If {
                    condition,
                    then_body,
                    else_ifs,
                    else_body,
                } => {
                    let cond = self.eval_expr(condition).await?;
                    if cond.is_truthy() {
                        return self.execute_block(then_body).await;
                    }
                    for (eif_cond, eif_body) in else_ifs {
                        let cond = self.eval_expr(eif_cond).await?;
                        if cond.is_truthy() {
                            return self.execute_block(eif_body).await;
                        }
                    }
                    if let Some(eb) = else_body {
                        return self.execute_block(eb).await;
                    }
                    Ok(Value::Nil)
                }

                StmtKind::For {
                    var,
                    start,
                    end,
                    step,
                    body,
                    label,
                } => {
                    let start_value = self.eval_expr(start).await?;
                    let start_val = as_loop_bound(
                        "for-loop start",
                        required_number("for-loop start", &start_value)?,
                    )?;
                    let end_value = self.eval_expr(end).await?;
                    let end_val = as_loop_bound(
                        "for-loop end",
                        required_number("for-loop end", &end_value)?,
                    )?;
                    let step_val = match step {
                        Some(s) => {
                            let value = self.eval_expr(s).await?;
                            as_loop_step(
                                "for-loop step",
                                required_number("for-loop step", &value)?,
                            )?
                        }
                        None => 1.0,
                    };
                    // Fast path: body is `[Assignment]` with a fully-sync RHS
                    // (arithmetic/var-reads/literals only). Skip the per-
                    // iteration Box::pin'd execute(body) future. loop_arith's
                    // 1M iterations dominate the bench; this avoids 1M heap
                    // allocations of the future state machine.
                    let fast_body = if body.len() == 1 {
                        if let StmtKind::Assignment {
                            name: a_name,
                            value: a_value,
                        } = &body[0].kind
                        {
                            Some((a_name, a_value))
                        } else {
                            None
                        }
                    } else {
                        None
                    };

                    // Fast path 2: body is a mutating-builtin call with a
                    // Variable target and (optionally) a sync-evaluable
                    // value arg. list_build pushes 20000 ints into a list
                    // and falls outside the assignment fast path; the
                    // generic path Box::pin's execute(body) every iter
                    // plus call_mutating_builtin once more — two heap
                    // allocs × 20000 = ~40k Box::pin worth of overhead.
                    // This branch dispatches inline.
                    let mut_body: Option<(&str, &str, Option<&Expr>)> = if body.len() == 1 {
                        if let StmtKind::Expression(Expr::FunctionCall {
                            name: fn_name,
                            args: fn_args,
                        }) = &body[0].kind
                        {
                            if matches!(fn_name.as_str(), "push" | "pop" | "shift") {
                                if let Some(Expr::Variable(target)) = fn_args.first() {
                                    let val_expr = if fn_name.as_str() == "push" {
                                        fn_args.get(1)
                                    } else {
                                        None
                                    };
                                    if fn_name.as_str() != "push" || val_expr.is_some() {
                                        let val_sync = match val_expr {
                                            Some(e) => Self::is_expr_sync(e),
                                            None => true,
                                        };
                                        if val_sync {
                                            Some((fn_name.as_str(), target.as_str(), val_expr))
                                        } else {
                                            None
                                        }
                                    } else {
                                        None
                                    }
                                } else {
                                    None
                                }
                            } else {
                                None
                            }
                        } else {
                            None
                        }
                    } else {
                        None
                    };

                    // Fast path 3: body is a single `[IndexAssignment]`
                    // with sync-evaluable index and value. table bench's
                    // 6000 iters of `$t["k${i}"] = $i*2` otherwise Box::pin
                    // execute(body) plus two Box::pin'd eval_expr per iter
                    // (index and value). Hoist the target Map/List slot
                    // pointer once, drive index+value through the sync
                    // evaluators inside the loop, and insert in place.
                    let idx_body: Option<(&str, &Expr, &Expr)> = if body.len() == 1 {
                        if let StmtKind::IndexAssignment {
                            object,
                            index,
                            value,
                        } = &body[0].kind
                        {
                            if Self::is_expr_sync(index) && Self::is_expr_sync(value) {
                                Some((object.as_str(), index, value))
                            } else {
                                None
                            }
                        } else {
                            None
                        }
                    } else {
                        None
                    };

                    let mut i = start_val;
                    if let Some((target, idx_expr, val_expr)) = idx_body {
                        self.check_event_readonly(target)?;
                        let in_function = self.ctx.function_depth > 0;
                        if !in_function
                            && self.scope.is_top_level()
                            && self.can_use_shared_scope_raw_slots()
                        {
                            // Ensure the target slot exists as a Map (mirrors
                            // generic IndexAssignment); ensure_map is a no-op
                            // when the slot is already a List or Map.
                            self.ensure_map(target);
                            let (target_slot, var_slot) =
                                self.scope.global_slot_ptr_pair(target, var);

                            // Pre-bind every Variable referenced by index +
                            // value so reads inside try_eval_expr_sync skip
                            // the HashMap probe — same trick as the fast_body
                            // path. For table bench's `$t["k${i}"] = $i * 2`
                            // this caches `$i` (loop counter, also pushed as
                            // f64 below).
                            let mut rhs_vars: Vec<&str> = Vec::new();
                            Self::collect_variable_refs(idx_expr, &mut rhs_vars);
                            Self::collect_variable_refs(val_expr, &mut rhs_vars);
                            let cache_start = self.ctx.var_slot_cache.len();
                            for n in &rhs_vars {
                                // Skip the loop counter — already in
                                // var_slot_cache_f64 once we push it below.
                                // Distinct cache lookups don't conflict, so
                                // duplication is harmless but wasteful.
                                if *n == var.as_str() {
                                    continue;
                                }
                                // try_ variant — never insert. An unbound
                                // RHS name must surface as the canonical
                                // undefined-variable RuntimeError via the
                                // cache-miss fallback in try_eval_expr_sync,
                                // not be silently created as Nil.
                                if let Some(p) = self.scope.try_global_slot_ptr(n) {
                                    self.ctx
                                        .var_slot_cache
                                        .push((*n as *const str, p as *const Value));
                                }
                            }
                            // Push the loop counter as f64 so $i reads in
                            // the index/value expressions resolve to a single
                            // deref. Set initial value so the first iter
                            // sees it.
                            unsafe {
                                *var_slot = Value::Number(i);
                            }
                            let var_f64: *mut f64 = match unsafe { &mut *var_slot } {
                                Value::Number(n) => n as *mut f64,
                                _ => unreachable!("var_slot just set to Number"),
                            };
                            let f64_cache_start = self.ctx.var_slot_cache_f64.len();
                            self.ctx
                                .var_slot_cache_f64
                                .push((var.as_str() as *const str, var_f64 as *const f64));

                            let mut loop_budget: u32 = 0;
                            let result: MixResult<Value> = (|| {
                                loop {
                                    if step_val > 0.0 && i > end_val {
                                        break;
                                    }
                                    if step_val < 0.0 && i < end_val {
                                        break;
                                    }
                                    self.poll_native_loop(&mut loop_budget)?;
                                    unsafe {
                                        *var_f64 = i;
                                    }
                                    let idx_val = match self.try_eval_expr_sync(idx_expr) {
                                        Some(Ok(v)) => v,
                                        Some(Err(e)) => return Err(e),
                                        None => unreachable!("is_expr_sync verified above"),
                                    };
                                    let v_val = match self.try_eval_expr_sync(val_expr) {
                                        Some(Ok(v)) => v,
                                        Some(Err(e)) => return Err(e),
                                        None => unreachable!("is_expr_sync verified above"),
                                    };
                                    // Shared assign helper returns the error to
                                    // raise (if any); its `&mut *target_slot`
                                    // borrow ends when it returns, so calling
                                    // self.runtime_err afterward can't alias self
                                    // through the live raw-pointer &mut. Same
                                    // signed-index + non-container semantics as
                                    // the generic IndexAssignment arm.
                                    let max_map = self.ctx.max_map_len;
                                    let assign_err = assign_index_in_place(
                                        unsafe { &mut *target_slot },
                                        idx_val,
                                        v_val,
                                        max_map,
                                    );
                                    if let Some(err) = assign_err {
                                        return Err(self.attach_span(err));
                                    }
                                    i += step_val;
                                }
                                Ok(Value::Nil)
                            })();
                            self.ctx.var_slot_cache.truncate(cache_start);
                            self.ctx.var_slot_cache_f64.truncate(f64_cache_start);
                            return result;
                        }
                    }

                    if let Some((mut_name, target_var, val_expr)) = mut_body {
                        self.check_event_readonly(target_var)?;
                        let in_function = self.ctx.function_depth > 0;
                        if !in_function
                            && self.scope.is_top_level()
                            && self.can_use_shared_scope_raw_slots()
                        {
                            // Hoist target list ptr, loop var slot, and any
                            // val_expr variable slots once. list_build's 20k
                            // pushes drop three HashMap probes per iter (loop
                            // var write, val_expr Variable read, target
                            // get_mut) to raw ptr ops.
                            //
                            // SAFETY: try_eval_expr_sync only reads scope; it
                            // never inserts into globals. We pre-allocate
                            // every key we'll touch (var, target, rhs_vars)
                            // via global_slot_ptr so the bucket array can't
                            // relocate inside the loop. Vec<Value>'s heap
                            // buffer may reallocate as it grows, but the
                            // outer Value::List slot in the HashMap stays
                            // put, so re-resolving the inner Vec ptr each
                            // iter would be safe — but pushing through the
                            // outer slot's enum-variant match is identical
                            // to a single deref + match here.
                            let (var_slot, target_slot) =
                                self.scope.global_slot_ptr_pair(var, target_var);
                            if !matches!(unsafe { &*target_slot }, Value::List(_)) {
                                return Err(MixError::RuntimeError {
                                    span: None,
                                    msg: format!("{}() expects a list variable", mut_name),
                                });
                            }
                            let mut rhs_vars: Vec<&str> = Vec::new();
                            if let Some(e) = val_expr {
                                Self::collect_variable_refs(e, &mut rhs_vars);
                            }
                            let cache_start = self.ctx.var_slot_cache.len();
                            for n in &rhs_vars {
                                // try_ variant — unbound names must error,
                                // not be created as Nil. See sibling site.
                                if let Some(p) = self.scope.try_global_slot_ptr(n) {
                                    self.ctx
                                        .var_slot_cache
                                        .push((*n as *const str, p as *const Value));
                                }
                            }
                            let mut loop_budget: u32 = 0;
                            let inner: MixResult<()> = (|| {
                                loop {
                                    if step_val > 0.0 && i > end_val {
                                        break;
                                    }
                                    if step_val < 0.0 && i < end_val {
                                        break;
                                    }
                                    self.poll_native_loop(&mut loop_budget)?;
                                    // This native loop bypasses Expr::FunctionCall,
                                    // so emit the same once-per-attempt builtin event
                                    // the generic path would have emitted.
                                    self.track_builtin_attempt(mut_name);
                                    unsafe {
                                        *var_slot = Value::Number(i);
                                    }
                                    let push_val = if let Some(e) = val_expr {
                                        match self.try_eval_expr_sync(e) {
                                            Some(Ok(v)) => Some(v),
                                            Some(Err(e)) => return Err(e),
                                            None => unreachable!("is_expr_sync verified above"),
                                        }
                                    } else {
                                        None
                                    };
                                    // Knob D: capture + report after the
                                    // `&mut *target_slot` borrow ends (see the
                                    // map-insert site above for the aliasing
                                    // rationale).
                                    let max_list = self.ctx.max_list_len;
                                    let mut cap_violation: Option<usize> = None;
                                    // CoW invariant (card 4 §4): the slot is
                                    // the only owner on this fast path (the
                                    // pushed value was evaluated into a local
                                    // above) — make_mut stays in-place; never
                                    // add a second Rc owner of the target
                                    // list before this point.
                                    match unsafe { &mut *target_slot } {
                                        Value::List(list) => {
                                            let list = Rc::make_mut(list);
                                            match mut_name {
                                                "push" => {
                                                    list.push(push_val.unwrap());
                                                    if let Some(max) = max_list
                                                        && list.len() > max
                                                    {
                                                        cap_violation = Some(list.len());
                                                    }
                                                }
                                                "pop" => {
                                                    list.pop();
                                                }
                                                "shift" => {
                                                    if !list.is_empty() {
                                                        list.remove(0);
                                                    }
                                                }
                                                _ => unreachable!(),
                                            }
                                        }
                                        _ => unreachable!("verified above"),
                                    }
                                    if let Some(n) = cap_violation {
                                        return Err(self.runtime_err(format!(
                                            "list length {} exceeds limit {}",
                                            n,
                                            max_list.unwrap_or(0)
                                        )));
                                    }
                                    i += step_val;
                                }
                                Ok(())
                            })();
                            self.ctx.var_slot_cache.truncate(cache_start);
                            inner?;
                            return Ok(Value::Nil);
                        }
                        let mut loop_budget: u32 = 0;
                        loop {
                            if step_val > 0.0 && i > end_val {
                                break;
                            }
                            if step_val < 0.0 && i < end_val {
                                break;
                            }
                            self.poll_native_loop(&mut loop_budget)?;
                            // This native loop bypasses Expr::FunctionCall; keep
                            // stats identical to the generic mutating-builtin path.
                            self.track_builtin_attempt(mut_name);
                            // Loop-var binding respects function scope (see the
                            // ForEach generic loop): inside a function bind
                            // locally so the counter shadows rather than
                            // clobbers a same-named caller/global var.
                            if in_function {
                                self.scope.set_in_current(var.clone(), Value::Number(i));
                            } else {
                                self.scope.update_or_set(var, Value::Number(i));
                            }
                            let push_val = if let Some(e) = val_expr {
                                match self.try_eval_expr_sync(e) {
                                    Some(Ok(v)) => Some(v),
                                    Some(Err(e)) => return Err(e),
                                    None => unreachable!("is_expr_sync verified above"),
                                }
                            } else {
                                None
                            };
                            // SPEC 18 Phase 2 WS3-C.7b — γ-aware
                            // mutation; `with_mut_value` walks the
                            // shared global frame when present, where
                            // `get_mut` returns None. Result encoding:
                            // `Some(Ok)` = matched a list; `Some(Err)` =
                            // matched non-list (canonical error); `None`
                            // = variable not visible.
                            // Knob D: capture the cap + report out of band
                            // (the with_mut_value closure can't touch self).
                            let max_list = self.ctx.max_list_len;
                            let mut cap_violation: Option<usize> = None;
                            let outcome = self.scope.with_mut_value(target_var, |v| match v {
                                Value::List(list) => {
                                    // CoW invariant (card 4 §4): sole owner
                                    // here — see the raw-slot sibling above.
                                    let list = Rc::make_mut(list);
                                    match mut_name {
                                        "push" => {
                                            list.push(push_val.unwrap());
                                            if let Some(max) = max_list
                                                && list.len() > max
                                            {
                                                cap_violation = Some(list.len());
                                            }
                                        }
                                        "pop" => {
                                            list.pop();
                                        }
                                        "shift" => {
                                            if !list.is_empty() {
                                                list.remove(0);
                                            }
                                        }
                                        _ => unreachable!(),
                                    }
                                    Ok(())
                                }
                                _ => Err(()),
                            });
                            match outcome {
                                Some(Ok(())) => {}
                                _ => {
                                    return Err(MixError::RuntimeError {
                                        span: None,
                                        msg: format!("{}() expects a list variable", mut_name),
                                    });
                                }
                            }
                            if let Some(n) = cap_violation {
                                return Err(self.runtime_err(format!(
                                    "list length {} exceeds limit {}",
                                    n,
                                    max_list.unwrap_or(0)
                                )));
                            }
                            i += step_val;
                        }
                        return Ok(Value::Nil);
                    }

                    if let Some((assign_name, assign_value)) = fast_body {
                        self.check_event_readonly(assign_name)?;
                        // Hoist the function_depth branch out of the 1M-iter
                        // hot loop: it can't change inside a sync-fast-path
                        // body (no function calls allowed by `try_eval_expr_sync`).
                        let in_function = self.ctx.function_depth > 0;

                        // Top-level + fully-sync RHS fast path: cache raw
                        // pointers to the loop var and assignment target
                        // slots in the global frame. Hot loop becomes pure
                        // pointer writes — no FxHash, no bucket probe, no
                        // top-level branch in update_or_set per iter.
                        // loop_arith does 4 HashMap ops per iter × 1M; this
                        // collapses two of them (the writes) to ptr stores.
                        //
                        // SAFETY: `try_eval_expr_sync` only does read lookups
                        // (`scope.get`); it never inserts into `frames[0]`,
                        // so the bucket array never relocates and the cached
                        // pointers stay valid for the whole loop. The static
                        // `is_expr_sync` gate guarantees no fall-back to the
                        // async `eval_expr` path inside the loop.
                        if !in_function
                            && self.scope.is_top_level()
                            && Self::is_expr_sync(assign_value)
                            && self.can_use_shared_scope_raw_slots()
                        {
                            let (var_slot, assign_slot) =
                                self.scope.global_slot_ptr_pair(var, assign_name);
                            // Pre-bind every Variable referenced in the RHS
                            // so its read inside `try_eval_expr_sync` finds
                            // a cached slot ptr and skips the HashMap probe.
                            // For loop_arith ($sum = $sum + $i*2 - 1) this
                            // covers $sum and $i — every read in the 1M-iter
                            // body lands on the cache. Keys point into the
                            // AST (Expr::Variable's owned `name: String`)
                            // which is stable for the lifetime of `&self`.
                            let mut rhs_vars: Vec<&str> = Vec::new();
                            Self::collect_variable_refs(assign_value, &mut rhs_vars);
                            let cache_start = self.ctx.var_slot_cache.len();
                            for name in &rhs_vars {
                                // try_ variant — unbound names must error,
                                // not be created as Nil. See sibling site.
                                if let Some(ptr) = self.scope.try_global_slot_ptr(name) {
                                    self.ctx
                                        .var_slot_cache
                                        .push((*name as *const str, ptr as *const Value));
                                }
                            }
                            let result: MixResult<Value> = (|| {
                                // Numeric specialization: probe the RHS as
                                // an f64-valued expression. If every leaf
                                // resolves to Number (loop_arith: $sum + $i
                                // * 2 - 1 — both $sum and $i are written
                                // with Value::Number each iter), the loop
                                // body becomes pure f64 arithmetic, no
                                // Value clones. Type stability holds for
                                // the loop's lifetime: the assignment
                                // writes Number, the iteration var writes
                                // Number, and we don't write any other
                                // slots in this fast path.
                                unsafe {
                                    *var_slot = Value::Number(i);
                                }
                                if step_val > 0.0 && i > end_val {
                                    return Ok(Value::Nil);
                                }
                                if step_val < 0.0 && i < end_val {
                                    return Ok(Value::Nil);
                                }
                                if let Some(probe) = self.try_eval_expr_num(assign_value) {
                                    let first = match probe {
                                        Ok(n) => n,
                                        Err(e) => return Err(e),
                                    };
                                    unsafe {
                                        *assign_slot = Value::Number(first);
                                    }
                                    // In-place f64 stores: once both slots
                                    // hold Value::Number, we update the
                                    // payload f64 directly instead of
                                    // overwriting the entire 72-byte Value
                                    // each iter. Type-stability is ours by
                                    // construction (var_slot is the For
                                    // counter, assign_slot is set by us
                                    // every iter to Value::Number).
                                    let var_f64: *mut f64 = match unsafe { &mut *var_slot } {
                                        Value::Number(n) => n as *mut f64,
                                        _ => unreachable!("var_slot just set to Number"),
                                    };
                                    let assign_f64: *mut f64 = match unsafe { &mut *assign_slot } {
                                        Value::Number(n) => n as *mut f64,
                                        _ => unreachable!("assign_slot just set to Number"),
                                    };
                                    // f64 cache: the loop has type-stable
                                    // Number slots for both the counter and
                                    // the accumulator. Pushing them into
                                    // var_slot_cache_f64 makes per-iter
                                    // Variable reads of $sum / $i resolve
                                    // through a single deref, skipping the
                                    // Value-cache walk and the Value::Number
                                    // variant-tag match. The pointers are
                                    // valid for the loop's lifetime (the
                                    // global slot's bucket array doesn't
                                    // relocate inside the loop body — no
                                    // Assignment runs above this layer).
                                    // Names are borrowed `&str` from the
                                    // caller's stack via global_slot_ptr's
                                    // signature; we push them as the
                                    // For-stmt's owned String slices.
                                    let f64_cache_start = self.ctx.var_slot_cache_f64.len();
                                    self.ctx
                                        .var_slot_cache_f64
                                        .push((var.as_str() as *const str, var_f64 as *const f64));
                                    self.ctx.var_slot_cache_f64.push((
                                        assign_name.as_str() as *const str,
                                        assign_f64 as *const f64,
                                    ));
                                    // NumOp bytecode fast path: compile
                                    // assign_value once against the f64 cache.
                                    // Each Variable read becomes a single
                                    // *const f64 deref; tree recursion is
                                    // replaced with a linear walk over a
                                    // contiguous Vec<NumOpcode>. loop_arith's
                                    // `$sum + $i * 2 - 1` compiles to 7 opcodes.
                                    let mut prog: Vec<NumOpcode> = Vec::with_capacity(8);
                                    // Depth-gated: a program needing more
                                    // live operands than the VM's fixed
                                    // stack falls back to the AST walker.
                                    let compiled = build_num_program(
                                        assign_value,
                                        &self.ctx.var_slot_cache_f64,
                                        &mut prog,
                                    )
                                    .is_some_and(|depth| depth <= NUM_STACK_SLOTS);
                                    i += step_val;
                                    let mut loop_budget: u32 = 0;
                                    let inner: MixResult<()> = (|| {
                                        if compiled {
                                            loop {
                                                if step_val > 0.0 && i > end_val {
                                                    break;
                                                }
                                                if step_val < 0.0 && i < end_val {
                                                    break;
                                                }
                                                self.poll_native_loop(&mut loop_budget)?;
                                                unsafe {
                                                    *var_f64 = i;
                                                }
                                                let n = eval_num_program(&prog)?;
                                                unsafe {
                                                    *assign_f64 = n;
                                                }
                                                i += step_val;
                                            }
                                            return Ok(());
                                        }
                                        loop {
                                            if step_val > 0.0 && i > end_val {
                                                break;
                                            }
                                            if step_val < 0.0 && i < end_val {
                                                break;
                                            }
                                            self.poll_native_loop(&mut loop_budget)?;
                                            unsafe {
                                                *var_f64 = i;
                                            }
                                            let n = match self.try_eval_expr_num(assign_value) {
                                                Some(Ok(n)) => n,
                                                Some(Err(e)) => return Err(e),
                                                None => unreachable!(
                                                    "type-stable per fast-path safety contract"
                                                ),
                                            };
                                            unsafe {
                                                *assign_f64 = n;
                                            }
                                            i += step_val;
                                        }
                                        Ok(())
                                    })(
                                    );
                                    self.ctx.var_slot_cache_f64.truncate(f64_cache_start);
                                    inner?;
                                    return Ok(Value::Nil);
                                }
                                // Generic Value path: probe failed (RHS
                                // touches a non-Number variable, or uses
                                // an op outside the numeric subset like
                                // string concat / comparisons returning
                                // Bool).
                                //
                                // In-place self-concat fast path: detect
                                // `$s = $s .. rhs` shape with `$s` already
                                // a String. The strings bench is 3000 iters
                                // of `$s = $s .. "ab"` which otherwise does
                                // quadratic memcpy via `format!` → 9 MB of
                                // allocator traffic. Push_str into the slot
                                // String in-place reuses geometric capacity
                                // and drops the bench from O(n²) to O(n).
                                let concat_self_rhs: Option<&Expr> =
                                    if let Expr::BinaryOp { left, op, right } = assign_value {
                                        if matches!(op, BinOp::Concat) {
                                            if let Expr::Variable(left_name) = &**left {
                                                if left_name == assign_name {
                                                    Some(&**right)
                                                } else {
                                                    None
                                                }
                                            } else {
                                                None
                                            }
                                        } else {
                                            None
                                        }
                                    } else {
                                        None
                                    };
                                if let Some(rhs_expr) = concat_self_rhs
                                    && matches!(unsafe { &*assign_slot }, Value::String(_))
                                {
                                    let mut loop_budget: u32 = 0;
                                    loop {
                                        if step_val > 0.0 && i > end_val {
                                            break;
                                        }
                                        if step_val < 0.0 && i < end_val {
                                            break;
                                        }
                                        self.poll_native_loop(&mut loop_budget)?;
                                        unsafe {
                                            *var_slot = Value::Number(i);
                                        }
                                        let rhs_val = match self.try_eval_expr_sync(rhs_expr) {
                                            Some(Ok(v)) => v,
                                            Some(Err(e)) => return Err(e),
                                            None => unreachable!(
                                                "is_expr_sync guarantees try_eval_expr_sync returns Some"
                                            ),
                                        };
                                        // Knob D: capture the in-place-grown
                                        // length and report AFTER the
                                        // `&mut *assign_slot` borrow ends — this
                                        // O(n) push_str loop bypasses the `..`
                                        // operator check, and calling
                                        // self.runtime_err while the raw-pointer
                                        // &mut is live would alias self.
                                        let max_string = self.ctx.max_string_len;
                                        let mut cap_violation: Option<usize> = None;
                                        match unsafe { &mut *assign_slot } {
                                            Value::String(s) => {
                                                rhs_val.write_mix(s);
                                                if let Some(max) = max_string
                                                    && s.len() > max
                                                {
                                                    cap_violation = Some(s.len());
                                                }
                                            }
                                            _ => unreachable!(
                                                "type-stable: only writers in this loop are push_str into the same slot"
                                            ),
                                        }
                                        if let Some(cur) = cap_violation {
                                            return Err(self.runtime_err(format!(
                                                "string length {} exceeds limit {}",
                                                cur,
                                                max_string.unwrap_or(0)
                                            )));
                                        }
                                        i += step_val;
                                    }
                                    return Ok(Value::Nil);
                                }
                                let val = match self.try_eval_expr_sync(assign_value) {
                                    Some(Ok(v)) => v,
                                    Some(Err(e)) => return Err(e),
                                    None => unreachable!(
                                        "is_expr_sync guarantees try_eval_expr_sync returns Some"
                                    ),
                                };
                                unsafe {
                                    *assign_slot = val;
                                }
                                i += step_val;
                                let mut loop_budget: u32 = 0;
                                loop {
                                    if step_val > 0.0 && i > end_val {
                                        break;
                                    }
                                    if step_val < 0.0 && i < end_val {
                                        break;
                                    }
                                    self.poll_native_loop(&mut loop_budget)?;
                                    unsafe {
                                        *var_slot = Value::Number(i);
                                    }
                                    let val = match self.try_eval_expr_sync(assign_value) {
                                        Some(Ok(v)) => v,
                                        Some(Err(e)) => return Err(e),
                                        None => unreachable!(
                                            "is_expr_sync guarantees try_eval_expr_sync returns Some"
                                        ),
                                    };
                                    unsafe {
                                        *assign_slot = val;
                                    }
                                    i += step_val;
                                }
                                Ok(Value::Nil)
                            })();
                            self.ctx.var_slot_cache.truncate(cache_start);
                            return result;
                        }

                        let mut loop_budget: u32 = 0;
                        loop {
                            if step_val > 0.0 && i > end_val {
                                break;
                            }
                            if step_val < 0.0 && i < end_val {
                                break;
                            }
                            self.poll_native_loop(&mut loop_budget)?;
                            // Loop-var binding respects function scope (see
                            // the ForEach generic loop) — bind locally inside
                            // a function so the counter shadows, not clobbers.
                            if in_function {
                                self.scope.set_in_current(var.clone(), Value::Number(i));
                            } else {
                                self.scope.update_or_set(var, Value::Number(i));
                            }
                            let val = match self.try_eval_expr_sync(assign_value) {
                                Some(Ok(v)) => v,
                                Some(Err(e)) => return Err(e),
                                None => self.eval_expr(assign_value).await?,
                            };
                            if in_function {
                                self.scope.set_in_current(assign_name.clone(), val);
                            } else {
                                self.scope.update_or_set(assign_name, val);
                            }
                            i += step_val;
                        }
                        return Ok(Value::Nil);
                    }

                    // Generic numeric-For fallback (fast paths above all
                    // returned). Loop-var binding respects function scope.
                    let in_function = self.ctx.function_depth > 0;
                    loop {
                        if step_val > 0.0 && i > end_val {
                            break;
                        }
                        if step_val < 0.0 && i < end_val {
                            break;
                        }
                        // Loop-var binding respects function scope (see the
                        // ForEach generic loop) — bind locally inside a
                        // function so the counter shadows, not clobbers.
                        if in_function {
                            self.scope.set_in_current(var.clone(), Value::Number(i));
                        } else {
                            self.scope.update_or_set(var, Value::Number(i));
                        }
                        match self.execute_block(body).await {
                            Ok(_) => {}
                            Err(MixError::Break(None)) => break,
                            Err(MixError::Break(Some(ref l))) if label.as_deref() == Some(l) => {
                                break;
                            }
                            Err(MixError::Continue(None)) => {}
                            Err(MixError::Continue(Some(ref l))) if label.as_deref() == Some(l) => {
                            }
                            Err(e) => return Err(e),
                        }
                        i += step_val;
                    }
                    Ok(Value::Nil)
                }

                StmtKind::ForEach {
                    var,
                    index_var,
                    iterable,
                    body,
                    label,
                } => {
                    let iter_val = self.eval_expr(iterable).await?;
                    // CoW (card 4): the List arm holds the snapshot Rc —
                    // O(1) at entry (the old code deep-cloned the whole
                    // list here). A body mutation of the source variable
                    // detaches the scope slot once via make_mut, so the
                    // iteration set stays pinned at entry exactly as the
                    // deep-clone snapshot behaved. String/Map arms build
                    // fresh per-entry Vecs either way and keep by-move
                    // item extraction (`mem::take`) so `for each $c in
                    // $str` doesn't pay a per-char String clone.
                    enum IterSrc {
                        Snap(Rc<Vec<Value>>),
                        Owned(Vec<Value>),
                    }
                    // `Some(keys)` ONLY for a two-variable map loop: what the
                    // first variable binds to per iteration. `None` everywhere
                    // else, and the binding site falls back to the position
                    // index — so lists, strings and bytes are unchanged.
                    let mut map_pair_keys: Option<Vec<Value>> = None;
                    let mut src = match &iter_val {
                        Value::List(items) => IterSrc::Snap(Rc::clone(items)),
                        Value::String(s) => IterSrc::Owned(
                            s.chars().map(|c| Value::String(c.to_string())).collect(),
                        ),
                        // A MAP binds by ARITY (v0.68.0, Release A.1):
                        //   one variable  → the KEYS      (unchanged)
                        //   two variables → (KEY, VALUE)  (was (index, key))
                        // Both are snapshotted at entry, matching the key
                        // snapshot this arm has always taken, so a body that
                        // mutates the source map iterates the entry set.
                        //
                        // The flip's gate was "no map-pair dependant exists":
                        // MIX-D3006 made every two-variable loop visible for
                        // the 0.63.0 cycle and the inventory came back with
                        // every site iterating a LIST, on the fleet and
                        // locally. Lists, strings and bytes are untouched —
                        // they still bind (index, item).
                        Value::Map(m) if index_var.is_some() => {
                            map_pair_keys =
                                Some(m.keys().map(|k| Value::String(k.clone())).collect());
                            IterSrc::Owned(m.values().cloned().collect())
                        }
                        Value::Map(m) => {
                            IterSrc::Owned(m.keys().map(|k| Value::String(k.clone())).collect())
                        }
                        // bytes/buffer iterate as NUMBERS 0-255 (v0.64.0),
                        // the same unit `$b[i]` yields. Materialised once at
                        // entry like the String/Map arms, so the iteration
                        // set is pinned even though a `Buffer` is
                        // reference-semantic and the body may grow it.
                        Value::Bytes(b) => {
                            IterSrc::Owned(b.iter().map(|&x| Value::Number(x as f64)).collect())
                        }
                        Value::Buffer(b) => IterSrc::Owned(
                            b.borrow().iter().map(|&x| Value::Number(x as f64)).collect(),
                        ),
                        other => {
                            return Err(MixError::RuntimeError {
                                span: None,
                                msg: format!("cannot iterate over {}", other.type_name()),
                            });
                        }
                    };
                    let items: &[Value] = match &src {
                        IterSrc::Snap(rc) => rc,
                        IterSrc::Owned(v) => v,
                    };

                    // Fast path: top-level + body = `[Assignment]` with a
                    // fully-sync RHS. list_build's `for each $x in $xs:
                    // $total = $total + $x` runs 20000 iters; the generic
                    // path Box::pin's execute(body) every iter. Hoist the
                    // accumulator slot ptr once and drive RHS through the
                    // sync evaluator. When all items are Number, also push
                    // an f64 slot for $x so the body's Variable read of $x
                    // is a single deref.
                    let fast_body: Option<(&str, &Expr)> = if body.len() == 1 {
                        if let StmtKind::Assignment {
                            name: a_name,
                            value: a_value,
                        } = &body[0].kind
                        {
                            if Self::is_expr_sync(a_value) {
                                Some((a_name.as_str(), a_value))
                            } else {
                                None
                            }
                        } else {
                            None
                        }
                    } else {
                        None
                    };
                    let in_function = self.ctx.function_depth > 0;
                    let all_num_items = items.iter().all(|v| matches!(v, Value::Number(_)));
                    let take_fast = !in_function
                        && self.scope.is_top_level()
                        && self.can_use_shared_scope_raw_slots()
                        && index_var.is_none()
                        && fast_body.is_some()
                        && all_num_items;
                    // Mixed-type fast path: items aren't all Number but
                    // assign target is numeric and the body resolves via
                    // try_eval_expr_num (Index Variable specialization
                    // handles `$t[$k]`). Probes the first item — commits
                    // only if it succeeds, leaving the slower path
                    // unaffected if the body shape isn't actually numeric.
                    let take_mixed = !in_function
                        && !take_fast
                        && self.scope.is_top_level()
                        && self.can_use_shared_scope_raw_slots()
                        && index_var.is_none()
                        && fast_body.is_some()
                        && !items.is_empty();
                    if take_mixed {
                        let (assign_name, assign_value) = fast_body.unwrap();
                        self.check_event_readonly(assign_name)?;
                        let (assign_slot, var_slot) =
                            self.scope.global_slot_ptr_pair(assign_name, var);
                        if matches!(unsafe { &*assign_slot }, Value::Number(_)) {
                            let mut rhs_vars: Vec<&str> = Vec::new();
                            Self::collect_variable_refs(assign_value, &mut rhs_vars);
                            let cache_start = self.ctx.var_slot_cache.len();
                            for n in &rhs_vars {
                                if *n == var.as_str() || *n == assign_name {
                                    continue;
                                }
                                // try_ variant — unbound names must error,
                                // not be created as Nil. See sibling site.
                                if let Some(p) = self.scope.try_global_slot_ptr(n) {
                                    self.ctx
                                        .var_slot_cache
                                        .push((*n as *const str, p as *const Value));
                                }
                            }
                            // Loop var pushed as Value-typed cache entry —
                            // try_eval_expr_sync's Variable arm walks this
                            // cache and reads through the slot ptr.
                            self.ctx
                                .var_slot_cache
                                .push((var.as_str() as *const str, var_slot as *const Value));
                            let assign_f64: *mut f64 = match unsafe { &mut *assign_slot } {
                                Value::Number(n) => n as *mut f64,
                                _ => unreachable!(),
                            };
                            let f64_cache_start = self.ctx.var_slot_cache_f64.len();
                            self.ctx
                                .var_slot_cache_f64
                                .push((assign_name as *const str, assign_f64 as *const f64));
                            // Snapshot $total before any mutation so the
                            // generic fall-through doesn't double-count if
                            // try_eval_expr_num rejects mid-loop. (The
                            // per-iter `*assign_f64 = n` writes accumulate
                            // into the live slot; on probe failure we
                            // restore.)
                            let saved_assign = unsafe { *assign_f64 };
                            let inner: MixResult<bool> = (|| {
                                for item in items.iter() {
                                    // Normal assignment (NOT ptr::write):
                                    // the old slot value must DROP — a
                                    // ptr::write here leaked every previous
                                    // non-Number item, and under CoW a
                                    // leaked Rc also pins refcounts and
                                    // degrades later make_muts to copies
                                    // (card 4 c10; Codex-confirmed leak).
                                    unsafe {
                                        *var_slot = item.clone();
                                    }
                                    let n = match self.try_eval_expr_num(assign_value) {
                                        Some(Ok(n)) => n,
                                        Some(Err(e)) => return Err(e),
                                        None => return Ok(false),
                                    };
                                    unsafe {
                                        *assign_f64 = n;
                                    }
                                }
                                Ok(true)
                            })();
                            self.ctx.var_slot_cache_f64.truncate(f64_cache_start);
                            self.ctx.var_slot_cache.truncate(cache_start);
                            match inner {
                                Ok(true) => return Ok(Value::Nil),
                                Ok(false) => {
                                    // Body shape wasn't fully numeric;
                                    // restore $total and let the generic
                                    // loop below run from scratch.
                                    unsafe {
                                        *assign_f64 = saved_assign;
                                    }
                                }
                                Err(e) => return Err(e),
                            }
                        }
                    }
                    if take_fast {
                        let (assign_name, assign_value) = fast_body.unwrap();
                        self.check_event_readonly(assign_name)?;
                        let (assign_slot, var_slot) =
                            self.scope.global_slot_ptr_pair(assign_name, var);
                        if matches!(unsafe { &*assign_slot }, Value::Number(_)) {
                            let mut rhs_vars: Vec<&str> = Vec::new();
                            Self::collect_variable_refs(assign_value, &mut rhs_vars);
                            let cache_start = self.ctx.var_slot_cache.len();
                            for n in &rhs_vars {
                                // try_ variant — unbound names must error,
                                // not be created as Nil. See sibling site.
                                if let Some(p) = self.scope.try_global_slot_ptr(n) {
                                    self.ctx
                                        .var_slot_cache
                                        .push((*n as *const str, p as *const Value));
                                }
                            }
                            unsafe {
                                *var_slot = Value::Number(0.0);
                            }
                            let var_f64: *mut f64 = match unsafe { &mut *var_slot } {
                                Value::Number(n) => n as *mut f64,
                                _ => unreachable!(),
                            };
                            let assign_f64: *mut f64 = match unsafe { &mut *assign_slot } {
                                Value::Number(n) => n as *mut f64,
                                _ => unreachable!(),
                            };
                            let f64_cache_start = self.ctx.var_slot_cache_f64.len();
                            self.ctx
                                .var_slot_cache_f64
                                .push((var.as_str() as *const str, var_f64 as *const f64));
                            self.ctx
                                .var_slot_cache_f64
                                .push((assign_name as *const str, assign_f64 as *const f64));
                            let inner: MixResult<()> = (|| {
                                for item in items.iter() {
                                    let v = match item {
                                        Value::Number(n) => *n,
                                        _ => unreachable!("all_num verified"),
                                    };
                                    unsafe {
                                        *var_f64 = v;
                                    }
                                    let n = match self.try_eval_expr_num(assign_value) {
                                        Some(Ok(n)) => n,
                                        Some(Err(e)) => return Err(e),
                                        None => {
                                            // Body produced a non-numeric
                                            // value (Concat/Eq/bound non-
                                            // Number/unbound positional).
                                            // The take_fast path here exits
                                            // Ok(()) and the caller returns
                                            // Ok(Value::Nil) WITHOUT running
                                            // the generic loop — a latent
                                            // silent-no-body-execution shape
                                            // pre-dating the sync-fastpath
                                            // panic fix. Fully resolving it
                                            // requires either inner_kind
                                            // ("not applicable" vs "done")
                                            // or wiring a true fall-through
                                            // back to the generic loop.
                                            // Tracked for a focused refactor.
                                            return Ok(());
                                        }
                                    };
                                    unsafe {
                                        *assign_f64 = n;
                                    }
                                }
                                Ok(())
                            })();
                            self.ctx.var_slot_cache_f64.truncate(f64_cache_start);
                            self.ctx.var_slot_cache.truncate(cache_start);
                            inner?;
                            return Ok(Value::Nil);
                        }
                    }

                    let item_count = items.len();
                    for i in 0..item_count {
                        // Item extraction per source: a List snapshot hands
                        // out clones (O(1) Rc bumps / scalar copies under
                        // CoW — cost-equal to what the old deep-clone-at-
                        // entry paid per element); an Owned (String/Map)
                        // source moves items out via mem::take (no clone).
                        let item = match &mut src {
                            IterSrc::Snap(rc) => rc[i].clone(),
                            IterSrc::Owned(v) => std::mem::replace(&mut v[i], Value::Nil),
                        };
                        // Loop-var binding must respect function scope: the
                        // loop var shadows locally, it does NOT clobber a
                        // same-named caller/global var. (Mix has no
                        // per-iteration block scope — the same current-frame
                        // slot is reused each iteration and persists after
                        // the loop, as it always has.) Inside a function
                        // (`function_depth > 0`) bind in the current frame —
                        // mirroring `StmtKind::Assignment`
                        // — instead of `update_or_set`, whose globals-fallback
                        // would overwrite a same-named global (e.g. a lib
                        // function's `for each $p in …` trashing the caller's
                        // `$p`). The fast paths above are `!in_function`
                        // guarded, so this generic loop owns the in-function case.
                        if let Some(idx_var) = index_var {
                            // (key, value) for a two-variable MAP loop, else
                            // the position index. `map_pair_keys` is built
                            // from the same snapshot as `items`, so the two
                            // are index-aligned by construction.
                            let first = match &map_pair_keys {
                                Some(keys) => keys[i].clone(),
                                None => Value::Number(i as f64),
                            };
                            if in_function {
                                self.scope.set_in_current(idx_var.clone(), first);
                            } else {
                                self.scope.update_or_set(idx_var, first);
                            }
                        }
                        if in_function {
                            self.scope.set_in_current(var.clone(), item);
                        } else {
                            self.scope.update_or_set(var, item);
                        }
                        match self.execute_block(body).await {
                            Ok(_) => {}
                            Err(MixError::Break(None)) => break,
                            Err(MixError::Break(Some(ref l))) if label.as_deref() == Some(l) => {
                                break;
                            }
                            Err(MixError::Continue(None)) => {}
                            Err(MixError::Continue(Some(ref l))) if label.as_deref() == Some(l) => {
                            }
                            Err(e) => return Err(e),
                        }
                    }
                    Ok(Value::Nil)
                }

                StmtKind::While {
                    condition,
                    body,
                    label,
                } => {
                    loop {
                        let cond = self.eval_expr(condition).await?;
                        if !cond.is_truthy() {
                            break;
                        }
                        match self.execute_block(body).await {
                            Ok(_) => {}
                            Err(MixError::Break(None)) => break,
                            Err(MixError::Break(Some(ref l))) if label.as_deref() == Some(l) => {
                                break;
                            }
                            Err(MixError::Continue(None)) => {}
                            Err(MixError::Continue(Some(ref l))) if label.as_deref() == Some(l) => {
                            }
                            Err(e) => return Err(e),
                        }
                    }
                    Ok(Value::Nil)
                }

                StmtKind::Loop { body, label } => {
                    loop {
                        match self.execute_block(body).await {
                            Ok(_) => {}
                            Err(MixError::Break(None)) => break,
                            Err(MixError::Break(Some(ref l))) if label.as_deref() == Some(l) => {
                                break;
                            }
                            Err(MixError::Continue(None)) => {}
                            Err(MixError::Continue(Some(ref l))) if label.as_deref() == Some(l) => {
                            }
                            Err(e) => return Err(e),
                        }
                    }
                    Ok(Value::Nil)
                }

                StmtKind::Break(label) => Err(MixError::Break(label.clone())),
                StmtKind::Continue(label) => Err(MixError::Continue(label.clone())),
                StmtKind::BreakIf(cond, label) => {
                    let val = self.eval_expr(cond).await?;
                    if val.is_truthy() {
                        Err(MixError::Break(label.clone()))
                    } else {
                        Ok(Value::Nil)
                    }
                }
                StmtKind::ContinueIf(cond, label) => {
                    let val = self.eval_expr(cond).await?;
                    if val.is_truthy() {
                        Err(MixError::Continue(label.clone()))
                    } else {
                        Ok(Value::Nil)
                    }
                }

                StmtKind::FunctionDef { name, params, body } => {
                    // Loud diagnostic for the silent push-into-a-by-value-
                    // parameter footgun (dead mutation lost to the caller).
                    // Emitted HERE and not from the hoisting pre-pass
                    // (execute_inner), so a hoisted definition warns once,
                    // at the same point in the output as before hoisting
                    // existed.
                    self.warn_dead_param_pushes(name, params, body);
                    let f = self.build_function(name, params, body);
                    self.scope.define_function(f);
                    Ok(Value::Nil)
                }

                StmtKind::Return(expr) => {
                    let value = match expr {
                        Some(e) => self.eval_expr(e).await?,
                        None => Value::Nil,
                    };
                    Err(MixError::Return { value })
                }

                StmtKind::Select {
                    value,
                    cases,
                    otherwise,
                } => {
                    let val = self.eval_expr(value).await?;
                    for (case_expr, case_body) in cases {
                        let case_val = self.eval_expr(case_expr).await?;
                        if val == case_val {
                            return self.execute_block(case_body).await;
                        }
                    }
                    if let Some(ow) = otherwise {
                        return self.execute_block(ow).await;
                    }
                    Ok(Value::Nil)
                }

                StmtKind::Print { args, stderr } => {
                    let mut parts = Vec::new();
                    for a in args {
                        parts.push(self.eval_expr(a).await?.to_mix_string());
                    }
                    let output = parts.join(" ");
                    let mut g = self.globals.borrow_mut();
                    if *stderr {
                        writeln!(g.stderr, "{}", output).ok();
                    } else {
                        writeln!(g.stdout, "{}", output).ok();
                    }
                    Ok(Value::Nil)
                }

                StmtKind::Parse { source, parts } => {
                    let src = self.eval_expr(source).await?.to_mix_string();
                    self.exec_parse(&src, parts);
                    Ok(Value::Nil)
                }

                StmtKind::Die(expr) => {
                    let msg = self.eval_expr(expr).await?.to_mix_string();
                    Err(MixError::DieError { msg })
                }

                StmtKind::TryCatch {
                    try_body,
                    catch,
                    finally_body,
                } => {
                    // Run the try body, then the catch clause if the
                    // error is catchable — producing the "pending
                    // outcome" (Ok, a handled Ok/Err, or a propagating
                    // Err incl. Return/Break/Continue and an uncaught or
                    // catch-raised error).
                    let pending: MixResult<Value> = match self.execute_block(try_body).await {
                        Ok(v) => Ok(v),
                        // Catchable = DieError | RuntimeError | Structured.
                        // Control-flow signals (Return/Break/Continue) and
                        // lex/parse errors are NOT caught — but `finally`
                        // still runs for them (below).
                        Err(
                            e @ (MixError::DieError { .. }
                            | MixError::RuntimeError { .. }
                            | MixError::Structured(_)),
                        ) if catch.is_some() => {
                            let clause = catch.as_ref().unwrap();
                            // Snapshot BEFORE binding so the optional
                            // second binding sees code/frames even for
                            // errors that never crossed a function
                            // boundary. Idempotent for already-snapshotted.
                            let info = match self.snapshot_error(e) {
                                MixError::Structured(info) => info,
                                _ => unreachable!("snapshot_error contract"),
                            };
                            // `catch $e` is a binder, like loop vars and
                            // params: inside a function it must bind
                            // locally so it can't clobber a same-named
                            // caller/global via update_or_set's fallback.
                            self.bind_scoped(&clause.var, Value::String(info.message.clone()));
                            if let Some(err_var) = &clause.err_var {
                                self.bind_scoped(err_var, info.to_value());
                            }
                            self.execute_block(&clause.body).await
                        }
                        Err(e) => Err(e),
                    };

                    // `finally` runs on EVERY exit path above — normal
                    // completion, a caught error's handler result, and a
                    // propagating error/Return/Break/Continue/ExitRequest
                    // (D6). Only panic() and process-level termination such
                    // as SIGKILL bypass this evaluator unwind. A finally
                    // error OVERRIDES the pending outcome and records the
                    // displaced error (if any) as its `cause`; ExitRequest is
                    // control flow, not a structured error, so it is not a
                    // cause payload.
                    if let Some(fbody) = finally_body {
                        match self.execute_block(fbody).await {
                            Ok(_) => pending,
                            Err(fin_err) => {
                                let fin = match self.snapshot_error(fin_err) {
                                    MixError::Structured(info) => info,
                                    // Return/Break/Continue/ExitRequest from a
                                    // finally body legitimately hijack control.
                                    other => return Err(other),
                                };
                                // Attach the displaced error (if it was
                                // a real error, not Return/Break/Continue)
                                // as cause — snapshot it first so a raw
                                // Die/RuntimeError becomes structured.
                                let mut fin = fin;
                                if let Err(displaced) = pending
                                    && matches!(
                                        displaced,
                                        MixError::DieError { .. }
                                            | MixError::RuntimeError { .. }
                                            | MixError::Structured(_)
                                    )
                                    && let MixError::Structured(info) =
                                        self.snapshot_error(displaced)
                                {
                                    fin.cause = Some(info);
                                }
                                Err(MixError::Structured(fin))
                            }
                        }
                    } else {
                        pending
                    }
                }

                StmtKind::Export { name, value } => {
                    // `export` mutates process-global state (the
                    // environment every thread reads), so it is gated by
                    // the Process capability class like the other
                    // shell-syntax authority paths. No policy installed
                    // (CLI / login shell) → permitted as always.
                    self.check_capability_class(
                        crate::builtins::CapabilityClass::Process,
                        "export",
                    )?;
                    let val = self.eval_expr(value).await?;
                    let val_str = val.to_mix_string();
                    #[cfg(not(target_arch = "wasm32"))]
                    // SAFETY: set_var is UB if another thread touches the
                    // environment concurrently. The CLI / login shell runs
                    // Mix on the main thread with no concurrent getenv;
                    // an embedder that runs Mix on a worker thread in a
                    // multi-threaded process MUST install a capability
                    // policy denying Process, which makes this arm
                    // unreachable (clean Mix error above).
                    unsafe {
                        std::env::set_var(name, &val_str);
                    }
                    self.scope.update_or_set(name, Value::String(val_str));
                    Ok(Value::Nil)
                }

                StmtKind::Alias { name, command } => {
                    // Evaluate name/command Exprs at runtime so dynamic
                    // forms (`alias $h = "ssh " .. $h`) work alongside
                    // bare-identifier literals.
                    let name_str = if let Some(n) = name {
                        Some(self.eval_expr(n).await?.to_mix_string())
                    } else {
                        None
                    };
                    let command_str = if let Some(c) = command {
                        Some(self.eval_expr(c).await?.to_mix_string())
                    } else {
                        None
                    };
                    match (name_str, command_str) {
                        // `alias name = "expansion"` — define
                        (Some(n), Some(c)) => {
                            if n.is_empty() {
                                return Err(MixError::RuntimeError {
                                    span: None,
                                    msg: "alias: name evaluates to empty string".to_string(),
                                });
                            }
                            if c.is_empty() {
                                // An empty expansion would silently eat the
                                // first word at shell-mode expand time (see
                                // cosmix-mix::shell::expand_alias) — almost
                                // certainly a bug in the calling script.
                                return Err(MixError::RuntimeError {
                                    span: None,
                                    msg: format!(
                                        "alias: {}: expansion evaluates to empty string",
                                        n
                                    ),
                                });
                            }
                            self.globals.borrow_mut().aliases.insert(n, c);
                        }
                        // `alias name` — show one
                        (Some(n), None) => {
                            let mut g = self.globals.borrow_mut();
                            if let Some(expansion) = g.aliases.get(&n).cloned() {
                                writeln!(g.stdout, "{} = {}", n, expansion).ok();
                            } else {
                                writeln!(g.stderr, "alias: {}: not found", n).ok();
                            }
                        }
                        // `alias` — list all
                        (None, _) => {
                            // Snapshot the name/expansion pairs before grabbing
                            // the mut borrow of stdout so we don't hold a
                            // shared borrow on `aliases` across writes into
                            // the same RefCell.
                            let pairs: Vec<(String, String)> = {
                                let g = self.globals.borrow();
                                let mut names: Vec<&String> = g.aliases.keys().collect();
                                names.sort();
                                names
                                    .into_iter()
                                    .filter_map(|n| {
                                        g.aliases.get(n).map(|e| (n.clone(), e.clone()))
                                    })
                                    .collect()
                            };
                            let mut g = self.globals.borrow_mut();
                            for (name, expansion) in &pairs {
                                writeln!(g.stdout, "{} = {}", name, expansion).ok();
                            }
                        }
                    }
                    Ok(Value::Nil)
                }

                StmtKind::Send {
                    target,
                    command,
                    args,
                } => self.exec_send(target, command, args).await,

                StmtKind::Address { target, body } => {
                    let addr = self.eval_expr(target).await?.to_mix_string();
                    self.ctx.address_stack.push(addr);
                    let result = self.execute_block(body).await;
                    self.ctx.address_stack.pop();
                    result
                }

                StmtKind::Emit {
                    target,
                    command,
                    args,
                } => self.exec_emit(target, command, args).await,

                StmtKind::On {
                    command,
                    is_async,
                    body,
                } => {
                    // Register the handler body. Multiple `on <same-command>`
                    // statements append additional handlers; they fire in
                    // registration order at dispatch time.
                    //
                    // SPEC 18 Phase 2 WS2: each registered entry carries the
                    // parse-time `is_async` flag (WS1) so WS3-C can compute
                    // chain class as Class C if ANY matching handler is async,
                    // else Class S. Until WS3-C wires the LocalSet spawn,
                    // every chain still runs Class S inline regardless of the
                    // stored flag — the field is consumed at registration but
                    // not yet at dispatch.
                    let existing = self
                        .globals
                        .borrow()
                        .handlers
                        .get(command)
                        .map(|v| v.len())
                        .unwrap_or(0);
                    if existing > 0 {
                        // Surface the common bug: `on` inside a loop registers
                        // a fresh handler on every iteration. Parse-time is
                        // permissive (the pattern is occasionally legitimate
                        // for dynamic subscriptions), but runtime observability
                        // makes the surprising case visible in the journal.
                        tracing::debug!(
                            command = %command,
                            handler_index = existing + 1,
                            line = self.ctx.current_line,
                            "additional `on` handler registered for command"
                        );
                    }
                    let entry = HandlerEntry {
                        body: Arc::new(body.clone()),
                        is_async: *is_async,
                    };
                    if entry.is_async {
                        // Surface Class C registration in the journal so
                        // operators can see at a glance which handlers will
                        // (once WS3-C lands) participate in cooperative
                        // interleaving. Plain Class S registrations stay
                        // silent (today's tracing baseline) — emitting a
                        // line per handler register would be noise for the
                        // 99% case.
                        tracing::debug!(
                            command = %command,
                            handler_index = existing + 1,
                            line = self.ctx.current_line,
                            class = "async",
                            "registered Class C handler (SPEC 18 Phase 2)"
                        );
                    }
                    self.globals
                        .borrow_mut()
                        .handlers
                        .entry(command.clone())
                        .or_default()
                        .push(entry);
                    Ok(Value::Nil)
                }

                StmtKind::Source { path } => {
                    let path_str = self.eval_expr(path).await?.to_mix_string();
                    self.exec_source(&path_str).await
                }

                StmtKind::Include { path } => {
                    let path_str = self.eval_expr(path).await?.to_mix_string();
                    self.exec_include(&path_str).await
                }

                StmtKind::Sh { command } => {
                    let cmd = self.eval_expr(command).await?.to_mix_string();
                    self.exec_sh_stmt(&cmd)
                }

                StmtKind::PipeToExternal { stmt, command } => {
                    self.exec_pipe_to_external(stmt, command).await
                }

                StmtKind::Chain { left, op, right } => {
                    let left_val = self.exec_stmt(left).await?;
                    // $rc gates the chain. Absent means SUCCESS — pure Mix
                    // statements like `print` never set $rc, so a cold
                    // `print(...) && next` could not run otherwise. A finite
                    // whole Number compares against 0.0 directly (the old
                    // `as i64` cast read NaN as 0 = success). Anything else
                    // is a corrupted $rc and RAISES: coercing corruption to
                    // an arbitrary verdict leaves one of `&&`/`||` running
                    // its right side off garbage — raising stops both.
                    // Numeric strings and bools count as corruption — the
                    // documented invariant is that $rc is a number.
                    //
                    // get_value, NOT get: `Scope::get` is standalone-only
                    // for globals — inside a shared-scope handler
                    // activation (--serve Class S/C) it cannot see the
                    // shared global frame, so a real `$rc = 1` would read
                    // as ABSENT = success and `&&` would run its right
                    // side. The old gate had the same blind spot; this is
                    // the one reader where it gates a destructive action.
                    let rc_val = self.scope.get_value("rc");
                    let success = match rc_val.as_deref() {
                        None => true,
                        Some(Value::Number(n)) if n.is_finite() && n.fract() == 0.0 => *n == 0.0,
                        Some(Value::Number(n)) => {
                            return Err(self.attach_span(MixError::structured(
                                "VALUE_OUT_OF_RANGE",
                                format!(
                                    "chain condition: $rc must be a finite whole number, got {n}"
                                ),
                            )));
                        }
                        Some(other) => {
                            return Err(self.attach_span(MixError::structured(
                                "TYPE_MISMATCH",
                                format!(
                                    "chain condition: $rc must be a number, got {} ({})",
                                    other.to_mix_string(),
                                    other.type_name()
                                ),
                            )));
                        }
                    };
                    match op {
                        ChainOp::And => {
                            if success {
                                self.exec_stmt(right).await
                            } else {
                                Ok(left_val)
                            }
                        }
                        ChainOp::Or => {
                            if !success {
                                self.exec_stmt(right).await
                            } else {
                                Ok(left_val)
                            }
                        }
                    }
                }
            }
        })
    }

    /// Synchronous fast path for the common `[Assignment]` body in
    /// hot for-loops: avoids the Box::pin'd future allocation per
    /// iteration. Returns `Some(Ok(v))` on success, `Some(Err(e))` on
    /// runtime error, and `None` when the expression contains anything
    /// the sync path can't handle (function calls, short-circuit ops,
    /// strings/lists/maps that need allocation handling, etc.) — caller
    /// then falls back to the async `eval_expr`.
    fn try_eval_expr_sync(&mut self, expr: &Expr) -> Option<MixResult<Value>> {
        match expr {
            Expr::NumberLiteral(n) => Some(Ok(Value::Number(*n))),
            Expr::StringLiteral(s) | Expr::EscapedQuoteStringLiteral(s) => {
                Some(Ok(Value::String(s.clone())))
            }
            Expr::BoolLiteral(b) => Some(Ok(Value::Bool(*b))),
            Expr::NilLiteral => Some(Ok(Value::Nil)),
            // Sync interpolation: only Literal/Variable parts. Mirrors
            // the eval_expr arm but stays inside the sync evaluator so
            // For-loop fast paths can drive `"k${i}"`-style index keys
            // without Box::pin per iter (see the [IndexAssignment] fast
            // path in StmtKind::For).
            Expr::InterpolatedString(parts) | Expr::Heredoc(parts) => {
                let mut result = String::new();
                for part in parts {
                    match part {
                        StringPart::Literal(s) => result.push_str(s),
                        StringPart::Variable(name) => {
                            let (path, coalesce) = split_interp_coalesce(name);
                            // A coalescing default (`?? / ?:`) may need to parse
                            // and evaluate an expression — that lives on the
                            // async path, so defer the whole interpolation.
                            if coalesce.is_some() {
                                return None;
                            }
                            let mut parts_iter = path.split('.');
                            let head = parts_iter.next().unwrap_or("");
                            // Variable read walks the same caches as
                            // Expr::Variable to pick up var_slot_cache_f64
                            // (loop counters) without a HashMap probe.
                            let mut found: Option<Value> = None;
                            if !self.ctx.var_slot_cache_f64.is_empty() {
                                for (key_ptr, n_ptr) in self.ctx.var_slot_cache_f64.iter().rev() {
                                    let key_str: &str = unsafe { &**key_ptr };
                                    if key_str == head {
                                        found = Some(Value::Number(unsafe { *(*n_ptr) }));
                                        break;
                                    }
                                }
                            }
                            if found.is_none() && !self.ctx.var_slot_cache.is_empty() {
                                for (key_ptr, val_ptr) in self.ctx.var_slot_cache.iter().rev() {
                                    let key_str: &str = unsafe { &**key_ptr };
                                    if key_str == head {
                                        found = Some(unsafe { (**val_ptr).clone() });
                                        break;
                                    }
                                }
                            }
                            // SPEC 18 Phase 2 WS3-C.7b — γ-aware read (shared
                            // global frame) with env fallback. (An explicit nil
                            // VALUE is bound, so it stays here and renders
                            // "nil".)
                            //
                            // A head unbound in BOTH scope and env RAISES the
                            // same uniform error the async path raises. It used
                            // to return None ("defer to async") — but a caller
                            // that reached here because `is_expr_sync` classified
                            // the expression as sync reads that None as
                            // classifier/evaluator DRIFT and raises "internal
                            // evaluator error … please report this Mix bug". So
                            // `$s = "${UNBOUND}"` told the user their ordinary
                            // typo was an interpreter bug (reproduced on 0.32.3).
                            // Answering with the real error keeps classifier and
                            // evaluator in agreement.
                            let resolved: Option<Value> = found.or_else(|| {
                                self.scope
                                    .get_value(head)
                                    .map(|v| v.into_value())
                                    .or_else(|| std::env::var(head).ok().map(Value::String))
                            });
                            let mut val: Value = match resolved {
                                Some(v) => v,
                                None => {
                                    return Some(Err(MixError::RuntimeError {
                                        span: None,
                                        msg: format!(
                                            "undefined variable '${head}' in interpolation \
                                             (use ${{{head} ?? default}} for a fallback)"
                                        ),
                                    }));
                                }
                            };
                            for field in parts_iter {
                                val = match &val {
                                    Value::Map(m) => m
                                        .get(field)
                                        .or_else(|| m.get("*"))
                                        .cloned()
                                        .unwrap_or(Value::Nil),
                                    _ => Value::Nil,
                                };
                            }
                            val.write_mix(&mut result);
                        }
                        StringPart::CommandSub(_) => return None,
                        StringPart::EnvVar(name) => {
                            result.push_str(&std::env::var(name).unwrap_or_default());
                        }
                    }
                }
                // Knob D: cap the interpolated string (sync path).
                let v = Value::String(result);
                match self.check_collection_size(&v) {
                    Ok(()) => Some(Ok(v)),
                    Err(e) => Some(Err(e)),
                }
            }
            Expr::Variable(name) => {
                // Fast path: callers that pre-bind variable slots
                // (For-loop top-level fast path, `try_call_function_
                // sync_block` for `body_no_local_assigns` functions)
                // skip a HashMap probe here. Walked top-down (rev) so
                // the innermost binding wins on shadowing — fib's
                // recursive `$n` resolves to the current call's slot,
                // not an outer call's.
                let needle = name.as_str();
                if !self.ctx.var_slot_cache_f64.is_empty() {
                    for (key_ptr, n_ptr) in self.ctx.var_slot_cache_f64.iter().rev() {
                        let key_str: &str = unsafe { &**key_ptr };
                        if key_str == needle {
                            return Some(Ok(Value::Number(unsafe { *(*n_ptr) })));
                        }
                    }
                }
                if !self.ctx.var_slot_cache.is_empty() {
                    for (key_ptr, val_ptr) in self.ctx.var_slot_cache.iter().rev() {
                        // SAFETY: `key_ptr` was sourced from a stable
                        // owner (AST or `MixFunction::params`); the
                        // contract guarantees it outlives the cache
                        // entry. `val_ptr` likewise comes from a frame
                        // whose bucket array won't relocate during the
                        // cache's lifetime.
                        let key_str: &str = unsafe { &**key_ptr };
                        if key_str == needle {
                            return Some(Ok(unsafe { (**val_ptr).clone() }));
                        }
                    }
                }
                // `Expr::Variable` is `is_expr_sync == true` regardless
                // of whether the name is bound, so this arm must return a
                // `Some` (the *precise* error) even when the variable is
                // undefined. Mirror the async `eval_expr` arm's semantics:
                // positional args ($1, $2, …) read as nil; other unbound
                // names raise a RuntimeError. Returning `None` here would
                // signal "couldn't evaluate sync" — which `eval_sync_verified`
                // now downgrades to a generic classifier-drift error rather
                // than the panic this used to cause; keep the `Some(Err(..))`
                // so callers still see "undefined variable", not the drift
                // fallback.
                // SPEC 18 Phase 2 WS3-C.7b — γ-aware fallback after the
                // pointer caches miss. `get_value` walks the local
                // frames first, then the shared global frame; legacy
                // `get` returned None for promoted scopes, which the
                // arm below would have mis-reported as undefined.
                match self.scope.get_value(name) {
                    Some(v) => Some(Ok(v.into_value())),
                    None => {
                        if name.chars().all(|c| c.is_ascii_digit()) {
                            Some(Ok(Value::Nil))
                        } else {
                            Some(Err(MixError::structured(
                                "NAME_UNDEFINED",
                                format!("undefined variable '${}'", name),
                            )))
                        }
                    }
                }
            }
            Expr::BinaryOp { left, op, right } => {
                if matches!(
                    op,
                    BinOp::And | BinOp::Or | BinOp::NilCoalesce | BinOp::Pipe
                ) {
                    return None;
                }
                let mut lval = match self.try_eval_expr_sync(left)? {
                    Ok(v) => v,
                    Err(e) => return Some(Err(e)),
                };
                let rval = match self.try_eval_expr_sync(right)? {
                    Ok(v) => v,
                    Err(e) => return Some(Err(e)),
                };
                Some(self.eval_binop(op, &mut lval, &rval))
            }
            // Map/List/String indexing inside the sync evaluator. When
            // `object` is a Variable, resolve it through the slot
            // pointer chain so the container itself isn't cloned —
            // critical for the table bench's `$total = $total + $t[$k]`
            // body, which would otherwise deep-clone the 6000-entry
            // IndexMap each iter.
            Expr::Index { object, index } => {
                let idx_val = match self.try_eval_expr_sync(index)? {
                    Ok(v) => v,
                    Err(e) => return Some(Err(e)),
                };
                if let Expr::Variable(name) = &**object {
                    let needle = name.as_str();
                    if !self.ctx.var_slot_cache.is_empty() {
                        for (key_ptr, val_ptr) in self.ctx.var_slot_cache.iter().rev() {
                            let key_str: &str = unsafe { &**key_ptr };
                            if key_str == needle {
                                let obj_ref = unsafe { &**val_ptr };
                                return Some(Self::index_value_clone(obj_ref, &idx_val));
                            }
                        }
                    }
                    // Same contract as the Variable arm above: callers
                    // require `Some` for is_expr_sync-true shapes.
                    // Unbound positional → index nil (which itself
                    // errors as "cannot index nil with <T>", matching
                    // async); other unbound → RuntimeError.
                    // SPEC 18 Phase 2 WS3-C.7b — γ-aware fallback. The
                    // `index_value_clone` helper takes a `&Value`, so we
                    // deref the `ScopeValue<'_>` (Borrowed or Owned)
                    // through its `Deref` impl rather than materializing
                    // the Owned arm into a clone.
                    return match self.scope.get_value(name) {
                        Some(obj_ref) => Some(Self::index_value_clone(&obj_ref, &idx_val)),
                        None => {
                            if name.chars().all(|c| c.is_ascii_digit()) {
                                Some(Self::index_value_clone(&Value::Nil, &idx_val))
                            } else {
                                Some(Err(MixError::structured(
                                    "NAME_UNDEFINED",
                                    format!("undefined variable '${}'", name),
                                )))
                            }
                        }
                    };
                }
                let obj_val = match self.try_eval_expr_sync(object)? {
                    Ok(v) => v,
                    Err(e) => return Some(Err(e)),
                };
                Some(Self::index_value_clone(&obj_val, &idx_val))
            }
            // Self-recursion: when we're already inside the sync-block
            // fast path for a function whose body recursively calls
            // itself, we can keep the recursion sync (no Box::pin per
            // call). `block_sync_self` was statically verified, so the
            // arity match is guaranteed; the call is dispatched
            // straight to `try_call_function_sync_block` which is just
            // a regular Rust function — no async allocs.
            //
            // 0/1/2-arg arities use stack arrays to avoid the per-call
            // `Vec::with_capacity` heap alloc — fib's recursive
            // `fib($n-1)`/`fib($n-2)` arrives here with arity 1.
            Expr::FunctionCall { name, args } => {
                let self_func = match &self.ctx.sync_self_func {
                    Some(f) if f.name.as_ref() == name.as_str() => f.clone(),
                    _ => return None,
                };
                if args.len() != self_func.params.len() {
                    return None;
                }
                match args.len() {
                    0 => self.try_call_function_sync_block(&self_func, &[]),
                    1 => {
                        let a0 = self.eval_arg_value(&args[0])?;
                        let a0 = match a0 {
                            Ok(v) => v,
                            Err(e) => return Some(Err(e)),
                        };
                        let stack = [a0];
                        self.try_call_function_sync_block(&self_func, &stack)
                    }
                    2 => {
                        let a0 = self.eval_arg_value(&args[0])?;
                        let a0 = match a0 {
                            Ok(v) => v,
                            Err(e) => return Some(Err(e)),
                        };
                        let a1 = self.eval_arg_value(&args[1])?;
                        let a1 = match a1 {
                            Ok(v) => v,
                            Err(e) => return Some(Err(e)),
                        };
                        let stack = [a0, a1];
                        self.try_call_function_sync_block(&self_func, &stack)
                    }
                    _ => {
                        let mut eval_args = Vec::with_capacity(args.len());
                        for arg in args {
                            let v = self.eval_arg_value(arg)?;
                            match v {
                                Ok(v) => eval_args.push(v),
                                Err(e) => return Some(Err(e)),
                            }
                        }
                        self.try_call_function_sync_block(&self_func, &eval_args)
                    }
                }
            }
            // Ternary on the sync fast path — short-circuit to the taken
            // branch. Reached only when `is_expr_sync` classified the whole
            // ternary sync (all three sub-exprs sync), so the taken branch
            // is guaranteed `Some` — preserving the no-drift invariant that
            // `eval_sync_verified` relies on. (`Expr::If` is intentionally
            // NOT handled here: `is_expr_sync` returns false for it, so it
            // always takes the async path — `None` here stays consistent.)
            Expr::Ternary {
                cond,
                then_branch,
                else_branch,
            } => {
                let c = match self.try_eval_expr_sync(cond)? {
                    Ok(v) => v,
                    Err(e) => return Some(Err(e)),
                };
                if c.is_truthy() {
                    self.try_eval_expr_sync(then_branch)
                } else {
                    self.try_eval_expr_sync(else_branch)
                }
            }
            _ => None,
        }
    }

    /// Evaluate a single argument expression, preferring the numeric
    /// path. fib's recursive args (`$n-1`, `$n-2`) are pure numeric
    /// arithmetic — the numeric path skips the intermediate
    /// `Value::Number` construction at every leaf, then we wrap once
    /// at the end. Falls back to the generic `try_eval_expr_sync`
    /// for non-numeric or non-sync arg shapes.
    #[inline]
    fn eval_arg_value(&mut self, arg: &Expr) -> Option<MixResult<Value>> {
        if let Some(r) = self.try_eval_expr_num(arg) {
            return Some(match r {
                Ok(n) => Ok(Value::Number(n)),
                Err(e) => Err(e),
            });
        }
        self.try_eval_expr_sync(arg)
    }

    /// Walk a sync-eligible expression and append every distinct
    /// `Variable` name encountered to `out` as a borrow into the AST.
    /// Used by the For-loop top-level fast path to pre-bind RHS
    /// variable slots so reads inside the 1M-iter hot loop skip the
    /// HashMap probe. Borrowing avoids the `String::clone` per slot
    /// that an owned `Vec<String>` would pay.
    fn collect_variable_refs<'a>(expr: &'a Expr, out: &mut Vec<&'a str>) {
        match expr {
            Expr::Variable(name) => {
                if !out.contains(&name.as_str()) {
                    out.push(name.as_str());
                }
            }
            Expr::BinaryOp { left, right, .. } => {
                Self::collect_variable_refs(left, out);
                Self::collect_variable_refs(right, out);
            }
            Expr::Ternary {
                cond,
                then_branch,
                else_branch,
            } => {
                Self::collect_variable_refs(cond, out);
                Self::collect_variable_refs(then_branch, out);
                Self::collect_variable_refs(else_branch, out);
            }
            Expr::InterpolatedString(parts) | Expr::Heredoc(parts) => {
                for part in parts {
                    if let StringPart::Variable(name) = part {
                        // Only the dotted-path head matters for slot
                        // caching; nested field accesses go through
                        // Map lookups, not slot reads. Strip any `?? / ?:`
                        // default first so the head is the bare name.
                        let head = interp_head(name);
                        if !head.is_empty() && !out.contains(&head) {
                            out.push(head);
                        }
                    }
                }
            }
            _ => {}
        }
    }

    /// Numeric-typed sync evaluator: returns `f64` directly when the
    /// expression is a pure-arithmetic combination of Number literals
    /// and Number-valued variables. Skips the full `Value` clone (32+
    /// bytes per leaf) that the generic `try_eval_expr_sync` pays.
    /// Used inside the For-loop top-level fast path: loop_arith's
    /// `$sum + $i * 2 - 1` does 6 leaf clones × 1M iters in the
    /// generic path; this collapses each leaf to a single f64 read.
    ///
    /// Returns `None` if any leaf is non-Number or any op is not in
    /// the numeric subset. Caller falls back to the Value-based path.
    /// Single-element clone for sync Index access — mirrors the async
    /// `eval_expr` arm closely enough for the bench paths that use it.
    fn index_value_clone(obj: &Value, idx: &Value) -> MixResult<Value> {
        match (obj, idx) {
            (Value::List(items), Value::Number(n)) => {
                match crate::builtins::resolve_signed_index(*n as i64, items.len()) {
                    Some(i) => Ok(items.get(i).cloned().unwrap_or(Value::Nil)),
                    None => Ok(Value::Nil),
                }
            }
            (Value::Map(map), Value::String(s)) => Ok(map
                .get(s.as_str())
                .or_else(|| map.get("*"))
                .cloned()
                .unwrap_or(Value::Nil)),
            (Value::Map(map), other) => {
                let key = other.to_mix_string();
                Ok(map
                    .get(&key)
                    .or_else(|| map.get("*"))
                    .cloned()
                    .unwrap_or(Value::Nil))
            }
            (Value::String(s), Value::Number(n)) => {
                let chars: Vec<char> = s.chars().collect();
                match crate::builtins::resolve_signed_index(*n as i64, chars.len()) {
                    Some(i) => Ok(Value::String(chars[i].to_string())),
                    None => Ok(Value::Nil),
                }
            }
            // `$b[i]` on bytes/buffer yields the BYTE as a number 0-255
            // (v0.64.0). Same signed-index and out-of-range rules as a
            // list: `-1` is the last byte, out of range is `nil` (NOT a
            // raise — that is what every other indexable value does).
            //
            // This is deliberately NOT `buffer_get`'s contract. The two
            // agree on an in-range non-negative integer, and both answer
            // nil past the end, but `buffer_get` RAISES on a negative or
            // fractional index where indexing resolves/truncates it. That
            // is the right split: an accessor paired with `buffer_set`
            // must refuse a nonsense index, while `$x[i]` has to read the
            // same way on every indexable value. Pinned by
            // tests/scripts/bytes_ops.mix so it can't drift silently.
            (Value::Bytes(b), Value::Number(n)) => {
                match crate::builtins::resolve_signed_index(*n as i64, b.len()) {
                    Some(i) => Ok(Value::Number(b[i] as f64)),
                    None => Ok(Value::Nil),
                }
            }
            (Value::Buffer(b), Value::Number(n)) => {
                // Borrow scoped to the read: never held across an eval,
                // so a script cannot deadlock its own buffer.
                let buf = b.borrow();
                match crate::builtins::resolve_signed_index(*n as i64, buf.len()) {
                    Some(i) => Ok(Value::Number(buf[i] as f64)),
                    None => Ok(Value::Nil),
                }
            }
            // Mirror the async Index arm's `(other, _) => Err(...)`
            // fallthrough — `cannot index <T> with <U>` rather than a
            // silent Nil. Without this, sync paths diverged from the
            // canonical error semantics for non-indexable values.
            _ => Err(MixError::RuntimeError {
                span: None,
                msg: format!("cannot index {} with {}", obj.type_name(), idx.type_name()),
            }),
        }
    }

    fn try_eval_expr_num(&mut self, expr: &Expr) -> Option<MixResult<f64>> {
        match expr {
            Expr::NumberLiteral(n) => Some(Ok(*n)),
            Expr::Variable(name) => {
                let needle = name.as_str();
                // f64 cache: numeric self-recursion path stores arg
                // values directly as f64, skipping the Value::Number
                // wrap-then-unwrap pair. Walked first because for fib
                // every Variable read of $n hits here (each call pushes
                // one f64 entry, the most-recent one is the answer).
                // For non-numeric code this cache is empty and the
                // is_empty() check fast-exits.
                if !self.ctx.var_slot_cache_f64.is_empty() {
                    for (key_ptr, n_ptr) in self.ctx.var_slot_cache_f64.iter().rev() {
                        let key_str: &str = unsafe { &**key_ptr };
                        if key_str == needle {
                            return Some(Ok(unsafe { *(*n_ptr) }));
                        }
                    }
                }
                if !self.ctx.var_slot_cache.is_empty() {
                    for (key_ptr, val_ptr) in self.ctx.var_slot_cache.iter().rev() {
                        let key_str: &str = unsafe { &**key_ptr };
                        if key_str == needle {
                            return match unsafe { &**val_ptr } {
                                Value::Number(n) => Some(Ok(*n)),
                                _ => None,
                            };
                        }
                    }
                }
                // Mirror `try_eval_expr_sync`'s Variable arm: unbound
                // non-positional names must surface the canonical
                // "undefined variable" RuntimeError. Returning None
                // would otherwise propagate up as a silent fall-through
                // (e.g. the take_fast inner closure exits as `Ok(())`,
                // and the outer loop returns `Ok(Value::Nil)` without
                // ever executing the body).
                // SPEC 18 Phase 2 WS3-C.7b — γ-aware fallback. Match
                // through `Deref<Target=Value>` so the Borrowed arm
                // never materializes an Owned clone for the common
                // non-numeric mismatch case (returns None to defer to
                // the async path).
                match self.scope.get_value(name) {
                    Some(v) => match &*v {
                        Value::Number(n) => Some(Ok(*n)),
                        _ => None,
                    },
                    None => {
                        if name.chars().all(|c| c.is_ascii_digit()) {
                            None
                        } else {
                            Some(Err(MixError::structured(
                                "NAME_UNDEFINED",
                                format!("undefined variable '${}'", name),
                            )))
                        }
                    }
                }
            }
            // Map/List indexing returning Number — required for the
            // table bench's `$total + $t[$k]` to drive entirely through
            // try_eval_expr_num.
            Expr::Index { .. } => match self.try_eval_expr_sync(expr)? {
                Ok(Value::Number(n)) => Some(Ok(n)),
                Ok(_) => None,
                Err(e) => Some(Err(e)),
            },
            Expr::BinaryOp { left, op, right } => {
                let l = match self.try_eval_expr_num(left)? {
                    Ok(v) => v,
                    Err(e) => return Some(Err(e)),
                };
                let r = match self.try_eval_expr_num(right)? {
                    Ok(v) => v,
                    Err(e) => return Some(Err(e)),
                };
                match op {
                    BinOp::Add => Some(Ok(l + r)),
                    BinOp::Sub => Some(Ok(l - r)),
                    BinOp::Mul => Some(Ok(l * r)),
                    BinOp::Div => {
                        if r == 0.0 {
                            Some(Err(MixError::RuntimeError {
                                span: None,
                                msg: "division by zero".to_string(),
                            }))
                        } else {
                            Some(Ok(l / r))
                        }
                    }
                    BinOp::Mod => {
                        if r == 0.0 {
                            Some(Err(MixError::RuntimeError {
                                span: None,
                                msg: "modulo by zero".to_string(),
                            }))
                        } else {
                            Some(Ok(l % r))
                        }
                    }
                    BinOp::Power => Some(Ok(l.powf(r))),
                    _ => None,
                }
            }
            // Self-recursion at numeric type. fib's recursive
            // `fib($n-1) + fib($n-2)` calls reach here through the
            // outer BinaryOp(Add) — each leg can return f64 directly,
            // skipping the Value::Number wrap-then-unwrap that
            // try_eval_expr_sync forces. Wrapping happens only at the
            // outermost boundary (e.g. eval_arg_value).
            Expr::FunctionCall { name, args } => {
                let self_func = match &self.ctx.sync_self_func {
                    Some(f) if f.name.as_ref() == name.as_str() => f.clone(),
                    _ => return None,
                };
                if args.len() != self_func.params.len() {
                    return None;
                }
                // Numeric f64 fast path: when the callee is frameless
                // (body_no_local_assigns) and every arg evaluates to f64,
                // push args as f64 directly into var_slot_cache_f64 and
                // skip the Value::Number wrap-then-unwrap that the
                // generic eval_arg_value/try_call_function_sync_block
                // pair forces. fib's ~285k recursive calls each save
                // two 72-byte Value writes + two variant-tag matches.
                if self_func.block_sync_self
                    && !self_func.needs_frame_injection()
                    && self_func.body_no_local_assigns
                    && args.len() <= 2
                {
                    let n0 = if !args.is_empty() {
                        match self.try_eval_expr_num(&args[0])? {
                            Ok(v) => Some(v),
                            Err(e) => return Some(Err(e)),
                        }
                    } else {
                        None
                    };
                    let n1 = if args.len() == 2 {
                        match self.try_eval_expr_num(&args[1])? {
                            Ok(v) => Some(v),
                            Err(e) => return Some(Err(e)),
                        }
                    } else {
                        None
                    };
                    let stack: [f64; 2] = [n0.unwrap_or(0.0), n1.unwrap_or(0.0)];
                    // Bytecode fast path: when the function compiled to
                    // fib-style bytecode, every recursive call (the vast
                    // majority for fib) routes through `eval_fib_program`
                    // — a flat opcode loop that skips the per-call var_
                    // slot_cache push/truncate, sync_self_func swap/
                    // restore, function_depth ±, and AST shape match
                    // that the AST path pays. Sub-calls are pure Rust
                    // recursion through `eval_fib_program`'s `CallSelf`
                    // opcode (no scope/Rc traffic at all).
                    if let Some(prog) = &self_func.fib_bytecode {
                        // Seed the fib VM with the evaluator's live recursion
                        // budget + deadline (knobs B/C) so the bytecode path
                        // shares the same limits as the AST path.
                        let mut fib_ops: u64 = 0;
                        return Some(eval_fib_program(
                            prog,
                            &stack[..args.len()],
                            self.ctx.function_depth,
                            self.ctx.recursion_limit,
                            self.ctx.deadline,
                            &mut fib_ops,
                        ));
                    }
                    return self.try_call_function_sync_block_f64(&self_func, &stack[..args.len()]);
                }
                let result = match args.len() {
                    0 => self.try_call_function_sync_block(&self_func, &[]),
                    1 => {
                        let a0 = self.eval_arg_value(&args[0])?;
                        let a0 = match a0 {
                            Ok(v) => v,
                            Err(e) => return Some(Err(e)),
                        };
                        let stack = [a0];
                        self.try_call_function_sync_block(&self_func, &stack)
                    }
                    2 => {
                        let a0 = self.eval_arg_value(&args[0])?;
                        let a0 = match a0 {
                            Ok(v) => v,
                            Err(e) => return Some(Err(e)),
                        };
                        let a1 = self.eval_arg_value(&args[1])?;
                        let a1 = match a1 {
                            Ok(v) => v,
                            Err(e) => return Some(Err(e)),
                        };
                        let stack = [a0, a1];
                        self.try_call_function_sync_block(&self_func, &stack)
                    }
                    _ => {
                        let mut eval_args = Vec::with_capacity(args.len());
                        for arg in args {
                            let v = self.eval_arg_value(arg)?;
                            match v {
                                Ok(v) => eval_args.push(v),
                                Err(e) => return Some(Err(e)),
                            }
                        }
                        self.try_call_function_sync_block(&self_func, &eval_args)
                    }
                }?;
                match result {
                    Ok(Value::Number(n)) => Some(Ok(n)),
                    Ok(_) => None,
                    Err(e) => Some(Err(e)),
                }
            }
            _ => None,
        }
    }

    /// Bool-typed sync evaluator: handles `BinaryOp(<cmp>, num, num)`
    /// with operands resolved through `try_eval_expr_num`. Filter's
    /// hot lambda `function($x) = $x % 2 == 0` lands here from
    /// `try_call_function_sync`'s all_numeric path — without this the
    /// numeric path's `try_eval_expr_num` rejects on `Eq` and the
    /// caller has to redo the work via `try_eval_expr_sync` (Value-
    /// typed clone-and-match dispatch). 8000 filter calls × redundant
    /// retry add up.
    fn try_eval_expr_bool(&mut self, expr: &Expr) -> Option<MixResult<bool>> {
        match expr {
            Expr::BoolLiteral(b) => Some(Ok(*b)),
            Expr::BinaryOp { left, op, right } => match op {
                BinOp::Eq | BinOp::NotEq | BinOp::Lt | BinOp::Gt | BinOp::LtEq | BinOp::GtEq => {
                    let l = match self.try_eval_expr_num(left)? {
                        Ok(v) => v,
                        Err(e) => return Some(Err(e)),
                    };
                    let r = match self.try_eval_expr_num(right)? {
                        Ok(v) => v,
                        Err(e) => return Some(Err(e)),
                    };
                    Some(Ok(match op {
                        BinOp::Eq => l == r,
                        BinOp::NotEq => l != r,
                        BinOp::Lt => l < r,
                        BinOp::Gt => l > r,
                        BinOp::LtEq => l <= r,
                        BinOp::GtEq => l >= r,
                        _ => unreachable!(),
                    }))
                }
                _ => None,
            },
            _ => None,
        }
    }

    /// Static check: does this expression's evaluation never need to
    /// await? Pure subset only — no function calls, no await-emitting
    /// constructs. Used to guard the sync-prefix path in `execute`.
    /// The `push` self-assign fast path: `$m[$k] = push($m[$k], $v)`.
    ///
    /// `push` reaches a list through a bare variable SLOT. Given any other
    /// first argument it appends to a copy and returns it — which is why
    /// the assign-it-back idiom is the correct spelling, and also why it
    /// was O(N²): reading `$m[$k]` bumps the refcount, so `push` deep-copied
    /// the growing list on every iteration. This walks the path with
    /// `Rc::make_mut` and appends in place instead.
    ///
    /// Fires ONLY when:
    /// - the RHS is exactly `push(<lvalue>, <value>)`;
    /// - that lvalue is structurally IDENTICAL to the assignment target
    ///   (same root, same segment kinds — `.k` never matches `["k"]`);
    /// - every index expression and the pushed value is `is_expr_sync`, so
    ///   nothing can rebind the root or run a side effect between the
    ///   conceptual read and the write;
    /// - the target already resolves to a LIST via exact-key lookup.
    ///
    /// Anything else returns `Ok(None)` and takes the generic path, which
    /// stays correct (just copying). Deliberately NOT extended to
    /// `pop`/`shift`: they return the removed ELEMENT, so `$p = pop($p)`
    /// legitimately replaces the list with that element — "mutate and keep
    /// the list" would be a different, wrong, meaning.
    async fn try_push_self_assign(
        &mut self,
        root: &str,
        lhs: &[LvSeg<'_>],
        value: &Expr,
    ) -> MixResult<Option<Value>> {
        // Cheap discriminant gate first: ordinary assignments pay one
        // enum check and nothing else.
        let Expr::FunctionCall { name, args } = value else {
            return Ok(None);
        };
        if name != "push" || args.len() != 2 {
            return Ok(None);
        }
        let Some((rhs_root, rhs_segs)) = expr_as_lvalue(&args[0]) else {
            return Ok(None);
        };
        if rhs_root != root
            || rhs_segs.len() != lhs.len()
            || !lhs
                .iter()
                .zip(rhs_segs.iter())
                .all(|(a, b)| lv_seg_eq(*a, *b))
        {
            return Ok(None);
        }
        // Purity: an index expression or a pushed value that could call
        // out (and rebind the root, or run a side effect a second time)
        // stays on the generic path.
        let all_sync = lhs.iter().all(|s| match s {
            LvSeg::Field(_) => true,
            LvSeg::Index(e) => Self::is_expr_sync(e),
        }) && Self::is_expr_sync(&args[1]);
        if !all_sync {
            return Ok(None);
        }

        // Knob D: with ANY collection limit active, decline. The generic
        // path validates the COMPLETE resulting list recursively — every
        // existing element, at every depth — which is inherently O(n) per
        // push. A linear append and that guarantee are mutually exclusive,
        // so when an embedder has asked for the guarantee it wins, and the
        // append stays generic (correct, quadratic). Checking only the
        // appended value would let an oversized element that the host
        // injected via `set_global` (which does not validate) survive a
        // push the generic spelling rejects — two spellings, two answers.
        // Unlimited is the default, and the mesh/CLI case this exists for.
        if self.ctx.max_list_len.is_some()
            || self.ctx.max_map_len.is_some()
            || self.ctx.max_string_len.is_some()
        {
            return Ok(None);
        }

        // `$event` is readonly, and a generic assignment refuses BEFORE it
        // evaluates any LHS index — so this must too, or the error a
        // handler sees for `$event[$missing] = push($event[$missing], 1)`
        // changes from "readonly" to "undefined name". Cheap and pure (a
        // name comparison), so a candidate that later declines simply
        // re-checks in the generic arm at no cost.
        self.check_event_readonly(root)?;

        let mut segs: Vec<ResolvedSeg> = Vec::with_capacity(lhs.len());
        for s in lhs {
            segs.push(match s {
                LvSeg::Field(f) => ResolvedSeg::Field((*f).to_string()),
                LvSeg::Index(e) => ResolvedSeg::Index(self.eval_expr(e).await?),
            });
        }

        // Preflight: the target must ALREADY be a list. A missing key or a
        // non-list falls back to the generic path, which produces the same
        // value/error it always did. (Index exprs are sync/pure, so the
        // generic path re-evaluating them is free of observable effects.)
        //
        // This runs BEFORE the capability gate on purpose: a candidate we
        // end up declining must not have consulted the capability policy at
        // all, or a stateful quota/audit policy would see two `push` checks
        // for one push, and an allow-once policy would turn a plain type
        // error into CAPABILITY_DENIED.
        let is_list = self
            .scope
            .with_value(root, |v| {
                matches!(path_leaf(v, &segs), Some(Value::List(_)))
            })
            .unwrap_or(false);
        if !is_list {
            return Ok(None);
        }

        // Committed to the fast path.
        // Knob A: the fast path RUNS `push`, so it must pass the same
        // capability gate as any other `push` call. Skipping it would let
        // `$m.a = push($m.a, 1)` slip past an embedder's DenyByName("push")
        // policy that every ordinary `push` obeys — a sandbox escape.
        self.track_builtin_attempt("push");
        self.check_capability("push")?;

        let val = self.eval_expr(&args[1]).await?;
        let max_list = self.ctx.max_list_len;
        let outcome = self
            .scope
            .with_mut_value(root, |v| push_through_path(v, &segs, val, max_list));
        match outcome {
            Some(Ok(())) => Ok(Some(Value::Nil)),
            Some(Err(msg)) => Err(self.runtime_err(msg)),
            // with_mut_value found no binding — impossible, the preflight
            // just read it. Fall back rather than guess.
            None => Ok(None),
        }
    }

    fn is_expr_sync(expr: &Expr) -> bool {
        match expr {
            Expr::NumberLiteral(_)
            | Expr::StringLiteral(_)
            | Expr::EscapedQuoteStringLiteral(_)
            | Expr::BoolLiteral(_)
            | Expr::NilLiteral
            | Expr::Variable(_) => true,
            Expr::BinaryOp { left, op, right } => {
                !matches!(
                    op,
                    BinOp::And | BinOp::Or | BinOp::NilCoalesce | BinOp::Pipe
                ) && Self::is_expr_sync(left)
                    && Self::is_expr_sync(right)
            }
            // CommandSub parts (`$(cmd)`) shell out — evaluable sync via
            // exec_sh_expr but avoided here so the fast path doesn't run
            // shells in tight loops. Literal+Variable cover the common
            // `"k${i}"` case used in table indexing.
            //
            // A coalescing default (`${x ?? expr}` / `${x ?: expr}`) PARSES
            // AND EVALUATES ARBITRARY MIX — it can call a function that
            // rebinds the very variable a fast path is about to mutate. It
            // is therefore neither sync nor pure, and `try_eval_expr_sync`
            // already defers on it. Classifying it as sync here was a
            // classifier/evaluator disagreement: the self-concat path
            // detected the mismatch at runtime and raised "sync
            // classifier/evaluator drift", and the push fast path (which
            // has no such guard) silently mutated the REBOUND container.
            // Reject it at classify time so both simply take the generic
            // path and keep generic semantics.
            Expr::InterpolatedString(parts) | Expr::Heredoc(parts) => {
                parts.iter().all(|p| match p {
                    StringPart::Literal(_) | StringPart::EnvVar(_) => true,
                    StringPart::Variable(spec) => split_interp_coalesce(spec).1.is_none(),
                    _ => false,
                })
            }
            // `$t[$k]` / `$xs[$i]` indexing into Map/List/String. Used
            // by the table bench's 2nd loop `$total = $total + $t[$k]`.
            Expr::Index { object, index } => {
                Self::is_expr_sync(object) && Self::is_expr_sync(index)
            }
            // Ternary is sync iff all three sub-exprs are — the runtime
            // only evaluates the taken branch, but at classify time we
            // can't know which, so require both branches sync. (`Expr::If`
            // falls through to `false`: its statement-block branches make
            // it async-only, executed via `eval_expr`.)
            Expr::Ternary {
                cond,
                then_branch,
                else_branch,
            } => {
                Self::is_expr_sync(cond)
                    && Self::is_expr_sync(then_branch)
                    && Self::is_expr_sync(else_branch)
            }
            _ => false,
        }
    }

    /// Like `is_expr_sync` but treats `FunctionCall(self_name, ...)`
    /// with sync args as sync. Required arity must match exactly so
    /// that the runtime sync path doesn't have to evaluate defaults.
    fn is_expr_sync_self(expr: &Expr, self_name: &str, self_arity: usize) -> bool {
        match expr {
            Expr::NumberLiteral(_)
            | Expr::BoolLiteral(_)
            | Expr::NilLiteral
            | Expr::Variable(_) => true,
            Expr::BinaryOp { left, op, right } => {
                !matches!(
                    op,
                    BinOp::And | BinOp::Or | BinOp::NilCoalesce | BinOp::Pipe
                ) && Self::is_expr_sync_self(left, self_name, self_arity)
                    && Self::is_expr_sync_self(right, self_name, self_arity)
            }
            Expr::FunctionCall { name, args } => {
                name == self_name
                    && args.len() == self_arity
                    && args
                        .iter()
                        .all(|a| Self::is_expr_sync_self(a, self_name, self_arity))
            }
            Expr::Ternary {
                cond,
                then_branch,
                else_branch,
            } => {
                Self::is_expr_sync_self(cond, self_name, self_arity)
                    && Self::is_expr_sync_self(then_branch, self_name, self_arity)
                    && Self::is_expr_sync_self(else_branch, self_name, self_arity)
            }
            _ => false,
        }
    }

    /// Like `is_stmt_sync` but treats self-recursive function calls
    /// with sync args as sync. Used at definition time to compute the
    /// `block_sync_self` flag.
    fn is_stmt_sync_self(stmt: &Stmt, self_name: &str, self_arity: usize) -> bool {
        match &stmt.kind {
            StmtKind::Assignment { value, .. } => {
                Self::is_expr_sync_self(value, self_name, self_arity)
            }
            StmtKind::Return(Some(e)) => Self::is_expr_sync_self(e, self_name, self_arity),
            StmtKind::Return(None) => true,
            StmtKind::Expression(e) => Self::is_expr_sync_self(e, self_name, self_arity),
            StmtKind::If {
                condition,
                then_body,
                else_ifs,
                else_body,
            } => {
                Self::is_expr_sync_self(condition, self_name, self_arity)
                    && then_body
                        .iter()
                        .all(|s| Self::is_stmt_sync_self(s, self_name, self_arity))
                    && else_ifs.iter().all(|(c, b)| {
                        Self::is_expr_sync_self(c, self_name, self_arity)
                            && b.iter()
                                .all(|s| Self::is_stmt_sync_self(s, self_name, self_arity))
                    })
                    && else_body.as_ref().is_none_or(|b| {
                        b.iter()
                            .all(|s| Self::is_stmt_sync_self(s, self_name, self_arity))
                    })
            }
            _ => false,
        }
    }

    /// Walk a body looking for any `StmtKind::Assignment` — used to
    /// disqualify the param-slot caching path in sync-block calls
    /// (an assignment could insert a new key into the function frame
    /// and rehash the bucket array, invalidating cached raw ptrs).
    fn body_has_local_assigns(body: &FunctionBody) -> bool {
        fn stmt_has_assign(stmt: &Stmt) -> bool {
            match &stmt.kind {
                StmtKind::Assignment { .. } => true,
                StmtKind::FieldAssignment { .. } => true,
                StmtKind::IndexAssignment { .. } => true,
                StmtKind::PathAssignment { .. } => true,
                StmtKind::If {
                    then_body,
                    else_ifs,
                    else_body,
                    ..
                } => {
                    then_body.iter().any(stmt_has_assign)
                        || else_ifs
                            .iter()
                            .any(|(_, body)| body.iter().any(stmt_has_assign))
                        || else_body
                            .as_ref()
                            .is_some_and(|b| b.iter().any(stmt_has_assign))
                }
                _ => false,
            }
        }
        match body {
            FunctionBody::Block(stmts) => stmts.iter().any(stmt_has_assign),
            FunctionBody::Expression(_) => false,
        }
    }

    /// Collect the free variables a lambda closes over, for **targeted
    /// closure capture** (vs. snapshotting the whole enclosing frame).
    ///
    /// Returns `Some(names)` where `names` is every variable read
    /// anywhere in the body or a parameter default — at any nesting
    /// depth, including inside nested lambdas — MINUS the lambda's own
    /// parameter names. This is a deliberately sound over-approximation
    /// of the true free set:
    ///   * It can never MISS a genuinely-free variable: any name the
    ///     body could read (directly, via `${…}` interpolation whose
    ///     names are static, via a `.field`/`[idx]` mutation target, or
    ///     inside a nested closure that must in turn capture it) is
    ///     collected.
    ///   * Over-collecting is harmless: a body-local or nested-lambda
    ///     param that happens to share a name with an enclosing frame
    ///     var gets captured then immediately shadowed by binding, and a
    ///     name absent from the current frame is dropped by
    ///     [`Scope::snapshot_frame_vars`] anyway.
    ///
    /// Returns `None` when the body can load code dynamically
    /// (`source`/`include`), whose contents could reference any
    /// enclosing variable the static walk cannot see; the caller then
    /// falls back to a full whole-frame snapshot (the pre-fix
    /// behaviour) so those rare cases stay correct.
    ///
    /// Because Mix has no `eval`, no dynamic `${<expr>}` form, and no
    /// builtin that reflects over frame variables, dropping the capture
    /// of a name the body never references is provably behaviour-
    /// neutral — it only avoids cloning data the lambda cannot observe.
    fn collect_free_vars(params: &[Param], body: &FunctionBody) -> Option<HashSet<String>> {
        fn expr_vars(e: &Expr, reads: &mut HashSet<String>, dynamic: &mut bool) {
            match e {
                Expr::Variable(name) => {
                    reads.insert(name.clone());
                }
                Expr::InterpolatedString(parts) | Expr::Heredoc(parts) => {
                    for p in parts {
                        // Only `Variable` parts touch Mix scope. `Literal`,
                        // `EnvVar` (process env), and `CommandSub` (raw
                        // /bin/sh text) reference no enclosing frame var.
                        if let StringPart::Variable(name) = p {
                            reads.insert(name.clone());
                        }
                    }
                }
                Expr::NumberLiteral(_)
                | Expr::StringLiteral(_)
                | Expr::EscapedQuoteStringLiteral(_)
                | Expr::BoolLiteral(_)
                | Expr::NilLiteral
                | Expr::CommandSub(_) => {}
                Expr::BinaryOp { left, right, .. } => {
                    expr_vars(left, reads, dynamic);
                    expr_vars(right, reads, dynamic);
                }
                Expr::UnaryOp { operand, .. } => expr_vars(operand, reads, dynamic),
                Expr::Ternary {
                    cond,
                    then_branch,
                    else_branch,
                } => {
                    expr_vars(cond, reads, dynamic);
                    expr_vars(then_branch, reads, dynamic);
                    expr_vars(else_branch, reads, dynamic);
                }
                Expr::If(ifx) => ifexpr_vars(ifx, reads, dynamic),
                Expr::FunctionCall { args, .. } => {
                    for a in args {
                        expr_vars(a, reads, dynamic);
                    }
                }
                Expr::FunctionLiteral { params, body } => {
                    // Recurse into the nested lambda so anything it closes
                    // over propagates to this (enclosing) lambda's capture
                    // set — the enclosing frame must carry it for the
                    // nested capture to find. Nested params are NOT
                    // subtracted here (harmless over-collection).
                    for p in params {
                        if let Some(d) = &p.default {
                            expr_vars(d, reads, dynamic);
                        }
                    }
                    match &**body {
                        FunctionBody::Expression(be) => expr_vars(be, reads, dynamic),
                        FunctionBody::Block(stmts) => {
                            for s in stmts {
                                stmt_vars(s, reads, dynamic);
                            }
                        }
                    }
                }
                Expr::ValueCall { callee, args } => {
                    expr_vars(callee, reads, dynamic);
                    for a in args {
                        expr_vars(a, reads, dynamic);
                    }
                }
                Expr::MethodCall { object, args, .. } => {
                    expr_vars(object, reads, dynamic);
                    for a in args {
                        expr_vars(a, reads, dynamic);
                    }
                }
                Expr::Index { object, index } => {
                    expr_vars(object, reads, dynamic);
                    expr_vars(index, reads, dynamic);
                }
                Expr::FieldAccess { object, .. } => expr_vars(object, reads, dynamic),
                Expr::ListLiteral(items) => {
                    for it in items {
                        expr_vars(it, reads, dynamic);
                    }
                }
                Expr::MapLiteral(entries) => {
                    for (_k, v) in entries {
                        expr_vars(v, reads, dynamic);
                    }
                }
                Expr::Send {
                    target,
                    command,
                    args,
                } => {
                    expr_vars(target, reads, dynamic);
                    expr_vars(command, reads, dynamic);
                    for (_k, v) in args {
                        expr_vars(v, reads, dynamic);
                    }
                }
                Expr::Sh(cmd) => expr_vars(cmd, reads, dynamic),
            }
        }

        fn ifexpr_vars(ifx: &IfExpr, reads: &mut HashSet<String>, dynamic: &mut bool) {
            expr_vars(&ifx.condition, reads, dynamic);
            for st in &ifx.then_body {
                stmt_vars(st, reads, dynamic);
            }
            for (c, b) in &ifx.else_ifs {
                expr_vars(c, reads, dynamic);
                for st in b {
                    stmt_vars(st, reads, dynamic);
                }
            }
            if let Some(b) = &ifx.else_body {
                for st in b {
                    stmt_vars(st, reads, dynamic);
                }
            }
        }

        fn stmt_vars(s: &Stmt, reads: &mut HashSet<String>, dynamic: &mut bool) {
            match &s.kind {
                StmtKind::Expression(e) => expr_vars(e, reads, dynamic),
                StmtKind::Assignment { value, .. } => expr_vars(value, reads, dynamic),
                StmtKind::FieldAssignment { object, value, .. } => {
                    // `$obj.f = …` reads $obj to fetch the map, then mutates.
                    reads.insert(object.clone());
                    expr_vars(value, reads, dynamic);
                }
                StmtKind::PathAssignment { root, path, value } => {
                    // `$m[$k].f = …` reads $m to fetch the container, then
                    // mutates through it — same shape as IndexAssignment.
                    reads.insert(root.clone());
                    for seg in path {
                        if let PathSeg::Index(e) = seg {
                            expr_vars(e, reads, dynamic);
                        }
                    }
                    expr_vars(value, reads, dynamic);
                }
                StmtKind::IndexAssignment {
                    object,
                    index,
                    value,
                } => {
                    reads.insert(object.clone());
                    expr_vars(index, reads, dynamic);
                    expr_vars(value, reads, dynamic);
                }
                StmtKind::If {
                    condition,
                    then_body,
                    else_ifs,
                    else_body,
                } => {
                    expr_vars(condition, reads, dynamic);
                    for st in then_body {
                        stmt_vars(st, reads, dynamic);
                    }
                    for (c, b) in else_ifs {
                        expr_vars(c, reads, dynamic);
                        for st in b {
                            stmt_vars(st, reads, dynamic);
                        }
                    }
                    if let Some(b) = else_body {
                        for st in b {
                            stmt_vars(st, reads, dynamic);
                        }
                    }
                }
                StmtKind::For {
                    start,
                    end,
                    step,
                    body,
                    ..
                } => {
                    expr_vars(start, reads, dynamic);
                    expr_vars(end, reads, dynamic);
                    if let Some(st) = step {
                        expr_vars(st, reads, dynamic);
                    }
                    for stt in body {
                        stmt_vars(stt, reads, dynamic);
                    }
                }
                StmtKind::ForEach { iterable, body, .. } => {
                    expr_vars(iterable, reads, dynamic);
                    for st in body {
                        stmt_vars(st, reads, dynamic);
                    }
                }
                StmtKind::While {
                    condition, body, ..
                } => {
                    expr_vars(condition, reads, dynamic);
                    for st in body {
                        stmt_vars(st, reads, dynamic);
                    }
                }
                StmtKind::Loop { body, .. } => {
                    for st in body {
                        stmt_vars(st, reads, dynamic);
                    }
                }
                StmtKind::Break(_) | StmtKind::Continue(_) => {}
                StmtKind::BreakIf(e, _) | StmtKind::ContinueIf(e, _) => {
                    expr_vars(e, reads, dynamic)
                }
                StmtKind::FunctionDef { params, body, .. } => {
                    for p in params {
                        if let Some(d) = &p.default {
                            expr_vars(d, reads, dynamic);
                        }
                    }
                    match body {
                        FunctionBody::Expression(be) => expr_vars(be, reads, dynamic),
                        FunctionBody::Block(stmts) => {
                            for st in stmts {
                                stmt_vars(st, reads, dynamic);
                            }
                        }
                    }
                }
                StmtKind::Return(opt) => {
                    if let Some(e) = opt {
                        expr_vars(e, reads, dynamic);
                    }
                }
                StmtKind::Select {
                    value,
                    cases,
                    otherwise,
                } => {
                    expr_vars(value, reads, dynamic);
                    for (c, b) in cases {
                        expr_vars(c, reads, dynamic);
                        for st in b {
                            stmt_vars(st, reads, dynamic);
                        }
                    }
                    if let Some(b) = otherwise {
                        for st in b {
                            stmt_vars(st, reads, dynamic);
                        }
                    }
                }
                StmtKind::Print { args, .. } => {
                    for a in args {
                        expr_vars(a, reads, dynamic);
                    }
                }
                StmtKind::Parse { source, .. } => expr_vars(source, reads, dynamic),
                StmtKind::Die(e) => expr_vars(e, reads, dynamic),
                StmtKind::TryCatch {
                    try_body,
                    catch,
                    finally_body,
                } => {
                    for st in try_body {
                        stmt_vars(st, reads, dynamic);
                    }
                    if let Some(clause) = catch {
                        for st in &clause.body {
                            stmt_vars(st, reads, dynamic);
                        }
                    }
                    if let Some(fbody) = finally_body {
                        for st in fbody {
                            stmt_vars(st, reads, dynamic);
                        }
                    }
                }
                StmtKind::Export { value, .. } => expr_vars(value, reads, dynamic),
                StmtKind::Alias { name, command } => {
                    if let Some(e) = name {
                        expr_vars(e, reads, dynamic);
                    }
                    if let Some(e) = command {
                        expr_vars(e, reads, dynamic);
                    }
                }
                StmtKind::Send {
                    target,
                    command,
                    args,
                }
                | StmtKind::Emit {
                    target,
                    command,
                    args,
                } => {
                    expr_vars(target, reads, dynamic);
                    expr_vars(command, reads, dynamic);
                    for (_k, v) in args {
                        expr_vars(v, reads, dynamic);
                    }
                }
                StmtKind::Address { target, body } => {
                    expr_vars(target, reads, dynamic);
                    for st in body {
                        stmt_vars(st, reads, dynamic);
                    }
                }
                StmtKind::On { body, .. } => {
                    for st in body {
                        stmt_vars(st, reads, dynamic);
                    }
                }
                // Dynamic code load — contents unknown to static analysis;
                // signal the caller to fall back to a full-frame snapshot.
                StmtKind::Source { .. } | StmtKind::Include { .. } => {
                    *dynamic = true;
                }
                StmtKind::Sh { command } => expr_vars(command, reads, dynamic),
                StmtKind::PipeToExternal { stmt, .. } => stmt_vars(stmt, reads, dynamic),
                StmtKind::Chain { left, right, .. } => {
                    stmt_vars(left, reads, dynamic);
                    stmt_vars(right, reads, dynamic);
                }
            }
        }

        let mut reads: HashSet<String> = HashSet::new();
        let mut dynamic = false;
        for p in params {
            if let Some(d) = &p.default {
                expr_vars(d, &mut reads, &mut dynamic);
            }
        }
        match body {
            FunctionBody::Expression(e) => expr_vars(e, &mut reads, &mut dynamic),
            FunctionBody::Block(stmts) => {
                for s in stmts {
                    stmt_vars(s, &mut reads, &mut dynamic);
                }
            }
        }
        if dynamic {
            return None;
        }
        // The lambda's own params are (re)bound at call time and always
        // shadow any same-named capture, so they never need capturing.
        for p in params {
            reads.remove(&p.name);
        }
        Some(reads)
    }

    /// Static check for the "dead push into a by-value parameter" footgun.
    /// Mix lists pass BY VALUE, so `push($p, x)` inside a helper mutates a
    /// throwaway copy — if `$p` is a parameter the caller never sees the
    /// change (it "ate the snare" building a `.asc` score). This finds
    /// each `push($p, …)` **statement** (result discarded) where `$p` is
    /// one of THIS function's own parameters and `$p` is **never read
    /// again anywhere in the body**, so the mutation is provably lost.
    ///
    /// Deliberately ZERO-FALSE-POSITIVE: a parameter that appears in ANY
    /// non-`push`-target read position — `return $p`, `length($p)`,
    /// `$p[i]`, `$y = push($p, …)` (result USED), `pop($p)`, passing `$p`
    /// onward, or even a coincidental mention inside a nested
    /// function/lambda — suppresses the warning entirely. It fires only
    /// when the ONLY thing the body ever does with `$p` is discard-result
    /// pushes. Candidate pushes are counted only in the function's OWN
    /// scope (a `push` inside a nested `function`/lambda body targets a
    /// different binding, so those subtrees are walked read-only —
    /// over-suppression is safe, a false alarm is not). If the body can
    /// `source`/`include` code (which runs in this scope and could read
    /// `$p`), the whole function is skipped.
    fn dead_param_push_sites(params: &[Param], body: &FunctionBody) -> Vec<(String, usize)> {
        struct Acc {
            reads: HashSet<String>,
            cand: Vec<(String, usize)>,
            dynamic: bool,
        }
        let param_set: HashSet<&str> = params.iter().map(|p| p.name.as_str()).collect();
        if param_set.is_empty() {
            return Vec::new();
        }

        fn expr_reads(e: &Expr, ps: &HashSet<&str>, acc: &mut Acc) {
            match e {
                Expr::Variable(n) => {
                    if ps.contains(n.as_str()) {
                        acc.reads.insert(n.clone());
                    }
                }
                Expr::InterpolatedString(parts) | Expr::Heredoc(parts) => {
                    for p in parts {
                        if let StringPart::Variable(n) = p
                            && ps.contains(n.as_str())
                        {
                            acc.reads.insert(n.clone());
                        }
                    }
                }
                Expr::NumberLiteral(_)
                | Expr::StringLiteral(_)
                | Expr::EscapedQuoteStringLiteral(_)
                | Expr::BoolLiteral(_)
                | Expr::NilLiteral
                | Expr::CommandSub(_) => {}
                Expr::BinaryOp { left, right, .. } => {
                    expr_reads(left, ps, acc);
                    expr_reads(right, ps, acc);
                }
                Expr::UnaryOp { operand, .. } => expr_reads(operand, ps, acc),
                Expr::Ternary {
                    cond,
                    then_branch,
                    else_branch,
                } => {
                    expr_reads(cond, ps, acc);
                    expr_reads(then_branch, ps, acc);
                    expr_reads(else_branch, ps, acc);
                }
                Expr::If(ifx) => {
                    expr_reads(&ifx.condition, ps, acc);
                    for st in &ifx.then_body {
                        stmt_walk(st, ps, true, acc);
                    }
                    for (c, b) in &ifx.else_ifs {
                        expr_reads(c, ps, acc);
                        for st in b {
                            stmt_walk(st, ps, true, acc);
                        }
                    }
                    if let Some(b) = &ifx.else_body {
                        for st in b {
                            stmt_walk(st, ps, true, acc);
                        }
                    }
                }
                Expr::FunctionCall { args, .. } => {
                    for a in args {
                        expr_reads(a, ps, acc);
                    }
                }
                Expr::FunctionLiteral { params, body } => {
                    // A lambda captures the enclosing scope FOR READS, so a
                    // mention of `$p` here IS a genuine read (suppresses).
                    // nested=true so a push inside it is not a candidate.
                    for p in params {
                        if let Some(d) = &p.default {
                            expr_reads(d, ps, acc);
                        }
                    }
                    match &**body {
                        FunctionBody::Expression(be) => expr_reads(be, ps, acc),
                        FunctionBody::Block(stmts) => {
                            for st in stmts {
                                stmt_walk(st, ps, true, acc);
                            }
                        }
                    }
                }
                Expr::ValueCall { callee, args } => {
                    expr_reads(callee, ps, acc);
                    for a in args {
                        expr_reads(a, ps, acc);
                    }
                }
                Expr::MethodCall { object, args, .. } => {
                    expr_reads(object, ps, acc);
                    for a in args {
                        expr_reads(a, ps, acc);
                    }
                }
                Expr::Index { object, index } => {
                    expr_reads(object, ps, acc);
                    expr_reads(index, ps, acc);
                }
                Expr::FieldAccess { object, .. } => expr_reads(object, ps, acc),
                Expr::ListLiteral(items) => {
                    for it in items {
                        expr_reads(it, ps, acc);
                    }
                }
                Expr::MapLiteral(entries) => {
                    for (_k, v) in entries {
                        expr_reads(v, ps, acc);
                    }
                }
                Expr::Send {
                    target,
                    command,
                    args,
                } => {
                    expr_reads(target, ps, acc);
                    expr_reads(command, ps, acc);
                    for (_k, v) in args {
                        expr_reads(v, ps, acc);
                    }
                }
                Expr::Sh(cmd) => expr_reads(cmd, ps, acc),
            }
        }

        fn stmt_walk(s: &Stmt, ps: &HashSet<&str>, nested: bool, acc: &mut Acc) {
            match &s.kind {
                // Candidate: `push($p, …)` as a bare statement (result
                // discarded), in this function's own scope, target = param.
                StmtKind::Expression(Expr::FunctionCall { name, args })
                    if !nested && name.as_str() == "push" =>
                {
                    if let Some(Expr::Variable(p)) = args.first()
                        && ps.contains(p.as_str())
                    {
                        acc.cand.push((p.clone(), s.line));
                        // The value arg(s) are still genuine reads
                        // (`push($p, length($p))` reads $p → suppress);
                        // only the args[0] target slot is not a read.
                        for a in args.iter().skip(1) {
                            expr_reads(a, ps, acc);
                        }
                        return;
                    }
                    // Not a param-target push (e.g. a local, or an index
                    // target) — walk all args as ordinary reads.
                    for a in args {
                        expr_reads(a, ps, acc);
                    }
                }
                StmtKind::Expression(e) => expr_reads(e, ps, acc),
                StmtKind::Assignment { value, .. } => expr_reads(value, ps, acc),
                StmtKind::FieldAssignment { object, value, .. } => {
                    if ps.contains(object.as_str()) {
                        acc.reads.insert(object.clone());
                    }
                    expr_reads(value, ps, acc);
                }
                StmtKind::PathAssignment { root, path, value } => {
                    if ps.contains(root.as_str()) {
                        acc.reads.insert(root.clone());
                    }
                    for seg in path {
                        if let PathSeg::Index(e) = seg {
                            expr_reads(e, ps, acc);
                        }
                    }
                    expr_reads(value, ps, acc);
                }
                StmtKind::IndexAssignment {
                    object,
                    index,
                    value,
                } => {
                    if ps.contains(object.as_str()) {
                        acc.reads.insert(object.clone());
                    }
                    expr_reads(index, ps, acc);
                    expr_reads(value, ps, acc);
                }
                StmtKind::If {
                    condition,
                    then_body,
                    else_ifs,
                    else_body,
                } => {
                    expr_reads(condition, ps, acc);
                    for st in then_body {
                        stmt_walk(st, ps, nested, acc);
                    }
                    for (c, b) in else_ifs {
                        expr_reads(c, ps, acc);
                        for st in b {
                            stmt_walk(st, ps, nested, acc);
                        }
                    }
                    if let Some(b) = else_body {
                        for st in b {
                            stmt_walk(st, ps, nested, acc);
                        }
                    }
                }
                StmtKind::For {
                    start,
                    end,
                    step,
                    body,
                    ..
                } => {
                    expr_reads(start, ps, acc);
                    expr_reads(end, ps, acc);
                    if let Some(st) = step {
                        expr_reads(st, ps, acc);
                    }
                    for stt in body {
                        stmt_walk(stt, ps, nested, acc);
                    }
                }
                StmtKind::ForEach { iterable, body, .. } => {
                    expr_reads(iterable, ps, acc);
                    for st in body {
                        stmt_walk(st, ps, nested, acc);
                    }
                }
                StmtKind::While {
                    condition, body, ..
                } => {
                    expr_reads(condition, ps, acc);
                    for st in body {
                        stmt_walk(st, ps, nested, acc);
                    }
                }
                StmtKind::Loop { body, .. } => {
                    for st in body {
                        stmt_walk(st, ps, nested, acc);
                    }
                }
                StmtKind::Break(_) | StmtKind::Continue(_) => {}
                StmtKind::BreakIf(e, _) | StmtKind::ContinueIf(e, _) => expr_reads(e, ps, acc),
                StmtKind::FunctionDef { params, body, .. } => {
                    // Nested named function: separate param scope, cannot
                    // see our locals. Walk read-only (nested=true) so a
                    // `$p` mention there suppresses but a push there is not
                    // charged to us.
                    for p in params {
                        if let Some(d) = &p.default {
                            expr_reads(d, ps, acc);
                        }
                    }
                    match body {
                        FunctionBody::Expression(be) => expr_reads(be, ps, acc),
                        FunctionBody::Block(stmts) => {
                            for st in stmts {
                                stmt_walk(st, ps, true, acc);
                            }
                        }
                    }
                }
                StmtKind::Return(opt) => {
                    if let Some(e) = opt {
                        expr_reads(e, ps, acc);
                    }
                }
                StmtKind::Select {
                    value,
                    cases,
                    otherwise,
                } => {
                    expr_reads(value, ps, acc);
                    for (c, b) in cases {
                        expr_reads(c, ps, acc);
                        for st in b {
                            stmt_walk(st, ps, nested, acc);
                        }
                    }
                    if let Some(b) = otherwise {
                        for st in b {
                            stmt_walk(st, ps, nested, acc);
                        }
                    }
                }
                StmtKind::Print { args, .. } => {
                    for a in args {
                        expr_reads(a, ps, acc);
                    }
                }
                StmtKind::Parse { source, .. } => expr_reads(source, ps, acc),
                StmtKind::Die(e) => expr_reads(e, ps, acc),
                StmtKind::TryCatch {
                    try_body,
                    catch,
                    finally_body,
                } => {
                    for st in try_body {
                        stmt_walk(st, ps, nested, acc);
                    }
                    if let Some(clause) = catch {
                        for st in &clause.body {
                            stmt_walk(st, ps, nested, acc);
                        }
                    }
                    if let Some(fbody) = finally_body {
                        for st in fbody {
                            stmt_walk(st, ps, nested, acc);
                        }
                    }
                }
                StmtKind::Export { value, .. } => expr_reads(value, ps, acc),
                StmtKind::Alias { name, command } => {
                    if let Some(e) = name {
                        expr_reads(e, ps, acc);
                    }
                    if let Some(e) = command {
                        expr_reads(e, ps, acc);
                    }
                }
                StmtKind::Send {
                    target,
                    command,
                    args,
                }
                | StmtKind::Emit {
                    target,
                    command,
                    args,
                } => {
                    expr_reads(target, ps, acc);
                    expr_reads(command, ps, acc);
                    for (_k, v) in args {
                        expr_reads(v, ps, acc);
                    }
                }
                StmtKind::Address { target, body } => {
                    expr_reads(target, ps, acc);
                    for st in body {
                        stmt_walk(st, ps, nested, acc);
                    }
                }
                StmtKind::On { body, .. } => {
                    // A handler body runs LATER in a fresh handler frame
                    // ($event injected), NOT in this function's frame — it
                    // never sees our by-value params. Walk read-only so a
                    // `push($p, …)` there is not charged as a candidate
                    // (an `address` block, by contrast, runs inline in this
                    // frame, so it keeps `nested`).
                    for st in body {
                        stmt_walk(st, ps, true, acc);
                    }
                }
                // Runs code loaded at runtime IN THIS SCOPE — it could read
                // `$p` invisibly, so bail on the whole function.
                StmtKind::Source { path } | StmtKind::Include { path } => {
                    acc.dynamic = true;
                    expr_reads(path, ps, acc);
                }
                StmtKind::Sh { command } => expr_reads(command, ps, acc),
                StmtKind::PipeToExternal { stmt, .. } => stmt_walk(stmt, ps, nested, acc),
                StmtKind::Chain { left, right, .. } => {
                    stmt_walk(left, ps, nested, acc);
                    stmt_walk(right, ps, nested, acc);
                }
            }
        }

        let mut acc = Acc {
            reads: HashSet::new(),
            cand: Vec::new(),
            dynamic: false,
        };
        match body {
            FunctionBody::Expression(e) => expr_reads(e, &param_set, &mut acc),
            FunctionBody::Block(stmts) => {
                for s in stmts {
                    stmt_walk(s, &param_set, false, &mut acc);
                }
            }
        }
        if acc.dynamic {
            return Vec::new();
        }
        acc.cand
            .into_iter()
            .filter(|(p, _)| !acc.reads.contains(p))
            .collect()
    }

    /// Construct the [`MixFunction`] for a `fn`/`function` definition —
    /// the static analyses (sync prefix, self-recursion shape, fib
    /// bytecode) plus the record itself. Shared by the in-flow
    /// `StmtKind::FunctionDef` arm and the top-level hoisting pre-pass in
    /// `execute_inner`; deliberately emits NO diagnostics (see the arm).
    fn build_function(&self, name: &str, params: &[Param], body: &FunctionBody) -> MixFunction {
        let sync_prefix_len = Self::compute_sync_prefix(body);
        let block_sync_self = Self::compute_block_sync_self(name, params, body);
        let body_no_local_assigns = !Self::body_has_local_assigns(body);
        let fib_bytecode = if block_sync_self && body_no_local_assigns {
            Self::compile_fib_bytecode(name, params, body)
        } else {
            None
        };
        MixFunction {
            name: std::rc::Rc::from(name),
            params: params.to_vec(),
            body: body.clone(),
            def_file: self.ctx.current_file.clone(),
            captures: None,
            sync_prefix_len,
            block_sync_self,
            body_no_local_assigns,
            fib_bytecode,
            module_env: None,
        }
    }

    /// Emit the dead-push-into-a-by-value-parameter warning (see
    /// [`dead_param_push_sites`]) to stderr, at most once per unique
    /// message per program run. Called when a function is defined.
    fn warn_dead_param_pushes(&self, fn_name: &str, params: &[Param], body: &FunctionBody) {
        let sites = Self::dead_param_push_sites(params, body);
        if sites.is_empty() {
            return;
        }
        let mut g = self.globals.borrow_mut();
        for (param, line) in sites {
            let where_ = if line > 0 {
                format!("{}() at line {}", fn_name, line)
            } else {
                format!("{}()", fn_name)
            };
            let msg = format!(
                "mix: warning: in {where_}: push(${param}, …) discards its result but ${param} is a by-value parameter never read again — the mutation is lost to the caller (Mix lists pass by value). Return the list or use concat(). See: mix man functions"
            );
            if g.warned_diagnostics.insert(msg.clone()) {
                let _ = writeln!(g.stderr, "{msg}");
            }
        }
    }

    /// Compute the `block_sync_self` flag for a function. True when
    /// every stmt in the body passes `is_stmt_sync_self`, i.e. the
    /// only async-flavoured constructs are recursive calls to this
    /// same function.
    fn compute_block_sync_self(name: &str, params: &[Param], body: &FunctionBody) -> bool {
        let arity = params.len();
        // Conservative: any param with a default could need async eval
        // at call time, so disqualify.
        if params.iter().any(|p| p.default.is_some()) {
            return false;
        }
        match body {
            FunctionBody::Block(stmts) => stmts
                .iter()
                .all(|s| Self::is_stmt_sync_self(s, name, arity)),
            FunctionBody::Expression(e) => Self::is_expr_sync_self(e, name, arity),
        }
    }

    /// Count leading sync statements in a function body. Used at
    /// function-definition time to cache `sync_prefix_len` on
    /// `MixFunction` so the per-call path can skip the prefix
    /// without re-walking the AST.
    fn compute_sync_prefix(body: &FunctionBody) -> usize {
        match body {
            FunctionBody::Block(stmts) => {
                stmts.iter().take_while(|s| Self::is_stmt_sync(s)).count()
            }
            FunctionBody::Expression(_) => 0,
        }
    }

    /// Compile a self-recursive numeric function body to fib-style
    /// bytecode. Returns `Some(bytecode)` when the body matches:
    ///   `if cond { return then_expr } return else_expr`
    /// where `cond` is a comparison of arith expressions, both branches
    /// are arith expressions over params/literals/`+ - * / mod **` and
    /// recursive calls to `name` only. Caller verified
    /// `block_sync_self && body_no_local_assigns`. Returns `None` on
    /// any shape/content mismatch — caller falls back to the AST path.
    fn compile_fib_bytecode(
        name: &str,
        params: &[Param],
        body: &FunctionBody,
    ) -> Option<Vec<FibOp>> {
        // `eval_fib_program::CallSelf` recurses through a fixed `[f64; 4]`
        // arg buffer; refuse to compile bytecode for functions with more
        // than 4 params so callers fall back to the AST path.
        if params.len() > 4 {
            return None;
        }
        let stmts = match body {
            FunctionBody::Block(s) => s,
            _ => return None,
        };
        if stmts.len() != 2 {
            return None;
        }
        let (cond, then_expr) = match &stmts[0].kind {
            StmtKind::If {
                condition,
                then_body,
                else_ifs,
                else_body,
            } => {
                if !else_ifs.is_empty() || else_body.is_some() || then_body.len() != 1 {
                    return None;
                }
                match &then_body[0].kind {
                    StmtKind::Return(Some(e)) => (condition, e),
                    _ => return None,
                }
            }
            _ => return None,
        };
        let else_expr = match &stmts[1].kind {
            StmtKind::Return(Some(e)) => e,
            _ => return None,
        };
        let (cmp_op, cmp_l, cmp_r) = match cond {
            Expr::BinaryOp { left, op, right }
                if matches!(
                    op,
                    BinOp::Lt | BinOp::Gt | BinOp::LtEq | BinOp::GtEq | BinOp::Eq | BinOp::NotEq
                ) =>
            {
                (op, &**left, &**right)
            }
            _ => return None,
        };
        let param_names: Vec<&str> = params.iter().map(|p| p.name.as_str()).collect();
        if param_names.len() > u8::MAX as usize {
            return None;
        }
        let arity = param_names.len() as u8;

        let mut prog: Vec<FibOp> = Vec::with_capacity(16);
        // Compile the cond's two operands as arith expressions. The right
        // operand evaluates with the left's result live on the stack
        // (hence `1 +`); the JZ pops both, so the then/else branches
        // start back at depth 0.
        let cmp_l_depth = Self::compile_fib_arith(cmp_l, name, arity, &param_names, &mut prog)?;
        let cmp_r_depth = Self::compile_fib_arith(cmp_r, name, arity, &param_names, &mut prog)?;
        // Reserve the comparison-jump opcode; we'll patch the target
        // once we know where the else-branch starts.
        let cmp_jump_idx = prog.len();
        prog.push(FibOp::Jump(0)); // placeholder, replaced below
        // Then-branch: evaluate then_expr, return.
        let then_depth = Self::compile_fib_arith(then_expr, name, arity, &param_names, &mut prog)?;
        prog.push(FibOp::Ret);
        // Patch the comparison-jump target to land here (start of else).
        let else_target: u16 = prog.len().try_into().ok()?;
        prog[cmp_jump_idx] = match cmp_op {
            BinOp::Lt => FibOp::LtJZ(else_target),
            BinOp::Gt => FibOp::GtJZ(else_target),
            BinOp::LtEq => FibOp::LeJZ(else_target),
            BinOp::GtEq => FibOp::GeJZ(else_target),
            BinOp::Eq => FibOp::EqJZ(else_target),
            BinOp::NotEq => FibOp::NeJZ(else_target),
            _ => return None,
        };
        // Else-branch: evaluate else_expr, return.
        let else_depth = Self::compile_fib_arith(else_expr, name, arity, &param_names, &mut prog)?;
        prog.push(FibOp::Ret);
        // `eval_fib_program` runs on a fixed [f64; NUM_STACK_SLOTS]
        // operand stack; refuse any program that would index past it.
        let max_depth = cmp_l_depth
            .max(1 + cmp_r_depth)
            .max(then_depth)
            .max(else_depth);
        if max_depth > NUM_STACK_SLOTS {
            return None;
        }
        Some(prog)
    }

    /// Compile an arithmetic expression for the fib bytecode. Accepts:
    /// NumberLiteral, Variable matching a param, BinaryOp Add/Sub/Mul/
    /// Div/Mod/Power over arith expressions, and FunctionCall to the
    /// declared self-name with arity-matching all-arith args.
    ///
    /// Returns the maximum operand-stack depth the compiled
    /// (sub)expression needs, relative to the stack level at entry —
    /// `compile_fib_program` gates the whole program against
    /// [`NUM_STACK_SLOTS`] so `eval_fib_program`'s fixed stack can't
    /// be overrun.
    fn compile_fib_arith(
        expr: &Expr,
        self_name: &str,
        self_arity: u8,
        param_names: &[&str],
        out: &mut Vec<FibOp>,
    ) -> Option<usize> {
        match expr {
            Expr::NumberLiteral(n) => {
                out.push(FibOp::PushConst(*n));
                Some(1)
            }
            Expr::Variable(name) => {
                let idx = param_names.iter().position(|p| *p == name.as_str())?;
                out.push(FibOp::PushArg(idx as u8));
                Some(1)
            }
            Expr::BinaryOp { left, op, right } => {
                let left_depth =
                    Self::compile_fib_arith(left, self_name, self_arity, param_names, out)?;
                let right_depth =
                    Self::compile_fib_arith(right, self_name, self_arity, param_names, out)?;
                out.push(match op {
                    BinOp::Add => FibOp::Add,
                    BinOp::Sub => FibOp::Sub,
                    BinOp::Mul => FibOp::Mul,
                    BinOp::Div => FibOp::Div,
                    BinOp::Mod => FibOp::Mod,
                    BinOp::Power => FibOp::Pow,
                    _ => return None,
                });
                Some(left_depth.max(1 + right_depth))
            }
            Expr::FunctionCall { name, args } => {
                if name.as_str() != self_name || args.len() != self_arity as usize {
                    return None;
                }
                // Arg i evaluates with the previous i results live.
                let mut depth = 1; // the CallSelf result
                for (i, arg) in args.iter().enumerate() {
                    let d = Self::compile_fib_arith(arg, self_name, self_arity, param_names, out)?;
                    depth = depth.max(i + d);
                }
                out.push(FibOp::CallSelf(self_arity));
                Some(depth)
            }
            _ => None,
        }
    }

    /// Static check: is this statement's full execution path sync?
    fn is_stmt_sync(stmt: &Stmt) -> bool {
        match &stmt.kind {
            StmtKind::Assignment { value, .. } => Self::is_expr_sync(value),
            StmtKind::Return(None) => true,
            StmtKind::Return(Some(e)) => Self::is_expr_sync(e),
            StmtKind::Expression(e) => Self::is_expr_sync(e),
            StmtKind::If {
                condition,
                then_body,
                else_ifs,
                else_body,
            } => {
                Self::is_expr_sync(condition)
                    && then_body.iter().all(Self::is_stmt_sync)
                    && else_ifs
                        .iter()
                        .all(|(c, b)| Self::is_expr_sync(c) && b.iter().all(Self::is_stmt_sync))
                    && else_body
                        .as_ref()
                        .map(|b| b.iter().all(Self::is_stmt_sync))
                        .unwrap_or(true)
            }
            _ => false,
        }
    }

    /// Evaluate an expression on the sync fast path, converting the
    /// "not sync-evaluable" sentinel (`try_eval_expr_sync` → `None`)
    /// into a recoverable runtime error instead of a process-aborting
    /// `.expect()` panic.
    ///
    /// `exec_stmt_sync` only runs statements the static `is_stmt_sync`
    /// / `is_expr_sync` classifier accepted, and that classifier is
    /// hand-maintained to mirror exactly what `try_eval_expr_sync` can
    /// evaluate. They are currently aligned, so a `None` here is
    /// unreachable in practice — but it would mean the two have DRIFTED.
    /// Returning an error (rather than panicking) keeps mix-as-login-shell
    /// alive: a panic on this path aborts the whole interactive session.
    /// See `_doc/mix/evaluator-bugs.md`.
    fn eval_sync_verified(&mut self, expr: &Expr) -> MixResult<Value> {
        match self.try_eval_expr_sync(expr) {
            Some(res) => res,
            None => Err(self.runtime_err(
                "internal evaluator error: sync classifier/evaluator drift \
                 (please report this Mix bug)",
            )),
        }
    }

    /// The classifier-drift error for the other verified-sync sites
    /// (`try_call_function_sync`, `run_sync_block_body`/`_f64`), which
    /// used to `.expect()`/`unreachable!()` — a panic on those paths
    /// aborts the whole interactive session / embedding daemon worker.
    /// Same recoverable-error treatment as [`eval_sync_verified`],
    /// with the drifted site named.
    fn sync_drift_err(&self, site: &str) -> MixError {
        self.runtime_err(format!(
            "internal evaluator error: sync classifier/evaluator drift \
             in {site} (please report this Mix bug)"
        ))
    }

    /// Run a sync statement (caller has verified `is_stmt_sync`).
    /// Returns the statement's value, or propagates `Err(MixError::Return)`
    /// when a `return` is encountered (caller in call_function unwraps).
    fn exec_stmt_sync(&mut self, stmt: &Stmt) -> MixResult<Value> {
        // Mirror `exec_stmt`: keep `current_line` accurate on the sync
        // fast path too, so the drift guard above — and any RuntimeError
        // raised from this statement (e.g. an undefined variable) —
        // reports this statement's line rather than a stale one left by
        // the previous async statement.
        if stmt.line > 0 {
            self.ctx.current_line = stmt.line;
        }
        // Trace on the sync fast path too — most REPL statements
        // (`$x = 1`, a bare expression) are `is_stmt_sync` and never reach
        // `exec_stmt`, so without this `mix trace on` would show nothing
        // for them. A statement runs through exactly one of the two paths,
        // so there is no double emit.
        self.trace_stmt(stmt);
        self.track_stmt_keywords(stmt);
        match &stmt.kind {
            StmtKind::Assignment { name, value } => {
                self.check_event_readonly(name)?;
                // In-place self-concat fast path (mirrors the async arm) — the
                // sync-verified executor runs the same accumulator loops.
                if let Some(rhs_expr) = Self::self_concat_rhs(name, value)
                    && Self::is_expr_sync(rhs_expr)
                    && self.assignment_target_is_string(name)
                {
                    let rhs_val = self.eval_sync_verified(rhs_expr)?;
                    self.append_to_assignment_string(name, &rhs_val)?;
                    return Ok(Value::Nil);
                }
                let val = self.eval_sync_verified(value)?;
                if self.ctx.function_depth > 0 {
                    self.scope.set_in_current(name.clone(), val);
                } else {
                    self.scope.update_or_set(name, val);
                }
                Ok(Value::Nil)
            }
            StmtKind::Return(expr) => {
                let value = match expr {
                    Some(e) => self.eval_sync_verified(e)?,
                    None => Value::Nil,
                };
                Err(MixError::Return { value })
            }
            StmtKind::Expression(e) => self.eval_sync_verified(e),
            StmtKind::If {
                condition,
                then_body,
                else_ifs,
                else_body,
            } => {
                let cond = self.eval_sync_verified(condition)?;
                if cond.is_truthy() {
                    let mut last = Value::Nil;
                    for s in then_body {
                        last = self.exec_stmt_sync(s)?;
                    }
                    return Ok(last);
                }
                for (eif_cond, eif_body) in else_ifs {
                    let cond = self.eval_sync_verified(eif_cond)?;
                    if cond.is_truthy() {
                        let mut last = Value::Nil;
                        for s in eif_body {
                            last = self.exec_stmt_sync(s)?;
                        }
                        return Ok(last);
                    }
                }
                if let Some(eb) = else_body {
                    let mut last = Value::Nil;
                    for s in eb {
                        last = self.exec_stmt_sync(s)?;
                    }
                    return Ok(last);
                }
                Ok(Value::Nil)
            }
            _ => unreachable!("exec_stmt_sync called on non-sync stmt"),
        }
    }

    fn eval_expr<'a>(
        &'a mut self,
        expr: &'a Expr,
    ) -> Pin<Box<dyn Future<Output = MixResult<Value>> + 'a>> {
        Box::pin(async move {
            match expr {
                Expr::NumberLiteral(n) => Ok(Value::Number(*n)),
                Expr::StringLiteral(s) | Expr::EscapedQuoteStringLiteral(s) => {
                    Ok(Value::String(s.clone()))
                }
                Expr::BoolLiteral(b) => Ok(Value::Bool(*b)),
                Expr::NilLiteral => Ok(Value::Nil),

                Expr::InterpolatedString(parts) | Expr::Heredoc(parts) => {
                    let mut result = String::new();
                    for part in parts {
                        match part {
                            StringPart::Literal(s) => result.push_str(s),
                            StringPart::Variable(name) => {
                                // Resolution + `?? / ?:` defaults live in a boxed
                                // helper so this arm stays tiny — keeping the
                                // recursive `eval_expr` future small (the runaway-
                                // recursion depth cap must fire before the real
                                // stack overflows; see recursion_no_crash.rs).
                                let v = self.resolve_interp_variable(name).await?;
                                v.write_mix(&mut result);
                            }
                            StringPart::CommandSub(cmd) => {
                                let val = self.exec_sh_expr(cmd)?;
                                val.write_mix(&mut result);
                            }
                            StringPart::EnvVar(name) => {
                                result.push_str(&std::env::var(name).unwrap_or_default());
                            }
                        }
                    }
                    // Knob D: interpolation grows a string (e.g.
                    // `$s = "${s}x"` in a loop), bypassing the `..` check.
                    let v = Value::String(result);
                    self.check_collection_size(&v)?;
                    Ok(v)
                }

                Expr::Variable(name) => {
                    // SPEC 18 Phase 2 WS3-C.7b — γ-aware read so the
                    // per-invocation child evaluator (and the parent
                    // after first promotion) walks the shared global
                    // frame on miss.
                    match self.scope.get_value(name) {
                        Some(val) => Ok(val.into_value()),
                        None => {
                            // Positional args ($1, $2, ...) return nil when not set
                            if name.chars().all(|c| c.is_ascii_digit()) {
                                Ok(Value::Nil)
                            } else {
                                Err(MixError::structured(
                                    "NAME_UNDEFINED",
                                    format!("undefined variable '${}'", name),
                                ))
                            }
                        }
                    }
                }

                Expr::BinaryOp { left, op, right } => {
                    // Fast path: skip Box::pin'ing the recursive eval_expr for
                    // leaf expressions (literals, variable reads). Tight loops
                    // like `$sum + $i * 2 - 1` evaluate 4 leaves per iteration;
                    // boxing each future was a per-iteration allocation tax.
                    let mut lval = match &**left {
                        Expr::NumberLiteral(n) => Value::Number(*n),
                        Expr::BoolLiteral(b) => Value::Bool(*b),
                        Expr::NilLiteral => Value::Nil,
                        // SPEC 18 Phase 2 WS3-C.7b — γ-aware leaf read.
                        Expr::Variable(name) => match self.scope.get_value(name) {
                            Some(v) => v.into_value(),
                            // Fall back so eval_expr emits the canonical
                            // "undefined variable" error (or nil for $1/$2).
                            None => self.eval_expr(left).await?,
                        },
                        Expr::FunctionCall { .. } => match self.try_inline_user_call(left).await? {
                            Some(v) => v,
                            None => self.eval_expr(left).await?,
                        },
                        _ => self.eval_expr(left).await?,
                    };

                    // Short-circuit for logical operators
                    match op {
                        BinOp::And => {
                            if !lval.is_truthy() {
                                return Ok(lval);
                            }
                            return self.eval_expr(right).await;
                        }
                        BinOp::Or => {
                            if lval.is_truthy() {
                                return Ok(lval);
                            }
                            return self.eval_expr(right).await;
                        }
                        BinOp::NilCoalesce => {
                            if !matches!(lval, Value::Nil) {
                                return Ok(lval);
                            }
                            return self.eval_expr(right).await;
                        }
                        BinOp::Pipe => {
                            // Pipe: bind the left result into `$_` for the
                            // right side. `$_` is a binder (introduced by the
                            // pipe), so it respects function scope — inside a
                            // function it stays local rather than clobbering a
                            // caller/global `$_` via the globals-fallback.
                            self.bind_scoped("_", lval);
                            return self.eval_expr(right).await;
                        }
                        _ => {}
                    }

                    let rval = match &**right {
                        Expr::NumberLiteral(n) => Value::Number(*n),
                        Expr::BoolLiteral(b) => Value::Bool(*b),
                        Expr::NilLiteral => Value::Nil,
                        // SPEC 18 Phase 2 WS3-C.7b — γ-aware leaf read.
                        Expr::Variable(name) => match self.scope.get_value(name) {
                            Some(v) => v.into_value(),
                            None => self.eval_expr(right).await?,
                        },
                        Expr::FunctionCall { .. } => {
                            match self.try_inline_user_call(right).await? {
                                Some(v) => v,
                                None => self.eval_expr(right).await?,
                            }
                        }
                        _ => self.eval_expr(right).await?,
                    };
                    self.eval_binop(op, &mut lval, &rval)
                }

                Expr::UnaryOp { op, operand } => {
                    let val = self.eval_expr(operand).await?;
                    match op {
                        UnaryOp::Neg => {
                            let n = val.to_number().ok_or_else(|| MixError::RuntimeError {
                                span: None,
                                msg: format!("cannot negate {}", val.type_name()),
                            })?;
                            Ok(Value::Number(-n))
                        }
                        UnaryOp::Not => Ok(Value::Bool(!val.is_truthy())),
                    }
                }

                Expr::FunctionCall { name, args } => {
                    // Strict arity (D5): gate EVERY metadata builtin here,
                    // before any inline special-form dispatch below can
                    // return — the HOF/general-builtin branches are too
                    // late for push/pop/shift, sleep, printf, db/jmap/bus,
                    // etc. (codex release review, MAJOR). The lookup is a
                    // single indexed probe; only under strict mode.
                    if self.ctx.arity_strict {
                        self.check_builtin_arity(name, args.len())?;
                    }
                    self.track_builtin_attempt(name);

                    // Special handling for mutating builtins (push/pop/shift)
                    // that need to modify the variable in scope
                    if matches!(name.as_str(), "push" | "pop" | "shift") {
                        return self.call_mutating_builtin(name, args).await;
                    }

                    let mut eval_args = Vec::with_capacity(args.len());
                    for arg in args {
                        // Sync fast path: fib's `fib($n-1) + fib($n-2)` evaluates
                        // arithmetic args once per recursive call (~175K times in
                        // BENCH_FIB). Skipping the per-arg `eval_expr` Box::pin
                        // when the arg is a literal/variable/sync-binop avoids
                        // an allocation per arg per call.
                        if let Some(r) = self.try_eval_expr_sync(arg) {
                            eval_args.push(r?);
                        } else {
                            eval_args.push(self.eval_expr(arg).await?);
                        }
                    }

                    // Async builtins (must be handled before sync builtins)
                    if name == "sleep" {
                        self.check_capability(name)?; // Knob A
                        let secs = match eval_args.first() {
                            Some(value) => required_number("sleep(): argument 1", value)?,
                            None => 0.0,
                        };
                        let duration = as_duration("sleep(): argument 1", secs)?;

                        // Non-tokio fallback: plain OS-thread sleep. Scripts
                        // compiled without the tokio-sleep feature don't have
                        // an async runtime at all, so `on` handlers can't work
                        // either — the thread-blocking path is the only
                        // correct behavior here.
                        #[cfg(not(feature = "tokio-sleep"))]
                        {
                            std::thread::sleep(duration);
                            return Ok(Value::Nil);
                        }

                        #[cfg(feature = "tokio-sleep")]
                        {
                            // Fast path: no handlers registered — plain sleep.
                            // The CancellationToken in run_source wraps the
                            // entire execution in a select! against ctrl_c,
                            // so cancellation arrives by dropping this future.
                            let no_handlers = {
                                let g = self.globals.borrow();
                                g.handlers.is_empty() || g.bus_handler.is_none()
                            };
                            if no_handlers {
                                tokio::time::sleep(duration).await;
                                return Ok(Value::Nil);
                            }

                            // SPEC 18 Phase 2 WS3-C.7d/C.7e — inside a
                            // held dispatch admission (writer OR any
                            // reader), sleep is a plain timer. The
                            // select!→dispatch_event arm below would
                            // deadlock on the writer permit a Class S
                            // body holds for its full duration, and
                            // would bypass the single-pump invariant
                            // for Class C bodies (the per-citizen event
                            // pump's spawn_local is the sole arrival
                            // site post-C.7d — a handler-body builtin
                            // must not be a second event pump).
                            //
                            // C.7e wraps the plain-timer with
                            // `await_with_class_c_yield` so a Class C
                            // async body releases its reader permit
                            // across the sleep, lets a queued Class S
                            // writer admit (or peers continue), and
                            // reacquires before returning. For Class S
                            // and Class C-embedded-plain (both
                            // writer-held — `ctx.class_c_read_permit ==
                            // None`), the helper is the await-through
                            // no-op fast path; the writer permit stays
                            // held and the sleep blocks all other
                            // dispatches as today's contract requires.
                            if self.globals.borrow().scheduler.is_dispatching() {
                                self.await_with_class_c_yield(tokio::time::sleep(duration))
                                    .await?;
                                return Ok(Value::Nil);
                            }

                            // Yield-point path: capture deadline ONCE so handlers
                            // that fire during the sleep don't extend its
                            // duration. Deadline-based (not duration-based) is
                            // correct for producer loops — the cadence holds
                            // even under handler load, and a handler that runs
                            // longer than the remaining time causes the next
                            // iteration to exit immediately (logged below).
                            let deadline = tokio::time::Instant::now() + duration;
                            let mut transport_closed = false;

                            // Local enum to carry the `select!` outcome OUT of
                            // the arm so `dispatch_event` can take `&mut self`
                            // afterward. Since WS3-C.3 the handler is held as
                            // an owned `Rc` clone (no `self.globals` borrow
                            // survives), but the `next_incoming()` future itself
                            // pins `&handler` for its duration; an inline match
                            // on `select!` output would keep that future's borrow
                            // alive into the match arms and still conflict with
                            // the `&mut self` needed for dispatch. The enum
                            // carries an owned value out of the tight future
                            // scope, breaking the chain cleanly.
                            // Do NOT inline this — collapsing it into a direct
                            // match reintroduces the borrow conflict.
                            enum SleepOutcome {
                                Deadline,
                                Event(Option<IncomingEvent>),
                            }

                            loop {
                                let now = tokio::time::Instant::now();
                                if now >= deadline {
                                    // Deadline reached (possibly already passed
                                    // if the previous iteration's handler ran
                                    // long). Log overshoot when it's meaningful.
                                    // TODO: the 10ms absolute threshold is a
                                    // first cut. Reconsider with a ratio-based
                                    // policy (e.g. overshoot > duration * 0.05,
                                    // or absolute > 100ms) after observing
                                    // sysmon in production — ratio-based catches
                                    // both "tiny sleep degraded a lot" and "big
                                    // sleep degraded noticeably" without a
                                    // single absolute number dominating both.
                                    let overshoot = now.saturating_duration_since(deadline);
                                    if overshoot > std::time::Duration::from_millis(10) {
                                        tracing::debug!(
                                            overshoot_ms = overshoot.as_millis() as u64,
                                            "sleep deadline passed on re-entry (handler ran longer than remaining)"
                                        );
                                    }
                                    return Ok(Value::Nil);
                                }

                                if transport_closed {
                                    // Transport is dead — finish the remaining
                                    // sleep as a plain timer with no yield
                                    // point. No more polling. The script may
                                    // not use events at all, and the main body
                                    // depends on the sleep duration being
                                    // honored; do NOT return early here.
                                    tokio::time::sleep_until(deadline).await;
                                    return Ok(Value::Nil);
                                }

                                // WS3-C.3 clone-out: hold an
                                // `Rc<dyn BusHandler>` clone in a local
                                // for the select!. Nothing on
                                // `self.globals` is borrowed across the
                                // await; after the block closes only an
                                // owned `SleepOutcome` and the dropped
                                // Rc remain, and `self` is free for
                                // `&mut` methods on the dispatch path.
                                //
                                // Cancellation safety (load-bearing invariants):
                                //   1. tokio::time::sleep_until is a pure timer
                                //      future; dropping it removes the timer
                                //      wheel entry with no side effects.
                                //   2. next_incoming holds the ReceiverGuard
                                //      (see cosmix-mix/src/bus.rs), whose drop
                                //      impl restores the receiver to the slot
                                //      whether the future completes normally
                                //      or is cancelled mid-await.
                                //   3. tokio::sync::mpsc::UnboundedReceiver::
                                //      recv() is documented cancellation-safe
                                //      — a dropped recv() future does not
                                //      lose messages; the next recv() sees
                                //      the same queue state.
                                //
                                // Any refactor that violates any of these three
                                // properties silently breaks the select! — read
                                // the comments, don't just rearrange the code.
                                let handler = { self.globals.borrow().bus_handler.clone() };
                                let Some(handler) = handler else {
                                    // Handler vanished between the Some-check
                                    // above and this clone (an embedder tore
                                    // down Bus mid-sleep). Degrade to the
                                    // plain timer rather than panicking.
                                    tokio::time::sleep_until(deadline).await;
                                    return Ok(Value::Nil);
                                };
                                let outcome: SleepOutcome = tokio::select! {
                                    _ = tokio::time::sleep_until(deadline) => SleepOutcome::Deadline,
                                    event = handler.next_incoming() => SleepOutcome::Event(event),
                                };

                                match outcome {
                                    SleepOutcome::Deadline => return Ok(Value::Nil),
                                    SleepOutcome::Event(Some(ev)) => {
                                        // dispatch_event takes the scheduler
                                        // admission guard, runs the handler
                                        // body with $event injected,
                                        // logs+swallows handler errors
                                        // internally, and drains any reentrant
                                        // events that accumulated during the
                                        // handler's own yield points.
                                        // The `?` is infallible today (handler
                                        // errors are swallowed internally), but
                                        // stays for future error-propagation
                                        // paths — a future non-handler error
                                        // in dispatch_event should surface at
                                        // the sleep site, not be silently
                                        // swallowed by `let _ = ...`.
                                        self.dispatch_event(ev).await?;
                                        // Loop re-enters select with same deadline.
                                    }
                                    SleepOutcome::Event(None) => {
                                        // Transport closed mid-sleep. Transition
                                        // to plain-sleep mode for the remainder.
                                        // The next iteration will observe the
                                        // flag and switch paths. Do not return
                                        // here — the main body may depend on
                                        // the sleep duration being honored
                                        // regardless of event-stream liveness.
                                        transport_closed = true;
                                    }
                                }
                            }
                        }
                    }
                    if name == "readline" {
                        self.check_capability(name)?; // Knob A
                        let prompt = eval_args
                            .first()
                            .map(|v| v.to_mix_string())
                            .unwrap_or_default();
                        if !prompt.is_empty() {
                            use std::io::Write;
                            print!("{}", prompt);
                            std::io::stdout().flush().ok();
                        }
                        let mut line = String::new();
                        // Not `.ok()`: on invalid UTF-8 read_line leaves `line`
                        // untouched and returns Err, so swallowing it returns ""
                        // — indistinguishable from EOF. A hook or filter reading
                        // one undecodable line then silently does nothing.
                        if let Err(e) = std::io::stdin().read_line(&mut line) {
                            return Err(MixError::RuntimeError {
                                span: None,
                                msg: format!("readline: {}", e),
                            });
                        }
                        let v = Value::String(line.trim_end_matches('\n').to_string());
                        self.check_collection_size(&v)?; // Knob D
                        return Ok(v);
                    }
                    if name == "read_stdin" {
                        self.check_capability(name)?; // Knob A
                        use std::io::Read;
                        let mut buf = String::new();
                        // Same trap as readline, and worse in practice: git
                        // feeds pre-push its ref list on stdin, ref names may
                        // hold non-UTF-8 bytes, and `.ok()` turned that into an
                        // empty string — the hook iterated zero refs and let the
                        // push through. Undecodable input is an error, not "".
                        if let Err(e) = std::io::stdin().read_to_string(&mut buf) {
                            return Err(MixError::RuntimeError {
                                span: None,
                                msg: format!("read_stdin: {}", e),
                            });
                        }
                        let v = Value::String(buf);
                        self.check_collection_size(&v)?; // Knob D (read_stdin can be huge)
                        return Ok(v);
                    }
                    // read_stdin_bytes — the binary twin (v0.65.0). Inline for
                    // the same two reasons read_stdin is: Knob A capability and
                    // the Knob D size check on an unbounded read.
                    if name == "read_stdin_bytes" {
                        self.check_capability(name)?; // Knob A
                        let v = builtins::read_stdin_bytes_impl(name, &eval_args)?;
                        self.check_collection_size(&v)?; // Knob D
                        return Ok(v);
                    }

                    // printf / eprintf — inline because they must write
                    // through self.globals.stdout / self.globals.stderr (which the test
                    // harness replaces with SharedBuf). The format engine
                    // itself lives in builtins::mix_format_public so `fmt`
                    // and `printf` share one grammar.
                    if name == "printf" || name == "eprintf" {
                        self.check_capability(name)?; // Knob A
                        if eval_args.is_empty() {
                            return Err(MixError::RuntimeError {
                                span: None,
                                msg: format!("{}() requires at least a template string", name),
                            });
                        }
                        let tmpl = eval_args[0].to_mix_string();
                        let formatted = builtins::mix_format_public(name, &tmpl, &eval_args[1..])?;
                        let mut g = self.globals.borrow_mut();
                        if name == "eprintf" {
                            write!(g.stderr, "{}", formatted).ok();
                        } else {
                            write!(g.stdout, "{}", formatted).ok();
                        }
                        return Ok(Value::Nil);
                    }

                    // write_stdout / write_stderr and their print_raw /
                    // eprint_raw spellings (v0.65.0) — ONE family, ONE
                    // contract, four names; the `_raw` pair are aliases, not
                    // variants, and if they ever diverge that is a bug.
                    //
                    // Inline for the same reason printf is: the bytes must go
                    // through globals.stdout/stderr. That is also what makes
                    // this fd 1/2 *as they are* — the sink is the process's
                    // own stdout, never a re-opened "/dev/stdout" path (which
                    // dies with EACCES the moment fd 1 belongs to another
                    // user, the bug that made Mix unusable as a sieve filter).
                    //
                    // Three deliberate differences from `print`: no trailing
                    // newline, no separator between arguments, and bytes/buffer
                    // written verbatim instead of as the `<bytes:N>`
                    // placeholder. A fourth is the error handling below.
                    if matches!(
                        name.as_str(),
                        "write_stdout" | "write_stderr" | "print_raw" | "eprint_raw"
                    ) {
                        self.check_capability(name)?; // Knob A
                        if eval_args.is_empty() {
                            return Err(MixError::RuntimeError {
                                span: None,
                                msg: format!("{name}() requires at least one value to write"),
                            });
                        }
                        let mut out: Vec<u8> = Vec::new();
                        for v in &eval_args {
                            builtins::append_output_bytes(v, &mut out);
                        }
                        let to_stderr = name == "write_stderr" || name == "eprint_raw";
                        let res = {
                            let mut g = self.globals.borrow_mut();
                            let sink: &mut dyn Write = if to_stderr {
                                &mut g.stderr
                            } else {
                                &mut g.stdout
                            };
                            // `write_all` loops over partial writes, so a short
                            // write is never silently a truncated one. The flush
                            // is not optional: the default stdout sink is
                            // line-buffered, and this family exists precisely to
                            // emit output with NO newline in it — without the
                            // flush a filter's bytes would sit in the buffer past
                            // the moment the consumer reads.
                            sink.write_all(&out).and_then(|()| sink.flush())
                        };
                        if let Err(e) = res {
                            // `print`/`printf` swallow this with `.ok()`. This
                            // family must not: its entire contract is that the
                            // named bytes reached the consumer, so a dropped
                            // write reported as success is the one failure mode
                            // it cannot have. Catchable and code-distinguished,
                            // so a pipeline filter can choose the quiet exit:
                            //   try
                            //     write_stdout($body)
                            //   catch $m, $e
                            //     if $e.code == "IO_BROKEN_PIPE" then exit(0) end
                            //     raise($e.code, $m)
                            //   end
                            let code = if e.kind() == std::io::ErrorKind::BrokenPipe {
                                "IO_BROKEN_PIPE"
                            } else {
                                "IO_WRITE_FAILED"
                            };
                            return Err(MixError::structured(code, format!("{name}: {e}")));
                        }
                        return Ok(Value::Nil);
                    }

                    // db_query / db_exec — delegate to the injected
                    // DbHandler (a host service: the in-process embed
                    // binds it to a scoped connection; an out-of-process
                    // worker would RPC it — the script is identical).
                    // Async + handler-delegating, so handled here among
                    // the inline builtins, before the sync dispatch.
                    if name == "db_query" || name == "db_exec" {
                        self.check_capability(name)?; // Knob A — CapabilityClass::Db
                        let sql = eval_args
                            .first()
                            .map(|v| v.to_mix_string())
                            .unwrap_or_default();
                        // Optional second arg: a List of positional bind
                        // params (?1, ?2, …). Absent / nil ⇒ no params.
                        let params: Vec<Value> = match eval_args.get(1) {
                            Some(Value::List(items)) => items.to_vec(),
                            None | Some(Value::Nil) => Vec::new(),
                            Some(other) => {
                                return Err(self.runtime_err(format!(
                                    "{name}: second argument must be a list of bind \
                                     params, got {}",
                                    other.type_name()
                                )));
                            }
                        };
                        // Clone-out before await (drop the Ref guard); the
                        // Rc keeps the handler alive across the yield.
                        let handler =
                            { self.globals.borrow().db_handler.clone() }.ok_or_else(|| {
                                self.snapshot_builtin_error(
                                    name,
                                    self.runtime_err(
                                        "database not available (no db handler registered)",
                                    ),
                                )
                            })?;
                        let fut = if name == "db_query" {
                            handler.query(&sql, &params)
                        } else {
                            handler.exec(&sql, &params)
                        };
                        let result = self
                            .await_with_class_c_yield(fut)
                            .await?
                            .map_err(|e| self.snapshot_builtin_error(name, e))?;
                        self.check_collection_size(&result)
                            .map_err(|e| self.snapshot_builtin_error(name, e))?; // Knob D — bound rows
                        return Ok(result);
                    }

                    // jmap — delegate to the injected JmapHandler (a host
                    // service: the in-process embed resolves it over HTTP
                    // to the vhost's maild upstream; an out-of-process
                    // worker would RPC it). Two forms (overloaded by the
                    // first arg's type):
                    //   jmap(method, args)  → single call; returns that
                    //     method's result map, RAISES on a method-level
                    //     `["error", …]` response (ergonomic sugar).
                    //   jmap([[method, args, callId], …]) → batch; returns
                    //     the methodResponses list verbatim (error triples
                    //     inline), so callers can chain ResultReferences.
                    // Both RAISE only on transport/protocol/arg failures.
                    // Async + handler-delegating, like db_query above.
                    if name == "jmap" {
                        self.check_capability(name)?; // Knob A — CapabilityClass::Jmap
                        // Parse the two forms. `single_method` is Some(name)
                        // for the sugar form (drives the unwrap below) and
                        // None for the batch form.
                        let (calls, single_method): (Vec<JmapCall>, Option<String>) =
                            match eval_args.first() {
                                Some(Value::String(method)) => {
                                    if eval_args.len() > 2 {
                                        return Err(self.runtime_err(format!(
                                            "jmap: single-call form takes (method, args); \
                                             got {} arguments",
                                            eval_args.len()
                                        )));
                                    }
                                    let args = match eval_args.get(1) {
                                        Some(Value::Map(_)) => eval_args[1].clone(),
                                        None | Some(Value::Nil) => Value::map(IndexMap::new()),
                                        Some(other) => {
                                            return Err(self.runtime_err(format!(
                                                "jmap: second argument must be an args map, \
                                                 got {}",
                                                other.type_name()
                                            )));
                                        }
                                    };
                                    (
                                        vec![JmapCall {
                                            method: method.clone(),
                                            args,
                                            call_id: "c0".to_string(),
                                        }],
                                        Some(method.clone()),
                                    )
                                }
                                Some(Value::List(items)) => {
                                    if eval_args.len() > 1 {
                                        return Err(self.runtime_err(format!(
                                            "jmap: batch form takes a single list of \
                                             calls; got {} arguments",
                                            eval_args.len()
                                        )));
                                    }
                                    let mut calls = Vec::with_capacity(items.len());
                                    for (i, item) in items.iter().enumerate() {
                                        let triple = match item {
                                            Value::List(t) if t.len() == 3 => t,
                                            _ => {
                                                return Err(self.runtime_err(format!(
                                                    "jmap: batch call {i} must be a \
                                                     3-element list [method, args, callId]"
                                                )));
                                            }
                                        };
                                        let method = match &triple[0] {
                                            Value::String(s) => s.clone(),
                                            o => {
                                                return Err(self.runtime_err(format!(
                                                    "jmap: batch call {i} method must be a \
                                                     string, got {}",
                                                    o.type_name()
                                                )));
                                            }
                                        };
                                        match &triple[1] {
                                            Value::Map(_) => {}
                                            o => {
                                                return Err(self.runtime_err(format!(
                                                    "jmap: batch call {i} args must be a \
                                                     map, got {}",
                                                    o.type_name()
                                                )));
                                            }
                                        }
                                        let call_id = match &triple[2] {
                                            Value::String(s) => s.clone(),
                                            o => {
                                                return Err(self.runtime_err(format!(
                                                    "jmap: batch call {i} callId must be a \
                                                     string, got {}",
                                                    o.type_name()
                                                )));
                                            }
                                        };
                                        calls.push(JmapCall {
                                            method,
                                            args: triple[1].clone(),
                                            call_id,
                                        });
                                    }
                                    (calls, None)
                                }
                                other => {
                                    return Err(self.runtime_err(format!(
                                        "jmap: first argument must be a method name \
                                         (string) or a list of [method, args, callId] \
                                         calls, got {}",
                                        other.map(|v| v.type_name()).unwrap_or("nil")
                                    )));
                                }
                            };
                        // Reject args carrying a value with no JSON form
                        // (function / bytes) before any host call — for a
                        // protocol builtin a silently-dropped arg is a
                        // footgun, not a convenience.
                        for (i, c) in calls.iter().enumerate() {
                            if let Some(bad) = jmap_unjsonable(&c.args) {
                                return Err(self.runtime_err(format!(
                                    "jmap: call {i} args contain a {bad} value, which has \
                                     no JSON representation (base64-encode bytes; functions \
                                     can't be sent)"
                                )));
                            }
                        }
                        // Clone-out before await (drop the Ref guard); the
                        // Rc keeps the handler alive across the yield.
                        let handler =
                            { self.globals.borrow().jmap_handler.clone() }.ok_or_else(|| {
                                self.snapshot_builtin_error(
                                    name,
                                    self.runtime_err(
                                        "jmap not available (no jmap handler registered)",
                                    ),
                                )
                            })?;
                        let fut = handler.request(&calls);
                        let responses = self
                            .await_with_class_c_yield(fut)
                            .await?
                            .map_err(|e| self.snapshot_builtin_error("jmap", e))?;
                        self.check_collection_size(&responses) // Knob D — bound rows
                            .map_err(|e| self.snapshot_builtin_error(name, e))?;
                        // Batch form: hand back the methodResponses verbatim.
                        let Some(method) = single_method else {
                            return Ok(responses);
                        };
                        // Single-call sugar: unwrap the one response, and
                        // raise on a method-level JMAP error.
                        let resp_list = match &responses {
                            Value::List(l) => l,
                            _ => {
                                return Err(self.runtime_err(
                                    "jmap: malformed response (methodResponses not a list)",
                                ));
                            }
                        };
                        // A single JMAP methodCall yields exactly one
                        // methodResponse (RFC 8620 §3.4) — anything else is
                        // a protocol violation, not a value to silently
                        // pick the first of.
                        if resp_list.len() != 1 {
                            return Err(self.runtime_err(format!(
                                "jmap: single call expected exactly one methodResponse, got {}",
                                resp_list.len()
                            )));
                        }
                        let first = &resp_list[0];
                        let triple = match first {
                            Value::List(t) if t.len() == 3 => t,
                            _ => {
                                return Err(
                                    self.runtime_err("jmap: malformed method response triple")
                                );
                            }
                        };
                        if matches!(&triple[0], Value::String(s) if s == "error") {
                            return Err(self.runtime_err(format!(
                                "jmap {method} error: {}",
                                triple[1].to_mix_string()
                            )));
                        }
                        return Ok(triple[1].clone());
                    }

                    // jmap_upload — the compose half of the JMAP seam.
                    // maild's `Email/set { create }` is blob-only, so a
                    // handler that sends mail must first upload the raw
                    // RFC822 message and reference the returned blobId.
                    // Delegates to the same injected JmapHandler (the
                    // in-process embed POSTs to the vhost's maild blob
                    // endpoint with the sealed-cookie Bearer). Gated by
                    // CapabilityClass::Jmap, like `jmap`.
                    //   jmap_upload(body[, content_type]) → blobId string
                    // `body` is a string (e.g. the RFC822 text) or raw
                    // bytes; `content_type` defaults to
                    // "application/octet-stream".
                    if name == "jmap_upload" {
                        self.check_capability(name)?; // Knob A — CapabilityClass::Jmap
                        let body_bytes: Vec<u8> = match eval_args.first() {
                            Some(Value::String(s)) => s.clone().into_bytes(),
                            Some(Value::Bytes(b)) => b.to_vec(),
                            Some(Value::Buffer(b)) => b.borrow().clone(),
                            Some(other) => {
                                return Err(self.runtime_err(format!(
                                    "jmap_upload: first argument must be a string or bytes \
                                     body, got {}",
                                    other.type_name()
                                )));
                            }
                            None => {
                                return Err(self.runtime_err(
                                    "jmap_upload: missing body argument \
                                     (jmap_upload(body, content_type))",
                                ));
                            }
                        };
                        let ctype = match eval_args.get(1) {
                            Some(Value::String(s)) => s.clone(),
                            None | Some(Value::Nil) => "application/octet-stream".to_string(),
                            Some(other) => {
                                return Err(self.runtime_err(format!(
                                    "jmap_upload: second argument must be a content-type \
                                     string, got {}",
                                    other.type_name()
                                )));
                            }
                        };
                        // Clone-out the handler Rc before await (drop the
                        // Ref guard); body_bytes + ctype are locals the
                        // future borrows across the yield.
                        let handler =
                            { self.globals.borrow().jmap_handler.clone() }.ok_or_else(|| {
                                self.snapshot_builtin_error(
                                    name,
                                    self.runtime_err(
                                        "jmap_upload not available (no jmap handler registered)",
                                    ),
                                )
                            })?;
                        let fut = handler.upload(&body_bytes, &ctype);
                        let blob = self
                            .await_with_class_c_yield(fut)
                            .await?
                            .map_err(|e| self.snapshot_builtin_error("jmap_upload", e))?;
                        return Ok(blob);
                    }

                    // bus_call — the delegated Bus control-plane seam. A web
                    // handler calls a maild verb under DELEGATED identity: the
                    // embedder (webd) bounds which verbs are reachable and
                    // injects the delegation envelope (actor/vhost/route) from
                    // trusted request state, so the script names no
                    // host/peer/actor and cannot reach an unlisted verb.
                    //   bus_call(verb, args) → reply value
                    if name == "bus_call" {
                        self.check_capability(name)?; // Knob A — CapabilityClass::Bus
                        let verb = match eval_args.first() {
                            Some(Value::String(s)) if !s.is_empty() => s.clone(),
                            Some(Value::String(_)) => {
                                return Err(self.runtime_err(
                                    "bus_call: verb (first argument) must be a non-empty string",
                                ));
                            }
                            _ => {
                                return Err(self.runtime_err(
                                    "bus_call: first argument must be a verb string \
                                     (bus_call(verb, args))",
                                ));
                            }
                        };
                        if eval_args.len() > 2 {
                            return Err(self.runtime_err(format!(
                                "bus_call takes (verb, args); got {} arguments",
                                eval_args.len()
                            )));
                        }
                        let call_args = match eval_args.get(1) {
                            Some(v @ Value::Map(_)) => v.clone(),
                            None | Some(Value::Nil) => Value::map(IndexMap::new()),
                            Some(other) => {
                                return Err(self.runtime_err(format!(
                                    "bus_call: second argument must be an args map, got {}",
                                    other.type_name()
                                )));
                            }
                        };
                        // Reject args with no JSON representation (functions/
                        // raw bytes) up front — the same footgun guard as jmap.
                        if let Some(bad) = jmap_unjsonable(&call_args) {
                            return Err(self.runtime_err(format!(
                                "bus_call: args contain a {bad} value, which has no JSON \
                                 representation (base64-encode bytes; functions can't be sent)"
                            )));
                        }
                        // Clone-out the handler Rc before await (drop the Ref
                        // guard); verb + call_args are locals the future
                        // borrows across the yield.
                        let handler =
                            { self.globals.borrow().bus_call_handler.clone() }.ok_or_else(
                                || {
                                    self.snapshot_builtin_error(name, self.runtime_err(
                                    "bus_call not available (no bus_call handler registered)",
                                ))
                                },
                            )?;
                        let fut = handler.call(&verb, &call_args);
                        let reply = self
                            .await_with_class_c_yield(fut)
                            .await?
                            .map_err(|e| self.snapshot_builtin_error("bus_call", e))?;
                        self.check_collection_size(&reply) // Knob D — bound rows
                            .map_err(|e| self.snapshot_builtin_error(name, e))?;
                        return Ok(reply);
                    }

                    // publish(topic, body[, opts]) — one-call topic publish
                    // (0.63.0): builds the SPEC-02 wire frame and routes it
                    // through `send noded topic.publish name=… retain=…
                    // body=…`. Removes the two recorded traps: the
                    // hand-built `---\n…\n---\n` frame, and `body=` being
                    // required before `name=`/`retain=` header-route.
                    // Sets $rc/$result exactly like `send`; returns rc.
                    if name == "publish" {
                        self.check_capability(name)?; // Knob A — CapabilityClass::Bus
                        return self.exec_publish(eval_args).await;
                    }

                    // HOF (eval) builtins — checked before pure builtins
                    // so a HOF entry can shadow a same-named pure stub
                    // during migration. In practice HOF names (sort_by,
                    // filter, __hof_smoke, …) don't collide with pure
                    // ones today; the order is set for forward safety.
                    if let Some(hof) = crate::builtins_hof::lookup(name) {
                        // Knob A: gate the HOF before it runs.
                        // (Strict arity is checked once at the top of the
                        // FunctionCall arm, covering every metadata builtin
                        // incl. HOFs.)
                        self.check_capability(name)?;
                        // Knob D: cap the produced collection (map/filter/
                        // sort_by/group_by/unique_by bypass the call_builtin
                        // result check below).
                        let result = hof(self, eval_args)
                            .await
                            .map_err(|e| self.snapshot_builtin_error(name, e))?;
                        self.check_collection_size(&result)
                            .map_err(|e| self.snapshot_builtin_error(name, e))?;
                        return Ok(result);
                    }

                    // Try builtins first. Gate the probe with `is_builtin` so
                    // user-defined function calls (the hot path for recursion-
                    // heavy programs like fib) skip the eval_args.clone() that
                    // would otherwise happen on every call. is_builtin is a
                    // pattern match — cheaper than a clone of even a one-arg
                    // Vec<Value>.
                    if builtins::is_builtin(name) {
                        // Knob A: gate the builtin before it runs (before the
                        // arg clone / dispatch). A denied call errors; a
                        // non-match (call_builtin -> None) falls through to
                        // the special-form checks below, as before.
                        self.check_capability(name)?;
                        // (Strict arity checked at the top of this arm.)
                        // Conditional capabilities (0.29.0): an option can
                        // add a capability class on top of the builtin's
                        // base class (http_get's ca_file needs fs-read).
                        // Metadata-driven from the contract's cond_caps;
                        // gated on policy presence so the unsandboxed hot
                        // path pays nothing.
                        if self.globals.borrow().capability_policy.is_some() {
                            for cc in builtins::conditional_capabilities(name) {
                                if builtins::conditional_cap_engaged(name, &eval_args, cc.option) {
                                    self.check_capability_class(cc.capability, name)?;
                                }
                            }
                        }
                        // `buffer_push` grows its buffer argument in place (the
                        // buffer is Rc-shared with `eval_args[0]`, so the growth
                        // is visible here) and returns nil, so the return-value
                        // guard below can't cap it. Record the pre-push length so
                        // an over-cap growth can be ROLLED BACK — a caught error
                        // must not leave an oversized buffer a `try/catch` loop
                        // could keep growing. Only in a limited (sandbox) context.
                        let buffer_push_pre_len =
                            if name == "buffer_push" && self.ctx.max_string_len.is_some() {
                                match eval_args.first() {
                                    Some(Value::Buffer(b)) => Some(b.borrow().len()),
                                    _ => None,
                                }
                            } else {
                                None
                            };
                        if let Some(result) = builtins::call_builtin(name, eval_args.clone())
                            .map_err(|e| self.snapshot_builtin_error(name, e))?
                        {
                            // Knob D: cap the produced collection (covers
                            // every collection-returning pure builtin —
                            // split/range/repeat/flat/keys/values/json_parse/…
                            // — in one place).
                            self.check_collection_size(&result)
                                .map_err(|e| self.snapshot_builtin_error(name, e))?;
                            if let (Some(pre_len), Some(max)) =
                                (buffer_push_pre_len, self.ctx.max_string_len)
                                && let Some(Value::Buffer(b)) = eval_args.first()
                            {
                                let grown = b.borrow().len();
                                if grown > max {
                                    // Roll the in-place growth back before
                                    // surfacing the error, so the cap holds
                                    // even when the error is caught.
                                    b.borrow_mut().truncate(pre_len);
                                    return Err(self.snapshot_builtin_error(
                                        name,
                                        self.runtime_err(format!(
                                            "buffer length {} exceeds limit {}",
                                            grown, max
                                        )),
                                    ));
                                }
                            }
                            return Ok(result);
                        }
                    }

                    // port_exists() — delegates to Bus handler
                    if name == "port_exists" {
                        // SPEC 18 Phase 2 WS3-C.5 — clone-out: scope the
                        // `Ref` guard so it is dropped before `.await`;
                        // the `Rc<dyn BusHandler>` keeps the handler
                        // alive across the yield point.
                        let handler =
                            { self.globals.borrow().bus_handler.clone() }.ok_or_else(|| {
                                MixError::RuntimeError {
                                    span: None,
                                    msg: "Bus not available (no handler registered)".to_string(),
                                }
                            })?;
                        let target = eval_args
                            .first()
                            .map(|v| v.to_mix_string())
                            .unwrap_or_default();
                        // SPEC 18 Phase 2 WS3-C.7e.4 — release the Class C
                        // reader permit across the wire roundtrip so peer
                        // handlers can interleave. No-op fast path when no
                        // permit is held (init / Class S / embedded-plain).
                        let fut = handler.port_exists(&target);
                        let exists = self.await_with_class_c_yield(fut).await??;
                        return Ok(Value::Bool(exists));
                    }

                    // bus_reconnect() — drop the cached broker client so
                    // the next send/emit re-dials (SPEC 18 §3.3
                    // acceptance affordance for a driver that induces a
                    // `cosmix-noded` bounce). DELIBERATELY does NOT
                    // hard-error when no handler is registered, unlike
                    // the siblings below: "no Bus wired" means "nothing
                    // cached to drop", which IS the requested
                    // post-condition (and the next send would itself
                    // surface "Bus not available"). Mirrors the trait's
                    // own per-method stance — reconnect's default is
                    // Ok(()), subscribe/reply's is Err — so this is not
                    // an inconsistency. Idempotent; always returns true.
                    if name == "bus_reconnect" {
                        // SPEC 18 Phase 2 WS3-C.5 — clone-out: only call
                        // when wired (idempotent no-op otherwise), and
                        // release the globals borrow before `.await`.
                        if let Some(handler) = { self.globals.borrow().bus_handler.clone() } {
                            // SPEC 18 Phase 2 WS3-C.7e.4 — wrap for the
                            // invariant: every handler-body reachable
                            // BusHandler await releases the Class C reader
                            // permit. `reconnect`'s production impl is
                            // currently local (cache teardown), but the
                            // discipline is uniform across the trait so
                            // future async work does not become a hazard.
                            let fut = handler.reconnect();
                            self.await_with_class_c_yield(fut).await??;
                        }
                        return Ok(Value::Bool(true));
                    }

                    // noded_register(name) — register as a named service on the broker
                    if name == "noded_register" {
                        // SPEC 18 Phase 2 WS3-C.5 — clone-out (see port_exists above).
                        let handler =
                            { self.globals.borrow().bus_handler.clone() }.ok_or_else(|| {
                                MixError::RuntimeError {
                                    span: None,
                                    msg: "Bus not available (no handler registered)".to_string(),
                                }
                            })?;
                        let svc_name = eval_args
                            .first()
                            .map(|v| v.to_mix_string())
                            .unwrap_or_default();
                        // SPEC 18 Phase 2 WS3-C.7e.4 — release the Class C
                        // reader permit across the broker register
                        // roundtrip (see port_exists above).
                        let fut = handler.register_as(&svc_name);
                        self.await_with_class_c_yield(fut).await??;
                        return Ok(Value::Bool(true));
                    }

                    // quit() — request graceful shutdown of the serve
                    // event pump (SPEC 18 §3.5). Sets the flag that
                    // `run_event_pump` checks at the top of its loop; the
                    // pump then breaks with reason `"quit"`, the serve
                    // entrypoint deregisters from the broker and exits 0.
                    // Zero-arg (any extra args are ignored — there is no
                    // exit-code channel: graceful serve shutdown is always
                    // exit 0, matching the Ch02 `QUIT` universal). Returns
                    // nil. For a synchronous handler or the init body the
                    // break is immediate; the statements after `quit()` in
                    // the same handler still run, then the pump stops.
                    if name == "quit" {
                        let g = self.globals.borrow();
                        g.quit_requested.store(true, Ordering::Relaxed);
                        // Wake a pump parked in `next_incoming().await` —
                        // essential when `quit()` is called from a Class C
                        // (spawned-task) handler, where the flag store alone
                        // would not be observed until the next inbound
                        // message. `notify_one` stores a permit if the pump
                        // is momentarily elsewhere, so the wake is not lost.
                        g.quit_notify.notify_one();
                        return Ok(Value::Nil);
                    }

                    // subscribe(name) / unsubscribe(name) — Ch03 topic
                    // (un)subscription (SPEC 18 WS2). One chokepoint for
                    // both init-body and handler-body callers: the
                    // supervised client records/forgets the topic
                    // transactionally for §3.3 reconnect replay. A
                    // missing/empty name is a hard error — a silently
                    // dropped subscribe is the partial-truth bug.
                    if name == "subscribe" || name == "unsubscribe" {
                        // Validate the argument before consulting the handler:
                        // an empty topic is a caller bug regardless of whether
                        // Bus is wired, and validating first keeps the error
                        // deterministic (handler-present vs absent).
                        let topic = eval_args
                            .first()
                            .map(|v| v.to_mix_string())
                            .unwrap_or_default();
                        if topic.is_empty() {
                            return Err(MixError::RuntimeError {
                                span: None,
                                msg: format!("{name}() requires a non-empty topic name"),
                            });
                        }
                        // SPEC 18 Phase 2 WS3-C.5 — clone-out (see port_exists above).
                        let handler =
                            { self.globals.borrow().bus_handler.clone() }.ok_or_else(|| {
                                MixError::RuntimeError {
                                    span: None,
                                    msg: "Bus not available (no handler registered)".to_string(),
                                }
                            })?;
                        // SPEC 18 Phase 2 WS3-C.7e.4 — release the Class C
                        // reader permit across the supervised-client
                        // (un)subscribe roundtrip so peer handlers can
                        // interleave during the broker hop.
                        if name == "subscribe" {
                            let fut = handler.subscribe_topic(&topic);
                            self.await_with_class_c_yield(fut).await??;
                        } else {
                            let fut = handler.unsubscribe_topic(&topic);
                            self.await_with_class_c_yield(fut).await??;
                        }
                        return Ok(Value::Bool(true));
                    }

                    // reply(body) / reply(rc, body) — answer the request
                    // the current `on` handler is servicing (SPEC 18
                    // WS-R). Correlation is not invented here: the
                    // evaluator captured from/command/id/type off the
                    // in-flight event in `run_handlers_for`; this only
                    // transmits via the one `BusHandler` chokepoint. A
                    // dropped reply is the silent-no-op partial-truth bug
                    // in its acute form (requester blocks forever), so
                    // every failure path is a hard error. Argument
                    // validation runs before the in-handler and
                    // Bus-present checks so the error is deterministic
                    // regardless of dispatch state or Bus wiring (same
                    // discipline as subscribe()/unsubscribe()).
                    if name == "reply" {
                        let (rc, body) = match eval_args.len() {
                            1 => (0u8, eval_args[0].to_mix_string()),
                            2 => {
                                let rc = match &eval_args[0] {
                                    Value::Number(n)
                                        if n.fract() == 0.0 && (0.0..=255.0).contains(n) =>
                                    {
                                        *n as u8
                                    }
                                    _ => {
                                        return Err(MixError::RuntimeError {
                                            span: None,
                                            msg: "reply(rc, body): rc must be an integer \
                                                  in 0..=255"
                                                .to_string(),
                                        });
                                    }
                                };
                                (rc, eval_args[1].to_mix_string())
                            }
                            _ => {
                                return Err(MixError::RuntimeError {
                                    span: None,
                                    msg: "reply() takes (body) or (rc, body)".to_string(),
                                });
                            }
                        };
                        // SPEC 18 Phase 2 WS3-C.7c — route through the
                        // registered `InvocationReplyHandle`. Cloning
                        // the `Rc` out releases any borrow on
                        // `self.current_reply` before `.await`, and
                        // `reply_once` carries the snapshotted handler
                        // + answered-once CAS internally (the answered
                        // bookkeeping that the §3.4 fault boundary
                        // consults now lives on the handle itself, so
                        // no separate post-await write is needed).
                        let handle = match self.current_reply.as_ref() {
                            None => {
                                return Err(MixError::RuntimeError {
                                    span: None,
                                    msg: "reply() can only be called from within an \
                                          `on` handler"
                                        .to_string(),
                                });
                            }
                            Some(h) if !h.is_request => {
                                return Err(MixError::RuntimeError {
                                    span: None,
                                    msg: "reply() can only answer a request (the current \
                                          event is not type=request — a topic delivery \
                                          has no caller to answer)"
                                        .to_string(),
                                });
                            }
                            // No `from`-presence gate: the response is
                            // correlated back to the caller by `id`
                            // (noded's pending_responses), and noded
                            // strips `from` for anonymous callers (the
                            // normal CLI / MCP-bridge case) as its
                            // SPEC 12 §15.5 impersonation defense. An
                            // empty `from` just yields an empty response
                            // `to`, exactly as the Rust
                            // NodedClient::respond_parts path does — it
                            // is not a reason to refuse the reply.
                            Some(h) => Rc::clone(h),
                        };
                        // SPEC 18 Phase 2 WS3-C.7e — yield-on-reply.
                        // `reply_once` ends in `handler.reply(...).await`
                        // on the wire; wrap with `await_with_class_c_yield`
                        // so a Class C async body releases its reader
                        // permit across the reply send. The handle is an
                        // owned `Rc` clone (no `self` borrow), and `body`
                        // is a local owned String — no `RefCell` borrow
                        // is held across the helper.
                        let reply_fut = handle.reply_once(rc, &body);
                        self.await_with_class_c_yield(reply_fut).await??;
                        return Ok(Value::Bool(true));
                    }

                    // Try extension functions
                    // SPEC 18 Phase 2 WS3-C.5 — clone the `Rc<dyn Fn>`
                    // out with the `Ref` guard tightly scoped so it is
                    // dropped before the extension future is awaited.
                    // ExtFn is `Rc<dyn Fn(...)>` so `.cloned()` bumps the
                    // refcount without invoking the closure.
                    if let Some(ext_fn) = { self.globals.borrow().extensions.get(name).cloned() } {
                        // SPEC 18 Phase 2 WS3-C.7e.4 — extension futures
                        // are opaque plugin code; they must not retain the
                        // Class C reader permit while awaiting. The
                        // contract is: extension futures may suspend
                        // freely, but the evaluator does not guarantee
                        // they hold the Class C read permit across
                        // suspension points (a peer Class S writer may
                        // admit, or another Class C reader may overlap,
                        // while the extension is parked on its own IO).
                        let fut = ext_fn(eval_args);
                        return self.await_with_class_c_yield(fut).await?;
                    }

                    // require() Card 3 — Function-valued variable dispatch,
                    // checked after extensions (so extensions keep their
                    // precedence) and before the address-send guess / the
                    // caller's function registry:
                    //   1. A Function-valued local in the ACTIVE function
                    //      frame (module_env / captures / params are frame-
                    //      injected) wins over the caller's registry — this
                    //      is what lets a module function call its siblings
                    //      and `_`-privates bareword regardless of the
                    //      requiring program's function names. Top-level
                    //      dispatch is unchanged (None outside a function).
                    //   2. Otherwise, when no named function exists, a
                    //      visible Function-valued variable (`$b = function
                    //      … end; b()`) is callable bareword — fires only
                    //      where dispatch previously errored or guessed an
                    //      address-send.
                    let func_var = match self.scope.get_function_var_in_function_frames(name) {
                        Some(f) => Some(f),
                        None if !self.scope.has_function(name) => {
                            match self.scope.get_value(name) {
                                Some(sv) => match &*sv {
                                    Value::Function(rc) => Some(Rc::clone(rc)),
                                    _ => None,
                                },
                                None => None,
                            }
                        }
                        None => None,
                    };
                    if let Some(func) = func_var {
                        self.track_function_attempt(name);
                        return self.call_function(&func, &eval_args).await;
                    }

                    // If inside an address block, unknown functions become sends.
                    // SPEC 18 Phase 2 WS3-C.7b — γ-aware `has_function` walks
                    // the shared function registry; the legacy
                    // `get_function().is_none()` was true after promotion
                    // (which moved the map behind the shared handle) and
                    // would have rerouted every user function call through
                    // the address-block path.
                    if !self.ctx.address_stack.is_empty()
                        && !self.scope.has_function(name)
                        && !self.globals.borrow().extensions.contains_key(name)
                    {
                        return self.address_block_send(name, eval_args).await;
                    }

                    // Try user-defined functions. SPEC 18 Phase 2 WS3-C.7b
                    // — `get_function_owned` is γ-aware and returns the
                    // `Rc<MixFunction>` either from the legacy in-frame
                    // map or from the shared function registry after
                    // promotion.
                    let func = self.scope.get_function_owned(name).ok_or_else(|| {
                        MixError::structured(
                            "FUNCTION_UNDEFINED",
                            format!("undefined function '{}'", name),
                        )
                    })?;

                    self.track_function_attempt(name);
                    self.call_function(&func, &eval_args).await
                }

                Expr::Index { object, index } => {
                    let idx = self.eval_expr(index).await?;
                    // Fast path: when the object is a plain `$var`, look up
                    // the binding by reference instead of evaluating it to
                    // an owned Value (which deep-clones a Map/List). The
                    // BENCH_TABLE read loop hits this 6000× per iteration
                    // — without this the entire 6000-entry IndexMap is
                    // cloned on every `$t[$k]` access.
                    // SPEC 18 Phase 2 WS3-C.7b — γ-aware fast-path read.
                    // The bound `obj_sv` carries a `ScopeValue::{Borrowed,
                    // Owned}` whose `Deref<Target=Value>` impl gives us the
                    // `&Value` the match arms expect. Borrowed avoids the
                    // 6000-entry IndexMap clone in the BENCH_TABLE loop;
                    // Owned (post-promotion shared global) is the safe
                    // path that doesn't outlive the borrow on the shared
                    // RefCell.
                    if let Expr::Variable(name) = &**object
                        && let Some(obj_sv) = self.scope.get_value(name)
                    {
                        let obj_ref: &Value = &obj_sv;
                        return Self::index_value_clone(obj_ref, &idx);
                    }
                    // variable not found — fall through so eval_expr emits
                    // the canonical "undefined variable" error (or Nil for
                    // positional $1/$2 names).
                    let obj = self.eval_expr(object).await?;
                    Self::index_value_clone(&obj, &idx)
                }

                Expr::FieldAccess { object, field } => {
                    let obj = self.eval_expr(object).await?;
                    match &obj {
                        Value::Map(map) => Ok(map
                            .get(field)
                            .or_else(|| map.get("*"))
                            .cloned()
                            .unwrap_or(Value::Nil)),
                        _ => Err(MixError::RuntimeError {
                            span: None,
                            msg: format!("cannot access field '{}' on {}", field, obj.type_name()),
                        }),
                    }
                }

                Expr::ListLiteral(items) => {
                    let mut vals = Vec::new();
                    for item in items {
                        vals.push(self.eval_expr(item).await?);
                    }
                    let v = Value::list(vals);
                    self.check_collection_size(&v)?; // Knob D
                    Ok(v)
                }

                Expr::MapLiteral(entries) => {
                    let mut map = IndexMap::new();
                    for (key, val_expr) in entries {
                        let val = self.eval_expr(val_expr).await?;
                        map.insert(key.clone(), val);
                    }
                    let v = Value::map(map);
                    self.check_collection_size(&v)?; // Knob D
                    Ok(v)
                }

                Expr::Send {
                    target,
                    command,
                    args,
                } => {
                    self.track_keyword_attempt("send");
                    self.exec_send(target, command, args).await
                }

                Expr::Sh(command) => {
                    self.track_keyword_attempt("sh");
                    let cmd = self.eval_expr(command).await?.to_mix_string();
                    self.exec_sh_expr(&cmd)
                }

                Expr::CommandSub(cmd) => self.exec_sh_expr(cmd),

                Expr::FunctionLiteral { params, body } => {
                    self.track_keyword_attempt("function");
                    // Capture-by-value only inside function frames.
                    // At function_depth == 0 the lambda will see globals
                    // live via the normal frame-isolation path; snapshotting
                    // there is redundant and would freeze subsequent global
                    // updates. At depth > 0 the inner function frame becomes
                    // unreachable after return, so capture is the only way
                    // to preserve its locals.
                    let captures = if self.ctx.function_depth > 0 {
                        // Free-variable capture: snapshot only the frame
                        // entries this lambda actually references, not the
                        // whole frame. A large in-scope value (e.g. a
                        // multi-thousand-event score held by the enclosing
                        // function) would otherwise be deep-cloned on
                        // *every* call to this lambda, turning a
                        // `sort_by`/`map`/`filter` over N items into an
                        // O(N · frame-data) blow-up (the acid-track
                        // `sort_by` hang). Falls back to a full-frame
                        // snapshot when the body can load code dynamically
                        // (`collect_free_vars` → None), preserving the old
                        // behaviour for those rare cases.
                        let snap = match Self::collect_free_vars(params, body) {
                            Some(free) => self.scope.snapshot_frame_vars(&free),
                            None => {
                                let mut snap: HashMap<String, Value> = HashMap::new();
                                for (k, v) in self.scope.current_frame_entries() {
                                    snap.insert(k, v);
                                }
                                snap
                            }
                        };
                        // Keep `Some(snap)` even when the snapshot is empty.
                        // At function_depth > 0 the lambda MUST stay isolated
                        // — it may see only its own params plus globals, never
                        // the caller's live locals. Collapsing an empty
                        // capture to `None` would re-enable the frameless sync
                        // fast path (which bails whenever `captures.is_some()`
                        // and does NOT push an isolated frame), so a
                        // non-param read would resolve against the *dynamic*
                        // caller frame: `fn() = $G` called inside a function
                        // holding a local `$G` would read the local, not the
                        // global — a dynamic-scope leak. `Some({})`'s
                        // per-call injection loop is a no-op, so the
                        // O(1)-capture win is unaffected either way.
                        Some(snap)
                    } else {
                        None
                    };
                    let sync_prefix_len = Self::compute_sync_prefix(body);
                    let block_sync_self = Self::compute_block_sync_self("<lambda>", params, body);
                    let body_no_local_assigns = !Self::body_has_local_assigns(body);
                    Ok(Value::Function(Rc::new(MixFunction {
                        name: std::rc::Rc::from("<lambda>"),
                        params: params.clone(),
                        body: (**body).clone(),
                        def_file: self.ctx.current_file.clone(),
                        captures,
                        sync_prefix_len,
                        block_sync_self,
                        body_no_local_assigns,
                        fib_bytecode: None,
                        module_env: None,
                    })))
                }

                Expr::ValueCall { callee, args } => {
                    let callee_val = self.eval_expr(callee).await?;
                    let func = match &callee_val {
                        Value::Function(rc) => Rc::clone(rc),
                        other => {
                            return Err(MixError::RuntimeError {
                                span: None,
                                msg: format!(
                                    "cannot call value of type '{}' as a function",
                                    other.type_name()
                                ),
                            });
                        }
                    };
                    let mut eval_args = Vec::new();
                    for arg in args {
                        eval_args.push(self.eval_expr(arg).await?);
                    }
                    self.call_function(&func, &eval_args).await
                }

                // Method-syntax call whose name is NOT a builtin / HOF /
                // inline special form (those keep the legacy UFCS desugar
                // at parse time). Dispatch order preserves every call
                // that works today — object evaluated first, then args
                // left-to-right, exactly like the old desugar's arg loop:
                //   1. extensions (UFCS: object is first arg);
                //   2. user functions via the registry (UFCS — the
                //      "global beats member" precedence is pinned);
                //   3. inside an `address` block, an unknown name is
                //      still a send (use `$m["f"](…)` for members there);
                //   4. NEW: a map member holding a Function is called
                //      with the plain args (no implicit object) — the
                //      require() exports calling convention;
                //   5. else the undefined-function error, enriched.
                Expr::MethodCall {
                    object,
                    field,
                    args,
                } => {
                    let object_val = self.eval_expr(object).await?;
                    let mut ufcs_args = Vec::with_capacity(args.len() + 1);
                    ufcs_args.push(object_val);
                    for arg in args {
                        if let Some(r) = self.try_eval_expr_sync(arg) {
                            ufcs_args.push(r?);
                        } else {
                            ufcs_args.push(self.eval_expr(arg).await?);
                        }
                    }

                    // 1. Extensions — same clone-out + yield discipline
                    // as the FunctionCall arm.
                    if let Some(ext_fn) = { self.globals.borrow().extensions.get(field).cloned() } {
                        let fut = ext_fn(ufcs_args);
                        return self.await_with_class_c_yield(fut).await?;
                    }

                    // 2. Map member holding a Function — the OBJECT's own
                    // function beats a registry (user/prelude) function of
                    // the same name (v0.72.0; before that UFCS won, which
                    // made a module export named `sum` or `lines` silently
                    // unreachable via dot-call — the prelude function ran
                    // on the stringified map instead). Extensions stay
                    // ABOVE members on purpose: serve-mode injects reserved
                    // verbs (props.*) as extensions, and a citizen's map
                    // must never fake those. Builtin-named members remain
                    // unreachable via dot-call — `.name(` where name is a
                    // builtin desugars to a plain FunctionCall at PARSE
                    // time; index access is still the spelling for those.
                    if let Value::Map(map) = &ufcs_args[0]
                        && let Some(Value::Function(rc)) = map.get(field)
                    {
                        let func = Rc::clone(rc);
                        self.track_function_attempt(field);
                        return self.call_function(&func, &ufcs_args[1..]).await;
                    }

                    // 3. User functions (UFCS).
                    if let Some(func) = self.scope.get_function_owned(field) {
                        self.track_function_attempt(field);
                        return self.call_function(&func, &ufcs_args).await;
                    }

                    // 4. Address block: bit-for-bit with the old desugar —
                    // the object rides along as `_0`.
                    if !self.ctx.address_stack.is_empty() {
                        return self.address_block_send(field, ufcs_args).await;
                    }

                    // 5. Enriched undefined error.
                    Err(MixError::structured(
                        "FUNCTION_UNDEFINED",
                        format!(
                            "undefined function '{}' (and the object has no \
                             function-valued member '{}')",
                            field, field
                        ),
                    ))
                }

                // Ternary `cond ? a : b` — short-circuit: evaluate the
                // condition, then ONLY the taken branch. Mirrors the
                // And/Or/NilCoalesce discipline above.
                Expr::Ternary {
                    cond,
                    then_branch,
                    else_branch,
                } => {
                    let c = self.eval_expr(cond).await?;
                    if c.is_truthy() {
                        self.eval_expr(then_branch).await
                    } else {
                        self.eval_expr(else_branch).await
                    }
                }

                // Expression-position `if` — same execution as
                // `StmtKind::If`: evaluate the condition, run the taken
                // branch as a block, and yield its value (the last
                // statement's value, or nil if empty / no branch matched).
                Expr::If(ifx) => {
                    self.track_keyword_attempt("if");
                    let cond = self.eval_expr(&ifx.condition).await?;
                    if cond.is_truthy() {
                        return self.execute_block(&ifx.then_body).await;
                    }
                    for (eif_cond, eif_body) in &ifx.else_ifs {
                        let c = self.eval_expr(eif_cond).await?;
                        if c.is_truthy() {
                            return self.execute_block(eif_body).await;
                        }
                    }
                    if let Some(eb) = &ifx.else_body {
                        return self.execute_block(eb).await;
                    }
                    Ok(Value::Nil)
                }
            }
        })
    }

    #[inline]
    fn eval_binop(&self, op: &BinOp, left: &mut Value, right: &Value) -> MixResult<Value> {
        // Hot path: Number op Number for the four common arithmetic ops.
        // Skips the function-pointer indirection of `num_op` and the
        // double `to_number` match. Loop-heavy benches hammer this
        // (loop_arith does `$sum + $i * 2 - 1` per iteration).
        if let (Value::Number(l), Value::Number(r)) = (&*left, right) {
            match op {
                BinOp::Add => return Ok(Value::Number(l + r)),
                BinOp::Sub => return Ok(Value::Number(l - r)),
                BinOp::Mul => return Ok(Value::Number(l * r)),
                BinOp::Lt => return Ok(Value::Bool(l < r)),
                BinOp::Gt => return Ok(Value::Bool(l > r)),
                BinOp::LtEq => return Ok(Value::Bool(l <= r)),
                BinOp::GtEq => return Ok(Value::Bool(l >= r)),
                BinOp::Eq => return Ok(Value::Bool(l == r)),
                BinOp::NotEq => return Ok(Value::Bool(l != r)),
                _ => {}
            }
        }
        match op {
            BinOp::Add => {
                // If both can be numbers, do arithmetic; otherwise concat
                if let (Some(l), Some(r)) = (left.to_number(), right.to_number()) {
                    Ok(Value::Number(l + r))
                } else {
                    // Knob D: the `+` string fallback grows a string too.
                    let v =
                        Value::String(format!("{}{}", left.to_mix_string(), right.to_mix_string()));
                    self.check_collection_size(&v)?;
                    Ok(v)
                }
            }
            BinOp::Sub => num_op(left, right, |a, b| a - b),
            BinOp::Mul => num_op(left, right, |a, b| a * b),
            BinOp::Div => {
                let r = number_operand(right)?;
                if r == 0.0 {
                    return Err(MixError::RuntimeError {
                        span: None,
                        msg: "division by zero".to_string(),
                    });
                }
                let l = number_operand(left)?;
                Ok(Value::Number(l / r))
            }
            BinOp::Mod => {
                let r = number_operand(right)?;
                if r == 0.0 {
                    return Err(MixError::RuntimeError {
                        span: None,
                        msg: "modulo by zero".to_string(),
                    });
                }
                let l = number_operand(left)?;
                Ok(Value::Number(l % r))
            }
            BinOp::Power => num_op(left, right, |a, b| a.powf(b)),
            // Map/List `==`/`!=` RAISES (v0.68.0, Release A.1) rather than
            // silently answering false/true. `Value`'s `PartialEq` has no
            // structural arm, so `[1,2] == [1,2]` was `false` and
            // `[1,2] != [1,2]` was `true` — an answer that looks like a
            // comparison and is really a constant. `deep_eq(a, b)` (0.63.0)
            // is the structural comparison, and the diagnostic names it.
            //
            // SCOPED TO THE OPERATORS ONLY, deliberately. `PartialEq` itself
            // is untouched, so every INTERNAL comparison keeps working:
            // `contains`, `index_of`, `unique`'s dedup, map key lookup, and
            // `deep_eq`'s own leaf compares all call `==` on `Value` in Rust
            // and never come through `eval_binop`. Making PartialEq raise
            // would have been unimplementable anyway — it returns `bool`.
            //
            // BOTH operands must be collections — a DELIBERATE narrowing of
            // the MIX-D3007 note's "either operand" test, and the narrowing is
            // the whole safety of this flip.
            //
            // Two reasons, and note which one is NOT being claimed:
            // "constant regardless of contents" is true of `[1] == nil` too,
            // so it does not by itself draw the line. What draws it is
            // (i) a SAME-TYPE `false` MISLEADS — it invites the reading "these
            // two lists differ", which is not what was computed — whereas a
            // type-mismatch `false` is truthful, exactly as `1 == "a"` is
            // honestly false; and (ii) `$map[$key] == nil` is THE key-absence
            // idiom:
            // 3,856 `== nil`/`!= nil` sites across 381 local .mix files, 1,059
            // of them the indexed shape whose value is a list or map whenever
            // the key happens to be populated. Raising there would have broken
            // them data-dependently — the worst failure mode available, since
            // the same line works until the map is non-empty.
            //
            // That case is not hypothetical: it broke this repo's own
            // `docs/build/gen-builtins-md.mix:70` the first time the wider
            // rule ran. The static D3007 inventory could never have caught it
            // — the note only fired on PROVABLE collection operands, as its
            // own comment admitted — so "zero D3007 sites" was evidence for
            // this narrow scope, never for the wide one.
            //
            // Bytes and Buffer are excluded throughout: they have real content
            // equality (see value.rs), so `==` on them was never a constant.
            BinOp::Eq | BinOp::NotEq
                if matches!(&*left, Value::List(_) | Value::Map(_))
                    && matches!(right, Value::List(_) | Value::Map(_)) =>
            {
                let op_str = if matches!(op, BinOp::Eq) { "==" } else { "!=" };
                Err(MixError::structured(
                    "TYPE_ERROR",
                    format!(
                        "`{op_str}` is not defined for {} and {} — it would always answer {}, \
                         not compare them. Use deep_eq(a, b) for structural comparison",
                        left.type_name(),
                        right.type_name(),
                        if matches!(op, BinOp::Eq) {
                            "false"
                        } else {
                            "true"
                        }
                    ),
                ))
            }
            BinOp::Eq => Ok(Value::Bool(&*left == right)),
            BinOp::NotEq => Ok(Value::Bool(&*left != right)),
            BinOp::Gt => num_cmp(
                left,
                right,
                |a, b| a > b,
                |o| o == std::cmp::Ordering::Greater,
            ),
            BinOp::Lt => num_cmp(left, right, |a, b| a < b, |o| o == std::cmp::Ordering::Less),
            BinOp::GtEq => num_cmp(
                left,
                right,
                |a, b| a >= b,
                |o| o != std::cmp::Ordering::Less,
            ),
            BinOp::LtEq => num_cmp(
                left,
                right,
                |a, b| a <= b,
                |o| o != std::cmp::Ordering::Greater,
            ),
            BinOp::StrEq => Ok(Value::Bool(left.to_mix_string() == right.to_mix_string())),
            BinOp::StrNe => Ok(Value::Bool(left.to_mix_string() != right.to_mix_string())),
            BinOp::Concat => {
                // Reuse the left operand's String buffer (it's an owned temp
                // here) instead of format!'s two fresh allocations. mem::take
                // leaves String("") behind; left is discarded after this. NOTE:
                // this does NOT fix the `$s = $s .. rhs` accumulator (evaluating
                // the Variable `$s` already cloned it upstream) — the assignment
                // fast path handles that; this trims ordinary `$a .. $b`.
                let mut result = match left {
                    Value::String(s) => std::mem::take(s),
                    other => other.to_mix_string(),
                };
                right.write_mix(&mut result);
                // Knob D: cap the concatenated string (the `..` operator
                // is the main string-growth vector in a loop).
                let v = Value::String(result);
                self.check_collection_size(&v)?;
                Ok(v)
            }
            // And, Or, NilCoalesce handled in eval_expr for short-circuiting
            BinOp::And | BinOp::Or | BinOp::NilCoalesce => unreachable!(),
            BinOp::Pipe => {
                // Pipe: result of left becomes... for now just return right
                Ok(right.clone())
            }
        }
    }

    /// Public entry point for HOF builtins (builtins_hof.rs) that
    /// need to call a `Value::Function` captured from their args.
    /// Thin wrapper around the private `call_function` — kept `pub`
    /// so the HOF registry in a sibling module can reach it without
    /// widening the private API surface beyond this one method.
    pub fn call_function_public<'a>(
        &'a mut self,
        func: &'a MixFunction,
        args: &'a [Value],
    ) -> Pin<Box<dyn Future<Output = MixResult<Value>> + 'a>> {
        self.call_function(func, args)
    }

    /// Inline user-function call from BinaryOp position. Returns `None`
    /// when `expr` isn't a FunctionCall to a non-shadowed user function;
    /// the caller then falls back to `eval_expr` which handles all the
    /// other cases (HOFs, builtins, address sends, …) uniformly.
    ///
    /// Hot in fib's `fib($n-1) + fib($n-2)` — both binop operands are
    /// FunctionCall and previously each went through `eval_expr`'s
    /// outer Box::pin per recursive call (~175K calls in BENCH_FIB).
    /// This async fn is non-recursive, so the compiler can inline it
    /// into the caller's state machine without an extra Box::pin.
    async fn try_inline_user_call(&mut self, expr: &Expr) -> MixResult<Option<Value>> {
        let (name, args) = match expr {
            Expr::FunctionCall { name, args } => (name, args),
            _ => return Ok(None),
        };
        // Single lookup: fetch the user function first so we can early-
        // return without doing the shadow checks if it doesn't exist.
        // Saves one redundant scope.get_function probe per recursive
        // call (fib's path hits this ~57K times). SPEC 18 Phase 2 WS3-
        // C.7b — `get_function_owned` is γ-aware (walks the shared
        // function registry after promotion); legacy `get_function`
        // returned None for promoted scopes and would have silently
        // turned every Class C user function call into an unknown.
        let func = match self.scope.get_function_owned(name) {
            Some(f) => f,
            None => return Ok(None),
        };
        // require() Card 3 — a Function-valued local in the active
        // function frame (module_env / captures / params) outranks the
        // registry (module sibling calls). Bail to the full dispatch
        // arm so the precedence is applied in exactly one place. One
        // frame probe, and only when a registry hit exists AND we are
        // inside a function frame.
        if self
            .scope
            .get_function_var_in_function_frames(name)
            .is_some()
        {
            return Ok(None);
        }
        if crate::builtins::is_builtin(name)
            || crate::builtins_hof::lookup(name).is_some()
            || self.globals.borrow().extensions.contains_key(name)
            || matches!(
                name.as_str(),
                "port_exists"
                    | "noded_register"
                    | "bus_reconnect"
                    | "subscribe"
                    | "unsubscribe"
                    | "reply"
                    | "quit"
                    | "push"
                    | "pop"
                    | "shift"
                    | "sleep"
            )
            || !self.ctx.address_stack.is_empty()
        {
            return Ok(None);
        }
        self.track_function_attempt(name);
        // Specialize 0/1/2-arg calls to use stack arrays. fib's
        // `fib($n-1)` and `fib($n-2)` go through arity 1 — saving the
        // `Vec::with_capacity(1)` heap alloc per recursive call.
        // `try_call_function_sync_block` is tried first when the
        // function's body is sync-self-eligible (fib qualifies), which
        // collapses the entire recursive call tree to plain stack
        // recursion — no `Box::pin` per call.
        match args.len() {
            0 => {
                if let Some(r) = self.try_call_function_sync_block(&func, &[]) {
                    return Ok(Some(r?));
                }
                Ok(Some(self.call_function(&func, &[]).await?))
            }
            1 => {
                let a0 = match self.try_eval_expr_sync(&args[0]) {
                    Some(r) => r?,
                    None => self.eval_expr(&args[0]).await?,
                };
                let stack = [a0];
                if let Some(r) = self.try_call_function_sync_block(&func, &stack) {
                    return Ok(Some(r?));
                }
                Ok(Some(self.call_function(&func, &stack).await?))
            }
            2 => {
                let a0 = match self.try_eval_expr_sync(&args[0]) {
                    Some(r) => r?,
                    None => self.eval_expr(&args[0]).await?,
                };
                let a1 = match self.try_eval_expr_sync(&args[1]) {
                    Some(r) => r?,
                    None => self.eval_expr(&args[1]).await?,
                };
                let stack = [a0, a1];
                if let Some(r) = self.try_call_function_sync_block(&func, &stack) {
                    return Ok(Some(r?));
                }
                Ok(Some(self.call_function(&func, &stack).await?))
            }
            _ => {
                let mut eval_args = Vec::with_capacity(args.len());
                for arg in args {
                    if let Some(r) = self.try_eval_expr_sync(arg) {
                        eval_args.push(r?);
                    } else {
                        eval_args.push(self.eval_expr(arg).await?);
                    }
                }
                if let Some(r) = self.try_call_function_sync_block(&func, &eval_args) {
                    return Ok(Some(r?));
                }
                Ok(Some(self.call_function(&func, &eval_args).await?))
            }
        }
    }

    /// Sync fast path for calling a user function whose body is a
    /// statically-sync expression (e.g. `function($x) = $x * 3 + 1`).
    /// Returns `None` when the function isn't eligible — the caller
    /// must then fall back to the async `call_function_public` path.
    ///
    /// Eligibility:
    ///   - body is `FunctionBody::Expression` and `is_expr_sync` of it
    ///   - no captured frame (None) — captures themselves are cheap
    ///     to inject but the common HOF lambda case has none
    ///   - args.len() >= params.len() so no default-eval is needed
    ///     (defaults can contain arbitrary expressions and are rare
    ///     in HOF lambdas)
    ///
    /// Hot in HOFs: `map`/`filter`/`reduce` over an 8000-element list
    /// calls the lambda once per element. Skipping `Box::pin` on both
    /// `call_function` and `eval_expr` saves two allocations per call.
    /// True if entering another user-function call frame would exceed
    /// the configured recursion cap (knob B). Borrow-free — reads the
    /// per-invocation ctx copy of the limit, so the fib hot path pays
    /// only an integer compare. See [`DEFAULT_RECURSION_LIMIT`].
    #[inline]
    fn recursion_limit_reached(&self) -> bool {
        self.ctx.function_depth >= self.ctx.recursion_limit
    }

    /// The `recursion depth exceeded` runtime error, carrying the active
    /// limit. A catchable Mix error returned *instead of* overflowing
    /// the native stack (which would be an uncatchable, process-fatal
    /// SIGSEGV). Checked at every function-entry site that increments
    /// `function_depth`, plus the `eval_fib_program` bytecode VM.
    fn recursion_error(&self) -> MixError {
        self.runtime_err(format!(
            "recursion depth exceeded (limit {})",
            self.ctx.recursion_limit
        ))
    }

    /// Knob C: throttled wall-clock check for the **sync** call paths,
    /// which run function bodies via `run_sync_block_body`/
    /// `exec_stmt_sync` and so bypass the per-statement deadline poll in
    /// `execute`. Samples the clock only every 2^16 calls, and not at
    /// all when no `time_limit` is set (`deadline` stays `None`) — so the
    /// fib hot path pays at most one field read + branch per call. The
    /// deadline itself is armed by `execute`'s poll on the first
    /// statement, which always precedes any sync recursion.
    #[inline]
    fn deadline_exceeded_throttled(&mut self) -> bool {
        // Arm lazily here too (not only at execute's poll) so a caller
        // that drives sync recursion through a public call path with
        // only `time_limit` set still gets a bounded clock.
        let dl = match self.ctx.deadline {
            Some(dl) => dl,
            None => match self.ctx.time_limit {
                Some(lim) => {
                    let dl = std::time::Instant::now() + lim;
                    self.ctx.deadline = Some(dl);
                    dl
                }
                None => return false,
            },
        };
        self.ctx.deadline_tick = self.ctx.deadline_tick.wrapping_add(1);
        self.ctx.deadline_tick & 0xFFFF == 0 && std::time::Instant::now() >= dl
    }

    /// Throttled interrupt + deadline poll for the compiled/native For
    /// fast-path loops, which run to completion in pure Rust without
    /// re-entering `execute_inner`'s per-statement poll — without this,
    /// `EvalLimits::time_limit`'s "loop-based runaways are bounded"
    /// guarantee would be false on those paths and Ctrl-C would be
    /// swallowed. `counter` is loop-local; every 2^16 iterations the
    /// interrupt flag and wall-clock deadline are sampled exactly like
    /// the per-statement poll (same flag handling, same error
    /// messages). Between samples the cost is one increment + branch.
    #[inline]
    fn poll_native_loop(&mut self, counter: &mut u32) -> MixResult<()> {
        *counter = counter.wrapping_add(1);
        if *counter & 0xFFFF != 0 {
            return Ok(());
        }
        let interrupted = {
            let g = self.globals.borrow();
            if g.interrupted.load(Ordering::Relaxed) {
                g.interrupted.store(false, Ordering::Relaxed);
                true
            } else {
                false
            }
        };
        if interrupted {
            return Err(self.runtime_err("interrupted"));
        }
        if let Some(lim) = self.ctx.time_limit {
            let dl = *self
                .ctx
                .deadline
                .get_or_insert_with(|| std::time::Instant::now() + lim);
            if std::time::Instant::now() >= dl {
                return Err(self.runtime_err("deadline exceeded"));
            }
        }
        Ok(())
    }

    /// Knob A: consult the installed capability policy (if any) before
    /// running builtin/HOF `name`. `Ok(())` when no policy is installed
    /// (the permissive default) or the policy allows it; a `capability
    /// denied` runtime error otherwise. Call only for names that resolve
    /// to a builtin/HOF — user functions are never gated. Cloning the
    /// `Option<Rc>` is free when no policy is set.
    fn check_capability(&self, name: &str) -> MixResult<()> {
        let policy = { self.globals.borrow().capability_policy.clone() };
        match policy {
            Some(p) => p.check_builtin(name).map_err(|reason| {
                self.snapshot_builtin_error(
                    name,
                    MixError::structured(
                        "CAPABILITY_DENIED",
                        format!("capability denied: {reason}"),
                    ),
                )
            }),
            None => Ok(()),
        }
    }

    /// Like [`check_capability`](Self::check_capability) but gates a
    /// capability *class* directly — for the shell-syntax execution
    /// paths (`sh`/`$()`/pipes/shell-dispatch) that spawn `/bin/sh`
    /// without resolving to a named builtin. `what` names the construct
    /// for the error message. Free when no policy is installed.
    fn check_capability_class(
        &self,
        class: crate::builtins::CapabilityClass,
        what: &str,
    ) -> MixResult<()> {
        let policy = { self.globals.borrow().capability_policy.clone() };
        match policy {
            Some(p) => p.check_class(class).map_err(|reason| {
                self.snapshot_builtin_error(
                    what,
                    MixError::structured(
                        "CAPABILITY_DENIED",
                        format!("capability denied: {what}: {reason}"),
                    ),
                )
            }),
            None => Ok(()),
        }
    }

    /// Knob D: enforce the configured collection-size caps on `v`,
    /// **recursively** — so a builtin that returns a nested structure
    /// (e.g. `group_by`'s map-of-lists, `json_parse`'s nested doc)
    /// can't smuggle an oversized inner collection past a top-level
    /// check. When no cap is configured (the default) this is a single
    /// triple-`is_none` test and returns immediately — the common path
    /// pays essentially nothing; the recursive walk only runs once an
    /// embedder has opted into caps (and then it short-circuits on the
    /// first violation). String length is in bytes.
    fn check_collection_size(&self, v: &Value) -> MixResult<()> {
        if self.ctx.max_list_len.is_none()
            && self.ctx.max_map_len.is_none()
            && self.ctx.max_string_len.is_none()
        {
            return Ok(());
        }
        self.check_collection_size_rec(v)
    }

    // Iterative worklist (not recursion): this walks the same
    // arbitrarily-deep user data whose recursive Clone/Drop the manual
    // `Value` impls exist to protect against — a recursive checker here
    // would reintroduce the native stack overflow it is checking for.
    fn check_collection_size_rec(&self, v: &Value) -> MixResult<()> {
        let mut work: Vec<&Value> = vec![v];
        while let Some(v) = work.pop() {
            match v {
                Value::List(items) => {
                    if let Some(max) = self.ctx.max_list_len
                        && items.len() > max
                    {
                        return Err(self.runtime_err(format!(
                            "list length {} exceeds limit {}",
                            items.len(),
                            max
                        )));
                    }
                    work.extend(items.iter());
                }
                Value::Map(m) => {
                    if let Some(max) = self.ctx.max_map_len
                        && m.len() > max
                    {
                        return Err(self.runtime_err(format!(
                            "map size {} exceeds limit {}",
                            m.len(),
                            max
                        )));
                    }
                    work.extend(m.values());
                }
                Value::String(s) => {
                    if let Some(max) = self.ctx.max_string_len
                        && s.len() > max
                    {
                        return Err(self.runtime_err(format!(
                            "string length {} exceeds limit {}",
                            s.len(),
                            max
                        )));
                    }
                }
                // Knob D byte-buffer analogue of the String cap: without
                // this arm an oversized `Bytes` (http body,
                // read_file_bytes) sails past the size guard entirely.
                Value::Bytes(b) => {
                    if let Some(max) = self.ctx.max_string_len
                        && b.len() > max
                    {
                        return Err(self.runtime_err(format!(
                            "bytes length {} exceeds limit {}",
                            b.len(),
                            max
                        )));
                    }
                }
                // Same cap for a mutable buffer — an oversized `Buffer` must
                // not slip past the size guard that bounds `Bytes`.
                Value::Buffer(b) => {
                    let len = b.borrow().len();
                    if let Some(max) = self.ctx.max_string_len
                        && len > max
                    {
                        return Err(self
                            .runtime_err(format!("buffer length {} exceeds limit {}", len, max)));
                    }
                }
                _ => {}
            }
        }
        Ok(())
    }

    pub fn try_call_function_sync(
        &mut self,
        func: &MixFunction,
        args: &[Value],
    ) -> Option<MixResult<Value>> {
        let expr = match &func.body {
            FunctionBody::Expression(e) if Self::is_expr_sync(e) => e,
            _ => return None,
        };
        if func.needs_frame_injection() {
            return None;
        }
        if args.len() < func.params.len() {
            return None;
        }
        // Cross-file expression lambdas (e.g. a module lambda driven by
        // map/filter) decline the fast path so errors raise under the
        // callee's file via the async path (codex C4 review residual).
        if !Self::same_def_file(&func.def_file, &self.ctx.current_file) {
            return None;
        }
        // Strict arity: any inexact call bails to the async path, which
        // owns the ARITY_MISMATCH raise (this path ignores surplus args).
        if self.ctx.arity_strict && args.len() != func.params.len() {
            return None;
        }
        // Knob B: gate before either function_depth increment below.
        if self.recursion_limit_reached() {
            return Some(Err(self.recursion_error()));
        }
        // Knob C: bound a wide sync recursion that never re-enters the poll.
        if self.deadline_exceeded_throttled() {
            return Some(Err(self.runtime_err("deadline exceeded")));
        }

        // Frameless path for Expression-bodied lambdas: the body has
        // no statements and is_expr_sync — no Assignment can run, so
        // params are write-once. Bind them via var_slot_cache directly
        // from the caller's args slice and skip enter/leave_function_
        // frame entirely. HOF lambdas (`function($x) = $x*3+1`) are
        // hammered per-element by map/filter/reduce; each call drops
        // one frame push + HashMap insert + Value clone per param.
        //
        // Numeric fast path: when every arg is `Value::Number`, push
        // entries into `var_slot_cache_f64` pointing into each arg's
        // payload, then try `try_eval_expr_num` first. The HOF body
        // runs through the f64-typed evaluator (no Value::Number
        // wrap-then-unwrap at every leaf, no eval_binop dispatch) and
        // we re-wrap the f64 result once at the end. `function($x) =
        // $x*3+1` collapses from 5 Value-typed BinaryOp/leaf evals
        // per element to a flat f64 walk. Falls back to the generic
        // Value path if `try_eval_expr_num` rejects the body shape.
        let all_numeric = args[..func.params.len()]
            .iter()
            .all(|v| matches!(v, Value::Number(_)));
        if all_numeric {
            let f64_cache_start = self.ctx.var_slot_cache_f64.len();
            self.ctx.function_depth += 1;
            for (i, param) in func.params.iter().enumerate() {
                let n_ptr: *const f64 = match &args[i] {
                    Value::Number(n) => n as *const f64,
                    _ => unreachable!("all_numeric verified above"),
                };
                self.ctx
                    .var_slot_cache_f64
                    .push((param.name.as_str() as *const str, n_ptr));
            }
            // Order the num/bool probes by the body's outermost op shape.
            // For filter predicates like `$x % 2 == 0`, top-level is Eq:
            // try_eval_expr_num walks the *whole* tree before returning
            // None at its `_ => None` op-match, then try_eval_expr_bool
            // re-walks the same tree. Probing bool first when the top-
            // level op is a comparison cuts the wasted walk on every
            // filter iter (8000× per HOFs bench).
            let bool_first = matches!(
                expr,
                Expr::BinaryOp {
                    op: BinOp::Eq
                        | BinOp::NotEq
                        | BinOp::Gt
                        | BinOp::Lt
                        | BinOp::GtEq
                        | BinOp::LtEq,
                    ..
                }
            );
            if bool_first {
                if let Some(r) = self.try_eval_expr_bool(expr) {
                    self.ctx.function_depth -= 1;
                    self.ctx.var_slot_cache_f64.truncate(f64_cache_start);
                    return Some(match r {
                        Ok(b) => Ok(Value::Bool(b)),
                        Err(e) => Err(e),
                    });
                }
                if let Some(r) = self.try_eval_expr_num(expr) {
                    self.ctx.function_depth -= 1;
                    self.ctx.var_slot_cache_f64.truncate(f64_cache_start);
                    return Some(match r {
                        Ok(n) => Ok(Value::Number(n)),
                        Err(e) => Err(e),
                    });
                }
            } else {
                if let Some(r) = self.try_eval_expr_num(expr) {
                    self.ctx.function_depth -= 1;
                    self.ctx.var_slot_cache_f64.truncate(f64_cache_start);
                    return Some(match r {
                        Ok(n) => Ok(Value::Number(n)),
                        Err(e) => Err(e),
                    });
                }
                if let Some(r) = self.try_eval_expr_bool(expr) {
                    self.ctx.function_depth -= 1;
                    self.ctx.var_slot_cache_f64.truncate(f64_cache_start);
                    return Some(match r {
                        Ok(b) => Ok(Value::Bool(b)),
                        Err(e) => Err(e),
                    });
                }
            }
            self.ctx.function_depth -= 1;
            self.ctx.var_slot_cache_f64.truncate(f64_cache_start);
        }

        let cache_start = self.ctx.var_slot_cache.len();
        self.ctx.function_depth += 1;
        for (i, param) in func.params.iter().enumerate() {
            self.ctx
                .var_slot_cache
                .push((param.name.as_str() as *const str, &args[i] as *const Value));
        }
        let result = match self.try_eval_expr_sync(expr) {
            Some(r) => r,
            None => Err(self.sync_drift_err("try_call_function_sync")),
        };
        self.ctx.function_depth -= 1;
        self.ctx.var_slot_cache.truncate(cache_start);
        Some(result)
    }

    /// Sync fast path for an entire `Block` body when `block_sync_self`
    /// is true — i.e. the body's only async-flavoured construct is
    /// recursive calls to this same function. fib's full call tree
    /// runs through here on the real stack, eliminating every
    /// `Box::pin` of the recursive path. The async `call_function`
    /// remains the canonical path; this helper is only reached from
    /// `try_inline_user_call` and from `try_eval_expr_sync`'s
    /// FunctionCall arm.
    ///
    /// Returns `None` when not eligible (caller falls back to
    /// `call_function`); `Some(Ok/Err)` when executed.
    fn try_call_function_sync_block(
        &mut self,
        func: &Rc<MixFunction>,
        args: &[Value],
    ) -> Option<MixResult<Value>> {
        if !func.block_sync_self || func.needs_frame_injection() || args.len() != func.params.len()
        {
            return None;
        }
        // The native/bytecode recursion paths deliberately bypass normal
        // FunctionCall and statement dispatch. With a collector attached that
        // would make recursive calls and their `if`/`return` executions vanish
        // from usage stats. Fall back to the canonical path while recording;
        // embedders with no collector retain the hot path unchanged.
        if self.stats_attached {
            return None;
        }
        // Cross-file functions decline the fast path: this helper does
        // not switch ctx.current_file to the callee's def_file, so a
        // foreign-file callee would raise spans/frames under the
        // CALLER's file. The async call_function does the switch;
        // same-file calls (including all self-recursion) pass via the
        // Rc::ptr_eq fast case. (codex C3 review, BLOCKER)
        if !Self::same_def_file(&func.def_file, &self.ctx.current_file) {
            return None;
        }
        // Knob B: gate before either function_depth increment below
        // (frameless and framed paths), ahead of any frame/cache setup.
        if self.recursion_limit_reached() {
            return Some(Err(self.recursion_error()));
        }
        // Knob C: bound a wide sync recursion that never re-enters the poll.
        if self.deadline_exceeded_throttled() {
            return Some(Err(self.runtime_err("deadline exceeded")));
        }

        // Frameless fast path: a `body_no_local_assigns` body in a
        // `block_sync_self` function reaches scope.frames for nothing.
        // - Variable reads route through `var_slot_cache` (we pre-bind
        //   every param slot below).
        // - There are no writes (Assignment / FieldAssignment /
        //   IndexAssignment all banned by `body_no_local_assigns`).
        // - There are no constructs that push frames (For/While/Loop/
        //   Block all banned by `is_stmt_sync_self`).
        // - There are no foreign function calls — `is_expr_sync_self`
        //   only allows self-recursive `FunctionCall(name, ...)` with
        //   matching arity, dispatched back through this same helper.
        // So we can skip `enter_function_frame` (Vec push, function_
        // floors entry, pool pop) and `leave_function_frame` (clear,
        // pool push) entirely, and bind params on the stack — no
        // String alloc per call. fib's recursive call tree (~285K
        // calls in BENCH_FIB) drops one HashMap insert + String alloc
        // per call.
        // The block body's statements write ctx.current_line; restore
        // the CALLER's line on exit (both arms) so a later failure in
        // the same caller expression doesn't inherit the callee's last
        // line — the sync twin of call_function's bracket.
        let caller_line = self.ctx.current_line;

        if func.body_no_local_assigns {
            let cache_start = self.ctx.var_slot_cache.len();
            self.ctx.function_depth += 1;
            let prev_self = self.ctx.sync_self_func.replace(func.clone());

            // `args` is borrowed from a stack array in the caller (for
            // the synchronous self-recursive arm) or from a Vec in
            // `try_inline_user_call`'s outer frame; either way it is
            // alive and immobile for the duration of this call. So
            // var_slot_cache can point directly into it — no per-call
            // Value clone for params. fib's 285k calls each save one
            // 32-byte memcpy of the f64 arg.
            for (i, param) in func.params.iter().enumerate() {
                self.ctx
                    .var_slot_cache
                    .push((param.name.as_str() as *const str, &args[i] as *const Value));
            }
            let result = self.run_sync_block_body(func);

            self.ctx.sync_self_func = prev_self;
            self.ctx.function_depth -= 1;
            self.ctx.var_slot_cache.truncate(cache_start);
            self.ctx.current_line = caller_line;
            return Some(result);
        }

        // Framed path: body has at least one Assignment, so we need
        // a real function frame. Param-slot caching is not safe here
        // (an Assignment could rehash the bucket array).
        self.scope.enter_function_frame();
        for (i, param) in func.params.iter().enumerate() {
            self.scope
                .set_in_current(param.name.clone(), args[i].clone());
        }
        let cache_start = self.ctx.var_slot_cache.len();
        self.ctx.function_depth += 1;
        let prev_self = self.ctx.sync_self_func.replace(func.clone());

        let result = self.run_sync_block_body(func);

        self.ctx.sync_self_func = prev_self;
        self.ctx.function_depth -= 1;
        self.ctx.var_slot_cache.truncate(cache_start);
        self.scope.leave_function_frame();
        self.ctx.current_line = caller_line;
        Some(result)
    }

    /// Strict-arity gate for builtin/HOF dispatch: raise ARITY_MISMATCH
    /// when the contract metadata does not accept `n` arguments.
    fn check_builtin_arity(&self, name: &str, n: usize) -> MixResult<()> {
        if let Some(info) = builtins::builtin_info_of(name)
            && !info.contract.accepts_arity(n)
        {
            return Err(self.snapshot_builtin_error(
                name,
                self.coded_err(
                    "ARITY_MISMATCH",
                    format!(
                        "{}: {} argument(s) not accepted (contract: {})",
                        name,
                        n,
                        info.signature()
                    ),
                ),
            ));
        }
        Ok(())
    }

    /// Whether a callee's definition file matches the currently
    /// executing file — the sync fast paths' eligibility check (they
    /// never switch `ctx.current_file`, so a mismatch must fall back
    /// to the async path). Same-file clones share one `Rc`, so the
    /// common case is a pointer compare.
    fn same_def_file(def: &Option<std::rc::Rc<str>>, cur: &Option<std::rc::Rc<str>>) -> bool {
        match (def, cur) {
            (None, None) => true,
            (Some(a), Some(b)) => std::rc::Rc::ptr_eq(a, b) || a == b,
            _ => false,
        }
    }

    /// Numeric-args variant of the frameless sync-block path. Callers
    /// (only the FunctionCall arm of `try_eval_expr_num`) have already
    /// evaluated all args as `f64` and the callee is verified
    /// frameless (`body_no_local_assigns` + `block_sync_self`). We
    /// push directly into `var_slot_cache_f64` instead of constructing
    /// a `[Value::Number(...)]` array. The body's Variable reads of
    /// the param hit the f64 cache first and return f64 with no
    /// variant-tag match. The body always returns `Value::Number(_)`
    /// in well-typed numeric self-recursion (e.g. fib), which we
    /// unwrap to f64 for the caller.
    fn try_call_function_sync_block_f64(
        &mut self,
        func: &Rc<MixFunction>,
        args: &[f64],
    ) -> Option<MixResult<f64>> {
        // Knob B: gate before the function_depth increment below.
        if self.recursion_limit_reached() {
            return Some(Err(self.recursion_error()));
        }
        // Knob C: bound a wide sync recursion that never re-enters the poll.
        if self.deadline_exceeded_throttled() {
            return Some(Err(self.runtime_err("deadline exceeded")));
        }
        let caller_line = self.ctx.current_line;
        let cache_start = self.ctx.var_slot_cache_f64.len();
        self.ctx.function_depth += 1;
        let prev_self = self.ctx.sync_self_func.replace(func.clone());

        // Borrow `args` directly: the slice is stack-allocated by the
        // caller (FunctionCall arm of try_eval_expr_num) and stays
        // alive for the duration of this call, so pointers into it
        // are valid for cache reads inside `run_sync_block_body`.
        for (i, param) in func.params.iter().enumerate() {
            self.ctx
                .var_slot_cache_f64
                .push((param.name.as_str() as *const str, &args[i] as *const f64));
        }
        // Numeric body fast path: when the body is the IfReturnReturn
        // shape and its chosen branch evaluates as f64, we return f64
        // straight up — skipping the Value::Number(n) wrap inside
        // run_sync_block_body and the variant-tag unwrap below. fib's
        // ~285k recursive calls each save one Value construction +
        // one variant match. Falls back to the generic Value path on
        // shape mismatch or non-numeric result.
        if let Some(r) = self.run_sync_block_body_f64(func) {
            self.ctx.sync_self_func = prev_self;
            self.ctx.function_depth -= 1;
            self.ctx.var_slot_cache_f64.truncate(cache_start);
            self.ctx.current_line = caller_line;
            return Some(r);
        }
        let result = self.run_sync_block_body(func);

        self.ctx.sync_self_func = prev_self;
        self.ctx.function_depth -= 1;
        self.ctx.var_slot_cache_f64.truncate(cache_start);
        self.ctx.current_line = caller_line;
        match result {
            Ok(Value::Number(n)) => Some(Ok(n)),
            Ok(_) => None,
            Err(e) => Some(Err(e)),
        }
    }

    /// f64-typed specialization of `run_sync_block_body`'s
    /// IfReturnReturn shape path. Returns `Some(Ok(f64))` when the
    /// fib-like `[If { cond, then=[Return(t)] }, Return(e)]` body
    /// resolves to f64; `Some(Err(_))` on runtime error inside the
    /// numeric path; `None` when the shape doesn't match or the
    /// chosen branch isn't numerically evaluable (caller falls back
    /// to the generic Value path).
    fn run_sync_block_body_f64(&mut self, func: &Rc<MixFunction>) -> Option<MixResult<f64>> {
        // Trace bypass: decline the numeric specialization under
        // `mix trace on` so the call falls through to run_sync_block_body's
        // generic exec_stmt_sync loop, which emits the per-statement trace.
        // Returning None is the existing "shape not handled, fall back"
        // contract, so the caller's fallback handles it unchanged.
        if self.globals.borrow().trace {
            return None;
        }
        let stmts = match &func.body {
            FunctionBody::Block(s) => s,
            _ => return None,
        };
        if stmts.len() != 2 {
            return None;
        }
        let (condition, then_body, else_ifs, else_body, else_expr) =
            match (&stmts[0].kind, &stmts[1].kind) {
                (
                    StmtKind::If {
                        condition,
                        then_body,
                        else_ifs,
                        else_body,
                    },
                    StmtKind::Return(Some(else_expr)),
                ) => (condition, then_body, else_ifs, else_body, else_expr),
                _ => return None,
            };
        if !else_ifs.is_empty() || else_body.is_some() || then_body.len() != 1 {
            return None;
        }
        let then_expr = match &then_body[0].kind {
            StmtKind::Return(Some(e)) => e,
            _ => return None,
        };
        // Evaluate the cond in numeric mode if possible (`$n < 2`
        // shape); otherwise fall back to the generic Value cond.
        let truthy = if let Expr::BinaryOp { left, op, right } = condition {
            if matches!(
                op,
                BinOp::Lt | BinOp::Gt | BinOp::LtEq | BinOp::GtEq | BinOp::Eq | BinOp::NotEq
            ) {
                match (self.try_eval_expr_num(left), self.try_eval_expr_num(right)) {
                    (Some(Ok(l)), Some(Ok(r))) => Some(match op {
                        BinOp::Lt => l < r,
                        BinOp::Gt => l > r,
                        BinOp::LtEq => l <= r,
                        BinOp::GtEq => l >= r,
                        BinOp::Eq => l == r,
                        BinOp::NotEq => l != r,
                        _ => unreachable!(),
                    }),
                    (Some(Err(e)), _) | (_, Some(Err(e))) => return Some(Err(e)),
                    _ => None,
                }
            } else {
                None
            }
        } else {
            None
        };
        let truthy = match truthy {
            Some(b) => b,
            None => match self.try_eval_expr_sync(condition) {
                Some(Ok(v)) => v.is_truthy(),
                Some(Err(e)) => return Some(Err(e)),
                None => return Some(Err(self.sync_drift_err("run_sync_block_body_f64"))),
            },
        };
        let chosen = if truthy { then_expr } else { else_expr };
        self.try_eval_expr_num(chosen)
    }

    /// Body executor shared by both framed and frameless sync-block
    /// paths. Caller is responsible for setting up param visibility
    /// (either via scope frames or via `var_slot_cache`).
    #[inline]
    fn run_sync_block_body(&mut self, func: &Rc<MixFunction>) -> MixResult<Value> {
        // When `mix trace on`, decline every fast-path specialization so
        // each body statement runs through the generic exec_stmt_sync loop
        // below — the one place `trace_stmt` fires on the sync path. Without
        // this, a fib-shaped function body would execute with zero trace
        // output. Mirrors the `!trace` gating on the partial-inline and
        // tail-return fast paths.
        let trace_on = self.globals.borrow().trace;
        match &func.body {
            FunctionBody::Block(stmts) => {
                // Specialized shape: `[If { cond, then=[Return(t)],
                // else_ifs=[], else=None }, Return(e)]` — fib's exact
                // body. Skips exec_stmt_sync's per-stmt match dispatch
                // and the MixError::Return round-trip on the recursive
                // branch (~285k Returns avoid being constructed-then-
                // unwrapped). Detected per-call here; the AST walk is
                // a few cheap pointer compares.
                if !trace_on
                    && stmts.len() == 2
                    && let (
                        StmtKind::If {
                            condition,
                            then_body,
                            else_ifs,
                            else_body,
                        },
                        StmtKind::Return(Some(else_expr)),
                    ) = (&stmts[0].kind, &stmts[1].kind)
                    && else_ifs.is_empty()
                    && else_body.is_none()
                    && then_body.len() == 1
                    && let StmtKind::Return(Some(then_expr)) = &then_body[0].kind
                {
                    // Numeric cond fast path: `$n < N`-style
                    // tests evaluate operands as f64 directly,
                    // skipping the Value::Number clone+drop
                    // and the Value::Bool boxing. Falls back
                    // to the generic Value path otherwise.
                    let truthy = if let Expr::BinaryOp { left, op, right } = condition {
                        if matches!(
                            op,
                            BinOp::Lt
                                | BinOp::Gt
                                | BinOp::LtEq
                                | BinOp::GtEq
                                | BinOp::Eq
                                | BinOp::NotEq
                        ) {
                            match (self.try_eval_expr_num(left), self.try_eval_expr_num(right)) {
                                (Some(Ok(l)), Some(Ok(r))) => Some(match op {
                                    BinOp::Lt => l < r,
                                    BinOp::Gt => l > r,
                                    BinOp::LtEq => l <= r,
                                    BinOp::GtEq => l >= r,
                                    BinOp::Eq => l == r,
                                    BinOp::NotEq => l != r,
                                    _ => unreachable!(),
                                }),
                                (Some(Err(e)), _) | (_, Some(Err(e))) => return Err(e),
                                _ => None,
                            }
                        } else {
                            None
                        }
                    } else {
                        None
                    };
                    let truthy = match truthy {
                        Some(b) => b,
                        None => match self.try_eval_expr_sync(condition) {
                            Some(Ok(v)) => v.is_truthy(),
                            Some(Err(e)) => return Err(e),
                            None => return Err(self.sync_drift_err("run_sync_block_body")),
                        },
                    };
                    let chosen = if truthy { then_expr } else { else_expr };
                    // Numeric return path: if the chosen
                    // expression is a pure numeric expr
                    // (literal, var, arithmetic, recursive
                    // numeric call), evaluate as f64 and
                    // wrap once. Avoids the Value::Number
                    // wrap-then-unwrap dance the BinaryOp
                    // arm does on the recursive branch.
                    if let Some(r) = self.try_eval_expr_num(chosen) {
                        return match r {
                            Ok(n) => Ok(Value::Number(n)),
                            Err(e) => Err(e),
                        };
                    }
                    return match self.try_eval_expr_sync(chosen) {
                        Some(r) => r,
                        None => Err(self.sync_drift_err("run_sync_block_body")),
                    };
                }
                let mut last = Value::Nil;
                let mut error: Option<MixError> = None;
                for stmt in stmts {
                    match self.exec_stmt_sync(stmt) {
                        Ok(v) => last = v,
                        Err(MixError::Return { value }) => {
                            last = value;
                            break;
                        }
                        Err(e) => {
                            error = Some(e);
                            break;
                        }
                    }
                }
                match error {
                    Some(e) => Err(self.attach_span(e)),
                    None => Ok(last),
                }
            }
            FunctionBody::Expression(expr) => match self.try_eval_expr_sync(expr) {
                Some(r) => r,
                None => Err(self.sync_drift_err("run_sync_block_body")),
            },
        }
    }

    fn call_function<'a>(
        &'a mut self,
        func: &'a MixFunction,
        args: &'a [Value],
    ) -> Pin<Box<dyn Future<Output = MixResult<Value>> + 'a>> {
        Box::pin(async move {
            // Knob B: gate before entering the frame / incrementing
            // function_depth, so a recursion overflow returns a clean
            // Mix error instead of overflowing the native stack.
            if self.recursion_limit_reached() {
                return Err(self.recursion_error());
            }
            // Strict arity (0.29.0, D5): raise BEFORE entering the
            // frame. min = one past the LAST required (non-default)
            // param — NOT the count, because Mix's parser permits a
            // required param after a defaulted one, and every param up
            // to the last required must be supplied positionally (codex
            // release review, MAJOR). max = all params.
            if self.ctx.arity_strict {
                let (min, max) = crate::scope::param_arity(&func.params);
                if args.len() < min || args.len() > max {
                    return Err(self.coded_err(
                        "ARITY_MISMATCH",
                        format!(
                            "{}: expected {} argument(s), got {}",
                            func.name,
                            if min == max {
                                min.to_string()
                            } else {
                                format!("{min}..{max}")
                            },
                            args.len()
                        ),
                    ));
                }
            }
            // Enter an isolated function frame. Lookups will only see
            // this frame (plus globals) until we leave — no per-call
            // Vec<HashMap> stash/restore.
            self.scope.enter_function_frame();
            // Knob B: increment BEFORE binding parameters. A default-
            // parameter expression (`function f($x = f()) ... end`) can
            // recurse via `eval_expr(default)` below, so the depth must
            // already reflect this frame or that path would bypass the
            // cap. The single cleanup after the inner block guarantees
            // the matching decrement + frame-leave on every exit path
            // (including an error while evaluating a default).
            self.ctx.function_depth += 1;
            // Traceback frame + caller context save. The frame records
            // the CALLER side of this call (its line and file); the
            // callee's execution window then runs under the callee's
            // own def_file so spans/frames raised inside it carry the
            // right file across require/include/source boundaries.
            // Both are restored in the single cleanup below, AFTER the
            // error-path snapshot has read the intact stack — and the
            // caller's line is restored even on SUCCESS, so a later
            // failure in the same caller expression doesn't inherit
            // the callee's last line (codex C2 review, MAJOR 1).
            let caller_line = self.ctx.current_line;
            self.ctx.call_frames.push(CallFrame {
                name: func.name.clone(),
                call_line: caller_line,
                caller_file: self.ctx.current_file.clone(),
            });
            // Switch to the callee's file only when it differs — the
            // same-file common case skips the swap and its restore.
            let file_switched = !Self::same_def_file(&func.def_file, &self.ctx.current_file);
            let caller_file = if file_switched {
                std::mem::replace(&mut self.ctx.current_file, func.def_file.clone())
            } else {
                None
            };

            // require() module environment — injected BEFORE captures
            // (which are injected before params), so captures shadow the
            // env and params shadow both. `Some` only on functions
            // harvested by `exec_require`; every other function skips
            // this with one Option check.
            if let Some(env) = &func.module_env
                && let Some(map) = env.get()
            {
                for (k, v) in map {
                    self.scope.set_in_current(k.clone(), v.clone());
                }
            }

            // Inject captured frame for inner-frame lambdas.
            // Named functions always have `captures: None` so this loop
            // is a no-op for them — keeping the named-function code path
            // byte-identical (the Phase 1.4 tripwire). Params are bound
            // *after* captures so a param of the same name correctly
            // shadows the capture.
            if let Some(caps) = &func.captures {
                for (k, v) in caps {
                    self.scope.set_in_current(k.clone(), v.clone());
                }
            }

            // Bind parameters. A default expression can recurse (depth is
            // already incremented above) or error — capture that error
            // rather than `?`-returning, so the single cleanup below
            // always runs (no frame/depth leak). Deliberately NOT wrapped
            // in an inner `async` block: that would deepen the per-call
            // future nesting on the hot recursion path and eat into the
            // very stack budget the recursion cap exists to protect.
            let mut bind_err: Option<MixError> = None;
            for (i, param) in func.params.iter().enumerate() {
                let val = if i < args.len() {
                    args[i].clone()
                } else if let Some(default) = &param.default {
                    match self.eval_expr(default).await {
                        Ok(v) => v,
                        Err(e) => {
                            bind_err = Some(e);
                            break;
                        }
                    }
                } else {
                    Value::Nil
                };
                self.scope.set_in_current(param.name.clone(), val);
            }

            let result: MixResult<Value> = if let Some(e) = bind_err {
                Err(e)
            } else {
                match &func.body {
                    FunctionBody::Block(stmts) => {
                        // Sync-prefix fast path: pre-cached `sync_prefix_len`
                        // tells us how many leading stmts pass `is_stmt_sync`.
                        // Run them inline; if one returns (fib's base case), skip
                        // the `execute()` Box::pin entirely.
                        let prefix = func.sync_prefix_len.min(stmts.len());
                        let mut early: Option<MixResult<Value>> = None;
                        for stmt in &stmts[..prefix] {
                            match self.exec_stmt_sync(stmt) {
                                Ok(_) => {}
                                Err(MixError::Return { value }) => {
                                    early = Some(Ok(value));
                                    break;
                                }
                                Err(e) => {
                                    early = Some(Err(self.attach_span(e)));
                                    break;
                                }
                            }
                        }
                        if let Some(r) = early {
                            r
                        } else if prefix >= stmts.len() {
                            Ok(Value::Nil)
                        } else if !self.globals.borrow().trace
                            && prefix + 1 == stmts.len()
                            && matches!(stmts[prefix].kind, StmtKind::Return(_))
                        {
                            // Tail-Return fast path: when the only stmt remaining
                            // after the sync prefix is `return <expr>`, skip
                            // execute()'s Box::pin entirely and evaluate the
                            // return expression directly. fib's recursive
                            // `return fib($n-1) + fib($n-2)` hits this on every
                            // non-base call (~28K of fib(22)'s ~57K calls).
                            let stmt = &stmts[prefix];
                            if stmt.line > 0 {
                                self.ctx.current_line = stmt.line;
                            }
                            self.track_stmt_keywords(stmt);
                            let ret_expr = match &stmt.kind {
                                StmtKind::Return(e) => e,
                                _ => unreachable!(),
                            };
                            match ret_expr {
                                Some(e) => match self.eval_expr(e).await {
                                    Ok(v) => Ok(v),
                                    Err(e) => Err(self.attach_span(e)),
                                },
                                None => Ok(Value::Nil),
                            }
                        } else {
                            match self.execute(&stmts[prefix..]).await {
                                Ok(v) => Ok(v),
                                Err(MixError::Return { value }) => Ok(value),
                                Err(e) => Err(e),
                            }
                        }
                    }
                    FunctionBody::Expression(expr) => self.eval_expr(expr).await,
                }
            };

            // Snapshot the traceback BEFORE popping this frame — the
            // first function boundary an error crosses is where the
            // stack is still intact (idempotent for errors snapshotted
            // deeper in).
            let result = result.map_err(|e| self.snapshot_error(e));

            self.ctx.call_frames.pop();
            self.ctx.current_line = caller_line;
            if file_switched {
                self.ctx.current_file = caller_file;
            }
            self.ctx.function_depth -= 1;
            self.scope.leave_function_frame();

            result
        })
    }

    /// Address-block fallthrough: an unknown function name inside
    /// `address "target" … end` IS a send, honouring the SAME non-fatal
    /// rc-band contract as exec_send: no handler → RC_UNAVAILABLE,
    /// transport Err → RC_TRANSPORT, both non-fatal (return Nil,
    /// continue); an Ok rc is written verbatim. Shared by the
    /// `FunctionCall` and `MethodCall` eval arms (the method arm passes
    /// the evaluated object as the leading arg, so it rides along as
    /// `_0` exactly as the old UFCS desugar did).
    async fn address_block_send(&mut self, name: &str, eval_args: Vec<Value>) -> MixResult<Value> {
        let target = self.ctx.address_stack.last().unwrap().clone();
        // Build args map from positional args
        let mut map = IndexMap::new();
        for (i, val) in eval_args.into_iter().enumerate() {
            map.insert(format!("_{}", i), val);
        }
        let args_map = Value::map(map);
        self.check_collection_size(&args_map)?; // Knob D
        // SPEC 18 Phase 2 WS3-C.5 — clone-out before await.
        let handler_opt = self.globals.borrow().bus_handler.clone();
        let handler = match handler_opt {
            Some(h) => h,
            None => {
                self.scope
                    .update_or_set("rc", Value::Number(RC_UNAVAILABLE as f64));
                self.scope.update_or_set(
                    "result",
                    Value::String("Bus not available (no handler registered)".to_string()),
                );
                return Ok(Value::Nil);
            }
        };
        // SPEC 18 Phase 2 WS3-C.7e — yield-on-send. The outer `?` keeps
        // a yield-machinery error fatal; the inner send outcome is
        // mapped to the rc bands, never aborts.
        let send_fut = handler.send(&target, name, &args_map);
        match self.await_with_class_c_yield(send_fut).await? {
            Ok((rc, result)) => {
                self.scope.update_or_set("rc", Value::Number(rc as f64));
                self.scope.update_or_set("result", result.clone());
                Ok(result)
            }
            Err(e) => {
                self.scope
                    .update_or_set("rc", Value::Number(RC_TRANSPORT as f64));
                self.scope
                    .update_or_set("result", Value::String(e.to_string()));
                Ok(Value::Nil)
            }
        }
    }

    fn call_mutating_builtin<'a>(
        &'a mut self,
        name: &'a str,
        args: &'a [Expr],
    ) -> Pin<Box<dyn Future<Output = MixResult<Value>> + 'a>> {
        Box::pin(async move {
            // Knob A: gate here so the in-place mutating path (push/pop/
            // shift on a variable) is covered too, not just the
            // non-mutating fallback below.
            self.check_capability(name)?;
            // First arg must be a variable to mutate in-place
            let var_name = match args.first() {
                Some(Expr::Variable(v)) => v.clone(),
                _ => {
                    // Fall back to non-mutating call
                    let mut eval_args = Vec::new();
                    for arg in args {
                        eval_args.push(self.eval_expr(arg).await?);
                    }
                    let result = builtins::call_builtin(name, eval_args)?.ok_or_else(|| {
                        MixError::structured(
                            "FUNCTION_UNDEFINED",
                            format!("undefined function '{}'", name),
                        )
                    })?;
                    // Knob D: cap the produced collection.
                    self.check_collection_size(&result)?;
                    return Ok(result);
                }
            };

            // Evaluate the value-to-push first (push only) so the &mut
            // borrow on scope is held only across the list mutation itself.
            let push_val = if name == "push" {
                if args.len() < 2 {
                    return Err(MixError::RuntimeError {
                        span: None,
                        msg: "push() requires 2 arguments".to_string(),
                    });
                }
                Some(self.eval_expr(&args[1]).await?)
            } else {
                None
            };

            // SPEC 18 Phase 2 WS3-C.7b — γ-aware in-place mutation. The
            // `with_mut_value` closure path delegates to the shared
            // global RefCell when the scope was promoted (per-invocation
            // child or post-promotion parent), so push/pop/shift on
            // globals continues to mutate the one canonical slot. The
            // standalone hot path still goes through the same closure;
            // it just resolves directly against the legacy frame map.
            // Outcome encoding: Some(Ok(v)) = found a list, Some(Err)
            // = found a non-list, None = name unbound.
            // Knob D: capture the cap + report out of band (the
            // with_mut_value closure can't touch self).
            let max_list = self.ctx.max_list_len;
            let mut cap_violation: Option<usize> = None;
            // CoW invariant (card 4 §4): this path must never hold a
            // second Rc owner of the target list when Rc::make_mut runs
            // — args dispatch here BEFORE eval_args is built and the
            // mutation goes through the scope slot, so the canonical
            // `push($acc, x)` accumulator is sole-owner and make_mut is
            // in-place. (push($l, $l) bumps the count via push_val — one
            // shallow copy, matching the old deep-clone-at-eval cost.)
            let outcome = self.scope.with_mut_value(&var_name, |v| match v {
                Value::List(list) => {
                    let list = Rc::make_mut(list);
                    match name {
                        "push" => {
                            list.push(push_val.unwrap());
                            if let Some(max) = max_list
                                && list.len() > max
                            {
                                cap_violation = Some(list.len());
                            }
                            Ok(Value::Nil)
                        }
                        "pop" => Ok(list.pop().unwrap_or(Value::Nil)),
                        "shift" => Ok(if list.is_empty() {
                            Value::Nil
                        } else {
                            list.remove(0)
                        }),
                        _ => unreachable!(),
                    }
                }
                _ => Err(()),
            });
            let result = match outcome {
                Some(Ok(v)) => v,
                _ => {
                    return Err(MixError::RuntimeError {
                        span: None,
                        msg: format!("{}() expects a list variable", name),
                    });
                }
            };
            if let Some(n) = cap_violation {
                return Err(self.runtime_err(format!(
                    "list length {} exceeds limit {}",
                    n,
                    max_list.unwrap_or(0)
                )));
            }

            Ok(result)
        })
    }

    /// Execute a send command: evaluate args, call Bus handler, set $rc and $result.
    /// `publish(topic, body[, opts])` — see the FunctionCall special arm.
    ///
    /// Contract (documented in bus.md):
    /// - `topic`: non-empty string, no newline/CR (frame-injection guard).
    ///   It is both the noded `name=` and, by default, the inner frame's
    ///   `command:` header (what subscribers' `on` matches).
    /// - `body`: the payload STRING (or nil → empty). Maps/lists are
    ///   refused with a pointer at json_encode — no hidden encoding.
    /// - `opts` map: `retain` (bool or "true"/"false", default false),
    ///   `command` (override the inner frame header — the fleet publishes
    ///   `svc.corner.entered` with inner command `corner.entered`),
    ///   `headers` (map of extra frame header lines; keys must not
    ///   contain `:`/newline, values must not contain newline).
    /// - `$rc`/`$result` set exactly like `send`; the rc is returned, so
    ///   `if publish(..) != 0` reads naturally.
    async fn exec_publish(&mut self, args: Vec<Value>) -> MixResult<Value> {
        let topic = match args.first() {
            Some(Value::String(s)) if !s.is_empty() => s.clone(),
            _ => {
                return Err(self.runtime_err(
                    "publish: topic (first argument) must be a non-empty string",
                ));
            }
        };
        if topic.contains('\n') || topic.contains('\r') {
            return Err(self.runtime_err("publish: topic must not contain newlines"));
        }
        let payload = match args.get(1) {
            None | Some(Value::Nil) => String::new(),
            Some(Value::String(s)) => s.clone(),
            Some(v @ (Value::Map(_) | Value::List(_))) => {
                return Err(self.runtime_err(format!(
                    "publish: body must be a string, got {} — encode it first \
                     (json_encode) so the wire format is explicit",
                    v.type_name()
                )));
            }
            Some(v) => v.to_mix_string(),
        };
        let mut retain = false;
        let mut command_hdr = topic.clone();
        let mut extra_headers: Vec<(String, String)> = Vec::new();
        match args.get(2) {
            None | Some(Value::Nil) => {}
            Some(Value::Map(opts)) => {
                for (k, v) in opts.iter() {
                    match k.as_str() {
                        "retain" => {
                            retain = match v {
                                Value::Bool(b) => *b,
                                Value::String(s) if s == "true" => true,
                                Value::String(s) if s == "false" => false,
                                other => {
                                    return Err(self.runtime_err(format!(
                                        "publish: retain must be a bool, got {}",
                                        other.type_name()
                                    )));
                                }
                            };
                        }
                        "command" => {
                            let c = v.to_mix_string();
                            if c.is_empty() || c.contains('\n') || c.contains('\r') {
                                return Err(self.runtime_err(
                                    "publish: command override must be a non-empty \
                                     string without newlines",
                                ));
                            }
                            command_hdr = c;
                        }
                        "headers" => {
                            let Value::Map(hdrs) = v else {
                                return Err(self.runtime_err(format!(
                                    "publish: headers must be a map, got {}",
                                    v.type_name()
                                )));
                            };
                            for (hk, hv) in hdrs.iter() {
                                let hv = hv.to_mix_string();
                                if hk.is_empty()
                                    || hk.contains(':')
                                    || hk.contains('\n')
                                    || hk.contains('\r')
                                    || hv.contains('\n')
                                    || hv.contains('\r')
                                {
                                    return Err(self.runtime_err(format!(
                                        "publish: header '{hk}' — keys must be \
                                         colon- and newline-free, values \
                                         newline-free (frame-injection guard)"
                                    )));
                                }
                                extra_headers.push((hk.clone(), hv));
                            }
                        }
                        other => {
                            return Err(self.runtime_err(format!(
                                "publish: unknown option '{other}' \
                                 (retain, command, headers)"
                            )));
                        }
                    }
                }
            }
            Some(other) => {
                return Err(self.runtime_err(format!(
                    "publish: third argument must be an options map, got {}",
                    other.type_name()
                )));
            }
        }
        if args.len() > 3 {
            return Err(self.runtime_err(format!(
                "publish takes (topic, body, opts); got {} arguments",
                args.len()
            )));
        }

        // The SPEC-02 wire frame subscribers receive.
        let mut frame = format!("---\ncommand: {command_hdr}\n");
        for (k, v) in &extra_headers {
            frame.push_str(k);
            frame.push_str(": ");
            frame.push_str(v);
            frame.push('\n');
        }
        frame.push_str("---\n");
        frame.push_str(&payload);

        let mut args_map = IndexMap::new();
        args_map.insert("name".to_string(), Value::String(topic));
        args_map.insert(
            "retain".to_string(),
            Value::String(if retain { "true" } else { "false" }.to_string()),
        );
        args_map.insert("body".to_string(), Value::String(frame));
        let args_value = Value::map(args_map);

        // Same degrade/transport contract as exec_send, without the
        // per-send timeout leg (publish is fire-to-broker; add a
        // timeout option if a real caller ever needs one).
        let bus_handler = self.globals.borrow().bus_handler.clone();
        let handler = match bus_handler {
            Some(h) => h,
            None => {
                self.scope
                    .update_or_set("rc", Value::Number(RC_UNAVAILABLE as f64));
                self.scope.update_or_set(
                    "result",
                    Value::String("Bus not available (no handler registered)".to_string()),
                );
                return Ok(Value::Number(RC_UNAVAILABLE as f64));
            }
        };
        let fut = handler.send("noded", "topic.publish", &args_value);
        match self.await_with_class_c_yield(fut).await? {
            Ok((rc, result)) => {
                self.scope.update_or_set("rc", Value::Number(rc as f64));
                self.scope.update_or_set("result", result);
                Ok(Value::Number(rc as f64))
            }
            Err(e) => {
                self.scope
                    .update_or_set("rc", Value::Number(RC_TRANSPORT as f64));
                self.scope
                    .update_or_set("result", Value::String(e.to_string()));
                Ok(Value::Number(RC_TRANSPORT as f64))
            }
        }
    }

    fn exec_send<'a>(
        &'a mut self,
        target: &'a Expr,
        command: &'a Expr,
        args: &'a [(String, Expr)],
    ) -> Pin<Box<dyn Future<Output = MixResult<Value>> + 'a>> {
        Box::pin(async move {
            let target_str = self.eval_expr(target).await?.to_mix_string();
            let command_str = self.eval_expr(command).await?.to_mix_string();
            if command_str.is_empty() {
                return Err(MixError::RuntimeError {
                    span: None,
                    msg: "send: command evaluates to empty string".to_string(),
                });
            }

            // SPEC 18 Phase 2 WS4 — single-pass walk of `args` in source
            // order that (1) consumes the optional `timeout=<sec>` control
            // kwarg locally so it never reaches the downstream handler, and
            // (2) builds the downstream args map at the same time. The
            // two-pass shape (`eval_send_timeout` + `eval_send_args`) would
            // reorder side effects whenever `timeout=` sat between named or
            // positional args — the timeout expression would evaluate
            // ahead of every other arg's expression. See
            // [`Self::eval_send_args_and_timeout`].
            let (timeout, args_map) = self.eval_send_args_and_timeout(args).await?;

            // SPEC 18 Phase 2 WS3-C.5 — clone-out so the `Ref` is dropped
            // at end of the `let` statement, before any `.await`.
            let bus_handler = self.globals.borrow().bus_handler.clone();
            let handler = match bus_handler {
                Some(h) => h,
                None => {
                    // Bus unavailable BEFORE delivery → RC_UNAVAILABLE (-3),
                    // numeric (was the string "-1"). Non-fatal: continue.
                    let err_msg = "Bus not available (no handler registered)".to_string();
                    self.scope
                        .update_or_set("rc", Value::Number(RC_UNAVAILABLE as f64));
                    self.scope.update_or_set("result", Value::String(err_msg));
                    return Ok(Value::Nil);
                }
            };

            // SPEC 18 Phase 2 WS3-C.7e — yield-on-send. Wrap the Bus
            // await with `await_with_class_c_yield` so a Class C async
            // body releases its reader permit across the send (letting
            // a queued Class S writer admit or peers continue) and
            // reacquires before reading `(rc, result)`. For Class S /
            // Class C-embedded-plain (writer held; no `class_c_read_permit`
            // on ctx), the helper is the await-through no-op fast path.
            // No `self` borrow held across the helper — `target_str`,
            // `command_str`, `args_map` are local owned values; `handler`
            // is an owned `Rc<dyn BusHandler>`.
            //
            // SPEC 18 Phase 2 WS4 — per-send timeout. When the caller
            // supplied `timeout=<sec>`, wrap the Bus future in
            // `tokio::time::timeout` *inside* the yield helper. On budget
            // exceeded, drop the Bus future (citizen-side RAII cleans up
            // the pending correlation entry and any sockets; a late
            // downstream reply lands with no matching correlation id and
            // is dropped at the broker — no resource leak) and write the
            // typed `(rc=-1, result="timeout: ...")` shape that mirrors
            // other transport failures (don't invent a new rc="timeout").
            let send_fut = handler.send(&target_str, &command_str, &args_map);
            let timeout_secs = timeout.map(|d| d.as_secs_f64());
            let outcome = match timeout {
                None => self.await_with_class_c_yield(send_fut).await?,
                Some(dur) => {
                    let timed = tokio::time::timeout(dur, send_fut);
                    match self.await_with_class_c_yield(timed).await? {
                        Ok(inner) => inner,
                        Err(_elapsed) => {
                            // Per-send timeout → RC_TIMEOUT (-2), numeric (was
                            // the string "-1", conflated with transport).
                            let secs = timeout_secs.expect("timeout set above");
                            let err_msg =
                                format!("timeout: send to {} exceeded {}s", target_str, secs);
                            self.scope
                                .update_or_set("rc", Value::Number(RC_TIMEOUT as f64));
                            self.scope.update_or_set("result", Value::String(err_msg));
                            return Ok(Value::Nil);
                        }
                    }
                }
            };
            match outcome {
                Ok((rc, result)) => {
                    // The handler's signed rc verbatim: 0 ok, >=10 broker/app
                    // error, or RC_UNAVAILABLE (-3) for its own no-broker
                    // degrade. Always numeric.
                    self.scope.update_or_set("rc", Value::Number(rc as f64));
                    self.scope.update_or_set("result", result.clone());
                    Ok(result)
                }
                Err(e) => {
                    // A route/handler existed but the transport failed (lost
                    // broker, handler transport error) → RC_TRANSPORT (-1),
                    // numeric (was the string "-1"). Non-fatal: continue.
                    let err_msg = e.to_string();
                    self.scope
                        .update_or_set("rc", Value::Number(RC_TRANSPORT as f64));
                    self.scope.update_or_set("result", Value::String(err_msg));
                    Ok(Value::Nil)
                }
            }
        })
    }

    /// SPEC 18 Phase 2 WS4 — single-pass evaluation of `send` args that
    /// also consumes the optional `timeout=<sec>` control kwarg.
    ///
    /// Walks `args` exactly once in source order. Every expression is
    /// evaluated where it appears, so any side effects observed by the
    /// caller (logging, counter increments, prompt-builtins) fire in the
    /// same order regardless of where `timeout=` sits relative to other
    /// kwargs. The `timeout=` slot does not contribute to the downstream
    /// args map — it's local to this `send` site — but the index used
    /// for empty-keyed positional naming (`_0`, `_1`, ...) skips over
    /// timeout entries so the downstream map shape is identical to
    /// "filter out timeout, then evaluate". Duplicate `timeout=` kwargs
    /// are last-wins (mirrors `IndexMap`'s last-write behaviour for
    /// duplicate named keys in the same `send`).
    ///
    /// Validation (raised as `RuntimeError`, identical to the prior
    /// two-pass behaviour):
    /// - Nil is treated as absent (`None`) — lets callers write
    ///   `send target cmd timeout=$t` where `$t` may be unbound.
    /// - Non-numeric values raise an error.
    /// - Non-finite (NaN / ±inf) values raise an error.
    /// - Zero or negative values raise an error — "no timeout" is
    ///   expressed by absence, not by a sentinel.
    async fn eval_send_args_and_timeout(
        &mut self,
        args: &[(String, Expr)],
    ) -> MixResult<(Option<std::time::Duration>, Value)> {
        let mut map = IndexMap::new();
        let mut last_timeout: Option<f64> = None;
        // Index used to name empty-keyed positional args. Incremented
        // only for non-timeout entries so the naming matches the
        // pre-WS4 "filtered slice" shape (timeout doesn't occupy a
        // positional slot in the downstream map).
        let mut filtered_idx: usize = 0;
        for (key, expr) in args {
            let val = self.eval_expr(expr).await?;
            if key == "timeout" {
                match val {
                    Value::Nil => {
                        last_timeout = None;
                    }
                    Value::Number(n) => {
                        if !n.is_finite() {
                            return Err(MixError::RuntimeError {
                                span: None,
                                msg: "send: timeout= must be a finite number".to_string(),
                            });
                        }
                        if n <= 0.0 {
                            return Err(MixError::RuntimeError {
                                span: None,
                                msg: "send: timeout= must be positive (absence means no timeout)"
                                    .to_string(),
                            });
                        }
                        last_timeout = Some(n);
                    }
                    other => {
                        return Err(MixError::RuntimeError {
                            span: None,
                            msg: format!(
                                "send: timeout= must be a positive number, got {}",
                                other.type_name()
                            ),
                        });
                    }
                }
            } else {
                let k = if key.is_empty() {
                    format!("_{}", filtered_idx)
                } else {
                    key.clone()
                };
                filtered_idx += 1;
                map.insert(k, val);
            }
        }
        // Knob D: source-bounded, but recurse-check guards a large arg
        // value that reached here via any construction path.
        let m = Value::map(map);
        self.check_collection_size(&m)?;
        Ok((last_timeout.map(std::time::Duration::from_secs_f64), m))
    }

    /// Execute an emit (fire-and-forget): evaluate args, call Bus handler.
    fn exec_emit<'a>(
        &'a mut self,
        target: &'a Expr,
        command: &'a Expr,
        args: &'a [(String, Expr)],
    ) -> Pin<Box<dyn Future<Output = MixResult<Value>> + 'a>> {
        Box::pin(async move {
            let target_str = self.eval_expr(target).await?.to_mix_string();
            let command_str = self.eval_expr(command).await?.to_mix_string();
            if command_str.is_empty() {
                return Err(MixError::RuntimeError {
                    span: None,
                    msg: "emit: command evaluates to empty string".to_string(),
                });
            }
            let args_map = self.eval_send_args(args).await?;

            // FIRE-AND-FORGET is non-fatal on EVERY delivery failure
            // (2026-07-02 audit, Codex-ruled): NO handler registered, a
            // dead/absent broker (NeverPresent / Lost), or a transport error
            // must NOT abort the script — `emit` promises no delivery
            // guarantee, and `send` is the status-bearing form. A missing
            // handler is therefore a silent no-op here (was a hard raise),
            // symmetric with `send`'s no-handler degrade. emit writes no
            // `$rc`/`$result`. (Arg/command-parse errors already raised ABOVE,
            // before the handler was consulted — those still abort.)
            // Clone-out in its own `let` so the `Ref` guard drops before the
            // `.await` below (SPEC 18 Phase 2 WS3-C.5).
            let handler_opt = self.globals.borrow().bus_handler.clone();
            let handler = match handler_opt {
                Some(h) => h,
                None => return Ok(Value::Nil),
            };

            // SPEC 18 Phase 2 WS3-C.7e — yield-on-send (fire-and-forget
            // variant). Same release/reacquire dance as `exec_send`. The
            // handler's `Err` (NeverPresent / Lost / connected transport
            // failure) is swallowed — only the yield machinery's own error
            // (the outer `?`) is fatal.
            let emit_fut = handler.emit(&target_str, &command_str, &args_map);
            let _ = self.await_with_class_c_yield(emit_fut).await?;
            Ok(Value::Nil)
        })
    }

    /// Evaluate send/emit argument list into a Value::Map.
    async fn eval_send_args(&mut self, args: &[(String, Expr)]) -> MixResult<Value> {
        let mut map = IndexMap::new();
        for (i, (key, expr)) in args.iter().enumerate() {
            let val = self.eval_expr(expr).await?;
            let k = if key.is_empty() {
                format!("_{}", i)
            } else {
                key.clone()
            };
            map.insert(k, val);
        }
        // Knob D: source-bounded, but recurse-check guards a large arg value.
        let m = Value::map(map);
        self.check_collection_size(&m)?;
        Ok(m)
    }

    /// Execute `source "file.mix"` — read, parse, and execute in current
    /// scope. CWD-relative, runs every time (shell `source` semantics).
    async fn exec_source(&mut self, path: &str) -> MixResult<Value> {
        self.exec_file_in_scope(path, "source", None).await
    }

    /// Resolve an `include` path: absolute as-is; otherwise relative to
    /// the INCLUDING file's directory if that resolves to an existing
    /// file; otherwise CWD-relative (the original `source` behavior).
    fn resolve_include_path(&self, path: &str) -> String {
        let p = std::path::Path::new(path);
        if p.is_absolute() {
            return path.to_string();
        }
        if let Some(dir) = self
            .ctx
            .current_file
            .as_deref()
            .map(|c| {
                // Follow a symlinked ENTRY script to its real location
                // (0.74.0): the symlinked-launcher pattern
                // (~/.local/bin/x -> repo/_bin/x.mix) must resolve
                // "../_lib/…" against the repo the script lives in, not
                // against the directory the symlink sits in — before this,
                // every such launcher was silently CWD-dependent.
                let pb = std::path::Path::new(c);
                std::fs::canonicalize(pb).unwrap_or_else(|_| pb.to_path_buf())
            })
            .and_then(|c| c.parent().map(|d| d.to_path_buf()))
        {
            let candidate = dir.join(path);
            // `is_file` (not `exists`): a same-named *directory* next to the
            // script must not shadow a real file reachable via the CWD fallback.
            if candidate.is_file() {
                return candidate.to_string_lossy().into_owned();
            }
        }
        path.to_string()
    }

    /// Execute `include "lib.mix"` — like `source`, but script-relative
    /// (see [`Self::resolve_include_path`]) and loaded at most once per
    /// program run. Module semantics for sharing helper functions.
    async fn exec_include(&mut self, path: &str) -> MixResult<Value> {
        // FsRead-gate BEFORE any filesystem touch — `resolve_include_path`
        // stats (`is_file`), `canonicalize`, and the dedup check all probe
        // the filesystem, so a Pure-only policy must be refused here, not
        // at the later read in `exec_file_in_scope` (which still re-checks,
        // covering `source`; the redundant include re-check is harmless).
        self.check_capability_class(crate::builtins::CapabilityClass::FsRead, "include")?;
        let resolved = self.resolve_include_path(path);
        // Dedup key: canonicalize so `./a`, `a`, and `/abs/a` collapse to
        // one entry; fall back to the resolved string if the file can't be
        // canonicalized (it doesn't exist yet — the read below reports it).
        // Uses the SAME canonicalisation as the `realpath` builtin (dogfooded).
        let key = crate::builtins::canonicalize_path(&resolved).unwrap_or_else(|| resolved.clone());
        if self.globals.borrow().included_files.contains(&key) {
            return Ok(Value::Nil);
        }
        // The key is marked inside exec_file_in_scope ONLY once read+parse
        // succeed and just before the body runs — so a failed include is
        // retryable (not silently no-op'd), while a cyclic include (A→B→A)
        // still terminates because the mark precedes body execution.
        self.exec_file_in_scope(&resolved, "include", Some(key))
            .await
    }

    /// Shared body for `source`/`include`: read `read_path`, parse, and
    /// execute the statements in the current scope. `label` is the keyword
    /// name used in the file-read diagnostic. `dedup_key` is `Some` for
    /// `include` — the canonical key is recorded in `included_files` ONLY
    /// once read+parse succeed and just before the body executes, so a
    /// failed include stays retryable while a cyclic include still
    /// terminates; `None` for `source` (never deduped). `include`
    /// (`Some`) is a pure-Mix loader and does NOT take the per-line shell
    /// fallback; that path is `source`-only. Restores the caller's
    /// file/line on EVERY exit path (normal, `?`, panic).
    async fn exec_file_in_scope(
        &mut self,
        read_path: &str,
        label: &str,
        dedup_key: Option<String>,
    ) -> MixResult<Value> {
        // `include`/`source` read a file from disk — that is FsRead host
        // authority, so gate them under the same capability class as
        // `read_file` (an ungated file-reading eval path would let a
        // Pure-only sandbox load + run arbitrary on-disk code). No policy
        // installed (the `mix` binary, plain library use) ⇒ permissive.
        self.check_capability_class(crate::builtins::CapabilityClass::FsRead, label)?;
        let content = std::fs::read_to_string(read_path).map_err(|e| MixError::RuntimeError {
            span: None,
            msg: format!("{}: {}: {}", label, read_path, e),
        })?;
        let logical_content = crate::continuation::splice_with_metadata(&content)?;
        let forced_shell_lines = if dedup_key.is_none() {
            self.continued_lines_classified_as_shell(
                &logical_content.source,
                &logical_content.continued_lines,
            )
        } else {
            Vec::new()
        };

        let old_file = self.ctx.current_file.take();
        let old_line = self.ctx.current_line;
        self.ctx.current_file = Some(std::rc::Rc::from(read_path));
        self.ctx.current_line = 0;

        // Restore the caller's file/line on EVERY exit path — normal
        // return, a `?` parse error, AND a panic in the sourced body.
        // Without this a `mix --serve` handler that `source`s a file
        // which fails to parse or panics would leave every later
        // diagnostic misattributed to the sourced path (SPEC 18 §3.4: a
        // caught panic — and an early error return — must not strand an
        // execute-scoped restore bracket).
        //
        // This parse is *provisional*, not authoritative: on success these
        // statements are the ones executed, but on failure the per-line shell
        // fallback below reparses and this attempt is discarded. So parse it
        // speculatively and flush the withheld deprecations only once it has
        // won — otherwise a file that falls back reports the same deprecation
        // from every attempt that touched it.
        let parsed = crate::lexer::Lexer::new(&content)
            .tokenize()
            .and_then(|tokens| {
                let mut parser = crate::parser::Parser::new_speculative(tokens, &content);
                let result = parser.parse_program();
                if result.is_ok() {
                    for warning in parser.take_deprecations() {
                        eprintln!("{}", warning);
                    }
                }
                result
            });
        let stmts = match parsed {
            Ok(stmts) if forced_shell_lines.is_empty() => stmts,
            parsed => {
                let e = match parsed {
                    Ok(_) => MixError::ParseError {
                        msg: "continued command line requires shell classification".to_string(),
                        span: Span {
                            line: forced_shell_lines[0],
                            column: 1,
                            file: None,
                        },
                    },
                    Err(error) => error,
                };
                // Whole-file Mix parse failed. The per-line shell fallback
                // is a `source`-ONLY path (`dedup_key.is_none()`): `source`
                // loads `.mixrc`-style files that mix bareword shell lines
                // with Mix. `include` is a pure-Mix module loader — a parse
                // failure is a hard error (and, since nothing was marked or
                // executed, the file stays retryable and free of partial
                // side effects). This also keeps `source` byte-identical to
                // its pre-`include` behavior.
                //
                // If a `ShellHandler` is registered (REPL / script-runner
                // installs one; daemon/serve modes leave it unset), fall back
                // to the two-phase per-line loop. The original error `e` is
                // handed to the fallback so it can be re-raised verbatim when
                // the file turns out to contain no shell lines — partial
                // execution of a syntax-invalid pure-Mix file would be a
                // regression vs. the pre-fallback behavior. The whole
                // fallback-plus-restore is wrapped in `catch_unwind` so a
                // panic anywhere inside (parser, shell handler, Mix execute)
                // still restores the caller's file/line bracket before
                // resuming the panic.
                if dedup_key.is_none() && self.globals.borrow().shell_handler.is_some() {
                    let outcome = std::panic::AssertUnwindSafe(self.exec_source_per_line(
                        &logical_content.source,
                        e,
                        &forced_shell_lines,
                    ))
                    .catch_unwind()
                    .await;
                    self.ctx.current_file = old_file;
                    self.ctx.current_line = old_line;
                    return match outcome {
                        Ok(result) => result,
                        Err(payload) => std::panic::resume_unwind(payload),
                    };
                }
                self.ctx.current_file = old_file;
                self.ctx.current_line = old_line;
                return Err(e);
            }
        };
        // Read+parse succeeded: mark an `include` as loaded BEFORE the body
        // runs, so a cyclic include (A→B→A) dedups and terminates. A failed
        // read/parse above never reaches here, keeping failures retryable.
        if let Some(k) = &dedup_key {
            self.globals.borrow_mut().included_files.insert(k.clone());
        }
        // `AssertUnwindSafe`: the `execute` future borrows `&mut self`
        // (`!UnwindSafe`). Sound because we do not resume on partial
        // local state — file/line are restored unconditionally and the
        // panic is re-raised so the `run_handlers_for` §3.4 boundary
        // still rewinds the centrally-tracked evaluator state and
        // synthesises the error reply.
        let outcome = std::panic::AssertUnwindSafe(self.execute(&stmts))
            .catch_unwind()
            .await;
        self.ctx.current_file = old_file;
        self.ctx.current_line = old_line;
        match outcome {
            Ok(result) => result,
            Err(payload) => std::panic::resume_unwind(payload),
        }
    }

    /// `require(path)` — evaluate a Mix module ONCE in a fresh, fully
    /// isolated `Scope::new()` and return its exports (Card 3; spec:
    /// cosmix `_plan/2026-07-06-mix-4-feature-rebuild.md` + the
    /// regenerated card3-require spec).
    ///
    /// FIX 1 (the load-bearing constraint): the module executes against
    /// a swapped-in `Scope::new()` — NEVER `with_shared`, whose empty
    /// local-root frame strands module top-level `$var`s where no
    /// harvest can see them — and the exports are lifted out by
    /// consuming that scope (`Scope::into_module_parts`). The caller's
    /// scope, file/line, function depth, address stack, sync-self slot
    /// and slot caches are restored on EVERY exit path (normal, error,
    /// panic). Cached once per canonical path (success only, so a
    /// failed module is retryable); cycles and >64-deep nesting are
    /// hard errors. The module body runs under the caller's EvalLimits,
    /// capability policy, interrupt flag and deadline; `function_depth`
    /// resets to 0 for the module top level, which grants each require
    /// level a fresh recursion budget — bounded by the 64-deep require
    /// cap.
    pub(crate) async fn exec_require(&mut self, path: &str) -> MixResult<Value> {
        // Gate BEFORE any filesystem touch (mirror exec_include).
        self.check_capability_class(crate::builtins::CapabilityClass::FsRead, "require")?;
        let resolved = self.resolve_include_path(path);
        // SAME canonicalisation as the `realpath` builtin (dogfooded) — the per-path module
        // cache and realpath() can never disagree on what "the same file" means.
        let key = crate::builtins::canonicalize_path(&resolved).unwrap_or_else(|| resolved.clone());
        // Cache hit → clone out. Sync borrow, released immediately (γ).
        let cached = { self.globals.borrow().module_cache.get(&key).cloned() };
        if let Some(v) = cached {
            return Ok(v);
        }
        if self.ctx.require_stack.iter().any(|k| k == &key) {
            return Err(self.runtime_err(format!(
                "require: circular module dependency: {} -> {}",
                self.ctx.require_stack.join(" -> "),
                key
            )));
        }
        if self.ctx.require_stack.len() >= 64 {
            return Err(self.runtime_err("require: module nesting too deep (64)"));
        }
        let content = std::fs::read_to_string(&resolved).map_err(|e| MixError::RuntimeError {
            span: None,
            msg: format!("require: {}: {}", resolved, e),
        })?;
        // Lex+parse under the MODULE's file attribution so a parse
        // error points at the module, then keep the same bracket for
        // the body. Restored on every exit path below.
        let saved_file = self
            .ctx
            .current_file
            .replace(std::rc::Rc::from(resolved.as_str()));
        let saved_line = std::mem::replace(&mut self.ctx.current_line, 0);
        let parsed = crate::lexer::Lexer::new(&content)
            .tokenize()
            .and_then(|tokens| crate::parser::Parser::new(tokens, &content).parse_program());
        let stmts = match parsed {
            Ok(stmts) => stmts,
            Err(e) => {
                self.ctx.current_file = saved_file;
                self.ctx.current_line = saved_line;
                return Err(e);
            }
        };
        // `on` handlers registered from a module would later run
        // against the CALLER's scope without the module environment —
        // a semantic trap; refuse in v1.
        if stmts.iter().any(|s| matches!(&s.kind, StmtKind::On { .. })) {
            self.ctx.current_file = saved_file;
            self.ctx.current_line = saved_line;
            return Err(MixError::RuntimeError {
                span: None,
                msg: format!(
                    "require: {}: 'on' handlers cannot be registered from a module",
                    resolved
                ),
            });
        }
        // Commit: cycle-stack entry + full caller-state save. FIX 1:
        // the module scope is a plain Scope::new().
        self.ctx.require_stack.push(key.clone());
        // (mem::take: Scope::default() IS Scope::new() — the FIX 1
        // standalone scope.)
        let saved_scope = std::mem::take(&mut self.scope);
        let saved_depth = std::mem::take(&mut self.ctx.function_depth);
        let saved_addr = std::mem::take(&mut self.ctx.address_stack);
        let saved_self = self.ctx.sync_self_func.take();
        // A module's top level runs ONCE per path (cache), so a
        // `reply()` there would consume the enclosing handler's reply
        // handle on first require and silently no-op on every cache
        // hit. Take the handle for the module's duration: module
        // top-level `reply()` gets the standard "only from within an
        // `on` handler" error instead. (Exported functions called
        // later from a handler body see the live handle as usual.)
        let saved_reply = self.current_reply.take();
        let slot_len = self.ctx.var_slot_cache.len();
        let slot_f64_len = self.ctx.var_slot_cache_f64.len();
        let replay_prelude = { self.globals.borrow().prelude_loaded };
        // Prelude replay + module body under ONE catch_unwind so a
        // panic restores the caller state before resuming (mirror of
        // exec_file_in_scope's §3.4 discipline). AssertUnwindSafe: we
        // do not resume on partial state — everything is restored
        // unconditionally below and the panic is re-raised.
        let outcome = std::panic::AssertUnwindSafe(async {
            let mut prelude_fns: HashMap<String, Rc<MixFunction>> = HashMap::new();
            if replay_prelude {
                self.load_prelude().await;
                // Snapshot the prelude's registry entries (by Rc
                // identity) so auto-export can exclude them while a
                // module REDEFINING a prelude name (fresh Rc) still
                // exports its own version.
                for (n, _) in self.scope.function_names() {
                    if let Some(f) = self.scope.get_function_owned(&n) {
                        prelude_fns.insert(n, f);
                    }
                }
            }
            // Top-level fn hoisting applies to a module body exactly as
            // to a directly-run script — same file, same forward-call
            // semantics either way (the body runs via execute_block, so
            // execute_inner's own pre-pass never fires here). AFTER the
            // prelude snapshot above: a module fn shadowing a prelude
            // name must be bound as a fresh Rc so auto-export still
            // includes it.
            for stmt in &stmts {
                if let StmtKind::FunctionDef { name, params, body } = &stmt.kind {
                    let f = self.build_function(name, params, body);
                    self.scope.define_function(f);
                }
            }
            // Non-unwrapping executor: ONLY an explicit top-level
            // `return <expr>` overrides auto-export; an incidental
            // non-nil last-statement value must not.
            let ret = match self.execute_block(&stmts).await {
                Ok(_) => None,
                Err(MixError::Return { value }) => Some(value),
                Err(e) => return Err(e),
            };
            Ok((ret, prelude_fns))
        })
        .catch_unwind()
        .await;
        // Restore EVERYTHING unconditionally, in reverse.
        let module_scope = std::mem::replace(&mut self.scope, saved_scope);
        self.ctx.current_file = saved_file;
        self.ctx.current_line = saved_line;
        self.ctx.function_depth = saved_depth;
        self.ctx.address_stack = saved_addr;
        self.ctx.sync_self_func = saved_self;
        self.current_reply = saved_reply;
        self.ctx.var_slot_cache.truncate(slot_len);
        self.ctx.var_slot_cache_f64.truncate(slot_f64_len);
        self.ctx.require_stack.pop();
        let (ret, prelude_fns) = match outcome {
            Ok(Ok(pair)) => pair,
            Ok(Err(e)) => return Err(e),
            Err(payload) => std::panic::resume_unwind(payload),
        };
        // Harvest (FIX 1 mechanism) + cache on success only.
        let exports = Self::build_module_exports(module_scope, ret, &prelude_fns);
        self.check_collection_size(&exports)?; // Knob D
        self.globals
            .borrow_mut()
            .module_cache
            .insert(key, exports.clone());
        Ok(exports)
    }

    /// Build a module's exports from its consumed scope (see
    /// [`Self::exec_require`]).
    ///
    /// - An explicit top-level `return <expr>` (`ret_override`)
    ///   replaces auto-export entirely (any type — Lua-style).
    /// - Otherwise: a map of every top-level function and `$var`,
    ///   excluding `_`-prefixed names (private by convention), the
    ///   frame-noise names written implicitly by send/shell dispatch,
    ///   and un-redefined prelude functions.
    /// - Every exported/reachable `Value::Function` is rewrapped to
    ///   carry the shared write-once `module_env` — the map of ALL
    ///   module top-level names (privates included) that
    ///   `call_function` injects per call, so module functions can call
    ///   siblings and read module vars from the caller's scope.
    ///   Module-level state is per-call snapshots by value: writes
    ///   inside a call do not persist across calls.
    fn build_module_exports(
        module_scope: Scope,
        ret_override: Option<Value>,
        prelude_fns: &HashMap<String, Rc<MixFunction>>,
    ) -> Value {
        let (mut vars, funcs) = module_scope.into_module_parts();
        let env: Rc<std::cell::OnceCell<HashMap<String, Value>>> =
            Rc::new(std::cell::OnceCell::new());
        let is_prelude = |name: &str, f: &Rc<MixFunction>| {
            prelude_fns.get(name).is_some_and(|p| Rc::ptr_eq(p, f))
        };
        // Phase 1: wrap each module function ONCE (deep body clone) with
        // the shared env Rc; reuse the wrapped value for env and exports
        // (Value::Function clones are Rc bumps).
        let mut wrapped: Vec<(String, Value)> = funcs
            .iter()
            .filter(|(name, f)| !is_prelude(name, f))
            .map(|(name, f)| {
                (
                    name.clone(),
                    Value::Function(Rc::new(MixFunction {
                        module_env: Some(Rc::clone(&env)),
                        ..(**f).clone()
                    })),
                )
            })
            .collect();
        wrapped.sort_by(|a, b| a.0.cmp(&b.0)); // deterministic export order
        // Phase 2: rewrap Functions reachable through top-level $vars
        // (module lambdas, functions inside lists/maps). A function
        // that already carries an env — a nested require's exports
        // stored in a var — keeps ITS module's env.
        for v in vars.values_mut() {
            Self::rewrap_functions(v, &env);
        }
        // Phase 3: fill the env — functions first, vars second (a var
        // wins a name collision, matching the exports rule below).
        let mut env_map: HashMap<String, Value> = HashMap::new();
        for (name, f) in &wrapped {
            env_map.insert(name.clone(), f.clone());
        }
        for (name, v) in &vars {
            env_map.insert(name.clone(), v.clone());
        }
        let _ = env.set(env_map); // single fill; cannot already be set
        // Phase 4: exports.
        if let Some(mut ret) = ret_override {
            Self::rewrap_functions(&mut ret, &env);
            return ret;
        }
        const EXCLUDED_VARS: &[&str] = &["rc", "result", "status", "event"];
        let mut exports = IndexMap::new();
        for (name, f) in wrapped {
            if !name.starts_with('_') {
                exports.insert(name, f);
            }
        }
        let mut var_names: Vec<String> = vars.keys().cloned().collect();
        var_names.sort();
        for name in var_names {
            if !name.starts_with('_')
                && !EXCLUDED_VARS.contains(&name.as_str())
                && let Some(v) = vars.remove(&name)
            {
                exports.insert(name, v);
            }
        }
        Value::map(exports)
    }

    /// Recursively rewrap every `Value::Function` reachable in `v` to
    /// carry `env` (module lambdas and functions nested in lists/maps).
    /// Functions that already carry a module env — a nested `require`'s
    /// exports stored in this module's vars — are left with their own.
    /// Values are trees (value semantics — no cycles); a `Buffer` is a
    /// byte store and cannot contain functions.
    fn rewrap_functions(v: &mut Value, env: &Rc<std::cell::OnceCell<HashMap<String, Value>>>) {
        match v {
            Value::Function(rc) => {
                if rc.module_env.is_none() {
                    *v = Value::Function(Rc::new(MixFunction {
                        module_env: Some(Rc::clone(env)),
                        ..(**rc).clone()
                    }));
                }
            }
            Value::List(items) => {
                // CoW: exports are freshly harvested (sole owner) —
                // make_mut is in-place here in practice.
                for it in Rc::make_mut(items) {
                    Self::rewrap_functions(it, env);
                }
            }
            Value::Map(map) => {
                for (_k, val) in Rc::make_mut(map).iter_mut() {
                    Self::rewrap_functions(val, env);
                }
            }
            _ => {}
        }
    }

    /// Two-phase per-line `source` fallback. See the [`ShellHandler`]
    /// trait doc for the contract.
    ///
    /// **Phase 1 — unambiguous-shell probe with evolving aliases.**
    /// Walk every non-blank line carrying a working copy of the
    /// caller's aliases. A line is treated as unambiguous shell
    /// **only when** `classify` returns `Shell` AND the same line
    /// does not also parse standalone as Mix. (A bareword that
    /// exists on `PATH` but is also a valid Mix identifier — e.g.
    /// `ls` — is ambiguous; treating it as proof of shell content
    /// would let a syntax-invalid pure-Mix file partial-execute
    /// the bareword before failing on a later line.) A line named in
    /// `forced_shell_lines` is the explicit-continuation exception: the
    /// complete logical command must reach the shell classifier even when it
    /// is also valid Mix arithmetic. Along the way
    /// we extract literal `alias name "value"` Mix statements into
    /// the working alias map so a line of the form `gg world`
    /// classifies correctly after an earlier `alias gg "ls"` set
    /// it. If no unambiguous shell line is found, the file is
    /// "supposed to be Mix" — propagate `original_err`.
    ///
    /// **Phase 2 — interleaved strict classify + execute.** Walk
    /// lines in order. Each line is *re*-classified just before it
    /// runs against the evaluator's live (now-mutating) alias map.
    /// Shell lines run via [`ShellHandler::execute_shell`] with
    /// `self` passed as a [`ShellVarResolver`] so `$VAR` expansion
    /// matches the REPL's `ExternalCommand` arm. The exit code is
    /// published to the `status` scope variable. Mix lines are
    /// re-parsed against just that line and executed in the source
    /// caller's scope. Any classify or execute error aborts the
    /// source.
    async fn exec_source_per_line(
        &mut self,
        content: &str,
        original_err: MixError,
        forced_shell_lines: &[usize],
    ) -> MixResult<Value> {
        // SPEC 18 Phase 2 WS3-C.5 — clone the handle out so the
        // `Ref` drops at end of this `let` statement. The downstream
        // loop body re-borrows `&mut self` to execute Mix lines and
        // re-issues its own `self.globals.borrow*()` for sync reads,
        // and the cloned `Rc` keeps the handler alive across any
        // `.await` inside the per-line shell-exec path without holding
        // a `Ref` over the yield. Refcount bump only — handler state
        // lives behind the `Rc<dyn ShellHandler>`.
        let handler = self
            .globals
            .borrow()
            .shell_handler
            .as_ref()
            .expect("exec_source_per_line called without shell_handler")
            .clone();

        // Materialize lines once so Phase 1 and Phase 2 walk the
        // same view of the file (no double-tokenize of `\r`-stripped
        // boundaries).
        let lines: Vec<(usize, String)> = content
            .lines()
            .enumerate()
            .filter_map(|(idx, raw)| {
                let s = raw.trim_end_matches('\r').to_string();
                if s.trim().is_empty() {
                    None
                } else {
                    Some((idx + 1, s))
                }
            })
            .collect();

        // Phase 1: probe for an UNAMBIGUOUS shell line while
        // tracking alias evolution from literal `alias` statements
        // in earlier Mix lines.
        let mut probe_aliases = self.globals.borrow().aliases.clone();
        let mut has_unambiguous_shell = false;
        for (line_no, line) in &lines {
            let classified = handler.classify(line, &probe_aliases);
            // Pre-parse once: used by both the unambiguous-shell
            // gate (Mix-parseable lines are ambiguous) and the
            // alias-evolution scan below. Speculative — this is a
            // routing probe over lines that may turn out to be
            // shell, and the line is reparsed for execution below.
            let parsed = crate::lexer::Lexer::new(line)
                .tokenize()
                .and_then(|tokens| {
                    crate::parser::Parser::new_speculative(tokens, line).parse_program()
                });
            if matches!(classified, Ok(ShellLineKind::Shell))
                && (parsed.is_err() || forced_shell_lines.binary_search(line_no).is_ok())
            {
                has_unambiguous_shell = true;
                break;
            }
            // Extract `alias name "value"` statements with both
            // literal name and literal command. Dynamic forms
            // (`alias $h = ...`) and the query/list forms are
            // skipped — best-effort: missing them only fails to
            // open Phase 2 for files that need alias-expansion to
            // detect shell content, which still surfaces the
            // original whole-file Mix parse error.
            if let Ok(stmts) = parsed {
                for stmt in stmts {
                    if let crate::ast::StmtKind::Alias {
                        name: Some(crate::ast::Expr::StringLiteral(n)),
                        command: Some(crate::ast::Expr::StringLiteral(c)),
                    } = stmt.kind
                    {
                        probe_aliases.insert(n, c);
                    }
                }
            }
        }
        if !has_unambiguous_shell {
            return Err(original_err);
        }

        // Phase 2: interleaved strict pass.
        let mut last_value = Value::Nil;
        for (line_no, line) in lines {
            self.ctx.current_line = line_no;
            let kind = handler
                .classify(&line, &self.globals.borrow().aliases)
                .map_err(|msg| MixError::RuntimeError {
                    span: None,
                    msg: format!("source: line {}: {}", line_no, msg),
                })?;
            match kind {
                ShellLineKind::Empty => continue,
                ShellLineKind::Shell => {
                    // A bare shell-dispatch line spawns `/bin/sh` via the
                    // handler — gate it as Process so a restrictive
                    // embedder policy covers this path too.
                    self.check_capability_class(
                        crate::builtins::CapabilityClass::Process,
                        "shell command",
                    )?;
                    // Snapshot aliases so `&self` can also be
                    // re-borrowed as `&dyn ShellVarResolver` for
                    // `$VAR` expansion in the handler.
                    let aliases_snap = self.globals.borrow().aliases.clone();
                    let resolver: &dyn ShellVarResolver = &*self;
                    let (outcome, commands) = handler
                        .execute_shell_tracked(&line, &aliases_snap, resolver)
                        .map_err(|msg| MixError::RuntimeError {
                            span: None,
                            msg: format!("source: line {}: {}", line_no, msg),
                        })?;
                    self.track_command_attempts(&commands);
                    match outcome {
                        ShellExecResult::Status(code) => self
                            .scope
                            .update_or_set("status", Value::Number(code as f64)),
                        ShellExecResult::ExitRequested(code) => {
                            return Err(MixError::ExitRequest { code });
                        }
                    }
                }
                ShellLineKind::Mix => {
                    let stmts = crate::lexer::Lexer::new(&line)
                        .tokenize()
                        .and_then(|tokens| {
                            crate::parser::Parser::new(tokens, &line).parse_program()
                        })?;
                    if stmts.is_empty() {
                        continue;
                    }
                    let outcome = std::panic::AssertUnwindSafe(self.execute(&stmts))
                        .catch_unwind()
                        .await;
                    match outcome {
                        Ok(Ok(v)) => last_value = v,
                        Ok(Err(e)) => return Err(e),
                        Err(payload) => std::panic::resume_unwind(payload),
                    }
                }
            }
        }
        Ok(last_value)
    }

    /// Execute `sh "command"` as a statement — inherits stdio, streams output.
    fn exec_sh_stmt(&mut self, cmd: &str) -> MixResult<Value> {
        self.check_capability_class(crate::builtins::CapabilityClass::Process, "sh")?;
        let status = std::process::Command::new("sh")
            .arg("-c")
            .arg(cmd)
            .status()
            .map_err(|e| MixError::RuntimeError {
                span: None,
                msg: format!("sh: {}", e),
            })?;
        let code = status.code().unwrap_or(-1);
        self.scope.update_or_set("rc", Value::Number(code as f64));
        Ok(Value::Nil)
    }

    /// Execute `sh "command"` as an expression — captures stdout.
    fn exec_sh_expr(&mut self, cmd: &str) -> MixResult<Value> {
        self.check_capability_class(crate::builtins::CapabilityClass::Process, "$()")?;
        let output = std::process::Command::new("sh")
            .arg("-c")
            .arg(cmd)
            .output()
            .map_err(|e| MixError::RuntimeError {
                span: None,
                msg: format!("sh: {}", e),
            })?;
        let code = output.status.code().unwrap_or(-1);
        self.scope.update_or_set("rc", Value::Number(code as f64));
        let stdout = String::from_utf8_lossy(&output.stdout)
            .trim_end()
            .to_string();
        Ok(Value::String(stdout))
    }

    /// Execute a statement and pipe its output to an external command.
    fn exec_pipe_to_external<'a>(
        &'a mut self,
        stmt: &'a Stmt,
        command: &'a str,
    ) -> Pin<Box<dyn Future<Output = MixResult<Value>> + 'a>> {
        Box::pin(async move {
            // Gate before running the inner statement: the pipe target
            // spawns `/bin/sh` (Process class), so a denied capability
            // must short-circuit before any side effects.
            self.check_capability_class(crate::builtins::CapabilityClass::Process, "pipe")?;
            // Capture stdout from the inner statement. Restore the real
            // stdout on EVERY exit path — normal return, panic in the
            // piped statement, AND future-cancellation while suspended
            // at any `.await` inside `exec_stmt`. A stranded capture
            // buffer would silently swallow every later handler's output
            // in a `mix --serve` citizen (SPEC 18 §3.4).
            //
            // The cancellation hazard is real: when the surrounding
            // future is dropped (SIGINT during serve, a `select!` losing
            // race, etc.) Rust runs `Drop` for every owned local but
            // skips any code after the `.await` suspension point — so a
            // post-await `mem::replace`-back is not run. Holding the
            // restore in a `Drop` impl makes it automatic across every
            // exit shape. `AssertUnwindSafe` remains sound: we do not
            // resume on partial local state; the panic is re-raised so
            // the §3.4 boundary still rewinds centrally-tracked state.
            //
            // SPEC 18 Phase 2 WS3-C.5 — both the swap and the guard's
            // restore are short, sync `RefCell` writes; the inner
            // `exec_stmt(stmt)` future never holds a borrow across its
            // own awaits, so the guard's `borrow_mut()` cannot collide
            // with a live borrow at drop time.
            let capture = SharedBuf::new();
            let original = std::mem::replace(
                &mut self.globals.borrow_mut().stdout,
                Box::new(capture.clone()),
            );
            let _stdout_guard = StdoutSwapGuard {
                globals: self.globals.clone(),
                original: Some(original),
            };
            let outcome = std::panic::AssertUnwindSafe(self.exec_stmt(stmt))
                .catch_unwind()
                .await;
            // `_stdout_guard` drops at end of this scope, restoring stdout
            // on the normal-completion path as well as panic/cancellation.
            let result = match outcome {
                Ok(result) => result,
                Err(payload) => std::panic::resume_unwind(payload),
            };

            let result = result?;

            // Use captured stdout, or the return value if nothing was printed.
            // `Value::Bytes` writes the raw buffer verbatim — anything else
            // funnels through the existing text/JSON pipeline. Without the
            // Bytes branch, piping `read_file_bytes(...) | hexdump` would
            // pass `<bytes:N>` to the child's stdin (the same placeholder
            // bug class `Value::Bytes` was added to escape).
            let captured = capture.to_string_lossy();
            let input: Vec<u8> = if !captured.is_empty() {
                captured.into_bytes()
            } else if let Value::Bytes(buf) = &result {
                buf.to_vec()
            } else if let Value::Buffer(b) = &result {
                b.borrow().clone()
            } else if result != Value::Nil {
                // Use JSON for structured data (maps/lists) so tools like jq work
                #[cfg(feature = "json")]
                {
                    match &result {
                        Value::Map(_) | Value::List(_) => {
                            // Best-effort pipe feed: a value JSON can't carry
                            // (NaN/inf) falls back to the Mix repr rather than
                            // aborting the pipe (json_encode is the loud path).
                            crate::json::mix_to_json(&result)
                                .ok()
                                .and_then(|jv| serde_json::to_string(&jv).ok())
                                .unwrap_or_else(|| result.to_mix_string())
                                .into_bytes()
                        }
                        _ => result.to_mix_string().into_bytes(),
                    }
                }
                #[cfg(not(feature = "json"))]
                {
                    result.to_mix_string().into_bytes()
                }
            } else {
                Vec::new()
            };

            // Pipe to external command via sh -c
            use std::process::{Command, Stdio};
            let mut child = Command::new("sh")
                .arg("-c")
                .arg(command)
                .stdin(Stdio::piped())
                .spawn()
                .map_err(|e| MixError::RuntimeError {
                    span: None,
                    msg: format!("pipe: {}", e),
                })?;

            if let Some(mut stdin) = child.stdin.take() {
                use std::io::Write;
                let _ = stdin.write_all(&input);
            }

            let status = child.wait().map_err(|e| MixError::RuntimeError {
                span: None,
                msg: format!("pipe: {}", e),
            })?;

            let code = status.code().unwrap_or(-1);
            self.scope.update_or_set("rc", Value::Number(code as f64));
            Ok(Value::Nil)
        })
    }

    fn exec_parse(&mut self, source: &str, parts: &[ParsePart]) {
        // Collect variable names and delimiters in order
        let mut vars: Vec<&str> = Vec::new();
        let mut delimiters: Vec<Option<&str>> = Vec::new(); // delimiter BEFORE each variable

        // First pass: separate into vars and the delimiters between them
        let mut pending_delim: Option<&str> = None;
        for part in parts {
            match part {
                ParsePart::Variable(name) => {
                    delimiters.push(pending_delim.take());
                    vars.push(name);
                }
                ParsePart::Delimiter(s) => {
                    pending_delim = Some(s);
                }
            }
        }

        if vars.is_empty() {
            return;
        }

        let mut remaining = source;

        // If no delimiters at all, do word splitting
        let has_delimiters = delimiters.iter().any(|d| d.is_some());

        if !has_delimiters {
            // Word splitting mode: split on whitespace
            let words: Vec<&str> = remaining
                .splitn(vars.len(), char::is_whitespace)
                .map(|w| w.trim())
                .collect();
            for (i, var) in vars.iter().enumerate() {
                let val = words.get(i).unwrap_or(&"");
                self.bind_scoped(var, Value::String(val.to_string()));
            }
        } else {
            // Delimiter mode: split on literal delimiters
            for (i, var) in vars.iter().enumerate() {
                if let Some(Some(delim)) = delimiters.get(i) {
                    // Skip past the delimiter
                    if let Some(pos) = remaining.find(delim) {
                        remaining = &remaining[pos + delim.len()..];
                    }
                }

                // Find the next delimiter to know where this variable's value ends
                let next_delim = delimiters.get(i + 1).and_then(|d| d.as_ref());

                if i == vars.len() - 1 {
                    // Last variable gets the remainder
                    self.bind_scoped(var, Value::String(remaining.to_string()));
                } else if let Some(delim) = next_delim {
                    // Split at the next delimiter
                    if let Some(pos) = remaining.find(delim) {
                        let val = &remaining[..pos];
                        self.bind_scoped(var, Value::String(val.to_string()));
                        remaining = &remaining[pos..];
                    } else {
                        // Delimiter not found, give remainder to this var
                        self.bind_scoped(var, Value::String(remaining.to_string()));
                        remaining = "";
                    }
                } else {
                    // No next delimiter; split on whitespace to next word
                    let trimmed = remaining.trim_start();
                    if let Some(pos) = trimmed.find(char::is_whitespace) {
                        let val = &trimmed[..pos];
                        self.bind_scoped(var, Value::String(val.to_string()));
                        remaining = &trimmed[pos..];
                    } else {
                        self.bind_scoped(var, Value::String(trimmed.to_string()));
                        remaining = "";
                    }
                }
            }
        }
    }

    fn ensure_map(&mut self, name: &str) {
        // SPEC 18 Phase 2 WS3-C.7b — γ-aware existence check; the legacy
        // `get()` returns None for a value that lives in the shared
        // global map, which would clobber it with an empty Map.
        if self.scope.get_value(name).is_none() {
            self.scope.update_or_set(name, Value::map(IndexMap::new()));
        }
    }
}

fn shown_value(value: &Value) -> String {
    match value {
        Value::String(s) => format!("{s:?}"),
        _ => value.to_mix_string(),
    }
}

fn required_number(context: &str, value: &Value) -> MixResult<f64> {
    extract_number(value, InputPolicy::StandardCoercion).ok_or_else(|| {
        MixError::structured(
            "TYPE_MISMATCH",
            format!(
                "{context} must be a number, got {} ({})",
                shown_value(value),
                value.type_name()
            ),
        )
    })
}

fn number_operand(value: &Value) -> MixResult<f64> {
    value.to_number().ok_or_else(|| MixError::RuntimeError {
        span: None,
        msg: format!("cannot use '{}' as number", value.to_mix_string()),
    })
}

fn num_op(left: &Value, right: &Value, op: fn(f64, f64) -> f64) -> MixResult<Value> {
    let l = number_operand(left)?;
    let r = number_operand(right)?;
    Ok(Value::Number(op(l, r)))
}

fn dirs_home() -> Option<std::path::PathBuf> {
    std::env::var("HOME").ok().map(std::path::PathBuf::from)
}

/// Ordering comparison (`<` `>` `<=` `>=`).
///
/// **Numeric-first, with a lexicographic string fallback.** If both
/// operands coerce to numbers (numbers, or numeric strings like `"5"`),
/// compare numerically via `num_op` — this preserves every comparison
/// that already worked (incl. `"5" < "10"` → numeric → true). Only when
/// that fails AND both operands are strings do we compare them
/// lexicographically by Unicode codepoints via `str_op` (`str::cmp` is
/// byte-order, which equals codepoint order for valid UTF-8 and matches
/// Mix's byte-exact match ops). That case previously raised
/// "cannot compare … as number", so this is purely additive — it turns
/// a former error (e.g. slugify's `$ch >= "a"`) into the natural result.
/// A genuinely incomparable mix (non-numeric string vs number) still
/// errors.
fn num_cmp(
    left: &Value,
    right: &Value,
    num_op: fn(f64, f64) -> bool,
    str_op: fn(std::cmp::Ordering) -> bool,
) -> MixResult<Value> {
    if let (Some(l), Some(r)) = (left.to_number(), right.to_number()) {
        return Ok(Value::Bool(num_op(l, r)));
    }
    if let (Value::String(a), Value::String(b)) = (left, right) {
        return Ok(Value::Bool(str_op(a.cmp(b))));
    }
    // Mixed / non-numeric non-string — surface the original error,
    // naming whichever side failed to coerce.
    if left.to_number().is_none() {
        return Err(MixError::RuntimeError {
            span: None,
            msg: format!("cannot compare '{}' as number", left.to_mix_string()),
        });
    }
    Err(MixError::RuntimeError {
        span: None,
        msg: format!("cannot compare '{}' as number", right.to_mix_string()),
    })
}

#[cfg(test)]
mod sync_drift_tests {
    use super::*;

    // `eval_sync_verified` is the guard that turns a classifier/evaluator
    // DRIFT (the runtime declining an expression the static classifier
    // accepted) into a recoverable error instead of the old `.expect()`
    // panic — the "mix-loop trap" that could kill mix-as-login-shell.
    // A real drift can't be constructed (the two are kept aligned), so we
    // drive the helper directly with a shape `try_eval_expr_sync` is known
    // to return `None` for: a short-circuit `BinaryOp` `And` (the async
    // path owns short-circuit eval). The classifier would route this to
    // async, NOT to `exec_stmt_sync` — calling the helper directly
    // simulates the `None` branch a drift would hit. Must yield a
    // RuntimeError, never panic.
    #[test]
    fn eval_sync_verified_errors_instead_of_panicking_on_drift() {
        let mut eval = Evaluator::new();
        let drift = Expr::BinaryOp {
            left: Box::new(Expr::BoolLiteral(true)),
            op: BinOp::And,
            right: Box::new(Expr::BoolLiteral(true)),
        };
        match eval.eval_sync_verified(&drift) {
            Err(MixError::RuntimeError { msg, .. }) => {
                assert!(
                    msg.contains("sync classifier/evaluator drift"),
                    "expected drift message, got: {msg}"
                );
            }
            other => panic!("expected RuntimeError, got {other:?}"),
        }
    }

    // Sanity: a genuinely sync expression still evaluates normally through
    // the same helper.
    #[test]
    fn eval_sync_verified_passes_through_normal_exprs() {
        let mut eval = Evaluator::new();
        let ok = Expr::BinaryOp {
            left: Box::new(Expr::NumberLiteral(2.0)),
            op: BinOp::Add,
            right: Box::new(Expr::NumberLiteral(3.0)),
        };
        match eval.eval_sync_verified(&ok) {
            Ok(Value::Number(n)) => assert_eq!(n, 5.0),
            other => panic!("expected Number(5), got {other:?}"),
        }
    }

    // A ternary of sync sub-exprs is classified sync AND evaluates through
    // the sync path — the drift invariant (`is_expr_sync ⟹ try returns
    // Some`) holds for the new variant. Picks the `then` branch here.
    #[test]
    fn ternary_sync_classified_and_evaluates() {
        let tern = Expr::Ternary {
            cond: Box::new(Expr::BoolLiteral(true)),
            then_branch: Box::new(Expr::NumberLiteral(1.0)),
            else_branch: Box::new(Expr::NumberLiteral(2.0)),
        };
        assert!(
            Evaluator::is_expr_sync(&tern),
            "ternary of sync sub-exprs must classify sync"
        );
        let mut eval = Evaluator::new();
        match eval.eval_sync_verified(&tern) {
            Ok(Value::Number(n)) => assert_eq!(n, 1.0),
            other => panic!("expected Number(1), got {other:?}"),
        }
    }

    // Short-circuit: only the taken branch is evaluated. The non-taken
    // branch is an undefined variable — reaching it would error. With the
    // condition false, the `else` (a literal) wins and no error surfaces.
    #[test]
    fn ternary_short_circuits_untaken_branch() {
        let tern = Expr::Ternary {
            cond: Box::new(Expr::BoolLiteral(false)),
            then_branch: Box::new(Expr::Variable("never_evaluated".to_string())),
            else_branch: Box::new(Expr::StringLiteral("safe".to_string())),
        };
        let mut eval = Evaluator::new();
        match eval.eval_sync_verified(&tern) {
            Ok(Value::String(ref s)) => assert_eq!(s, "safe"),
            other => panic!("expected String(\"safe\"), got {other:?}"),
        }
    }

    // `Expr::If` is intentionally async-only: `is_expr_sync` must return
    // false so it never reaches `try_eval_expr_sync`/`exec_stmt_sync` (whose
    // statement-block branches the sync path can't drive). Keeping the two
    // aligned at `false`/`None` is what keeps the drift invariant intact.
    #[test]
    fn if_expr_classified_async_only() {
        let ifx = Expr::If(Box::new(IfExpr {
            condition: Expr::BoolLiteral(true),
            then_body: vec![Stmt::new(StmtKind::Expression(Expr::NumberLiteral(1.0)), 1)],
            else_ifs: Vec::new(),
            else_body: Some(vec![Stmt::new(
                StmtKind::Expression(Expr::NumberLiteral(2.0)),
                1,
            )]),
        }));
        assert!(
            !Evaluator::is_expr_sync(&ifx),
            "if-expression must be classified non-sync (async-only)"
        );
    }
}

#[cfg(test)]
mod chain_class_tests {
    use super::{ChainClass, HandlerEntry};
    use std::sync::Arc;

    fn entry(is_async: bool) -> HandlerEntry {
        HandlerEntry {
            body: Arc::new(Vec::new()),
            is_async,
        }
    }

    #[test]
    fn empty_chain_is_class_s() {
        assert_eq!(ChainClass::from_entries(&[]), ChainClass::ClassS);
    }

    #[test]
    fn all_sync_is_class_s() {
        let entries = [entry(false), entry(false), entry(false)];
        assert_eq!(ChainClass::from_entries(&entries), ChainClass::ClassS);
    }

    #[test]
    fn all_async_is_class_c() {
        let entries = [entry(true), entry(true)];
        assert_eq!(ChainClass::from_entries(&entries), ChainClass::ClassC);
    }

    #[test]
    fn mixed_chain_is_class_c() {
        let entries = [entry(false), entry(true), entry(false)];
        assert_eq!(ChainClass::from_entries(&entries), ChainClass::ClassC);
    }

    #[test]
    fn single_async_promotes_chain() {
        assert_eq!(ChainClass::from_entries(&[entry(true)]), ChainClass::ClassC);
    }
}

#[cfg(test)]
mod chain_rc_shared_scope_tests {
    use super::{Evaluator, SharedBuf};

    fn block_on<T>(fut: impl std::future::Future<Output = T>) -> T {
        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap()
            .block_on(fut)
    }

    /// The chain gate must read `$rc` through the SHARED global frame.
    /// `Scope::get` is standalone-only for globals, so inside a
    /// per-invocation activation (`for_invocation`, the --serve handler
    /// path) it returns None for a real shared `$rc` — and None means
    /// SUCCESS, so `&&` ran its right side off a live failure rc. Found
    /// by the 0.60.0 cold review; this is the regression pin.
    #[test]
    fn chain_gate_sees_shared_global_rc() {
        block_on(async {
            let stdout = SharedBuf::new();
            let stderr = SharedBuf::new();
            let mut parent =
                Evaluator::with_output(Box::new(stdout.clone()), Box::new(stderr.clone()));
            parent
                .execute_script_source("$rc = 1")
                .await
                .expect("set rc");
            let mut inv = parent.for_invocation();
            inv.execute_script_source("print(\"L\") && print(\"MUST_NOT\")")
                .await
                .expect("chain executes");
            inv.execute_script_source("print(\"L2\") || print(\"or-ran\")")
                .await
                .expect("chain executes");
            let out = stdout.to_string_lossy();
            assert_eq!(out, "L\nL2\nor-ran\n", "shared $rc=1 must gate as FAILURE");
        });
    }

    /// A corrupted shared `$rc` must RAISE inside an activation, not be
    /// read as absent-success.
    #[test]
    fn chain_gate_raises_on_corrupt_shared_rc() {
        block_on(async {
            let stdout = SharedBuf::new();
            let stderr = SharedBuf::new();
            let mut parent =
                Evaluator::with_output(Box::new(stdout.clone()), Box::new(stderr.clone()));
            parent
                .execute_script_source("$rc = \"bad\"")
                .await
                .expect("set rc");
            let mut inv = parent.for_invocation();
            let err = inv
                .execute_script_source("print(\"x\") && print(\"y\")")
                .await
                .expect_err("corrupt shared $rc must raise");
            let msg = format!("{err}");
            assert!(
                msg.contains("chain condition") && msg.contains("must be a number"),
                "got: {msg}"
            );
        });
    }
}

#[cfg(test)]
mod invocation_reply_tests {
    use super::{
        BusFuture, BusHandler, InvocationReplyHandle, InvocationReplyRegistry, MixError, MixResult,
        Value,
    };
    use std::cell::{Cell, RefCell};
    use std::rc::Rc;

    /// Recorded `reply()` call — preserved fully so future synth /
    /// shutdown-drain tests can assert rc+body shape, not just count.
    struct RecordedReply {
        to: String,
        command: String,
        id: Option<String>,
        rc: u8,
        body: String,
    }

    /// Test double for `BusHandler::reply` — records each call so the
    /// tests can assert exactly how many wire replies actually fired
    /// (which is the load-bearing property reply-once guards).
    struct RecordingReplyHandler {
        /// One entry per `reply()` invocation.
        calls: RefCell<Vec<RecordedReply>>,
        /// When `true`, `reply()` returns an error before recording the
        /// call. Lets the "failed send leaves handle un-answered" test
        /// exercise the same code path the real transport would take
        /// on a broken socket.
        fail: Cell<bool>,
    }

    impl RecordingReplyHandler {
        fn new() -> Rc<Self> {
            Rc::new(Self {
                calls: RefCell::new(Vec::new()),
                fail: Cell::new(false),
            })
        }
    }

    impl BusHandler for RecordingReplyHandler {
        fn send<'a>(
            &'a self,
            _target: &'a str,
            _command: &'a str,
            _args: &'a Value,
        ) -> BusFuture<'a, MixResult<(i32, Value)>> {
            Box::pin(async { Ok((0, Value::Nil)) })
        }

        fn emit<'a>(
            &'a self,
            _target: &'a str,
            _command: &'a str,
            _args: &'a Value,
        ) -> BusFuture<'a, MixResult<()>> {
            Box::pin(async { Ok(()) })
        }

        fn port_exists<'a>(&'a self, _target: &'a str) -> BusFuture<'a, MixResult<bool>> {
            Box::pin(async { Ok(false) })
        }

        fn next_incoming<'a>(&'a self) -> BusFuture<'a, Option<super::IncomingEvent>> {
            Box::pin(async { None })
        }

        fn reply<'a>(
            &'a self,
            to: &'a str,
            command: &'a str,
            id: Option<&'a str>,
            rc: u8,
            body: &'a str,
        ) -> BusFuture<'a, MixResult<()>> {
            let to = to.to_string();
            let command = command.to_string();
            let id = id.map(|s| s.to_string());
            let body = body.to_string();
            let fail = self.fail.get();
            let calls_cell = &self.calls;
            Box::pin(async move {
                if fail {
                    return Err(MixError::RuntimeError {
                        span: None,
                        msg: "test handler simulated send failure".to_string(),
                    });
                }
                calls_cell.borrow_mut().push(RecordedReply {
                    to,
                    command,
                    id,
                    rc,
                    body,
                });
                Ok(())
            })
        }
    }

    /// Register a request handle with the given Bus handler and
    /// intentionally leak the `InvocationRegistration` token so the
    /// handle stays in the registry for the test's duration. Named
    /// `leaked_*` to make the suppression explicit per Codex C.7c R1
    /// NIT — tests that exercise the deregistration lifecycle build
    /// their own registry inline and call `complete()` / observe
    /// Drop explicitly.
    fn leaked_registered_handle(handler: Option<Rc<dyn BusHandler>>) -> Rc<InvocationReplyHandle> {
        let registry = Rc::new(InvocationReplyRegistry::new());
        let (handle, registration) = registry.register(
            "caller".to_string(),
            "test.cmd".to_string(),
            Some("corr-1".to_string()),
            true,
            handler,
        );
        std::mem::forget(registration);
        handle
    }

    fn block_on<F: std::future::Future>(fut: F) -> F::Output {
        let rt = tokio::runtime::Builder::new_current_thread()
            .build()
            .expect("build tokio runtime");
        let local = tokio::task::LocalSet::new();
        local.block_on(&rt, fut)
    }

    #[test]
    fn reply_once_fires_once_then_no_ops() {
        let handler = RecordingReplyHandler::new();
        let handle = leaked_registered_handle(Some(handler.clone() as Rc<dyn BusHandler>));
        block_on(async {
            let first = handle.reply_once(0, "ok").await.expect("first reply ok");
            let second = handle
                .reply_once(1, "second")
                .await
                .expect("second reply ok");
            assert!(first, "first reply_once should report it fired");
            assert!(!second, "second reply_once should report no-op");
        });
        let calls = handler.calls.borrow();
        assert_eq!(calls.len(), 1, "exactly one wire reply should fire");
        assert_eq!(calls[0].to, "caller");
        assert_eq!(calls[0].command, "test.cmd");
        assert_eq!(calls[0].id.as_deref(), Some("corr-1"));
        assert_eq!(calls[0].rc, 0, "first call's rc preserved");
        assert_eq!(calls[0].body, "ok", "first call's body preserved");
        assert!(handle.is_answered());
        assert!(!handle.is_unanswered_request());
    }

    #[test]
    fn reply_once_errors_when_no_handler_registered() {
        let handle = leaked_registered_handle(None);
        block_on(async {
            let err = handle
                .reply_once(0, "x")
                .await
                .expect_err("must surface Bus-not-available");
            match err {
                MixError::RuntimeError { msg, .. } => {
                    assert!(
                        msg.contains("Bus not available"),
                        "expected Bus-not-available message, got {msg}"
                    );
                }
                other => panic!("expected RuntimeError, got {other:?}"),
            }
        });
        assert!(
            !handle.is_answered(),
            "no-handler error must leave handle un-answered"
        );
    }

    #[test]
    fn reply_once_failed_send_leaves_handle_unanswered() {
        let handler = RecordingReplyHandler::new();
        handler.fail.set(true);
        let handle = leaked_registered_handle(Some(handler.clone() as Rc<dyn BusHandler>));
        block_on(async {
            let err = handle.reply_once(0, "x").await.expect_err("send fails");
            assert!(matches!(err, MixError::RuntimeError { .. }));
        });
        assert!(
            !handle.is_answered(),
            "send failure must leave handle un-answered so the §3.4 / shutdown synth can still try"
        );
        handler.fail.set(false);
        block_on(async {
            let ok = handle.reply_once(0, "retry").await.expect("retry succeeds");
            assert!(ok);
        });
        assert!(handle.is_answered());
    }

    #[test]
    fn mark_answered_cas_returns_true_once() {
        let handle = leaked_registered_handle(None);
        assert!(handle.mark_answered());
        assert!(!handle.mark_answered());
        assert!(handle.is_answered());
    }

    #[test]
    fn is_unanswered_request_false_for_non_request() {
        let registry = Rc::new(InvocationReplyRegistry::new());
        let (handle, _guard) = registry.register(
            String::new(),
            "topic.broadcast".to_string(),
            None,
            false,
            None,
        );
        assert!(
            !handle.is_unanswered_request(),
            "a topic delivery is never an unanswered request"
        );
    }

    #[test]
    fn registry_assigns_monotonic_ids_and_snapshots_live_handles() {
        let registry = Rc::new(InvocationReplyRegistry::new());
        assert_eq!(registry.len(), 0);
        let (_h1, g1) = registry.register("a".to_string(), "cmd.one".to_string(), None, true, None);
        let (_h2, g2) = registry.register("b".to_string(), "cmd.two".to_string(), None, true, None);
        assert_eq!(registry.len(), 2);
        let snap = registry.snapshot();
        assert_eq!(snap.len(), 2);
        let mut commands: Vec<&str> = snap.iter().map(|h| h.command.as_str()).collect();
        commands.sort();
        assert_eq!(commands, vec!["cmd.one", "cmd.two"]);
        // Codex C.7c R1 BLOCKER fix: plain Drop is a no-op. The
        // registry retains the entries so C.7f's shutdown drain can
        // enumerate cancellation victims; only `complete()`
        // explicitly removes.
        drop(g1);
        assert_eq!(
            registry.len(),
            2,
            "plain Drop must not deregister — cancellation victims stay for C.7f drain"
        );
        g2.complete();
        assert_eq!(
            registry.len(),
            1,
            "complete() removes its entry (normal-completion path)"
        );
    }

    #[test]
    fn registry_holds_handle_alive_past_complete() {
        let registry = Rc::new(InvocationReplyRegistry::new());
        let (handle, registration) =
            registry.register("a".to_string(), "cmd".to_string(), None, true, None);
        // The Rc<InvocationReplyHandle> the caller stores in
        // `Evaluator::current_reply` must keep working even after
        // the registration is gone — covers the corner where a late
        // synth or test inspection reads state post-deregister.
        registration.complete();
        assert_eq!(registry.len(), 0);
        assert!(!handle.is_answered());
        assert!(handle.mark_answered());
        assert!(handle.is_answered());
    }

    #[test]
    fn plain_drop_of_registration_is_noop_for_shutdown_drain() {
        // Codex C.7c R1 BLOCKER assertion: a cancelled invocation
        // (modelled by dropping the registration without calling
        // `complete()`) leaves the handle enumerable by the
        // shutdown drain. C.7f will rely on this to synthesize a
        // §3.4 reply for the cancelled request.
        let registry = Rc::new(InvocationReplyRegistry::new());
        let (handle, registration) = registry.register(
            "stuck-caller".to_string(),
            "stuck.cmd".to_string(),
            Some("corr-x".to_string()),
            true,
            None,
        );
        drop(registration);
        assert_eq!(registry.len(), 1, "Drop must not remove the entry");
        let snap = registry.snapshot();
        assert_eq!(snap.len(), 1);
        assert!(Rc::ptr_eq(&snap[0], &handle));
        assert!(snap[0].is_unanswered_request());
    }

    #[test]
    fn reply_once_pre_await_state_blocks_concurrent_duplicate() {
        // Codex C.7c R1 MAJOR scenario: while one `reply_once` is
        // mid-`.await`, a second `reply_once` on the same handle
        // must see `Replying` and return `Ok(false)` without
        // entering the wire-send branch. Models the C.7f-during-
        // C.7d-Class-C edge where the shutdown drain and the
        // owning task race on the same handle.
        let handler = RecordingReplyHandler::new();
        let handle = leaked_registered_handle(Some(handler.clone() as Rc<dyn BusHandler>));
        // Pre-claim the slot by hand — equivalent to a peer task
        // having just claimed `Replying` and yielded inside
        // `handler.reply(...).await`.
        handle.state.set(super::ReplyState::Replying);
        block_on(async {
            let outcome = handle
                .reply_once(0, "racer")
                .await
                .expect("Replying must short-circuit, not error");
            assert!(
                !outcome,
                "second caller in Replying state must report no-op (Ok(false))"
            );
        });
        assert_eq!(
            handler.calls.borrow().len(),
            0,
            "no wire reply should have fired while another reply was in flight"
        );
    }

    #[test]
    fn reply_once_failed_send_rolls_back_to_unanswered() {
        // Codex C.7c R1 MAJOR rollback path. A failed wire send
        // must restore `Unanswered` so the §3.4 / shutdown synth
        // can retry — otherwise a transient transport hiccup
        // would strand the requester.
        let handler = RecordingReplyHandler::new();
        handler.fail.set(true);
        let handle = leaked_registered_handle(Some(handler.clone() as Rc<dyn BusHandler>));
        block_on(async {
            let _err = handle
                .reply_once(0, "x")
                .await
                .expect_err("simulated send failure");
        });
        assert_eq!(
            handle.state.get(),
            super::ReplyState::Unanswered,
            "failed wire send must roll back Replying → Unanswered"
        );
        assert!(handle.is_unanswered_request());
    }

    #[test]
    fn reply_once_rejects_non_request_handle() {
        // Codex C.7c R1 MINOR — defensive `is_request` check inside
        // `reply_once`. C.7f's drain enumerates registry snapshots
        // and must not wire-reply to a topic delivery that has no
        // caller to receive it.
        let registry = Rc::new(InvocationReplyRegistry::new());
        let (handle, registration) = registry.register(
            String::new(),
            "topic.delivery".to_string(),
            None,
            false,
            None,
        );
        std::mem::forget(registration);
        block_on(async {
            let err = handle
                .reply_once(0, "x")
                .await
                .expect_err("non-request handle must be rejected");
            match err {
                MixError::RuntimeError { msg, .. } => assert!(
                    msg.contains("non-request"),
                    "expected non-request rejection, got {msg}"
                ),
                other => panic!("expected RuntimeError, got {other:?}"),
            }
        });
        assert!(!handle.is_answered());
    }
}

#[cfg(test)]
mod event_args_tests {
    //! `$event.args` — the parsed-body view added alongside `$event.body`.
    //! A `body=` text payload reads back as nil (not JSON); a structured
    //! JSON body (the positional `_0`/`_1` or map send path) reads back
    //! pre-parsed so a handler skips the manual `json_parse($event.body)`.
    use super::*;
    use std::collections::BTreeMap;

    // Feature-independent: empty / non-JSON / JSON-null bodies are nil in
    // BOTH the `json` and no-`json` builds (the no-json path always yields
    // nil; the json path yields nil for empty, invalid, and literal null).
    #[test]
    fn parse_event_args_empty_body_is_nil() {
        assert_eq!(parse_event_args(""), Value::Nil);
    }

    #[test]
    fn parse_event_args_raw_text_is_nil() {
        // A raw `body="hi there"` text payload is not valid JSON → nil, so
        // the handler falls back to $event.body for the raw string.
        assert_eq!(parse_event_args("hi there"), Value::Nil);
    }

    #[cfg(feature = "json")]
    #[test]
    fn parse_event_args_json_null_is_nil() {
        assert_eq!(parse_event_args("null"), Value::Nil);
    }

    #[cfg(feature = "json")]
    #[test]
    fn parse_event_args_positional_object() {
        // The positional send path: `send svc cmd "a" "b"` → {"_0":"a","_1":"b"}.
        match parse_event_args(r#"{"_0":"a","_1":"b"}"#) {
            Value::Map(ref m) => {
                assert_eq!(m.get("_0"), Some(&Value::String("a".to_string())));
                assert_eq!(m.get("_1"), Some(&Value::String("b".to_string())));
            }
            other => panic!("expected Map, got {other:?}"),
        }
    }

    #[cfg(feature = "json")]
    #[test]
    fn parse_event_args_array_body() {
        match parse_event_args("[1,2,3]") {
            Value::List(ref l) => assert_eq!(l.len(), 3),
            other => panic!("expected List, got {other:?}"),
        }
    }

    #[cfg(feature = "json")]
    #[test]
    fn incoming_event_exposes_args_alongside_raw_body() {
        let ev = IncomingEvent {
            command: "probe.echo".to_string(),
            headers: BTreeMap::new(),
            body: r#"{"_0":"hi"}"#.to_string(),
        };
        match incoming_event_to_mix_value(&ev) {
            Value::Map(ref m) => {
                // body stays the raw string...
                assert_eq!(
                    m.get("body"),
                    Some(&Value::String(r#"{"_0":"hi"}"#.to_string()))
                );
                // ...args is the pre-parsed structured view.
                match m.get("args") {
                    Some(Value::Map(a)) => {
                        assert_eq!(a.get("_0"), Some(&Value::String("hi".to_string())))
                    }
                    other => panic!("expected args Map, got {other:?}"),
                }
                assert_eq!(
                    m.get("command"),
                    Some(&Value::String("probe.echo".to_string()))
                );
            }
            other => panic!("expected Map, got {other:?}"),
        }
    }

    #[cfg(feature = "json")]
    #[test]
    fn incoming_event_text_body_has_nil_args() {
        // A plain-text body: args is nil, body carries the raw text.
        let ev = IncomingEvent {
            command: "probe.echo".to_string(),
            headers: BTreeMap::new(),
            body: "raw text here".to_string(),
        };
        match incoming_event_to_mix_value(&ev) {
            Value::Map(ref m) => {
                assert_eq!(m.get("args"), Some(&Value::Nil));
                assert_eq!(
                    m.get("body"),
                    Some(&Value::String("raw text here".to_string()))
                );
            }
            other => panic!("expected Map, got {other:?}"),
        }
    }
}
