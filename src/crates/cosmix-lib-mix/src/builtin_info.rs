//! Single source of truth for builtin metadata.
//!
//! The `builtin_table!` macro emits two const arrays from one list:
//!   - `$INFOS: &[BuiltinInfo]` — name + capability + category + description
//!     + machine-readable [`BuiltinContract`]
//!   - `$NAMES: &[&str]`       — names only (for completion / "never used" audit)
//!
//! This replaces the v0.2.5-and-earlier pattern of three hand-copied
//! lists (`builtins::BUILTIN_NAMES`, `builtins_hof::HOF_NAMES`,
//! `meta::BUILTIN_DESCRIPTIONS`) that silently drifted every time a
//! new builtin landed. Now adding a builtin means editing exactly one
//! table — the one next to the implementation.
//!
//! Since 0.29.0 every entry carries a structured [`BuiltinContract`]
//! (arity, per-argument names/kinds, return shape, effect flags,
//! operational-failure mode, conditional capabilities) declared with
//! the [`contract!`] macro. The human-facing signature string is
//! *derived* from the contract ([`BuiltinInfo::signature`]) rather than
//! hand-maintained, and `mix builtins --json` / `--data` expose the
//! same structure to agents (metadata_schema 1). There is no
//! unstructured fallback: an entry without a contract does not compile.

#[derive(Debug, Clone, Copy)]
pub struct BuiltinInfo {
    pub name: &'static str,
    /// Capability class this builtin exercises — declared at the table
    /// (the single source of truth), so a `CapabilityPolicy` can gate it.
    /// Required for every entry, which is what makes a sandbox-bypassing
    /// fail-open *impossible by construction*: you cannot add a builtin
    /// without naming its class.
    pub capability: crate::builtins::CapabilityClass,
    pub category: &'static str,
    pub description: &'static str,
    /// Machine-readable contract: arity, argument kinds, return shape,
    /// effect flags, operational-failure mode, conditional capabilities.
    /// Mandatory — declared with [`contract!`] beside the entry.
    pub contract: BuiltinContract,
}

/// Recursive value-shape vocabulary for argument kinds and return
/// shapes. Wire names (JSON/`--data`) are the lowercase Mix type names;
/// see [`TypeShape::wire_name`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TypeShape {
    Any,
    Nil,
    Bool,
    Number,
    String,
    Bytes,
    Buffer,
    Function,
    /// Homogeneous list; `list` in the DSL is sugar for `List(&Any)`.
    List(&'static TypeShape),
    /// Map, optionally tagged with a named result shape (e.g.
    /// `"process_result"`) and optionally carrying known fields.
    Map {
        shape: Option<&'static str>,
        fields: &'static [FieldInfo],
    },
    /// Union of simple shapes (`any_of(string, number)`).
    AnyOf(&'static [TypeShape]),
}

/// A named field of a `map(...)` shape (used by shared shape constants
/// such as `PROCESS_RESULT`; most entries leave `fields` empty).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FieldInfo {
    pub name: &'static str,
    pub kind: &'static TypeShape,
}

/// One declared argument. Ordering invariant (checked by a unit test,
/// not the type system): required args precede optional args; at most
/// one variadic arg, and it must be last.
#[derive(Debug, Clone, Copy)]
pub struct ArgInfo {
    pub name: &'static str,
    pub required: bool,
    pub variadic: bool,
    pub kind: TypeShape,
}

/// Effect flags an analyzer or agent can key on.
///
/// - `must_use`: discarding the result is almost certainly a bug
///   (lint MIX-W2201).
/// - `blocking`: may block the evaluator for unbounded or
///   externally-determined time (child processes, network, stdin,
///   sleep, db/jmap/bus round-trips). Ordinary file I/O is not flagged.
/// - `shell`: execution is backed by `/bin/sh` (shell-injection
///   surface; prefer argv builtins where available).
/// - `mutates_args`: mutates an argument in place (`push`,
///   `buffer_push`, ...).
/// - `terminates`: does not return control to the script (`exit`,
///   `panic`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct EffectFlags {
    pub must_use: bool,
    pub blocking: bool,
    pub shell: bool,
    pub mutates_args: bool,
    pub terminates: bool,
}

