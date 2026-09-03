use std::cell::RefCell;
use std::collections::{HashMap, HashSet};
use std::hash::{BuildHasherDefault, Hasher};
use std::ops::Deref;
use std::rc::Rc;

use crate::ast::{FunctionBody, Param};
use crate::value::Value;

/// FxHash-style non-cryptographic hasher. Variable lookups in Mix
/// hammer HashMap probes with short string keys (typically 1–8 chars
/// like "n", "sum", "i"); SipHash (Rust's default) is collision-
/// resistant but pays a per-byte mixing cost that dominates at this
/// length. FxHasher does a constant-time multiply-and-rotate per byte
/// — cheap for short keys, still adequate distribution for variable-
/// name workloads (no adversarial input to defend against here).
///
/// Constants are the standard FxHash values used by rustc's data
/// structures crate; they are not cryptographically meaningful.
#[derive(Default, Clone)]
pub struct FxHasher {
    hash: u64,
}

impl Hasher for FxHasher {
    #[inline]
    fn write(&mut self, bytes: &[u8]) {
        let mut h = self.hash;
        for &b in bytes {
            h = (h.rotate_left(5) ^ (b as u64)).wrapping_mul(0x517c_c1b7_2722_0a95);
        }
        self.hash = h;
    }
    #[inline]
    fn finish(&self) -> u64 {
        self.hash
    }
}

pub type FxBuildHasher = BuildHasherDefault<FxHasher>;
pub type FxHashMap<K, V> = HashMap<K, V, FxBuildHasher>;

/// Minimum and maximum positional arity of a parameter list for strict
/// arity (D5) and lint (E1202). `max` = total params. `min` = one past
/// the index of the LAST parameter without a default — because Mix's
/// parser allows a required parameter to follow a defaulted one
/// (`function f($a = 1, $b)`), and every param up to the last required
/// must be supplied positionally. `min` is 0 only when every param has
/// a default. Shared by the evaluator and the analyzer so they never
/// disagree.
pub fn param_arity(params: &[Param]) -> (usize, usize) {
    let min = params
        .iter()
        .rposition(|p| p.default.is_none())
        .map_or(0, |i| i + 1);
    (min, params.len())
}

/// SPEC 18 Phase 2 WS3-C.7b — handle types for the γ shared global frame
/// and function registry. `Rc<RefCell<_>>` is the per-citizen storage
/// shared between the dispatcher's standalone Evaluator and every Class C
/// `for_invocation`-spawned Evaluator. Aliased so the type appears as a
/// short name at each declaration / signature site (clippy
/// `type_complexity` and Codex review legibility).
pub(crate) type SharedGlobals = Rc<RefCell<FxHashMap<String, Value>>>;
pub(crate) type SharedFunctions = Rc<RefCell<HashMap<String, Rc<MixFunction>>>>;

/// Bytecode opcodes for self-recursive numeric functions (fib-like).
/// Compiled once at function-definition time when the body shape and
/// content match — see `Evaluator::compile_fib_bytecode`. The bytecode
/// evaluator skips the per-call AST walks inside `run_sync_block_body_
/// f64` (shape match, BinaryOp recursion, FunctionCall dispatch through
/// `try_call_function_sync_block_f64`), turning fib's hot inner work
/// into a flat opcode loop with stack-allocated f64 args.
#[derive(Debug, Clone)]
pub enum FibOp {
    /// Push `args[idx]` onto the stack.
    PushArg(u8),
    PushConst(f64),
    Add,
    Sub,
    Mul,
    Div,
    Mod,
    Pow,
    /// Pop r, l. If `!(l < r)`, jump to target. Else fall through.
    LtJZ(u16),
    GtJZ(u16),
    LeJZ(u16),
    GeJZ(u16),
    EqJZ(u16),
    NeJZ(u16),
    Jump(u16),
    /// Pop `arity` args, recursively evaluate this same program with
    /// them, push the result.
    CallSelf(u8),
    /// Return the top of the stack as the function's result.
    Ret,
}

#[derive(Debug, Clone)]
pub struct MixFunction {
    /// `Rc<str>`: cloned into the traceback stack on EVERY call — a
    /// refcount bump, not a heap copy (0.29.0).
    pub name: std::rc::Rc<str>,
    pub params: Vec<Param>,
    pub body: FunctionBody,
    /// Source file this function was DEFINED in (`None` in the REPL /
    /// `-c` / pure-library use). Captured from `ctx.current_file` at
    /// definition time; `call_function` switches `ctx.current_file` to
    /// this for the callee's execution window so error spans and
    /// traceback frames carry the right file across `require`d modules
    /// and `include`/`source` boundaries (0.29.0). `Rc<str>` because it
    /// is cloned on every call.
    pub def_file: Option<std::rc::Rc<str>>,
    /// Captured frame snapshot for inner-frame anonymous functions.
    /// `None` for named functions and top-level lambdas (they see
    /// globals live via the existing frame-isolation model). `Some`
    /// only when a `function(...)` expression is evaluated at
    /// function_depth > 0 — the snapshot is injected into the
    /// callee's frame before parameter binding, so params still win
    /// on name collision.
    pub captures: Option<HashMap<String, Value>>,
    /// Cached count of leading body statements that pass `is_stmt_sync`.
    /// Computed once at function-definition time so the per-call hot
    /// path can skip those statements' Box::pin'd `execute()` framing
    /// without re-walking each stmt's AST. 0 for `FunctionBody::Expression`
    /// (no Block to short-circuit) and for Blocks whose first stmt is
    /// non-sync. Recursion-heavy fib hits this every call (the leading
    /// `if $n<2 then return $n end` is always sync).
    pub sync_prefix_len: usize,
    /// Cached: the entire body is sync if we treat this function's
    /// own self-recursive calls as sync. When true, callers can take
    /// `try_call_function_sync_block` and avoid every Box::pin alloc
    /// on the recursive path — fib's full call chain runs on the
    /// real stack. Computed once via `is_stmt_sync_self` at definition.
    pub block_sync_self: bool,
    /// Cached: the body never assigns to a local variable. Combined
    /// with `block_sync_self`, this lets the sync-block fast path
    /// pre-bind raw pointers to each param's slot in the freshly-
    /// pushed function frame and reuse them for every Variable read
    /// inside the body — fib's `$n` reads skip the per-access
    /// HashMap probe. False whenever the body contains any
    /// `Assignment` stmt (which could insert new keys and rehash
    /// the bucket array, invalidating cached pointers).
    pub body_no_local_assigns: bool,
    /// Compiled fib-style bytecode for self-recursive numeric functions.
    /// `Some` when the body fits the IfReturnReturn shape, the cond is
    /// a numeric comparison of arith expressions, both branches are
    /// arith expressions over params/literals/`+-*/` and self-calls
    /// only, and `block_sync_self && body_no_local_assigns &&
    /// captures.is_none()`. The recursive-call hot path
    /// (`try_eval_expr_num`'s `FunctionCall` arm against
    /// `sync_self_func`) dispatches into `eval_fib_program` instead of
    /// going through `try_call_function_sync_block_f64` +
    /// `run_sync_block_body_f64`, eliminating the per-call var_slot_
    /// cache push/truncate, sync_self_func swap/restore, and shape
    /// match. fib's ~141k bench calls drop those costs each.
    pub fib_bytecode: Option<Vec<FibOp>>,
    /// `require()` module environment. `Some` only on functions
    /// harvested (or rewrapped) by `Evaluator::exec_require`: an
    /// Rc-shared, write-once map of the module's top-level names —
    /// every module function (as `Value::Function`, themselves
    /// env-carrying) plus every top-level `$var` snapshotted by value
    /// at require time. Injected into the callee frame by
    /// `call_function` BEFORE `captures` (which is injected before
    /// params), so captures shadow env and params shadow both.
    /// `OnceCell` because construction is necessarily cyclic (the
    /// wrapped functions hold the `Rc` before the map is filled with
    /// those same functions); after `set()` it is immutable. The
    /// resulting Rc cycle (env → Function → env) is an intentional
    /// program-lifetime retention, same lifetime as the module cache.
    pub module_env: Option<Rc<std::cell::OnceCell<HashMap<String, Value>>>>,
}

