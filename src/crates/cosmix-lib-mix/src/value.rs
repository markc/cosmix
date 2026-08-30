use indexmap::IndexMap;
use std::cell::RefCell;
use std::fmt;
use std::rc::Rc;

use crate::scope::MixFunction;

// CoW invariant (card 4, 0.28.0): List/Map/Bytes payloads are Rc-backed
// copy-on-write. NO `Weak` to these Rcs may ever be created — the
// iterative Drop's sole-owner test (strong==1 && weak==0) relies on it
// to flatten deep chains; a Weak would silently push last-owner drops
// back onto the recursive glue (the SIGSEGV class the manual Drop
// exists to prevent).
#[derive(Debug)]
pub enum Value {
    String(String),
    Number(f64),
    Bool(bool),
    /// Value-semantic list. `Rc` payload = copy-on-write: `Clone` is an
    /// O(1) refcount bump; every mutation site goes through
    /// `Rc::make_mut`, which copies one level exactly when another
    /// binding still shares the allocation. Observable semantics are
    /// identical to the pre-0.28 deep-clone representation.
    List(Rc<Vec<Value>>),
    /// Value-semantic map. Same CoW discipline as `List`.
    Map(Rc<IndexMap<String, Value>>),
    /// A first-class function value. `Rc` because the evaluator is
    /// `!Send` and lambdas are frequently cloned into scope slots and
    /// argument vectors; sharing through Rc avoids deep-cloning body
    /// AST. Identity comparison is deliberately disabled (see
    /// `PartialEq`) — function equality at the Mix level is never
    /// useful and produces confusing behaviour.
    Function(Rc<MixFunction>),
    /// Raw byte buffer. The escape hatch for binary I/O that Mix's
    /// UTF-8 `String` cannot carry without corruption — HTTP response
    /// bodies, `read_file_bytes`, `base64_decode`. Distinct from
    /// `String` on purpose: no implicit coercion, callers ask for
    /// bytes when they need bytes. Rc payload = CoW, same as `List`;
    /// still value-semantic (unlike `Buffer`).
    Bytes(Rc<Vec<u8>>),
    /// Reference-semantic, mutable byte buffer — the deliberate escape
    /// hatch from Mix's universal value semantics, for large binary data
    /// (audio/video/`.mid`/`.wav` construction). `Rc<RefCell<Vec<u8>>>`:
    /// `Clone` is a shallow `Rc::clone`, so `$b = $a` shares ONE backing
    /// buffer and mutations are visible through every alias — unlike
    /// `Bytes` (and `List`/`Map`), which are value-semantic. Built and
    /// grown in place by the `buffer` / `buffer_push` builtins (append is
    /// O(1) amortized instead of the O(n²) that value-semantic `Bytes`
    /// append incurs); `freeze` snapshots it to value-semantic `Bytes`
    /// for the write_file / hash / base64 sinks. Holds no nested `Value`,
    /// so it sits outside the deep-clone / deep-drop worklist.
    Buffer(Rc<RefCell<Vec<u8>>>),
    Nil,
}

/// All payloads are scalar or Rc-backed, so `Clone` is O(1) per value —
/// containers SHARE their allocation (copy-on-write: mutation sites go
/// through `Rc::make_mut`, which copies one level exactly when the
/// allocation is shared). The old explicit-stack deep clone
/// (`clone_container`) is gone: there is no deep clone anymore, so
/// there is no recursion to protect against. Value SEMANTICS are
/// unchanged — sharing is never observable from Mix.
impl Clone for Value {
    fn clone(&self) -> Self {
        match self {
            Value::String(s) => Value::String(s.clone()),
            Value::Number(n) => Value::Number(*n),
            Value::Bool(b) => Value::Bool(*b),
            Value::List(l) => Value::List(Rc::clone(l)),
            Value::Map(m) => Value::Map(Rc::clone(m)),
            Value::Function(f) => Value::Function(Rc::clone(f)),
            Value::Bytes(b) => Value::Bytes(Rc::clone(b)),
            // Shallow — `Rc::clone` shares the backing buffer. For Buffer
            // this is REFERENCE semantics (mutations visible through every
            // alias); for List/Map/Bytes it is CoW value semantics.
            Value::Buffer(b) => Value::Buffer(Rc::clone(b)),
            Value::Nil => Value::Nil,
        }
    }
}