impl EffectFlags {
    pub const NONE: EffectFlags = EffectFlags {
        must_use: false,
        blocking: false,
        shell: false,
        mutates_args: false,
        terminates: false,
    };
}

/// How the builtin reports *operational* failure (I/O errors, network
/// failure, child exit) — distinct from argument-validation errors,
/// which always raise.
///
/// - `NotApplicable`: pure computation; no operational failure mode.
/// - `Raises`: operational failure raises a catchable Mix error.
/// - `ReturnsResult`: operational failure is encoded in the returned
///   value (`ok:false`, `status:0`, exit code, ...) and does not raise.
/// - `Terminates`: the builtin ends the process (`exit`, `panic`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OperationalFailure {
    NotApplicable,
    Raises,
    ReturnsResult,
    Terminates,
}

impl OperationalFailure {
    pub fn wire_name(self) -> &'static str {
        match self {
            OperationalFailure::NotApplicable => "not-applicable",
            OperationalFailure::Raises => "raises",
            OperationalFailure::ReturnsResult => "returns-result",
            OperationalFailure::Terminates => "terminates",
        }
    }
}

/// A capability the builtin exercises only when a given option is
/// present (e.g. `http_get` needs `fs-read` only when `ca_file` is
/// passed). The base `capability` on the entry is always required.
#[derive(Debug, Clone, Copy)]
pub struct CondCap {
    pub option: &'static str,
    pub capability: crate::builtins::CapabilityClass,
}

/// The machine-readable contract carried by every table entry.
#[derive(Debug, Clone, Copy)]
pub struct BuiltinContract {
    pub args: &'static [ArgInfo],
    pub returns: TypeShape,
    pub effects: EffectFlags,
    pub failure: OperationalFailure,
    pub cond_caps: &'static [CondCap],
    /// For builtins whose valid arities are NOT a contiguous range
    /// (e.g. `random()` takes exactly 0 or 2 arguments — never 1),
    /// the exact accepted argument counts. `None` = the min..=max
    /// range derived from `args` applies. Declared with the
    /// `arities[0, 2]` clause in [`contract!`].
    pub exact_arities: Option<&'static [usize]>,
}

impl BuiltinContract {
    /// Minimum number of arguments (count of required args).
    pub fn arity_min(&self) -> usize {
        self.args.iter().filter(|a| a.required).count()
    }

    /// Maximum number of arguments; `None` = unbounded (variadic).
    pub fn arity_max(&self) -> Option<usize> {
        if self.args.iter().any(|a| a.variadic) {
            None
        } else {
            Some(self.args.len())
        }
    }

    /// Whether a call with `n` arguments satisfies the declared arity
    /// (the check `mix lint` applies).
    pub fn accepts_arity(&self, n: usize) -> bool {
        if let Some(set) = self.exact_arities {
            return set.contains(&n);
        }
        n >= self.arity_min() && self.arity_max().is_none_or(|max| n <= max)
    }
}

impl TypeShape {
    /// Wire name for the JSON/`--data` discovery surface.
    pub fn wire_name(&self) -> &'static str {
        match self {
            TypeShape::Any => "any",
            TypeShape::Nil => "nil",
            TypeShape::Bool => "bool",
            TypeShape::Number => "number",
            TypeShape::String => "string",
            TypeShape::Bytes => "bytes",
            TypeShape::Buffer => "buffer",
            TypeShape::Function => "function",
            TypeShape::List(_) => "list",
            TypeShape::Map { .. } => "map",
            TypeShape::AnyOf(_) => "any_of",
        }
    }

    /// Human rendering for derived signatures: `list`, `map`,
    /// `map<process_result>`, `string | number`.
    pub fn human(&self) -> String {
        match self {
            TypeShape::Map {
                shape: Some(name), ..
            } => format!("map<{name}>"),
            TypeShape::AnyOf(shapes) => shapes
                .iter()
                .map(|s| s.human())
                .collect::<Vec<_>>()
                .join(" | "),
            other => other.wire_name().to_string(),
        }
    }
}