impl MixFunction {
    /// `true` when calling this function requires injecting values into
    /// the freshly-pushed callee frame (`captures` for inner-frame
    /// lambdas, `module_env` for `require()`-harvested module
    /// functions). Every sync fast path that skips `call_function`'s
    /// frame-injection loop MUST gate on this — a missed gate runs a
    /// module function without its environment (silent nil reads).
    #[inline]
    pub fn needs_frame_injection(&self) -> bool {
        self.captures.is_some() || self.module_env.is_some()
    }
}

/// SPEC 18 Phase 2 WS3-C.7b — Return shape for variable lookups that
/// must work in both **standalone** and **shared-globals** modes (the
/// γ design from the pre-impl Codex consult — `Rc<RefCell<HashMap>>`
/// shared global frame inside `Scope`).
///
/// In standalone mode the global frame is owned, so every lookup hands
/// back a `Borrowed(&Value)` and pays nothing extra. In shared mode the
/// global frame lives behind `Rc<RefCell<_>>`; a lookup that lands on
/// the global slot must clone the slot out (the `RefCell` borrow
/// cannot outlive the lookup call by Rust's borrow rules, **and** the
/// γ rule forbids holding any `RefCell` borrow across `.await`). Local
/// frames stay owned in both modes, so a local hit still returns
/// `Borrowed`.
///
/// `Deref<Target = Value>` lets callers stay agnostic when they only
/// need read access (`if let Some(v) = scope.get_value("k") { use(&*v) }`).
/// `Owned` arms remain alive for the variable binding's scope (Rust's
/// auto-ref/Drop pairs cleanly with `match` and `if let`), so the
/// borrow-window concern only applies INSIDE this enum's construction
/// site, not at the callsite.
pub enum ScopeValue<'a> {
    /// The value lives in an owned local frame; zero-copy.
    Borrowed(&'a Value),
    /// The value was cloned out of the shared global frame; the caller
    /// holds the only copy until the binding goes out of scope. The
    /// γ-mandated clone is acceptable because shared-global access is
    /// the per-invocation slow path; the hot-path standalone case
    /// never reaches this arm.
    Owned(Value),
}

impl Deref for ScopeValue<'_> {
    type Target = Value;
    #[inline]
    fn deref(&self) -> &Value {
        match self {
            ScopeValue::Borrowed(v) => v,
            ScopeValue::Owned(v) => v,
        }
    }
}

impl ScopeValue<'_> {
    /// Consume the wrapper, returning an owned `Value`. The `Owned`
    /// arm moves out (no extra clone — the shared-globals lookup
    /// already paid one clone to escape the `RefCell` borrow); the
    /// `Borrowed` arm clones from the local frame. The intended
    /// callsite is the evaluator's variable-read arms, which
    /// uniformly need an owned `Value` for downstream binop / index /
    /// store operations.
    #[inline]
    pub fn into_value(self) -> Value {
        match self {
            ScopeValue::Borrowed(v) => v.clone(),
            ScopeValue::Owned(v) => v,
        }
    }
}

pub struct Scope {
    frames: Vec<FxHashMap<String, Value>>,
    /// User-defined functions stored behind `Rc` so the named-function
    /// call path (`scope.get_function(name).cloned()`) bumps a refcount
    /// instead of deep-cloning the body AST. Recursion-heavy programs
    /// (fib, ackermann) called this many thousands of times per run.
    functions: HashMap<String, Rc<MixFunction>>,
    /// Stack of frame indices marking each active function's first
    /// visible frame. Empty at the top level. Lookups search
    /// `frames[floor..]` (where `floor` is the topmost entry), then
    /// fall back to globals (frame 0). This replaces the old
    /// stash/restore pair — function isolation is now O(1) push/pop
    /// of a usize instead of draining a `Vec<HashMap>` per call.
    function_floors: Vec<usize>,
    /// Pool of dropped frames retained for reuse. Each call to
    /// `enter_function_frame` would otherwise allocate a fresh
    /// `HashMap`'s bucket array on first insert; recursion-heavy
    /// programs (fib makes ~50K calls per run) paid that allocation
    /// repeatedly. Pooling keeps the bucket arrays warm — a cleared
    /// HashMap retains its capacity, so reuse skips the alloc on
    /// subsequent param binding. Pool entries are capped at a small
    /// capacity so a runaway frame can't bloat the pool indefinitely.
    frame_pool: Vec<FxHashMap<String, Value>>,
    /// SPEC 18 Phase 2 WS3-C.7b — γ shared global frame.
    ///
    /// **Standalone mode** (`None`): `frames[0]` is the global frame.
    /// Today's hot paths apply unchanged; no `RefCell` borrow on any
    /// lookup.
    ///
    /// **Shared mode** (`Some(handle)`): `frames[0]` is an empty
    /// **local root** frame (it still indexes `frames[0]` so `floor`
    /// arithmetic stays unchanged); globals live behind `handle`,
    /// Rc-cloned across every per-invocation `Scope` that points at
    /// the same citizen-wide global frame. The discipline is
    /// **never hold a `RefCell` borrow across `.await`** — every
    /// global lookup must complete its borrow inside the calling Mix
    /// statement, then release before the next yield point. The audit
    /// gate `Evaluator::can_use_shared_scope_raw_slots()` returns
    /// `false` whenever this is `Some`, disabling the raw-pointer
    /// fast paths that would otherwise hand out a `*mut Value` into a
    /// `RefCell`-protected map (UB on concurrent insert by another
    /// invocation).
    ///
    /// Promoted lazily by `shared_globals_handle()` on first call —
    /// standalone evaluators pay nothing until they spawn a Class C
    /// invocation. The lazy promotion hoists `frames[0]`'s contents
    /// into a fresh `Rc<RefCell<_>>` and replaces `frames[0]` with
    /// an empty map; this MUST happen outside any raw-slot live
    /// window (i.e. only from dispatch setup before a handler body
    /// runs, never from inside expression evaluation), and Codex
    /// round-2 of this slice confirmed the existing fast-path AST
    /// classifiers already enforce that window structurally.
    shared_globals: Option<SharedGlobals>,
    /// SPEC 18 Phase 2 WS3-C.7b — γ shared function registry.
    ///
    /// Parallel to `shared_globals`. Class C spawned invocations need
    /// to see user-defined functions registered by the script's init
    /// body (and by previously-completed handlers); without sharing,
    /// the second hazard from Codex Q6 ("handlers can see globals but
    /// not user functions") would surface in C.7d. Lazy promotion
    /// same shape as `shared_globals`; standalone evaluators are
    /// untouched.
    shared_functions: Option<SharedFunctions>,
}