/// `Drop` stays manual under CoW: the drop glue for the LAST owner of a
/// deeply nested `List`/`Map` still recurses per nesting level (Rc drop
/// → Vec glue → child Rc drop → …), so last-owner containers are
/// flattened onto a heap worklist first. A SHARED `Rc` drops by
/// refcount decrement only — O(1), no recursion, regardless of
/// contents — so shared subtrees are left to the native glue untouched;
/// every node is flattened by exactly its last owner.
///
/// Sole ownership is `strong == 1 && weak == 0` (the crate-level
/// invariant at the top of this file forbids `Weak` to these Rcs; the
/// weak check makes a violation a visible flatten-refusal at the
/// `Rc::get_mut` take rather than a silent recursive drop — Codex D1/D3
/// condition).
impl Drop for Value {
    fn drop(&mut self) {
        #[inline]
        fn sole_owner<T>(rc: &Rc<T>) -> bool {
            Rc::strong_count(rc) == 1 && Rc::weak_count(rc) == 0
        }
        /// Child that could recurse when dropped from here: a
        /// last-owner, non-empty container. (Shared children and
        /// scalars drop O(1).)
        #[inline]
        fn is_deep_last_owner(v: &Value) -> bool {
            match v {
                Value::List(rc) => sole_owner(rc) && !rc.is_empty(),
                Value::Map(rc) => sole_owner(rc) && !rc.is_empty(),
                _ => false,
            }
        }
        /// Move the children out iff we are the sole owner; a shared
        /// container is left intact (its drop is a decrement).
        fn take_children(v: &mut Value, out: &mut Vec<Value>) {
            match v {
                Value::List(rc) => {
                    if let Some(items) = Rc::get_mut(rc) {
                        out.append(items);
                    }
                }
                Value::Map(rc) => {
                    if let Some(m) = Rc::get_mut(rc) {
                        out.extend(m.drain(..).map(|(_, v)| v));
                    }
                }
                _ => {}
            }
        }

        // Fast path: only a LAST-OWNER container whose direct children
        // include a last-owner non-empty container can recurse more
        // than one level. Everything else — scalars, Rc-backed leaves,
        // shared containers, containers of scalars/shared children —
        // returns to the native glue.
        match self {
            Value::List(rc) => {
                if !sole_owner(rc) || !rc.iter().any(is_deep_last_owner) {
                    return;
                }
            }
            Value::Map(rc) => {
                if !sole_owner(rc) || !rc.values().any(is_deep_last_owner) {
                    return;
                }
            }
            _ => return,
        }
        let mut work: Vec<Value> = Vec::new();
        take_children(self, &mut work);
        while let Some(mut v) = work.pop() {
            take_children(&mut v, &mut work);
            // `v` drops here with its children already moved out (or
            // shared), so its own Drop takes the fast path above.
        }
    }
}

impl Value {
    /// Construct a value-semantic list (CoW `Rc` payload). Use this —
    /// not `Value::List(Rc::new(..))` — at every construction site, so
    /// the payload representation stays swappable in one place.
    #[inline]
    pub fn list(items: Vec<Value>) -> Value {
        Value::List(Rc::new(items))
    }
    /// Construct a value-semantic map (CoW `Rc` payload).
    #[inline]
    pub fn map(m: IndexMap<String, Value>) -> Value {
        Value::Map(Rc::new(m))
    }
    /// Construct a value-semantic byte string (CoW `Rc` payload).
    #[inline]
    pub fn bytes(b: Vec<u8>) -> Value {
        Value::Bytes(Rc::new(b))
    }
}

impl Value {
    pub fn is_truthy(&self) -> bool {
        match self {
            Value::Nil => false,
            Value::Bool(b) => *b,
            Value::String(s) => !s.is_empty() && s != "0",
            Value::Number(n) => *n != 0.0,
            Value::List(l) => !l.is_empty(),
            Value::Map(m) => !m.is_empty(),
            Value::Function(_) => true,
            Value::Bytes(b) => !b.is_empty(),
            Value::Buffer(b) => !b.borrow().is_empty(),
        }
    }

    #[inline]
    pub fn to_number(&self) -> Option<f64> {
        match self {
            Value::Number(n) => Some(*n),
            // A numeric STRING is digits/sign/decimal/exponent only. The Rust
            // f64 parser also accepts "inf"/"infinity"/"nan" (any case), but
            // those words are not Mix numeric strings — `is_number("inf")`
            // must be false and `"inf" < 5` a compare error, not numeric inf.
            // An overflowing literal ("1e999") is likewise unrepresentable.
            // Number(inf/NaN) VALUES still pass through above: math keeps
            // propagating IEEE-754 non-finites; only STRING coercion is strict.
            Value::String(s) => s.trim().parse::<f64>().ok().filter(|f| f.is_finite()),
            Value::Bool(b) => Some(if *b { 1.0 } else { 0.0 }),
            _ => None,
        }
    }