impl BuiltinInfo {
    /// Derived human-facing signature, e.g.
    /// `substr(s, start[, len]) -> string` or
    /// `run_argv(argv[, opts]) -> map<process_result>`.
    /// Generated from the contract so it can never drift from the
    /// machine metadata.
    pub fn signature(&self) -> String {
        render_signature(self.name, &self.contract)
    }
}

/// Render a derived signature for any (name, contract) pair — shared by
/// [`BuiltinInfo::signature`] and the statement extras (`print`/`eprint`)
/// in the CLI's discovery surface. Contracts with an exact-arity set
/// render one variant per accepted count (`random() | random(min, max)
/// -> number`), so the display can never imply an arity the runtime
/// rejects.
pub fn render_signature(name: &str, contract: &BuiltinContract) -> String {
    if let Some(set) = contract.exact_arities {
        let variants: Vec<String> = set
            .iter()
            .map(|&n| {
                let names: Vec<&str> = contract.args.iter().take(n).map(|a| a.name).collect();
                format!("{}({})", name, names.join(", "))
            })
            .collect();
        return format!("{} -> {}", variants.join(" | "), contract.returns.human());
    }
    {
        let mut out = String::new();
        out.push_str(name);
        out.push('(');
        let mut first = true;
        for arg in contract.args {
            if arg.required {
                if !first {
                    out.push_str(", ");
                }
                out.push_str(arg.name);
            } else if arg.variadic {
                if first {
                    out.push_str("...");
                } else {
                    out.push_str("[, ...");
                }
                out.push_str(arg.name);
                if !first {
                    out.push(']');
                }
            } else if first {
                out.push('[');
                out.push_str(arg.name);
                out.push(']');
            } else {
                out.push_str("[, ");
                out.push_str(arg.name);
                out.push(']');
            }
            first = false;
        }
        out.push_str(") -> ");
        out.push_str(&contract.returns.human());
        out
    }
}

/// Declare a builtin metadata table. Emits a pair of `pub const`s:
/// the full `BuiltinInfo` array and a names-only `&[&str]`.
///
/// Each entry is `(name, capability, category, description, contract)`.
/// The `capability` (a `CapabilityClass`) and the `contract` (a
/// [`BuiltinContract`], normally written with [`contract!`]) are both
/// mandatory: a builtin cannot be added without classifying it and
/// declaring its structured contract.
///
/// Usage:
/// ```ignore
/// use crate::builtin_table;
/// builtin_table! {
///     BUILTINS, BUILTIN_NAMES,
///     ("length", CapabilityClass::Pure, "string", "Length of string/list/map",
///      contract!((v: any_of(string, list, map)) -> number)),
/// }
/// ```
#[macro_export]
macro_rules! builtin_table {
    (
        $infos:ident, $names:ident,
        $( ($name:literal, $cap:expr, $cat:literal, $desc:literal, $contract:expr) ),* $(,)?
    ) => {
        pub const $infos: &[$crate::builtin_info::BuiltinInfo] = &[
            $(
                $crate::builtin_info::BuiltinInfo {
                    name: $name,
                    capability: $cap,
                    category: $cat,
                    description: $desc,
                    contract: $contract,
                },
            )*
        ];
        pub const $names: &[&str] = &[ $( $name, )* ];
    };
}

/// Build a [`TypeShape`] from the compact DSL:
/// `any nil bool number string bytes buffer function list list(T) map
/// map("shape_name") map("shape_name", {field: TYPE, ...})
/// any_of(a, b, ...)`.
/// `any_of` elements must be simple (single-token) shapes; use a plain
/// `list`/`map` inside unions.
#[macro_export]
macro_rules! shape {
    (any) => { $crate::builtin_info::TypeShape::Any };
    (nil) => { $crate::builtin_info::TypeShape::Nil };
    (bool) => { $crate::builtin_info::TypeShape::Bool };
    (number) => { $crate::builtin_info::TypeShape::Number };
    (string) => { $crate::builtin_info::TypeShape::String };
    (bytes) => { $crate::builtin_info::TypeShape::Bytes };
    (buffer) => { $crate::builtin_info::TypeShape::Buffer };
    (function) => { $crate::builtin_info::TypeShape::Function };
    (list) => { $crate::builtin_info::TypeShape::List(&$crate::builtin_info::TypeShape::Any) };
    (list( $($inner:tt)+ )) => {
        $crate::builtin_info::TypeShape::List(&$crate::shape!($($inner)+))
    };
    (map) => {
        $crate::builtin_info::TypeShape::Map { shape: None, fields: &[] }
    };
    (map( $name:literal, { $( $field:ident : $ft:ident $( ( $($fta:tt)* ) )? ),* $(,)? } )) => {
        $crate::builtin_info::TypeShape::Map {
            shape: Some($name),
            fields: &[ $( $crate::builtin_info::FieldInfo {
                name: stringify!($field),
                kind: &$crate::shape!($ft $( ( $($fta)* ) )?),
            } ),* ],
        }
    };
    (map( $name:literal )) => {
        $crate::builtin_info::TypeShape::Map { shape: Some($name), fields: &[] }
    };
    (any_of( $($t:tt),+ $(,)? )) => {
        $crate::builtin_info::TypeShape::AnyOf(&[ $( $crate::shape!($t) ),+ ])
    };
}