impl Default for Scope {
    fn default() -> Self {
        Self::new()
    }
}

impl Scope {
    pub fn new() -> Self {
        Scope {
            frames: vec![FxHashMap::default()],
            functions: HashMap::new(),
            function_floors: Vec::new(),
            frame_pool: Vec::new(),
            shared_globals: None,
            shared_functions: None,
        }
    }

    /// SPEC 18 Phase 2 WS3-C.7b — construct a per-invocation Scope
    /// (γ shared mode).
    ///
    /// The new Scope's `frames[0]` is an empty local-root frame; both
    /// global reads/writes and function lookups route through the
    /// shared handles. Used by `Evaluator::for_invocation` (C.7d
    /// wires the actual spawn).
    #[allow(dead_code)] // wired by C.7d's Class C spawn path
    pub(crate) fn with_shared(globals: SharedGlobals, functions: SharedFunctions) -> Self {
        Scope {
            frames: vec![FxHashMap::default()],
            functions: HashMap::new(),
            function_floors: Vec::new(),
            frame_pool: Vec::new(),
            shared_globals: Some(globals),
            shared_functions: Some(functions),
        }
    }

    /// SPEC 18 Phase 2 WS3-C.7b — `true` iff this Scope's global
    /// frame is shared via `Rc<RefCell<_>>` (γ shared mode). The
    /// `Evaluator::can_use_shared_scope_raw_slots()` audit gate
    /// keys on this to disable the raw-`*mut Value` cache paths
    /// for per-invocation evaluators.
    #[inline]
    pub fn uses_shared_globals(&self) -> bool {
        self.shared_globals.is_some()
    }

    /// SPEC 18 Phase 2 WS3-C.7b — `true` iff this Scope's function
    /// registry is shared via `Rc<RefCell<_>>`.
    #[inline]
    pub fn uses_shared_functions(&self) -> bool {
        self.shared_functions.is_some()
    }

    /// SPEC 18 Phase 2 WS3-C.7b — return the shared global frame
    /// handle, **promoting standalone state lazily** on first call.
    ///
    /// Promotion replaces the owned `frames[0]` with a fresh
    /// `Rc<RefCell<_>>` holding the same content; subsequent global
    /// reads on this Scope then route through the shared handle. The
    /// `Evaluator::for_invocation` factory then clones the returned
    /// `Rc` into the per-invocation `Scope::with_shared` constructor,
    /// so the parent and the spawned child see the same global state.
    ///
    /// **Safety / live-window note.** Promotion mutates `frames[0]`
    /// (via `std::mem::take`), invalidating any in-flight `*mut Value`
    /// cached pointer into the old frame. Promotion MUST happen only
    /// outside a raw-slot live window — i.e. from dispatch setup
    /// *before* a handler body runs, not from inside expression /
    /// loop evaluation. Codex round-2 of this slice confirmed the
    /// existing fast-path AST classifiers (`is_expr_sync`, the
    /// single-statement For-loop fast-path shape) already make the
    /// live window structurally non-yielding; spawn happens at a
    /// dispatch boundary outside that window, so a hoist at spawn
    /// time cannot strand a live pointer.
    #[allow(dead_code)] // wired by C.7d's Class C spawn path
    pub(crate) fn shared_globals_handle(&mut self) -> SharedGlobals {
        if let Some(h) = &self.shared_globals {
            return Rc::clone(h);
        }
        // The factory path (Evaluator::for_invocation) only calls this
        // on the parent at a dispatch boundary, before any handler
        // body runs — no transient frames, no in-flight function
        // call. Asserting both invariants protects future callers
        // that might try to promote mid-evaluation (which would
        // invalidate any live `frames[0]` raw-pointer cache and
        // leave function-local frames stranded above an empty
        // global scaffold).
        debug_assert!(
            self.function_floors.is_empty(),
            "shared_globals_handle: must not promote with active function frames"
        );
        debug_assert!(
            self.frames.len() == 1,
            "shared_globals_handle: must not promote with transient frames present"
        );
        let frame = std::mem::take(&mut self.frames[0]);
        let handle = Rc::new(RefCell::new(frame));
        self.shared_globals = Some(Rc::clone(&handle));
        handle
    }

    /// SPEC 18 Phase 2 WS3-C.7b — return the shared function registry
    /// handle, **promoting standalone state lazily** on first call.
    /// Same shape as `shared_globals_handle`; the local `functions`
    /// map is moved into a fresh `Rc<RefCell<_>>` and replaced with
    /// an empty map.
    #[allow(dead_code)] // wired by C.7d's Class C spawn path
    pub(crate) fn shared_functions_handle(&mut self) -> SharedFunctions {
        if let Some(h) = &self.shared_functions {
            return Rc::clone(h);
        }
        let functions = std::mem::take(&mut self.functions);
        let handle = Rc::new(RefCell::new(functions));
        self.shared_functions = Some(Rc::clone(&handle));
        handle
    }

    /// Index of the lowest frame visible to current code: the topmost
    /// function's own frame, or 0 (globals) at the top level.
    #[inline]
    fn floor(&self) -> usize {
        *self.function_floors.last().unwrap_or(&0)
    }

    /// Borrowing variable lookup. **Standalone-mode only** for globals:
    /// in shared mode (`uses_shared_globals()`) a lookup that would
    /// have fallen through to the global frame returns `None` here, by
    /// design — the shared `Rc<RefCell<_>>` cannot hand out a
    /// `&Value` that outlives a `borrow()`. Per-invocation callers
    /// must use [`Self::get_value`] which returns a [`ScopeValue`]
    /// that clones out of the shared frame.
    ///
    /// Local-frame lookups work identically in both modes (locals
    /// stay owned), so this method remains the hot-path API for
    /// standalone / sync-fast-path callers that have proved they are
    /// not running inside a per-invocation evaluator.
    #[inline]
    pub fn get(&self, name: &str) -> Option<&Value> {
        // Top-level fast path: a single global frame and no function
        // floors active. loop_arith does ~4M `scope.get` calls per
        // iter through this branch — skipping the `floor()` call and
        // the iter+rev machinery saves measurable time. Gated on
        // standalone mode (in shared mode `frames[0]` is an empty
        // local-root frame, not the global frame).
        if self.shared_globals.is_none()
            && self.function_floors.is_empty()
            && self.frames.len() == 1
        {
            return self.frames[0].get(name);
        }
        let floor = self.floor();
        for frame in self.frames[floor..].iter().rev() {
            if let Some(val) = frame.get(name) {
                return Some(val);
            }
        }
        // Standalone-mode globals fallback when inside a function.
        // Shared-mode global fallback is `Self::get_value`'s job (it
        // can clone-out of the `RefCell`).
        if self.shared_globals.is_none()
            && floor > 0
            && let Some(val) = self.frames[0].get(name)
        {
            return Some(val);
        }
        None
    }