    pub fn to_mix_string(&self) -> String {
        let mut s = String::new();
        self.write_mix(&mut s);
        s
    }

    /// Coerce this value to a map-key string, moving the `String`
    /// payload out instead of cloning it. `Value` implements `Drop`,
    /// so callers can't move the payload out in a pattern themselves
    /// (E0509); this is the canonical `idx → key` helper.
    pub fn into_map_key(mut self) -> String {
        match &mut self {
            Value::String(s) => std::mem::take(s),
            other => other.to_mix_string(),
        }
    }

    /// Append the Mix string form of this value to an existing buffer.
    /// Avoids the intermediate `String` allocations that `to_mix_string`
    /// would produce when the result is going to be `push_str`'d into
    /// a buffer anyway (string interpolation, list/map formatting,
    /// `print`'s join). The two methods produce identical output.
    pub fn write_mix(&self, out: &mut String) {
        use std::fmt::Write;
        match self {
            Value::String(s) => out.push_str(s),
            Value::Number(n) => {
                // Integer formatting only when the i64 cast round-trips
                // exactly — `as i64` saturates, so a merely-integral gate
                // (`n == n.floor()`) would print 1e19 as i64::MAX. Same
                // gate as `write_mix_data`.
                let as_int = *n as i64;
                if (as_int as f64) == *n {
                    let _ = write!(out, "{}", as_int);
                } else {
                    let _ = write!(out, "{}", n);
                }
            }
            Value::Bool(b) => out.push_str(if *b { "true" } else { "false" }),
            Value::Nil => out.push_str("nil"),
            Value::List(items) => {
                out.push('[');
                for (i, v) in items.iter().enumerate() {
                    if i > 0 {
                        out.push_str(", ");
                    }
                    v.write_mix(out);
                }
                out.push(']');
            }
            Value::Map(map) => {
                out.push('{');
                for (i, (k, v)) in map.iter().enumerate() {
                    if i > 0 {
                        out.push_str(", ");
                    }
                    out.push_str(k);
                    out.push_str(": ");
                    v.write_mix(out);
                }
                out.push('}');
            }
            Value::Function(func) => {
                let _ = write!(out, "<function/{}>", func.params.len());
            }
            Value::Bytes(b) => {
                // Friendly placeholder for `print`/string interpolation —
                // never dump raw bytes to stdout (would corrupt terminals
                // for the same high-bit-byte reason that originally
                // motivated this variant). Callers who want the raw
                // payload write_file($bytes) it or base64_encode it.
                let _ = write!(out, "<bytes:{}>", b.len());
            }
            Value::Buffer(b) => {
                // Same rationale as `Bytes` — never dump raw bytes to a
                // terminal. `freeze` + write_file / base64_encode reach the
                // payload.
                let _ = write!(out, "<buffer:{}>", b.borrow().len());
            }
        }
    }