/// Build [`EffectFlags`] from a flag-name list: `effect_flags!(must_use, blocking)`.
#[macro_export]
macro_rules! effect_flags {
    ($($flag:ident),* $(,)?) => {{
        #[allow(unused_mut)]
        let mut f = $crate::builtin_info::EffectFlags::NONE;
        $( $crate::effect_flags!(@set f $flag); )*
        f
    }};
    (@set $f:ident must_use) => { $f.must_use = true; };
    (@set $f:ident blocking) => { $f.blocking = true; };
    (@set $f:ident shell) => { $f.shell = true; };
    (@set $f:ident mutates_args) => { $f.mutates_args = true; };
    (@set $f:ident terminates) => { $f.terminates = true; };
}

#[doc(hidden)]
#[macro_export]
macro_rules! op_failure {
    () => {
        $crate::builtin_info::OperationalFailure::NotApplicable
    };
    (not_applicable) => {
        $crate::builtin_info::OperationalFailure::NotApplicable
    };
    (raises) => {
        $crate::builtin_info::OperationalFailure::Raises
    };
    (returns_result) => {
        $crate::builtin_info::OperationalFailure::ReturnsResult
    };
    (terminates) => {
        $crate::builtin_info::OperationalFailure::Terminates
    };
}

#[doc(hidden)]
#[macro_export]
macro_rules! exact_arities {
    () => {
        None
    };
    ($($a:literal),+ $(,)?) => {
        Some(&[ $( $a ),+ ])
    };
}

#[doc(hidden)]
#[macro_export]
macro_rules! cond_caps {
    () => {
        &[]
    };
    ($($opt:ident : $cap:ident),+ $(,)?) => {
        &[ $( $crate::builtin_info::CondCap {
            option: stringify!($opt),
            capability: $crate::builtins::CapabilityClass::$cap,
        } ),+ ]
    };
}