    /// SPEC 18 Phase 2 WS3-C.7b — γ-aware variable lookup. Works in
    /// both standalone and shared-globals modes. Returns
    /// [`ScopeValue::Borrowed`] for local frame hits (zero-copy in
    /// both modes) and for standalone global hits;
    /// [`ScopeValue::Owned`] for shared global hits (clone-out of the
    /// `RefCell`). `ScopeValue` derefs to `&Value` so read-only
    /// callers stay agnostic.
    pub fn get_value(&self, name: &str) -> Option<ScopeValue<'_>> {
        let floor = self.floor();
        // Visible local frames first (top-down).
        for frame in self.frames[floor..].iter().rev() {
            if let Some(val) = frame.get(name) {
                return Some(ScopeValue::Borrowed(val));
            }
        }
        // Globals fallback.
        if let Some(shared) = self.shared_globals.as_ref() {
            return shared.borrow().get(name).cloned().map(ScopeValue::Owned);
        }
        // Standalone with an active function: try the owned global
        // frame at `frames[0]`. (When `floor == 0` standalone, the
        // loop above already covered `frames[0]` and would have
        // returned.)
        if floor > 0
            && let Some(val) = self.frames[0].get(name)
        {
            return Some(ScopeValue::Borrowed(val));
        }
        None
    }

    /// Mutable counterpart to `get`. Used by mutating builtins
    /// (push/pop/shift) to edit lists in place instead of
    /// clone → mutate → reinsert (O(n) per call on a list).
    ///
    /// **Standalone-mode only** for globals: in shared mode this
    /// method walks visible local frames only and returns `None` for
    /// global misses. Shared-mode in-place mutation of globals must
    /// route through [`Self::with_mut_value`], which holds the
    /// `RefCell` borrow only for the duration of the synchronous
    /// mutation closure (the γ "no borrow across `.await`" rule).
    pub fn get_mut(&mut self, name: &str) -> Option<&mut Value> {
        let floor = self.floor();
        // Search visible (function-local) frames top-down first.
        for i in (floor..self.frames.len()).rev() {
            if self.frames[i].contains_key(name) {
                return self.frames[i].get_mut(name);
            }
        }
        // Standalone-mode globals fallback. In shared mode the global
        // frame lives behind a `RefCell` and cannot hand out a
        // `&mut Value` that outlives a `borrow_mut()`; caller uses
        // `with_mut_value` instead.
        if self.shared_globals.is_none() && floor > 0 && self.frames[0].contains_key(name) {
            return self.frames[0].get_mut(name);
        }
        None
    }

    /// SPEC 18 Phase 2 WS3-C.7b — γ-aware in-place mutation. Holds
    /// the `RefCell` borrow on the shared global frame only for the
    /// closure's call window (a single synchronous mutation), then
    /// releases. Caller MUST NOT `.await` inside `f`.
    ///
    /// Search order matches `get_mut` / `get_value`: visible local
    /// frames top-down, then global fallback (owned `frames[0]` in
    /// standalone mode, the shared `RefCell`-protected frame in
    /// shared mode). Returns `None` (and does not call `f`) if no
    /// matching name exists in any visible frame.
    #[allow(dead_code)] // wired by C.7d's Class C spawn path
    pub fn with_mut_value<R>(&mut self, name: &str, f: impl FnOnce(&mut Value) -> R) -> Option<R> {
        let floor = self.floor();
        for i in (floor..self.frames.len()).rev() {
            if self.frames[i].contains_key(name) {
                return self.frames[i].get_mut(name).map(f);
            }
        }
        if let Some(shared) = self.shared_globals.as_ref() {
            let mut globals = shared.borrow_mut();
            return globals.get_mut(name).map(f);
        }
        if floor > 0 && self.frames[0].contains_key(name) {
            return self.frames[0].get_mut(name).map(f);
        }
        None
    }

    /// Read-only counterpart of [`Self::with_mut_value`], with the same
    /// lookup order.
    ///
    /// Exists because neither read accessor fits a validate-then-mutate
    /// pass over a large container: `get` returns `None` for shared
    /// globals by design, and `get_value` has to CLONE out of the
    /// `RefCell` to hand back an owned value — which for the nested-write
    /// precheck would copy the very map the write is trying not to copy.
    /// This borrows in place for the closure's window instead. Caller
    /// MUST NOT `.await` inside `f`.
    pub fn with_value<R>(&self, name: &str, f: impl FnOnce(&Value) -> R) -> Option<R> {
        let floor = self.floor();
        for i in (floor..self.frames.len()).rev() {
            if let Some(v) = self.frames[i].get(name) {
                return Some(f(v));
            }
        }
        if let Some(shared) = self.shared_globals.as_ref() {
            let globals = shared.borrow();
            return globals.get(name).map(f);
        }
        if floor > 0
            && let Some(v) = self.frames[0].get(name)
        {
            return Some(f(v));
        }
        None
    }

    /// Mutate a value ONLY when it exists in the current (top) frame —
    /// never walking outward to a caller/global binding. The assignment
    /// self-concat fast path inside a function must use this (not
    /// `with_mut_value`): ordinary function assignment writes the current
    /// frame via `set_in_current`, even when a same-named read resolves to
    /// an outer frame, so mutating an outer binding in place would change
    /// semantics. `None` (f not called) if the current frame has no such name.
    pub fn with_mut_current_value<R>(
        &mut self,
        name: &str,
        f: impl FnOnce(&mut Value) -> R,
    ) -> Option<R> {
        self.frames.last_mut()?.get_mut(name).map(f)
    }

    pub fn set(&mut self, name: String, value: Value) {
        // Set in the current (top) frame
        if let Some(frame) = self.frames.last_mut() {
            frame.insert(name, value);
        }
    }

    /// True iff we're operating against a single global frame (no
    /// active function call). The caller can then take fast paths
    /// that assume `frames[0]` is the only frame.
    ///
    /// **Returns `false` in shared-globals mode** even when
    /// `function_floors.is_empty() && frames.len() == 1`: the
    /// per-invocation `Scope`'s `frames[0]` is an empty local-root
    /// frame, not the global frame, so the "fast paths that assume
    /// frames[0] is the only frame" assumption no longer holds. The
    /// audit gate `Evaluator::can_use_shared_scope_raw_slots()` also
    /// flips for per-invocation evaluators; the two predicates
    /// together protect every fast path that previously relied on
    /// "globals live in `frames[0]`."
    #[inline]
    pub fn is_top_level(&self) -> bool {
        self.shared_globals.is_none() && self.function_floors.is_empty() && self.frames.len() == 1
    }

    /// Ensure `name` exists in the global frame, then return a raw
    /// mutable pointer to its slot. Caller takes responsibility for
    /// preventing further insertions on the global frame while the
    /// pointer is in use — only update existing slots (which never
    /// rehash the bucket array). Used by the For-loop top-level fast
    /// path to cache slot addresses outside the 1M-iter hot loop.
    ///
    /// Safety contract on the *returned* pointer:
    /// 1. Valid only as long as no other call inserts into / removes
    ///    from / reallocates `frames[0]`. The For-loop fast path holds
    ///    the pointer across `try_eval_expr_sync` calls, which only do
    ///    read lookups (`scope.get`) — no inserts. If RHS eval falls
    ///    back to async `eval_expr` (which could insert anything), the
    ///    caller must NOT use the cached pointer for that iteration.
    /// 2. The caller must prove the pointer's live window is statically
    ///    non-yielding, or otherwise excludes concurrent insertion by
    ///    another invocation. `Scope` cannot know scheduler facts or
    ///    AST sync facts — this proof is the caller's responsibility.
    /// 3. `try_global_slot_ptr` (below) is the must-not-insert sibling;
    ///    use it whenever absence must surface as the canonical
    ///    undefined-variable error rather than be silently created as
    ///    a fresh `Nil`.
    ///
    /// SPEC 18 Phase 2 WS3-C.4b: the For-loop top-level fast paths
    /// route the **environmental permission** to use shared-`frames[0]`
    /// raw slots through `Evaluator::can_use_shared_scope_raw_slots()`
    /// as a single named audit gate. Obligation (2) — proving the live
    /// window is statically non-yielding / excludes concurrent insertion
    /// — is still discharged at each call site by the surrounding AST
    /// classifiers (`Self::is_expr_sync`, single-statement fast-path
    /// shape) and is not what the predicate encodes. New callers in the
    /// evaluator MUST thread through the predicate AND discharge (2)
    /// locally (or document why their live window is structurally safe
    /// without it). Callers outside the evaluator — there are none
    /// today — would also need their own equivalent.
    pub fn global_slot_ptr(&mut self, name: &str) -> *mut Value {
        // SPEC 18 Phase 2 WS3-C.7b — γ defensive guard. The
        // `Evaluator::can_use_shared_scope_raw_slots()` audit gate
        // already returns `false` when `uses_shared_globals()`, so
        // no caller should reach this path on a shared scope. Panic
        // loudly if one ever does, rather than handing out a pointer
        // into a transiently-borrowed `RefCell` (UB) or into the
        // now-empty local-root `frames[0]` (silent wrong-frame
        // mutation that would leak per-invocation writes into the
        // local root). Belt-and-braces; the gate is the contract.
        assert!(
            self.shared_globals.is_none(),
            "global_slot_ptr called on a shared-globals Scope; \
             callers must check Evaluator::can_use_shared_scope_raw_slots() first \
             (or use Self::with_mut_value for in-place mutation under γ)"
        );
        let frame = &mut self.frames[0];
        if !frame.contains_key(name) {
            frame.insert(name.to_string(), Value::Nil);
        }
        frame.get_mut(name).unwrap() as *mut Value
    }

    /// Like `global_slot_ptr`, but returns `None` when the slot is
    /// absent instead of inserting a fresh `Nil`. RHS prebinding for
    /// the For-loop sync fast path must use this — inserting Nil for
    /// an unbound RHS reference silently masks an undefined-variable
    /// error, because subsequent `scope.get(name)` reads would then
    /// see the synthetic Nil instead of falling through to the
    /// canonical RuntimeError.
    ///
    /// Safety contract on the returned `*mut Value` is identical to
    /// `global_slot_ptr` above (see its three-clause contract and the
    /// WS3-C.4b note); this method only differs in the absence-handling
    /// of the lookup, not in the pointer-validity rules.
    pub fn try_global_slot_ptr(&mut self, name: &str) -> Option<*mut Value> {
        // SPEC 18 Phase 2 WS3-C.7b — γ defensive guard. See
        // `global_slot_ptr` above for the rationale; this sibling
        // method is gated identically.
        assert!(
            self.shared_globals.is_none(),
            "try_global_slot_ptr called on a shared-globals Scope; \
             callers must check Evaluator::can_use_shared_scope_raw_slots() first \
             (or use Self::with_mut_value for in-place mutation under γ)"
        );
        let frame = &mut self.frames[0];
        frame.get_mut(name).map(|v| v as *mut Value)
    }

    /// Acquire stable raw pointers to TWO global slots at once.
    ///
    /// **Why this exists.** Creating a missing slot via
    /// [`global_slot_ptr`](Self::global_slot_ptr) may insert into
    /// `frames[0]` and rehash its bucket array. Two *sequential*
    /// `global_slot_ptr` calls are therefore a use-after-free: the
    /// second call's insert can relocate the buckets and invalidate the
    /// pointer the first call returned. The For/ForEach raw-slot fast
    /// paths hit exactly this (loop var + accumulator/target). This
    /// helper inserts BOTH keys first (discarding the transient
    /// pointers), then re-fetches via the non-inserting lookup — so once
    /// both keys exist no further insert (hence no rehash) can occur and
    /// both returned pointers are stable for the same live-window the
    /// single-slot contract already requires. If `a == b` the two
    /// pointers intentionally alias the one slot.
    pub fn global_slot_ptr_pair(&mut self, a: &str, b: &str) -> (*mut Value, *mut Value) {
        let _ = self.global_slot_ptr(a);
        let _ = self.global_slot_ptr(b);
        // Both keys now present; the non-inserting fetch cannot rehash.
        let pa = self
            .try_global_slot_ptr(a)
            .expect("slot just created by global_slot_ptr(a)");
        let pb = self
            .try_global_slot_ptr(b)
            .expect("slot just created by global_slot_ptr(b)");
        (pa, pb)
    }

    /// Raw pointer to a key already present in the current (top) frame.
    /// Returns `None` if missing. Used by `try_call_function_sync_block`
    /// to cache param-slot addresses immediately after binding, so
    /// `try_eval_expr_sync` Variable reads inside the body skip the
    /// per-access HashMap probe. Safety contract identical to
    /// `global_slot_ptr` — caller must guarantee no inserts on the top
    /// frame for the lifetime of the pointer (enforced by the
    /// `body_no_local_assigns` flag on `MixFunction`).
    pub fn current_frame_slot_ptr(&mut self, name: &str) -> Option<*mut Value> {
        let frame = self.frames.last_mut()?;
        frame.get_mut(name).map(|v| v as *mut Value)
    }

    #[inline]
    /// Bind `name` in the GLOBAL frame — the shared handle when present,
    /// else the owned `frames[0]`. Safe sibling of `global_slot_ptr` (no
    /// raw pointers, no γ restriction): `dispatch_event` uses it for the
    /// dynamic-extent `$event` binding (0.63.0), which must land in the
    /// frame function bodies fall through to.
    pub fn set_global(&mut self, name: &str, value: Value) {
        if let Some(shared) = self.shared_globals.as_ref() {
            shared.borrow_mut().insert(name.to_string(), value);
        } else {
            self.frames[0].insert(name.to_string(), value);
        }
    }

    /// Read `name` from the GLOBAL frame ONLY (no local-frame walk) —
    /// the save half of a global save/restore bracket.
    pub fn get_global_only(&self, name: &str) -> Option<Value> {
        if let Some(shared) = self.shared_globals.as_ref() {
            shared.borrow().get(name).cloned()
        } else {
            self.frames[0].get(name).cloned()
        }
    }

    pub fn set_in_current(&mut self, name: String, value: Value) {
        if let Some(frame) = self.frames.last_mut() {
            frame.insert(name, value);
        }
    }

    /// Update a variable that already exists in some visible scope, or
    /// create it in the current frame.
    ///
    /// Takes `&str` rather than `String` so the common hot-loop case
    /// ("variable already exists, just update its value") doesn't pay
    /// for a `String` allocation per call. Only the create-new-entry
    /// branch needs to allocate.
    #[inline]
    pub fn update_or_set(&mut self, name: &str, value: Value) {
        // Top-level fast path: bypass the floor walk when there's only
        // one frame and no active function. loop_arith's per-iter
        // `update_or_set("sum", …)` and `update_or_set("i", …)` come
        // through here — skipping the iter saves cache + branches.
        // Gated on standalone mode: in shared mode `frames[0]` is an
        // empty local-root frame, not the global frame, so this fast
        // path would silently land global updates in the local root.
        if self.shared_globals.is_none()
            && self.function_floors.is_empty()
            && self.frames.len() == 1
        {
            let frame = &mut self.frames[0];
            if let Some(slot) = frame.get_mut(name) {
                *slot = value;
                return;
            }
            frame.insert(name.to_string(), value);
            return;
        }
        let floor = self.floor();
        // Visible frames top-down.
        for i in (floor..self.frames.len()).rev() {
            if let Some(slot) = self.frames[i].get_mut(name) {
                *slot = value;
                return;
            }
        }
        // Globals fallback: standalone via owned `frames[0]`,
        // shared via the `RefCell`-protected handle. The
        // `update_or_set` semantics — "update existing same-named
        // global if one exists, else stay local to the current
        // frame" — must be preserved exactly; mis-stating this would
        // silently leak per-invocation `$rc` writes into other
        // invocations' visibility (or vice versa), the plan §4 WS3
        // "Scope resolution semantics — preserved verbatim" hazard.
        if let Some(shared) = self.shared_globals.as_ref() {
            let mut globals = shared.borrow_mut();
            if let Some(slot) = globals.get_mut(name) {
                *slot = value;
                return;
            }
            // Fall through to local-insert path. Drop the borrow
            // first — `frames.last_mut()` below is unrelated state,
            // but explicit drop documents the borrow window.
            drop(globals);
        } else if floor > 0
            && let Some(slot) = self.frames[0].get_mut(name)
        {
            *slot = value;
            return;
        }
        // Not found — must allocate a new String key in the current frame.
        if let Some(frame) = self.frames.last_mut() {
            frame.insert(name.to_string(), value);
        }
    }

    pub fn push_frame(&mut self) {
        self.frames.push(FxHashMap::default());
    }

    pub fn pop_frame(&mut self) {
        if self.frames.len() > 1 {
            self.frames.pop();
        }
    }

    /// Enter a new function call: push a fresh frame and record its
    /// index as a floor so subsequent lookups can't see frames below
    /// it (except globals). O(1). Reuses a pooled frame if available
    /// so the bucket array allocation amortizes across recursive calls.
    pub fn enter_function_frame(&mut self) {
        self.function_floors.push(self.frames.len());
        let frame = self.frame_pool.pop().unwrap_or_default();
        self.frames.push(frame);
    }

    /// Leave the current function call: drop all frames at or above
    /// the recorded floor (the function's own frame plus any block
    /// frames that may not have been popped, e.g. on early return).
    /// Cleared frames go back into the pool for reuse — the HashMap
    /// retains its bucket capacity, so the next `enter_function_frame`
    /// can bind params without reallocating.
    pub fn leave_function_frame(&mut self) {
        if let Some(floor) = self.function_floors.pop() {
            while self.frames.len() > floor {
                let mut frame = self.frames.pop().unwrap();
                frame.clear();
                // Cap pool entries to avoid retaining bloated frames
                // (e.g. a function that touched 1000 vars). 32 buckets
                // covers typical small frames without unbounded growth.
                if frame.capacity() <= 32 && self.frame_pool.len() < 64 {
                    self.frame_pool.push(frame);
                }
            }
        }
    }

    pub fn frame_count(&self) -> usize {
        self.frames.len()
    }

    /// Forcibly restore the frame stack to `floor` frames after a
    /// **caught panic** unwound `Evaluator::execute` mid-handler
    /// (SPEC 18 §3.4 fault isolation, `run_handlers_for`). A panic skips
    /// every `pop_frame` / `leave_function_frame` between the panic site
    /// and the catch, leaving block frames and `function_floors` for
    /// frames that no longer logically exist. `floor` MUST be a depth
    /// captured via [`frame_count`](Self::frame_count) *before* the
    /// handler ran, so the global frame (index 0) is never dropped.
    ///
    /// Only ever truncates (never grows): a clean handler already
    /// balances its own frames, so on the non-panic path this is a
    /// no-op. Stale `function_floors` (those at or above the restored
    /// top) are dropped so the next handler's variable lookups don't
    /// search through floors pointing past the live stack. Cleared
    /// frames are not pooled — a panicked frame's capacity is untrusted.
    ///
    /// SPEC 18 Phase 2 WS3-C.7b (γ invariant): truncation operates on
    /// `self.frames` ONLY. `shared_globals` and `shared_functions`
    /// are NEVER touched here — they live behind `Rc<RefCell<_>>`
    /// and represent state shared across all per-invocation
    /// evaluators in the same citizen. A panic in one invocation
    /// MUST NOT erase another invocation's globals or function
    /// definitions. The `target = floor.max(1)` guard also ensures
    /// the local-root `frames[0]` (empty in shared mode, the global
    /// frame in standalone mode) is never dropped.
    pub fn restore_frames_to(&mut self, floor: usize) {
        let target = floor.max(1);
        self.frames.truncate(target);
        while self
            .function_floors
            .last()
            .is_some_and(|&f| f >= self.frames.len())
        {
            self.function_floors.pop();
        }
    }

    /// Iterate the current (top) frame's entries. Used by
    /// `Expr::FunctionLiteral` to snapshot captures for inner-frame
    /// lambdas. Returns owned clones so callers don't tangle borrow
    /// lifetimes with subsequent `&mut self` operations.
    pub fn current_frame_entries(&self) -> Vec<(String, Value)> {
        self.frames
            .last()
            .map(|f| f.iter().map(|(k, v)| (k.clone(), v.clone())).collect())
            .unwrap_or_default()
    }

    /// Snapshot only the current (top) frame entries whose name appears
    /// in `names`. Used by `Expr::FunctionLiteral` for **free-variable
    /// capture**: a lambda closes over just the variables its body
    /// actually references, not the whole enclosing frame.
    ///
    /// This is behaviour-identical to [`current_frame_entries`] filtered
    /// to `names`, but skips cloning frame values the lambda never
    /// reads. A name absent from the current frame is simply omitted —
    /// matching the old whole-frame snapshot, which also never reached
    /// below the top frame (a genuinely-global reference resolves live
    /// via frame 0 at call time either way). Iterating `names` (usually
    /// a handful of referenced identifiers) and probing the frame keeps
    /// this O(referenced-vars), not O(frame-size) — the whole point of
    /// the fix, since a large in-scope value (e.g. a multi-thousand-
    /// event score) would otherwise be deep-cloned on every lambda call.
    pub fn snapshot_frame_vars(&self, names: &HashSet<String>) -> HashMap<String, Value> {
        let mut snap = HashMap::with_capacity(names.len());
        if let Some(frame) = self.frames.last() {
            for name in names {
                if let Some(v) = frame.get(name) {
                    snap.insert(name.clone(), v.clone());
                }
            }
        }
        snap
    }

    pub fn define_function(&mut self, func: MixFunction) {
        // SPEC 18 Phase 2 WS3-C.7b (γ): in shared mode, function
        // definitions land in the shared registry so peer per-
        // invocation Scopes (and the parent on resume) see them. In
        // standalone mode this stays a plain local insert — no
        // `RefCell` touch on the hot single-evaluator path.
        if let Some(shared) = self.shared_functions.as_ref() {
            shared
                .borrow_mut()
                .insert(func.name.to_string(), Rc::new(func));
        } else {
            self.functions.insert(func.name.to_string(), Rc::new(func));
        }
    }

    /// Borrowing function lookup. **Standalone-mode only**: in shared
    /// mode this returns `None` (the shared `Rc<RefCell<_>>` cannot
    /// hand out a `&Rc<MixFunction>` that outlives a `borrow()`).
    /// Per-invocation callers must use [`Self::get_function_owned`]
    /// which clones the inner `Rc` out (a refcount bump, no AST
    /// copy).
    pub fn get_function(&self, name: &str) -> Option<&Rc<MixFunction>> {
        if self.shared_functions.is_some() {
            return None;
        }
        self.functions.get(name)
    }

    /// SPEC 18 Phase 2 WS3-C.7b — γ-aware function lookup. Works in
    /// both modes; returns a cloned `Rc<MixFunction>` (a refcount
    /// bump only — the AST body is reference-counted, never deep-
    /// copied). Use in shared mode, or anywhere the caller will
    /// `.clone()` the result anyway.
    pub fn get_function_owned(&self, name: &str) -> Option<Rc<MixFunction>> {
        if let Some(shared) = self.shared_functions.as_ref() {
            return shared.borrow().get(name).cloned();
        }
        self.functions.get(name).cloned()
    }

    /// SPEC 18 Phase 2 WS3-C.7b — γ-aware "does a function with this
    /// name exist?" predicate. Works in both modes without paying the
    /// `Rc` clone that `get_function_owned(name).is_some()` would.
    /// Public API like `Evaluator::has_function` and the address-block
    /// "unknown function falls through to send" check route through
    /// this instead of `get_function(name).is_some()`, which would
    /// return `false` in shared mode and break the contract.
    pub fn has_function(&self, name: &str) -> bool {
        if let Some(shared) = self.shared_functions.as_ref() {
            return shared.borrow().contains_key(name);
        }
        self.functions.contains_key(name)
    }

    /// Get all user-defined function names with their parameter names.
    /// SPEC 18 Phase 2 WS3-C.7b (γ): in shared mode this enumerates
    /// the shared registry (the local `functions` map is empty after
    /// promotion).
    pub fn function_names(&self) -> Vec<(String, Vec<String>)> {
        if let Some(shared) = self.shared_functions.as_ref() {
            return shared
                .borrow()
                .iter()
                .map(|(name, func)| {
                    let params: Vec<String> = func.params.iter().map(|p| p.name.clone()).collect();
                    (name.clone(), params)
                })
                .collect();
        }
        self.functions
            .iter()
            .map(|(name, func)| {
                let params: Vec<String> = func.params.iter().map(|p| p.name.clone()).collect();
                (name.clone(), params)
            })
            .collect()
    }

    /// Get all variable names visible in current scope (for tab completion).
    /// Honours function isolation: inside a function only visible frames
    /// + globals are reported.
    pub fn variable_names(&self) -> Vec<String> {
        let floor = self.floor();
        let mut names = Vec::new();
        for frame in &self.frames[floor..] {
            for key in frame.keys() {
                if !names.contains(key) {
                    names.push(key.clone());
                }
            }
        }
        // Globals: standalone owns `frames[0]` (only consulted when
        // `floor > 0` since the loop above already covered
        // `frames[0]` at `floor == 0`); shared mode always consults
        // the shared frame (the loop never covers it — `frames[0]`
        // is the empty local-root in shared mode).
        if let Some(shared) = self.shared_globals.as_ref() {
            for key in shared.borrow().keys() {
                if !names.contains(key) {
                    names.push(key.clone());
                }
            }
        } else if floor > 0 {
            for key in self.frames[0].keys() {
                if !names.contains(key) {
                    names.push(key.clone());
                }
            }
        }
        names
    }

    /// `require()` harvest (FIX 1): consume a scope — the swapped-out
    /// module scope — into its global frame and flat function registry.
    /// ONLY meaningful for an unpromoted standalone scope
    /// (`Scope::new()` + top-level execution): with `Scope::new()`,
    /// `frames[0]` IS the global frame, so every module top-level
    /// `$var` landed exactly where this reads. A `with_shared` scope
    /// would strand module top-level vars in an empty local-root frame
    /// (the shared-globals `update_or_set` double-miss falls through to
    /// a local insert) — harvesting one is the single silent defect
    /// that kills `require`, hence the debug asserts.
    pub(crate) fn into_module_parts(
        mut self,
    ) -> (FxHashMap<String, Value>, HashMap<String, Rc<MixFunction>>) {
        debug_assert!(
            !self.uses_shared_globals(),
            "module scope must be standalone"
        );
        debug_assert!(
            !self.uses_shared_functions(),
            "module scope must be standalone"
        );
        debug_assert!(
            self.function_floors.is_empty(),
            "module scope consumed mid-call"
        );
        (
            std::mem::take(&mut self.frames[0]),
            std::mem::take(&mut self.functions),
        )
    }

    /// Function-valued variable lookup restricted to the ACTIVE
    /// function's own frames (at or above the topmost function floor).
    /// Returns `None` at the top level (no active function) — bareword
    /// dispatch outside a function is unchanged by `require()`'s
    /// sibling-call rule. Inside a function frame, `module_env` /
    /// `captures` / params are frame-injected, so a `require()`d
    /// module's sibling and private functions resolve here and win
    /// over the caller's function registry regardless of the caller's
    /// function names.
    pub(crate) fn get_function_var_in_function_frames(
        &self,
        name: &str,
    ) -> Option<Rc<MixFunction>> {
        let floor = *self.function_floors.last()?;
        for frame in self.frames[floor..].iter().rev() {
            if let Some(v) = frame.get(name) {
                return match v {
                    Value::Function(rc) => Some(Rc::clone(rc)),
                    _ => None,
                };
            }
        }
        None
    }

    /// Get all variables visible in current scope with their values.
    /// Topmost frame wins on duplicates. Honours function isolation.
    pub fn all_variables(&self) -> Vec<(String, Value)> {
        let floor = self.floor();
        let mut seen = std::collections::HashSet::new();
        let mut result = Vec::new();
        // Visible frames top-down so topmost wins.
        for frame in self.frames[floor..].iter().rev() {
            for (k, v) in frame {
                if seen.insert(k.clone()) {
                    result.push((k.clone(), v.clone()));
                }
            }
        }
        // Globals (γ-aware — same shape as `variable_names`).
        if let Some(shared) = self.shared_globals.as_ref() {
            for (k, v) in shared.borrow().iter() {
                if seen.insert(k.clone()) {
                    result.push((k.clone(), v.clone()));
                }
            }
        } else if floor > 0 {
            for (k, v) in &self.frames[0] {
                if seen.insert(k.clone()) {
                    result.push((k.clone(), v.clone()));
                }
            }
        }
        result.sort_by(|a, b| a.0.cmp(&b.0));
        result
    }
}