    pub fn type_name(&self) -> &'static str {
        match self {
            Value::String(_) => "string",
            Value::Number(_) => "number",
            Value::Bool(_) => "bool",
            Value::List(_) => "list",
            Value::Map(_) => "map",
            Value::Function(_) => "function",
            Value::Bytes(_) => "bytes",
            Value::Buffer(_) => "buffer",
            Value::Nil => "nil",
        }
    }

    /// Serialize this value as a strict-data Mix source string that
    /// round-trips through `parse_data` for any data-shaped tree.
    ///
    /// Differs from `to_mix_string` (intended for `print`-style human
    /// output) in two ways: strings are emitted with double quotes and
    /// escape sequences (`\\`, `\"`, `\n`, `\t`, `\r`, `\e`, `\$`, plus
    /// a leading `\~` when the string is exactly `"~"` or starts with
    /// `"~/"` — see `write_data_string` for the round-trip rationale),
    /// and map keys are quoted unconditionally. Both are necessary so
    /// that values like `Value::String("hello world")`,
    /// `Value::String("true")`, `Value::String("")`, and map keys like
    /// `"with-dash"` survive the write → re-parse cycle.
    ///
    /// Fallible: returns `MixError::DataSerializeError` if the tree
    /// contains a value that has no strict-data representation —
    /// currently non-finite numbers (`f64::INFINITY`,
    /// `f64::NEG_INFINITY`, `f64::NAN`), `Value::Function`, and
    /// `Value::Bytes` (no `.*.mix` consumer carries bytes; a hex/base64
    /// literal can be added later if one appears).
    pub fn to_mix_data_string(&self) -> crate::error::MixResult<String> {
        let mut s = String::new();
        self.write_mix_data(&mut s)?;
        Ok(s)
    }

    /// Like [`to_mix_data_string`](Self::to_mix_data_string) but emits a
    /// multi-line, 2-space-indented layout: one map entry / list element
    /// per line. Same escaping and same strict-data grammar — the pretty
    /// form round-trips through `parse_data` identically, since the
    /// strict-data parser treats inter-token whitespace as insignificant.
    /// Empty maps/lists stay compact (`{}` / `[]`). Intended for
    /// human-readable generated config files (a `default.conf.mix`).
    pub fn to_mix_data_string_pretty(&self) -> crate::error::MixResult<String> {
        let mut s = String::new();
        self.write_mix_data_indented(&mut s, 0, true)?;
        Ok(s)
    }

    /// Append the strict-data form of this value to `out`. See
    /// `to_mix_data_string` for the contract.
    pub fn write_mix_data(&self, out: &mut String) -> crate::error::MixResult<()> {
        self.write_mix_data_indented(out, 0, false)
    }

    /// Shared strict-data writer. `pretty == false` reproduces the
    /// single-line compact form byte-for-byte (the round-trip-tested
    /// default); `pretty == true` breaks maps/lists across lines at
    /// `2 * indent` spaces. `indent` is the current nesting depth and is
    /// ignored in compact mode.
    fn write_mix_data_indented(
        &self,
        out: &mut String,
        indent: usize,
        pretty: bool,
    ) -> crate::error::MixResult<()> {
        use crate::error::MixError;
        use std::fmt::Write;
        match self {
            Value::String(s) => write_data_string(s, out),
            Value::Number(n) => {
                if !n.is_finite() {
                    return Err(MixError::DataSerializeError {
                        msg: format!("non-finite number {} has no strict-data representation", n),
                    });
                }
                // Use the integer formatting branch only when the
                // i64 cast is exact — otherwise large integral floats
                // like 1e20 saturate to i64::MAX and silently change
                // value. `as i64` saturates on out-of-range, so the
                // round-trip check `(n as i64) as f64 == n` is the
                // correct gate.
                let as_int = *n as i64;
                if (as_int as f64) == *n {
                    let _ = write!(out, "{}", as_int);
                } else {
                    let _ = write!(out, "{}", n);
                }
            }
            Value::Bool(b) => out.push_str(if *b { "true" } else { "false" }),
            Value::Nil => out.push_str("nil"),
            Value::List(items) => {
                if !pretty || items.is_empty() {
                    out.push('[');
                    for (i, v) in items.iter().enumerate() {
                        if i > 0 {
                            out.push_str(", ");
                        }
                        v.write_mix_data_indented(out, indent, pretty)?;
                    }
                    out.push(']');
                } else {
                    out.push_str("[\n");
                    let inner = indent + 1;
                    let n = items.len();
                    for (i, v) in items.iter().enumerate() {
                        push_indent(out, inner);
                        v.write_mix_data_indented(out, inner, pretty)?;
                        if i + 1 < n {
                            out.push(',');
                        }
                        out.push('\n');
                    }
                    push_indent(out, indent);
                    out.push(']');
                }
            }
            Value::Map(map) => {
                if !pretty || map.is_empty() {
                    out.push('{');
                    for (i, (k, v)) in map.iter().enumerate() {
                        if i > 0 {
                            out.push_str(", ");
                        }
                        write_data_string(k, out);
                        out.push_str(": ");
                        v.write_mix_data_indented(out, indent, pretty)?;
                    }
                    out.push('}');
                } else {
                    out.push_str("{\n");
                    let inner = indent + 1;
                    let n = map.len();
                    for (i, (k, v)) in map.iter().enumerate() {
                        push_indent(out, inner);
                        write_data_string(k, out);
                        out.push_str(": ");
                        v.write_mix_data_indented(out, inner, pretty)?;
                        if i + 1 < n {
                            out.push(',');
                        }
                        out.push('\n');
                    }
                    push_indent(out, indent);
                    out.push('}');
                }
            }
            Value::Function(func) => {
                return Err(MixError::DataSerializeError {
                    msg: format!(
                        "function value (arity {}) has no strict-data representation",
                        func.params.len()
                    ),
                });
            }
            Value::Bytes(b) => {
                return Err(MixError::DataSerializeError {
                    msg: format!(
                        "bytes value ({} bytes) has no strict-data representation",
                        b.len()
                    ),
                });
            }
            Value::Buffer(b) => {
                return Err(MixError::DataSerializeError {
                    msg: format!(
                        "buffer value ({} bytes) has no strict-data representation",
                        b.borrow().len()
                    ),
                });
            }
        }
        Ok(())
    }
}