/// Argument-list tt-muncher for [`contract!`]. Grammar per argument:
/// `name: TYPE` (required) · `name?: TYPE` (optional) ·
/// `name: ...TYPE` (variadic, must be last). Argument names must be
/// valid Rust identifiers (avoid keywords: use `kind` not `type`,
/// `func` not `fn`).
#[doc(hidden)]
#[macro_export]
macro_rules! contract_args {
    // NOTE: the `@acc` muncher rules MUST precede the public catch-all
    // entry rule, or the catch-all re-wraps `@acc ...` input forever.

    // variadic (terminal by invariant): name: ...TYPE
    (@acc [$($done:expr,)*] $n:ident : ... $t:ident $( ( $($ta:tt)* ) )?) => {
        [$($done,)* $crate::builtin_info::ArgInfo {
            name: stringify!($n), required: false, variadic: true,
            kind: $crate::shape!($t $( ( $($ta)* ) )?),
        },]
    };
    // optional, more follow: name?: TYPE, ...
    (@acc [$($done:expr,)*] $n:ident ? : $t:ident $( ( $($ta:tt)* ) )? , $($rest:tt)+) => {
        $crate::contract_args!(@acc [$($done,)* $crate::builtin_info::ArgInfo {
            name: stringify!($n), required: false, variadic: false,
            kind: $crate::shape!($t $( ( $($ta)* ) )?),
        },] $($rest)+)
    };
    // optional, last: name?: TYPE
    (@acc [$($done:expr,)*] $n:ident ? : $t:ident $( ( $($ta:tt)* ) )?) => {
        [$($done,)* $crate::builtin_info::ArgInfo {
            name: stringify!($n), required: false, variadic: false,
            kind: $crate::shape!($t $( ( $($ta)* ) )?),
        },]
    };
    // required, more follow: name: TYPE, ...
    (@acc [$($done:expr,)*] $n:ident : $t:ident $( ( $($ta:tt)* ) )? , $($rest:tt)+) => {
        $crate::contract_args!(@acc [$($done,)* $crate::builtin_info::ArgInfo {
            name: stringify!($n), required: true, variadic: false,
            kind: $crate::shape!($t $( ( $($ta)* ) )?),
        },] $($rest)+)
    };
    // required, last: name: TYPE
    (@acc [$($done:expr,)*] $n:ident : $t:ident $( ( $($ta:tt)* ) )?) => {
        [$($done,)* $crate::builtin_info::ArgInfo {
            name: stringify!($n), required: true, variadic: false,
            kind: $crate::shape!($t $( ( $($ta)* ) )?),
        },]
    };

    // public entry points (keep last — see NOTE above)
    () => { &[] };
    ($($rest:tt)+) => { &$crate::contract_args!(@acc [] $($rest)+) };
}