#[cfg(test)]
mod gamma_tests {
    //! SPEC 18 Phase 2 WS3-C.7b — γ dual-mode `Scope` unit tests.
    //!
    //! Standalone-mode behaviour stays bit-for-bit identical to
    //! today (existing integration tests guard that); these tests
    //! focus on the new shared-mode paths and the lazy-promotion
    //! seam that `Evaluator::for_invocation` will exercise in
    //! C.7d.
    use super::*;
    use crate::value::Value;
    use std::cell::RefCell;
    use std::rc::Rc;

    fn empty_shared() -> (SharedGlobals, SharedFunctions) {
        (
            Rc::new(RefCell::new(FxHashMap::default())),
            Rc::new(RefCell::new(HashMap::new())),
        )
    }

    #[test]
    fn standalone_scope_basic_unchanged() {
        let mut s = Scope::new();
        assert!(!s.uses_shared_globals());
        assert!(!s.uses_shared_functions());
        s.set("x".to_string(), Value::Number(1.0));
        assert!(matches!(s.get("x"), Some(Value::Number(n)) if *n == 1.0));
        s.update_or_set("x", Value::Number(42.0));
        assert!(matches!(s.get("x"), Some(Value::Number(n)) if *n == 42.0));
        assert!(s.is_top_level());
    }