/// Quote a string for strict-data Mix output. Mirrors the escapes
/// the double-quoted string lexer recognises (`\n` `\t` `\r` `\e`
/// `\"` `\\` `\$` `\~`); other characters pass through unchanged.
///
/// The lexer also accepts `\u{XXXX}` on INPUT, but the writer does NOT
/// emit it — a non-ASCII char round-trips as its raw UTF-8 bytes (read
/// back verbatim by the lexer), so no `\u` emission is needed. `\u{…}` is
/// an authoring convenience, not a serialization form.
///
/// The leading `\~` escape exists because the lexer expands a leading
/// `~` followed by `/` or the closing `"` to `$HOME` at runtime
/// (`lexer.rs:lex_double_string`). Without escaping, a strict-data
/// `Value::String("~")` or `Value::String("~/foo")` would round-trip
/// through `parse_data` as `${HOME}` / `${HOME}/foo` — silently losing
/// the original value. The narrow escape is only needed at position
/// zero AND only when the next char is `/` or end-of-string; mid-string
/// `~` and `~letter` at position zero stay literal under the lexer rule
/// so they need no escape.
/// Push `2 * level` spaces — the pretty-mode indentation unit.
fn push_indent(out: &mut String, level: usize) {
    for _ in 0..level {
        out.push_str("  ");
    }
}

fn write_data_string(s: &str, out: &mut String) {
    use std::fmt::Write;
    out.push('"');
    let needs_leading_tilde_escape = {
        let mut chars = s.chars();
        chars.next() == Some('~') && matches!(chars.next(), None | Some('/'))
    };
    if needs_leading_tilde_escape {
        out.push_str("\\~");
    }
    for ch in s
        .chars()
        .skip(if needs_leading_tilde_escape { 1 } else { 0 })
    {
        match ch {
            '\\' => out.push_str("\\\\"),
            '"' => out.push_str("\\\""),
            '\n' => out.push_str("\\n"),
            '\t' => out.push_str("\\t"),
            '\r' => out.push_str("\\r"),
            '\x1b' => out.push_str("\\e"),
            '$' => out.push_str("\\$"),
            // Remaining C0/C1 controls and NUL would otherwise pass
            // through verbatim — unreadable, terminal-hostile, and a
            // NUL truncation hazard for any C consumer of the file.
            // The lexer accepts `\u{XXXX}` in double-quoted strings,
            // so the round-trip through `parse_data` still holds.
            c if c.is_control() => {
                let _ = write!(out, "\\u{{{:X}}}", c as u32);
            }
            c => out.push(c),
        }
    }
    out.push('"');
}

impl fmt::Display for Value {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.to_mix_string())
    }
}

impl PartialEq for Value {
    fn eq(&self, other: &Self) -> bool {
        match (self, other) {
            (Value::String(a), Value::String(b)) => a == b,
            (Value::Number(a), Value::Number(b)) => a == b,
            (Value::Bool(a), Value::Bool(b)) => a == b,
            (Value::Bytes(a), Value::Bytes(b)) => a == b,
            // Content equality, like `Bytes`. Two distinct buffers with
            // equal bytes compare equal; a same-`Rc` self-compare is safe
            // (two shared immutable borrows coexist). `Buffer` vs `Bytes`
            // stays `false` — `freeze` first to compare across types.
            (Value::Buffer(a), Value::Buffer(b)) => *a.borrow() == *b.borrow(),
            (Value::Nil, Value::Nil) => true,
            // Cross-type comparison: coerce to string
            (Value::Number(n), Value::String(s)) | (Value::String(s), Value::Number(n)) => {
                if let Ok(sn) = s.parse::<f64>() {
                    *n == sn
                } else {
                    false
                }
            }
            _ => false,
        }
    }
}