/// Declare a [`BuiltinContract`]:
///
/// ```ignore
/// contract!(() -> number)
/// contract!((s: string, start: number, len?: number) -> string)
/// contract!((tmpl: string, args: ...any) -> string)
/// contract!((argv: list(string), opts?: map("run_argv_options",
///           {timeout: number, stream: bool})) -> map("process_result");
///           effects[must_use, blocking]; failure[returns_result])
/// contract!((url: string, opts?: map) -> map("http_response");
///           effects[must_use, blocking]; failure[returns_result];
///           cond_caps[ca_file: FsRead])
/// ```
///
/// Clauses after the return shape are optional but fixed-order:
/// `arities[n, m]` (exact accepted argument counts, for non-contiguous
/// arity like `random`'s 0-or-2), then `effects[...]`, then
/// `failure[...]` (default `not_applicable`), then
/// `cond_caps[option: CapabilityClass, ...]`.
#[macro_export]
macro_rules! contract {
    (
        ( $($args:tt)* ) -> $rt:ident $( ( $($rta:tt)* ) )?
        $(; arities[$($ar:literal),+ $(,)?] )?
        $(; effects[$($e:ident),* $(,)?] )?
        $(; failure[$f:ident] )?
        $(; cond_caps[$($cn:ident : $cc:ident),* $(,)?] )?
    ) => {
        $crate::builtin_info::BuiltinContract {
            args: $crate::contract_args!($($args)*),
            returns: $crate::shape!($rt $( ( $($rta)* ) )?),
            effects: $crate::effect_flags!($( $($e),* )?),
            failure: $crate::op_failure!($( $f )?),
            cond_caps: $crate::cond_caps!($( $($cn : $cc),* )?),
            exact_arities: $crate::exact_arities!($( $($ar),+ )?),
        }
    };
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::builtins::CapabilityClass;

    const T_SUBSTR: BuiltinContract = contract!((s: string, start: number, len?: number) -> string);
    const T_FMT: BuiltinContract = contract!((tmpl: string, args: ...any) -> string);
    const T_NOARG: BuiltinContract = contract!(() -> number);
    const T_FULL: BuiltinContract = contract!(
        (argv: list(string), opts?: map("run_argv_options", {timeout: number, stream: bool})) -> map("process_result");
        effects[must_use, blocking];
        failure[returns_result];
        cond_caps[ca_file: FsRead]
    );

    #[test]
    fn arity_derivation() {
        assert_eq!(T_SUBSTR.arity_min(), 2);
        assert_eq!(T_SUBSTR.arity_max(), Some(3));
        assert_eq!(T_FMT.arity_min(), 1);
        assert_eq!(T_FMT.arity_max(), None);
        assert_eq!(T_NOARG.arity_min(), 0);
        assert_eq!(T_NOARG.arity_max(), Some(0));
    }

    #[test]
    #[allow(clippy::assertions_on_constants)]
    fn clauses_and_shapes() {
        assert!(T_FULL.effects.must_use);
        assert!(T_FULL.effects.blocking);
        assert!(!T_FULL.effects.shell);
        assert_eq!(T_FULL.failure, OperationalFailure::ReturnsResult);
        assert_eq!(T_FULL.cond_caps.len(), 1);
        assert_eq!(T_FULL.cond_caps[0].option, "ca_file");
        assert!(matches!(
            T_FULL.cond_caps[0].capability,
            CapabilityClass::FsRead
        ));
        match T_FULL.args[0].kind {
            TypeShape::List(inner) => assert_eq!(*inner, TypeShape::String),
            other => panic!("expected list(string), got {other:?}"),
        }
        match T_FULL.args[1].kind {
            TypeShape::Map { shape, fields } => {
                assert_eq!(shape, Some("run_argv_options"));
                assert_eq!(fields.len(), 2);
                assert_eq!(fields[1].name, "stream");
                assert_eq!(*fields[1].kind, TypeShape::Bool);
            }
            other => panic!("expected run_argv options map, got {other:?}"),
        }
        match T_FULL.returns {
            TypeShape::Map { shape, .. } => assert_eq!(shape, Some("process_result")),
            other => panic!("expected map shape, got {other:?}"),
        }
        assert_eq!(T_NOARG.failure, OperationalFailure::NotApplicable);
        assert_eq!(T_NOARG.effects, EffectFlags::NONE);
    }

    #[test]
    fn signature_rendering() {
        let info = BuiltinInfo {
            name: "substr",
            capability: CapabilityClass::Pure,
            category: "string",
            description: "",
            contract: T_SUBSTR,
        };
        assert_eq!(info.signature(), "substr(s, start[, len]) -> string");

        let vararg = BuiltinInfo {
            name: "fmt",
            capability: CapabilityClass::Pure,
            category: "format",
            description: "",
            contract: T_FMT,
        };
        assert_eq!(vararg.signature(), "fmt(tmpl[, ...args]) -> string");

        let full = BuiltinInfo {
            name: "run_argv",
            capability: CapabilityClass::Process,
            category: "system",
            description: "",
            contract: T_FULL,
        };
        assert_eq!(
            full.signature(),
            "run_argv(argv[, opts]) -> map<process_result>"
        );

        let noarg = BuiltinInfo {
            name: "pi",
            capability: CapabilityClass::Pure,
            category: "math",
            description: "",
            contract: T_NOARG,
        };
        assert_eq!(noarg.signature(), "pi() -> number");

        let union = BuiltinInfo {
            name: "reverse",
            capability: CapabilityClass::Pure,
            category: "string",
            description: "",
            contract: contract!((v: any_of(string, list)) -> any_of(string, list)),
        };
        assert_eq!(union.signature(), "reverse(v) -> string | list");
    }

    #[test]
    fn exact_arities_render_and_check() {
        const T_RANDOM: BuiltinContract =
            contract!((min?: number, max?: number) -> number; arities[0, 2]);
        assert!(T_RANDOM.accepts_arity(0));
        assert!(!T_RANDOM.accepts_arity(1));
        assert!(T_RANDOM.accepts_arity(2));
        assert!(!T_RANDOM.accepts_arity(3));
        let info = BuiltinInfo {
            name: "random",
            capability: CapabilityClass::Pure,
            category: "math",
            description: "",
            contract: T_RANDOM,
        };
        assert_eq!(info.signature(), "random() | random(min, max) -> number");
    }

    #[test]
    #[allow(clippy::assertions_on_constants)]
    fn arg_ordering_invariants_hold_for_dsl_output() {
        // required-before-optional and variadic-last are table-wide
        // invariants; the real tables are checked by a test in
        // builtins.rs. Here: the DSL preserves declaration order.
        assert_eq!(T_SUBSTR.args[0].name, "s");
        assert!(!T_SUBSTR.args[2].required);
        assert!(T_FMT.args[1].variadic);
    }
}