    #[test]
    fn shared_globals_visible_across_scopes() {
        let (globals, functions) = empty_shared();
        let mut a = Scope::with_shared(Rc::clone(&globals), Rc::clone(&functions));
        let b = Scope::with_shared(globals, functions);
        // Write a global from `a` (no local with this name yet → no
        // local match → shared globals try → not present → falls
        // through to local insert. So this stays local.)
        a.update_or_set("local_only", Value::Number(7.0));
        // Direct injection into the shared map: peer scope sees it
        // via `get_value`.
        a.shared_globals_handle()
            .borrow_mut()
            .insert("g".to_string(), Value::Number(99.0));
        let v = b.get_value("g").expect("global visible across scopes");
        assert!(matches!(&*v, Value::Number(n) if *n == 99.0));
        // Local insert in `a` is invisible to `b`.
        assert!(b.get_value("local_only").is_none());
    }

    #[test]
    fn update_or_set_walks_shared_globals_when_present() {
        let (globals, functions) = empty_shared();
        globals
            .borrow_mut()
            .insert("counter".to_string(), Value::Number(0.0));
        let mut a = Scope::with_shared(Rc::clone(&globals), Rc::clone(&functions));
        let b = Scope::with_shared(globals, functions);
        // Existing global → `update_or_set` updates in place (visible
        // to peer).
        a.update_or_set("counter", Value::Number(5.0));
        let v = b.get_value("counter").expect("shared update visible");
        assert!(matches!(&*v, Value::Number(n) if *n == 5.0));
    }

    #[test]
    fn shared_globals_promoted_lazily_preserves_existing() {
        let mut s = Scope::new();
        s.set("preexisting".to_string(), Value::Number(123.0));
        // First call promotes — same content, now behind `Rc<RefCell<_>>`.
        let handle = s.shared_globals_handle();
        assert!(s.uses_shared_globals());
        assert!(matches!(
            handle.borrow().get("preexisting"),
            Some(Value::Number(n)) if *n == 123.0
        ));
        // Second call returns the SAME handle (Rc clone, not a fresh one).
        let handle2 = s.shared_globals_handle();
        assert!(Rc::ptr_eq(&handle, &handle2));
        // `get_value` still surfaces the promoted slot (clone-out arm).
        let v = s.get_value("preexisting").expect("post-promotion lookup");
        assert!(matches!(&*v, Value::Number(n) if *n == 123.0));
    }

    #[test]
    fn restore_frames_to_does_not_touch_shared_globals() {
        let (globals, functions) = empty_shared();
        globals
            .borrow_mut()
            .insert("g".to_string(), Value::Number(1.0));
        let mut s = Scope::with_shared(Rc::clone(&globals), Rc::clone(&functions));
        s.push_frame();
        s.push_frame();
        let baseline = 1;
        s.restore_frames_to(baseline);
        assert_eq!(s.frame_count(), 1);
        // Shared global UNAFFECTED by frame truncation.
        assert!(matches!(
            globals.borrow().get("g"),
            Some(Value::Number(n)) if *n == 1.0
        ));
    }

    #[test]
    fn get_value_returns_owned_for_shared_and_borrowed_for_local() {
        let (globals, functions) = empty_shared();
        globals
            .borrow_mut()
            .insert("g".to_string(), Value::Number(10.0));
        let mut s = Scope::with_shared(globals, functions);
        s.set_in_current("loc".to_string(), Value::Number(20.0));
        match s.get_value("g") {
            Some(ScopeValue::Owned(_)) => {}
            other => panic!(
                "expected Owned for shared global, got {:?}",
                other.is_some()
            ),
        }
        match s.get_value("loc") {
            Some(ScopeValue::Borrowed(_)) => {}
            other => panic!("expected Borrowed for local, got {:?}", other.is_some()),
        }
        assert!(s.get_value("missing").is_none());
    }

    #[test]
    fn with_mut_value_mutates_shared_globals_in_place() {
        let (globals, functions) = empty_shared();
        globals
            .borrow_mut()
            .insert("g".to_string(), Value::Number(5.0));
        let mut s = Scope::with_shared(Rc::clone(&globals), functions);
        let prev = s
            .with_mut_value("g", |v| {
                let old = v.clone();
                *v = Value::Number(50.0);
                old
            })
            .expect("with_mut_value hit");
        assert!(matches!(prev, Value::Number(n) if n == 5.0));
        assert!(matches!(
            globals.borrow().get("g"),
            Some(Value::Number(n)) if *n == 50.0
        ));
        // Missing name → None, closure not called.
        let r = s.with_mut_value("missing", |_v| panic!("closure must not fire"));
        assert!(r.is_none());
    }

    #[test]
    fn shared_functions_visible_across_scopes_and_define_routes() {
        let (globals, functions) = empty_shared();
        let mut a = Scope::with_shared(Rc::clone(&globals), Rc::clone(&functions));
        let b = Scope::with_shared(globals, functions);
        a.define_function(MixFunction {
            name: std::rc::Rc::from("f"),
            params: vec![],
            body: FunctionBody::Expression(crate::ast::Expr::NumberLiteral(7.0)),
            def_file: None,
            captures: None,
            sync_prefix_len: 0,
            block_sync_self: false,
            body_no_local_assigns: false,
            fib_bytecode: None,
            module_env: None,
        });
        // Standalone-mode `get_function` returns None on shared
        // scopes (per its doc contract); use `get_function_owned`.
        assert!(b.get_function("f").is_none());
        let f = b.get_function_owned("f").expect("shared function visible");
        assert_eq!(f.name.as_ref(), "f");
        // `function_names` enumerates the shared registry in shared
        // mode.
        let names: Vec<String> = b.function_names().into_iter().map(|(n, _)| n).collect();
        assert_eq!(names, vec!["f".to_string()]);
    }

    #[test]
    #[should_panic(expected = "global_slot_ptr called on a shared-globals Scope")]
    fn global_slot_ptr_panics_on_shared_scope() {
        let (globals, functions) = empty_shared();
        let mut s = Scope::with_shared(globals, functions);
        let _ = s.global_slot_ptr("x");
    }

    #[test]
    #[should_panic(expected = "try_global_slot_ptr called on a shared-globals Scope")]
    fn try_global_slot_ptr_panics_on_shared_scope() {
        let (globals, functions) = empty_shared();
        let mut s = Scope::with_shared(globals, functions);
        let _ = s.try_global_slot_ptr("x");
    }

    #[test]
    fn is_top_level_false_in_shared_mode() {
        let (globals, functions) = empty_shared();
        let s = Scope::with_shared(globals, functions);
        // `frames.len() == 1 && function_floors.is_empty()` holds,
        // but shared mode flips the predicate per its updated doc
        // contract.
        assert!(!s.is_top_level());
    }
}
