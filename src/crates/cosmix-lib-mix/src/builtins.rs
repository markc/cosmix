use crate::builtin_table;
use crate::contract;
use crate::error::{MixError, MixResult};
use crate::numeric::{
    InputPolicy, as_count, as_duration, as_exact_integer, as_exit_code, as_finite_number,
    as_loop_step, extract_number,
};
// The timestamp domain exists only where the date/time builtins do.
#[cfg(feature = "datetime")]
use crate::numeric::as_timestamp;
use crate::value::Value;
use std::cell::RefCell;
use std::rc::Rc;
use unicode_segmentation::UnicodeSegmentation;
use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};

/// Magnitude of a negative signed index, saturated to `usize`. The one
/// safe spelling: `(-idx)` overflows at `i64::MIN` (debug panic, release
/// wraparound — reachable from script via any bound that saturates the
/// f64→i64 cast, e.g. `slice($l, -1e25)`), and `unsigned_abs() as usize`
/// truncates on a 32-bit target. Compare in u64, cast only what fits;
/// a saturated `usize::MAX` correctly reads as "past the start" at every
/// caller (`<= len` fails; `saturating_sub` floors at 0).
pub(crate) fn neg_index_magnitude(idx: i64) -> usize {
    let m = idx.unsigned_abs(); // u64 — exact even at i64::MIN
    if m > usize::MAX as u64 {
        usize::MAX
    } else {
        m as usize
    }
}

/// Resolve a signed Mix index to a Rust `usize` against a container
/// of length `len`. Negative indices count from the end (`-1` is the
/// last element). Returns `None` when the resolved position is out
/// of bounds — callers map that to `Value::Nil`, matching Mix's
/// "silent nil on OOB" rule from `Expr::Index`.
///
/// Shared by `Expr::Index` (list + string), `slice`, `take`, and
/// `drop` so negative-index semantics stay consistent across every
/// entry point. Keep this free-standing (not a method on `Value`)
/// so it can be called from evaluator code where the container
/// length is already known without reborrowing the value.
pub fn resolve_signed_index(idx: i64, len: usize) -> Option<usize> {
    if idx >= 0 {
        let u = idx as usize;
        if u < len { Some(u) } else { None }
    } else {
        let neg = neg_index_magnitude(idx);
        if neg <= len { Some(len - neg) } else { None }
    }
}

/// Clamp a signed Mix index to a slice boundary — unlike
/// `resolve_signed_index`, this saturates to `0` / `len` instead of
/// returning `None`, because `slice(xs, -100, 100)` on a 5-element
/// list should return the whole list, not nil. Used by `slice`,
/// `take`, and `drop`.
pub fn clamp_signed_index(idx: i64, len: usize) -> usize {
    if idx >= 0 {
        (idx as usize).min(len)
    } else {
        len.saturating_sub(neg_index_magnitude(idx))
    }
}

// Single source of truth for every pure builtin. The `builtin_table!`
// macro (from `crate::builtin_info`) emits BOTH a `BUILTINS: &[BuiltinInfo]`
// array (for `mix help` / `mix builtins` / `mix what`) AND a `BUILTIN_NAMES:
// &[&str]` array (for REPL completion and the "never used" stats audit).
// Adding a new builtin means editing this one list — not three.
//
// Order roughly mirrors the dispatch order in `call_builtin` below.
// Categories line up with `meta::cmd_help_full` display buckets:
// string, type, list, map, io, system, format, json, hof (hof lives
// in builtins_hof.rs because HOFs use a different dispatch path).
builtin_table! {
    BUILTINS, BUILTIN_NAMES,
    ("length", CapabilityClass::Pure,          "string",  "Length of a string (codepoints), list/map (elements), or bytes/buffer (bytes, v0.64.0)", contract!((v: any_of(string, list, map, bytes, buffer)) -> number)),
    ("len", CapabilityClass::Pure,             "string",  "Alias for length()", contract!((v: any_of(string, list, map, bytes, buffer)) -> number)),
    ("upper", CapabilityClass::Pure,           "string",  "Convert string to uppercase", contract!((s: string) -> string)),
    ("lower", CapabilityClass::Pure,           "string",  "Convert string to lowercase", contract!((s: string) -> string)),
    ("left", CapabilityClass::Pure,            "string",  "Return leftmost N characters", contract!((s: string, n: number) -> string; failure[raises])),
    ("right", CapabilityClass::Pure,           "string",  "Return rightmost N characters", contract!((s: string, n: number) -> string; failure[raises])),
    ("substr", CapabilityClass::Pure,          "string",  "Extract substring by codepoint position and length (splits emoji/combining; see grapheme_substr)", contract!((s: string, start: number, len?: number) -> string; failure[raises])),
    ("pos", CapabilityClass::Pure,             "string",  "Find first position of needle in haystack (1-based, 0=not found — so `if pos(..)` reads correctly, since 0 is falsy). 0-based twin: index_of(), which takes its args the other way round and is NOT safe in a condition", contract!((needle: string, haystack: string) -> number)),
    ("lastpos", CapabilityClass::Pure,         "string",  "Find last position of needle in haystack (1-based, 0=not found — safe in a condition, 0 is falsy). 0-based twin: last_index_of(), which takes its args the other way round", contract!((needle: string, haystack: string) -> number)),
    ("strip", CapabilityClass::Pure,           "string",  "Remove leading/trailing whitespace, or codepoints in charset: strip(s[, charset]) (0.63.0 — the 2nd arg was silently IGNORED before)", contract!((s: string, charset?: string) -> string)),
    ("trim", CapabilityClass::Pure,            "string",  "Alias for strip(): trim(s[, charset]) — charset is a SET of codepoints to strip from both ends (0.63.0; was silently ignored). One-sided: ltrim/rtrim", contract!((s: string, charset?: string) -> string)),
    ("replace", CapabilityClass::Pure,         "string",  "Replace all occurrences of old with new in string", contract!((s: string, old: string, new: string) -> string)),
    ("split", CapabilityClass::Pure,           "string",  "Split string into list by delimiter (default: space)", contract!((s: string, delim?: string) -> list(string))),
    ("join", CapabilityClass::Pure,            "string",  "Join list into string with delimiter (default: space)", contract!((list: list, delim?: string) -> string)),
    ("starts_with", CapabilityClass::Pure,     "string",  "Test if string starts with prefix", contract!((s: string, prefix: string) -> bool)),
    ("ends_with", CapabilityClass::Pure,       "string",  "Test if string ends with suffix", contract!((s: string, suffix: string) -> bool)),
    ("contains", CapabilityClass::Pure,        "string",  "Test if string/list contains a value — the correct yes/no test, and what to use instead of a bare index_of() in a condition", contract!((s_or_list: any_of(string, list), v: any) -> bool)),
    ("repeat", CapabilityClass::Pure,          "string",  "Repeat string N times", contract!((s: string, n: number) -> string; failure[raises])),
    ("lpad", CapabilityClass::Pure,            "string",  "Left-pad string to width (codepoint count; see lpad_w for display cells). Optional 3rd arg is the fill character, default space: lpad(s, 12, \"0\") (v0.54.0)", contract!((s: string, width: number, fill?: string) -> string; failure[raises])),
    ("rpad", CapabilityClass::Pure,            "string",  "Right-pad string to width (codepoint count; see rpad_w for display cells). Optional 3rd arg is the fill character, default space (v0.54.0)", contract!((s: string, width: number, fill?: string) -> string; failure[raises])),
    ("lpad_w", CapabilityClass::Pure,          "string",  "Left-pad to width in terminal display CELLS (UAX #11; CJK/emoji=2) — aligns wide-char columns. Optional 3rd arg is the fill character (must be 1 cell wide), default space (v0.54.0)", contract!((s: string, width: number, fill?: string) -> string; failure[raises])),
    ("rpad_w", CapabilityClass::Pure,          "string",  "Right-pad to width in terminal display CELLS (UAX #11; CJK/emoji=2) — aligns wide-char columns. Optional 3rd arg is the fill character (must be 1 cell wide), default space (v0.54.0)", contract!((s: string, width: number, fill?: string) -> string; failure[raises])),
    ("reverse", CapabilityClass::Pure,         "string",  "Reverse a string (by codepoint; splits emoji — see grapheme_reverse) or list", contract!((v: any_of(string, list)) -> any_of(string, list))),
    ("words", CapabilityClass::Pure,           "string",  "Count whitespace-delimited words in string", contract!((s: string) -> number)),
    ("word", CapabilityClass::Pure,            "string",  "Extract Nth word from string (1-based)", contract!((s: string, n: number) -> any_of(string, nil); failure[raises])),
    ("grep", CapabilityClass::Pure,            "string",  "Return lines from text matching pattern (regex when enabled)", contract!((pattern: string, text: string) -> list(string))),
    // --- Subject-first string helpers (0.63.0). Tier 1 (delimiter family):
    // absent delimiter/marker -> nil, "" is a REAL result (delimiter at the
    // edge), empty delimiter raises — nil and "" never blur. Tier 2
    // (prefix/suffix/replace forms): "nothing to strip/replace" returns the
    // subject UNCHANGED, never nil. Indices are 0-based codepoints.
    ("before", CapabilityClass::Pure,          "string",  "Text before the FIRST delim: before(s, delim) -> string | nil (nil when delim absent; \"\" is a real result — delim at the start). Empty delim raises", contract!((s: string, delim: string) -> any_of(string, nil); failure[raises])),
    ("after", CapabilityClass::Pure,           "string",  "Text after the FIRST delim: after(s, delim) -> string | nil (nil when delim absent; \"\" when delim at the end). Empty delim raises. Want a default? `after($s, \"=\") or \"\"`", contract!((s: string, delim: string) -> any_of(string, nil); failure[raises])),
    ("before_last", CapabilityClass::Pure,     "string",  "Text before the LAST delim -> string | nil (nil when absent). Empty delim raises", contract!((s: string, delim: string) -> any_of(string, nil); failure[raises])),
    ("after_last", CapabilityClass::Pure,      "string",  "Text after the LAST delim -> string | nil (nil when absent) — basename/extension: after_last(path, \"/\"), after_last(name, \".\"). Empty delim raises", contract!((s: string, delim: string) -> any_of(string, nil); failure[raises])),
    ("split_once", CapabilityClass::Pure,      "string",  "Split at the FIRST delim: split_once(s, delim) -> [head, tail] | nil (nil when absent — never a 1-element list). Empty delim raises", contract!((s: string, delim: string) -> any_of(list, nil); failure[raises])),
    ("rsplit_once", CapabilityClass::Pure,     "string",  "Split at the LAST delim -> [head, tail] | nil (nil when absent). Empty delim raises", contract!((s: string, delim: string) -> any_of(list, nil); failure[raises])),
    ("between", CapabilityClass::Pure,         "string",  "Text after the first a and before the NEXT b: between(s, a, b) -> string | nil (nil if either marker absent, in that order). Empty a or b raises", contract!((s: string, a: string, b: string) -> any_of(string, nil); failure[raises])),
    ("strip_prefix", CapabilityClass::Pure,    "string",  "s without a leading p, else s UNCHANGED (never nil — \"nothing to strip\" is an answer). Empty p -> unchanged. Kills the starts_with+substr idiom", contract!((s: string, p: string) -> string)),
    ("strip_suffix", CapabilityClass::Pure,    "string",  "s without a trailing x, else s UNCHANGED (never nil). Empty x -> unchanged", contract!((s: string, x: string) -> string)),
    ("replace_first", CapabilityClass::Pure,   "string",  "Replace the FIRST occurrence of old; old absent -> s unchanged. Empty old mirrors replace(): inserts new at the start (replace_first(\"ab\", \"\", \"X\") is \"Xab\")", contract!((s: string, old: string, new: string) -> string)),
    ("count_of", CapabilityClass::Pure,        "string",  "Non-overlapping occurrences of needle in s; 0 for an empty needle. (The HOF count(list, pred) is a different builtin)", contract!((s: string, needle: string) -> number)),
    ("ltrim", CapabilityClass::Pure,           "string",  "Strip leading whitespace, or leading codepoints in charset: ltrim(s[, charset]) — PHP-style charset is a SET of codepoints, not a prefix string", contract!((s: string, charset?: string) -> string)),
    ("rtrim", CapabilityClass::Pure,           "string",  "Strip trailing whitespace, or trailing codepoints in charset: rtrim(s[, charset])", contract!((s: string, charset?: string) -> string)),
    ("lines", CapabilityClass::Pure,           "string",  "Split into lines: \\n-separated, ONE trailing \\r stripped per line (CRLF and LF both work; a lone \\r is not a terminator), exactly one trailing empty element dropped (the final newline). lines(\"\") -> []; \"a\\n\\n\" -> [\"a\", \"\"]. Native since 0.63.0 (was a prelude fn that kept \\r and the trailing \"\")", contract!((s: string) -> list(string))),
    ("fields", CapabilityClass::Pure,          "string",  "awk-style fields: split on whitespace RUNS, no empties; fields(\"\") -> []. 0-based access fields(s)[2]; the 1-based single-field form is word(s, n)", contract!((s: string) -> list(string))),
    ("chars", CapabilityClass::Pure,           "string",  "Codepoints as 1-char strings (grapheme_* builtins exist for clusters). Native since 0.63.0 (was a prelude fn)", contract!((s: string) -> list(string))),
    ("last_index_of", CapabilityClass::Pure,   "string",  "0-based codepoint index of the LAST occurrence in a string, or last index of a value in a list; -1 if absent. The 0-based twin of lastpos() (args reversed). List search compares with == — SCALAR elements only, exactly like index_of (a map/list element never matches; deep_eq is the structural comparison). ⚠ Like index_of, NEVER bare in a condition: -1 is truthy, 0 is falsy", contract!((seq: any_of(list, string), v: any) -> number)),
    ("deep_eq", CapabilityClass::Pure,         "type",    "Structural equality for any two values: maps compare by key set + deep_eq values (insertion order IGNORED), lists elementwise in order, scalars as ==. The answer `==` cannot give for maps/lists (it is always false there today). Caveats, inherited from ==: a FUNCTION value is never equal (even to itself — a callback-bearing map is not deep_eq its own copy) and Buffer-vs-Bytes is false (freeze() first). Raises past 512 nesting levels", contract!((a: any, b: any) -> bool; failure[raises])),
    ("template", CapabilityClass::Pure,        "string",  "Substitute single-brace {key} placeholders in a string from a map", contract!((tmpl: string, vars: map) -> string)),
    ("word_wrap", CapabilityClass::Pure,       "string",  "Wrap text to a column width (codepoint budget; see word_wrap_w for display cells)", contract!((text: string, width: number) -> string; failure[raises])),
    ("word_wrap_w", CapabilityClass::Pure,     "string",  "Wrap text to a column width in terminal display CELLS (UAX #11; CJK/emoji=2)", contract!((text: string, width: number) -> string; failure[raises])),
    ("markdown_escape", CapabilityClass::Pure, "string",  "Escape markdown metacharacters in a string", contract!((s: string) -> string)),
    ("markdown", CapabilityClass::Pure,        "string",  "Render CommonMark + GFM markdown (tables, strikethrough, task lists, footnotes) to HTML; raw HTML is escaped and unsafe URL schemes neutralised (requires markdown feature)", contract!((s: string) -> string)),
    ("html_escape", CapabilityClass::Pure,     "string",  "Escape & < > \" ' for HTML element text + quoted attribute values (not JS/CSS/URL/srcdoc contexts)", contract!((s: string) -> string)),
    ("sanitize", CapabilityClass::Pure,        "string",  "Make untrusted bytes safe for one-line diagnostics: collapse line breaks (incl. U+2028/9) to spaces, replace C0/C1 controls and Trojan-Source bidi/zero-width chars with '?'", contract!((s: string) -> string)),
    ("regex_match", CapabilityClass::Pure,     "string",  "Test if pattern matches string (requires regex feature)", contract!((pattern: string, s: string) -> bool)),
    ("regex_find", CapabilityClass::Pure,      "string",  "Return ALL regex matches as a list of {match, start, end[, groups]} maps (empty list if none)", contract!((pattern: string, text: string) -> list(map))),
    ("regex_replace", CapabilityClass::Pure,   "string",  "Replace regex matches with replacement text", contract!((pattern: string, text: string, replacement: string) -> string)),
    ("regex_split", CapabilityClass::Pure,     "string",  "Split string by regex pattern", contract!((pattern: string, s: string) -> list(string))),
    // --- Subject-first regex family (0.63.0): same engine as regex_*, args
    // the consistent way round (subject first, like every literal-string
    // builtin). The legacy pattern-first names stay until release B.
    ("re_match", CapabilityClass::Pure,        "string",  "Subject-first regex test: re_match(s, pattern) -> bool, true if pattern matches anywhere in s", contract!((s: string, pattern: string) -> bool; failure[raises])),
    ("re_find", CapabilityClass::Pure,         "string",  "All matches as {match, start, end[, groups]} maps with CODEPOINT offsets (compose with substr/slice/index_of; legacy regex_find returns UTF-8 BYTE offsets). [] when none", contract!((s: string, pattern: string) -> list(map); failure[raises])),
    ("re_replace", CapabilityClass::Pure,      "string",  "Replace ALL matches: re_replace(s, pattern, replacement) — subject FIRST; $1/${name} backrefs in replacement", contract!((s: string, pattern: string, replacement: string) -> string; failure[raises])),
    ("re_split", CapabilityClass::Pure,        "string",  "Split s on each match of pattern (subject first)", contract!((s: string, pattern: string) -> list(string); failure[raises])),
    ("grep_lines", CapabilityClass::Pure,      "string",  "Lines of text matching pattern (subject first; regex when enabled, else substring) — grep() with the args the consistent way round", contract!((text: string, pattern: string) -> list(string); failure[raises])),
    ("csv_parse", CapabilityClass::Pure,       "string",  "Parse CSV string into a list of header-keyed row maps", contract!((s: string, delim?: string) -> list(map))),
    ("ini_parse", CapabilityClass::Pure,       "string",  "Parse INI string into nested map of sections", contract!((s: string) -> map)),
    ("xml_parse", CapabilityClass::Pure,       "string",  "Parse a strict-XML string (or bytes, e.g. an HTTP body) into a Value tree (requires xml feature). Default simple mode is the SOAP/RSS consumer shape: {RootName: …} with namespace prefixes stripped, attributes as @name keys, repeated sibling elements collapsed to a list, a leaf element's text as its value, mixed text under #text, xmlns declarations dropped. Pass {mode:\"tree\"} for full fidelity: nodes are {name, attrs, children} with prefixes + xmlns preserved and text children as plain strings. Strict XML only — real-world HTML is tag soup and will NOT parse.", contract!((s: any_of(string, bytes), opts?: map) -> map)),
    ("url_parse", CapabilityClass::Pure,       "string",  "Parse URL into {scheme, host, port, path, query, fragment}", contract!((url: string) -> map("url_parts", {scheme: string, host: string, port: any, path: string, query: string, fragment: string}))),
    ("url_decode", CapabilityClass::Pure,      "string",  "Percent-decode a URL/form-encoded string ('+' → space)", contract!((s: string) -> string)),
    ("url_encode", CapabilityClass::Pure,      "string",  "Percent-encode a string for use in a URL/form body", contract!((s: string) -> string)),
    ("parse_query", CapabilityClass::Pure,     "string",  "Parse a k=v&k2=v2 query/form string into a map (url-decoded, last-wins)", contract!((s: string) -> map)),
    ("parse_form", CapabilityClass::Pure,      "string",  "Parse an x-www-form-urlencoded body into a map (alias of parse_query)", contract!((s: string) -> map)),

    // Char-aware string ops (_doc/planned/2026-06-02-mix-char-aware-strings.md).
    // byte_* preserve the raw UTF-8 byte offsets/sizes that length/pos/lastpos/
    // index_of returned pre-0.8.0 (the escape hatch now that those are codepoint-
    // based); grapheme_* count/slice user-perceived characters (emoji + combining
    // marks); display_width = terminal cells.
    ("byte_length", CapabilityClass::Pure,     "string",  "Length of a string in raw UTF-8 bytes (the pre-0.8.0 length() value for strings)", contract!((s: string) -> number)),
    ("byte_pos", CapabilityClass::Pure,        "string",  "Byte offset of needle in haystack (1-based, 0=not found) — byte twin of pos(), 0-based twin byte_index_of(). Safe in a condition (0 is falsy)", contract!((needle: string, haystack: string) -> number)),
    ("byte_lastpos", CapabilityClass::Pure,    "string",  "Last byte offset of needle in haystack (1-based, 0=not found) — byte twin of lastpos()", contract!((needle: string, haystack: string) -> number)),
    ("byte_index_of", CapabilityClass::Pure,   "string",  "Byte offset of needle in string (0-based, -1=not found) — byte twin of string index_of(), 1-based twin byte_pos(). ⚠ NEVER use bare in a condition: -1 is truthy, 0 is falsy (MIX-W2305)", contract!((haystack: string, needle: string) -> number)),
    ("grapheme_count", CapabilityClass::Pure,   "string", "Count grapheme clusters (user-perceived chars: emoji/flags/combining count as 1)", contract!((s: string) -> number)),
    ("grapheme_substr", CapabilityClass::Pure,  "string", "Substring by grapheme cluster position and length (won't split emoji/combining marks)", contract!((s: string, start: number, len?: number) -> string; failure[raises])),
    ("grapheme_reverse", CapabilityClass::Pure, "string", "Reverse a string by grapheme cluster (emoji/combining-safe, unlike reverse())", contract!((s: string) -> string)),
    ("display_width", CapabilityClass::Pure,    "string", "Terminal display width in cells (UAX #11; CJK/emoji=2, combining=0, East-Asian-ambiguous=1)", contract!((s: string) -> number)),

    ("type", CapabilityClass::Pure,            "type",    "Return type name: string, number, bool, list, map, nil", contract!((v: any) -> string)),
    ("to_number", CapabilityClass::Pure,       "type",    "Convert value to number (nil if not numeric)", contract!((v: any) -> any_of(number, nil))),
    ("to_string", CapabilityClass::Pure,       "type",    "Convert value to its string representation", contract!((v: any) -> string)),
    ("is_number", CapabilityClass::Pure,       "type",    "Test if value is numeric or a numeric string", contract!((v: any) -> bool)),
    ("is_empty", CapabilityClass::Pure,        "type",    "Test if string/list/map is empty, or value is nil (\"0\" is NOT empty)", contract!((v: any) -> bool)),

    // Math (v0.19.0). Pure f64 numerics — coerce numeric strings/bools like
    // the rest of Mix, raise on a non-numeric arg, and propagate IEEE-754
    // NaN/inf (sqrt(-1)→NaN, ln(0)→-inf) rather than erroring. The rounding
    // family takes an optional 2nd arg = decimal places (negative = tens/
    // hundreds). min/max/abs/clamp replace the old prelude shims.
    ("round", CapabilityClass::Pure,           "math",    "Round to nearest integer, half away from zero; round(x, n) to n decimal places (n<0 rounds to tens/hundreds) (v0.19.0)", contract!((x: number, n?: number) -> number)),
    ("floor", CapabilityClass::Pure,           "math",    "Round down toward -inf; floor(x, n) to n decimal places (v0.19.0)", contract!((x: number, n?: number) -> number)),
    ("ceil", CapabilityClass::Pure,            "math",    "Round up toward +inf; ceil(x, n) to n decimal places (v0.19.0)", contract!((x: number, n?: number) -> number)),
    ("trunc", CapabilityClass::Pure,           "math",    "Truncate toward zero (drop the fraction); trunc(x, n) to n decimal places (v0.19.0)", contract!((x: number, n?: number) -> number)),
    ("abs", CapabilityClass::Pure,             "math",    "Absolute value (v0.19.0)", contract!((x: number) -> number)),
    ("sign", CapabilityClass::Pure,            "math",    "Sign of x: -1, 0, or 1 (±0→0, NaN→NaN) (v0.19.0)", contract!((x: number) -> number)),
    ("band", CapabilityClass::Pure,            "math",    "Bitwise AND of two integers. Both arguments must be exact integers within ±2^53 (the range f64 represents without loss); a fraction, an infinity, a NaN or an out-of-range magnitude raises rather than silently truncating. Chiefly for permission bits: band(stat(p)[\"perm\"], 0o111) != 0 asks whether any execute bit is set (v0.46.0)", contract!((a: number, b: number) -> number; failure[raises])),
    ("bor", CapabilityClass::Pure,             "math",    "Bitwise OR of two integers; same exact-integer domain as band (v0.46.0)", contract!((a: number, b: number) -> number; failure[raises])),
    ("bxor", CapabilityClass::Pure,            "math",    "Bitwise exclusive-OR of two integers; same exact-integer domain as band (v0.46.0)", contract!((a: number, b: number) -> number; failure[raises])),
    ("bnot", CapabilityClass::Pure,            "math",    "Bitwise NOT (one's complement) over 64-bit two's complement: bnot(0) is -1. Same exact-integer domain as band (v0.46.0)", contract!((x: number) -> number; failure[raises])),
    ("bshl", CapabilityClass::Pure,            "math",    "Shift left by n bits (n in 0..63). Raises rather than wrapping if the result leaves the exact-integer range (v0.46.0)", contract!((x: number, n: number) -> number; failure[raises])),
    ("bshr", CapabilityClass::Pure,            "math",    "Arithmetic shift right by n bits (n in 0..63); the sign bit is replicated, so bshr(-8, 1) is -4 (v0.46.0)", contract!((x: number, n: number) -> number; failure[raises])),
    ("sqrt", CapabilityClass::Pure,            "math",    "Square root (negative→NaN) (v0.19.0)", contract!((x: number) -> number)),
    ("cbrt", CapabilityClass::Pure,            "math",    "Cube root (defined for negatives) (v0.19.0)", contract!((x: number) -> number)),
    ("pow", CapabilityClass::Pure,             "math",    "Raise to a power: pow(base, exp) = base^exp (v0.19.0)", contract!((base: number, exp: number) -> number)),
    ("exp", CapabilityClass::Pure,             "math",    "e raised to the x (v0.19.0)", contract!((x: number) -> number)),
    ("ln", CapabilityClass::Pure,              "math",    "Natural logarithm, base e (ln(0)→-inf, ln(neg)→NaN) (v0.19.0)", contract!((x: number) -> number)),
    ("log10", CapabilityClass::Pure,           "math",    "Base-10 logarithm (v0.19.0)", contract!((x: number) -> number)),
    ("log2", CapabilityClass::Pure,            "math",    "Base-2 logarithm (v0.19.0)", contract!((x: number) -> number)),
    ("log", CapabilityClass::Pure,             "math",    "Logarithm in an arbitrary base: log(x, base) (v0.19.0)", contract!((x: number, base: number) -> number)),
    ("min", CapabilityClass::Pure,             "math",    "Smallest of the number arguments, or of a single list argument: min(a, b, …) | min(list). Lexicographic when all args are strings; NaN-skipping (v0.19.0)", contract!((a: any, rest: ...any) -> any_of(number, string))),
    ("max", CapabilityClass::Pure,             "math",    "Largest of the number arguments, or of a single list argument: max(a, b, …) | max(list). Lexicographic when all args are strings; NaN-skipping (v0.19.0)", contract!((a: any, rest: ...any) -> any_of(number, string))),
    ("clamp", CapabilityClass::Pure,           "math",    "Constrain a number to a range: clamp(x, lo, hi) (errors if lo > hi) (v0.19.0)", contract!((x: number, lo: number, hi: number) -> number)),
    ("hypot", CapabilityClass::Pure,           "math",    "Euclidean distance sqrt(x²+y²) computed without intermediate overflow: hypot(x, y) (v0.19.0)", contract!((x: number, y: number) -> number)),
    ("sin", CapabilityClass::Pure,             "math",    "Sine of x in radians (v0.19.0)", contract!((x: number) -> number)),
    ("cos", CapabilityClass::Pure,             "math",    "Cosine of x in radians (v0.19.0)", contract!((x: number) -> number)),
    ("tan", CapabilityClass::Pure,             "math",    "Tangent of x in radians (v0.19.0)", contract!((x: number) -> number)),
    ("asin", CapabilityClass::Pure,            "math",    "Arcsine in radians (domain [-1, 1], else NaN) (v0.19.0)", contract!((x: number) -> number)),
    ("acos", CapabilityClass::Pure,            "math",    "Arccosine in radians (domain [-1, 1], else NaN) (v0.19.0)", contract!((x: number) -> number)),
    ("atan", CapabilityClass::Pure,            "math",    "Arctangent in radians (v0.19.0)", contract!((x: number) -> number)),
    ("atan2", CapabilityClass::Pure,           "math",    "Angle in radians of the point (x, y): atan2(y, x) (v0.19.0)", contract!((y: number, x: number) -> number)),
    ("pi", CapabilityClass::Pure,              "math",    "The constant π (v0.19.0)", contract!(() -> number)),
    ("e", CapabilityClass::Pure,               "math",    "Euler's number e (v0.19.0)", contract!(() -> number)),
    ("random", CapabilityClass::Pure,          "math",    "random() -> float in [0,1); random(min, max) -> integer in [min, max] inclusive (v0.23.0)", contract!((min?: number, max?: number) -> number; arities[0, 2])),

    ("push", CapabilityClass::Pure,            "list",    "Append value to end of list (mutates)", contract!((list: list, v: any) -> nil; effects[mutates_args])),
    ("pop", CapabilityClass::Pure,             "list",    "Remove and return last element of list (mutates)", contract!((list: list) -> any; effects[mutates_args])),
    ("shift", CapabilityClass::Pure,           "list",    "Remove and return first element of list (mutates)", contract!((list: list) -> any; effects[mutates_args])),
    ("sort", CapabilityClass::Pure,            "list",    "Return sorted copy of list (all-number lists sort numerically; else lexicographic)", contract!((list: list) -> list)),
    ("index_of", CapabilityClass::Pure,        "list",    "Find 0-based index of value in a list, or of a needle substring in a string (codepoint-based); -1 if absent. 1-based twin: pos() (args reversed). ⚠ NEVER use bare in a condition — -1 (absent) is TRUTHY and 0 (first position) is FALSY, so both answers invert; use contains() for yes/no or compare >= 0 (MIX-W2305)", contract!((seq: any_of(list, string), v: any) -> number)),
    ("unique", CapabilityClass::Pure,          "list",    "Return list with duplicates removed", contract!((list: list) -> list)),
    ("range", CapabilityClass::Pure,           "list",    "Generate list of numbers from start to end with optional step. Bounds/step must be whole numbers within i64 — fractional or oversized values raise VALUE_OUT_OF_RANGE instead of silently saturating (strict since v0.59.0)", contract!((start: number, end: number, step?: number) -> list(number); failure[raises])),
    ("flat", CapabilityClass::Pure,            "list",    "Flatten nested lists into a single list", contract!((list: list) -> list)),
    ("concat", CapabilityClass::Pure,          "list",    "Concatenate 2+ lists into one new list (one level; each arg must be a list)", contract!((a: list, b: list, rest: ...list) -> list)),
    ("slice", CapabilityClass::Pure,           "list",    "Sub-sequence [start, end): negative indices and out-of-range clamp, a reversed range is empty (v0.2.0). Slices a list (elements), string (codepoints), or bytes/buffer (bytes → a new value-semantic bytes, v0.64.0)", contract!((seq: any_of(list, string, bytes, buffer), start: number, end?: any_of(number, nil)) -> any_of(list, string, bytes))),
    ("take", CapabilityClass::Pure,            "list",    "First N items of a list (negative N = last N) (v0.2.0)", contract!((seq: any_of(list, string), n: number) -> any_of(list, string))),
    ("drop", CapabilityClass::Pure,            "list",    "Skip first N items of a list (negative N = drop last N) (v0.2.0)", contract!((seq: any_of(list, string), n: number) -> any_of(list, string))),
    ("zip", CapabilityClass::Pure,             "list",    "Pair two lists element-wise into [a, b] tuples (v0.2.0)", contract!((a: list, b: list) -> list(list))),

    ("keys", CapabilityClass::Pure,            "map",     "Return list of map keys", contract!((map: map) -> list(string))),
    ("values", CapabilityClass::Pure,          "map",     "Return list of map values", contract!((map: map) -> list)),
    ("has_key", CapabilityClass::Pure,         "map",     "Test if map contains a key", contract!((map: map, k: string) -> bool)),
    ("merge", CapabilityClass::Pure,           "map",     "Merge two maps (second wins on conflicts)", contract!((a: map, b: map) -> map)),
    ("delete", CapabilityClass::Pure,          "map",     "Return map with key removed", contract!((map: map, k: string) -> map)),

    ("read_file", CapabilityClass::FsRead,       "io",      "Read entire file contents as string", contract!((path: string) -> string; failure[raises])),
    ("read_file_bytes", CapabilityClass::FsRead, "io",      "Read file contents as raw bytes. Optional 2nd arg caps the read: read_file_bytes(path, 8192) reads at most 8192 bytes (header-sniffing without slurping a huge file) (v0.3.1; cap v0.17.1)", contract!((path: string, max?: number) -> bytes; failure[raises])),
    ("read_lines", CapabilityClass::FsRead,      "io",      "Read file as a list of lines (trailing newline stripped, empty last line dropped) (v0.2.3)", contract!((path: string) -> list(string); failure[raises])),
    ("load_data", CapabilityClass::FsRead,       "io",      "Read + parse a strict-data .mix file (bare-key `k: v`, the zones.mix/conf.mix form) into a Value — the non-executing twin of source/include, for substrate-internal data that must NOT run as code (v0.9.0)", contract!((path: string) -> any; failure[raises])),
    ("write_file", CapabilityClass::FsWrite,      "io",      "Write string or bytes to file (creates/overwrites). Bytes are written verbatim (v0.3.1).", contract!((path: string, data: any) -> nil; failure[raises])),
    ("write_new", CapabilityClass::FsWrite,       "io",      "Atomically create a new file with mode. write_new(path, content, 0o600) — mode as a value (octal literal) or octal string \"0600\"; fails if path exists; mode applied at creation (no umask race)", contract!((path: string, content: any, mode: any_of(number, string)) -> nil; failure[raises])),
    ("append_file", CapabilityClass::FsWrite,     "io",      "Append string to file", contract!((path: string, s: any) -> nil; failure[raises])),
    ("exists", CapabilityClass::FsRead,          "io",      "Test if path exists. FOLLOWS symlinks by default, so a dangling link reads as absent — that is the right answer for \"can I open something here\" and the wrong one for \"is this name taken\". exists(path, {follow_symlinks: false}) is the lstat form and sees the link itself (v0.39.0).", contract!((path: string, opts?: map) -> bool)),
    ("access", CapabilityClass::FsRead,          "io",      "Ask the kernel whether this process can access path using its effective uid/gid: mode is a non-empty, duplicate-free string of r/w/x/f letters (f = existence and is redundant when combined). Follows symlinks. Unlike inspecting stat().perm, this honours POSIX ACLs. Ordinary absence/denial returns false; malformed input or an unexpected syscall failure raises (v0.45.0).", contract!((path: string, mode: string) -> bool; failure[raises])),
    ("is_dir", CapabilityClass::FsRead,          "io",      "Test if path is a directory", contract!((path: string) -> bool)),
    ("is_file", CapabilityClass::FsRead,         "io",      "Test if path is a regular file", contract!((path: string) -> bool)),
    ("realpath", CapabilityClass::FsRead,        "io",      "Canonicalise a path: resolve every symlink + `.`/`..` to the absolute real path (like `readlink -f` / realpath(3)). The path MUST exist. realpath(path) -> string | nil (nil when it can't be resolved — a missing component, a symlink loop, or a non-UTF-8 resolved path). NORMALISATION ONLY, not a race-free authorization primitive: canonicalise-then-use is not atomic, so for an exec/open safety check, exec/open the RETURNED canonical path (which has no symlinks to re-traverse), not the original (v0.31.2)", contract!((path: string) -> any_of(string, nil))),
    ("glob", CapabilityClass::FsRead,            "io",      "List files matching a glob pattern (supports ** globstar in v0.2.1)", contract!((pattern: string) -> list(string); failure[raises])),
    ("ls", CapabilityClass::FsRead,              "io",      "List directory entries", contract!((path?: string) -> list(string); failure[raises])),
    ("mkdir", CapabilityClass::FsWrite,           "io",      "Create directory: mkdir(path[, {parents}]). parents defaults to true (create_dir_all). {parents: false} creates only the final component and fails if the parent is missing — the form to use when the parent was placed deliberately and re-creating it would hide its removal (v0.42.0)", contract!((path: string, opts?: map) -> nil; failure[raises])),
    ("flock", CapabilityClass::FsWrite,           "io",      "Take a process-held advisory file lock: flock(path[, {shared, wait}]) -> bool. Exclusive and non-blocking by default; contention returns false, genuine filesystem errors raise. wait is seconds (0 = do not wait). Repeated acquisition of the same canonical path by this process is idempotent-true (v0.43.0)", contract!((path: string, opts?: map) -> bool; failure[raises])),
    ("funlock", CapabilityClass::FsWrite,         "io",      "Release and close this process's advisory lock for path. Returns true when held, false when not held (v0.43.0)", contract!((path: string) -> bool; failure[raises])),
    ("copy", CapabilityClass::FsWrite,            "io",      "Copy a single file: copy(src, dst). Overwrites dst; preserves the source permission bits. Use copy_tree for a directory (v0.22.0).", contract!((src: string, dst: string) -> nil; failure[raises])),
    ("copy_tree", CapabilityClass::FsWrite,       "io",      "Recursively copy a directory: copy_tree(src, dst). Creates dst, copies files (perms preserved) and symlinks (as symlinks); merges into an existing dst (v0.22.0).", contract!((src: string, dst: string) -> nil; failure[raises])),
    ("symlink", CapabilityClass::FsWrite,         "io",      "Create a symbolic link: symlink(target, linkpath) — symlink(2), arguments in symlink(2) order (target first, the link to create second). `target` is stored verbatim and is NOT resolved or validated: a relative target resolves against the link's own directory, and creating a dangling link is legal. Raises EEXIST if linkpath already exists. Read the other way with read_link() (v0.38.0).", contract!((target: string, linkpath: string) -> nil; failure[raises])),
    ("read_link", CapabilityClass::FsRead,         "io",      "Read a symbolic link's target: read_link(path) -> string — readlink(2), returning the target EXACTLY as stored (possibly relative, possibly dangling), which is what distinguishes it from realpath()'s full resolution. Raises EINVAL when path is not a symlink; test first with stat(path, {follow_symlinks: false}).is_symlink (v0.38.0).", contract!((path: string) -> string; failure[raises])),
    ("rename", CapabilityClass::FsWrite,          "io",      "Rename/move a path within one filesystem: rename(src, dst) — rename(2), so replacing an existing dst is ATOMIC (a concurrent reader sees either the old file or the new one, never a partial write). This is the primitive for a safe in-place update: write a temp file beside the target, then rename over it. Raises EXDEV across filesystems (copy + remove instead) and ENOENT when src is missing (v0.37.0).", contract!((src: string, dst: string) -> nil; failure[raises])),
    ("remove", CapabilityClass::FsWrite,          "io",      "Remove a single file/symlink: remove(path). No-op if already gone (rm -f). Errors if path is a directory — use remove_dir (v0.22.0).", contract!((path: string) -> nil; failure[raises])),
    ("remove_dir", CapabilityClass::FsWrite,      "io",      "Recursively remove a directory and its contents: remove_dir(path) (rm -rf). No-op if already gone (v0.22.0).", contract!((path: string) -> nil; failure[raises])),
    ("chmod", CapabilityClass::FsWrite,           "io",      "Set file/directory permissions. chmod(path, 0o755) — mode as a VALUE (use an octal literal) or an octal string \"0755\" (v0.11.0: a number is now the value, not its decimal digits read as octal)", contract!((path: string, mode: any_of(number, string)) -> nil; failure[raises])),
    ("chown", CapabilityClass::FsWrite,           "io",      "Set file owner/group by numeric uid/gid: chown(path, 1000, 1000). Follows symlinks. Numeric only (no name resolution) (v0.17.1)", contract!((path: string, uid: number, gid: number) -> nil; failure[raises])),
    ("stat", CapabilityClass::FsRead,            "io",      "Stat a path → map {uid, gid, nlink, size, mode, perm, ino, dev, ctime, mtime, atime, ctime_nsec, mtime_nsec, atime_nsec, is_file, is_dir, is_symlink}. ino/dev are STRINGS (u64, exceed f64 exact range); mode is full st_mode, perm = mode & 0o7777; *time are epoch seconds (f64) and *_nsec the sub-second part 0..=999999999 (v0.44.0) — compare the PAIR to see a same-second rewrite. Follows symlinks by default; stat(path, {follow_symlinks:false}) is lstat (v0.17.1)", contract!((path: string, opts?: map) -> map("stat", {uid: number, gid: number, nlink: number, size: number, mode: number, perm: number, ino: string, dev: string, ctime: number, mtime: number, atime: number, ctime_nsec: number, mtime_nsec: number, atime_nsec: number, is_file: bool, is_dir: bool, is_symlink: bool}); failure[raises])),
    ("line_count", CapabilityClass::FsRead,      "io",      "Count lines in a file by streaming — never loads the whole file (byte-oriented, so it works on non-UTF-8 files too) (streams since v0.28.1)", contract!((path: string) -> number; failure[raises])),
    ("head", CapabilityClass::FsRead,            "io",      "First N lines of a file as a list (default 10) — streams and stops after N lines, never reads the rest (the no-slurp twin of take(read_lines(p), n)) (v0.28.1)", contract!((path: string, n?: number) -> list(string); failure[raises])),
    ("tail", CapabilityClass::FsRead,            "io",      "Last N lines of a file as a list (default 10) — reads backwards in blocks from EOF, never slurps the whole file (the no-slurp twin of take(read_lines(p), -n)) (v0.28.1)", contract!((path: string, n?: number) -> list(string); failure[raises])),
    ("basename", CapabilityClass::Pure,        "io",      "Return the filename component of a path", contract!((path: string) -> string)),
    ("dirname", CapabilityClass::Pure,         "io",      "Return the directory component of a path", contract!((path: string) -> string)),
    ("extname", CapabilityClass::Pure,         "io",      "Return the file extension (including the leading dot)", contract!((path: string) -> string)),
    ("path_join", CapabilityClass::Pure,       "io",      "Join path components with the native separator", contract!((a: string, b: string) -> string)),
    ("path_parts", CapabilityClass::Pure,      "io",      "Decompose a path into {dir, base, stem, ext} (v0.2.1)", contract!((path: string) -> map("path_parts", {dir: string, base: string, stem: string, ext: string}))),
    ("walk", CapabilityClass::FsRead,            "io",      "Recursive directory walk: walk(dir, {max_depth, follow_symlinks, include_dirs}); invalid max_depth raises instead of becoming unlimited (strict since v0.55.0)", contract!((dir: string, opts?: map) -> list(string); failure[raises])),
    ("readline", CapabilityClass::Env,        "io",      "Read a line from stdin (optional prompt argument)", contract!((prompt?: string) -> string; effects[blocking])),
    ("read_stdin", CapabilityClass::Env,      "io",      "Read all of stdin to EOF as a string (for pipe/hook input). STRICT UTF-8 — binary stdin raises; use read_stdin_bytes for that", contract!(() -> string; effects[blocking])),
    ("read_stdin_bytes", CapabilityClass::Env, "io",     "Read all of stdin to EOF as raw bytes — the binary twin of read_stdin, which refuses invalid UTF-8. Optional arg caps the read: read_stdin_bytes(8192) reads at most 8192 bytes, the same cap contract as read_file_bytes (v0.65.0)", contract!((max?: number) -> bytes; effects[blocking]; failure[raises])),
    ("sqlopen", CapabilityClass::FsWrite,         "io",      "Open a SQLite database and return a handle", contract!((path: string, mode?: string) -> number; failure[raises])),
    ("sqlexec", CapabilityClass::FsWrite,         "io",      "Execute SQL on a SQLite handle, return result rows", contract!((handle: number, sql: string, params?: any) -> any_of(list, map); failure[raises])),
    ("sqlclose", CapabilityClass::FsWrite,        "io",      "Close a SQLite database handle", contract!((handle: number) -> nil; failure[raises])),
    ("db_query", CapabilityClass::Db,        "db",      "Query the host-injected scoped DB: db_query(sql, [params]) → rows", contract!((sql: string, params?: list) -> list; effects[blocking]; failure[raises])),
    ("db_exec", CapabilityClass::Db,         "db",      "Exec on the host-injected scoped DB: db_exec(sql, [params]) → {affected, last_insert_id}", contract!((sql: string, params?: list) -> map("db_exec_result", {affected: number, last_insert_id: number}); effects[blocking]; failure[raises])),
    ("jmap", CapabilityClass::Jmap,            "jmap",    "Call the host-injected JMAP upstream: jmap(method, args) → result, or jmap([[method,args,callId],…]) → methodResponses", contract!((method: any_of(string, list), args?: map) -> any_of(map, list); effects[blocking]; failure[raises])),
    ("jmap_upload", CapabilityClass::Jmap,     "jmap",    "Upload bytes as a JMAP blob via the host-injected upstream: jmap_upload(body[, content_type]) → blobId (the compose half of the mail seam; Email/set create is blob-only)", contract!((body: any_of(string, bytes, buffer), content_type?: string) -> string; effects[blocking]; failure[raises])),
    ("bus_call", CapabilityClass::Bus,         "bus",     "Call a host-injected Bus verb under delegated identity: bus_call(verb, args) → reply. The embedder bounds which verbs are reachable and injects the delegation envelope; the script names no host/peer/actor", contract!((verb: string, args?: map) -> any; effects[blocking]; failure[raises])),
    ("publish", CapabilityClass::Bus,          "bus",     "One-call topic publish (0.63.0): publish(topic, body[, opts]) builds the SPEC-02 wire frame and sends it via noded topic.publish — no hand-built ---\\n frames, no body=/name= header-route trap. body is the payload STRING (json_encode a map first); opts: {retain: bool, command: string (inner frame header override, defaults to topic), headers: map}. Sets $rc/$result like `send`; returns rc (0 = published)", contract!((topic: string, body?: any_of(string, nil), opts?: map) -> number; effects[blocking]; failure[raises])),

    ("env", CapabilityClass::Env,             "system",  "Get environment variable value (\"\" if unset); env(name, default) returns default when unset or empty", contract!((name: string, default?: any) -> any)),
    ("time", CapabilityClass::Pure,            "system",  "Return current Unix timestamp as float", contract!(() -> number)),
    ("pid", CapabilityClass::Env,             "system",  "Return current process ID", contract!(() -> number)),
    ("uid", CapabilityClass::Env,             "system",  "Effective user id of this process (geteuid) — normally the id a file access is checked against (Linux checks fsuid, which tracks euid unless setfsuid(2) is called; no Mix script can call it, though an embedder can), so it is the one to compare a stat() map's `uid` against when deciding whether a path is yours (v0.41.0)", contract!(() -> number)),
    ("gid", CapabilityClass::Env,             "system",  "Effective group id of this process (getegid) — the companion to uid(), for comparing against a stat() map's `gid`; same fsgid caveat, and it answers only whether a file's group is the EFFECTIVE one, so use groups() to decide which permission class applies (v0.41.0)", contract!(() -> number)),
    ("groups", CapabilityClass::Env,          "system",  "Every group id this process is in — the getgroups(2) supplementary set plus the effective gid, sorted. This is what decides whether the kernel applies a file's GROUP or OTHER permission bits: gid() alone cannot answer it for a file grouped under one of your other groups (v0.42.0)", contract!(() -> list(number))),
    ("args", CapabilityClass::Pure,            "system",  "Return list of script arguments", contract!(() -> list(string))),
    ("getopt", CapabilityClass::Pure,          "system",  "Parse args against a spec map: getopt(args(), {all:{short:\"a\"}, out:{short:\"o\", arg:true}}) -> {opts, rest, errors}. opts has every declared option (flag->bool, value->string|nil); rest=positionals (incl. post `--`); errors=collected unknown-option/missing-value strings ([]=clean). Forms: --long, -s, --k=v, --k v, -s v, -- terminator. Minimal: no bundling/abbrev (v0.12.0)", contract!((args: list, spec: map) -> map("getopt_result", {opts: map, rest: list, errors: list}))),
    ("exit", CapabilityClass::Process,            "system",  "Exit with optional status code", contract!((code?: number) -> nil; effects[terminates]; failure[terminates])),
    ("sleep", CapabilityClass::Pure,           "system",  "Sleep for N seconds (async)", contract!((n: number) -> nil; effects[blocking]; failure[raises])),
    ("run", CapabilityClass::Process,             "system",  "Run shell command via sh, return trimmed stdout as string. run(cmd, [{timeout: seconds}]) — 0 (default) = no deadline; a timed-out child is PG-killed and run dies (catchable)", contract!((cmd: string, opts?: map) -> string; effects[blocking, shell]; failure[raises])),
    ("run_rc", CapabilityClass::Process,          "system",  "Run shell command, return {rc, stdout, stderr, timed_out, interrupted} map. run_rc(cmd, [{timeout: seconds}]) — 0 (default) = no deadline; timeout → rc=-1 timed_out=true", contract!((cmd: string, opts?: map) -> map("run_rc_result", {rc: number, stdout: string, stderr: string, timed_out: bool, interrupted: bool}); effects[must_use, blocking, shell]; failure[returns_result])),
    ("run_stream", CapabilityClass::Process,      "system",  "Run an argv LIST directly (no sh), inheriting stdio so output streams live and the child can use the terminal (interactive when it has a pty, e.g. ssh -t); returns the exit code. run_stream(argv, [{env, clear_env, cwd}]) — same env/cwd semantics as run_argv, so an interactive child gets variables without an `env` prefix exposing them in its ps argv (v0.51.0). The run_argv-only opts (timeout, stdin, stdout, stderr, max_output, stream) are rejected by name: this runner blocks until the child exits and captures nothing", contract!((argv: list(string), opts?: map("run_stream_options", {env: map, clear_env: bool, cwd: any_of(string, nil)})) -> number; effects[must_use, blocking]; failure[returns_result])),
    ("run_argv", CapabilityClass::Process,        "system",  "Run an argv list directly (no shell) with structured stdio routing and a whole-call deadline that starts before route opening. opts: timeout; stdin nil|string|bytes|buffer|{file}|{null:true}; stdout capture|inherit|null|{file,append?,mode?}; stderr capture|inherit|null|stdout|{file,append?,mode?}; cwd/env/clear_env; max_output; stream. stdout/stderr default capture; output files default truncate, mode 0o600. Routed non-capture streams return \"\" with truncation false and are not capped. stderr:stdout merges into stdout. File-open failure or route-open deadline is a PROCESS_STDIO value and the child is not spawned. Captured output abandoned at a deadline is returned partially with its truncation flag true. stream:true + stdout:inherit and all bad options raise OPTION_INVALID before spawn. Ordinary command/setup failure is encoded in the VALUE; timeout default 30s; max_output default 8 MiB per captured stream", contract!((argv: list(string), opts?: map("run_argv_options", {timeout: number, stdin: any_of(string, bytes, buffer, map, nil), stdout: any_of(string, map), stderr: any_of(string, map), cwd: any_of(string, nil), env: map, clear_env: bool, max_output: number, stream: bool})) -> map("process_result", {ok: bool, exit_code: any, stdout: string, stderr: string, timed_out: bool, interrupted: bool, signal: any, duration_ms: number, stdout_truncated: bool, stderr_truncated: bool, utf8_lossy: bool, error_code: any, error: any}); effects[must_use, blocking]; failure[returns_result])),
    ("run_argv_must", CapabilityClass::Process,   "system",  "Fail-fast run_argv with the same structured stdio opts: returns captured stdout unchanged when ok and no captured stream truncated (\"\" when stdout is routed), else raises PROCESS_EXIT_NONZERO / PROCESS_TIMEOUT / PROCESS_SIGNAL / PROCESS_INTERRUPTED / PROCESS_OUTPUT_LIMIT or the result's setup/lifecycle error_code (PROCESS_STDIO / PROCESS_SPAWN / PROCESS_IO / PROCESS_INTERNAL) with the complete result map in $err.details.result", contract!((argv: list(string), opts?: map("run_argv_options", {timeout: number, stdin: any_of(string, bytes, buffer, map, nil), stdout: any_of(string, map), stderr: any_of(string, map), cwd: any_of(string, nil), env: map, clear_env: bool, max_output: number, stream: bool})) -> string; effects[blocking]; failure[raises])),
    ("run_pipeline", CapabilityClass::Process,    "system",  "Run one or more argv stages without a shell, connecting each stdout to the next stdin. Stage maps accept argv/cwd/env/clear_env/stderr, plus stdin on the first stage and stdout on the last, using run_argv's stdio grammar. Every route and pipe is prepared before any stage runs, so PIPELINE_STDIO means no stage ran. Returns a distinct pipeline_result with final stdout/exit fields and per-stage outcomes. One whole-call deadline starts before route opening; captured output abandoned at that deadline is partial with its truncation flag true. Non-final SIGPIPE is NOT accepted by default: any stage killed by a signal makes the pipeline not-ok, matching `set -o pipefail`. Pass allow_signal:true to accept a non-final SIGPIPE when every downstream stage succeeded (the `yes | head -1` idiom). Ordinary failure is encoded in the VALUE — never raises", contract!((stages: list, opts?: map("run_pipeline_options", {timeout: number, max_output: number, allow_signal: bool})) -> map("pipeline_result", {ok: bool, exit_code: any, stdout: string, stderr: string, timed_out: bool, interrupted: bool, signal: any, duration_ms: number, stdout_truncated: bool, stderr_truncated: bool, utf8_lossy: bool, error_code: any, error: any, stages: list(map("pipeline_stage_result", {index: number, argv: list(string), ok: bool, exit_code: any, signal: any, duration_ms: number, stderr: string, stderr_truncated: bool, utf8_lossy: bool, accepted_signal: bool}))}); effects[must_use, blocking]; failure[returns_result])),
    ("run_pipeline_must", CapabilityClass::Process, "system", "Fail-fast run_pipeline twin: returns final stdout unchanged when the pipeline is ok and no captured output truncated; otherwise raises PIPELINE_* with the complete pipeline_result in $err.details.result", contract!((stages: list, opts?: map("run_pipeline_options", {timeout: number, max_output: number, allow_signal: bool})) -> string; effects[blocking]; failure[raises])),
    ("spawn", CapabilityClass::Process,           "system",  "Start background process via /bin/sh -c, return PID. Every argument must be a STRING and none is coerced — a non-string raises TYPE_MISMATCH rather than being stringified into a doomed sh command (spawn returns a PID, not a result map, so a misrun child would otherwise fail invisibly). There is no argv form: use run_argv for an argv list in the foreground (strict since v0.52.0)", contract!((cmd: string, stdout?: string, stderr?: string) -> number; effects[shell]; failure[raises])),
    ("kill", CapabilityClass::Process,            "system",  "Send signal to process (default SIGTERM); returns false when the signal could not be delivered. Both arguments must be whole NUMBERS and neither is coerced — a bool/string pid raises TYPE_MISMATCH rather than becoming 0 (which signals this process's whole group), and an unrecognised signal raises rather than silently defaulting to SIGTERM (strict since v0.52.0)", contract!((pid: number, signal?: number) -> bool; effects[must_use]; failure[returns_result])),
    ("shell_quote", CapabilityClass::Pure,     "system",  "Single-quote-wrap a string for safe interpolation into a POSIX shell command", contract!((s: string) -> string)),
    ("sql_quote", CapabilityClass::Pure,       "system",  "Escape a string for SQL string literals: doubles ' and escapes \\ (MySQL/MariaDB-safe — the documented target; also safe for SQLite, where a literal backslash arrives doubled — use sqlexec binds for exact bytes); NUL bytes stripped", contract!((s: string) -> string)),
    ("random_password", CapabilityClass::Pure, "system",  "Generate an alphanumeric password (default len 16, no O/o, guaranteed upper+lower+digit)", contract!((len?: number) -> string)),
    ("ssh_run", CapabilityClass::Network,         "system",  "Run a command on a remote host via ssh; returns {stdout, stderr, exit_code, ok, duration_ms, host, timed_out, interrupted, utf8_lossy}", contract!((host: string, cmd: any_of(string, list), opts?: map) -> map("ssh_result", {stdout: string, stderr: string, exit_code: number, ok: bool, duration_ms: number, host: string, timed_out: bool, interrupted: bool, utf8_lossy: bool}); effects[must_use, blocking]; failure[returns_result])),
    ("ssh_must", CapabilityClass::Network,        "system",  "ssh_run wrapper: returns stdout on success, throws a Mix error otherwise", contract!((host: string, cmd: any_of(string, list), opts?: map) -> string; effects[blocking]; failure[raises])),
    ("ssh_mix", CapabilityClass::Network,         "system",  "Run Mix source on a remote host: ships the source over ssh stdin into `/opt/cosmix/bin/mix -`, bypassing ALL shell quoting. ssh_mix(host, source, [opts]) -> same map as ssh_run; bindings maps valid Mix identifier names to strict-data-encoded values prepended as `$name` assignments, and decode:\"data\"|\"json\" adds a parsed `.value` from stdout. Accepts every ssh_run opt except stdin/env_transport. Remote command failure stays in the result value; invalid arguments/options raise locally. (v0.20.4)", contract!((host: string, source: string, opts?: map("ssh_mix_options", {timeout: number, connect_timeout: number, multiplex: bool, batch: bool, strict_host_key: string, env: map, cwd: string, extra_ssh_args: list(string), decode: string, bindings: map})) -> map("ssh_result", {stdout: string, stderr: string, exit_code: number, ok: bool, duration_ms: number, host: string, timed_out: bool, interrupted: bool, utf8_lossy: bool, value: any}); effects[must_use, blocking]; failure[returns_result])),
    ("ssh_exec", CapabilityClass::Network,        "system",  "Run an argv list DIRECTLY on a remote host via a strict-data driver and remote run_argv. Remote stdio allowlist: stdin nil|string|{file}|{null:true} (a stdin STRING is always data, as locally — there is no stdin \"inherit\" route on either side); stdout capture|null|{file}; stderr capture|null|stdout|{file}. File paths resolve remotely. stdout/stderr inherit and stream:true raise OPTION_INVALID locally before ssh because they would corrupt or bypass the result envelope. Binary stdin also raises locally. Transport/protocol failures and remote command failure are returned in the process_result plus host; a remote without run_argv returns SSH_REMOTE_UNSUPPORTED without running the command", contract!((host: string, argv: list(string), opts?: map) -> map("process_result", {ok: bool, exit_code: any, stdout: string, stderr: string, timed_out: bool, interrupted: bool, signal: any, duration_ms: number, stdout_truncated: bool, stderr_truncated: bool, utf8_lossy: bool, error_code: any, error: any, host: string}); effects[must_use, blocking]; failure[returns_result])),
    ("process_alive", CapabilityClass::Process,   "system",  "Test if a process exists (signal 0 check). pid must be a whole NUMBER and is not coerced — a bool/string pid raises TYPE_MISMATCH rather than becoming 0, which would make the reaping waitpid() collect an arbitrary child of this process group and then report a boolean as alive (strict since v0.52.0)", contract!((pid: number) -> bool)),
    ("panic", CapabilityClass::Process,           "system",  "Abort via an uncatchable Rust panic (distinct from catchable die); the SPEC 18 §3.4 handler boundary isolates it in --serve mode", contract!((msg: string) -> nil; effects[terminates]; failure[terminates])),
    ("raise", CapabilityClass::Pure,           "system",  "Raise a catchable structured error: raise(code, message[, details]) — code is UPPER_SNAKE (e.g. \"VALIDATION_REQUIRED\", stable identifiers, scripts may define their own); a non-string message is coerced to its string form; catch with `catch $msg, $err` and read $err.code / $err.details / $err.frames (v0.29.0)", contract!((code: string, message: any, details?: map) -> nil; failure[raises])),

    ("require_key", CapabilityClass::Pure,     "validate", "Assert a map key is present with a non-nil value and return it; raises VALIDATION_REQUIRED with details {path, expected, actual_type} (v0.29.0)", contract!((map: map, key: string) -> any; failure[raises])),
    ("expect_type", CapabilityClass::Pure,     "validate", "Assert a value's type and return it: expect_type($v, \"integer\") — types: any nil bool number integer string bytes buffer list map function (integer = finite whole within ±2^53-1); raises VALIDATION_TYPE (v0.29.0)", contract!((v: any, kind: string) -> any; failure[raises])),
    ("nonblank", CapabilityClass::Pure,        "validate", "Assert a string contains a non-whitespace character, return it UNTRIMMED; the optional label names the value in the error; raises VALIDATION_NONBLANK — the boundary guard against nil/\"\" flowing into hostnames and paths (v0.29.0)", contract!((v: any, label?: string) -> string; failure[raises])),
    ("get_or", CapabilityClass::Pure,          "validate", "Map lookup with a default that covers BOTH an absent key and a nil value (the tolerant twin of require_key) (v0.29.0)", contract!((map: map, key: string, default: any) -> any)),
    ("validate", CapabilityClass::Pure,        "validate", "Validate a map against a field spec at a job/API boundary: validate($raw, {node: {type: \"string\", nonblank: true}, plan: {enum: [\"gold\", \"silver\"]}, vmid: {type: \"integer\", min: 100, max: 999999}, tags: {required: false, type: \"list\", items: {type: \"string\"}}, owner: {type: \"map\", schema: {name: {nonblank: true}}}}). Rules: required (default TRUE) / type (string or list of types) / nonblank / enum / min / max / min_length / max_length / items / schema. Returns the ORIGINAL map unchanged; optional absent-or-nil fields skip their rules; unknown INPUT fields pass through; unknown RULE keys raise VALIDATION_SPEC; violations raise VALIDATION_* with details {path, expected, actual_type} — paths like owner.name and tags[2] (v0.29.0)", contract!((value: map, spec: map) -> map; failure[raises])),
    ("hostname", CapabilityClass::Env,        "system",  "Return the system hostname", contract!(() -> string)),
    ("cwd", CapabilityClass::Env,             "system",  "Return current working directory", contract!(() -> string)),
    ("chdir", CapabilityClass::Process,           "system",  "Change current working directory", contract!((path: string) -> nil; failure[raises])),
    ("platform", CapabilityClass::Env,        "system",  "Return OS platform string (linux, macos, windows, etc.)", contract!(() -> map("platform"))),
    ("which", CapabilityClass::Env,           "system",  "Locate an EXECUTABLE in PATH: a PATH entry is returned only if it is a regular file the kernel says this process may execute (faccessat2 X_OK, so POSIX ACLs count), never merely a file that exists, and never a directory. cmd must be a string and is not coerced. Returns nil when nothing on PATH is runnable under that name (executability enforced since v0.52.0)", contract!((cmd: string) -> any_of(string, nil); failure[raises])),
    ("date_format", CapabilityClass::Pure,     "system",  "Format Unix timestamp with strftime pattern", contract!((ts: number, fmt?: string) -> string; failure[raises])),
    ("date_parse", CapabilityClass::Pure,      "system",  "Parse date string with strftime pattern into Unix timestamp", contract!((s: string) -> number)),
    ("now_iso", CapabilityClass::Pure,         "system",  "Current time as ISO 8601 string", contract!(() -> string)),
    ("duration_format", CapabilityClass::Pure, "system",  "Format seconds as human-readable duration (e.g. \"2h 15m\")", contract!((secs: number) -> string; failure[raises])),
    ("relative_time", CapabilityClass::Pure,   "system",  "Format timestamp as relative string (e.g. \"3 hours ago\")", contract!((ts: number) -> string; failure[raises])),
    ("base64_encode", CapabilityClass::Pure,   "system",  "Encode string as base64", contract!((s: any_of(string, bytes, buffer)) -> string)),
    ("base64_decode", CapabilityClass::Pure,   "system",  "Decode base64 string", contract!((s: any_of(string, bytes, buffer)) -> bytes)),
    ("hash_blake3", CapabilityClass::Pure,     "system",  "BLAKE3 hash of a string/bytes/buffer → lowercase hex; pass {raw:true} for the raw digest as bytes (v0.66.0)", contract!((s: any_of(string, bytes, buffer), opts?: map) -> any_of(string, bytes); failure[raises])),
    ("hash_sha256", CapabilityClass::Pure,     "system",  "SHA-256 hash of a string/bytes/buffer → lowercase hex; pass {raw:true} for the raw 32 digest bytes (v0.66.0 — before that a second argument was silently IGNORED)", contract!((s: any_of(string, bytes, buffer), opts?: map) -> any_of(string, bytes); failure[raises])),
    ("hash_md5", CapabilityClass::Pure,        "system",  "MD5 hash of a string/bytes/buffer → lowercase hex; {raw:true} → bytes. ⚠ CRYPTOGRAPHICALLY BROKEN (collisions since 2004) — legacy interop only (Content-MD5, mail dedup keys, checksums against existing tools), NEVER a security decision; use hash_sha256/hash_blake3 for those (v0.66.0)", contract!((s: any_of(string, bytes, buffer), opts?: map) -> any_of(string, bytes); failure[raises])),
    ("hash_sha1", CapabilityClass::Pure,       "system",  "SHA-1 hash of a string/bytes/buffer → lowercase hex; {raw:true} → bytes. ⚠ CRYPTOGRAPHICALLY BROKEN (SHAttered, 2017) — legacy interop only (git object ids, older ETags/APIs), NEVER a security decision; use hash_sha256/hash_blake3 for those (v0.66.0)", contract!((s: any_of(string, bytes, buffer), opts?: map) -> any_of(string, bytes); failure[raises])),
    ("hmac_sha256", CapabilityClass::Pure,     "system",  "HMAC-SHA256 (RFC 2104) of a message with a secret key → lowercase hex; {raw:true} → the 32 MAC bytes (v0.66.0) — webhook signature verification (Stripe-Signature etc). Accepts string/bytes/buffer for both args (requires crypto feature)", contract!((key: any_of(string, bytes, buffer), msg: any_of(string, bytes, buffer), opts?: map) -> any_of(string, bytes); failure[raises])),
    ("constant_time_eq", CapabilityClass::Pure, "system",  "Timing-safe equality for secrets/MACs: compares full length with no early exit (plain == leaks a timing oracle). Use for webhook signature comparison. Accepts string/bytes/buffer", contract!((a: any_of(string, bytes, buffer), b: any_of(string, bytes, buffer)) -> bool)),
    ("hash_file", CapabilityClass::FsRead,     "system",  "Streaming digest of a file, fixed 64 KiB working set whatever the size: hash_file(path[, \"md5\"|\"sha1\"|\"sha256\"|\"blake3\"][, {raw:true}]) → lowercase hex, or bytes with {raw:true}. md5/sha1 added v0.66.0 and are BROKEN hashes for legacy interop only (v0.24.0)", contract!((path: string, algo?: string, opts?: map) -> any_of(string, bytes); failure[raises])),
    ("uuid", CapabilityClass::Pure,            "system",  "Generate a new random UUID v4 string", contract!(() -> string)),
    ("dkim_keygen", CapabilityClass::Pure,     "system",  "Generate a DKIM keypair. dkim_keygen(\"rsa\", [bits=2048]) or dkim_keygen(\"ed25519\") → {algorithm, private_pem, public_b64, dns_txt_record}", contract!((algo: string, bits?: number) -> map("dkim_keypair", {algorithm: string, private_pem: string, public_b64: string, dns_txt_record: string}))),
    ("http_get", CapabilityClass::Network,        "system",  "HTTP GET. http_get(url, [headers], [{timeout, ssl_verify, ca_file, ca_pem}] — timeout default 30, 0 disables; ssl_verify default true, false skips TLS cert/hostname checks like curl -k; ca_file/ca_pem ADD a private CA to the default roots — mutually exclusive with each other and with ssl_verify:false, 4 MiB cap, bad PEM raises HTTP_TLS, v0.29.0) → {status, body, bytes, headers, final_url, duration_ms, error_code, error} (headers lowercase-name→list; final_url after redirects; transport failure = status:0 + HTTP_* error_code; v0.30.0). `body` is the response decoded as UTF-8 (nil if not valid UTF-8); `bytes` is the raw byte buffer. Response bodies are capped at 64 MiB (over-cap → {status:0, error}).", contract!((url: string, headers?: map, opts?: map) -> map("http_response", {status: number, body: any, bytes: bytes, headers: map, final_url: string, duration_ms: number, error_code: any, error: any}); effects[must_use, blocking]; failure[returns_result]; cond_caps[ca_file: FsRead])),
    ("http_post", CapabilityClass::Network,       "system",  "HTTP POST. http_post(url, body, [headers], [{timeout, ssl_verify, ca_file, ca_pem}]) → {status, body, bytes, headers, final_url, duration_ms, error_code, error} (headers lowercase-name→list; final_url after redirects; transport failure = status:0 + HTTP_* error_code; v0.30.0). Opts (incl. ssl_verify: false → skip TLS verification like curl -k) and `body`/`bytes` semantics (incl. the 64 MiB body cap) match http_get.", contract!((url: string, body: any, headers?: map, opts?: map) -> map("http_response", {status: number, body: any, bytes: bytes, headers: map, final_url: string, duration_ms: number, error_code: any, error: any}); effects[must_use, blocking]; failure[returns_result]; cond_caps[ca_file: FsRead])),
    ("http_request", CapabilityClass::Network,    "system",  "HTTP any-verb. http_request(method, url, [body], [headers], [{timeout, ssl_verify, ca_file, ca_pem}]) → {status, body, bytes, headers, final_url, duration_ms, error_code, error} (headers lowercase-name→list; final_url after redirects; transport failure = status:0 + HTTP_* error_code; v0.30.0). Opts (incl. ssl_verify: false → skip TLS verification like curl -k) and `body`/`bytes` semantics (incl. the 64 MiB body cap) match http_get.", contract!((method: string, url: string, body?: any, headers?: map, opts?: map) -> map("http_response", {status: number, body: any, bytes: bytes, headers: map, final_url: string, duration_ms: number, error_code: any, error: any}); effects[must_use, blocking]; failure[returns_result]; cond_caps[ca_file: FsRead])),
    ("bytes_len", CapabilityClass::Pure,       "system",  "Length of a Value::Bytes buffer in bytes (v0.3.1)", contract!((b: any_of(bytes, buffer)) -> number)),
    ("string_to_bytes", CapabilityClass::Pure, "system",  "Convert a string to its UTF-8 byte representation (v0.3.1)", contract!((s: string) -> bytes)),
    ("bytes_to_string", CapabilityClass::Pure, "system",  "Convert a bytes buffer to a string; strict UTF-8, or pass {lossy:true} for a from_utf8_lossy decode (v0.17.2). Also accepts a Buffer.", contract!((b: any_of(bytes, buffer), opts?: map) -> string)),
    ("bytes_find", CapabilityClass::Pure,      "system",  "0-based byte offset of needle in a bytes/buffer value, -1 if absent; optional `from` (signed, clamped) starts the scan and the result stays ABSOLUTE. Needle: bytes/buffer/string/byte number 0-255, and must not be empty. ⚠ NEVER use bare in a condition: -1 is truthy, 0 is falsy (MIX-W2305) (v0.64.0)", contract!((b: any_of(bytes, buffer), needle: any_of(bytes, buffer, string, number), from?: number) -> number; failure[raises])),
    ("bytes_starts_with", CapabilityClass::Pure, "system", "Test whether a bytes/buffer value starts with a prefix (bytes/buffer/string/byte number 0-255); an empty prefix is true (v0.64.0)", contract!((b: any_of(bytes, buffer), prefix: any_of(bytes, buffer, string, number)) -> bool; failure[raises])),
    ("bytes_split", CapabilityClass::Pure,     "system",  "Split a bytes/buffer value on a separator (bytes/buffer/string/byte number 0-255) → list of bytes. Same piece rules as split(): absent separator → one whole piece, leading/trailing separator → empty piece. An EMPTY separator raises (v0.64.0)", contract!((b: any_of(bytes, buffer), sep: any_of(bytes, buffer, string, number)) -> list(bytes); failure[raises])),
    ("bytes_concat", CapabilityClass::Pure,    "system",  "Concatenate 1+ bytes/buffer/string values into one new bytes (a string joins as its own UTF-8). A list argument raises and names bytes_from (v0.64.0)", contract!((a: any_of(bytes, buffer, string), rest: ...any_of(bytes, buffer, string)) -> bytes; failure[raises])),
    ("bytes_from", CapabilityClass::Pure,      "system",  "Build bytes from a LIST, flat-splicing each item — int 0-255 = one byte, string = its UTF-8, bytes/buffer = its content (same item vocabulary as buffer([items])) (v0.64.0)", contract!((items: list) -> bytes; failure[raises])),
    ("bytes_to_hex", CapabilityClass::Pure,    "system",  "Lowercase hex of a bytes/buffer value, two chars per byte, no separator (v0.64.0)", contract!((b: any_of(bytes, buffer)) -> string; failure[raises])),
    ("bytes_from_hex", CapabilityClass::Pure,  "system",  "Decode a hex string to bytes — strict: even length, [0-9a-fA-F] only, no separators or whitespace. Exact inverse of bytes_to_hex (v0.64.0)", contract!((hex: string) -> bytes; failure[raises])),
    ("buffer", CapabilityClass::Pure,          "buffer",  "Create a reference-semantic MUTABLE byte buffer (the escape hatch from value semantics for large binary/audio/video). buffer() empty; buffer(n) n zero bytes; buffer(string) UTF-8; buffer(bytes|buffer) independent copy; buffer([items]) flat splice of int 0-255 / string / bytes / buffer. Append with buffer_push (O(1) amortized, aliases share); freeze() to a value-semantic bytes (v0.26.0)", contract!((init?: any_of(number, string, bytes, buffer, list)) -> buffer)),
    ("buffer_push", CapabilityClass::Pure,     "buffer",  "Append bytes to a buffer IN PLACE (reference-semantic: every alias sees the growth). Each item is an int 0-255, string (UTF-8), bytes, or buffer. Self-append-safe (v0.26.0)", contract!((buf: buffer, item: any_of(number, string, bytes, buffer), rest: ...any_of(number, string, bytes, buffer)) -> nil; effects[mutates_args])),
    ("buffer_get", CapabilityClass::Pure,      "buffer",  "Byte at 0-based index i as a number 0-255, or nil if out of range (v0.26.0)", contract!((buf: buffer, i: number) -> any_of(number, nil))),
    ("buffer_set", CapabilityClass::Pure,      "buffer",  "Write byte (0-255) at 0-based index i, in place; errors if i is out of range — grow with buffer_push first (v0.26.0)", contract!((buf: buffer, i: number, byte: number) -> nil; effects[mutates_args])),
    ("freeze", CapabilityClass::Pure,          "buffer",  "Snapshot a buffer to a value-semantic bytes (a copy of the current content) — the bridge into write_file/hash/base64/http (v0.26.0)", contract!((buf: buffer) -> bytes)),
    ("dns_lookup", CapabilityClass::Network,      "system",  "Resolve a hostname to a list of IP address strings", contract!((host: string) -> list(string); effects[blocking]; failure[raises])),
    ("help", CapabilityClass::Pure,            "system",  "Show Mix builtin help in the REPL", contract!(() -> nil)),

    ("fmt", CapabilityClass::Pure,             "format",  "printf-style format string → string. Specs: %s %d %f %.Nf %Nd %-Ns %0Nd %% (v0.2.0; %0Nd zero-pad v0.54.0 — numeric only, use lpad(s,n,\"0\") for strings). Dynamic width `*` takes the width from the next argument: %*s %-*s %*d %0*d %*f (v0.63.0; the width argument must be a non-negative integer)", contract!((tmpl: string, args: ...any) -> string)),
    ("printf", CapabilityClass::Pure,          "format",  "Formatted write to stdout (no trailing newline — include \\n explicitly) (v0.2.0)", contract!((tmpl: string, args: ...any) -> nil)),
    ("eprintf", CapabilityClass::Pure,         "format",  "Formatted write to stderr (v0.2.0)", contract!((tmpl: string, args: ...any) -> nil)),
    ("write_stdout", CapabilityClass::Pure,    "io",      "Write values to fd 1 AS THEY ARE — no trailing newline, no separator, flushed. bytes/buffer go out verbatim; every other value renders exactly as print() renders it. Never re-opens a path, so it works when fd 1 belongs to another user (the sieve-filter case). A failed write RAISES (catchable): IO_BROKEN_PIPE when the consumer went away, else IO_WRITE_FAILED — unlike print, which swallows it (v0.65.0)", contract!((v: any, rest: ...any) -> nil; failure[raises])),
    ("write_stderr", CapabilityClass::Pure,    "io",      "Write values to fd 2 as they are — the stderr twin of write_stdout, same contract (v0.65.0)", contract!((v: any, rest: ...any) -> nil; failure[raises])),
    ("print_raw", CapabilityClass::Pure,       "io",      "ALIAS of write_stdout — identical contract, the spelling for `print` without the trailing newline (v0.65.0)", contract!((v: any, rest: ...any) -> nil; failure[raises])),
    ("eprint_raw", CapabilityClass::Pure,      "io",      "ALIAS of write_stderr — identical contract, the spelling for `eprint` without the trailing newline (v0.65.0)", contract!((v: any, rest: ...any) -> nil; failure[raises])),
    ("format_bytes", CapabilityClass::Pure,    "format",  "Format byte count as human-readable size (e.g. \"1.5 MB\"); a non-numeric argument raises (strict since v0.55.0)", contract!((n: number) -> string; failure[raises])),
    ("format_number", CapabilityClass::Pure,   "format",  "Format number with thousands separators; non-numeric value/decimals arguments raise (strict since v0.55.0)", contract!((n: number, decimals?: number) -> string; failure[raises])),

    ("json_parse", CapabilityClass::Pure,      "json",    "Parse JSON string into Mix value", contract!((s: string) -> any)),
    ("json_encode", CapabilityClass::Pure,     "json",    "Encode Mix value as JSON string", contract!((v: any, pretty?: any) -> string)),
    ("jq", CapabilityClass::Pure,              "json",    "Run a jq filter; filter MUST yield 0 (→nil) or 1 (→value) output, >1 raises. jq(value, filter)", contract!((v: any, filter: string) -> any)),
    ("jq_all", CapabilityClass::Pure,          "json",    "Run a jq filter, collect ALL outputs as a list (the stream case). jq_all(value, filter)", contract!((v: any, filter: string) -> list)),
    ("read_json", CapabilityClass::FsRead,       "json",    "Read a single-record JSON file directly into a Mix value (v0.2.3)", contract!((path: string) -> any; failure[raises])),
    ("read_jsonl", CapabilityClass::FsRead,      "json",    "Read a JSON-lines file — list of records, strict by default, {skip_errors: true} for lenient (v0.2.3)", contract!((path: string, opts?: map) -> list; failure[raises])),
    ("toml_parse", CapabilityClass::Pure,      "json",    "Parse TOML string into Mix map", contract!((s: string) -> map)),
    ("toml_encode", CapabilityClass::Pure,     "json",    "Encode Mix value as TOML. Raises TOML_UNREPRESENTABLE with {path,type} details for nil, function, bytes, or buffer values instead of silently replacing them with empty strings (strict since v0.55.0)", contract!((v: any) -> string; failure[raises])),
    ("data_parse", CapabilityClass::Pure,      "json",    "Parse a strict-data `.conf.mix` string into a Mix value (inverse of data_encode) (v0.3.2)", contract!((s: string) -> any)),
    ("data_encode", CapabilityClass::Pure,     "json",    "Encode a Mix value as a strict-data `.conf.mix` string with correct \\$ / \\~ / \\\\ escaping; round-trips through data_parse. data_encode(value, [pretty]) — truthy 2nd arg emits multi-line indented output (v0.3.2)", contract!((v: any, pretty?: any) -> string)),

    ("ds_patch_elements", CapabilityClass::Pure, "datastar", "Frame an HTML fragment as a Datastar patch-elements SSE event: ds_patch_elements(html, [{selector, mode, view_transition}]) → event string. mode=outer(default)/inner/remove/replace/prepend/append/before/after. Caller MUST html_escape() untrusted content first — this only frames (requires datastar feature) (v0.18.1)", contract!((html: string, opts?: map) -> string)),
    ("ds_patch_signals", CapabilityClass::Pure, "datastar", "Frame a signal update as a Datastar patch-signals SSE event: ds_patch_signals(signals_map_or_json, [{only_if_missing}]) → event string. A map is JSON-encoded; a string is used verbatim (requires datastar feature) (v0.18.1)", contract!((signals: any_of(map, string, list), opts?: map) -> string)),
    ("ds_sse", CapabilityClass::Pure,          "datastar", "Assemble a text/event-stream response body from one event string or a list of them: ds_sse(event | [events]) → body. Pair with headers={\"Content-Type\":\"text/event-stream\"} (requires datastar feature) (v0.18.1)", contract!((event: any_of(string, list)) -> string)),
}

pub fn call_builtin(name: &str, args: Vec<Value>) -> MixResult<Option<Value>> {
    match name {
        "length" | "len" => builtin_len(args),
        "upper" => builtin_upper(args),
        "lower" => builtin_lower(args),
        "left" => builtin_left(args),
        "right" => builtin_right(args),
        "substr" => builtin_substr(args),
        "pos" => builtin_pos(args),
        "strip" | "trim" => builtin_strip(args),
        "replace" => builtin_replace(args),
        "split" => builtin_split(args),
        "join" => builtin_join(args),
        "starts_with" => builtin_starts_with(args),
        "ends_with" => builtin_ends_with(args),
        "contains" => builtin_contains(args),
        "repeat" => builtin_repeat(args),
        "lpad" => builtin_lpad(args),
        "rpad" => builtin_rpad(args),
        "lpad_w" => builtin_lpad_w(args),
        "rpad_w" => builtin_rpad_w(args),
        "reverse" => builtin_reverse(args),
        "words" => builtin_words(args),
        "word" => builtin_word(args),
        // Char-aware string ops (P0) — see the registry block above.
        "byte_length" => builtin_byte_length(args),
        "byte_pos" => builtin_byte_pos(args),
        "byte_lastpos" => builtin_byte_lastpos(args),
        "byte_index_of" => builtin_byte_index_of(args),
        "grapheme_count" => builtin_grapheme_count(args),
        "grapheme_substr" => builtin_grapheme_substr(args),
        "grapheme_reverse" => builtin_grapheme_reverse(args),
        "display_width" => builtin_display_width(args),
        "type" => builtin_type(args),
        "to_number" => builtin_to_number(args),
        "to_string" => builtin_to_string(args),
        "is_number" => builtin_is_number(args),
        "is_empty" => builtin_is_empty(args),
        // Math (v0.19.0)
        "round" => builtin_round_family("round", args, f64::round),
        "floor" => builtin_round_family("floor", args, f64::floor),
        "ceil" => builtin_round_family("ceil", args, f64::ceil),
        "trunc" => builtin_round_family("trunc", args, f64::trunc),
        "abs" => builtin_unary_math("abs", args, f64::abs),
        "sign" => builtin_sign(args),
        "band" => builtin_bitwise("band", args, |a, b| a & b),
        "bor" => builtin_bitwise("bor", args, |a, b| a | b),
        "bxor" => builtin_bitwise("bxor", args, |a, b| a ^ b),
        "bnot" => builtin_bnot(args),
        "bshl" => builtin_bshift("bshl", args),
        "bshr" => builtin_bshift("bshr", args),
        "sqrt" => builtin_unary_math("sqrt", args, f64::sqrt),
        "cbrt" => builtin_unary_math("cbrt", args, f64::cbrt),
        "exp" => builtin_unary_math("exp", args, f64::exp),
        "ln" => builtin_unary_math("ln", args, f64::ln),
        "log10" => builtin_unary_math("log10", args, f64::log10),
        "log2" => builtin_unary_math("log2", args, f64::log2),
        "sin" => builtin_unary_math("sin", args, f64::sin),
        "cos" => builtin_unary_math("cos", args, f64::cos),
        "tan" => builtin_unary_math("tan", args, f64::tan),
        "asin" => builtin_unary_math("asin", args, f64::asin),
        "acos" => builtin_unary_math("acos", args, f64::acos),
        "atan" => builtin_unary_math("atan", args, f64::atan),
        "pow" => builtin_binary_math("pow", args, f64::powf),
        "log" => builtin_binary_math("log", args, f64::log),
        "atan2" => builtin_binary_math("atan2", args, f64::atan2),
        "hypot" => builtin_binary_math("hypot", args, f64::hypot),
        "min" => builtin_min(args),
        "max" => builtin_max(args),
        "clamp" => builtin_clamp(args),
        "pi" => builtin_pi(args),
        "e" => builtin_e(args),
        "random" => builtin_random(args),
        "push" => builtin_push(args),
        "pop" => builtin_pop(args),
        "shift" => builtin_shift(args),
        "sort" => builtin_sort(args),
        "index_of" => builtin_index_of(args),
        "unique" => builtin_unique(args),
        "range" => builtin_range(args),
        "flat" => builtin_flat(args),
        "concat" => builtin_concat(args),
        "slice" => builtin_slice(args),
        "take" => builtin_take(args),
        "drop" => builtin_drop(args),
        "zip" => builtin_zip(args),
        "keys" => builtin_keys(args),
        "values" => builtin_values(args),
        "has_key" => builtin_has_key(args),
        "merge" => builtin_merge(args),
        "delete" => builtin_delete(args),
        "env" => builtin_env(args),
        "time" => builtin_time(args),
        "pid" => builtin_pid(args),
        "uid" => builtin_uid(args),
        "gid" => builtin_gid(args),
        "groups" => builtin_groups(args),
        "args" => builtin_args(args),
        "getopt" => builtin_getopt(args),
        "exit" => builtin_exit(args),
        // sleep is handled as async in the evaluator
        // "sleep" => builtin_sleep(args),
        "run" => builtin_run(args),
        "lastpos" => builtin_lastpos(args),
        "spawn" => builtin_spawn(args),
        "kill" => builtin_kill(args),
        "process_alive" => builtin_process_alive(args),
        "panic" => builtin_panic(args),
        "raise" => builtin_raise(args),
        "require_key" => builtin_require_key(args),
        "expect_type" => builtin_expect_type(args),
        "nonblank" => builtin_nonblank(args),
        "get_or" => builtin_get_or(args),
        "validate" => builtin_validate(args),
        "shell_quote" => builtin_shell_quote(args),
        "sql_quote" => builtin_sql_quote(args),
        "random_password" => builtin_random_password(args),
        "ssh_run" => builtin_ssh_run(args),
        "ssh_must" => builtin_ssh_must(args),
        "ssh_mix" => builtin_ssh_mix(args),
        "ssh_exec" => builtin_ssh_exec(args),
        "run_rc" => builtin_run_rc(args),
        "run_stream" => builtin_run_stream(args),
        "run_argv" => builtin_run_argv(args),
        "run_argv_must" => builtin_run_argv_must(args),
        "run_pipeline" => builtin_run_pipeline(args),
        "run_pipeline_must" => builtin_run_pipeline_must(args),
        "grep" => builtin_grep(args),
        "before" => builtin_before(args),
        "after" => builtin_after(args),
        "before_last" => builtin_before_last(args),
        "after_last" => builtin_after_last(args),
        "split_once" => builtin_split_once(args),
        "rsplit_once" => builtin_rsplit_once(args),
        "between" => builtin_between(args),
        "strip_prefix" => builtin_strip_prefix(args),
        "strip_suffix" => builtin_strip_suffix(args),
        "replace_first" => builtin_replace_first(args),
        "count_of" => builtin_count_of(args),
        "ltrim" => builtin_ltrim(args),
        "rtrim" => builtin_rtrim(args),
        "lines" => builtin_lines(args),
        "fields" => builtin_fields(args),
        "chars" => builtin_chars(args),
        "last_index_of" => builtin_last_index_of(args),
        "deep_eq" => builtin_deep_eq(args),
        "grep_lines" => builtin_grep_lines(args),
        "line_count" => builtin_line_count(args),
        "head" => builtin_head(args),
        "tail" => builtin_tail(args),
        "read_file" => builtin_read_file(args),
        "read_file_bytes" => builtin_read_file_bytes(args),
        "read_lines" => builtin_read_lines(args),
        "load_data" => builtin_load_data(args),
        "write_file" => builtin_write_file(args),
        "write_new" => builtin_write_new(args),
        "append_file" => builtin_append_file(args),
        "exists" => builtin_exists(args),
        "access" => builtin_access(args),
        "is_dir" => builtin_is_dir(args),
        "is_file" => builtin_is_file(args),
        "realpath" => builtin_realpath(args),
        "glob" => builtin_glob(args),
        "ls" => builtin_ls(args),
        "mkdir" => builtin_mkdir(args),
        "flock" => builtin_flock(args),
        "funlock" => builtin_funlock(args),
        "copy" => builtin_copy(args),
        "copy_tree" => builtin_copy_tree(args),
        "rename" => builtin_rename(args),
        "symlink" => builtin_symlink(args),
        "read_link" => builtin_read_link(args),
        "remove" => builtin_remove(args),
        "remove_dir" => builtin_remove_dir(args),
        "chmod" => builtin_chmod(args),
        "chown" => builtin_chown(args),
        "stat" => builtin_stat(args),
        // Strict-data serializer pair — core (no feature gate): both
        // `Value::to_mix_data_string` and `parse_data` are unconditional.
        "data_encode" => builtin_data_encode(args),
        "data_parse" => builtin_data_parse(args),
        #[cfg(feature = "json")]
        "json_parse" => builtin_json_parse(args),
        #[cfg(feature = "json")]
        "read_json" => builtin_read_json(args),
        #[cfg(feature = "json")]
        "read_jsonl" => builtin_read_jsonl(args),
        #[cfg(feature = "json")]
        "json_encode" => builtin_json_encode(args),
        #[cfg(feature = "json")]
        "jq" => builtin_jq(args),
        #[cfg(feature = "json")]
        "jq_all" => builtin_jq_all(args),
        #[cfg(feature = "regex")]
        "regex_match" => builtin_regex_match(args),
        #[cfg(feature = "regex")]
        "regex_find" => builtin_regex_find(args),
        #[cfg(feature = "regex")]
        "regex_replace" => builtin_regex_replace(args),
        #[cfg(feature = "regex")]
        "regex_split" => builtin_regex_split(args),
        #[cfg(feature = "regex")]
        "re_match" => builtin_re_match(args),
        #[cfg(feature = "regex")]
        "re_find" => builtin_re_find(args),
        #[cfg(feature = "regex")]
        "re_replace" => builtin_re_replace(args),
        #[cfg(feature = "regex")]
        "re_split" => builtin_re_split(args),
        // The registry lists these names unconditionally (is_builtin,
        // capability_category, `mix help` are all table-driven), so a
        // no-regex build must refuse LOUDLY here — without these arms the
        // call falls through to a misleading FUNCTION_UNDEFINED (the
        // markdown/ds_*/xml_parse precedent; GLM review of 4caec4e1,
        // MAJOR 1 — which also flagged the same pre-existing hole for
        // the four legacy regex_* names, closed below).
        #[cfg(not(feature = "regex"))]
        "re_match" | "re_find" | "re_replace" | "re_split" | "regex_match" | "regex_find"
        | "regex_replace" | "regex_split" => Err(MixError::RuntimeError {
            span: None,
            msg: format!("{name}() requires the `regex` feature"),
        }),
        #[cfg(feature = "toml")]
        "toml_parse" => builtin_toml_parse(args),
        #[cfg(feature = "toml")]
        "toml_encode" => builtin_toml_encode(args),
        #[cfg(feature = "datetime")]
        "date_format" => builtin_date_format(args),
        #[cfg(feature = "datetime")]
        "date_parse" => builtin_date_parse(args),
        #[cfg(feature = "datetime")]
        "now_iso" => builtin_now_iso(args),
        #[cfg(feature = "datetime")]
        "duration_format" => builtin_duration_format(args),
        #[cfg(feature = "datetime")]
        "relative_time" => builtin_relative_time(args),
        "basename" => builtin_basename(args),
        "dirname" => builtin_dirname(args),
        "extname" => builtin_extname(args),
        "path_join" => builtin_path_join(args),
        "path_parts" => builtin_path_parts(args),
        "walk" => builtin_walk(args),
        "hostname" => builtin_hostname(args),
        "cwd" => builtin_cwd(args),
        "chdir" => builtin_chdir(args),
        "platform" => builtin_platform(args),
        "which" => builtin_which(args),
        "format_bytes" => builtin_format_bytes(args),
        "format_number" => builtin_format_number(args),
        "template" => builtin_template(args),
        "fmt" => builtin_fmt(args),
        // printf / eprintf are handled inline in the evaluator
        // (evaluator.rs FunctionCall arm) because they need access
        // to self.globals.stdout / self.globals.stderr — the test
        // harness replaces those with SharedBuf captures and the
        // pure builtin path can't reach them. Keep them OUT of
        // call_builtin so the inline handler is the only code path.
        "word_wrap" => builtin_word_wrap(args),
        "word_wrap_w" => builtin_word_wrap_w(args),
        "markdown_escape" => builtin_markdown_escape(args),
        #[cfg(feature = "markdown")]
        "markdown" => builtin_markdown(args),
        // The registry lists `markdown` unconditionally (for help/listing), so a
        // build without the feature would otherwise fall through to the generic
        // `_ => Ok(None)` and silently return nil. Fail loudly instead.
        #[cfg(not(feature = "markdown"))]
        "markdown" => Err(MixError::RuntimeError {
            span: None,
            msg: "markdown() requires the `markdown` feature (pulldown-cmark)".to_string(),
        }),
        #[cfg(feature = "datastar")]
        "ds_patch_elements" => builtin_ds_patch_elements(args),
        #[cfg(feature = "datastar")]
        "ds_patch_signals" => builtin_ds_patch_signals(args),
        #[cfg(feature = "datastar")]
        "ds_sse" => builtin_ds_sse(args),
        // Registry lists the `ds_*` set unconditionally (for help/listing);
        // without the feature, fail loudly rather than fall through to the
        // generic `_ => Ok(None)` silent-nil (mirrors `markdown` above).
        #[cfg(not(feature = "datastar"))]
        "ds_patch_elements" | "ds_patch_signals" | "ds_sse" => Err(MixError::RuntimeError {
            span: None,
            msg: format!("{name}() requires the `datastar` feature"),
        }),
        "html_escape" => builtin_html_escape(args),
        "sanitize" => builtin_sanitize(args),
        "csv_parse" => builtin_csv_parse(args),
        "ini_parse" => builtin_ini_parse(args),
        #[cfg(feature = "xml")]
        "xml_parse" => builtin_xml_parse(args),
        // Registry lists `xml_parse` unconditionally (for help/listing);
        // without the feature, fail loudly rather than fall through to the
        // generic `_ => Ok(None)` silent-nil (mirrors `markdown` above).
        #[cfg(not(feature = "xml"))]
        "xml_parse" => Err(MixError::RuntimeError {
            span: None,
            msg: "xml_parse() requires the `xml` feature (quick-xml)".to_string(),
        }),
        #[cfg(feature = "url")]
        "url_parse" => builtin_url_parse(args),
        // url_decode/url_encode are hand-rolled percent-coding (no `url`
        // crate), so they are always available — a CMS handler in a
        // build without the `url` feature still needs form/query decode.
        "url_decode" => builtin_url_decode(args),
        "url_encode" => builtin_url_encode(args),
        "parse_query" => builtin_parse_urlencoded(args, "parse_query"),
        "parse_form" => builtin_parse_urlencoded(args, "parse_form"),
        #[cfg(feature = "crypto")]
        "base64_encode" => builtin_base64_encode(args),
        #[cfg(feature = "crypto")]
        "base64_decode" => builtin_base64_decode(args),
        #[cfg(feature = "crypto")]
        "hash_blake3" => builtin_hash_blake3(args),
        #[cfg(feature = "crypto")]
        "hash_sha256" => builtin_hash_sha256(args),
        #[cfg(feature = "crypto")]
        "hash_md5" => builtin_hash_md5(args),
        #[cfg(feature = "crypto")]
        "hash_sha1" => builtin_hash_sha1(args),
        #[cfg(feature = "crypto")]
        "hmac_sha256" => builtin_hmac_sha256(args),
        #[cfg(not(feature = "crypto"))]
        "hmac_sha256" => Err(MixError::RuntimeError {
            span: None,
            msg: "hmac_sha256() requires the `crypto` feature".to_string(),
        }),
        #[cfg(feature = "crypto")]
        "constant_time_eq" => builtin_constant_time_eq(args),
        #[cfg(not(feature = "crypto"))]
        "constant_time_eq" => Err(MixError::RuntimeError {
            span: None,
            msg: "constant_time_eq() requires the `crypto` feature".to_string(),
        }),
        #[cfg(feature = "crypto")]
        "hash_file" => builtin_hash_file(args),
        #[cfg(feature = "crypto")]
        "uuid" => builtin_uuid(args),
        #[cfg(feature = "dkim")]
        "dkim_keygen" => builtin_dkim_keygen(args),
        #[cfg(feature = "http")]
        "http_get" => builtin_http_get(args),
        #[cfg(feature = "http")]
        "http_post" => builtin_http_post(args),
        #[cfg(feature = "http")]
        "http_request" => builtin_http_request(args),
        "bytes_len" => builtin_bytes_len(args),
        "string_to_bytes" => builtin_string_to_bytes(args),
        "bytes_to_string" => builtin_bytes_to_string(args),
        "bytes_find" => builtin_bytes_find(args),
        "bytes_starts_with" => builtin_bytes_starts_with(args),
        "bytes_split" => builtin_bytes_split(args),
        "bytes_concat" => builtin_bytes_concat(args),
        "bytes_from" => builtin_bytes_from(args),
        "bytes_to_hex" => builtin_bytes_to_hex(args),
        "bytes_from_hex" => builtin_bytes_from_hex(args),
        "buffer" => builtin_buffer(args),
        "buffer_push" => builtin_buffer_push(args),
        "buffer_get" => builtin_buffer_get(args),
        "buffer_set" => builtin_buffer_set(args),
        "freeze" => builtin_freeze(args),
        "dns_lookup" => builtin_dns_lookup(args),
        #[cfg(feature = "sqlite")]
        "sqlopen" => builtin_sqlopen(args),
        #[cfg(feature = "sqlite")]
        "sqlexec" => builtin_sqlexec(args),
        #[cfg(feature = "sqlite")]
        "sqlclose" => builtin_sqlclose(args),
        "help" => builtin_help(args),
        _ => Ok(None), // Not a builtin
    }
}

/// Names registered in the builtin table but resolved by an early-return
/// branch in the evaluator (`if name == …`), never through `call_builtin`.
/// They are deliberately NOT in the `is_builtin` gate: the gate answers
/// "will `call_builtin` handle this name", and for these it will not.
/// Keep in sync with the stdio special-form arms in `evaluator.rs`.
pub const EVAL_SPECIAL_BUILTINS: &[&str] = &[
    "printf",
    "eprintf",
    "readline",
    "read_stdin",
    "read_stdin_bytes",
    "write_stdout",
    "write_stderr",
    "print_raw",
    "eprint_raw",
];

/// Membership gate the evaluator consults before dispatching to
/// `call_builtin`. Since 0.29.0 this is GENERATED from `BUILTIN_NAMES`
/// (minus the evaluator-special names) instead of the hand-maintained
/// `matches!` list it used to be — that list was the third touch point
/// that silently drifted whenever a new builtin landed.
pub fn is_builtin(name: &str) -> bool {
    use std::collections::HashSet;
    use std::sync::OnceLock;
    static GATE: OnceLock<HashSet<&str, crate::scope::FxBuildHasher>> = OnceLock::new();
    GATE.get_or_init(|| {
        BUILTIN_NAMES
            .iter()
            .copied()
            .filter(|n| !EVAL_SPECIAL_BUILTINS.contains(n))
            .collect()
    })
    .contains(name)
}

/// Capability classes for the builtin table — the axis a
/// [`CapabilityPolicy`](crate::evaluator::CapabilityPolicy) gates on.
///
/// Assigned by [`capability_category`]. `Pure` builtins (string/number/
/// in-memory data ops) carry no host authority and are always safe to
/// allow; the rest touch the filesystem, network, other processes, or
/// host/environment info. The taxonomy is provisional — embedders that
/// need finer control can match exact builtin names in their own
/// [`CapabilityPolicy`].
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum CapabilityClass {
    /// No host authority: string/number/collection/in-memory-data ops.
    Pure,
    /// Reads the filesystem (`read_file`, `glob`, `ls`, `exists`, …).
    FsRead,
    /// Mutates the filesystem (`write_file`, `mkdir`, `chmod`, …).
    FsWrite,
    /// Talks to the network (`http_*`, `ssh_run`, `dns_lookup`).
    Network,
    /// Controls other processes (`run`, `spawn`, `kill`, `exit`, …).
    Process,
    /// Reads host/environment info or stdin (`env`, `hostname`, `cwd`,
    /// `readline`, `read_stdin`, …).
    Env,
    /// Reads or writes a host-injected database via the
    /// [`DbHandler`](crate::evaluator::DbHandler) seam (`db_query`,
    /// `db_exec`). Distinct from `FsWrite` (the raw `sqlopen`/`sqlexec`
    /// file builtins) because the seam is a *scoped, mediated* store the
    /// embedder controls — an embedder grants `Db` to a CMS handler
    /// without granting raw filesystem writes.
    Db,
    /// Calls a host-injected JMAP endpoint via the
    /// [`JmapHandler`](crate::evaluator::JmapHandler) seam (`jmap`).
    /// Distinct from `Network` (the raw `http_*` builtins) because the
    /// seam is a *scoped, mediated* upstream the embedder controls — an
    /// embedder grants `Jmap` to a PIM handler without granting arbitrary
    /// outbound HTTP. The script never names a host; reach is only the
    /// embedder's configured upstream.
    Jmap,
    /// Calls a host-injected Bus verb under DELEGATED identity via the
    /// [`BusCallHandler`](crate::evaluator::BusCallHandler) seam
    /// (`bus_call`). Distinct from the bare `send`/`emit` broker forms
    /// (which a sandboxed handler never gets) because the seam is a
    /// *scoped, mediated* control-plane channel the embedder fully
    /// controls: the embedder bounds WHICH verbs are callable (a per-route
    /// exact-verb allowlist) and injects the delegation envelope (the
    /// authenticated actor, vhost, route) from trusted request state. The
    /// Mix script supplies ONLY the verb + args and can name no
    /// host/peer/actor — it cannot forge identity or reach an unlisted
    /// verb.
    Bus,
}

impl CapabilityClass {
    /// Stable kebab-case name for agent-facing introspection
    /// (`mix builtins --json`). Kept in lockstep with the enum variants;
    /// consumed by capability-probe tooling, so treat the strings as a
    /// wire surface.
    pub fn as_str(&self) -> &'static str {
        match self {
            CapabilityClass::Pure => "pure",
            CapabilityClass::FsRead => "fs-read",
            CapabilityClass::FsWrite => "fs-write",
            CapabilityClass::Network => "network",
            CapabilityClass::Process => "process",
            CapabilityClass::Env => "env",
            CapabilityClass::Db => "db",
            CapabilityClass::Jmap => "jmap",
            CapabilityClass::Bus => "bus",
        }
    }
}

/// Classify a builtin by the host authority it exercises. The class is
/// **declared at each builtin's table entry** (`BUILTINS` / `HOFS`, via
/// `builtin_table!`) — the single source of truth — so adding a builtin
/// without naming its class is a compile error, not a silent
/// `Pure`-default sandbox bypass (the former hand-maintained `match`
/// failed open). A name that is not a registered builtin returns `Pure`:
/// it carries no host authority of its own, and the `is_builtin` gate
/// rejects non-builtins before dispatch anyway.
///
/// (`panic` is classed `Process` in the table: it is a real Rust
/// `panic!`, uncatchable by Mix `try/catch`, a termination primitive a
/// sandbox must be able to deny — like `exit`.)
pub fn capability_category(name: &str) -> CapabilityClass {
    BUILTINS
        .iter()
        .chain(crate::builtins_hof::HOFS.iter())
        .find(|b| b.name == name)
        .map(|b| b.capability)
        .unwrap_or(CapabilityClass::Pure)
}

/// Conditional capabilities a builtin exercises only when a given
/// option is present (declared in the contract's `cond_caps[...]`, e.g.
/// `http_get`'s `ca_file` needs `fs-read` on top of `network`). The
/// evaluator consults this at dispatch when a `CapabilityPolicy` is
/// installed. Backed by a lazily-built index so the (rare) sandboxed
/// dispatch path stays O(1).
pub fn conditional_capabilities(name: &str) -> &'static [crate::builtin_info::CondCap] {
    builtin_info_of(name).map_or(&[], |b| b.contract.cond_caps)
}

/// Whether a conditional-capability `option` is ACTUALLY engaged by a
/// call's arguments — i.e. present in the map the builtin will really
/// consult, not merely in *some* map argument. For `http_*` that means
/// the resolved OPTS map (a `{ca_file: ...}` sitting in the headers or
/// body slot never reads a file, so it must not engage `fs-read` —
/// codex release review, MINOR). The trailing-map resolution lives
/// here beside the builtins that own it, behind a generic interface.
pub fn conditional_cap_engaged(name: &str, args: &[Value], option: &str) -> bool {
    let map_has = |v: Option<&Value>| matches!(v, Some(Value::Map(m)) if m.contains_key(option));
    #[cfg(feature = "http")]
    {
        // The opts slot is the last positional map, but a lone trailing
        // all-opt-keys map in the HEADERS slot is ALSO read as opts
        // (http_headers_and_timeout). Mirror that exactly per builtin.
        let opts_in_slots = |headers: Option<&Value>, opts: Option<&Value>| -> bool {
            if opts.is_none()
                && let Some(Value::Map(m)) = headers
                && http_map_is_opts(m)
            {
                return m.contains_key(option);
            }
            map_has(opts)
        };
        match name {
            "http_get" => return opts_in_slots(args.get(1), args.get(2)),
            "http_post" => return opts_in_slots(args.get(2), args.get(3)),
            "http_request" => {
                // 3-arg sole-trailing-opts special case.
                if args.len() == 3
                    && let Some(Value::Map(m)) = args.get(2)
                    && http_map_is_opts(m)
                {
                    return m.contains_key(option);
                }
                return opts_in_slots(args.get(3), args.get(4));
            }
            _ => {}
        }
    }
    // `name` selects a per-builtin resolver, and only http builtins have
    // one today — without the http feature the selector is legitimately
    // unread (the signature stays feature-stable for callers).
    #[cfg(not(feature = "http"))]
    let _ = name;
    // No option-carrying builtin outside http today; conservative
    // fallback (any map arg) for a future one added without a resolver.
    args.iter().any(|v| map_has(Some(v)))
}

/// O(1) lookup of a builtin's full metadata entry (builtins + HOFs) —
/// the machine side of the discovery surface, used by the strict-arity
/// gate, conditional capabilities, and `mix lint`'s arity checker.
pub fn builtin_info_of(name: &str) -> Option<&'static crate::builtin_info::BuiltinInfo> {
    use std::collections::HashMap;
    use std::sync::OnceLock;
    static INDEX: OnceLock<HashMap<&'static str, &'static crate::builtin_info::BuiltinInfo>> =
        OnceLock::new();
    INDEX
        .get_or_init(|| {
            BUILTINS
                .iter()
                .chain(crate::builtins_hof::HOFS.iter())
                .map(|b| (b.name, b))
                .collect()
        })
        .get(name)
        .copied()
}

/// A ready-made [`CapabilityPolicy`](crate::evaluator::CapabilityPolicy)
/// that allows [`CapabilityClass::Pure`] plus an explicit allow-set of
/// other classes, denying everything else. Convenient for embedders and
/// tests; a daemon that wants finer control writes its own policy.
#[derive(Clone, Debug, Default)]
pub struct CategoryAllowList {
    allowed: std::collections::HashSet<CapabilityClass>,
}

impl CategoryAllowList {
    /// Allow [`CapabilityClass::Pure`] plus the listed classes; deny the rest.
    pub fn new(allowed: &[CapabilityClass]) -> Self {
        CategoryAllowList {
            allowed: allowed.iter().copied().collect(),
        }
    }
}

impl crate::evaluator::CapabilityPolicy for CategoryAllowList {
    fn check_builtin(&self, name: &str) -> Result<(), String> {
        let class = capability_category(name);
        if class == CapabilityClass::Pure || self.allowed.contains(&class) {
            Ok(())
        } else {
            Err(format!("{name} requires {class:?} capability"))
        }
    }

    /// Gate shell-syntax execution (`sh`/`$()`/pipes → `/bin/sh`,
    /// classed as [`CapabilityClass::Process`]) against the same
    /// allow-set as builtins, rather than the trait's representative-name
    /// default.
    fn check_class(&self, class: CapabilityClass) -> Result<(), String> {
        if class == CapabilityClass::Pure || self.allowed.contains(&class) {
            Ok(())
        } else {
            Err(format!("{class:?} capability not allowed"))
        }
    }
}

fn expect_args(name: &str, args: &[Value], min: usize) -> MixResult<()> {
    if args.len() < min {
        Err(MixError::RuntimeError {
            span: None,
            msg: format!(
                "{}() expects at least {} argument(s), got {}",
                name,
                min,
                args.len()
            ),
        })
    } else {
        Ok(())
    }
}

/// Coerce a supplied builtin argument through Mix's normal numeric doorway.
/// Numeric strings and bools remain valid; only values `to_number()` cannot
/// represent fail. The 1-based argument position and value are included so a
/// caller can fix the exact input rather than debugging a plausible fallback.
fn required_number_value(context: &str, value: &Value) -> MixResult<f64> {
    extract_number(value, InputPolicy::StandardCoercion).ok_or_else(|| {
        let shown = match value {
            Value::String(s) => format!("{s:?}"),
            _ => value.to_mix_string(),
        };
        MixError::structured(
            "TYPE_MISMATCH",
            format!(
                "{context} must be a number, got {shown} ({})",
                value.type_name()
            ),
        )
    })
}

fn number_arg(name: &str, args: &[Value], index: usize) -> MixResult<f64> {
    required_number_value(&format!("{name}(): argument {}", index + 1), &args[index])
}

fn builtin_len(args: Vec<Value>) -> MixResult<Option<Value>> {
    expect_args("len", &args, 1)?;
    let n = match &args[0] {
        // P1 (char-aware strings): codepoint count, NOT bytes, so `length`
        // composes with codepoint `substr`/`reverse`/`$s[i]`. `byte_length`
        // is the byte escape hatch. List/map length unchanged (element count).
        // O(n) scan vs the old O(1) `s.len()` — accepted tradeoff.
        Value::String(s) => s.chars().count() as f64,
        Value::List(l) => l.len() as f64,
        Value::Map(m) => m.len() as f64,
        // bytes/buffer: the BYTE count (v0.64.0) — the same unit `$b[i]`
        // and `for each` use, and identical to `bytes_len`, which stays
        // as the strict spelling for code that wants a non-bytes argument
        // rejected rather than counted.
        Value::Bytes(b) => b.len() as f64,
        Value::Buffer(b) => b.borrow().len() as f64,
        _ => {
            return Err(MixError::RuntimeError {
                span: None,
                msg: format!("len() not supported for {}", args[0].type_name()),
            });
        }
    };
    Ok(Some(Value::Number(n)))
}

fn builtin_upper(args: Vec<Value>) -> MixResult<Option<Value>> {
    expect_args("upper", &args, 1)?;
    Ok(Some(Value::String(args[0].to_mix_string().to_uppercase())))
}

fn builtin_lower(args: Vec<Value>) -> MixResult<Option<Value>> {
    expect_args("lower", &args, 1)?;
    Ok(Some(Value::String(args[0].to_mix_string().to_lowercase())))
}

fn builtin_left(args: Vec<Value>) -> MixResult<Option<Value>> {
    expect_args("left", &args, 2)?;
    let s = args[0].to_mix_string();
    let n = number_arg("left", &args, 1)? as usize;
    Ok(Some(Value::String(s.chars().take(n).collect())))
}

fn builtin_right(args: Vec<Value>) -> MixResult<Option<Value>> {
    expect_args("right", &args, 2)?;
    let s = args[0].to_mix_string();
    let n = number_arg("right", &args, 1)? as usize;
    let chars: Vec<char> = s.chars().collect();
    let start = chars.len().saturating_sub(n);
    Ok(Some(Value::String(chars[start..].iter().collect())))
}

fn builtin_substr(args: Vec<Value>) -> MixResult<Option<Value>> {
    expect_args("substr", &args, 2)?;
    let s = args[0].to_mix_string();
    let chars: Vec<char> = s.chars().collect();
    // Clamp start FIRST so the default-len subtraction can't underflow,
    // and saturating_add so a huge `len` can't wrap `start + len` past
    // usize::MAX into a start>end slice panic (mirrors grapheme_substr).
    // Negative/NaN args saturate to 0 via the f64→usize cast.
    let start = (number_arg("substr", &args, 1)? as usize).min(chars.len());
    let len = if args.len() > 2 {
        number_arg("substr", &args, 2)? as usize
    } else {
        chars.len() - start
    };
    let end = start.saturating_add(len).min(chars.len());
    Ok(Some(Value::String(chars[start..end].iter().collect())))
}

fn builtin_pos(args: Vec<Value>) -> MixResult<Option<Value>> {
    expect_args("pos", &args, 2)?;
    let needle = args[0].to_mix_string();
    let haystack = args[1].to_mix_string();
    // P1: codepoint offset (1-based, 0=not found). `find` returns a byte index
    // at a char boundary; count the codepoints in the prefix before it.
    // `byte_pos` keeps the raw byte offset.
    let pos = haystack
        .find(&needle)
        .map(|b| haystack[..b].chars().count() + 1)
        .unwrap_or(0);
    Ok(Some(Value::Number(pos as f64)))
}

// --- Char-aware string ops -------------------------------------------------
// _doc/planned/2026-06-02-mix-char-aware-strings.md.
//   byte_*    — raw UTF-8 byte offsets/sizes. The escape hatch preserving the
//               PRE-0.8.0 semantics of len/pos/lastpos/string-index_of, which the
//               P1 flip turned codepoint-based (see those fns). byte_* match by
//               raw bytes via str::find/rfind on the original string.
//   grapheme_* — user-perceived characters (UAX #29): emoji ZWJ sequences, flags,
//               and combining marks each count/slice as ONE unit.
//   display_width — terminal cells (UAX #11), summed over the string.
// All coerce their input via `to_mix_string()`, matching pos/substr/left/right.

/// Byte length of a string. STRING-SEMANTIC: coerces via `to_mix_string` like its
/// `byte_pos` siblings (and like `pos`/`substr`), so it is NOT a type-aware twin of
/// `length` — `byte_length($list)` counts the bytes of the stringified list, it does
/// NOT return element count. For a string it equals the PRE-0.8.0 `length()` exactly
/// (`to_mix_string` is identity on a `String`) — the byte migration target now that
/// `length` itself is codepoint-based.
fn builtin_byte_length(args: Vec<Value>) -> MixResult<Option<Value>> {
    expect_args("byte_length", &args, 1)?;
    let s = args[0].to_mix_string();
    Ok(Some(Value::Number(s.len() as f64)))
}

/// Byte offset of `needle` in `haystack` — 1-based, 0 = not found (byte twin of `pos`).
fn builtin_byte_pos(args: Vec<Value>) -> MixResult<Option<Value>> {
    expect_args("byte_pos", &args, 2)?;
    let needle = args[0].to_mix_string();
    let haystack = args[1].to_mix_string();
    let pos = haystack.find(&needle).map(|p| p + 1).unwrap_or(0);
    Ok(Some(Value::Number(pos as f64)))
}

/// Last byte offset of `needle` in `haystack` — 1-based, 0 = not found (byte twin of `lastpos`).
fn builtin_byte_lastpos(args: Vec<Value>) -> MixResult<Option<Value>> {
    expect_args("byte_lastpos", &args, 2)?;
    let needle = args[0].to_mix_string();
    let haystack = args[1].to_mix_string();
    let pos = haystack.rfind(&needle).map(|p| p + 1).unwrap_or(0);
    Ok(Some(Value::Number(pos as f64)))
}

/// Byte offset of `needle` in `s` — 0-based, -1 = not found (byte twin of string `index_of`).
/// Arg order matches `index_of(haystack, needle)`, NOT `pos(needle, haystack)`.
fn builtin_byte_index_of(args: Vec<Value>) -> MixResult<Option<Value>> {
    expect_args("byte_index_of", &args, 2)?;
    let s = args[0].to_mix_string();
    let needle = args[1].to_mix_string();
    let pos = s.find(&needle).map(|p| p as f64).unwrap_or(-1.0);
    Ok(Some(Value::Number(pos)))
}

/// Count grapheme clusters (extended, UAX #29): emoji/flags/combining = 1 each.
fn builtin_grapheme_count(args: Vec<Value>) -> MixResult<Option<Value>> {
    expect_args("grapheme_count", &args, 1)?;
    let s = args[0].to_mix_string();
    Ok(Some(Value::Number(s.graphemes(true).count() as f64)))
}

/// Substring by grapheme position/length. Clamps like `substr` (start past the
/// end → empty; missing `$len` → to the end) but never splits a cluster.
fn builtin_grapheme_substr(args: Vec<Value>) -> MixResult<Option<Value>> {
    expect_args("grapheme_substr", &args, 2)?;
    let s = args[0].to_mix_string();
    let graphemes: Vec<&str> = s.graphemes(true).collect();
    // Clamp start FIRST so the default-len subtraction can't underflow.
    let start = (number_arg("grapheme_substr", &args, 1)? as usize).min(graphemes.len());
    let len = if args.len() > 2 {
        number_arg("grapheme_substr", &args, 2)? as usize
    } else {
        graphemes.len() - start
    };
    let end = start.saturating_add(len).min(graphemes.len());
    Ok(Some(Value::String(graphemes[start..end].concat())))
}

/// Reverse a string by grapheme cluster (emoji/combining-safe, unlike `reverse`).
fn builtin_grapheme_reverse(args: Vec<Value>) -> MixResult<Option<Value>> {
    expect_args("grapheme_reverse", &args, 1)?;
    let s = args[0].to_mix_string();
    Ok(Some(Value::String(s.graphemes(true).rev().collect())))
}

/// Terminal display width in cells (UAX #11). Uses `width` (East-Asian-ambiguous
/// = narrow/1); a future `display_width_cjk` could use `width_cjk` (= 2).
fn builtin_display_width(args: Vec<Value>) -> MixResult<Option<Value>> {
    expect_args("display_width", &args, 1)?;
    let s = args[0].to_mix_string();
    Ok(Some(Value::Number(
        UnicodeWidthStr::width(s.as_str()) as f64
    )))
}

fn builtin_strip(args: Vec<Value>) -> MixResult<Option<Value>> {
    expect_args("strip", &args, 1)?;
    let s = args[0].to_mix_string();
    // Optional charset (0.63.0): a SET of codepoints to strip from both
    // ends. Before this the second argument was accepted and silently
    // IGNORED — the exact no-op a PHP-style trim(s, chars) caller hits.
    let cs = args.get(1).map(|v| v.to_mix_string());
    Ok(Some(Value::String(charset_trim(&s, cs.as_deref(), true, true))))
}

// Exact-match string ops boundary (char-aware strings P2): `replace`, `contains`,
// `starts_with`, `ends_with`, `split`, `join`, `template` all match by EXACT UTF-8
// bytes — NO Unicode normalization and NO case-folding. So precomposed `é` (U+00E9)
// does not match decomposed `e`+`◌́` (U+0065 U+0301). Normalize first (a future
// `unicode-normalization` layer) if you need NFC-insensitive matching. These are
// match-only (no offset/length), so the codepoint flip didn't touch them.
fn builtin_replace(args: Vec<Value>) -> MixResult<Option<Value>> {
    expect_args("replace", &args, 3)?;
    let s = args[0].to_mix_string();
    let old = args[1].to_mix_string();
    let new = args[2].to_mix_string();
    Ok(Some(Value::String(s.replace(&old, &new))))
}

// --- Subject-first string helpers (0.63.0) -------------------------------
// Tier 1 contract: absent delimiter -> Nil (never "" and never the whole
// subject — "" is a REAL result, the delimiter at the edge); empty
// delimiter raises. Tier 2 contract: nothing to strip/replace -> subject
// unchanged. All matching is exact UTF-8 bytes like replace/contains.

/// Tier-1 shared guard: the delimiter must be non-empty.
fn nonempty_delim(name: &str, d: &str) -> MixResult<()> {
    if d.is_empty() {
        return Err(MixError::RuntimeError {
            span: None,
            msg: format!("{name}: empty delimiter (matching \"\" everywhere answers nothing — pass a real delimiter)"),
        });
    }
    Ok(())
}

fn builtin_before(args: Vec<Value>) -> MixResult<Option<Value>> {
    expect_args("before", &args, 2)?;
    let s = args[0].to_mix_string();
    let d = args[1].to_mix_string();
    nonempty_delim("before", &d)?;
    Ok(Some(match s.find(&d) {
        Some(b) => Value::String(s[..b].to_string()),
        None => Value::Nil,
    }))
}

fn builtin_after(args: Vec<Value>) -> MixResult<Option<Value>> {
    expect_args("after", &args, 2)?;
    let s = args[0].to_mix_string();
    let d = args[1].to_mix_string();
    nonempty_delim("after", &d)?;
    Ok(Some(match s.find(&d) {
        Some(b) => Value::String(s[b + d.len()..].to_string()),
        None => Value::Nil,
    }))
}

fn builtin_before_last(args: Vec<Value>) -> MixResult<Option<Value>> {
    expect_args("before_last", &args, 2)?;
    let s = args[0].to_mix_string();
    let d = args[1].to_mix_string();
    nonempty_delim("before_last", &d)?;
    Ok(Some(match s.rfind(&d) {
        Some(b) => Value::String(s[..b].to_string()),
        None => Value::Nil,
    }))
}

fn builtin_after_last(args: Vec<Value>) -> MixResult<Option<Value>> {
    expect_args("after_last", &args, 2)?;
    let s = args[0].to_mix_string();
    let d = args[1].to_mix_string();
    nonempty_delim("after_last", &d)?;
    Ok(Some(match s.rfind(&d) {
        Some(b) => Value::String(s[b + d.len()..].to_string()),
        None => Value::Nil,
    }))
}

fn builtin_split_once(args: Vec<Value>) -> MixResult<Option<Value>> {
    expect_args("split_once", &args, 2)?;
    let s = args[0].to_mix_string();
    let d = args[1].to_mix_string();
    nonempty_delim("split_once", &d)?;
    Ok(Some(match s.split_once(&d) {
        Some((head, tail)) => Value::list(vec![
            Value::String(head.to_string()),
            Value::String(tail.to_string()),
        ]),
        None => Value::Nil,
    }))
}

fn builtin_rsplit_once(args: Vec<Value>) -> MixResult<Option<Value>> {
    expect_args("rsplit_once", &args, 2)?;
    let s = args[0].to_mix_string();
    let d = args[1].to_mix_string();
    nonempty_delim("rsplit_once", &d)?;
    Ok(Some(match s.rsplit_once(&d) {
        Some((head, tail)) => Value::list(vec![
            Value::String(head.to_string()),
            Value::String(tail.to_string()),
        ]),
        None => Value::Nil,
    }))
}

fn builtin_between(args: Vec<Value>) -> MixResult<Option<Value>> {
    expect_args("between", &args, 3)?;
    let s = args[0].to_mix_string();
    let a = args[1].to_mix_string();
    let b = args[2].to_mix_string();
    nonempty_delim("between", &a)?;
    nonempty_delim("between", &b)?;
    let Some(start) = s.find(&a) else {
        return Ok(Some(Value::Nil));
    };
    let rest = &s[start + a.len()..];
    Ok(Some(match rest.find(&b) {
        Some(end) => Value::String(rest[..end].to_string()),
        None => Value::Nil,
    }))
}

fn builtin_strip_prefix(args: Vec<Value>) -> MixResult<Option<Value>> {
    expect_args("strip_prefix", &args, 2)?;
    let s = args[0].to_mix_string();
    let p = args[1].to_mix_string();
    // Empty prefix -> unchanged (strip_prefix("", ..) would also be a
    // no-op via str::strip_prefix, but make the contract explicit).
    Ok(Some(Value::String(match s.strip_prefix(&p) {
        Some(rest) if !p.is_empty() => rest.to_string(),
        _ => s,
    })))
}

fn builtin_strip_suffix(args: Vec<Value>) -> MixResult<Option<Value>> {
    expect_args("strip_suffix", &args, 2)?;
    let s = args[0].to_mix_string();
    let x = args[1].to_mix_string();
    Ok(Some(Value::String(match s.strip_suffix(&x) {
        Some(rest) if !x.is_empty() => rest.to_string(),
        _ => s,
    })))
}

fn builtin_replace_first(args: Vec<Value>) -> MixResult<Option<Value>> {
    expect_args("replace_first", &args, 3)?;
    let s = args[0].to_mix_string();
    let old = args[1].to_mix_string();
    let new = args[2].to_mix_string();
    // Empty `old` mirrors replace() (Rust str::replace semantics): one
    // insertion, at the start — siblings must not disagree.
    Ok(Some(Value::String(s.replacen(&old, &new, 1))))
}

fn builtin_count_of(args: Vec<Value>) -> MixResult<Option<Value>> {
    expect_args("count_of", &args, 2)?;
    let s = args[0].to_mix_string();
    let needle = args[1].to_mix_string();
    let n = if needle.is_empty() {
        0
    } else {
        s.matches(&needle).count()
    };
    Ok(Some(Value::Number(n as f64)))
}

/// Shared trim engine: default whitespace, or a charset treated as a SET
/// of codepoints (PHP-style). An empty charset is an empty set — strips
/// nothing.
fn charset_trim(s: &str, charset: Option<&str>, from_start: bool, from_end: bool) -> String {
    match charset {
        None => match (from_start, from_end) {
            (true, true) => s.trim(),
            (true, false) => s.trim_start(),
            _ => s.trim_end(),
        }
        .to_string(),
        Some(cs) => {
            let set: Vec<char> = cs.chars().collect();
            let pred = |c: char| set.contains(&c);
            let mut out = s;
            if from_start {
                out = out.trim_start_matches(pred);
            }
            if from_end {
                out = out.trim_end_matches(pred);
            }
            out.to_string()
        }
    }
}

fn builtin_ltrim(args: Vec<Value>) -> MixResult<Option<Value>> {
    expect_args("ltrim", &args, 1)?;
    let s = args[0].to_mix_string();
    let cs = args.get(1).map(|v| v.to_mix_string());
    Ok(Some(Value::String(charset_trim(&s, cs.as_deref(), true, false))))
}

fn builtin_rtrim(args: Vec<Value>) -> MixResult<Option<Value>> {
    expect_args("rtrim", &args, 1)?;
    let s = args[0].to_mix_string();
    let cs = args.get(1).map(|v| v.to_mix_string());
    Ok(Some(Value::String(charset_trim(&s, cs.as_deref(), false, true))))
}

fn builtin_lines(args: Vec<Value>) -> MixResult<Option<Value>> {
    expect_args("lines", &args, 1)?;
    let s = args[0].to_mix_string();
    if s.is_empty() {
        return Ok(Some(Value::list(Vec::new())));
    }
    let mut parts: Vec<&str> = s.split('\n').collect();
    // Exactly ONE trailing empty element dropped — the final newline is a
    // terminator, not the start of an empty line ("a\n\n" keeps its real
    // empty line: ["a", ""]).
    if parts.last() == Some(&"") {
        parts.pop();
    }
    let out: Vec<Value> = parts
        .into_iter()
        .map(|l| Value::String(l.strip_suffix('\r').unwrap_or(l).to_string()))
        .collect();
    Ok(Some(Value::list(out)))
}

fn builtin_fields(args: Vec<Value>) -> MixResult<Option<Value>> {
    expect_args("fields", &args, 1)?;
    let s = args[0].to_mix_string();
    let out: Vec<Value> = s
        .split_whitespace()
        .map(|f| Value::String(f.to_string()))
        .collect();
    Ok(Some(Value::list(out)))
}

fn builtin_chars(args: Vec<Value>) -> MixResult<Option<Value>> {
    expect_args("chars", &args, 1)?;
    let s = args[0].to_mix_string();
    let out: Vec<Value> = s.chars().map(|c| Value::String(c.to_string())).collect();
    Ok(Some(Value::list(out)))
}

fn builtin_last_index_of(args: Vec<Value>) -> MixResult<Option<Value>> {
    expect_args("last_index_of", &args, 2)?;
    match &args[0] {
        Value::List(items) => {
            let pos = items.iter().rposition(|v| v == &args[1]);
            Ok(Some(Value::Number(pos.map(|p| p as f64).unwrap_or(-1.0))))
        }
        Value::String(s) => {
            // 0-based codepoint index of the LAST occurrence — the twin
            // of index_of, via rfind.
            let needle = args[1].to_mix_string();
            let pos = s
                .rfind(&needle)
                .map(|b| s[..b].chars().count() as f64)
                .unwrap_or(-1.0);
            Ok(Some(Value::Number(pos)))
        }
        _ => Err(MixError::RuntimeError {
            span: None,
            msg: "last_index_of() expects a list or string".to_string(),
        }),
    }
}

/// Structural equality: the answer `==` cannot give for maps/lists (Value's
/// PartialEq is always false there). Maps: same key set, values deep_eq,
/// insertion order IGNORED. Lists: elementwise, order-sensitive. Scalars:
/// Value's own PartialEq (including the Number/String coercion, so
/// deep_eq agrees with == wherever == already works) — which means a
/// FUNCTION value is never equal, even to itself (`==` on functions is
/// deliberately false), and Buffer-vs-Bytes is false (freeze first).
/// Both documented in the table row and pinned by tests.
///
/// Depth-capped: this is the one builtin whose whole job is "traverse
/// everything", so adversarial nesting gets a catchable error instead of
/// a stack abort (GLM review of 4caec4e1, MINOR 1). 512 is far beyond
/// legitimate data and small enough that the guard itself fits a 2 MiB
/// test-thread stack — 4096 overflowed exactly where the pin ran.
const DEEP_EQ_MAX_DEPTH: usize = 512;

fn deep_eq_values(a: &Value, b: &Value, depth: usize) -> MixResult<bool> {
    if depth > DEEP_EQ_MAX_DEPTH {
        return Err(MixError::RuntimeError {
            span: None,
            msg: format!("deep_eq: nesting deeper than {DEEP_EQ_MAX_DEPTH} levels"),
        });
    }
    Ok(match (a, b) {
        (Value::List(la), Value::List(lb)) => {
            if la.len() != lb.len() {
                return Ok(false);
            }
            for (x, y) in la.iter().zip(lb.iter()) {
                if !deep_eq_values(x, y, depth + 1)? {
                    return Ok(false);
                }
            }
            true
        }
        (Value::Map(ma), Value::Map(mb)) => {
            if ma.len() != mb.len() {
                return Ok(false);
            }
            for (k, va) in ma.iter() {
                match mb.get(k) {
                    Some(vb) if deep_eq_values(va, vb, depth + 1)? => {}
                    _ => return Ok(false),
                }
            }
            true
        }
        _ => a == b,
    })
}

fn builtin_deep_eq(args: Vec<Value>) -> MixResult<Option<Value>> {
    expect_args("deep_eq", &args, 2)?;
    Ok(Some(Value::Bool(deep_eq_values(&args[0], &args[1], 0)?)))
}

fn builtin_split(args: Vec<Value>) -> MixResult<Option<Value>> {
    expect_args("split", &args, 1)?;
    let s = args[0].to_mix_string();
    let delim = if args.len() > 1 {
        args[1].to_mix_string()
    } else {
        " ".to_string()
    };
    let parts: Vec<Value> = s
        .split(&delim)
        .map(|p| Value::String(p.to_string()))
        .collect();
    Ok(Some(Value::list(parts)))
}

fn builtin_join(args: Vec<Value>) -> MixResult<Option<Value>> {
    expect_args("join", &args, 1)?;
    match &args[0] {
        Value::List(items) => {
            let delim = if args.len() > 1 {
                args[1].to_mix_string()
            } else {
                " ".to_string()
            };
            let parts: Vec<String> = items.iter().map(|v| v.to_mix_string()).collect();
            Ok(Some(Value::String(parts.join(&delim))))
        }
        _ => Err(MixError::RuntimeError {
            span: None,
            msg: "join() expects a list".to_string(),
        }),
    }
}

fn builtin_starts_with(args: Vec<Value>) -> MixResult<Option<Value>> {
    expect_args("starts_with", &args, 2)?;
    let s = args[0].to_mix_string();
    let prefix = args[1].to_mix_string();
    Ok(Some(Value::Bool(s.starts_with(&prefix))))
}

fn builtin_ends_with(args: Vec<Value>) -> MixResult<Option<Value>> {
    expect_args("ends_with", &args, 2)?;
    let s = args[0].to_mix_string();
    let suffix = args[1].to_mix_string();
    Ok(Some(Value::Bool(s.ends_with(&suffix))))
}

fn builtin_contains(args: Vec<Value>) -> MixResult<Option<Value>> {
    expect_args("contains", &args, 2)?;
    match &args[0] {
        Value::String(s) => {
            let sub = args[1].to_mix_string();
            Ok(Some(Value::Bool(s.contains(&sub))))
        }
        Value::List(items) => Ok(Some(Value::Bool(items.contains(&args[1])))),
        _ => Err(MixError::RuntimeError {
            span: None,
            msg: "contains() expects a string or list".to_string(),
        }),
    }
}

// Hard cap on a builtin-constructed string (repeat/lpad/rpad and the
// _w twins): 256 MiB. A huge count/width arg must return a normal Mix
// runtime error, not a capacity-overflow panic or daemon OOM.
const MAX_BUILT_STRING_BYTES: usize = 268_435_456;

/// Shared over-cap error for the string-building builtins.
fn built_string_cap_err(name: &str) -> MixError {
    MixError::RuntimeError {
        span: None,
        msg: format!(
            "{}() result would exceed {} bytes (256 MiB cap)",
            name, MAX_BUILT_STRING_BYTES
        ),
    }
}

fn builtin_repeat(args: Vec<Value>) -> MixResult<Option<Value>> {
    expect_args("repeat", &args, 2)?;
    let s = args[0].to_mix_string();
    // Negative/NaN counts saturate to 0 via the f64→usize cast; a huge
    // count saturates to usize::MAX and is caught by checked_mul.
    let n = number_arg("repeat", &args, 1)? as usize;
    match s.len().checked_mul(n) {
        Some(total) if total <= MAX_BUILT_STRING_BYTES => Ok(Some(Value::String(s.repeat(n)))),
        _ => Err(built_string_cap_err("repeat")),
    }
}

// Resolve the optional fill argument for lpad/rpad. Defaults to a space.
// Must be exactly ONE codepoint: a multi-char fill cannot pad to an exact
// width without either overshooting or truncating mid-fill, so reject it
// rather than silently producing a mis-aligned column.
fn pad_fill(name: &str, args: &[Value]) -> MixResult<char> {
    if args.len() < 3 {
        return Ok(' ');
    }
    let f = args[2].to_mix_string();
    let mut it = f.chars();
    match (it.next(), it.next()) {
        (Some(c), None) => Ok(c),
        _ => Err(MixError::RuntimeError {
            span: None,
            msg: format!(
                "{}: fill must be exactly one character, got {:?} ({} chars)",
                name,
                f,
                f.chars().count()
            ),
        }),
    }
}

// Pad `s` to `width` codepoints with `fill`. Saturating: never truncates when
// the content is already at or beyond `width`, matching the previous behaviour.
//
// The cap is enforced on the OUTPUT BYTES the call would actually build --
// pad chars times the fill's UTF-8 width, plus the original string. The old
// width-only guard in the callers was exact for ASCII fills but off by up to
// 4x for multibyte ones: lpad("", 200_000_000, "{1F980}") passed the 256 MiB
// width check and then built an 800 MB string.
fn pad_with(name: &str, s: &str, width: usize, fill: char, left: bool) -> MixResult<String> {
    let have = s.chars().count();
    if have >= width {
        return Ok(s.to_string());
    }
    let total_bytes = (width - have)
        .checked_mul(fill.len_utf8())
        .and_then(|p| p.checked_add(s.len()));
    match total_bytes {
        Some(t) if t <= MAX_BUILT_STRING_BYTES => {}
        _ => return Err(built_string_cap_err(name)),
    }
    let pad: String = std::iter::repeat_n(fill, width - have).collect();
    Ok(if left {
        pad + s
    } else {
        let mut out = s.to_string();
        out.push_str(&pad);
        out
    })
}

fn builtin_lpad(args: Vec<Value>) -> MixResult<Option<Value>> {
    expect_args_between("lpad", &args, 2, 3)?;
    let s = args[0].to_mix_string();
    let width = number_arg("lpad", &args, 1)? as usize;
    if width > MAX_BUILT_STRING_BYTES {
        return Err(built_string_cap_err("lpad"));
    }
    let fill = pad_fill("lpad", &args)?;
    Ok(Some(Value::String(pad_with(
        "lpad", &s, width, fill, true,
    )?)))
}

fn builtin_rpad(args: Vec<Value>) -> MixResult<Option<Value>> {
    expect_args_between("rpad", &args, 2, 3)?;
    let s = args[0].to_mix_string();
    let width = number_arg("rpad", &args, 1)? as usize;
    if width > MAX_BUILT_STRING_BYTES {
        return Err(built_string_cap_err("rpad"));
    }
    let fill = pad_fill("rpad", &args)?;
    Ok(Some(Value::String(pad_with(
        "rpad", &s, width, fill, false,
    )?)))
}

// Display-width padding (P2, char-aware strings): pad to `width` TERMINAL CELLS
// (UAX #11), not codepoints — so a CJK/emoji column lines up. `lpad`/`rpad` use
// Rust `format!` width = scalar count (a CJK glyph counts as 1, misaligning a
// table by one cell per wide char); `lpad_w`/`rpad_w` use the rendered width.
// Space-padded (a space is 1 cell) like lpad/rpad; never truncates when the
// content is already wider than `width` (saturating, same as lpad/rpad).
// The `_w` variants take the same optional fill as lpad/rpad, but the fill must
// itself be exactly 1 display cell — a CJK glyph or emoji fill would overshoot
// the target by one cell per pad character, defeating the point of padding by
// display width.
fn pad_fill_w(name: &str, args: &[Value]) -> MixResult<char> {
    let c = pad_fill(name, args)?;
    let cells = UnicodeWidthChar::width(c).unwrap_or(0);
    if cells != 1 {
        return Err(MixError::RuntimeError {
            span: None,
            msg: format!(
                "{}: fill must be exactly 1 display cell wide, got {:?} ({} cells)",
                name, c, cells
            ),
        });
    }
    Ok(c)
}

fn builtin_lpad_w(args: Vec<Value>) -> MixResult<Option<Value>> {
    expect_args_between("lpad_w", &args, 2, 3)?;
    let s = args[0].to_mix_string();
    let width = number_arg("lpad_w", &args, 1)? as usize;
    if width > MAX_BUILT_STRING_BYTES {
        return Err(built_string_cap_err("lpad_w"));
    }
    let fill = pad_fill_w("lpad_w", &args)?;
    let pad = width.saturating_sub(UnicodeWidthStr::width(s.as_str()));
    // Cap the OUTPUT BYTES, not the cell width: a 1-cell fill may still be
    // multibyte UTF-8, so the width guard alone under-counts by up to 4x.
    match pad
        .checked_mul(fill.len_utf8())
        .and_then(|p| p.checked_add(s.len()))
    {
        Some(t) if t <= MAX_BUILT_STRING_BYTES => {}
        _ => return Err(built_string_cap_err("lpad_w")),
    }
    Ok(Some(Value::String(format!(
        "{}{}",
        fill.to_string().repeat(pad),
        s
    ))))
}

fn builtin_rpad_w(args: Vec<Value>) -> MixResult<Option<Value>> {
    expect_args_between("rpad_w", &args, 2, 3)?;
    let s = args[0].to_mix_string();
    let width = number_arg("rpad_w", &args, 1)? as usize;
    if width > MAX_BUILT_STRING_BYTES {
        return Err(built_string_cap_err("rpad_w"));
    }
    let fill = pad_fill_w("rpad_w", &args)?;
    let pad = width.saturating_sub(UnicodeWidthStr::width(s.as_str()));
    // Cap the OUTPUT BYTES, not the cell width: a 1-cell fill may still be
    // multibyte UTF-8, so the width guard alone under-counts by up to 4x.
    match pad
        .checked_mul(fill.len_utf8())
        .and_then(|p| p.checked_add(s.len()))
    {
        Some(t) if t <= MAX_BUILT_STRING_BYTES => {}
        _ => return Err(built_string_cap_err("rpad_w")),
    }
    Ok(Some(Value::String(format!(
        "{}{}",
        s,
        fill.to_string().repeat(pad)
    ))))
}

fn builtin_reverse(args: Vec<Value>) -> MixResult<Option<Value>> {
    expect_args("reverse", &args, 1)?;
    match &args[0] {
        Value::String(s) => Ok(Some(Value::String(s.chars().rev().collect()))),
        Value::List(items) => {
            // CoW: shallow one-level copy (element Rc bumps) — the result
            // is a fresh Value; cheaper than the old deep clone.
            let mut reversed = items.to_vec();
            reversed.reverse();
            Ok(Some(Value::list(reversed)))
        }
        _ => Err(MixError::RuntimeError {
            span: None,
            msg: "reverse() expects a string or list".to_string(),
        }),
    }
}

fn builtin_words(args: Vec<Value>) -> MixResult<Option<Value>> {
    expect_args("words", &args, 1)?;
    let s = args[0].to_mix_string();
    let count = s.split_whitespace().count();
    Ok(Some(Value::Number(count as f64)))
}

fn builtin_word(args: Vec<Value>) -> MixResult<Option<Value>> {
    expect_args("word", &args, 2)?;
    let s = args[0].to_mix_string();
    let n = number_arg("word", &args, 1)? as usize;
    let w: Vec<&str> = s.split_whitespace().collect();
    if n >= 1 && n <= w.len() {
        Ok(Some(Value::String(w[n - 1].to_string())))
    } else {
        Ok(Some(Value::Nil))
    }
}

fn builtin_type(args: Vec<Value>) -> MixResult<Option<Value>> {
    expect_args("type", &args, 1)?;
    Ok(Some(Value::String(args[0].type_name().to_string())))
}

fn builtin_to_number(args: Vec<Value>) -> MixResult<Option<Value>> {
    expect_args("to_number", &args, 1)?;
    match args[0].to_number() {
        Some(n) => Ok(Some(Value::Number(n))),
        None => Ok(Some(Value::Nil)),
    }
}

fn builtin_to_string(args: Vec<Value>) -> MixResult<Option<Value>> {
    expect_args("to_string", &args, 1)?;
    Ok(Some(Value::String(args[0].to_mix_string())))
}

fn builtin_is_number(args: Vec<Value>) -> MixResult<Option<Value>> {
    expect_args("is_number", &args, 1)?;
    // The string case delegates to Value::to_number — the single source of
    // truth for "is this a numeric string" (trims, and rejects the
    // "inf"/"nan"/"1e999" spellings the raw f64 parser would accept).
    // Bools stay non-numbers here even though to_number coerces them.
    let result = matches!(&args[0], Value::Number(_))
        || (matches!(&args[0], Value::String(_)) && args[0].to_number().is_some());
    Ok(Some(Value::Bool(result)))
}

fn builtin_is_empty(args: Vec<Value>) -> MixResult<Option<Value>> {
    expect_args("is_empty", &args, 1)?;
    // `_` absorbs Number/Bool/Function — scalars and callables are
    // never "empty"; use `== 0` for numeric zero, `not $x` for booleans.
    let result = match &args[0] {
        Value::String(s) => s.is_empty(),
        Value::Bytes(b) => b.is_empty(),
        Value::Buffer(b) => b.borrow().is_empty(),
        Value::List(l) => l.is_empty(),
        Value::Map(m) => m.is_empty(),
        Value::Nil => true,
        _ => false,
    };
    Ok(Some(Value::Bool(result)))
}

// ----- Math (v0.19.0) -------------------------------------------------------
//
// Pure numeric primitives over f64. Each coerces its arguments the way the
// rest of Mix does (`to_number`: numbers, numeric strings, bools) and raises
// a RuntimeError on a genuinely non-numeric argument. Following IEEE-754,
// out-of-domain results propagate as NaN / ±inf rather than erroring
// (`sqrt(-1)` → NaN, `ln(0)` → -inf); Mix prints those as "NaN"/"inf"/"-inf".
// `min`/`max`/`abs`/`clamp` replace the former prelude shims — `min`/`max`
// mirror the `<`/`>` operator ordering so they remain a strict superset.

/// Coerce one argument to f64, or raise a clear "expects a number" error
/// naming the offending type. Shared by every math builtin.
fn num_arg(name: &str, v: &Value) -> MixResult<f64> {
    extract_number(v, InputPolicy::StandardCoercion).ok_or_else(|| MixError::RuntimeError {
        span: None,
        msg: format!("{}() expects a number, got {}", name, v.type_name()),
    })
}

/// A 1-argument math builtin: coerce arg 0 and apply `f` (sqrt, sin, …).
fn builtin_unary_math(name: &str, args: Vec<Value>, f: fn(f64) -> f64) -> MixResult<Option<Value>> {
    expect_args(name, &args, 1)?;
    Ok(Some(Value::Number(f(num_arg(name, &args[0])?))))
}

/// A 2-argument math builtin. Arg order follows the underlying f64 method's
/// receiver-first convention: pow(base, exp), log(x, base), atan2(y, x),
/// hypot(x, y).
fn builtin_binary_math(
    name: &str,
    args: Vec<Value>,
    f: fn(f64, f64) -> f64,
) -> MixResult<Option<Value>> {
    expect_args(name, &args, 2)?;
    let a = num_arg(name, &args[0])?;
    let b = num_arg(name, &args[1])?;
    Ok(Some(Value::Number(f(a, b))))
}

/// Shared core of round/floor/ceil/trunc with an optional decimal-places
/// argument. Without a 2nd arg it rounds to a whole number; `round(x, n)`
/// rounds to `n` decimal places (negative `n` rounds to tens/hundreds/…).
fn builtin_round_family(
    name: &str,
    args: Vec<Value>,
    op: fn(f64) -> f64,
) -> MixResult<Option<Value>> {
    expect_args(name, &args, 1)?;
    let x = num_arg(name, &args[0])?;
    let nd = if args.len() > 1 {
        let n = num_arg(name, &args[1])?;
        // A NaN/inf "number of decimal places" is meaningless — surface it
        // rather than silently coercing it to 0 places (which would round to
        // an integer and hide the caller's bad input).
        as_finite_number(&format!("{name}(): argument 2"), n)?
    } else {
        0.0
    };
    Ok(Some(Value::Number(round_to(x, nd, op))))
}

/// Apply `op` (round/floor/ceil/trunc) to `x` at `nd` decimal places. `nd` is
/// finite (the caller rejects NaN/inf). Two regimes keep the result correct
/// across the whole f64 magnitude range while never producing a spurious
/// NaN/inf.
///
/// For `nd > 0` (round to a fraction) it scales up by `10^nd`, rounds, and
/// scales back; if that scale-up overflows then `x` is already coarser than
/// `10^-nd` (no representable fraction at that precision), so it is returned
/// unchanged. For `nd < 0` (round to tens/hundreds/…) it DIVIDES by `10^(-nd)`,
/// rounds, then multiplies back — dividing (factor ≥ 10) avoids the
/// underflow-to-zero, and resulting `0/0` = NaN, that multiplying by a tiny
/// `10^nd` would hit for coarse rounding (the bug a naïve `x * 10^nd` has at,
/// e.g., `round(5e19, -20)`).
///
/// `nd` is clamped to ±308 (10^308 is the largest finite power of ten; beyond
/// it the scale factor itself is non-finite). A non-finite `x` has no fraction
/// to round and is returned unchanged.
fn round_to(x: f64, nd: f64, op: fn(f64) -> f64) -> f64 {
    if !x.is_finite() {
        return x;
    }
    // `as i32` saturates a huge finite nd to i32::MIN/MAX before the clamp.
    let nd = (nd.trunc() as i32).clamp(-308, 308);
    if nd == 0 {
        return op(x);
    }
    if nd > 0 {
        let factor = 10f64.powi(nd);
        let scaled = x * factor;
        if !scaled.is_finite() {
            return x;
        }
        op(scaled) / factor
    } else {
        let divisor = 10f64.powi(-nd); // -nd ∈ 1..=308 ⇒ finite, ≥ 10
        let result = op(x / divisor) * divisor;
        if !result.is_finite() {
            // Rounding up overflowed near f64::MAX — the integer rounding of x
            // is the best finite answer available.
            return op(x);
        }
        result
    }
}

/// The exact-integer domain every bitwise builtin shares.
///
/// Mix numbers are `f64`, so a bitwise operation has to say what it does with a
/// value that is not an integer, or is one too large to have survived the trip
/// into an `f64` intact. It refuses both. The ceiling is 2^53-1 rather than
/// `i64::MAX` because past that point `f64` cannot distinguish adjacent
/// integers: accepting them would mean the argument the caller wrote and the
/// bits actually operated on are different numbers, which is precisely the
/// class of silent wrongness this family exists to let callers avoid.
const BITWISE_EXACT_MAX: f64 = 9_007_199_254_740_991.0; // 2^53 - 1
fn bitwise_int_arg(name: &str, v: &Value) -> MixResult<i64> {
    let x = num_arg(name, v)?;
    as_exact_integer(
        &format!("{name}(): numeric argument"),
        x,
        -(BITWISE_EXACT_MAX as i64),
        BITWISE_EXACT_MAX as i64,
    )
}

/// `i64` back to a Mix number, refusing anything the round trip would corrupt.
/// Only `bshl` can reach this, and only by shifting bits off the top.
// The result is taken as i128 so that a value which overflowed i64 is still
// the TRUE value when the range check below reads it. `bshl` originally
// wrapped in i64 and then checked the wrapped result, which is no check at
// all: 2^52 shifted left 12 is 2^64, wraps to 0, and 0 is comfortably inside
// the range, so the builtin answered `0` to a question whose honest answer is
// "that does not fit". Cold review round 32 found it. Everything else here
// (and/or/xor/not) cannot overflow i64, so widening costs them nothing.
fn bitwise_result(name: &str, r: i128) -> MixResult<Option<Value>> {
    if r.unsigned_abs() > BITWISE_EXACT_MAX as u128 {
        as_exact_integer(
            &format!("{name}(): result"),
            r as f64,
            -(BITWISE_EXACT_MAX as i64),
            BITWISE_EXACT_MAX as i64,
        )?;
        unreachable!("out-of-range bitwise result passed exact-integer validation");
    }
    Ok(Some(Value::Number(r as f64)))
}

fn builtin_bitwise(
    name: &str,
    args: Vec<Value>,
    op: fn(i64, i64) -> i64,
) -> MixResult<Option<Value>> {
    expect_args_between(name, &args, 2, 2)?;
    let a = bitwise_int_arg(name, &args[0])?;
    let b = bitwise_int_arg(name, &args[1])?;
    bitwise_result(name, op(a, b) as i128)
}

fn builtin_bnot(args: Vec<Value>) -> MixResult<Option<Value>> {
    expect_args_between("bnot", &args, 1, 1)?;
    bitwise_result("bnot", !bitwise_int_arg("bnot", &args[0])? as i128)
}

fn builtin_bshift(name: &str, args: Vec<Value>) -> MixResult<Option<Value>> {
    expect_args_between(name, &args, 2, 2)?;
    let x = bitwise_int_arg(name, &args[0])?;
    let n = bitwise_int_arg(name, &args[1])?;
    if !(0..=63).contains(&n) {
        as_exact_integer(&format!("{name}(): argument 2"), n as f64, 0, 63)?;
        unreachable!("out-of-range shift passed exact-integer validation");
    }
    // In i128, so a left shift past i64 keeps its true value for the range
    // check rather than wrapping into an innocent-looking small number. The
    // widest case is (2^53 - 1) << 63, far inside i128. A right shift is
    // arithmetic and cannot overflow, but shares the path.
    let r = if name == "bshl" {
        (x as i128) << n
    } else {
        (x as i128) >> n
    };
    bitwise_result(name, r)
}

fn builtin_sign(args: Vec<Value>) -> MixResult<Option<Value>> {
    expect_args("sign", &args, 1)?;
    let x = num_arg("sign", &args[0])?;
    // Not f64::signum: that returns ±1 for ±0.0 and NaN for NaN. We want 0
    // for either signed zero, and NaN passed through unchanged.
    let s = if x.is_nan() {
        f64::NAN
    } else if x > 0.0 {
        1.0
    } else if x < 0.0 {
        -1.0
    } else {
        0.0
    };
    Ok(Some(Value::Number(s)))
}

fn builtin_clamp(args: Vec<Value>) -> MixResult<Option<Value>> {
    expect_args("clamp", &args, 3)?;
    let x = num_arg("clamp", &args[0])?;
    let lo = num_arg("clamp", &args[1])?;
    let hi = num_arg("clamp", &args[2])?;
    // A NaN bound is a malformed range (it can't order anything), and the
    // `lo > hi` guard can't catch it — `NaN > _` is always false — so it would
    // otherwise slip through and silently mask an invalid range. Reject it
    // here. ±inf bounds are legitimate (clamp(x, 0, inf) = max(x, 0)).
    if lo.is_nan() || hi.is_nan() {
        return Err(MixError::RuntimeError {
            span: None,
            msg: "clamp() bounds must be numbers, not NaN".to_string(),
        });
    }
    if lo > hi {
        return Err(MixError::RuntimeError {
            span: None,
            msg: format!("clamp() lower bound {} exceeds upper bound {}", lo, hi),
        });
    }
    // Manual, not f64::clamp — that panics on a NaN bound. With NaN bounds
    // rejected and lo <= hi checked, a NaN *x* falls through both comparisons
    // and is returned as-is (value propagation, matching the other math fns).
    let r = if x < lo {
        lo
    } else if x > hi {
        hi
    } else {
        x
    };
    Ok(Some(Value::Number(r)))
}

fn builtin_min(args: Vec<Value>) -> MixResult<Option<Value>> {
    min_max("min", args, true)
}

fn builtin_max(args: Vec<Value>) -> MixResult<Option<Value>> {
    min_max("max", args, false)
}

/// `min`/`max` over the scalar arguments, or over a single list argument.
/// Mirrors the `<`/`>` operator ordering: numeric when every value coerces to
/// a number (NaN-skipping, like f64::min/max), else lexicographic when every
/// value is a string. A genuinely mixed/incomparable set errors. It preserves
/// the value/ordering the prelude `min`/`max($a, $b)` shim selected, but
/// normalizes a numeric result to a number — a numeric-string arg like "5"
/// returns the number 5, not the string "5" (the all-numeric path coerces via
/// to_number); the all-string path returns the original string unchanged.
fn min_max(name: &str, args: Vec<Value>, want_min: bool) -> MixResult<Option<Value>> {
    expect_args(name, &args, 1)?;
    // A lone list argument operates on its elements: max([1,2,3]) == max(1,2,3).
    let items: &[Value] = match (args.len(), args.first()) {
        (1, Some(Value::List(l))) => {
            if l.is_empty() {
                return Err(MixError::RuntimeError {
                    span: None,
                    msg: format!("{}() of an empty list", name),
                });
            }
            l.as_slice()
        }
        _ => args.as_slice(),
    };

    // All-numeric path (the common case). f64::min/max return the non-NaN
    // operand, so a stray NaN is skipped; an all-NaN set stays NaN.
    if items.iter().all(|v| v.to_number().is_some()) {
        let mut acc = items[0].to_number().unwrap();
        for v in &items[1..] {
            let n = v.to_number().unwrap();
            acc = if want_min { acc.min(n) } else { acc.max(n) };
        }
        return Ok(Some(Value::Number(acc)));
    }

    // All-string path → lexicographic by codepoint, matching `<`/`>` on strings.
    if items.iter().all(|v| matches!(v, Value::String(_))) {
        let mut best = match &items[0] {
            Value::String(s) => s,
            _ => unreachable!("every item checked to be a string"),
        };
        for v in &items[1..] {
            if let Value::String(s) = v {
                let take = if want_min { s < best } else { s > best };
                if take {
                    best = s;
                }
            }
        }
        return Ok(Some(Value::String(best.clone())));
    }

    Err(MixError::RuntimeError {
        span: None,
        msg: format!(
            "{}() needs all-numeric or all-string arguments (cannot compare a mix)",
            name
        ),
    })
}

fn builtin_pi(args: Vec<Value>) -> MixResult<Option<Value>> {
    expect_args("pi", &args, 0)?;
    Ok(Some(Value::Number(std::f64::consts::PI)))
}

fn builtin_e(args: Vec<Value>) -> MixResult<Option<Value>> {
    expect_args("e", &args, 0)?;
    Ok(Some(Value::Number(std::f64::consts::E)))
}

/// Validate a `random(min, max)` bound: a whole number within the f64
/// exact-integer range (±2^53). Mix numbers are f64, so beyond 2^53 not every
/// integer is representable — a wider bound would let `random_range` draw an
/// i64 that aliases to a *different* Mix number on the `as f64` return (a
/// silently biased, lossy result). Bounding at ±2^53 keeps the draw exact both
/// ways, and 2^53 is comfortably inside i64 so the `as i64` cast is exact too
/// (unlike `i64::MAX as f64`, which rounds UP to 2^63 and would wrongly admit
/// an out-of-range bound).
fn random_int_bound(v: &Value) -> MixResult<i64> {
    // 2^53 - 1 -- the largest magnitude with no rounded-integer twin: 2^53
    // itself is what 2^53 + 1 rounds to, so admitting it admits an aliased
    // bound the caller never wrote.
    const MAX_SAFE_INT: i64 = 9_007_199_254_740_991;
    let n = num_arg("random", v)?;
    as_exact_integer("random(): bound", n, -MAX_SAFE_INT, MAX_SAFE_INT)
}

/// `random()` -> float in [0.0, 1.0);  `random(min, max)` -> integer in
/// [min, max] inclusive. Sourced from the thread-local RNG (auto-seeded from
/// OS entropy) — fast enough for tight loops. Non-deterministic by design and
/// NOT cryptographically strong: for secrets use `random_password` or `uuid`
/// (which draw from a cryptographically secure RNG), not this.
fn builtin_random(args: Vec<Value>) -> MixResult<Option<Value>> {
    use rand::Rng;
    match args.len() {
        0 => Ok(Some(Value::Number(rand::rng().random::<f64>()))),
        2 => {
            let lo = random_int_bound(&args[0])?;
            let hi = random_int_bound(&args[1])?;
            if lo > hi {
                return Err(MixError::RuntimeError {
                    span: None,
                    msg: format!("random() lower bound {} exceeds upper bound {}", lo, hi),
                });
            }
            Ok(Some(
                Value::Number(rand::rng().random_range(lo..=hi) as f64),
            ))
        }
        n => Err(MixError::RuntimeError {
            span: None,
            msg: format!(
                "random() expects 0 args (float [0,1)) or 2 args (int min, max), got {}",
                n
            ),
        }),
    }
}

fn builtin_push(mut args: Vec<Value>) -> MixResult<Option<Value>> {
    expect_args("push", &args, 2)?;
    let val = args[1].clone();
    match &mut args[0] {
        Value::List(items) => {
            // CoW: this is the NON-mutating fallback (the in-place path
            // lives in call_mutating_builtin) — the owned arg may share
            // with a scope binding, so make_mut copies exactly then,
            // preserving the caller-invisible-no-op semantics at shallow
            // (was: deep) cost.
            Rc::make_mut(items).push(val);
            Ok(Some(args[0].clone()))
        }
        _ => Err(MixError::RuntimeError {
            span: None,
            msg: "push() expects a list".to_string(),
        }),
    }
}

fn builtin_pop(mut args: Vec<Value>) -> MixResult<Option<Value>> {
    expect_args("pop", &args, 1)?;
    match &mut args[0] {
        Value::List(items) => Ok(Some(Rc::make_mut(items).pop().unwrap_or(Value::Nil))),
        _ => Err(MixError::RuntimeError {
            span: None,
            msg: "pop() expects a list".to_string(),
        }),
    }
}

fn builtin_shift(mut args: Vec<Value>) -> MixResult<Option<Value>> {
    expect_args("shift", &args, 1)?;
    match &mut args[0] {
        Value::List(items) => {
            if items.is_empty() {
                Ok(Some(Value::Nil))
            } else {
                Ok(Some(Rc::make_mut(items).remove(0)))
            }
        }
        _ => Err(MixError::RuntimeError {
            span: None,
            msg: "shift() expects a list".to_string(),
        }),
    }
}

fn builtin_sort(args: Vec<Value>) -> MixResult<Option<Value>> {
    expect_args("sort", &args, 1)?;
    match &args[0] {
        Value::List(items) => {
            // CoW: shallow copy — the result is a fresh Value.
            let mut sorted = items.to_vec();
            // A homogeneous list of numbers sorts NUMERICALLY. Sorting by
            // the decimal string (the historical behaviour, kept for every
            // other element type) orders `[3, 1, 20]` as `[1, 20, 3]` —
            // almost never what a caller wants from `sort` on numbers, and
            // a sharp edge for data/large-text work. Mixed or non-numeric
            // lists keep the stable stringwise order (unchanged).
            if !sorted.is_empty() && sorted.iter().all(|v| matches!(v, Value::Number(_))) {
                // `total_cmp` is a genuine total order over ALL f64 —
                // including NaN (which Mix math builtins like `sqrt(-1)` can
                // produce). `partial_cmp().unwrap_or(Equal)` would make NaN
                // compare Equal to everything, violating the comparator's
                // total-order contract (unspecified/again-panicking sort).
                // With `total_cmp`, non-finite values get a defined, stable
                // placement instead.
                sorted.sort_by(|a, b| match (a, b) {
                    (Value::Number(x), Value::Number(y)) => x.total_cmp(y),
                    _ => std::cmp::Ordering::Equal, // unreachable: all-number gate above
                });
            } else {
                sorted.sort_by_key(|a| a.to_mix_string());
            }
            Ok(Some(Value::list(sorted)))
        }
        _ => Err(MixError::RuntimeError {
            span: None,
            msg: "sort() expects a list".to_string(),
        }),
    }
}

fn builtin_index_of(args: Vec<Value>) -> MixResult<Option<Value>> {
    expect_args("index_of", &args, 2)?;
    match &args[0] {
        Value::List(items) => {
            let pos = items.iter().position(|v| v == &args[1]);
            Ok(Some(Value::Number(pos.map(|p| p as f64).unwrap_or(-1.0))))
        }
        // String overload: returns the position of `needle` in `haystack`
        // (-1 if absent). Mirrors the list shape — `contains` already covers
        // the boolean case for both types; `index_of` covers the positional
        // case, also for both. Added when the SPEC 18 Phase 2 WS5 harness
        // needed `substr(s, index_of(s, "result=") + 7, ...)` to slice out a
        // known marker. (Was a byte offset pre-0.8.0; the P1 flip below made it
        // codepoint-based — exactly so that `substr` composition stopped
        // corrupting on multibyte. `byte_index_of` keeps the old byte offset.)
        Value::String(s) => {
            let needle = args[1].to_mix_string();
            // P1: codepoint offset (0-based, -1=not found) — count codepoints in
            // the byte prefix before the (char-boundary) match.
            let pos = s
                .find(&needle)
                .map(|b| s[..b].chars().count() as f64)
                .unwrap_or(-1.0);
            Ok(Some(Value::Number(pos)))
        }
        _ => Err(MixError::RuntimeError {
            span: None,
            msg: "index_of() expects a list or string".to_string(),
        }),
    }
}

fn builtin_unique(args: Vec<Value>) -> MixResult<Option<Value>> {
    expect_args("unique", &args, 1)?;
    match &args[0] {
        Value::List(items) => {
            let mut result: Vec<Value> = Vec::new();
            for item in items.iter() {
                if !result.iter().any(|x| x == item) {
                    result.push(item.clone());
                }
            }
            Ok(Some(Value::list(result)))
        }
        _ => Err(MixError::RuntimeError {
            span: None,
            msg: "unique() expects a list".to_string(),
        }),
    }
}

fn builtin_range(args: Vec<Value>) -> MixResult<Option<Value>> {
    expect_args("range", &args, 2)?;
    let start = as_exact_integer(
        "range(): argument 1",
        number_arg("range", &args, 0)?,
        i64::MIN,
        i64::MAX,
    )?;
    let end = as_exact_integer(
        "range(): argument 2",
        number_arg("range", &args, 1)?,
        i64::MIN,
        i64::MAX,
    )?;
    let step = if args.len() > 2 {
        let step = as_loop_step("range(): argument 3", number_arg("range", &args, 2)?)?;
        as_exact_integer("range(): argument 3", step, i64::MIN, i64::MAX)?
    } else {
        1
    };
    // Hard cap on element count — range(0, 1e18) must error, not push
    // forever. Count up-front in i128 so start/end at the i64 extremes
    // (huge f64 args saturate there) can't overflow the subtraction. Check
    // direction FIRST: when start/end ordering opposes the step (e.g.
    // `range(5, 1, 10)`) the range is empty — the `/ step + 1` formula would
    // otherwise truncate a negative distance to 0 and report a spurious 1.
    const MAX_RANGE_ELEMENTS: i128 = 10_000_000;
    let count: i128 = if step > 0 && start <= end {
        (end as i128 - start as i128) / (step as i128) + 1
    } else if step < 0 && start >= end {
        (start as i128 - end as i128) / (-(step as i128)) + 1
    } else {
        0
    };
    if count > MAX_RANGE_ELEMENTS {
        return Err(MixError::RuntimeError {
            span: None,
            msg: format!(
                "range() would produce {} elements (cap {})",
                count, MAX_RANGE_ELEMENTS
            ),
        });
    }
    // `count <= 0` means start/end ordering opposes the step direction (e.g.
    // `range(5, 0)` with step +1) — an empty list, as before. Otherwise emit
    // exactly `count` values computed in i128, so the stride never overflows
    // i64 even when start/end sit at the saturated f64→i64 extremes
    // (`range(1e30, 1e30)` previously panicked/wrapped on the post-loop step).
    let mut result = Vec::new();
    if count > 0 {
        result.reserve(count as usize);
        let (start, step) = (start as i128, step as i128);
        for k in 0..count {
            result.push(Value::Number((start + k * step) as f64));
        }
    }
    Ok(Some(Value::list(result)))
}

fn builtin_flat(args: Vec<Value>) -> MixResult<Option<Value>> {
    expect_args("flat", &args, 1)?;
    fn flatten(val: &Value) -> Vec<Value> {
        match val {
            Value::List(items) => items.iter().flat_map(flatten).collect(),
            other => vec![other.clone()],
        }
    }
    Ok(Some(Value::list(flatten(&args[0]))))
}

/// `concat($a, $b, ...)` — join 2+ lists into ONE new list, one level
/// deep (unlike `flat`, which recurses into nested lists). Functional:
/// returns a fresh list, mutating nothing. This is the clean, O(total)
/// way to accumulate a sequence across helpers under Mix's by-value
/// list model — a helper returns its events, the caller `concat`s them
/// in, sidestepping the `push($param, …)`-into-a-copy trap entirely.
fn builtin_concat(args: Vec<Value>) -> MixResult<Option<Value>> {
    if args.len() < 2 {
        return Err(MixError::RuntimeError {
            span: None,
            msg: format!("concat() expects at least 2 lists, got {}", args.len()),
        });
    }
    let mut out: Vec<Value> = Vec::new();
    for (i, a) in args.iter().enumerate() {
        match a {
            Value::List(items) => {
                out.reserve(items.len());
                out.extend(items.iter().cloned());
            }
            other => {
                return Err(MixError::RuntimeError {
                    span: None,
                    msg: format!(
                        "concat() arg {} is {}, expected a list",
                        i + 1,
                        other.type_name()
                    ),
                });
            }
        }
    }
    Ok(Some(Value::list(out)))
}

/// `slice($list, $start, $end = nil)` — sublist from `$start` to
/// `$end` (exclusive). Negative indices count from the end. `$end`
/// may be omitted or `nil` to slice through the end of the list.
/// Boundaries clamp instead of erroring: `slice(xs, -100, 100)` on
/// a 3-element list returns the whole list.
fn builtin_slice(args: Vec<Value>) -> MixResult<Option<Value>> {
    if args.len() != 2 && args.len() != 3 {
        return Err(MixError::RuntimeError {
            span: None,
            msg: format!("slice() expects 2 or 3 args, got {}", args.len()),
        });
    }
    let items = match &args[0] {
        Value::List(items) => items.clone(),
        Value::String(s) => {
            // String slicing works char-wise to stay unicode-safe.
            // `slice` on a string returns a new string; scripts that
            // want code-point access use `$s[-1]` for single chars.
            let chars: Vec<char> = s.chars().collect();
            let start = args[1].to_number().ok_or_else(|| MixError::RuntimeError {
                span: None,
                msg: "slice(): start must be a number".to_string(),
            })? as i64;
            let end = match args.get(2) {
                None | Some(Value::Nil) => chars.len() as i64,
                Some(v) => v.to_number().ok_or_else(|| MixError::RuntimeError {
                    span: None,
                    msg: "slice(): end must be a number or nil".to_string(),
                })? as i64,
            };
            let s_idx = clamp_signed_index(start, chars.len());
            let e_idx = clamp_signed_index(end, chars.len()).max(s_idx);
            let out: String = chars[s_idx..e_idx].iter().collect();
            return Ok(Some(Value::String(out)));
        }
        // bytes/buffer slice BYTE-wise and always return a value-semantic
        // `bytes` (v0.64.0) — slicing a mutable Buffer hands back an
        // independent snapshot, never an alias into it. Same clamping as
        // the list arm: out-of-range saturates, a reversed range is empty.
        Value::Bytes(_) | Value::Buffer(_) => {
            let buf = subject_bytes("slice", &args[0])?;
            let start = args[1].to_number().ok_or_else(|| MixError::RuntimeError {
                span: None,
                msg: "slice(): start must be a number".to_string(),
            })? as i64;
            let end = match args.get(2) {
                None | Some(Value::Nil) => buf.len() as i64,
                Some(v) => v.to_number().ok_or_else(|| MixError::RuntimeError {
                    span: None,
                    msg: "slice(): end must be a number or nil".to_string(),
                })? as i64,
            };
            let s_idx = clamp_signed_index(start, buf.len());
            let e_idx = clamp_signed_index(end, buf.len()).max(s_idx);
            return Ok(Some(Value::bytes(buf[s_idx..e_idx].to_vec())));
        }
        other => {
            return Err(MixError::RuntimeError {
                span: None,
                msg: format!(
                    "slice() expects a list, string, bytes or buffer, got {}",
                    other.type_name()
                ),
            });
        }
    };
    let start = args[1].to_number().ok_or_else(|| MixError::RuntimeError {
        span: None,
        msg: "slice(): start must be a number".to_string(),
    })? as i64;
    let end = match args.get(2) {
        None | Some(Value::Nil) => items.len() as i64,
        Some(v) => v.to_number().ok_or_else(|| MixError::RuntimeError {
            span: None,
            msg: "slice(): end must be a number or nil".to_string(),
        })? as i64,
    };
    let s_idx = clamp_signed_index(start, items.len());
    let e_idx = clamp_signed_index(end, items.len()).max(s_idx);
    Ok(Some(Value::list(items[s_idx..e_idx].to_vec())))
}

/// `take($list, $n)` — first `$n` items. Sugar over `slice(xs, 0, n)`.
/// Negative `$n` takes from the end: `take(xs, -3)` → last three.
fn builtin_take(args: Vec<Value>) -> MixResult<Option<Value>> {
    expect_args("take", &args, 2)?;
    let n = args[1].to_number().ok_or_else(|| MixError::RuntimeError {
        span: None,
        msg: "take(): n must be a number".to_string(),
    })? as i64;
    match &args[0] {
        Value::List(items) => {
            if n >= 0 {
                let end = (n as usize).min(items.len());
                Ok(Some(Value::list(items[..end].to_vec())))
            } else {
                let start = items.len().saturating_sub(neg_index_magnitude(n));
                Ok(Some(Value::list(items[start..].to_vec())))
            }
        }
        Value::String(s) => {
            let chars: Vec<char> = s.chars().collect();
            let out: String = if n >= 0 {
                chars.iter().take(n as usize).collect()
            } else {
                let start = chars.len().saturating_sub(neg_index_magnitude(n));
                chars[start..].iter().collect()
            };
            Ok(Some(Value::String(out)))
        }
        other => Err(MixError::RuntimeError {
            span: None,
            msg: format!("take() expects a list or string, got {}", other.type_name()),
        }),
    }
}

/// `drop($list, $n)` — skip first `$n` items. Sugar over
/// `slice(xs, n, nil)`. Negative `$n` drops from the end.
fn builtin_drop(args: Vec<Value>) -> MixResult<Option<Value>> {
    expect_args("drop", &args, 2)?;
    let n = args[1].to_number().ok_or_else(|| MixError::RuntimeError {
        span: None,
        msg: "drop(): n must be a number".to_string(),
    })? as i64;
    match &args[0] {
        Value::List(items) => {
            if n >= 0 {
                let start = (n as usize).min(items.len());
                Ok(Some(Value::list(items[start..].to_vec())))
            } else {
                let end = items.len().saturating_sub(neg_index_magnitude(n));
                Ok(Some(Value::list(items[..end].to_vec())))
            }
        }
        Value::String(s) => {
            let chars: Vec<char> = s.chars().collect();
            let out: String = if n >= 0 {
                chars.iter().skip(n as usize).collect()
            } else {
                let end = chars.len().saturating_sub(neg_index_magnitude(n));
                chars[..end].iter().collect()
            };
            Ok(Some(Value::String(out)))
        }
        other => Err(MixError::RuntimeError {
            span: None,
            msg: format!("drop() expects a list or string, got {}", other.type_name()),
        }),
    }
}

/// `zip($a, $b)` — pairs corresponding items. Length is `min(len(a), len(b))`.
/// Each pair is a 2-element list so scripts can destructure with
/// `$pair[0]` / `$pair[1]`.
fn builtin_zip(args: Vec<Value>) -> MixResult<Option<Value>> {
    expect_args("zip", &args, 2)?;
    let a = match &args[0] {
        Value::List(items) => items,
        other => {
            return Err(MixError::RuntimeError {
                span: None,
                msg: format!("zip() arg 1 expected list, got {}", other.type_name()),
            });
        }
    };
    let b = match &args[1] {
        Value::List(items) => items,
        other => {
            return Err(MixError::RuntimeError {
                span: None,
                msg: format!("zip() arg 2 expected list, got {}", other.type_name()),
            });
        }
    };
    let out: Vec<Value> = a
        .iter()
        .zip(b.iter())
        .map(|(x, y)| Value::list(vec![x.clone(), y.clone()]))
        .collect();
    Ok(Some(Value::list(out)))
}

fn builtin_keys(args: Vec<Value>) -> MixResult<Option<Value>> {
    expect_args("keys", &args, 1)?;
    match &args[0] {
        Value::Map(m) => {
            let keys: Vec<Value> = m.keys().map(|k| Value::String(k.clone())).collect();
            Ok(Some(Value::list(keys)))
        }
        _ => Err(MixError::RuntimeError {
            span: None,
            msg: "keys() expects a map".to_string(),
        }),
    }
}

fn builtin_values(args: Vec<Value>) -> MixResult<Option<Value>> {
    expect_args("values", &args, 1)?;
    match &args[0] {
        Value::Map(m) => {
            let vals: Vec<Value> = m.values().cloned().collect();
            Ok(Some(Value::list(vals)))
        }
        _ => Err(MixError::RuntimeError {
            span: None,
            msg: "values() expects a map".to_string(),
        }),
    }
}

fn builtin_has_key(args: Vec<Value>) -> MixResult<Option<Value>> {
    expect_args("has_key", &args, 2)?;
    match &args[0] {
        Value::Map(m) => {
            let key = args[1].to_mix_string();
            Ok(Some(Value::Bool(m.contains_key(&key))))
        }
        _ => Err(MixError::RuntimeError {
            span: None,
            msg: "has_key() expects a map".to_string(),
        }),
    }
}

fn builtin_merge(args: Vec<Value>) -> MixResult<Option<Value>> {
    expect_args("merge", &args, 2)?;
    match (&args[0], &args[1]) {
        (Value::Map(a), Value::Map(b)) => {
            // CoW: shallow one-level copies — the result is a fresh Value.
            let mut result = (**a).clone();
            result.extend(b.iter().map(|(k, v)| (k.clone(), v.clone())));
            Ok(Some(Value::map(result)))
        }
        _ => Err(MixError::RuntimeError {
            span: None,
            msg: "merge() expects two maps".to_string(),
        }),
    }
}

fn builtin_delete(args: Vec<Value>) -> MixResult<Option<Value>> {
    expect_args("delete", &args, 2)?;
    match &args[0] {
        Value::Map(m) => {
            let key = args[1].to_mix_string();
            // CoW: shallow copy — the result is a fresh Value.
            let mut result = (**m).clone();
            result.shift_remove(&key);
            Ok(Some(Value::map(result)))
        }
        _ => Err(MixError::RuntimeError {
            span: None,
            msg: "delete() expects a map".to_string(),
        }),
    }
}

// --- System builtins ---

fn builtin_env(args: Vec<Value>) -> MixResult<Option<Value>> {
    expect_args_between("env", &args, 1, 2)?;
    let name = args[0].to_mix_string();
    let val = std::env::var(&name).unwrap_or_default();
    // Two-arg form: env(name, default) returns the default when the variable is
    // unset OR set-but-empty (shell `${VAR:-default}` semantics) — the common
    // "env or fallback" need in config code. The one-arg form is unchanged:
    // still "" for unset, so existing scripts keep working. The default is
    // returned verbatim (any Value), so env("PORT", 8080) yields the number.
    if val.is_empty() && args.len() == 2 {
        return Ok(Some(args[1].clone()));
    }
    Ok(Some(Value::String(val)))
}

fn builtin_time(_args: Vec<Value>) -> MixResult<Option<Value>> {
    use std::time::{SystemTime, UNIX_EPOCH};
    let secs = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs_f64();
    Ok(Some(Value::Number(secs)))
}

fn builtin_pid(_args: Vec<Value>) -> MixResult<Option<Value>> {
    Ok(Some(Value::Number(std::process::id() as f64)))
}

/// The EFFECTIVE uid/gid — normally what a file access is checked against, and
/// so what a script comparing itself against a `stat()` map wants. Linux
/// strictly checks the *filesystem* ids, which track the effective ids unless
/// `setfsuid(2)`/`setfsgid(2)` is called; no Mix script can call either, so for
/// Mix code the two are the same answer. An embedder that does call them makes
/// this report the wrong one, which is why the man page says so out loud.
/// Without these the only way for Mix code to learn its own uid was to create a
/// file and stat it, and that probe is a liability rather than a measurement:
/// the write follows symlinks, so another uid who wins the race gets an
/// arbitrary file truncated as you and the answer comes back as *their* uid.
/// A missing primitive, not a missing workaround.
fn builtin_uid(_args: Vec<Value>) -> MixResult<Option<Value>> {
    // SAFETY: geteuid() cannot fail and touches no memory the caller owns.
    Ok(Some(Value::Number(unsafe { libc::geteuid() } as f64)))
}

fn builtin_gid(_args: Vec<Value>) -> MixResult<Option<Value>> {
    // SAFETY: getegid() cannot fail and touches no memory the caller owns.
    Ok(Some(Value::Number(unsafe { libc::getegid() } as f64)))
}

/// Every group this process is in — the supplementary set plus the effective
/// gid, which `getgroups(2)` is not required to include. Without it, deciding
/// which permission class the kernel will apply to a file owned by someone else
/// is *unanswerable* from Mix: `gid()` alone says only whether the file's group
/// happens to be the effective one, so a script had to report "cannot tell" for
/// every file whose group is one of the caller's other groups. That is a missing
/// primitive, not a question with no answer.
fn builtin_groups(_args: Vec<Value>) -> MixResult<Option<Value>> {
    // getgroups(2) is a size-then-fill pair, and the set can change between the
    // two calls: a *shrink* is handled by truncating to the returned count, but a
    // *growth* makes the fill call fail with EINVAL because the buffer it is
    // handed is now too small. Mix itself cannot call setgroups, so this needs
    // another thread in an embedder to happen at all — which is precisely the
    // case that matters, since the daemons embed this interpreter. Reporting
    // "cannot tell" there would be wrong: the answer is available, the first
    // measurement was just stale. So re-measure and retry.
    //
    // Bounded, because an unbounded retry against a process rewriting its group
    // set in a loop is a hang. A handful of attempts covers a racing writer; a
    // set that will not hold still for four reads is reported as unreadable
    // rather than guessed at.
    let mut attempts = 0;
    let buf: Vec<libc::gid_t> = loop {
        // SAFETY: getgroups(0, null) only reports how many entries there are; it
        // writes nothing. A negative return means the call failed, which leaves
        // the supplementary set unknown — reported as an error rather than as
        // "none", because an empty list is a claim.
        let n = unsafe { libc::getgroups(0, std::ptr::null_mut()) };
        if n < 0 {
            return Err(MixError::RuntimeError {
                span: None,
                msg: format!(
                    "groups(): getgroups() failed: {}",
                    std::io::Error::last_os_error()
                ),
            });
        }
        // max(n, 1), never n: gidsetsize 0 is not "a buffer of zero entries",
        // it is the kernel's *size query* — the same call as the measurement
        // above. So a process with no supplementary groups that gains one
        // between the two calls got `got == 1` from a call that wrote nothing,
        // truncate(1) on an empty vec left it empty, and the growth was
        // silently reported as "no supplementary groups" instead of taking the
        // EINVAL retry path every other growth takes. Asking for one slot makes
        // the second call a genuine fill in every case; a set that grew past
        // one still fails EINVAL and re-measures.
        let mut b: Vec<libc::gid_t> = vec![0; std::cmp::max(n, 1) as usize];
        // SAFETY: b has room for the size being passed, which is what the call
        // above reported (or one, when that was zero). A concurrent grower
        // invalidates that; the kernel says so with EINVAL rather than
        // overrunning the buffer, and the loop re-measures. A shrink is handled
        // by truncating to the returned count.
        let got = unsafe { libc::getgroups(std::cmp::max(n, 1), b.as_mut_ptr()) };
        if got >= 0 {
            b.truncate(got as usize);
            break b;
        }
        let err = std::io::Error::last_os_error();
        attempts += 1;
        if err.raw_os_error() != Some(libc::EINVAL) || attempts >= 4 {
            return Err(MixError::RuntimeError {
                span: None,
                msg: format!("groups(): getgroups() failed: {err}"),
            });
        }
    };
    // SAFETY: getegid() cannot fail and touches no memory the caller owns.
    let egid = unsafe { libc::getegid() };
    Ok(Some(Value::list(
        normalize_group_set(buf, egid)
            .into_iter()
            .map(|g| Value::Number(g as f64))
            .collect(),
    )))
}

/// The egid-plus-set-semantics half of `groups()`, split out from the syscall
/// half so it can be tested against an input the kernel will not produce on
/// demand. Testing it through `groups()` alone was a hollow test: this
/// workstation's kernel returns a duplicate-free list, so deleting the dedup
/// changed nothing observable and the probe stayed green.
///
/// The documented result is a SET including the effective gid. A process can be
/// given the same gid twice in its supplementary list and getgroups reports it
/// twice, so without the dedup the manual's "no duplicates" is a promise the
/// builtin does not keep and a caller counting matches double-counts.
fn normalize_group_set(mut gids: Vec<libc::gid_t>, egid: libc::gid_t) -> Vec<libc::gid_t> {
    gids.push(egid);
    gids.sort_unstable();
    gids.dedup();
    gids
}

/// The script's arguments as the caller parsed them — everything after the
/// script path, or after the `-c` code. Set once by the CLI; `args()` reads it.
static SCRIPT_ARGV: std::sync::OnceLock<Vec<String>> = std::sync::OnceLock::new();

/// Tell `args()` what the script arguments actually are.
///
/// `args()` used to re-derive them from `std::env::args().skip(2)`, on the
/// assumption that argv is always `[mix, script, args…]`. It is not: any flag
/// before the script name shifts the list by one (`--strict-arity script a b`
/// reported the script path as the first argument), and under `-c` the skip
/// landed on the code itself, so `args()` returned the program text as
/// argument zero. `$1`, `$2`, … were always right, because they come from the
/// list the CLI parsed — this makes `args()` read the same list.
///
/// Idempotent: the first call wins, later calls are ignored.
pub fn set_script_argv(argv: Vec<String>) {
    let _ = SCRIPT_ARGV.set(argv);
}

fn builtin_args(_args: Vec<Value>) -> MixResult<Option<Value>> {
    // No CLI set it (embedded interpreter, unit test): no script arguments.
    // Guessing from the host process's argv is how this went wrong before.
    let argv: Vec<Value> = SCRIPT_ARGV
        .get()
        .map(|a| a.iter().cloned().map(Value::String).collect())
        .unwrap_or_default();
    Ok(Some(Value::list(argv)))
}

fn builtin_exit(args: Vec<Value>) -> MixResult<Option<Value>> {
    let code = if args.is_empty() {
        0
    } else {
        as_exit_code("exit(): argument 1", number_arg("exit", &args, 0)?)?
    };
    Err(MixError::ExitRequest { code })
}

/// getopt(args, spec) — parse an argument list against a declarative spec map,
/// returning structured results (no global state, no process exit).
///
/// `spec` keys are long option names; each value is a map `{short?, arg?}`:
/// `short` is an optional single-character alias, `arg: true` means the option
/// takes a value (omitted/false = a boolean flag).
///
/// Returns `{opts, rest, errors}`:
/// - `opts`  — every declared option, always present: flags default `false`
///   (set `true` when seen), value-options default `nil` (set to their string).
/// - `rest`  — positional arguments, including everything after a `--` terminator.
/// - `errors`— collected human-readable strings for unknown options / missing
///   values. An empty list means a clean parse; the CALLER decides whether to
///   abort. (Contrast getopt(3), which mutates globals and the C runtime exits.)
///
/// Minimal grammar by design: `--long`, `-s`, `--key=value`, `--key value`,
/// `-s value`, and a `--` terminator. No short bundling (`-abc`), no attached
/// short values (`-svalue`), no long-option abbreviation.
///
/// A malformed SPEC (non-map entry, multi-char `short`, duplicate `short`,
/// non-bool `arg`) is a script bug and raises a catchable error — distinct from
/// bad user input, which is collected into `errors`.
fn builtin_getopt(args: Vec<Value>) -> MixResult<Option<Value>> {
    expect_args("getopt", &args, 2)?;
    let err = |msg: String| MixError::RuntimeError { span: None, msg };

    let argv: Vec<String> = match &args[0] {
        Value::List(l) => l.iter().map(|v| v.to_mix_string()).collect(),
        other => {
            return Err(err(format!(
                "getopt() arg 1 must be a list, got {}",
                other.type_name()
            )));
        }
    };
    let spec_map = match &args[1] {
        Value::Map(m) => m,
        other => {
            return Err(err(format!(
                "getopt() arg 2 must be a map, got {}",
                other.type_name()
            )));
        }
    };

    struct OptSpec {
        long: String,
        takes_arg: bool,
    }

    // Build the option table + short->long index. Spec errors die (script bug).
    let mut specs: Vec<OptSpec> = Vec::new();
    let mut short_to_long: indexmap::IndexMap<String, String> = indexmap::IndexMap::new();
    for (long, def) in spec_map.iter() {
        // A long name the `--long` / `--k=v` grammar can't address is a script
        // bug, not user error: empty (shadowed by `--`), a leading `-`, an `=`
        // (the parser splits on it), or whitespace (the shell word-splits it).
        if long.is_empty()
            || long.starts_with('-')
            || long.contains('=')
            || long.chars().any(char::is_whitespace)
        {
            return Err(err(format!(
                "getopt() spec: invalid long option name '{}' (no leading '-', '=', whitespace, or empty)",
                long
            )));
        }
        let def_map = match def {
            Value::Map(m) => m,
            other => {
                return Err(err(format!(
                    "getopt() spec '{}' must be a map, got {}",
                    long,
                    other.type_name()
                )));
            }
        };
        let short = match def_map.get("short") {
            None | Some(Value::Nil) => None,
            Some(Value::String(s)) => {
                if s.chars().count() != 1 {
                    return Err(err(format!(
                        "getopt() spec '{}': short must be a single character, got \"{}\"",
                        long, s
                    )));
                }
                Some(s.clone())
            }
            Some(other) => {
                return Err(err(format!(
                    "getopt() spec '{}': short must be a string, got {}",
                    long,
                    other.type_name()
                )));
            }
        };
        let takes_arg = match def_map.get("arg") {
            None | Some(Value::Nil) => false,
            Some(Value::Bool(b)) => *b,
            Some(other) => {
                return Err(err(format!(
                    "getopt() spec '{}': arg must be a bool, got {}",
                    long,
                    other.type_name()
                )));
            }
        };
        if let Some(s) = &short
            && let Some(prev) = short_to_long.insert(s.clone(), long.clone())
        {
            return Err(err(format!(
                "getopt() spec: short '-{}' bound to both '{}' and '{}'",
                s, prev, long
            )));
        }
        // `short` (if any) is recorded in `short_to_long` above; the per-option
        // struct just needs the long name and arg-arity.
        specs.push(OptSpec {
            long: long.clone(),
            takes_arg,
        });
    }

    // opts starts fully populated with declared defaults.
    let mut opts: indexmap::IndexMap<String, Value> = indexmap::IndexMap::new();
    for sp in &specs {
        opts.insert(
            sp.long.clone(),
            if sp.takes_arg {
                Value::Nil
            } else {
                Value::Bool(false)
            },
        );
    }
    let mut rest: Vec<Value> = Vec::new();
    let mut errors: Vec<Value> = Vec::new();

    let mut i = 0;
    let mut only_positional = false;
    while i < argv.len() {
        let tok = argv[i].clone();
        i += 1;

        if only_positional {
            rest.push(Value::String(tok));
            continue;
        }
        if tok == "--" {
            only_positional = true;
            continue;
        }
        // bare "-" (stdin convention) or any non-`-` token is positional.
        if tok == "-" || !tok.starts_with('-') {
            rest.push(Value::String(tok));
            continue;
        }

        if let Some(body) = tok.strip_prefix("--") {
            // long option, possibly --name=value
            let (name, inline_val) = match body.split_once('=') {
                Some((n, v)) => (n.to_string(), Some(v.to_string())),
                None => (body.to_string(), None),
            };
            match specs.iter().find(|s| s.long == name) {
                None => errors.push(Value::String(format!("unknown option: --{}", name))),
                Some(sp) => {
                    if sp.takes_arg {
                        let val = if let Some(v) = inline_val {
                            Some(v)
                        } else if i < argv.len() {
                            let v = argv[i].clone();
                            i += 1;
                            Some(v)
                        } else {
                            None
                        };
                        match val {
                            Some(v) => {
                                opts.insert(sp.long.clone(), Value::String(v));
                            }
                            None => errors
                                .push(Value::String(format!("option --{} requires a value", name))),
                        }
                    } else if inline_val.is_some() {
                        errors.push(Value::String(format!("option --{} takes no value", name)));
                    } else {
                        opts.insert(sp.long.clone(), Value::Bool(true));
                    }
                }
            }
            continue;
        }

        // short option: a single `-` prefix. Minimal = exactly one char after it.
        let body = &tok[1..];
        if body.chars().count() != 1 {
            errors.push(Value::String(format!(
                "unsupported short-option token: {} (no bundling or attached values; use -a -b … or the --long form)",
                tok
            )));
            continue;
        }
        match short_to_long
            .get(body)
            .and_then(|l| specs.iter().find(|s| &s.long == l))
        {
            None => errors.push(Value::String(format!("unknown option: -{}", body))),
            Some(sp) => {
                if sp.takes_arg {
                    if i < argv.len() {
                        let v = argv[i].clone();
                        i += 1;
                        opts.insert(sp.long.clone(), Value::String(v));
                    } else {
                        errors.push(Value::String(format!("option -{} requires a value", body)));
                    }
                } else {
                    opts.insert(sp.long.clone(), Value::Bool(true));
                }
            }
        }
    }

    let mut result: indexmap::IndexMap<String, Value> = indexmap::IndexMap::new();
    result.insert("opts".into(), Value::map(opts));
    result.insert("rest".into(), Value::list(rest));
    result.insert("errors".into(), Value::list(errors));
    Ok(Some(Value::map(result)))
}

/// run(cmd) — run command via sh, return trimmed stdout as string.
/// Fail-fast: a non-zero exit (or signal-kill) raises a catchable `die` error
/// containing the command excerpt, exit status, and a tail of stderr.
/// Pairs with `run_rc(cmd)` which returns the full {rc, stdout, stderr} map
/// without throwing — use that when you need to inspect a non-zero rc.
/// Parse the optional trailing opts map shared by `run`/`run_rc` and the
/// `http_*` builtins. Only `{timeout: <seconds>}` is accepted (0 disables
/// the deadline); an unknown key is a loud error so a typo (`timeout_ms`,
/// `timout`) can't silently produce an unbounded call. Absent/nil → default.
fn parse_timeout_opt(name: &str, v: Option<&Value>, default_s: u64) -> MixResult<u64> {
    let map = match v {
        None | Some(Value::Nil) => return Ok(default_s),
        Some(Value::Map(m)) => m,
        Some(other) => {
            return Err(MixError::RuntimeError {
                span: None,
                msg: format!(
                    "{name}: opts must be a map like {{timeout: 30}}, got {}",
                    other.type_name()
                ),
            });
        }
    };
    for key in map.keys() {
        if key != "timeout" {
            return Err(MixError::RuntimeError {
                span: None,
                msg: format!("{name}: unknown opt {key:?} (supported: timeout)"),
            });
        }
    }
    match map.get("timeout") {
        Some(v) => parse_nonneg_int_opt(name, "timeout", v),
        None => Ok(default_s),
    }
}

/// Exact-arity guard: min..=max args. A surplus argument is a loud error so
/// a misplaced opts map (`run(cmd, nil, {timeout: 1})`) can't be silently
/// ignored and leave a call unbounded.
fn expect_args_between(name: &str, args: &[Value], min: usize, max: usize) -> MixResult<()> {
    expect_args(name, args, min)?;
    if args.len() > max {
        return Err(MixError::RuntimeError {
            span: None,
            msg: format!(
                "{}() expects at most {} argument(s), got {}",
                name,
                max,
                args.len()
            ),
        });
    }
    Ok(())
}

/// Truncate + escape a command string for a one-line diagnostic.
fn cmd_excerpt_for_diag(cmd: &str) -> String {
    let cmd_safe = sanitize_for_diag(cmd);
    let cmd_chars: Vec<char> = cmd_safe.chars().collect();
    let truncated: String = if cmd_chars.len() > 80 {
        cmd_chars.iter().take(80).collect::<String>() + "…"
    } else {
        cmd_safe
    };
    truncated.replace('\'', "\\'")
}

/// `run(cmd, [{timeout: seconds}])` — stdout string, dies on failure.
///
/// The child runs under the same process-group kill + interrupt machinery
/// as `ssh_run` (`run_with_timeout`), so a bounded `run` can never wedge
/// the login shell past its deadline and Ctrl-C is honoured cooperatively.
/// Default timeout is 0 (no deadline) — the historic contract for long
/// builds; pass `{timeout: N}` to bound a call.
fn builtin_run(args: Vec<Value>) -> MixResult<Option<Value>> {
    expect_args_between("run", &args, 1, 2)?;
    let cmd = args[0].to_mix_string();
    let timeout_s = parse_timeout_opt("run", args.get(1), 0)?;
    let argv = vec!["sh".to_string(), "-c".to_string(), cmd.clone()];
    let outcome = run_with_timeout(&argv, None, timeout_s, "run")?;
    if outcome.interrupted {
        return Err(MixError::RuntimeError {
            span: None,
            msg: "run: interrupted".into(),
        });
    }
    if outcome.timed_out {
        return Err(MixError::DieError {
            msg: format!(
                "run: '{}' timed out after {}s",
                cmd_excerpt_for_diag(&cmd),
                timeout_s
            ),
        });
    }
    if outcome.exit_code != 0 {
        let cmd_excerpt = cmd_excerpt_for_diag(&cmd);
        let stderr_lossy = String::from_utf8_lossy(&outcome.stderr);
        let stderr_safe = sanitize_for_diag(stderr_lossy.trim());
        let stderr_chars: Vec<char> = stderr_safe.chars().collect();
        let stderr_tail = if stderr_chars.len() > 200 {
            "…".to_string()
                + &stderr_chars[stderr_chars.len() - 200..]
                    .iter()
                    .collect::<String>()
        } else {
            stderr_safe
        };
        // Signal exits arrive as 128+sig from the shared outcome mapping
        // (was "signal=N" pre-timeout-support; rc=128+N is the same fact
        // in shell convention).
        let status_str = format!("rc={}", outcome.exit_code);
        let msg = if stderr_tail.is_empty() {
            format!("run: '{}' failed ({})", cmd_excerpt, status_str)
        } else {
            format!(
                "run: '{}' failed ({}): {}",
                cmd_excerpt, status_str, stderr_tail
            )
        };
        return Err(MixError::DieError { msg });
    }
    let stdout = String::from_utf8_lossy(&outcome.stdout)
        .trim_end()
        .to_string();
    Ok(Some(Value::String(stdout)))
}

/// Sanitize a string for embedding in a one-line diagnostic message.
/// Collapses line breaks (incl. U+2028/U+2029) to spaces; replaces C0/C1
/// controls and the common invisible/format characters (Trojan-Source class:
/// bidi overrides, isolates, zero-width spoofing, BOM) with '?'. Keeps
/// printable ASCII and non-spoofing Unicode intact.
pub(crate) fn sanitize_for_diag(s: &str) -> String {
    s.chars()
        .map(|c| match c {
            '\n' | '\r' | '\u{2028}' | '\u{2029}' => ' ',
            c if c.is_control() => '?',
            '\u{00AD}' | '\u{180E}' | '\u{200B}' | '\u{200C}' | '\u{200D}' | '\u{200E}'
            | '\u{200F}' | '\u{202A}' | '\u{202B}' | '\u{202C}' | '\u{202D}' | '\u{202E}'
            | '\u{2060}' | '\u{2061}' | '\u{2062}' | '\u{2063}' | '\u{2064}' | '\u{2066}'
            | '\u{2067}' | '\u{2068}' | '\u{2069}' | '\u{FEFF}' => '?',
            c => c,
        })
        .collect()
}

/// Every `spawn` argument is a string and none of them is coerced. Coercion
/// here is a silent-wrong-answer machine: `spawn(["touch", $p])` used to
/// stringify the list to its display form and hand `[touch, /path]` to `sh`,
/// which died with "command not found" — while `spawn` returned a perfectly
/// healthy PID, because it does not wait and has no result map to carry the
/// failure. Nothing downstream could tell. The opts maps of the other runners
/// are validated loudly for the same reason; this is that rule reaching the
/// one runner it had missed.
fn spawn_string_arg(what: &str, v: &Value) -> MixResult<String> {
    match v {
        Value::String(s) if s.contains('\0') => Err(MixError::structured(
            "TYPE_MISMATCH",
            format!("spawn: {what} contains a NUL byte"),
        )),
        Value::String(s) => Ok(s.clone()),
        Value::List(_) if what == "cmd" => Err(MixError::structured(
            "TYPE_MISMATCH",
            "spawn: cmd must be a shell command string, got list — spawn runs \
             `sh -c` and has no argv form; build the string with shell_quote(), \
             or use run_argv/run_argv_must for an argv list in the foreground"
                .to_string(),
        )),
        other => Err(MixError::structured(
            "TYPE_MISMATCH",
            format!(
                "spawn: {what} must be a string, got {} (no coercion — encode explicitly)",
                other.type_name()
            ),
        )),
    }
}

/// spawn(cmd, [stdout_path], [stderr_path]) — start a background process
/// via /bin/sh -c, return PID.
///
/// - 1 arg:  both stdout and stderr → /dev/null
/// - 2 args: stdout → file (truncated), stderr → /dev/null
/// - 3 args: stdout → file1, stderr → file2. Pass the same path for both
///   to merge them into a single combined log (like bash `&>file`).
fn builtin_spawn(args: Vec<Value>) -> MixResult<Option<Value>> {
    expect_args("spawn", &args, 1)?;
    let cmd = spawn_string_arg("cmd", &args[0])?;

    let mut command = std::process::Command::new("sh");
    command
        .arg("-c")
        .arg(&cmd)
        .stdin(std::process::Stdio::null());

    let stdout_path = args
        .get(1)
        .map(|v| spawn_string_arg("stdout_path", v))
        .transpose()?;
    let stderr_path = args
        .get(2)
        .map(|v| spawn_string_arg("stderr_path", v))
        .transpose()?;

    match (stdout_path.as_deref(), stderr_path.as_deref()) {
        (None, _) => {
            command
                .stdout(std::process::Stdio::null())
                .stderr(std::process::Stdio::null());
        }
        (Some(out), None) => {
            let f = std::fs::File::create(out).map_err(|e| MixError::RuntimeError {
                span: None,
                msg: format!("spawn: opening stdout {out:?}: {e}"),
            })?;
            command.stdout(f).stderr(std::process::Stdio::null());
        }
        (Some(out), Some(err)) if out == err => {
            // Merge stderr into stdout by cloning the file handle
            let f = std::fs::File::create(out).map_err(|e| MixError::RuntimeError {
                span: None,
                msg: format!("spawn: opening combined log {out:?}: {e}"),
            })?;
            let f2 = f.try_clone().map_err(|e| MixError::RuntimeError {
                span: None,
                msg: format!("spawn: cloning fd: {e}"),
            })?;
            command.stdout(f).stderr(f2);
        }
        (Some(out), Some(err)) => {
            let f_out = std::fs::File::create(out).map_err(|e| MixError::RuntimeError {
                span: None,
                msg: format!("spawn: opening stdout {out:?}: {e}"),
            })?;
            let f_err = std::fs::File::create(err).map_err(|e| MixError::RuntimeError {
                span: None,
                msg: format!("spawn: opening stderr {err:?}: {e}"),
            })?;
            command.stdout(f_out).stderr(f_err);
        }
    }

    let child = command.spawn().map_err(|e| MixError::RuntimeError {
        span: None,
        msg: format!("spawn failed: {}", e),
    })?;
    Ok(Some(Value::Number(child.id() as f64)))
}

/// kill(pid, [signal]) — send signal to process. Default signal: 15 (SIGTERM).
/// `kill`'s two arguments are numbers and neither is coerced, for the same
/// reason `spawn`'s are strings: `to_number()` maps `Bool(false)` to `0.0`
/// (value.rs), and `kill(0, …)` signals **every process in the caller's own
/// process group**. A pid that arrived as `false` from a failed lookup would
/// have taken down the script and its siblings while returning `true`. The
/// signal argument was worse: `.and_then(to_number).unwrap_or(15.0)` turned
/// `kill($p, "SIGKILL")` into a silent SIGTERM, so the caller believed it had
/// sent SIGKILL. Truncation is refused too — `kill($p, 9.5)` is a typo, not a
/// request for signal 9.
fn pid_int_arg(caller: &str, what: &str, v: &Value) -> MixResult<i32> {
    let n = match extract_number(v, InputPolicy::NumberOnly) {
        Some(n) => n,
        None => {
            // The parenthetical explains the danger of the PID specifically;
            // quoting it at someone who mistyped a *signal* would misdirect
            // the reader of the one message they see when this fires.
            let why = if what == "pid" {
                " (no coercion — a coerced pid of 0 addresses this process's entire group)"
            } else {
                " (no coercion — pass the signal number, e.g. 9 for SIGKILL; names are not accepted)"
            };
            return Err(MixError::structured(
                "TYPE_MISMATCH",
                format!(
                    "{caller}: {what} must be a number, got {}{why}",
                    v.type_name()
                ),
            ));
        }
    };
    // The domain gate filters entry, but pid/signal errors keep the
    // TYPE_MISMATCH code documented "strict since v0.52.0" in the builtin
    // table — scripts match on it, and recoding a pinned structured code is
    // a silent breaking change (0.59.0 review round 1, finding M4).
    Ok(as_exact_integer(
        &format!("{caller}: {what}"),
        n,
        i32::MIN as i64,
        i32::MAX as i64,
    )
    .map_err(|_| {
        let msg = if !n.is_finite() || n.fract() != 0.0 {
            format!("{caller}: {what} must be a whole number, got {n}")
        } else {
            format!("{caller}: {what} {n} is out of range")
        };
        MixError::structured("TYPE_MISMATCH", msg)
    })? as i32)
}

fn builtin_kill(args: Vec<Value>) -> MixResult<Option<Value>> {
    expect_args("kill", &args, 1)?;
    let pid = pid_int_arg("kill", "pid", &args[0])?;
    let signal = match args.get(1) {
        Some(v) => pid_int_arg("kill", "signal", v)?,
        None => 15,
    };
    #[cfg(unix)]
    {
        let result = unsafe { libc::kill(pid, signal) };
        Ok(Some(Value::Bool(result == 0)))
    }
    #[cfg(not(unix))]
    {
        let _ = (pid, signal);
        Err(MixError::RuntimeError {
            span: None,
            msg: "kill: not supported on this platform".into(),
        })
    }
}

/// process_alive(pid) — check if a process is running (signal 0 test).
///
/// First attempts a non-blocking `waitpid(pid, WNOHANG)` to reap the
/// process if it's an unreaped zombie child of THIS process — `spawn()`
/// installs no SIGCHLD handler, so children that exit before
/// `process_alive` is called sit as `<defunct>` entries that `kill -0`
/// still reports as live (the PID slot is occupied until reaped). That
/// was the bug fixed when SPEC 18 Phase 2 WS5's harness saw every
/// fan-out child time out at 10s even though each had completed and
/// written its result line in ~2s. `ECHILD` (not our child) is
/// ignored: those processes don't need reaping and the subsequent
/// `kill(0)` is the right liveness probe for them.
fn builtin_process_alive(args: Vec<Value>) -> MixResult<Option<Value>> {
    expect_args("process_alive", &args, 1)?;
    // Same rule as kill(), and the stakes are the same shape: a coerced pid of
    // 0 makes the waitpid() below reap an arbitrary child of this process's
    // group — a side effect, not just a wrong answer — and then kill(0, 0)
    // succeeds, so `process_alive(false)` returned TRUE.
    let pid = pid_int_arg("process_alive", "pid", &args[0])?;
    #[cfg(unix)]
    {
        let mut status: libc::c_int = 0;
        // SAFETY: waitpid is async-signal-safe; WNOHANG makes it
        // non-blocking. We ignore the return value because we only care
        // about its side-effect (reaping a zombie child) — the kill(0)
        // below is the authoritative liveness check.
        unsafe {
            let _ = libc::waitpid(pid, &mut status, libc::WNOHANG);
        }
        let alive = unsafe { libc::kill(pid, 0) } == 0;
        Ok(Some(Value::Bool(alive)))
    }
    #[cfg(not(unix))]
    {
        let _ = pid;
        Ok(Some(Value::Bool(false)))
    }
}

/// panic(msg) — abort via an uncatchable Rust `panic!`. Deliberately
/// distinct from `die` (a *catchable* `MixError::DieError`): this is the
/// hard-abort primitive the SPEC 18 §3.4 handler boundary (WS6) is
/// designed to isolate. In `mix --serve`, a panic raised inside an `on`
/// handler is caught by the per-handler `catch_unwind`, its payload
/// sanitized, and the supervisor keeps running — that survival is exactly
/// the SPEC 18 §9(c) acceptance gate. Outside a handler it aborts the
/// process like any Rust panic. The only intended caller is the §9(c)
/// acceptance affordance (a documented, guarded sentinel branch in the
/// statecache reference citizen); it has no production use.
fn builtin_panic(args: Vec<Value>) -> MixResult<Option<Value>> {
    expect_args("panic", &args, 1)?;
    let msg = args[0].to_mix_string();
    panic!("{msg}")
}

/// raise(code, message[, details]) — raise a catchable structured error
/// (0.29.0). `code` must match the D6 shape (`UPPER_SNAKE`); `details`
/// must be a map when given. The evaluator fills the traceback frames
/// at the dispatch site (builtins have no evaluator access).
fn builtin_raise(args: Vec<Value>) -> MixResult<Option<Value>> {
    expect_args_between("raise", &args, 2, 3)?;
    let code = match &args[0] {
        Value::String(s) => s.clone(),
        other => {
            return Err(MixError::RuntimeError {
                msg: format!("raise: code must be a string, got {}", other.type_name()),
                span: None,
            });
        }
    };
    if !crate::error::is_valid_error_code(&code) {
        return Err(MixError::RuntimeError {
            msg: format!(
                "raise: invalid error code '{}' — must be UPPER_SNAKE ([A-Z][A-Z0-9]*(_[A-Z0-9]+)*)",
                sanitize_for_diag(&code)
            ),
            span: None,
        });
    }
    let message = args[1].to_mix_string();
    let details = match args.get(2) {
        None | Some(Value::Nil) => Value::Nil,
        Some(v @ Value::Map(_)) => v.clone(),
        Some(other) => {
            return Err(MixError::RuntimeError {
                msg: format!("raise: details must be a map, got {}", other.type_name()),
                span: None,
            });
        }
    };
    Err(MixError::Structured(Box::new(
        crate::error::ErrorInfo::new(code, message).with_details(details),
    )))
}

// ---------- validation builtins (0.29.0, decision record D7) ----------
//
// Boundary validation for jobs / API handlers / forms: Mix's tolerant
// nil semantics stay the default everywhere, but these make STRICTNESS
// a one-call choice at ingress. All are Pure (no host authority);
// failures raise structured VALIDATION_* errors carrying details
// {path, expected, actual_type} so callers never parse message prose.

/// D7 type vocabulary for `expect_type` / `validate`'s `type` rule.
const VALIDATION_TYPE_NAMES: &[&str] = &[
    "any", "nil", "bool", "number", "integer", "string", "bytes", "buffer", "list", "map",
    "function",
];

/// Largest f64 that exactly represents every smaller whole number
/// (2^53 - 1) — the D7 bound for the `integer` validation type.
const MAX_SAFE_INTEGER: f64 = 9_007_199_254_740_991.0;

/// `Some(matches)` for a known type name, `None` for an unknown one
/// (the caller raises VALIDATION_SPEC).
fn validation_type_matches(v: &Value, tname: &str) -> Option<bool> {
    Some(match tname {
        "any" => true,
        "nil" => matches!(v, Value::Nil),
        "bool" => matches!(v, Value::Bool(_)),
        "number" => matches!(v, Value::Number(_)),
        "integer" => matches!(v, Value::Number(n)
            if n.is_finite() && n.fract() == 0.0 && n.abs() <= MAX_SAFE_INTEGER),
        "string" => matches!(v, Value::String(_)),
        "bytes" => matches!(v, Value::Bytes(_)),
        "buffer" => matches!(v, Value::Buffer(_)),
        "list" => matches!(v, Value::List(_)),
        "map" => matches!(v, Value::Map(_)),
        "function" => matches!(v, Value::Function(_)),
        _ => return None,
    })
}

/// A VALIDATION_* structured error with the D7 details shape.
fn validation_err(code: &str, msg: String, path: &str, expected: &str, actual: &Value) -> MixError {
    let mut d = indexmap::IndexMap::new();
    d.insert("path".to_string(), Value::String(path.to_string()));
    d.insert("expected".to_string(), Value::String(expected.to_string()));
    d.insert(
        "actual_type".to_string(),
        Value::String(actual.type_name().to_string()),
    );
    MixError::Structured(Box::new(
        crate::error::ErrorInfo::new(code, msg).with_details(Value::map(d)),
    ))
}

/// A VALIDATION_SPEC error with the same structured details shape as
/// the data-violation errors: `path` = where in the spec, `expected` =
/// the rule shape wanted, `actual_type` = what was found (or "absent").
fn validation_spec_err_at(
    path: &str,
    expected: &str,
    actual: Option<&Value>,
    msg: String,
) -> MixError {
    let mut d = indexmap::IndexMap::new();
    d.insert("path".to_string(), Value::String(path.to_string()));
    d.insert("expected".to_string(), Value::String(expected.to_string()));
    d.insert(
        "actual_type".to_string(),
        Value::String(actual.map_or("absent", |v| v.type_name()).to_string()),
    );
    MixError::Structured(Box::new(
        crate::error::ErrorInfo::new("VALIDATION_SPEC", msg).with_details(Value::map(d)),
    ))
}

fn validation_spec_err(msg: String) -> MixError {
    validation_spec_err_at("", "well-formed spec", None, msg)
}

/// Recursion ceiling for `validate`'s `items`/`schema` nesting. The
/// spec drives all recursion (value trees deeper than the spec are not
/// walked), so bounding spec depth bounds the whole validation — a
/// host-supplied 12k-deep spec must raise, not overflow the native
/// stack (codex C4 review, BLOCKER).
const VALIDATE_MAX_DEPTH: usize = 64;

/// Preflight one rule map (and everything reachable from it) BEFORE
/// any input is consulted: every rule payload is shape-checked so a
/// malformed spec fails loudly even when the current input would skip
/// the rule (optional absent field, empty list, first-match union...).
fn preflight_rules(
    path: &str,
    rules: &indexmap::IndexMap<String, Value>,
    depth: usize,
) -> MixResult<()> {
    if depth > VALIDATE_MAX_DEPTH {
        return Err(validation_spec_err_at(
            path,
            &format!("spec nesting <= {VALIDATE_MAX_DEPTH}"),
            None,
            format!("validate: spec nesting exceeds {VALIDATE_MAX_DEPTH} at {path}"),
        ));
    }
    for (rk, rv) in rules {
        match rk.as_str() {
            "required" | "nonblank" => {
                if !matches!(rv, Value::Bool(_)) {
                    return Err(validation_spec_err_at(
                        path,
                        &format!("'{rk}' as bool"),
                        Some(rv),
                        format!(
                            "validate: '{rk}' at {path} must be a bool, got {}",
                            rv.type_name()
                        ),
                    ));
                }
            }
            "type" => {
                let names: Vec<&str> = match rv {
                    Value::String(s) => vec![s.as_str()],
                    Value::List(items) if !items.is_empty() => {
                        let mut out = Vec::with_capacity(items.len());
                        for item in items.iter() {
                            match item {
                                Value::String(s) => out.push(s.as_str()),
                                other => {
                                    return Err(validation_spec_err_at(
                                        path,
                                        "'type' list of type-name strings",
                                        Some(other),
                                        format!(
                                            "validate: 'type' list at {path} must contain strings, got {}",
                                            other.type_name()
                                        ),
                                    ));
                                }
                            }
                        }
                        out
                    }
                    other => {
                        return Err(validation_spec_err_at(
                            path,
                            "'type' as string or non-empty list of strings",
                            Some(other),
                            format!(
                                "validate: 'type' at {path} must be a string or non-empty list of strings, got {}",
                                other.type_name()
                            ),
                        ));
                    }
                };
                for name in names {
                    if !VALIDATION_TYPE_NAMES.contains(&name) {
                        return Err(validation_spec_err_at(
                            path,
                            &format!("one of: {}", VALIDATION_TYPE_NAMES.join(", ")),
                            None,
                            format!(
                                "validate: unknown type '{}' at {} (known: {})",
                                sanitize_for_diag(name),
                                path,
                                VALIDATION_TYPE_NAMES.join(", ")
                            ),
                        ));
                    }
                }
            }
            "enum" => {
                if !matches!(rv, Value::List(items) if !items.is_empty()) {
                    return Err(validation_spec_err_at(
                        path,
                        "'enum' as non-empty list",
                        Some(rv),
                        format!(
                            "validate: 'enum' at {path} must be a non-empty list, got {}",
                            rv.type_name()
                        ),
                    ));
                }
            }
            "min" | "max" => {
                if !matches!(rv, Value::Number(n) if n.is_finite()) {
                    return Err(validation_spec_err_at(
                        path,
                        &format!("'{rk}' as finite number"),
                        Some(rv),
                        format!(
                            "validate: '{rk}' at {path} must be a finite number, got {}",
                            rv.to_mix_string()
                        ),
                    ));
                }
            }
            "min_length" | "max_length" => {
                if !matches!(rv, Value::Number(n)
                    if n.is_finite() && *n >= 0.0 && n.fract() == 0.0)
                {
                    return Err(validation_spec_err_at(
                        path,
                        &format!("'{rk}' as non-negative whole number"),
                        Some(rv),
                        format!(
                            "validate: '{rk}' at {path} must be a non-negative whole number, got {}",
                            rv.to_mix_string()
                        ),
                    ));
                }
            }
            "items" => match rv {
                Value::Map(m) => preflight_rules(&format!("{path}[]"), m, depth + 1)?,
                other => {
                    return Err(validation_spec_err_at(
                        path,
                        "'items' as rule map",
                        Some(other),
                        format!(
                            "validate: 'items' at {path} must be a rule map, got {}",
                            other.type_name()
                        ),
                    ));
                }
            },
            "schema" => match rv {
                Value::Map(m) => preflight_spec(path, m, depth + 1)?,
                other => {
                    return Err(validation_spec_err_at(
                        path,
                        "'schema' as field-spec map",
                        Some(other),
                        format!(
                            "validate: 'schema' at {path} must be a field-spec map, got {}",
                            other.type_name()
                        ),
                    ));
                }
            },
            other => {
                return Err(validation_spec_err_at(
                    path,
                    &format!("one of: {}", VALIDATE_RULE_KEYS.join(", ")),
                    Some(rv),
                    format!(
                        "validate: unknown rule '{}' at {} (known: {})",
                        sanitize_for_diag(other),
                        path,
                        VALIDATE_RULE_KEYS.join(", ")
                    ),
                ));
            }
        }
    }
    // Ordered-bounds sanity while both are known-finite.
    if let (Some(Value::Number(lo)), Some(Value::Number(hi))) = (rules.get("min"), rules.get("max"))
        && lo > hi
    {
        return Err(validation_spec_err_at(
            path,
            "min <= max",
            None,
            format!("validate: min {lo} > max {hi} at {path}"),
        ));
    }
    if let (Some(Value::Number(lo)), Some(Value::Number(hi))) =
        (rules.get("min_length"), rules.get("max_length"))
        && lo > hi
    {
        return Err(validation_spec_err_at(
            path,
            "min_length <= max_length",
            None,
            format!("validate: min_length {lo} > max_length {hi} at {path}"),
        ));
    }
    Ok(())
}

/// Preflight a whole field-spec map (each value must be a rule map).
fn preflight_spec(
    base: &str,
    spec: &indexmap::IndexMap<String, Value>,
    depth: usize,
) -> MixResult<()> {
    if depth > VALIDATE_MAX_DEPTH {
        return Err(validation_spec_err_at(
            base,
            &format!("spec nesting <= {VALIDATE_MAX_DEPTH}"),
            None,
            format!("validate: spec nesting exceeds {VALIDATE_MAX_DEPTH} at {base}"),
        ));
    }
    for (field, rules_v) in spec {
        let path = validate_join_path(base, field);
        match rules_v {
            // Same depth — a field's rule map is not a new nesting
            // LEVEL; only an items/schema EDGE deepens (codex release
            // review, MINOR: schema was counting twice per level).
            Value::Map(m) => preflight_rules(&path, m, depth)?,
            other => {
                return Err(validation_spec_err_at(
                    &path,
                    "rule map",
                    Some(other),
                    format!(
                        "validate: spec for field '{}' must be a rule map, got {}",
                        sanitize_for_diag(field),
                        other.type_name()
                    ),
                ));
            }
        }
    }
    Ok(())
}

/// Join a violation path: dot notation for identifier-safe keys,
/// escaped-bracket notation otherwise, so a literal field named
/// "owner.name" cannot alias a nested one (codex C4 review).
fn validate_join_path(base: &str, field: &str) -> String {
    let ident_safe = !field.is_empty()
        && field
            .chars()
            .next()
            .is_some_and(|c| c.is_ascii_alphabetic() || c == '_')
        && field.chars().all(|c| c.is_ascii_alphanumeric() || c == '_');
    if ident_safe {
        if base.is_empty() {
            field.to_string()
        } else {
            format!("{base}.{field}")
        }
    } else {
        format!("{base}[\"{}\"]", field.replace('"', "\\\""))
    }
}

/// require_key(map, key) — the key must be present with a non-nil
/// value; returns that value.
fn builtin_require_key(args: Vec<Value>) -> MixResult<Option<Value>> {
    expect_args_between("require_key", &args, 2, 2)?;
    let m = match &args[0] {
        Value::Map(m) => m,
        other => {
            return Err(MixError::structured(
                "TYPE_MISMATCH",
                format!(
                    "require_key: first argument must be a map, got {}",
                    other.type_name()
                ),
            ));
        }
    };
    let key = args[1].to_mix_string();
    match m.get(&key) {
        Some(v) if !matches!(v, Value::Nil) => Ok(Some(v.clone())),
        held => Err(validation_err(
            "VALIDATION_REQUIRED",
            format!(
                "require_key: required key '{}' is missing or nil",
                sanitize_for_diag(&key)
            ),
            &key,
            "present non-nil value",
            held.unwrap_or(&Value::Nil),
        )),
    }
}

/// expect_type(value, kind) — assert the D7 type, return the value.
fn builtin_expect_type(args: Vec<Value>) -> MixResult<Option<Value>> {
    expect_args_between("expect_type", &args, 2, 2)?;
    let kind = match &args[1] {
        Value::String(s) => s.as_str(),
        other => {
            return Err(MixError::structured(
                "TYPE_MISMATCH",
                format!(
                    "expect_type: type name must be a string, got {}",
                    other.type_name()
                ),
            ));
        }
    };
    match validation_type_matches(&args[0], kind) {
        None => Err(validation_spec_err_at(
            "value",
            &format!("one of: {}", VALIDATION_TYPE_NAMES.join(", ")),
            Some(&args[1]),
            format!(
                "expect_type: unknown type '{}' (known: {})",
                sanitize_for_diag(kind),
                VALIDATION_TYPE_NAMES.join(", ")
            ),
        )),
        Some(true) => Ok(Some(args[0].clone())),
        Some(false) => Err(validation_err(
            "VALIDATION_TYPE",
            format!(
                "expect_type: expected {}, got {}",
                kind,
                args[0].type_name()
            ),
            "value",
            kind,
            &args[0],
        )),
    }
}

/// nonblank(value[, label]) — a string with at least one
/// non-whitespace character; returned UNTRIMMED.
fn builtin_nonblank(args: Vec<Value>) -> MixResult<Option<Value>> {
    expect_args_between("nonblank", &args, 1, 2)?;
    let label = match args.get(1) {
        None | Some(Value::Nil) => "value".to_string(),
        Some(Value::String(s)) => s.clone(),
        Some(other) => {
            return Err(MixError::structured(
                "TYPE_MISMATCH",
                format!(
                    "nonblank: label must be a string, got {}",
                    other.type_name()
                ),
            ));
        }
    };
    match &args[0] {
        Value::String(s) if s.chars().any(|c| !c.is_whitespace()) => Ok(Some(args[0].clone())),
        other => Err(validation_err(
            "VALIDATION_NONBLANK",
            format!("{}: must be a non-blank string", sanitize_for_diag(&label)),
            &label,
            "non-blank string",
            other,
        )),
    }
}

/// get_or(map, key, default) — the default covers BOTH absent and nil
/// (the tolerant twin of require_key).
fn builtin_get_or(args: Vec<Value>) -> MixResult<Option<Value>> {
    expect_args_between("get_or", &args, 3, 3)?;
    let m = match &args[0] {
        Value::Map(m) => m,
        other => {
            return Err(MixError::structured(
                "TYPE_MISMATCH",
                format!(
                    "get_or: first argument must be a map, got {}",
                    other.type_name()
                ),
            ));
        }
    };
    let key = args[1].to_mix_string();
    match m.get(&key) {
        Some(v) if !matches!(v, Value::Nil) => Ok(Some(v.clone())),
        _ => Ok(Some(args[2].clone())),
    }
}

const VALIDATE_RULE_KEYS: &[&str] = &[
    "required",
    "type",
    "nonblank",
    "enum",
    "min",
    "max",
    "min_length",
    "max_length",
    "items",
    "schema",
];

/// The countable length for min_length/max_length: string codepoints
/// (matching `length()`), list items, map entries.
fn validate_length_of(v: &Value) -> Option<usize> {
    match v {
        Value::String(s) => Some(s.chars().count()),
        Value::List(items) => Some(items.len()),
        Value::Map(m) => Some(m.len()),
        _ => None,
    }
}

/// Apply one field's rule map to an (optional) value at `path`.
fn validate_one(
    path: &str,
    value: Option<&Value>,
    rules: &indexmap::IndexMap<String, Value>,
) -> MixResult<()> {
    // Spec sanity first: unknown rule keys are a spec error even when
    // the value would pass — a typo'd rule must never silently no-op.
    let mut required = true;
    for (rk, rv) in rules {
        match rk.as_str() {
            "required" => {
                required = match rv {
                    Value::Bool(b) => *b,
                    other => {
                        return Err(validation_spec_err(format!(
                            "validate: 'required' at {} must be a bool, got {}",
                            path,
                            other.type_name()
                        )));
                    }
                };
            }
            k if VALIDATE_RULE_KEYS.contains(&k) => {}
            other => {
                return Err(validation_spec_err(format!(
                    "validate: unknown rule '{}' at {} (known: {})",
                    sanitize_for_diag(other),
                    path,
                    VALIDATE_RULE_KEYS.join(", ")
                )));
            }
        }
    }
    let v = match value {
        None | Some(Value::Nil) => {
            if required {
                return Err(validation_err(
                    "VALIDATION_REQUIRED",
                    format!("validate: {path} is required"),
                    path,
                    "present non-nil value",
                    value.unwrap_or(&Value::Nil),
                ));
            }
            // Optional and absent/nil: every remaining rule is skipped.
            return Ok(());
        }
        Some(v) => v,
    };
    if let Some(tv) = rules.get("type") {
        let names: Vec<String> = match tv {
            Value::String(s) => vec![s.clone()],
            Value::List(items) if !items.is_empty() => {
                let mut out = Vec::with_capacity(items.len());
                for item in items.iter() {
                    match item {
                        Value::String(s) => out.push(s.clone()),
                        other => {
                            return Err(validation_spec_err(format!(
                                "validate: 'type' list at {} must contain strings, got {}",
                                path,
                                other.type_name()
                            )));
                        }
                    }
                }
                out
            }
            other => {
                return Err(validation_spec_err(format!(
                    "validate: 'type' at {} must be a string or non-empty list of strings, got {}",
                    path,
                    other.type_name()
                )));
            }
        };
        let mut matched = false;
        for name in &names {
            match validation_type_matches(v, name) {
                None => {
                    return Err(validation_spec_err(format!(
                        "validate: unknown type '{}' at {} (known: {})",
                        sanitize_for_diag(name),
                        path,
                        VALIDATION_TYPE_NAMES.join(", ")
                    )));
                }
                Some(true) => {
                    matched = true;
                    break;
                }
                Some(false) => {}
            }
        }
        if !matched {
            let expected = names.join(" | ");
            return Err(validation_err(
                "VALIDATION_TYPE",
                format!("validate: {path} must be {expected}, got {}", v.type_name()),
                path,
                &expected,
                v,
            ));
        }
    }
    if let Some(nb) = rules.get("nonblank") {
        let want = match nb {
            Value::Bool(b) => *b,
            other => {
                return Err(validation_spec_err(format!(
                    "validate: 'nonblank' at {} must be a bool, got {}",
                    path,
                    other.type_name()
                )));
            }
        };
        if want && !matches!(v, Value::String(s) if s.chars().any(|c| !c.is_whitespace())) {
            return Err(validation_err(
                "VALIDATION_NONBLANK",
                format!("validate: {path} must be a non-blank string"),
                path,
                "non-blank string",
                v,
            ));
        }
    }
    if let Some(ev) = rules.get("enum") {
        let cands = match ev {
            Value::List(items) if !items.is_empty() => items,
            other => {
                return Err(validation_spec_err(format!(
                    "validate: 'enum' at {} must be a non-empty list, got {}",
                    path,
                    other.type_name()
                )));
            }
        };
        // Normal Mix equality (Value::PartialEq: number<->numeric-string
        // coercion included).
        if !cands.iter().any(|c| c == v) {
            let expected: Vec<String> = cands.iter().map(|c| c.to_mix_string()).collect();
            let expected = expected.join(" | ");
            return Err(validation_err(
                "VALIDATION_ENUM",
                format!("validate: {path} must be one of: {expected}"),
                path,
                &expected,
                v,
            ));
        }
    }
    for bound_key in ["min", "max"] {
        if let Some(bv) = rules.get(bound_key) {
            let bound = match bv {
                Value::Number(n) => *n,
                other => {
                    return Err(validation_spec_err(format!(
                        "validate: '{}' at {} must be a number, got {}",
                        bound_key,
                        path,
                        other.type_name()
                    )));
                }
            };
            let n = match v {
                Value::Number(n) => *n,
                other => {
                    return Err(validation_err(
                        "VALIDATION_TYPE",
                        format!("validate: {path} must be a number ({bound_key} rule)"),
                        path,
                        "number",
                        other,
                    ));
                }
            };
            let violated = if bound_key == "min" {
                n < bound
            } else {
                n > bound
            };
            if violated {
                let expected = format!("{bound_key} {bound}");
                return Err(validation_err(
                    "VALIDATION_RANGE",
                    format!("validate: {path} is {n}, violates {expected} (inclusive)"),
                    path,
                    &expected,
                    v,
                ));
            }
        }
    }
    for bound_key in ["min_length", "max_length"] {
        if let Some(bv) = rules.get(bound_key) {
            let n = match extract_number(bv, InputPolicy::NumberOnly) {
                Some(n) => n,
                None => {
                    return Err(validation_spec_err(format!(
                        "validate: '{}' at {} must be a non-negative whole number, got {}",
                        bound_key,
                        path,
                        bv.to_mix_string()
                    )));
                }
            };
            // Domain failures keep the documented VALIDATION_* contract —
            // 0.29.0's promise is that a bad spec dies at ingress with a
            // VALIDATION_* code, not a generic range error.
            let bound = as_count(&format!("validate: '{bound_key}' at {path}"), n, usize::MAX)
                .map_err(|_| {
                    validation_spec_err(format!(
                        "validate: '{}' at {} must be a non-negative whole number, got {}",
                        bound_key,
                        path,
                        bv.to_mix_string()
                    ))
                })?;
            let len = match validate_length_of(v) {
                Some(len) => len,
                None => {
                    return Err(validation_err(
                        "VALIDATION_TYPE",
                        format!(
                            "validate: {path} must be a string, list, or map ({bound_key} rule)"
                        ),
                        path,
                        "string | list | map",
                        v,
                    ));
                }
            };
            let violated = if bound_key == "min_length" {
                len < bound
            } else {
                len > bound
            };
            if violated {
                let expected = format!("{bound_key} {bound}");
                return Err(validation_err(
                    "VALIDATION_LENGTH",
                    format!("validate: {path} has length {len}, violates {expected} (inclusive)"),
                    path,
                    &expected,
                    v,
                ));
            }
        }
    }
    if let Some(iv) = rules.get("items") {
        let item_rules = match iv {
            Value::Map(m) => m,
            other => {
                return Err(validation_spec_err(format!(
                    "validate: 'items' at {} must be a rule map, got {}",
                    path,
                    other.type_name()
                )));
            }
        };
        let items = match v {
            Value::List(items) => items,
            other => {
                return Err(validation_err(
                    "VALIDATION_TYPE",
                    format!("validate: {path} must be a list (items rule)"),
                    path,
                    "list",
                    other,
                ));
            }
        };
        for (i, item) in items.iter().enumerate() {
            validate_one(&format!("{path}[{i}]"), Some(item), item_rules)?;
        }
    }
    if let Some(sv) = rules.get("schema") {
        let field_spec = match sv {
            Value::Map(m) => m,
            other => {
                return Err(validation_spec_err(format!(
                    "validate: 'schema' at {} must be a field-spec map, got {}",
                    path,
                    other.type_name()
                )));
            }
        };
        let inner = match v {
            Value::Map(m) => m,
            other => {
                return Err(validation_err(
                    "VALIDATION_TYPE",
                    format!("validate: {path} must be a map (schema rule)"),
                    path,
                    "map",
                    other,
                ));
            }
        };
        validate_fields(path, inner, field_spec)?;
    }
    Ok(())
}

fn validate_fields(
    base: &str,
    input: &indexmap::IndexMap<String, Value>,
    spec: &indexmap::IndexMap<String, Value>,
) -> MixResult<()> {
    for (field, rules_v) in spec {
        let rules = match rules_v {
            Value::Map(m) => m,
            other => {
                return Err(validation_spec_err(format!(
                    "validate: spec for field '{}' must be a rule map, got {}",
                    sanitize_for_diag(field),
                    other.type_name()
                )));
            }
        };
        let path = validate_join_path(base, field);
        validate_one(&path, input.get(field), rules)?;
    }
    Ok(())
}

/// validate(value, spec) — the boundary validator. Returns the
/// ORIGINAL map unchanged on success (composable, no hidden
/// normalization); unknown input fields are preserved and ignored.
fn builtin_validate(args: Vec<Value>) -> MixResult<Option<Value>> {
    expect_args_between("validate", &args, 2, 2)?;
    let input = match &args[0] {
        Value::Map(m) => m,
        other => {
            return Err(MixError::structured(
                "TYPE_MISMATCH",
                format!(
                    "validate: first argument must be a map, got {}",
                    other.type_name()
                ),
            ));
        }
    };
    let spec = match &args[1] {
        Value::Map(m) => m,
        other => {
            return Err(validation_spec_err_at(
                "",
                "field-spec map",
                Some(other),
                format!("validate: spec must be a map, got {}", other.type_name()),
            ));
        }
    };
    // Two passes (codex C4 review, MAJOR): the WHOLE spec tree is
    // shape-checked first, so a typo'd rule fails loudly even when the
    // current input would never exercise it (optional absent field,
    // empty list, satisfied union...). Then the input is validated.
    preflight_spec("", spec, 0)?;
    validate_fields("", input, spec)?;
    Ok(Some(args[0].clone()))
}

/// run_rc(cmd) — run command via sh, return map with {rc, stdout, stderr}.
/// `run_rc(cmd, [{timeout: seconds}])` — `{rc, stdout, stderr, timed_out,
/// interrupted}`, never raises on a non-zero exit.
///
/// Same `run_with_timeout` machinery as `run`/`ssh_run`: default 0 = no
/// deadline; on timeout `rc` is -1 with `timed_out: true` (the child's
/// process group is SIGKILLed), on Ctrl-C `rc` is -2 with `interrupted:
/// true`; a signal-killed child reports rc = 128+sig.
fn builtin_run_rc(args: Vec<Value>) -> MixResult<Option<Value>> {
    expect_args_between("run_rc", &args, 1, 2)?;
    let cmd = args[0].to_mix_string();
    let timeout_s = parse_timeout_opt("run_rc", args.get(1), 0)?;
    let argv = vec!["sh".to_string(), "-c".to_string(), cmd];
    let outcome = run_with_timeout(&argv, None, timeout_s, "run_rc")?;
    let mut map = indexmap::IndexMap::new();
    map.insert("rc".into(), Value::Number(outcome.exit_code as f64));
    map.insert(
        "stdout".into(),
        Value::String(
            String::from_utf8_lossy(&outcome.stdout)
                .trim_end()
                .to_string(),
        ),
    );
    map.insert(
        "stderr".into(),
        Value::String(
            String::from_utf8_lossy(&outcome.stderr)
                .trim_end()
                .to_string(),
        ),
    );
    map.insert("timed_out".into(), Value::Bool(outcome.timed_out));
    map.insert("interrupted".into(), Value::Bool(outcome.interrupted));
    Ok(Some(Value::map(map)))
}

/// Parsed + validated `run_argv` options (decision record D4). Every
/// validation failure raises BEFORE the child is spawned: argv problems
/// as `TYPE_MISMATCH`, option problems as `OPTION_INVALID`.
struct RunArgvOpts {
    timeout_ms: u64,
    stdin: RunArgvStdin,
    cwd: Option<String>,
    env: Vec<(String, String)>,
    clear_env: bool,
    max_output: Option<usize>,
    stream: bool,
    stdout: RunArgvOutput,
    stderr: RunArgvStderr,
}

enum RunArgvStdin {
    Null,
    Data(Vec<u8>),
    File(String),
}

#[derive(Clone)]
struct RunArgvFile {
    path: String,
    append: bool,
    mode: u32,
}

enum RunArgvOutput {
    Capture,
    Inherit,
    Null,
    File(RunArgvFile),
}

enum RunArgvStderr {
    Capture,
    Inherit,
    Null,
    Stdout,
    File(RunArgvFile),
}

const RUN_ARGV_OPT_KEYS: &[&str] = &[
    "timeout",
    "stdin",
    "cwd",
    "env",
    "clear_env",
    "max_output",
    "stream",
    "stdout",
    "stderr",
];

/// Default per-stream capture cap: 8 MiB (D4).
const RUN_ARGV_DEFAULT_MAX_OUTPUT: usize = 8 * 1024 * 1024;

fn run_argv_env_key_ok(k: &str) -> bool {
    let mut chars = k.chars();
    match chars.next() {
        Some(c) if c.is_ascii_alphabetic() || c == '_' => {}
        _ => return false,
    }
    chars.all(|c| c.is_ascii_alphanumeric() || c == '_')
}

fn parse_run_argv_argv(caller: &str, v: &Value) -> MixResult<Vec<String>> {
    let items = match v {
        Value::List(items) => items,
        other => {
            return Err(MixError::structured(
                "TYPE_MISMATCH",
                format!(
                    "{caller}: argv must be a list of strings, got {}",
                    other.type_name()
                ),
            ));
        }
    };
    if items.is_empty() {
        return Err(MixError::structured(
            "TYPE_MISMATCH",
            format!("{caller}: argv must not be empty"),
        ));
    }
    let mut argv = Vec::with_capacity(items.len());
    for (i, item) in items.iter().enumerate() {
        match item {
            Value::String(s) => {
                if s.contains('\0') {
                    return Err(MixError::structured(
                        "TYPE_MISMATCH",
                        format!("{caller}: argv[{i}] contains a NUL byte"),
                    ));
                }
                argv.push(s.clone());
            }
            other => {
                return Err(MixError::structured(
                    "TYPE_MISMATCH",
                    format!(
                        "{caller}: argv[{i}] must be a string, got {} (no coercion — encode explicitly)",
                        other.type_name()
                    ),
                ));
            }
        }
    }
    Ok(argv)
}

fn opt_invalid(caller: &str, msg: impl std::fmt::Display) -> MixError {
    MixError::structured("OPTION_INVALID", format!("{caller}: {msg}"))
}

fn parse_stdio_path(caller: &str, stream: &str, value: &Value) -> MixResult<String> {
    match value {
        Value::String(path) if path.contains('\0') => Err(opt_invalid(
            caller,
            format!("{stream} file path contains a NUL byte"),
        )),
        Value::String(path) => Ok(path.clone()),
        other => Err(opt_invalid(
            caller,
            format!("{stream} file must be a string, got {}", other.type_name()),
        )),
    }
}

fn parse_output_file(caller: &str, stream: &str, value: &Value) -> MixResult<RunArgvFile> {
    let map = match value {
        Value::Map(map) => map,
        other => {
            return Err(opt_invalid(
                caller,
                format!(
                    "{stream} must be a routing string or file map, got {}",
                    other.type_name()
                ),
            ));
        }
    };
    for (key, _) in map.iter() {
        if !matches!(key.as_str(), "file" | "append" | "mode") {
            return Err(opt_invalid(
                caller,
                format!(
                    "unknown {stream} file option '{}' (supported: file, append, mode)",
                    sanitize_for_diag(key)
                ),
            ));
        }
    }
    let path = match map.get("file") {
        Some(value) => parse_stdio_path(caller, stream, value)?,
        None => {
            return Err(opt_invalid(
                caller,
                format!("{stream} file map requires `file`"),
            ));
        }
    };
    let append = match map.get("append") {
        None => false,
        Some(Value::Bool(value)) => *value,
        Some(other) => {
            return Err(opt_invalid(
                caller,
                format!("{stream} append must be a bool, got {}", other.type_name()),
            ));
        }
    };
    let mode = match map.get("mode") {
        None => 0o600,
        Some(Value::Number(value))
            if value.is_finite()
                && *value >= 0.0
                && value.fract() == 0.0
                && *value <= 0o7777 as f64 =>
        {
            *value as u32
        }
        Some(Value::Number(value)) => {
            return Err(opt_invalid(
                caller,
                format!("{stream} mode must be a whole number from 0 to 0o7777, got {value}"),
            ));
        }
        Some(other) => {
            return Err(opt_invalid(
                caller,
                format!("{stream} mode must be a number, got {}", other.type_name()),
            ));
        }
    };
    Ok(RunArgvFile { path, append, mode })
}

fn parse_stdin_route(caller: &str, value: &Value) -> MixResult<RunArgvStdin> {
    match value {
        Value::Nil => Ok(RunArgvStdin::Null),
        Value::String(value) => Ok(RunArgvStdin::Data(value.as_bytes().to_vec())),
        Value::Bytes(value) => Ok(RunArgvStdin::Data(value.as_ref().clone())),
        Value::Buffer(value) => Ok(RunArgvStdin::Data(value.borrow().clone())),
        Value::Map(map) => {
            if map.len() != 1 {
                return Err(opt_invalid(
                    caller,
                    "stdin routing map must be exactly {file: string} or {null: true}",
                ));
            }
            match map.first() {
                Some((key, value)) if key == "file" => Ok(RunArgvStdin::File(parse_stdio_path(
                    caller, "stdin", value,
                )?)),
                Some((key, Value::Bool(true))) if key == "null" => Ok(RunArgvStdin::Null),
                _ => Err(opt_invalid(
                    caller,
                    "stdin routing map must be exactly {file: string} or {null: true}",
                )),
            }
        }
        other => Err(opt_invalid(
            caller,
            format!(
                "stdin must be nil, a string, bytes, buffer, {{file: string}}, or {{null: true}}, got {}",
                other.type_name()
            ),
        )),
    }
}

fn parse_stdout_route(caller: &str, value: &Value) -> MixResult<RunArgvOutput> {
    match value {
        Value::String(value) => match value.as_str() {
            "capture" => Ok(RunArgvOutput::Capture),
            "inherit" => Ok(RunArgvOutput::Inherit),
            "null" => Ok(RunArgvOutput::Null),
            other => Err(opt_invalid(
                caller,
                format!(
                    "stdout must be \"capture\", \"inherit\", \"null\", or a file map, got {:?}",
                    sanitize_for_diag(other)
                ),
            )),
        },
        Value::Map(_) => parse_output_file(caller, "stdout", value).map(RunArgvOutput::File),
        other => Err(opt_invalid(
            caller,
            format!(
                "stdout must be a routing string or file map, got {}",
                other.type_name()
            ),
        )),
    }
}

fn parse_stderr_route(caller: &str, value: &Value) -> MixResult<RunArgvStderr> {
    match value {
        Value::String(value) => match value.as_str() {
            "capture" => Ok(RunArgvStderr::Capture),
            "inherit" => Ok(RunArgvStderr::Inherit),
            "null" => Ok(RunArgvStderr::Null),
            "stdout" => Ok(RunArgvStderr::Stdout),
            other => Err(opt_invalid(
                caller,
                format!(
                    "stderr must be \"capture\", \"inherit\", \"null\", \"stdout\", or a file map, got {:?}",
                    sanitize_for_diag(other)
                ),
            )),
        },
        Value::Map(_) => parse_output_file(caller, "stderr", value).map(RunArgvStderr::File),
        other => Err(opt_invalid(
            caller,
            format!(
                "stderr must be a routing string or file map, got {}",
                other.type_name()
            ),
        )),
    }
}

fn parse_run_argv_opts(caller: &str, v: Option<&Value>) -> MixResult<RunArgvOpts> {
    let mut opts = RunArgvOpts {
        timeout_ms: 30_000,
        stdin: RunArgvStdin::Null,
        cwd: None,
        env: Vec::new(),
        clear_env: false,
        max_output: Some(RUN_ARGV_DEFAULT_MAX_OUTPUT),
        stream: false,
        stdout: RunArgvOutput::Capture,
        stderr: RunArgvStderr::Capture,
    };
    let map = match v {
        None | Some(Value::Nil) => return Ok(opts),
        Some(Value::Map(m)) => m,
        Some(other) => {
            return Err(opt_invalid(
                caller,
                format!("options must be a map, got {}", other.type_name()),
            ));
        }
    };
    for (k, val) in map.iter() {
        match k.as_str() {
            "timeout" => {
                let t = match extract_number(val, InputPolicy::NumberOnly) {
                    Some(n) => n,
                    None => {
                        return Err(opt_invalid(
                            caller,
                            format!(
                                "timeout must be a number of seconds, got {}",
                                val.type_name()
                            ),
                        ));
                    }
                };
                // The domain gate decides which values may ENTER; the option
                // boundary keeps its established OPTION_INVALID contract
                // (scripts match on $e.code — recoding it to
                // VALUE_OUT_OF_RANGE would be a silent breaking change).
                as_duration(&format!("{caller}: timeout"), t).map_err(|_| {
                    opt_invalid(
                        caller,
                        format!("timeout must be a finite non-negative number, got {t}"),
                    )
                })?;
                // A positive sub-millisecond timeout must stay a
                // deadline — rounding it to 0 would DISABLE it.
                opts.timeout_ms = if t > 0.0 {
                    (as_count(
                        &format!("{caller}: timeout milliseconds"),
                        (t * 1000.0).round(),
                        usize::MAX,
                    )
                    .map_err(|_| opt_invalid(caller, format!("timeout {t}s is out of range")))?
                        as u64)
                        .max(1)
                } else {
                    0
                };
            }
            "stdin" => {
                opts.stdin = parse_stdin_route(caller, val)?;
            }
            "cwd" => {
                opts.cwd = match val {
                    Value::Nil => None,
                    Value::String(s) if s.contains('\0') => {
                        return Err(opt_invalid(caller, "cwd contains a NUL byte"));
                    }
                    Value::String(s) => Some(s.clone()),
                    other => {
                        return Err(opt_invalid(
                            caller,
                            format!("cwd must be nil or a string, got {}", other.type_name()),
                        ));
                    }
                };
            }
            "env" => {
                let m = match val {
                    Value::Map(m) => m,
                    other => {
                        return Err(opt_invalid(
                            caller,
                            format!("env must be a map, got {}", other.type_name()),
                        ));
                    }
                };
                for (ek, ev) in m.iter() {
                    if !run_argv_env_key_ok(ek) {
                        return Err(opt_invalid(
                            caller,
                            format!(
                                "env key '{}' is not a valid name ([A-Za-z_][A-Za-z0-9_]*)",
                                sanitize_for_diag(ek)
                            ),
                        ));
                    }
                    let sval = match ev {
                        Value::String(s) => s.clone(),
                        Value::Number(_) | Value::Bool(_) => ev.to_mix_string(),
                        other => {
                            return Err(opt_invalid(
                                caller,
                                format!(
                                    "env value for '{ek}' must be a string, number, or bool, got {}",
                                    other.type_name()
                                ),
                            ));
                        }
                    };
                    if sval.contains('\0') {
                        return Err(opt_invalid(
                            caller,
                            format!("env value for '{ek}' contains a NUL byte"),
                        ));
                    }
                    opts.env.push((ek.clone(), sval));
                }
            }
            "clear_env" => {
                opts.clear_env = match val {
                    Value::Bool(b) => *b,
                    other => {
                        return Err(opt_invalid(
                            caller,
                            format!("clear_env must be a bool, got {}", other.type_name()),
                        ));
                    }
                };
            }
            "max_output" => {
                let n = match extract_number(val, InputPolicy::NumberOnly) {
                    Some(n) => n,
                    None => {
                        return Err(opt_invalid(
                            caller,
                            format!(
                                "max_output must be a number of bytes, got {}",
                                val.type_name()
                            ),
                        ));
                    }
                };
                // Same rule as timeout above: the option boundary keeps its
                // OPTION_INVALID contract; the domain gate only filters entry.
                let n = as_count(&format!("{caller}: max_output"), n, usize::MAX).map_err(|_| {
                    opt_invalid(
                        caller,
                        format!(
                            "max_output must be a non-negative whole number within \u{b1}2^53-1, got {n}"
                        ),
                    )
                })?;
                opts.max_output = if n == 0 { None } else { Some(n) };
            }
            "stream" => {
                opts.stream = match val {
                    Value::Bool(b) => *b,
                    other => {
                        return Err(opt_invalid(
                            caller,
                            format!("stream must be a bool, got {}", other.type_name()),
                        ));
                    }
                };
            }
            "stdout" => opts.stdout = parse_stdout_route(caller, val)?,
            "stderr" => opts.stderr = parse_stderr_route(caller, val)?,
            other => {
                return Err(opt_invalid(
                    caller,
                    format!(
                        "unknown option '{}' (supported: {})",
                        sanitize_for_diag(other),
                        RUN_ARGV_OPT_KEYS.join(", ")
                    ),
                ));
            }
        }
    }
    if opts.stream && matches!(opts.stdout, RunArgvOutput::Inherit) {
        return Err(opt_invalid(
            caller,
            "stream:true cannot be combined with stdout:\"inherit\"",
        ));
    }
    Ok(opts)
}

/// Build the D4 `process_result` map from an engine outcome. Field
/// order is the documented schema order.
fn run_argv_result_map(o: &ProcOutcome) -> Value {
    // Exact lossiness: check the raw bytes, so output that legitimately
    // contains U+FFFD is not falsely flagged (unlike the ssh_result_map
    // heuristic, which predates this and is pinned by its consumers).
    let utf8_lossy =
        std::str::from_utf8(&o.stdout).is_err() || std::str::from_utf8(&o.stderr).is_err();
    let stdout_lossy = String::from_utf8_lossy(&o.stdout);
    let stderr_lossy = String::from_utf8_lossy(&o.stderr);
    let ok = o.natural_code == Some(0) && !o.timed_out && !o.interrupted && o.signal.is_none();
    let mut m = indexmap::IndexMap::new();
    m.insert("ok".to_string(), Value::Bool(ok));
    m.insert(
        "exit_code".to_string(),
        o.natural_code
            .map_or(Value::Nil, |c| Value::Number(c as f64)),
    );
    m.insert(
        "stdout".to_string(),
        Value::String(stdout_lossy.into_owned()),
    );
    m.insert(
        "stderr".to_string(),
        Value::String(stderr_lossy.into_owned()),
    );
    m.insert("timed_out".to_string(), Value::Bool(o.timed_out));
    m.insert("interrupted".to_string(), Value::Bool(o.interrupted));
    m.insert(
        "signal".to_string(),
        o.signal.map_or(Value::Nil, |s| Value::Number(s as f64)),
    );
    m.insert(
        "duration_ms".to_string(),
        Value::Number(o.duration_ms as f64),
    );
    m.insert(
        "stdout_truncated".to_string(),
        Value::Bool(o.stdout_truncated),
    );
    m.insert(
        "stderr_truncated".to_string(),
        Value::Bool(o.stderr_truncated),
    );
    m.insert("utf8_lossy".to_string(), Value::Bool(utf8_lossy));
    m.insert("error_code".to_string(), Value::Nil);
    m.insert("error".to_string(), Value::Nil);
    Value::map(m)
}

/// The failure-shaped result map for spawn/lifecycle errors (D4:
/// these do NOT raise from `run_argv`).
fn run_argv_error_map(code: &str, message: &str) -> Value {
    let mut m = indexmap::IndexMap::new();
    m.insert("ok".to_string(), Value::Bool(false));
    m.insert("exit_code".to_string(), Value::Nil);
    m.insert("stdout".to_string(), Value::String(String::new()));
    m.insert("stderr".to_string(), Value::String(String::new()));
    m.insert("timed_out".to_string(), Value::Bool(false));
    m.insert("interrupted".to_string(), Value::Bool(false));
    m.insert("signal".to_string(), Value::Nil);
    m.insert("duration_ms".to_string(), Value::Number(0.0));
    m.insert("stdout_truncated".to_string(), Value::Bool(false));
    m.insert("stderr_truncated".to_string(), Value::Bool(false));
    m.insert("utf8_lossy".to_string(), Value::Bool(false));
    m.insert("error_code".to_string(), Value::String(code.to_string()));
    m.insert("error".to_string(), Value::String(message.to_string()));
    Value::map(m)
}

/// run_argv(argv[, opts]) — direct-argv, captured, bounded process
/// execution (D4). No shell anywhere; one consistent result map;
/// ordinary command failure is a VALUE (`ok:false`), never a raise —
/// only argument/option validation raises.
fn builtin_run_argv_impl(caller: &str, args: &[Value]) -> MixResult<Value> {
    if args.is_empty() || args.len() > 2 {
        return Err(MixError::structured(
            "TYPE_MISMATCH",
            format!(
                "{caller}: expected 1 or 2 args (argv, [opts]), got {}",
                args.len()
            ),
        ));
    }
    let argv = parse_run_argv_argv(caller, &args[0])?;
    let opts = parse_run_argv_opts(caller, args.get(1))?;
    let stdin = match &opts.stdin {
        RunArgvStdin::Null => ProcStdin::Null,
        RunArgvStdin::Data(data) => ProcStdin::Data(data),
        RunArgvStdin::File(path) => ProcStdin::File(path),
    };
    let stdout = match &opts.stdout {
        RunArgvOutput::Capture => ProcOutput::Capture,
        RunArgvOutput::Inherit => ProcOutput::Inherit,
        RunArgvOutput::Null => ProcOutput::Null,
        RunArgvOutput::File(file) => ProcOutput::File(file),
    };
    let stderr = match &opts.stderr {
        RunArgvStderr::Capture => ProcStderr::Capture,
        RunArgvStderr::Inherit => ProcStderr::Inherit,
        RunArgvStderr::Null => ProcStderr::Null,
        RunArgvStderr::Stdout => ProcStderr::Stdout,
        RunArgvStderr::File(file) => ProcStderr::File(file),
    };
    let outcome = run_process(&ProcSpec {
        argv: &argv,
        stdin,
        stdout,
        stderr,
        timeout_ms: opts.timeout_ms,
        caller,
        cwd: opts.cwd.as_deref(),
        env: &opts.env,
        clear_env: opts.clear_env,
        max_output: opts.max_output,
        stream: opts.stream,
    });
    match outcome {
        Ok(o) => Ok(run_argv_result_map(&o)),
        // Spawn/lifecycle failures come back as PROCESS_* structured
        // errors from the engine — encode them in the result value.
        Err(MixError::Structured(info))
            if matches!(
                info.code.as_str(),
                "PROCESS_SPAWN" | "PROCESS_STDIO" | "PROCESS_IO" | "PROCESS_INTERNAL"
            ) =>
        {
            Ok(run_argv_error_map(&info.code, &info.message))
        }
        Err(other) => Err(other),
    }
}

fn builtin_run_argv(args: Vec<Value>) -> MixResult<Option<Value>> {
    builtin_run_argv_impl("run_argv", &args).map(Some)
}

/// run_argv_must(argv[, opts]) — fail-fast twin: returns stdout only
/// when `ok` and neither stream was truncated; otherwise raises the
/// corresponding PROCESS_* structured error carrying the complete
/// result map under `details.result`.
fn builtin_run_argv_must(args: Vec<Value>) -> MixResult<Option<Value>> {
    let result = builtin_run_argv_impl("run_argv_must", &args)?;
    let m = match &result {
        Value::Map(m) => m,
        _ => unreachable!("run_argv result is always a map"),
    };
    let get_bool = |k: &str| matches!(m.get(k), Some(Value::Bool(true)));
    let program = match &args[0] {
        Value::List(items) => items.first().map(|v| v.to_mix_string()).unwrap_or_default(),
        _ => String::new(),
    };
    let excerpt = |k: &str| -> String {
        match m.get(k) {
            Some(Value::String(s)) => {
                let s = sanitize_for_diag(s.trim_end());
                let mut e: String = s.chars().take(200).collect();
                if e.len() < s.len() {
                    e.push('…');
                }
                e
            }
            _ => String::new(),
        }
    };
    let (code, msg) = if let Some(Value::String(ec)) = m.get("error_code") {
        (
            ec.clone(),
            match m.get("error") {
                Some(Value::String(e)) => e.clone(),
                _ => format!("run_argv_must: {program} failed"),
            },
        )
    } else if get_bool("interrupted") {
        (
            "PROCESS_INTERRUPTED".to_string(),
            format!("run_argv_must: interrupted while running {program}"),
        )
    } else if get_bool("timed_out") {
        (
            "PROCESS_TIMEOUT".to_string(),
            format!("run_argv_must: {program} exceeded its deadline"),
        )
    } else if let Some(Value::Number(sig)) = m.get("signal") {
        (
            "PROCESS_SIGNAL".to_string(),
            format!("run_argv_must: {program} killed by signal {sig}"),
        )
    } else if get_bool("stdout_truncated") || get_bool("stderr_truncated") {
        (
            "PROCESS_OUTPUT_LIMIT".to_string(),
            format!("run_argv_must: {program} output exceeded max_output"),
        )
    } else if !get_bool("ok") {
        let ec = match m.get("exit_code") {
            Some(Value::Number(n)) => *n as i64,
            _ => -3,
        };
        (
            "PROCESS_EXIT_NONZERO".to_string(),
            format!(
                "run_argv_must: {program} failed (exit_code={ec}): {}",
                excerpt("stderr")
            ),
        )
    } else {
        // ok and untruncated — return stdout unchanged.
        return Ok(Some(match m.get("stdout") {
            Some(v) => v.clone(),
            None => Value::String(String::new()),
        }));
    };
    let mut details = indexmap::IndexMap::new();
    details.insert("result".to_string(), result.clone());
    Err(MixError::Structured(Box::new(
        crate::error::ErrorInfo::new(code, msg).with_details(Value::map(details)),
    )))
}

struct PipelineOpts {
    timeout_ms: u64,
    max_output: Option<usize>,
    allow_signal: bool,
}

struct PipelineStage {
    argv: Vec<String>,
    opts: RunArgvOpts,
}

const RUN_PIPELINE_OPT_KEYS: &[&str] = &["timeout", "max_output", "allow_signal"];

fn parse_run_pipeline_opts(caller: &str, value: Option<&Value>) -> MixResult<PipelineOpts> {
    let map = match value {
        None | Some(Value::Nil) => None,
        Some(Value::Map(map)) => Some(map),
        Some(other) => {
            return Err(opt_invalid(
                caller,
                format!("options must be a map, got {}", other.type_name()),
            ));
        }
    };

    let mut shared = indexmap::IndexMap::new();
    // Defaults FALSE (Mark's call, 2026-08-18). Accepting SIGPIPE on a
    // non-final stage cannot distinguish benign backpressure — the reader
    // closed early, as in `yes | head -1` — from a stage that died of SIGPIPE
    // for a fatal reason of its own. A cold review demonstrated the gap:
    //
    //   run_pipeline([["yes"], ["sh","-c","printf fatal >&2; kill -PIPE $$"], ["true"]])
    //
    // reported ok:true and run_pipeline_must returned SUCCESS, despite the
    // middle stage deliberately killing itself after writing to stderr.
    // Reporting success for that is precisely the silent-wrong-answer class
    // this whole surface exists to remove, so honesty wins over ergonomics:
    // `ok` is false unless the caller opts in with allow_signal:true. This
    // matches `set -o pipefail`, which likewise reports 141 for `yes | head -1`.
    let mut allow_signal = false;
    if let Some(map) = map {
        for (key, value) in map.iter() {
            match key.as_str() {
                "timeout" | "max_output" => {
                    shared.insert(key.clone(), value.clone());
                }
                "allow_signal" => match value {
                    Value::Bool(value) => allow_signal = *value,
                    other => {
                        return Err(opt_invalid(
                            caller,
                            format!("allow_signal must be a bool, got {}", other.type_name()),
                        ));
                    }
                },
                other => {
                    return Err(opt_invalid(
                        caller,
                        format!(
                            "unknown option '{}' (supported: {})",
                            sanitize_for_diag(other),
                            RUN_PIPELINE_OPT_KEYS.join(", ")
                        ),
                    ));
                }
            }
        }
    }
    let shared_value = Value::map(shared);
    let shared_opts = parse_run_argv_opts(caller, Some(&shared_value))?;
    Ok(PipelineOpts {
        timeout_ms: shared_opts.timeout_ms,
        max_output: shared_opts.max_output,
        allow_signal,
    })
}

fn parse_run_pipeline_stages(caller: &str, value: &Value) -> MixResult<Vec<PipelineStage>> {
    let values = match value {
        Value::List(values) => values,
        other => {
            return Err(MixError::structured(
                "TYPE_MISMATCH",
                format!("{caller}: stages must be a list, got {}", other.type_name()),
            ));
        }
    };
    if values.is_empty() {
        return Err(MixError::structured(
            "TYPE_MISMATCH",
            format!("{caller}: stages must contain at least one stage"),
        ));
    }

    let last = values.len() - 1;
    let mut stages = Vec::with_capacity(values.len());
    for (index, value) in values.iter().enumerate() {
        match value {
            Value::List(_) => stages.push(PipelineStage {
                argv: parse_run_argv_argv(caller, value)?,
                opts: parse_run_argv_opts(caller, None)?,
            }),
            Value::Map(map) => {
                let argv = match map.get("argv") {
                    Some(argv) => parse_run_argv_argv(caller, argv)?,
                    None => {
                        return Err(MixError::structured(
                            "TYPE_MISMATCH",
                            format!("{caller}: stage[{index}] map requires `argv`"),
                        ));
                    }
                };
                let mut stage_opts = indexmap::IndexMap::new();
                for (key, value) in map.iter() {
                    match key.as_str() {
                        "argv" => {}
                        "cwd" | "env" | "clear_env" | "stderr" => {
                            stage_opts.insert(key.clone(), value.clone());
                        }
                        "stdin" if index == 0 => {
                            stage_opts.insert(key.clone(), value.clone());
                        }
                        "stdout" if index == last => {
                            stage_opts.insert(key.clone(), value.clone());
                        }
                        "stdin" => {
                            return Err(opt_invalid(
                                caller,
                                format!("stdin is only valid on stage[0], not stage[{index}]"),
                            ));
                        }
                        "stdout" => {
                            return Err(opt_invalid(
                                caller,
                                format!(
                                    "stdout is only valid on the last stage, not stage[{index}]"
                                ),
                            ));
                        }
                        other => {
                            return Err(opt_invalid(
                                caller,
                                format!(
                                    "unknown stage[{index}] option '{}' (supported: argv, cwd, env, clear_env, stderr{}{})",
                                    sanitize_for_diag(other),
                                    if index == 0 { ", stdin" } else { "" },
                                    if index == last { ", stdout" } else { "" }
                                ),
                            ));
                        }
                    }
                }
                let stage_opts_value = Value::map(stage_opts);
                stages.push(PipelineStage {
                    argv,
                    opts: parse_run_argv_opts(caller, Some(&stage_opts_value))?,
                });
            }
            other => {
                return Err(MixError::structured(
                    "TYPE_MISMATCH",
                    format!(
                        "{caller}: stage[{index}] must be an argv list or stage map, got {}",
                        other.type_name()
                    ),
                ));
            }
        }
    }
    Ok(stages)
}

fn pipeline_error_map(code: &str, message: &str, stages: Option<Value>) -> Value {
    let partial_stderr_truncated = match &stages {
        Some(Value::List(stages)) => stages.iter().any(|stage| match stage {
            Value::Map(stage) => stage.get("stderr_truncated") == Some(&Value::Bool(true)),
            _ => false,
        }),
        _ => false,
    };
    let partial_duration_ms = match &stages {
        Some(Value::List(stages)) => stages
            .iter()
            .filter_map(|stage| match stage {
                Value::Map(stage) => match stage.get("duration_ms") {
                    Some(Value::Number(duration)) => Some(*duration),
                    _ => None,
                },
                _ => None,
            })
            .fold(0.0, f64::max),
        _ => 0.0,
    };
    let mut map = indexmap::IndexMap::new();
    map.insert("ok".to_string(), Value::Bool(false));
    map.insert("exit_code".to_string(), Value::Nil);
    map.insert("stdout".to_string(), Value::String(String::new()));
    map.insert("stderr".to_string(), Value::String(String::new()));
    map.insert("timed_out".to_string(), Value::Bool(false));
    map.insert("interrupted".to_string(), Value::Bool(false));
    map.insert("signal".to_string(), Value::Nil);
    map.insert(
        "duration_ms".to_string(),
        Value::Number(partial_duration_ms),
    );
    map.insert("stdout_truncated".to_string(), Value::Bool(false));
    map.insert(
        "stderr_truncated".to_string(),
        Value::Bool(partial_stderr_truncated),
    );
    map.insert("utf8_lossy".to_string(), Value::Bool(false));
    map.insert("error_code".to_string(), Value::String(code.to_string()));
    map.insert("error".to_string(), Value::String(message.to_string()));
    map.insert(
        "stages".to_string(),
        stages.unwrap_or_else(|| Value::list(Vec::new())),
    );
    Value::map(map)
}

fn builtin_run_pipeline_impl(caller: &str, args: &[Value]) -> MixResult<Value> {
    if args.is_empty() || args.len() > 2 {
        return Err(MixError::structured(
            "TYPE_MISMATCH",
            format!(
                "{caller}: expected 1 or 2 args (stages, [opts]), got {}",
                args.len()
            ),
        ));
    }
    let stages = parse_run_pipeline_stages(caller, &args[0])?;
    let opts = parse_run_pipeline_opts(caller, args.get(1))?;
    match run_pipeline_processes(caller, &stages, &opts) {
        Ok(outcome) => Ok(pipeline_result_map(&stages, outcome, opts.allow_signal)),
        Err(MixError::Structured(info))
            if matches!(
                info.code.as_str(),
                "PIPELINE_SPAWN" | "PIPELINE_STDIO" | "PIPELINE_IO" | "PIPELINE_INTERNAL"
            ) =>
        {
            let partial_stages =
                matches!(info.details, Value::List(_)).then(|| info.details.clone());
            Ok(pipeline_error_map(
                &info.code,
                &info.message,
                partial_stages,
            ))
        }
        Err(other) => Err(other),
    }
}

fn builtin_run_pipeline(args: Vec<Value>) -> MixResult<Option<Value>> {
    builtin_run_pipeline_impl("run_pipeline", &args).map(Some)
}

fn builtin_run_pipeline_must(args: Vec<Value>) -> MixResult<Option<Value>> {
    let result = builtin_run_pipeline_impl("run_pipeline_must", &args)?;
    let map = match &result {
        Value::Map(map) => map,
        _ => unreachable!("run_pipeline result is always a map"),
    };
    let get_bool = |key: &str| matches!(map.get(key), Some(Value::Bool(true)));
    let (code, message) = if let Some(Value::String(error_code)) = map.get("error_code") {
        (
            error_code.clone(),
            match map.get("error") {
                Some(Value::String(error)) => error.clone(),
                _ => "run_pipeline_must: pipeline setup failed".to_string(),
            },
        )
    } else if get_bool("interrupted") {
        (
            "PIPELINE_INTERRUPTED".to_string(),
            "run_pipeline_must: pipeline was interrupted".to_string(),
        )
    } else if get_bool("timed_out") {
        (
            "PIPELINE_TIMEOUT".to_string(),
            "run_pipeline_must: pipeline exceeded its deadline".to_string(),
        )
    } else if get_bool("stdout_truncated") || get_bool("stderr_truncated") {
        (
            "PIPELINE_OUTPUT_LIMIT".to_string(),
            "run_pipeline_must: captured output exceeded max_output".to_string(),
        )
    } else if !get_bool("ok") {
        let failed_signal = match map.get("stages") {
            Some(Value::List(stages)) => stages.iter().enumerate().find_map(|(index, stage)| {
                let stage = match stage {
                    Value::Map(stage) => stage,
                    _ => return None,
                };
                if matches!(stage.get("accepted_signal"), Some(Value::Bool(true))) {
                    return None;
                }
                match stage.get("signal") {
                    Some(Value::Number(signal)) => Some((index, *signal as i64)),
                    _ => None,
                }
            }),
            _ => None,
        };
        if let Some((index, signal)) = failed_signal {
            (
                "PIPELINE_SIGNAL".to_string(),
                format!("run_pipeline_must: stage[{index}] killed by signal {signal}"),
            )
        } else {
            let failed_exit = match map.get("stages") {
                Some(Value::List(stages)) => {
                    stages.iter().enumerate().find_map(|(index, stage)| {
                        let stage = match stage {
                            Value::Map(stage) => stage,
                            _ => return None,
                        };
                        match (stage.get("ok"), stage.get("exit_code")) {
                            (Some(Value::Bool(false)), Some(Value::Number(code))) => {
                                Some((index, *code as i64))
                            }
                            _ => None,
                        }
                    })
                }
                _ => None,
            };
            let message = failed_exit.map_or_else(
                || "run_pipeline_must: pipeline failed".to_string(),
                |(index, code)| {
                    format!("run_pipeline_must: stage[{index}] failed (exit_code={code})")
                },
            );
            ("PIPELINE_EXIT_NONZERO".to_string(), message)
        }
    } else {
        return Ok(Some(
            map.get("stdout")
                .cloned()
                .unwrap_or_else(|| Value::String(String::new())),
        ));
    };

    let mut details = indexmap::IndexMap::new();
    details.insert("result".to_string(), result.clone());
    Err(MixError::Structured(Box::new(
        crate::error::ErrorInfo::new(code, message).with_details(Value::map(details)),
    )))
}

/// Options `run_stream` accepts — the subset of [`RUN_ARGV_OPT_KEYS`] that
/// means anything for a runner which inherits stdio and blocks until the child
/// exits.
const RUN_STREAM_OPT_KEYS: &[&str] = &["env", "clear_env", "cwd"];

/// Validate a `run_stream` option map, then delegate the actual parsing to
/// [`parse_run_argv_opts`] so `env` / `clear_env` / `cwd` keep byte-identical
/// semantics across the two argv runners — name validation, NUL rejection, the
/// number/bool coercion of env values, and the `OPTION_INVALID` code. The
/// remaining fields of the returned `RunArgvOpts` (timeout, stdio routing,
/// max_output, stream) are inapplicable here and never read; the key check below is what
/// guarantees no caller SET one and believed it took effect.
///
/// The run_argv-only keys are rejected BY NAME rather than swept into the
/// generic unknown-option arm: reaching for `timeout` on the runner that
/// deliberately has no deadline is the predictable mistake, and a message that
/// only lists what IS supported leaves the reader to infer why theirs isn't.
fn parse_run_stream_opts(caller: &str, v: Option<&Value>) -> MixResult<RunArgvOpts> {
    if let Some(Value::Map(m)) = v {
        for (k, _) in m.iter() {
            if RUN_STREAM_OPT_KEYS.contains(&k.as_str()) {
                continue;
            }
            let why = match k.as_str() {
                "timeout" => {
                    Some("run_stream blocks until the child exits — use run_argv for a deadline")
                }
                "stdin" => Some(
                    "run_stream inherits the parent's stdin — use run_argv's stdin: to pre-supply input",
                ),
                "max_output" | "stream" | "stdout" | "stderr" => Some(
                    "run_stream does not capture output, it streams straight to the parent's stdout/stderr — use run_argv to capture",
                ),
                _ => None,
            };
            return Err(opt_invalid(
                caller,
                match why {
                    Some(why) => format!(
                        "option '{}' is not supported ({why}); supported: {}",
                        sanitize_for_diag(k),
                        RUN_STREAM_OPT_KEYS.join(", ")
                    ),
                    None => format!(
                        "unknown option '{}' (supported: {})",
                        sanitize_for_diag(k),
                        RUN_STREAM_OPT_KEYS.join(", ")
                    ),
                },
            ));
        }
    }
    parse_run_argv_opts(caller, v)
}

/// run_stream(argv[, opts]) — run an argv LIST directly (no `/bin/sh`),
/// inheriting the parent's stdin/stdout/stderr so output streams live AND the
/// child can read the controlling terminal. Returns the exit code as a Number.
///
/// `opts` is `{env, clear_env, cwd}` (v0.51.0). `export KEY = value` reaches a
/// child too, but it mutates the process-global environment — it stays set for
/// the rest of the run, and every later child inherits it unless that child
/// clears or overrides the key; the option is scoped to the one call. The older per-call route was prefixing the argv with coreutils
/// `env`, which puts every value in the child's `ps` argv — so this keeps a
/// secret out of argv as much as it is a convenience.
///
/// Unlike `run`/`run_rc` (which capture via `.output()` and only surface
/// bytes once the child exits), this hands the terminal straight to the
/// child: progress appears as it happens, and an interactive prompt (apt
/// confirmation, a password) can be answered — provided the command itself
/// allocates a pty, e.g. `run_stream(["ssh", "-t", host, cmd])`.
///
/// Because the argv is a list, there is NO shell parsing: no word-splitting,
/// glob, quoting, or operator interpretation. Each element is one argument
/// verbatim — so values are injection-inert by construction (contrast `run`,
/// which goes through `sh -c`).
///
/// Like the capturing run builtins (`run`, `run_rc`), this BLOCKS the
/// evaluator thread in `.status()` until the child exits — it is meant for
/// FOREGROUND, one-shot use (the `mx` remote-shell wrapper), where blocking is
/// the point: the child owns the terminal while it runs. Don't put it in a hot
/// `on … async` event handler for a long-running/interactive child — there it
/// stalls the dispatch the same way `run`/`run_rc` would, and there is no
/// terminal for interactivity anyway.
fn builtin_run_stream(args: Vec<Value>) -> MixResult<Option<Value>> {
    expect_args("run_stream", &args, 1)?;
    let items = match &args[0] {
        Value::List(items) => items,
        other => {
            return Err(MixError::RuntimeError {
                span: None,
                msg: format!(
                    "run_stream: argument must be a list of strings, got {}",
                    other.type_name()
                ),
            });
        }
    };
    if items.is_empty() {
        return Err(MixError::RuntimeError {
            span: None,
            msg: "run_stream: argv list is empty (need at least the program)".into(),
        });
    }
    // Every element MUST be a string — no implicit stringification of bools /
    // numbers / nested values, which would silently spawn unintended args.
    let mut argv: Vec<String> = Vec::with_capacity(items.len());
    for (i, item) in items.iter().enumerate() {
        match item {
            Value::String(s) => argv.push(s.clone()),
            other => {
                return Err(MixError::RuntimeError {
                    span: None,
                    msg: format!(
                        "run_stream: argv[{}] must be a string, got {}",
                        i,
                        other.type_name()
                    ),
                });
            }
        }
    }
    let opts = parse_run_stream_opts("run_stream", args.get(1))?;
    // Default Command stdio is "inherit" — stdin/stdout/stderr are the
    // parent's, so the child streams live and owns the terminal. .status()
    // blocks until the child exits (correct for an interactive one-shot).
    let mut cmd = std::process::Command::new(&argv[0]);
    cmd.args(&argv[1..]);
    // Same order as the captured-runner engine (`run_process`): clear first,
    // then layer the explicit pairs, so {clear_env: true, env: {…}} means
    // "exactly these" rather than "these, minus whatever the clear dropped".
    if opts.clear_env {
        cmd.env_clear();
    }
    for (k, v) in opts.env.iter() {
        cmd.env(k, v);
    }
    if let Some(dir) = opts.cwd.as_deref() {
        cmd.current_dir(dir);
    }
    let status = cmd.status().map_err(|e| MixError::RuntimeError {
        span: None,
        msg: format!("run_stream: failed to spawn {}: {}", argv[0], e),
    })?;
    let code = match status.code() {
        Some(c) => c,
        None => {
            // No exit code → terminated by a signal; mirror shell 128+signo.
            #[cfg(unix)]
            {
                use std::os::unix::process::ExitStatusExt;
                status.signal().map(|s| 128 + s).unwrap_or(-1)
            }
            #[cfg(not(unix))]
            {
                -1
            }
        }
    };
    Ok(Some(Value::Number(code as f64)))
}

/// Single-quote-wrap `s` for POSIX shells so the wrapped value is
/// inert under shell parsing — equivalent to PHP `escapeshellarg`.
/// Internal `'` becomes `'\''` (close, escape, reopen).
///
/// Public so other crate code (e.g. `ssh_run`'s remote-command
/// builder) can compose with the same quoting logic without going
/// through the Mix dispatch layer.
pub fn shell_quote(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 2);
    out.push('\'');
    for ch in s.chars() {
        if ch == '\'' {
            out.push_str("'\\''");
        } else {
            out.push(ch);
        }
    }
    out.push('\'');
    out
}

fn builtin_shell_quote(args: Vec<Value>) -> MixResult<Option<Value>> {
    expect_args("shell_quote", &args, 1)?;
    let s = args[0].to_mix_string();
    Ok(Some(Value::String(shell_quote(&s))))
}

/// Escape `s` for interpolation inside a SQL string literal: doubles
/// every single quote AND escapes backslash (`\` → `\\`), and strips
/// NUL bytes (a NUL passed through can truncate the value in C-based
/// clients). Backslash escaping makes the result safe under
/// MySQL/MariaDB's DEFAULT sql_mode, where `\` is an escape character
/// — the documented target (e.g. `mariadb -e "SELECT * FROM t WHERE
/// name = '<sql_quote(name)>'"`); quote-doubling alone was injectable
/// there. It stays SAFE for SQLite/Postgres standard mode too, where
/// `\` is literal — the trade-off is that a literal backslash arrives
/// doubled; callers needing exact-byte SQLite literals should use
/// sqlexec() binds. Does NOT add outer quotes — the caller composes
/// them. Not a substitute for parameterised queries when a real SQL
/// connection is available.
pub fn sql_quote(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for ch in s.chars() {
        match ch {
            '\'' => out.push_str("''"),
            '\\' => out.push_str("\\\\"),
            '\0' => {} // NUL truncates in C clients — drop it
            c => out.push(c),
        }
    }
    out
}

fn builtin_sql_quote(args: Vec<Value>) -> MixResult<Option<Value>> {
    expect_args("sql_quote", &args, 1)?;
    let s = args[0].to_mix_string();
    Ok(Some(Value::String(sql_quote(&s))))
}

/// Alphabets for `random_password`. `O` and `o` are excluded up front
/// so the class-diversity guarantee holds for every `len ≥ 3` without
/// a post-substitution pass — see §7 of the ssh_run spec for the
/// rationale (the shell `newpw` idiom's `tr 'Oo' '00'` step has a
/// theoretical bug at `len=3` that this avoids).
const PW_UPPER: &[u8] = b"ABCDEFGHIJKLMNPQRSTUVWXYZ"; // 25, no O
const PW_LOWER: &[u8] = b"abcdefghijklmnpqrstuvwxyz"; // 25, no o
const PW_DIGIT: &[u8] = b"0123456789"; // 10
// FILL = UPPER ++ LOWER ++ DIGIT, 60 chars, contains no O/o.
const PW_FILL: &[u8] = b"ABCDEFGHIJKLMNPQRSTUVWXYZabcdefghijklmnpqrstuvwxyz0123456789";

/// Generate an alphanumeric password with guaranteed character-class
/// diversity (1 upper + 1 lower + 1 digit + fill) and no `O`/`o`.
/// Sources every random integer from `OsRng`. See §7 of the ssh_run
/// spec for the design contract.
pub fn random_password(len: usize) -> String {
    use rand::seq::SliceRandom;
    use rand::{Rng, TryRngCore, rngs::OsRng};

    // The pure helper is `pub` and callable from any consumer of the
    // crate, not just the Mix dispatch layer. Enforce the 3..=1024
    // contract in release too — `debug_assert!` is a no-op in release
    // and would silently produce a 3-character password for `len < 3`
    // (the prefix pushes always run, the fill loop is empty).
    assert!(
        (3..=1024).contains(&len),
        "random_password: len {} out of range (must be 3..=1024)",
        len
    );

    // rand 0.9: `OsRng` only implements `TryRngCore` (fallible). Wrap
    // it in `UnwrapErr` to get an infallible `RngCore`/`Rng` — OS
    // entropy failures are catastrophic and panicking is correct.
    let mut rng = OsRng.unwrap_err();
    let mut chars: Vec<u8> = Vec::with_capacity(len);
    chars.push(PW_UPPER[rng.random_range(0..PW_UPPER.len())]);
    chars.push(PW_LOWER[rng.random_range(0..PW_LOWER.len())]);
    chars.push(PW_DIGIT[rng.random_range(0..PW_DIGIT.len())]);
    for _ in 3..len {
        chars.push(PW_FILL[rng.random_range(0..PW_FILL.len())]);
    }
    chars.shuffle(&mut rng);
    // Output is ASCII by construction (all alphabets are ASCII), so
    // `String::from_utf8` cannot fail.
    String::from_utf8(chars).expect("random_password produced non-UTF8 bytes")
}

fn builtin_random_password(args: Vec<Value>) -> MixResult<Option<Value>> {
    if args.len() > 1 {
        return Err(MixError::RuntimeError {
            span: None,
            msg: format!(
                "random_password: expected 0 or 1 args (len), got {}",
                args.len()
            ),
        });
    }
    let len = match args.first() {
        None => 16usize,
        Some(v) => {
            let n = extract_number(v, InputPolicy::StandardCoercion).ok_or_else(|| {
                MixError::RuntimeError {
                    span: None,
                    msg: "random_password: len must be a number".into(),
                }
            })?;
            as_exact_integer("random_password(): argument 1", n, 3, 1024)? as usize
        }
    };
    Ok(Some(Value::String(random_password(len))))
}

// ---------------------------------------------------------------------------
// ssh_run — Mix builtin that wraps the system `ssh` binary.
//
// The implementation is split across this section:
//   - SshOpts / SshOutcome — parsed input and raw output
//   - is_valid_env_key, reject_nul — small validators
//   - parse_ssh_opts — opts-map → SshOpts (rejects unknown keys)
//   - build_remote_command — string|list + opts → remote shell snippet
//   - build_ssh_argv — host + remote snippet + opts → argv for `ssh`
//   - ssh_result_map — SshOutcome → Mix Map
//   - run_with_timeout — spawn the local ssh, drain pipes, collect outcome
//     (Task 4 ships the no-timeout/no-interrupt baseline; Task 5 of the
//      ssh_run plan layers in deadline + INTERRUPT_FLAG cooperation)
//   - builtin_ssh_run — Mix dispatch entry point
// See `src/_doc/2026-05-08-mix-ssh-builtin-spec.md` for the contract.

#[derive(Debug, Clone)]
struct SshOpts {
    timeout: u64,
    connect_timeout: u64,
    multiplex: bool,
    batch: bool,
    strict_host_key: String,
    env: Vec<(String, String)>,
    /// How env values travel: "mix" (stdin driver, default) | "sh" | "argv".
    env_transport: String,
    cwd: Option<String>,
    stdin: Option<String>,
    extra_ssh_args: Vec<String>,
}

impl Default for SshOpts {
    fn default() -> Self {
        Self {
            timeout: 30,
            connect_timeout: 10,
            multiplex: false,
            batch: true,
            strict_host_key: "accept-new".into(),
            env: Vec::new(),
            env_transport: "mix".into(),
            cwd: None,
            stdin: None,
            extra_ssh_args: Vec::new(),
        }
    }
}

#[derive(Debug)]
struct SshOutcome {
    stdout: Vec<u8>,
    stderr: Vec<u8>,
    exit_code: i32,
    timed_out: bool,
    interrupted: bool,
}

/// Reject NUL bytes in any string that will become argv or part of the
/// remote-command payload. `Command::arg` panics via `CString::new` on
/// NULs and the remote shell can't disambiguate them either, so they
/// must turn into a Mix error before we spawn.
fn reject_nul(field: &str, s: &str) -> MixResult<()> {
    if s.contains('\0') {
        return Err(MixError::RuntimeError {
            span: None,
            msg: format!("ssh_run: NUL byte in {}", field),
        });
    }
    Ok(())
}

/// `[A-Za-z_][A-Za-z0-9_]*` — POSIX-portable env-var name.
fn is_valid_env_key(k: &str) -> bool {
    let mut c = k.chars();
    match c.next() {
        Some(ch) if ch == '_' || ch.is_ascii_alphabetic() => {}
        _ => return false,
    }
    c.all(|ch| ch == '_' || ch.is_ascii_alphanumeric())
}

const SSH_OPT_KEYS: &[&str] = &[
    "timeout",
    "connect_timeout",
    "multiplex",
    "batch",
    "strict_host_key",
    "env",
    "env_transport",
    "cwd",
    "stdin",
    "extra_ssh_args",
];

const STRICT_HOST_KEY_VALUES: &[&str] = &["yes", "no", "accept-new", "ask"];

/// How `env:` values travel to the remote (decision: 2026-07-02 audit,
/// Codex-ruled). `"mix"` (DEFAULT) and `"sh"` ship the env inside a driver
/// script over ssh **stdin**, so a secret value never appears in local or
/// remote `ps` argv; `"argv"` is the legacy `export K='v'; ` command-string
/// prefix — visible in `ps` on BOTH ends, kept for compatibility only.
/// `"mix"` needs `/opt/cosmix/bin/mix` on the remote (every managed node);
/// `"sh"` is for arbitrary POSIX hosts (and is broken on mix-login-shell
/// nodes, where `sh -s` misroutes through the Mix classifier).
const ENV_TRANSPORT_VALUES: &[&str] = &["mix", "sh", "argv"];

/// Parse an `env:` opt map into (key, value) pairs. Values coerce like the
/// rest of Mix (string/whole-number/bool); NULs are rejected. Shared by
/// `parse_ssh_opts` and `ssh_mix`'s source-prefix env path.
fn parse_env_opt(v: &Value) -> MixResult<Vec<(String, String)>> {
    let env_map = match v {
        Value::Map(m) => m,
        _ => {
            return Err(MixError::RuntimeError {
                span: None,
                msg: "ssh_run: env must be a map of string→string".into(),
            });
        }
    };
    let mut out = Vec::with_capacity(env_map.len());
    for (k, vv) in env_map.iter() {
        let val = match vv {
            Value::String(s) => s.clone(),
            Value::Number(n) => {
                // Integer rendering only inside i64: `as i64` SATURATES, so
                // the old floor()-only test exported env {N: 1e30} as
                // "9223372036854775807" into the child's environ -- a
                // fabricated value crossing execve. NOT a round-trip check:
                // at exactly 2^63 the saturating cast and `i64::MAX as f64`'s
                // round-UP cancel, so `n == (n as i64) as f64` PASSES on the
                // one value it must refuse. The exclusive upper bound is the
                // json.rs predicate, whose comment names this exact trap.
                // Display renders whole f64s without a fraction anyway, so
                // the fallback prints the value the caller actually wrote.
                if *n == n.floor() && *n >= i64::MIN as f64 && *n < i64::MAX as f64 {
                    format!("{}", *n as i64)
                } else {
                    format!("{}", n)
                }
            }
            Value::Bool(b) => (if *b { "true" } else { "false" }).into(),
            _ => {
                return Err(MixError::RuntimeError {
                    span: None,
                    msg: format!("ssh_run: env value for {:?} must be string/number/bool", k),
                });
            }
        };
        reject_nul(&format!("env value for {:?}", k), &val)?;
        out.push((k.clone(), val));
    }
    Ok(out)
}

/// Validate every env KEY against the POSIX name rule, with the caller's
/// name in the diagnostic. (Values are free-form; keys become `export`
/// targets in a driver or command prefix.)
fn validate_env_keys(caller: &str, env: &[(String, String)]) -> MixResult<()> {
    for (k, _) in env {
        if !is_valid_env_key(k) {
            return Err(MixError::RuntimeError {
                span: None,
                msg: format!(
                    "{}: invalid env key {:?} (must match [A-Za-z_][A-Za-z0-9_]*)",
                    caller, k
                ),
            });
        }
    }
    Ok(())
}

/// Mix source for one `export KEY = "value"` line, value escaped through
/// the strict-data serializer so arbitrary content (quotes, `$`, newlines)
/// arrives as inert data. Shared by the "mix" env driver and `ssh_mix`'s
/// env prefix.
fn mix_export_lines(env: &[(String, String)]) -> MixResult<String> {
    let mut out = String::new();
    for (k, v) in env {
        let lit =
            Value::String(v.clone())
                .to_mix_data_string()
                .map_err(|e| MixError::RuntimeError {
                    span: None,
                    msg: format!("env value for {:?}: {}", k, e),
                })?;
        out.push_str(&format!("export {} = {}\n", k, lit));
    }
    Ok(out)
}

/// Mix source for typed `ssh_mix` bindings. Each value travels through
/// the same strict-data serializer as the `ssh_exec` argv/options driver,
/// then lands as a plain assignment before the caller's fixed source.
/// The generated literal is inert data: quotes, newlines, `$`, and
/// source-looking text cannot escape into another statement.
fn mix_binding_lines(bindings: &Value) -> MixResult<String> {
    let bindings = match bindings {
        Value::Map(bindings) => bindings,
        other => {
            return Err(opt_invalid(
                "ssh_mix",
                format!("bindings must be a map, got {}", other.type_name()),
            ));
        }
    };

    // Validate every name before encoding any value. There are no
    // reserved binding names: `$args`, `$argv`, and every other name
    // matching the ordinary identifier grammar are valid.
    for name in bindings.keys() {
        if !is_valid_env_key(name) {
            return Err(opt_invalid(
                "ssh_mix",
                format!(
                    "invalid bindings key {:?} (must match [A-Za-z_][A-Za-z0-9_]*)",
                    name
                ),
            ));
        }
    }

    let mut out = String::new();
    for (name, value) in bindings.iter() {
        let literal = value.to_mix_data_string().map_err(|error| match error {
            MixError::DataSerializeError { msg }
                if msg.starts_with("bytes value") || msg.starts_with("buffer value") =>
            {
                opt_invalid(
                    "ssh_mix",
                    format!(
                        "bindings value for {:?} must be strict-data encodable for remote execution \
                         (binary values cannot cross the strict-data driver) — encode it yourself, \
                         e.g. base64, and decode remotely",
                        name
                    ),
                )
            }
            other => opt_invalid(
                "ssh_mix",
                format!(
                    "bindings value for {:?} must be strict-data encodable: {}",
                    name, other
                ),
            ),
        })?;
        out.push_str(&format!("${name} = {literal}\n"));
    }
    Ok(out)
}

/// Build the stdin DRIVER for a secure-env `ssh_run` call.
///
/// `"mix"`: a Mix program for `/opt/cosmix/bin/mix -` — `export` lines put
/// the env in the remote mix process (inherited by children), then
/// `exit(run_stream(["sh", "-c", <cmd>]))` runs the actual command with
/// inherited stdio (stdout/stderr stream back through the ssh channel) and
/// propagates its exit code. Remote `ps` shows `sh -c <cmd>` — the command,
/// never the env values; local `ps` shows only `ssh … /opt/cosmix/bin/mix -`.
///
/// `"sh"`: a POSIX script for `sh -s` — `export K='v'` lines then the
/// command; the script travels on stdin so neither end's `ps` sees it.
fn build_env_driver(
    transport: &str,
    env: &[(String, String)],
    base_cmd: &str,
) -> MixResult<String> {
    match transport {
        "mix" => {
            let cmd_lit = Value::String(base_cmd.to_string())
                .to_mix_data_string()
                .map_err(|e| MixError::RuntimeError {
                    span: None,
                    msg: format!("ssh_run: command not driver-encodable: {}", e),
                })?;
            Ok(format!(
                "{}exit(run_stream([\"sh\", \"-c\", {}]))\n",
                mix_export_lines(env)?,
                cmd_lit
            ))
        }
        "sh" => {
            let mut out = String::new();
            for (k, v) in env {
                out.push_str(&format!("export {}={}\n", k, shell_quote(v)));
            }
            out.push_str(base_cmd);
            out.push('\n');
            Ok(out)
        }
        other => Err(MixError::RuntimeError {
            span: None,
            msg: format!("ssh_run: env_transport {:?} has no driver", other),
        }),
    }
}

fn parse_ssh_opts(v: Option<&Value>) -> MixResult<SshOpts> {
    let mut opts = SshOpts::default();
    let map = match v {
        None => return Ok(opts),
        Some(Value::Map(m)) => m,
        Some(_) => {
            return Err(MixError::RuntimeError {
                span: None,
                msg: "ssh_run: opts must be a map".into(),
            });
        }
    };
    for k in map.keys() {
        if !SSH_OPT_KEYS.contains(&k.as_str()) {
            return Err(MixError::RuntimeError {
                span: None,
                msg: format!(
                    "ssh_run: unknown opts key {:?} (allowed: {})",
                    k,
                    SSH_OPT_KEYS.join(", ")
                ),
            });
        }
    }
    if let Some(v) = map.get("timeout") {
        opts.timeout = parse_nonneg_int_opt("ssh_run", "timeout", v)?;
    }
    if let Some(v) = map.get("connect_timeout") {
        opts.connect_timeout = parse_nonneg_int_opt("ssh_run", "connect_timeout", v)?;
    }
    if let Some(v) = map.get("multiplex") {
        opts.multiplex = parse_bool_opt("multiplex", v)?;
    }
    if let Some(v) = map.get("batch") {
        opts.batch = parse_bool_opt("batch", v)?;
    }
    if let Some(v) = map.get("strict_host_key") {
        let s = match v {
            Value::String(s) => s.clone(),
            _ => {
                return Err(MixError::RuntimeError {
                    span: None,
                    msg: "ssh_run: strict_host_key must be a string".into(),
                });
            }
        };
        if !STRICT_HOST_KEY_VALUES.contains(&s.as_str()) {
            return Err(MixError::RuntimeError {
                span: None,
                msg: format!(
                    "ssh_run: strict_host_key {:?} not in {:?}",
                    s, STRICT_HOST_KEY_VALUES
                ),
            });
        }
        reject_nul("strict_host_key", &s)?;
        opts.strict_host_key = s;
    }
    if let Some(v) = map.get("env") {
        opts.env = parse_env_opt(v)?;
    }
    if let Some(v) = map.get("env_transport") {
        let s = match v {
            Value::String(s) => s.clone(),
            _ => {
                return Err(MixError::RuntimeError {
                    span: None,
                    msg: "ssh_run: env_transport must be \"mix\", \"sh\" or \"argv\"".into(),
                });
            }
        };
        if !ENV_TRANSPORT_VALUES.contains(&s.as_str()) {
            return Err(MixError::RuntimeError {
                span: None,
                msg: format!(
                    "ssh_run: env_transport {:?} not in {:?}",
                    s, ENV_TRANSPORT_VALUES
                ),
            });
        }
        opts.env_transport = s;
    }
    if let Some(v) = map.get("cwd") {
        let s = match v {
            Value::String(s) => s.clone(),
            _ => {
                return Err(MixError::RuntimeError {
                    span: None,
                    msg: "ssh_run: cwd must be a string".into(),
                });
            }
        };
        reject_nul("cwd", &s)?;
        opts.cwd = Some(s);
    }
    if let Some(v) = map.get("stdin") {
        let s = match v {
            Value::String(s) => s.clone(),
            _ => {
                return Err(MixError::RuntimeError {
                    span: None,
                    msg: "ssh_run: stdin must be a string".into(),
                });
            }
        };
        // stdin can legitimately contain NULs (binary payloads written
        // through the local ssh stdin pipe), so it is NOT nul-checked.
        opts.stdin = Some(s);
    }
    if let Some(v) = map.get("extra_ssh_args") {
        let xs = match v {
            Value::List(xs) => xs,
            _ => {
                return Err(MixError::RuntimeError {
                    span: None,
                    msg: "ssh_run: extra_ssh_args must be a list of strings".into(),
                });
            }
        };
        for (i, x) in xs.iter().enumerate() {
            let s = match x {
                Value::String(s) => s.clone(),
                _ => {
                    return Err(MixError::RuntimeError {
                        span: None,
                        msg: format!("ssh_run: extra_ssh_args[{}] must be a string", i),
                    });
                }
            };
            reject_nul(&format!("extra_ssh_args[{}]", i), &s)?;
            opts.extra_ssh_args.push(s);
        }
    }
    Ok(opts)
}

fn parse_nonneg_int_opt(caller: &str, name: &str, v: &Value) -> MixResult<u64> {
    let n = match extract_number(v, InputPolicy::NumberOnly) {
        Some(n) => n,
        None => {
            return Err(MixError::RuntimeError {
                span: None,
                msg: format!("{}: {} must be a non-negative integer", caller, name),
            });
        }
    };
    Ok(as_exact_integer(&format!("{caller}: {name}"), n, 0, i64::MAX)? as u64)
}

fn parse_bool_opt(name: &str, v: &Value) -> MixResult<bool> {
    match v {
        Value::Bool(b) => Ok(*b),
        _ => Err(MixError::RuntimeError {
            span: None,
            msg: format!("ssh_run: {} must be a boolean", name),
        }),
    }
}

fn build_remote_command(v: &Value, opts: &SshOpts) -> MixResult<String> {
    let parts: Vec<String> = match v {
        Value::String(s) => vec![s.clone()],
        Value::List(xs) if xs.is_empty() => {
            return Err(MixError::RuntimeError {
                span: None,
                msg: "ssh_run: commands list is empty".into(),
            });
        }
        Value::List(xs) => {
            let mut out = Vec::with_capacity(xs.len());
            for (i, x) in xs.iter().enumerate() {
                match x {
                    Value::String(s) => out.push(s.clone()),
                    _ => {
                        return Err(MixError::RuntimeError {
                            span: None,
                            msg: format!(
                                "ssh_run: commands list element at index {} is not a string",
                                i
                            ),
                        });
                    }
                }
            }
            out
        }
        _ => {
            return Err(MixError::RuntimeError {
                span: None,
                msg: "ssh_run: command must be string or list of strings".into(),
            });
        }
    };
    for p in &parts {
        reject_nul("command", p)?;
    }
    // §4: do NOT wrap each part in shell_quote — callers do that on
    // interpolated data inside the snippet. Just join with ` && `.
    let joined = parts.join(" && ");
    let mut prefix = String::new();
    // Use `export KEY='val';` rather than the `KEY=val command` shell
    // syntax: the latter sets the variable only for the first simple
    // command in a chain (`FOO=bar cmd1 && cmd2` leaves cmd2 with no
    // FOO), and is silently wrong with `cd` (a shell builtin) under
    // many shells. `export` puts the variable in the environment for
    // the entire remaining shell session, so every command in the
    // joined chain sees it.
    for (k, val) in &opts.env {
        if !is_valid_env_key(k) {
            return Err(MixError::RuntimeError {
                span: None,
                msg: format!(
                    "ssh_run: invalid env key {:?} (must match [A-Za-z_][A-Za-z0-9_]*)",
                    k
                ),
            });
        }
        prefix.push_str(&format!("export {}={}; ", k, shell_quote(val)));
    }
    if let Some(cwd) = &opts.cwd {
        prefix.push_str(&format!("cd {} && ", shell_quote(cwd)));
    }
    Ok(format!("{}{}", prefix, joined))
}

fn build_ssh_argv(host: &str, remote: &str, opts: &SshOpts) -> Vec<String> {
    let mut a = vec!["ssh".into()];
    if opts.batch {
        a.extend(["-o".into(), "BatchMode=yes".into()]);
    }
    a.extend([
        "-o".into(),
        format!("ConnectTimeout={}", opts.connect_timeout),
    ]);
    a.extend([
        "-o".into(),
        format!("StrictHostKeyChecking={}", opts.strict_host_key),
    ]);
    if opts.multiplex {
        // Resolve $HOME ourselves — std::process::Command bypasses the
        // shell, so a literal `~` would not expand. ssh's own `~` rules
        // inside ControlPath values are version-dependent; explicit
        // $HOME is reliable.
        //
        // `multiplex: true` weakens cancellation. `ControlPersist=60s`
        // keeps a mux master running after the initial slave exits. The first
        // call's master inherits our spawn-time PGID, but subsequent calls:
        //
        //   * spawn under a fresh PGID (a different `child_pid`),
        //   * connect to the *existing* master via the on-disk socket,
        //   * inherit the master's FDs through Unix-socket FD-passing,
        //
        // and the master is in the *prior* PGID — outside the current call's
        // `kill(-pgid, SIGKILL)` reach. A timeout cannot prove that the remote
        // work stopped. The bounded drain path still returns locally after a
        // short window, marking/encoding the timeout rather than joining a
        // capture worker indefinitely.
        //
        // The trade-off is intentional: callers choose connection reuse over
        // reliable remote cancellation. `ssh_close_multiplex(host)` (deferred,
        // see spec §6.5) is the future explicit teardown path.
        let home = std::env::var("HOME").unwrap_or_else(|_| ".".into());
        a.extend(["-o".into(), "ControlMaster=auto".into()]);
        a.extend(["-o".into(), format!("ControlPath={}/.ssh/cm-%C", home)]);
        a.extend(["-o".into(), "ControlPersist=60s".into()]);
    } else {
        // Explicitly opt out of any control-socket sharing the user's
        // `~/.ssh/config` may have configured globally (`ControlMaster
        // auto` + `ControlPath ...`). If a pre-existing mux master is
        // running for this host, the slave we'd otherwise spawn would
        // hand off its stdout/stderr FDs to the mux master via Unix-
        // socket FD-passing — which is in *its own* process group from
        // an earlier session, outside our spawn-time `setpgid(0, 0)`
        // group. Our timeout-kill then reaches our slave but not the
        // mux master. Setting `ControlPath=none` makes the spawned ssh
        // genuinely standalone so the PG kill normally stops all remote work,
        // rather than merely relying on bounded local drain abandonment.
        //
        // For the same reason, `multiplex: true` cannot make the same
        // cancellation guarantee — see the limitation block above.
        a.extend(["-o".into(), "ControlPath=none".into()]);
    }
    a.extend(opts.extra_ssh_args.iter().cloned());
    // `--` ends ssh option parsing so a host string can never be
    // interpreted as an ssh option. Without it, a host beginning with
    // `-` (e.g. `-oProxyCommand=<cmd>`) is parsed by ssh as an option
    // and can execute an arbitrary LOCAL command before any connection
    // — a local RCE whenever the host value is config-, mesh-, or
    // agent-derived. `builtin_ssh_run`/`builtin_ssh_must` also reject a
    // leading-dash host up front (defense in depth); `--` is the
    // load-bearing guard here.
    a.push("--".into());
    a.push(host.into());
    a.push(remote.into());
    a
}

fn ssh_result_map(host: &str, o: SshOutcome, elapsed: std::time::Duration) -> Value {
    let stdout_lossy = String::from_utf8_lossy(&o.stdout);
    let stderr_lossy = String::from_utf8_lossy(&o.stderr);
    // U+FFFD presence after a lossy decode flags invalid bytes that
    // got replaced. It is the standard "this output isn't byte-clean
    // UTF-8" signal — not a perfect test (the remote really could
    // emit a literal U+FFFD), but it's the same approximation other
    // shell-output crates use, and the false-positive rate on
    // typical tooling output is negligible.
    let utf8_lossy = stdout_lossy.contains('\u{FFFD}') || stderr_lossy.contains('\u{FFFD}');
    let mut m = indexmap::IndexMap::new();
    m.insert(
        "stdout".into(),
        Value::String(stdout_lossy.trim_end().to_string()),
    );
    m.insert(
        "stderr".into(),
        Value::String(stderr_lossy.trim_end().to_string()),
    );
    m.insert("exit_code".into(), Value::Number(o.exit_code as f64));
    m.insert("ok".into(), Value::Bool(o.exit_code == 0));
    m.insert(
        "duration_ms".into(),
        Value::Number(elapsed.as_millis() as f64),
    );
    m.insert("host".into(), Value::String(host.to_string()));
    m.insert("timed_out".into(), Value::Bool(o.timed_out));
    m.insert("interrupted".into(), Value::Bool(o.interrupted));
    m.insert("utf8_lossy".into(), Value::Bool(utf8_lossy));
    Value::map(m)
}

#[derive(Clone, Copy)]
enum ProcStdin<'a> {
    Null,
    Data(&'a [u8]),
    File(&'a str),
}

#[derive(Clone, Copy)]
enum ProcOutput<'a> {
    Capture,
    Inherit,
    Null,
    File(&'a RunArgvFile),
}

#[derive(Clone, Copy)]
enum ProcStderr<'a> {
    Capture,
    Inherit,
    Null,
    Stdout,
    File(&'a RunArgvFile),
}

/// Full request for [`run_process`] — the shared local child-process
/// lifecycle engine behind `run`, `run_rc`, `run_argv`, and the ssh
/// family (0.29.0 generalization of the old `run_with_timeout`
/// signature; that name survives as a compatibility adapter).
struct ProcSpec<'a> {
    argv: &'a [String],
    stdin: ProcStdin<'a>,
    stdout: ProcOutput<'a>,
    stderr: ProcStderr<'a>,
    /// Wall-clock deadline in milliseconds; 0 disables.
    timeout_ms: u64,
    /// Builtin name for diagnostics (`run`, `ssh_run`, `run_argv`, ...).
    caller: &'a str,
    /// Working directory for the child; `None` inherits.
    cwd: Option<&'a str>,
    /// Environment overlay applied on top of the inherited environment
    /// (or on an empty one when `clear_env`).
    env: &'a [(String, String)],
    clear_env: bool,
    /// Per-stream output cap in bytes; `None` = unbounded. Excess is
    /// drained and DISCARDED (the child is not killed, the pipe never
    /// backs up) with the outcome's truncation flag set.
    max_output: Option<usize>,
    /// Tee raw child chunks to the corresponding parent stream while
    /// retaining the normal capture result.
    stream: bool,
}

/// Outcome of [`run_process`]. `exit_code` keeps the legacy sentinel
/// encoding consumed by `run`/`run_rc`/`ssh_*` (-2 interrupted, -1
/// timed out, 128+signo signal-killed, -3 unknown); `signal` and
/// `natural_code` carry the undecoded facts for `run_argv`'s richer
/// result schema.
struct ProcOutcome {
    stdout: Vec<u8>,
    stderr: Vec<u8>,
    exit_code: i32,
    timed_out: bool,
    interrupted: bool,
    /// Signal that terminated the child, when it died to one.
    signal: Option<i32>,
    /// The child's real exit code, when it exited normally.
    natural_code: Option<i32>,
    stdout_truncated: bool,
    stderr_truncated: bool,
    duration_ms: u128,
}

#[derive(Clone, Copy)]
enum LiveStream {
    Stdout,
    Stderr,
}

impl LiveStream {
    fn write_chunk(self, chunk: &[u8]) -> std::io::Result<()> {
        use std::io::Write;
        match self {
            LiveStream::Stdout => {
                let mut out = std::io::stdout().lock();
                out.write_all(chunk)?;
                out.flush()
            }
            LiveStream::Stderr => {
                let mut out = std::io::stderr().lock();
                out.write_all(chunk)?;
                out.flush()
            }
        }
    }
}

#[derive(Default)]
struct DrainState {
    bytes: Vec<u8>,
    truncated: bool,
    error: Option<std::io::Error>,
}

struct DrainTask {
    state: std::sync::Arc<std::sync::Mutex<DrainState>>,
    done: std::sync::Arc<std::sync::atomic::AtomicBool>,
    live_enabled: Option<std::sync::Arc<std::sync::Mutex<bool>>>,
    handle: Option<std::thread::JoinHandle<()>>,
}

impl DrainTask {
    fn is_done(&self) -> bool {
        self.done.load(std::sync::atomic::Ordering::Acquire)
    }

    fn stop_live_tee(&self) {
        if let Some(enabled) = &self.live_enabled {
            *enabled
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner()) = false;
        }
    }
}

/// Drain into shared state so a hard deadline can take an honest snapshot and
/// abandon a reader whose EOF is held open by a detached descendant. Rust
/// threads cannot be cancelled safely; dropping the JoinHandle detaches that
/// reader, while the returned truncation flag records that EOF was not seen.
fn spawn_capped_drain(
    mut pipe: Box<dyn std::io::Read + Send>,
    cap: Option<usize>,
    mut live: Option<LiveStream>,
) -> DrainTask {
    use std::io::Read;
    use std::sync::atomic::Ordering;

    let state = std::sync::Arc::new(std::sync::Mutex::new(DrainState::default()));
    let done = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
    let live_enabled = live.map(|_| std::sync::Arc::new(std::sync::Mutex::new(true)));
    let thread_state = std::sync::Arc::clone(&state);
    let thread_done = std::sync::Arc::clone(&done);
    let thread_live_enabled = live_enabled.as_ref().map(std::sync::Arc::clone);
    let handle = std::thread::spawn(move || {
        let mut chunk = [0u8; 65536];
        loop {
            let n = match pipe.read(&mut chunk) {
                Ok(0) => break,
                Ok(n) => n,
                Err(error) if error.kind() == std::io::ErrorKind::Interrupted => continue,
                Err(error) => {
                    thread_state.lock().unwrap().error = Some(error);
                    break;
                }
            };
            {
                let mut state = thread_state.lock().unwrap();
                match cap {
                    None => state.bytes.extend_from_slice(&chunk[..n]),
                    Some(cap) if !state.truncated => {
                        let room = cap.saturating_sub(state.bytes.len());
                        if n <= room {
                            state.bytes.extend_from_slice(&chunk[..n]);
                        } else {
                            state.bytes.extend_from_slice(&chunk[..room]);
                            state.truncated = true;
                        }
                    }
                    Some(_) => {}
                }
            }
            if let Some(enabled) = &thread_live_enabled {
                // Abandonment takes this same lock to disable teeing. Holding
                // it across the enabled check and the real-fd write means an
                // in-flight chunk either finishes before stop_live_tee returns
                // or observes false and is suppressed.
                let enabled = enabled
                    .lock()
                    .unwrap_or_else(|poisoned| poisoned.into_inner());
                if *enabled
                    && let Some(dest) = live
                    && dest.write_chunk(&chunk[..n]).is_err()
                {
                    live = None;
                }
            }
        }
        thread_done.store(true, Ordering::Release);
    });
    DrainTask {
        state,
        done,
        live_enabled,
        handle: Some(handle),
    }
}

fn collect_drain(
    mut task: DrainTask,
    caller: &str,
    stream: &str,
    allow_abandon: bool,
) -> MixResult<(Vec<u8>, bool)> {
    let abandoned = allow_abandon && !task.is_done();
    if abandoned {
        // The detached reader can still receive bytes from an escaped
        // descendant.  Disable its parent-stream side effect before returning;
        // it may continue draining its private capture pipe to EOF, but it no
        // longer owns the completed call's right to write to fd 1 or fd 2.
        task.stop_live_tee();
    } else if let Some(handle) = task.handle.take()
        && handle.join().is_err()
    {
        return Err(MixError::structured(
            "PROCESS_INTERNAL",
            format!("{caller}: {stream} drain thread panicked"),
        ));
    }
    // An unfinished handle is deliberately detached. Snapshot only after the
    // done check; if the reader races to EOF now, reporting truncation remains
    // conservative and truthful.
    let mut state = task.state.lock().map_err(|_| {
        MixError::structured(
            "PROCESS_INTERNAL",
            format!("{caller}: {stream} drain state was poisoned"),
        )
    })?;
    if let Some(error) = state.error.take() {
        return Err(MixError::structured(
            "PROCESS_IO",
            format!("{caller}: {stream} drain failed: {error}"),
        ));
    }
    Ok((
        std::mem::take(&mut state.bytes),
        state.truncated || abandoned,
    ))
}

/// Spawn a child process and collect its outcome — the shared engine
/// behind `run`, `run_rc`, `run_argv`, and the ssh family.
///
/// Spawn `argv`, drive it under timeout + interrupt cooperation, and
/// collect its stdout/stderr/exit. Implements spec §8 / §10.6:
///
/// - Three concurrent drain threads (stdin writer, stdout reader,
///   stderr reader) avoid the classic full-pipe deadlock where a
///   child blocked on flushing stdout would prevent it ever reading
///   our stdin (or vice-versa). Completed workers are joined after the child
///   is reaped so I/O errors remain observable. At a deadline, unfinished
///   workers are detached after a short bounded drain window.
/// - A 50ms poll loop checks three conditions every tick:
///     1. `try_wait()` → child exited naturally; capture status.
///     2. `crate::interrupt::is_interrupted()` → user pressed Ctrl-C.
///        Wins over timeout in the tie-breaker (the user's intent
///        beats the wall clock).
///     3. `start.elapsed() >= timeout` (when `timeout_s > 0`) → set
///        `timed_out`. `timeout_s == 0` disables the deadline.
/// - On interrupt or timeout, escalate kill (cause-specific — see the
///   "Process-group discipline" section below for the rationale):
///     * **Timeout** → SIGKILL to the process group immediately. The
///       deadline is hard, no cooperative grace.
///     * **Interrupt** → SIGTERM to the process group → poll up to 2s
///       for cooperative exit → SIGKILL if still alive.
///     * **`try_wait` failure** → SIGKILL to the process group (state
///       is unknown, escalate immediately).
///
///   After the kill path, do a final blocking `wait()` so the OS
///   releases the direct child's PID slot.
/// - Exit-code sentinels (kept disjoint to let callers distinguish):
///     * `-2`: interrupted
///     * `-1`: timed out (and not interrupted)
///     * `128 + signo`: signal-killed with no exit code (Unix)
///     * `-3`: no code and no signal (non-Unix fallback, or a Unix
///       edge case we can't characterize)
///
/// Process-group discipline (Unix only): the spawned child is placed
/// in its own process group via `setpgid(0, 0)` at spawn time so the
/// kill path can signal the entire group with `kill(-pgid, sig)`. This
/// is load-bearing for two reasons:
///
///   1. `ssh` (and any child that forks helpers, like a multiplex
///      master) leaves descendants that inherit the stdout/stderr
///      pipes. Killing only the direct child orphans those descendants
///      to init while their FDs keep our drain threads' `read_to_end`
///      blocked until they exit naturally unless the caller abandons the
///      capture at its deadline.
///   2. `kill(-pgid, sig)` reaches every member of the group atomically
///      regardless of whether the leader is still alive, which closes
///      the inherited pipe FDs and lets the drain threads complete.
///
/// Escalation differs by cause:
///   * **Timeout** is a hard local deadline. SIGKILL goes straight to
///     the process group — SIGTERM is cooperative (`ssh` interprets it
///     as "tear down the channel cleanly", which can wait for the
///     remote to finish), so a graceful path here would re-introduce
///     the wall-clock drift the deadline exists to bound.
///   * **Interrupt** (Ctrl-C) prefers a cooperative exit: SIGTERM to
///     the group, 2s grace, then SIGKILL.
///   * **`try_wait` failure** is a defensive path — SIGKILL the group
///     immediately, since we can't trust the child's state.
///
/// On non-Unix targets we fall back to `child.kill()` (SIGKILL to the
/// direct child only); descendant cleanup is the OS's problem there
/// and `ssh_run` is currently used only from Unix hosts.
///
/// Residual limitations of the PG-kill substrate:
///
///   * **Detached descendants.** A child that intentionally calls
///     `setsid()` / double-forks while keeping our inherited
///     stdout/stderr FDs open escapes our process group. The PG-kill
///     does not reach it. After a bounded post-kill drain window Mix
///     snapshots the captured prefix, marks it truncated, and detaches
///     the reader rather than waiting for EOF. Linux
///     `PR_SET_PDEATHSIG` (armed below in pre_exec) covers the
///     direct child if *we* die before the kill path runs, but it
///     does not propagate to grandchildren that the direct child
///     itself spawned and detached. Proving that such work stopped requires
///     cgroup / subreaper containment; the local return deadline remains
///     bounded without it.
///   * **Pre-existing out-of-PG FD holders.** A process started before
///     our spawn (a user-config `ControlMaster` mux master, an unrelated
///     daemon that opened our pipe via FD-passing) is in its own PG
///     from the start. For the canonical ssh case, `build_ssh_argv`
///     emits `-o ControlPath=none` on the default (non-multiplex) path
///     so a fresh client we spawn cannot hand its FDs to a stale mux
///     master. The `multiplex: true` opt-in trades reliable remote
///     cancellation for connection reuse; bounded drain abandonment still
///     preserves the local return deadline.
fn process_stdio_error(caller: &str, stream: &str, path: &str, error: &std::io::Error) -> MixError {
    MixError::structured(
        "PROCESS_STDIO",
        format!(
            "{caller}: opening {stream} file '{}' failed: {error}",
            sanitize_for_diag(path)
        ),
    )
}

fn process_open_timeout(caller: &str, stream: &str, path: &str) -> MixError {
    MixError::structured(
        "PROCESS_STDIO",
        format!(
            "{caller}: opening {stream} file '{}' exceeded the process deadline",
            sanitize_for_diag(path)
        ),
    )
}

#[cfg(unix)]
fn restore_blocking(file: &std::fs::File) -> std::io::Result<()> {
    use std::os::fd::AsRawFd;

    // SAFETY: fcntl only reads/updates the status flags of this owned fd.
    let flags = unsafe { libc::fcntl(file.as_raw_fd(), libc::F_GETFL) };
    if flags == -1 {
        return Err(std::io::Error::last_os_error());
    }
    if flags & libc::O_NONBLOCK != 0
        && unsafe { libc::fcntl(file.as_raw_fd(), libc::F_SETFL, flags & !libc::O_NONBLOCK) } == -1
    {
        return Err(std::io::Error::last_os_error());
    }
    Ok(())
}

#[cfg(not(unix))]
fn restore_blocking(_file: &std::fs::File) -> std::io::Result<()> {
    Ok(())
}

fn recv_open_result(
    caller: &str,
    stream: &str,
    path: &str,
    deadline: Option<std::time::Instant>,
    receiver: std::sync::mpsc::Receiver<std::io::Result<std::fs::File>>,
) -> MixResult<std::fs::File> {
    let result = match deadline {
        Some(deadline) => {
            let Some(remaining) = deadline.checked_duration_since(std::time::Instant::now()) else {
                return Err(process_open_timeout(caller, stream, path));
            };
            receiver
                .recv_timeout(remaining)
                .map_err(|error| match error {
                    std::sync::mpsc::RecvTimeoutError::Timeout => {
                        process_open_timeout(caller, stream, path)
                    }
                    std::sync::mpsc::RecvTimeoutError::Disconnected => MixError::structured(
                        "PROCESS_STDIO",
                        format!("{caller}: opening {stream} file worker failed"),
                    ),
                })?
        }
        None => receiver.recv().map_err(|_| {
            MixError::structured(
                "PROCESS_STDIO",
                format!("{caller}: opening {stream} file worker failed"),
            )
        })?,
    };
    let file = result.map_err(|error| process_stdio_error(caller, stream, path, &error))?;
    restore_blocking(&file).map_err(|error| process_stdio_error(caller, stream, path, &error))?;
    Ok(file)
}

fn open_process_input(
    caller: &str,
    path: &str,
    deadline: Option<std::time::Instant>,
) -> MixResult<std::fs::File> {
    #[cfg(unix)]
    {
        use std::os::fd::AsRawFd;
        use std::os::unix::fs::{FileTypeExt, OpenOptionsExt};

        if deadline.is_some_and(|deadline| std::time::Instant::now() >= deadline) {
            return Err(process_open_timeout(caller, "stdin", path));
        }
        let input_anchor = {
            let mut options = std::fs::OpenOptions::new();
            options
                .read(true)
                .custom_flags(libc::O_CLOEXEC | libc::O_PATH);
            options
                .open(path)
                .map_err(|error| process_stdio_error(caller, "stdin", path, &error))?
        };
        let is_fifo = input_anchor
            .metadata()
            .map_err(|error| process_stdio_error(caller, "stdin", path, &error))?
            .file_type()
            .is_fifo();
        let owned_path = format!("/proc/self/fd/{}", input_anchor.as_raw_fd());
        let worker_path = owned_path.clone();
        let (sender, receiver) = std::sync::mpsc::sync_channel(1);
        let handle = std::thread::spawn(move || {
            let mut options = std::fs::OpenOptions::new();
            options.read(true).custom_flags(libc::O_CLOEXEC);
            let _ = sender.send(options.open(worker_path));
        });
        let result = match deadline {
            Some(deadline) => {
                let remaining = deadline.saturating_duration_since(std::time::Instant::now());
                match receiver.recv_timeout(remaining) {
                    Ok(result) => result,
                    Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => {
                        if handle.join().is_err() {
                            return Err(MixError::structured(
                                "PROCESS_STDIO",
                                format!("{caller}: opening stdin file worker panicked"),
                            ));
                        }
                        return Err(MixError::structured(
                            "PROCESS_STDIO",
                            format!("{caller}: opening stdin file worker failed"),
                        ));
                    }
                    Err(std::sync::mpsc::RecvTimeoutError::Timeout) if is_fifo => {
                        // Reopen the anchored inode, not the user pathname.
                        // O_NONBLOCK plus the pending read-open makes this wake
                        // incapable of blocking, even after rename/unlink.
                        let mut wake = std::fs::OpenOptions::new();
                        wake.write(true)
                            .custom_flags(libc::O_CLOEXEC | libc::O_NONBLOCK);
                        let wake_writer = wake
                            .open(&owned_path)
                            .map_err(|error| process_stdio_error(caller, "stdin", path, &error))?;
                        let _ = receiver.recv().map_err(|_| {
                            MixError::structured(
                                "PROCESS_STDIO",
                                format!("{caller}: opening stdin file worker failed"),
                            )
                        })?;
                        drop(wake_writer);
                        if handle.join().is_err() {
                            return Err(MixError::structured(
                                "PROCESS_STDIO",
                                format!("{caller}: opening stdin file worker panicked"),
                            ));
                        }
                        return Err(process_open_timeout(caller, "stdin", path));
                    }
                    Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {
                        return Err(process_open_timeout(caller, "stdin", path));
                    }
                }
            }
            None => receiver.recv().map_err(|_| {
                MixError::structured(
                    "PROCESS_STDIO",
                    format!("{caller}: opening stdin file worker failed"),
                )
            })?,
        };
        if handle.join().is_err() {
            return Err(MixError::structured(
                "PROCESS_STDIO",
                format!("{caller}: opening stdin file worker panicked"),
            ));
        }
        let file = result.map_err(|error| process_stdio_error(caller, "stdin", path, &error))?;
        restore_blocking(&file)
            .map_err(|error| process_stdio_error(caller, "stdin", path, &error))?;
        Ok(file)
    }

    #[cfg(not(unix))]
    {
        let owned_path = path.to_string();
        let (sender, receiver) = std::sync::mpsc::sync_channel(1);
        std::thread::spawn(move || {
            let mut options = std::fs::OpenOptions::new();
            options.read(true);
            let _ = sender.send(options.open(owned_path));
        });
        recv_open_result(caller, "stdin", path, deadline, receiver)
    }
}

fn open_process_output(
    caller: &str,
    stream: &str,
    route: &RunArgvFile,
    deadline: Option<std::time::Instant>,
) -> MixResult<std::fs::File> {
    let owned_route = route.clone();
    let (sender, receiver) = std::sync::mpsc::sync_channel(1);
    std::thread::spawn(move || {
        let mut options = std::fs::OpenOptions::new();
        options.write(true).create(true);
        if owned_route.append {
            options.append(true);
        }
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options
                .mode(owned_route.mode)
                .custom_flags(libc::O_NONBLOCK | libc::O_CLOEXEC);
        }
        let _ = sender.send(options.open(owned_route.path));
    });
    recv_open_result(caller, stream, &route.path, deadline, receiver)
}

/// Truncate a non-append output route, once every route in the same call has
/// opened.
///
/// `open_process_output` deliberately omits `O_TRUNC`: a route opened early
/// must not be destroyed by a *later* route failing to open. A caller opens the
/// whole set first, and only then truncates — so a `PROCESS_STDIO`/
/// `PIPELINE_STDIO` failure cannot promise a transaction: every route opens
/// before truncation begins, but an earlier successful truncation remains if a
/// later truncation fails.
fn truncate_process_output(
    caller: &str,
    stream: &str,
    route: &RunArgvFile,
    file: &std::fs::File,
) -> MixResult<()> {
    if route.append {
        return Ok(());
    }
    // `O_TRUNC` is a no-op on non-regular targets (`/dev/null`, a character
    // device, a FIFO) while `set_len` fails on them, so ask the same question
    // the kernel would and skip those.
    match file.metadata() {
        Ok(meta) if !meta.is_file() => return Ok(()),
        Ok(_) => {}
        Err(error) => return Err(process_stdio_error(caller, stream, &route.path, &error)),
    }
    file.set_len(0)
        .map_err(|error| process_stdio_error(caller, stream, &route.path, &error))
}

#[cfg(unix)]
fn process_capture_pipe(caller: &str) -> MixResult<(std::fs::File, std::fs::File)> {
    use std::os::fd::FromRawFd;

    let mut fds = [-1; 2];
    // CLOEXEC is load-bearing for pipelines: each source fd is duped onto the
    // requested child stdio fd (which clears CLOEXEC on that target), while
    // unrelated pipe ends must disappear at exec. Without it, an upstream
    // writer can inherit its own read end and never receive SIGPIPE.
    #[cfg(any(target_os = "linux", target_os = "android"))]
    let pipe_result = unsafe { libc::pipe2(fds.as_mut_ptr(), libc::O_CLOEXEC) };
    #[cfg(not(any(target_os = "linux", target_os = "android")))]
    let pipe_result = unsafe { libc::pipe(fds.as_mut_ptr()) };
    if pipe_result == -1 {
        return Err(MixError::structured(
            "PROCESS_STDIO",
            format!(
                "{caller}: creating process pipe failed: {}",
                std::io::Error::last_os_error()
            ),
        ));
    }
    #[cfg(not(any(target_os = "linux", target_os = "android")))]
    for fd in fds {
        // SAFETY: successful pipe() returned owned descriptors. If setting
        // CLOEXEC fails, close both below through File ownership and report a
        // setup error rather than creating a descriptor-leaking pipeline.
        if unsafe { libc::fcntl(fd, libc::F_SETFD, libc::FD_CLOEXEC) } == -1 {
            // SAFETY: both descriptors are fresh and not yet File-owned.
            unsafe {
                libc::close(fds[0]);
                libc::close(fds[1]);
            }
            return Err(MixError::structured(
                "PROCESS_STDIO",
                format!(
                    "{caller}: setting close-on-exec on pipe failed: {}",
                    std::io::Error::last_os_error()
                ),
            ));
        }
    }
    // SAFETY: successful `pipe` returned two fresh, owned descriptors.
    let reader = unsafe { std::fs::File::from_raw_fd(fds[0]) };
    let writer = unsafe { std::fs::File::from_raw_fd(fds[1]) };
    Ok((reader, writer))
}

#[cfg(not(unix))]
fn process_capture_pipe(caller: &str) -> MixResult<(std::fs::File, std::fs::File)> {
    Err(MixError::structured(
        "PROCESS_STDIO",
        format!("{caller}: stderr:\"stdout\" is not supported on this platform"),
    ))
}

/// A handle on the parent's OWN stdout, for `stderr: "stdout"` when stdout is
/// `"inherit"`.
///
/// `Stdio::inherit()` on stderr would hand the child the parent's *stderr*,
/// which is a different destination the moment the two are redirected apart —
/// and `stderr: "stdout"` promises the selected stdout destination, exactly as
/// `2>&1` does.
#[cfg(unix)]
fn inherited_stdout_handle(caller: &str) -> MixResult<std::fs::File> {
    use std::os::fd::FromRawFd;

    // F_DUPFD_CLOEXEC: the spare descriptor must not leak into an unrelated
    // child spawned concurrently; Command re-wires it onto fd 2 at exec.
    let fd = unsafe { libc::fcntl(libc::STDOUT_FILENO, libc::F_DUPFD_CLOEXEC, 0) };
    if fd == -1 {
        return Err(MixError::structured(
            "PROCESS_STDIO",
            format!(
                "{caller}: duplicating the parent's stdout for stderr:\"stdout\" failed: {}",
                std::io::Error::last_os_error()
            ),
        ));
    }
    // SAFETY: a successful `fcntl(F_DUPFD_CLOEXEC)` returns a fresh owned
    // descriptor with no other owner.
    Ok(unsafe { std::fs::File::from_raw_fd(fd) })
}

fn clone_process_stdio(
    caller: &str,
    stream: &str,
    path: &str,
    file: &std::fs::File,
) -> MixResult<std::fs::File> {
    file.try_clone()
        .map_err(|error| process_stdio_error(caller, stream, path, &error))
}

struct PipelineChildRuntime {
    child: std::process::Child,
    pid: u32,
    started: std::time::Instant,
    status: Option<std::process::ExitStatus>,
    duration_ms: u128,
}

struct PipelineRawStageOutcome {
    natural_code: Option<i32>,
    signal: Option<i32>,
    duration_ms: u128,
    stderr: Vec<u8>,
    stderr_truncated: bool,
}

struct PipelineProcessOutcome {
    stages: Vec<PipelineRawStageOutcome>,
    stdout: Vec<u8>,
    timed_out: bool,
    interrupted: bool,
    duration_ms: u128,
    stdout_truncated: bool,
}

fn pipeline_process_error(error: MixError) -> MixError {
    match error {
        MixError::Structured(info) => {
            let code = match info.code.as_str() {
                "PROCESS_STDIO" => "PIPELINE_STDIO",
                "PROCESS_SPAWN" => "PIPELINE_SPAWN",
                "PROCESS_IO" => "PIPELINE_IO",
                _ => "PIPELINE_INTERNAL",
            };
            MixError::structured(code, info.message)
        }
        other => other,
    }
}

#[cfg(unix)]
fn signal_pipeline_groups(children: &[PipelineChildRuntime], signal: i32) {
    for child in children {
        // SAFETY: every child becomes the leader of a fresh process group in
        // pre_exec below, so -pid addresses that stage and all descendants.
        // A stage which has already exited may return ESRCH; that is harmless.
        unsafe {
            libc::kill(-(child.pid as i32), signal);
        }
    }
}

#[cfg(not(unix))]
fn signal_pipeline_groups(_children: &[PipelineChildRuntime], _signal: i32) {}

fn reap_pipeline_after_setup_failure(children: &mut [PipelineChildRuntime]) {
    #[cfg(unix)]
    signal_pipeline_groups(children, libc::SIGKILL);
    #[cfg(not(unix))]
    for runtime in children.iter_mut() {
        let _ = runtime.child.kill();
    }
    for runtime in children {
        if let Ok(status) = runtime.child.wait() {
            runtime.status = Some(status);
            runtime.duration_ms = runtime.started.elapsed().as_millis();
        }
    }
}

fn pipeline_started_stage_values(
    stages: &[PipelineStage],
    children: &[PipelineChildRuntime],
) -> Value {
    let values = children
        .iter()
        .enumerate()
        .map(|(index, runtime)| {
            let natural_code = runtime.status.as_ref().and_then(|status| status.code());
            #[cfg(unix)]
            let signal = {
                use std::os::unix::process::ExitStatusExt;
                runtime.status.as_ref().and_then(|status| status.signal())
            };
            #[cfg(not(unix))]
            let signal: Option<i32> = None;
            let mut map = indexmap::IndexMap::new();
            map.insert("index".to_string(), Value::Number(index as f64));
            map.insert(
                "argv".to_string(),
                Value::list(
                    stages[index]
                        .argv
                        .iter()
                        .cloned()
                        .map(Value::String)
                        .collect(),
                ),
            );
            map.insert("ok".to_string(), Value::Bool(natural_code == Some(0)));
            map.insert(
                "exit_code".to_string(),
                natural_code.map_or(Value::Nil, |code| Value::Number(code as f64)),
            );
            map.insert(
                "signal".to_string(),
                signal.map_or(Value::Nil, |signal| Value::Number(signal as f64)),
            );
            map.insert(
                "duration_ms".to_string(),
                Value::Number(runtime.duration_ms as f64),
            );
            map.insert("stderr".to_string(), Value::String(String::new()));
            map.insert(
                "stderr_truncated".to_string(),
                Value::Bool(matches!(stages[index].opts.stderr, RunArgvStderr::Capture)),
            );
            map.insert("utf8_lossy".to_string(), Value::Bool(false));
            map.insert("accepted_signal".to_string(), Value::Bool(false));
            Value::map(map)
        })
        .collect();
    Value::list(values)
}

fn run_pipeline_processes(
    caller: &str,
    stages: &[PipelineStage],
    opts: &PipelineOpts,
) -> MixResult<PipelineProcessOutcome> {
    use std::io::Write;
    use std::process::{Command, Stdio};
    use std::time::{Duration, Instant};

    let start = Instant::now();
    let timeout = (opts.timeout_ms != 0).then(|| Duration::from_millis(opts.timeout_ms));
    let deadline = timeout.map(|timeout| start + timeout);
    let last = stages.len() - 1;
    let mut children: Vec<PipelineChildRuntime> = Vec::with_capacity(stages.len());

    // Preflight every descriptor before ANY stage is spawned. This includes
    // the ordinary inter-stage pipes and all capture/data pipes: relying on
    // Stdio::piped() inside Command::spawn would let a later EMFILE happen
    // after an earlier stage had already run.
    #[derive(Default)]
    struct StageStdioFiles {
        stdin: Option<std::fs::File>,
        stdout: Option<std::fs::File>,
        stderr: Option<std::fs::File>,
        stderr_merge: Option<std::fs::File>,
    }
    let mut pipeline_readers = Vec::with_capacity(last);
    let mut pipeline_writers = Vec::with_capacity(last);
    for _ in 0..last {
        let (reader, writer) = process_capture_pipe(caller).map_err(pipeline_process_error)?;
        pipeline_readers.push(Some(reader));
        pipeline_writers.push(Some(writer));
    }
    let (mut stdin_data_reader, stdin_data_writer) =
        if matches!(stages[0].opts.stdin, RunArgvStdin::Data(_)) {
            let (reader, writer) = process_capture_pipe(caller).map_err(pipeline_process_error)?;
            (Some(reader), Some(writer))
        } else {
            (None, None)
        };
    let (mut final_stdout_reader, mut final_stdout_writer) =
        if matches!(stages[last].opts.stdout, RunArgvOutput::Capture) {
            let (reader, writer) = process_capture_pipe(caller).map_err(pipeline_process_error)?;
            (Some(reader), Some(writer))
        } else {
            (None, None)
        };
    let mut stderr_capture_readers = Vec::with_capacity(stages.len());
    let mut stderr_capture_writers = Vec::with_capacity(stages.len());
    for stage in stages {
        if matches!(stage.opts.stderr, RunArgvStderr::Capture) {
            let (reader, writer) = process_capture_pipe(caller).map_err(pipeline_process_error)?;
            stderr_capture_readers.push(Some(reader));
            stderr_capture_writers.push(Some(writer));
        } else {
            stderr_capture_readers.push(None);
            stderr_capture_writers.push(None);
        }
    }

    let mut stage_files: Vec<StageStdioFiles> = Vec::with_capacity(stages.len());
    let mut pending_truncate: Vec<(&'static str, &RunArgvFile, std::fs::File)> = Vec::new();
    for (index, stage) in stages.iter().enumerate() {
        let mut files = StageStdioFiles::default();
        // Validation already confines stdin to the first stage and a stdout
        // route to the last; opening whatever is present keeps this pass honest
        // if that ever widens.
        if let RunArgvStdin::File(path) = &stage.opts.stdin {
            files.stdin =
                Some(open_process_input(caller, path, deadline).map_err(pipeline_process_error)?);
        }
        if let RunArgvOutput::File(route) = &stage.opts.stdout {
            let file = open_process_output(caller, "stdout", route, deadline)
                .map_err(pipeline_process_error)?;
            pending_truncate.push((
                "stdout",
                route,
                clone_process_stdio(caller, "stdout", &route.path, &file)
                    .map_err(pipeline_process_error)?,
            ));
            files.stdout = Some(file);
        }
        if let RunArgvStderr::File(route) = &stage.opts.stderr {
            let file = open_process_output(caller, "stderr", route, deadline)
                .map_err(pipeline_process_error)?;
            pending_truncate.push((
                "stderr",
                route,
                clone_process_stdio(caller, "stderr", &route.path, &file)
                    .map_err(pipeline_process_error)?,
            ));
            files.stderr = Some(file);
        }
        if matches!(stage.opts.stderr, RunArgvStderr::Stdout) {
            files.stderr_merge = Some(if index < last {
                clone_process_stdio(
                    caller,
                    "stdout",
                    "pipeline",
                    pipeline_writers[index]
                        .as_ref()
                        .expect("inter-stage writer was pre-created"),
                )
                .map_err(pipeline_process_error)?
            } else {
                match &stage.opts.stdout {
                    RunArgvOutput::Capture => clone_process_stdio(
                        caller,
                        "stdout",
                        "capture",
                        final_stdout_writer
                            .as_ref()
                            .expect("final capture writer was pre-created"),
                    )
                    .map_err(pipeline_process_error)?,
                    RunArgvOutput::Inherit => {
                        #[cfg(unix)]
                        {
                            inherited_stdout_handle(caller).map_err(pipeline_process_error)?
                        }
                        #[cfg(not(unix))]
                        {
                            return Err(MixError::structured(
                                "PIPELINE_STDIO",
                                format!("{caller}: stderr:\"stdout\" is unsupported"),
                            ));
                        }
                    }
                    RunArgvOutput::Null => std::fs::OpenOptions::new()
                        .write(true)
                        .open("/dev/null")
                        .map_err(|error| {
                            pipeline_process_error(process_stdio_error(
                                caller,
                                "stdout",
                                "/dev/null",
                                &error,
                            ))
                        })?,
                    RunArgvOutput::File(route) => clone_process_stdio(
                        caller,
                        "stdout",
                        &route.path,
                        files
                            .stdout
                            .as_ref()
                            .expect("stdout file route was pre-opened"),
                    )
                    .map_err(pipeline_process_error)?,
                }
            });
        }
        stage_files.push(files);
    }
    for (stream, route, file) in pending_truncate {
        truncate_process_output(caller, stream, route, &file).map_err(pipeline_process_error)?;
    }

    for (index, stage) in stages.iter().enumerate() {
        let mut files = std::mem::take(&mut stage_files[index]);
        let configured = (|| -> MixResult<std::process::Child> {
            let mut command = Command::new(&stage.argv[0]);
            command.args(&stage.argv[1..]);

            if index == 0 {
                match &stage.opts.stdin {
                    RunArgvStdin::Null => {
                        command.stdin(Stdio::null());
                    }
                    RunArgvStdin::Data(_) => {
                        command.stdin(Stdio::from(
                            stdin_data_reader
                                .take()
                                .expect("pipeline stdin pipe was pre-created"),
                        ));
                    }
                    RunArgvStdin::File(_) => {
                        command.stdin(Stdio::from(
                            files
                                .stdin
                                .take()
                                .expect("stage stdin file route was pre-opened"),
                        ));
                    }
                }
            } else {
                command.stdin(Stdio::from(pipeline_readers[index - 1].take().expect(
                    "every non-first pipeline stage has a pre-created input pipe",
                )));
            }

            if index < last {
                command.stdout(Stdio::from(
                    pipeline_writers[index]
                        .take()
                        .expect("inter-stage writer was pre-created"),
                ));
            } else {
                match &stage.opts.stdout {
                    RunArgvOutput::Capture => {
                        command.stdout(Stdio::from(
                            final_stdout_writer
                                .take()
                                .expect("final stdout pipe was pre-created"),
                        ));
                    }
                    RunArgvOutput::Inherit => {
                        command.stdout(Stdio::inherit());
                    }
                    RunArgvOutput::Null => {
                        command.stdout(Stdio::null());
                    }
                    RunArgvOutput::File(_) => {
                        let file = files
                            .stdout
                            .take()
                            .expect("stage stdout file route was pre-opened");
                        command.stdout(Stdio::from(file));
                    }
                }
            }

            if matches!(stage.opts.stderr, RunArgvStderr::Stdout) {
                command.stderr(Stdio::from(
                    files
                        .stderr_merge
                        .take()
                        .expect("stderr merge descriptor was pre-created"),
                ));
            } else {
                match &stage.opts.stderr {
                    RunArgvStderr::Capture => {
                        command.stderr(Stdio::from(
                            stderr_capture_writers[index]
                                .take()
                                .expect("stderr capture pipe was pre-created"),
                        ));
                    }
                    RunArgvStderr::Inherit => {
                        command.stderr(Stdio::inherit());
                    }
                    RunArgvStderr::Null => {
                        command.stderr(Stdio::null());
                    }
                    RunArgvStderr::File(_) => {
                        command.stderr(Stdio::from(
                            files
                                .stderr
                                .take()
                                .expect("stage stderr file route was pre-opened"),
                        ));
                    }
                    RunArgvStderr::Stdout => {
                        unreachable!("stderr merge was configured with the stage stdout")
                    }
                }
            }

            if stage.opts.clear_env {
                command.env_clear();
            }
            for (key, value) in &stage.opts.env {
                command.env(key, value);
            }
            if let Some(cwd) = &stage.opts.cwd {
                command.current_dir(cwd);
            }

            // Match run_process: each direct child leads a fresh process group,
            // and timeout/interrupt signalling targets the entire group. A
            // pipeline uses one deadline but one group per stage, avoiding the
            // race where a short-lived common group leader exits while later
            // stages are still being spawned.
            #[cfg(unix)]
            {
                use std::os::unix::process::CommandExt;
                let parent_pid = std::process::id() as libc::pid_t;
                // SAFETY: identical post-fork syscall-only setup to run_process.
                unsafe {
                    command.pre_exec(move || {
                        if libc::setpgid(0, 0) == -1 {
                            return Err(std::io::Error::last_os_error());
                        }
                        #[cfg(target_os = "linux")]
                        {
                            libc::prctl(
                                libc::PR_SET_PDEATHSIG,
                                libc::SIGKILL as libc::c_ulong,
                                0 as libc::c_ulong,
                                0 as libc::c_ulong,
                                0 as libc::c_ulong,
                            );
                            if libc::getppid() != parent_pid {
                                libc::_exit(0);
                            }
                        }
                        Ok(())
                    });
                }
            }

            let child = command.spawn().map_err(|error| {
                MixError::structured(
                    "PIPELINE_SPAWN",
                    format!(
                        "{caller}: spawn stage[{index}] {} failed: {error}",
                        stage.argv[0]
                    ),
                )
            })?;
            Ok(child)
        })();

        match configured {
            Ok(child) => {
                let pid = child.id();
                children.push(PipelineChildRuntime {
                    child,
                    pid,
                    started: Instant::now(),
                    status: None,
                    duration_ms: 0,
                });
            }
            Err(error) => {
                reap_pipeline_after_setup_failure(&mut children);
                let partial_stages = pipeline_started_stage_values(stages, &children);
                return Err(match error {
                    MixError::Structured(mut info) => {
                        info.details = partial_stages;
                        MixError::Structured(info)
                    }
                    other => other,
                });
            }
        }
    }

    let stdin_handle = match &stages[0].opts.stdin {
        RunArgvStdin::Data(data) => {
            let data = data.clone();
            let mut writer = stdin_data_writer.expect("pipeline stdin pipe was pre-created");
            Some(std::thread::spawn(move || -> std::io::Result<()> {
                writer.write_all(&data)
            }))
        }
        RunArgvStdin::Null | RunArgvStdin::File(_) => None,
    };

    let final_stdout_pipe: Option<Box<dyn std::io::Read + Send>> = final_stdout_reader
        .take()
        .map(|reader| Box::new(reader) as Box<dyn std::io::Read + Send>);
    let cap = opts.max_output;
    let stdout_task = final_stdout_pipe.map(|pipe| spawn_capped_drain(pipe, cap, None));

    let mut stderr_tasks = Vec::with_capacity(stages.len());
    for (index, stage) in stages.iter().enumerate() {
        let capture = matches!(stage.opts.stderr, RunArgvStderr::Capture);
        let task = if capture {
            let pipe = stderr_capture_readers[index]
                .take()
                .expect("stderr capture reader was pre-created");
            Some(spawn_capped_drain(Box::new(pipe), cap, None))
        } else {
            None
        };
        stderr_tasks.push(task);
    }

    let poll_interval = Duration::from_millis(50);
    let mut timed_out = false;
    let mut interrupted = false;
    let mut lifecycle_error: Option<MixError> = None;

    loop {
        let mut all_done = true;
        for (index, runtime) in children.iter_mut().enumerate() {
            if runtime.status.is_some() {
                continue;
            }
            all_done = false;
            match runtime.child.try_wait() {
                Ok(Some(status)) => {
                    runtime.status = Some(status);
                    runtime.duration_ms = runtime.started.elapsed().as_millis();
                }
                Ok(None) => {}
                Err(error) if error.kind() == std::io::ErrorKind::Interrupted => {}
                Err(error) => {
                    lifecycle_error = Some(MixError::structured(
                        "PIPELINE_INTERNAL",
                        format!("{caller}: try_wait stage[{index}] failed: {error}"),
                    ));
                    break;
                }
            }
        }
        if lifecycle_error.is_some() {
            break;
        }
        if all_done || children.iter().all(|runtime| runtime.status.is_some()) {
            break;
        }
        if crate::interrupt::is_interrupted() {
            interrupted = true;
            break;
        }
        if let Some(timeout) = timeout
            && start.elapsed() >= timeout
        {
            timed_out = true;
            break;
        }
        std::thread::sleep(poll_interval);
    }

    if interrupted {
        #[cfg(unix)]
        signal_pipeline_groups(&children, libc::SIGTERM);
        #[cfg(not(unix))]
        for runtime in &mut children {
            let _ = runtime.child.kill();
        }
        let grace_deadline = Instant::now() + Duration::from_secs(2);
        while Instant::now() < grace_deadline {
            let mut all_done = true;
            for runtime in &mut children {
                if runtime.status.is_none() {
                    match runtime.child.try_wait() {
                        Ok(Some(status)) => {
                            runtime.status = Some(status);
                            runtime.duration_ms = runtime.started.elapsed().as_millis();
                        }
                        Ok(None) | Err(_) => all_done = false,
                    }
                }
            }
            if all_done {
                break;
            }
            std::thread::sleep(poll_interval);
        }
        #[cfg(unix)]
        signal_pipeline_groups(&children, libc::SIGKILL);
    } else if timed_out || lifecycle_error.is_some() {
        #[cfg(unix)]
        signal_pipeline_groups(&children, libc::SIGKILL);
        #[cfg(not(unix))]
        for runtime in &mut children {
            let _ = runtime.child.kill();
        }
    }

    for (index, runtime) in children.iter_mut().enumerate() {
        if runtime.status.is_none() {
            match runtime.child.wait() {
                Ok(status) => {
                    runtime.status = Some(status);
                    runtime.duration_ms = runtime.started.elapsed().as_millis();
                }
                Err(error) if lifecycle_error.is_none() => {
                    lifecycle_error = Some(MixError::structured(
                        "PIPELINE_INTERNAL",
                        format!("{caller}: wait stage[{index}] failed: {error}"),
                    ));
                }
                Err(_) => {}
            }
        }
    }

    if !timed_out
        && !interrupted
        && let Some(timeout) = timeout
    {
        // The stdin writer belongs in this wait for the same reason the drains
        // do: a DESCENDANT in stage[0]'s group can hold the stdin read end
        // without reading it, so waiting for completion would block on a full
        // pipe after every direct stage has exited. Killing the groups on
        // expiry normally closes those read ends; an escaped holder is handled
        // by the bounded abandonment path below.
        while stdout_task.as_ref().is_some_and(|task| !task.is_done())
            || stderr_tasks
                .iter()
                .any(|task| task.as_ref().is_some_and(|task| !task.is_done()))
            || stdin_handle
                .as_ref()
                .is_some_and(|handle| !handle.is_finished())
        {
            if start.elapsed() >= timeout {
                #[cfg(unix)]
                signal_pipeline_groups(&children, libc::SIGKILL);
                timed_out = true;
                break;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
    }

    // Group kill closes ordinary descendants' descriptors. A process that
    // escaped with setsid can still retain them, so give drains a short final
    // opportunity and then detach unfinished workers. The result's truncation
    // flags record that EOF was abandoned.
    if timed_out || interrupted {
        let drain_deadline = Instant::now() + Duration::from_millis(100);
        while (stdout_task.as_ref().is_some_and(|task| !task.is_done())
            || stderr_tasks
                .iter()
                .any(|task| task.as_ref().is_some_and(|task| !task.is_done()))
            || stdin_handle
                .as_ref()
                .is_some_and(|handle| !handle.is_finished()))
            && Instant::now() < drain_deadline
        {
            std::thread::sleep(Duration::from_millis(5));
        }
    }

    if let Some(handle) = stdin_handle {
        if !handle.is_finished() && (timed_out || interrupted) {
            drop(handle);
        } else {
            match handle.join() {
                Ok(Ok(())) => {}
                Ok(Err(error)) if error.kind() == std::io::ErrorKind::BrokenPipe => {}
                Ok(Err(error)) => {
                    lifecycle_error.get_or_insert_with(|| {
                        MixError::structured(
                            "PIPELINE_IO",
                            format!("{caller}: writing pipeline stdin failed: {error}"),
                        )
                    });
                }
                Err(_) => {
                    lifecycle_error.get_or_insert_with(|| {
                        MixError::structured(
                            "PIPELINE_INTERNAL",
                            format!("{caller}: pipeline stdin writer thread panicked"),
                        )
                    });
                }
            }
        }
    }

    let (stdout, stdout_truncated) = match stdout_task {
        Some(task) => collect_drain(task, caller, "final stdout", timed_out || interrupted)
            .map_err(pipeline_process_error)?,
        None => (Vec::new(), false),
    };

    let mut stage_stderr = Vec::with_capacity(stages.len());
    for (index, task) in stderr_tasks.into_iter().enumerate() {
        let result = match task {
            Some(task) => collect_drain(
                task,
                caller,
                &format!("stage[{index}] stderr"),
                timed_out || interrupted,
            )
            .map_err(pipeline_process_error)?,
            None => (Vec::new(), false),
        };
        stage_stderr.push(result);
    }

    if let Some(error) = lifecycle_error {
        return Err(error);
    }

    let mut raw_stages = Vec::with_capacity(stages.len());
    for (runtime, (stderr, stderr_truncated)) in children.into_iter().zip(stage_stderr) {
        let status = runtime
            .status
            .expect("successful pipeline lifecycle always reaps every stage");
        let natural_code = status.code();
        #[cfg(unix)]
        let signal = {
            use std::os::unix::process::ExitStatusExt;
            status.signal()
        };
        #[cfg(not(unix))]
        let signal = None;
        raw_stages.push(PipelineRawStageOutcome {
            natural_code,
            signal,
            duration_ms: runtime.duration_ms,
            stderr,
            stderr_truncated,
        });
    }

    Ok(PipelineProcessOutcome {
        stages: raw_stages,
        stdout,
        timed_out,
        interrupted,
        duration_ms: start.elapsed().as_millis(),
        stdout_truncated,
    })
}

fn pipeline_result_map(
    stages: &[PipelineStage],
    outcome: PipelineProcessOutcome,
    allow_signal: bool,
) -> Value {
    let stage_count = outcome.stages.len();
    let mut stage_ok = vec![false; stage_count];
    let mut accepted_signal = vec![false; stage_count];
    let mut downstream_ok = true;
    for index in (0..stage_count).rev() {
        let raw = &outcome.stages[index];
        #[cfg(unix)]
        let accept = allow_signal
            && index + 1 < stage_count
            && raw.signal == Some(libc::SIGPIPE)
            && downstream_ok;
        #[cfg(not(unix))]
        let accept = false;
        accepted_signal[index] = accept;
        stage_ok[index] = raw.natural_code == Some(0) || accept;
        downstream_ok = stage_ok[index] && downstream_ok;
    }

    let mut aggregate_stderr = Vec::new();
    let mut stderr_truncated = false;
    let mut utf8_lossy = std::str::from_utf8(&outcome.stdout).is_err();
    let mut stage_values = Vec::with_capacity(stage_count);
    for (index, (stage, raw)) in stages.iter().zip(outcome.stages.iter()).enumerate() {
        aggregate_stderr.extend_from_slice(&raw.stderr);
        stderr_truncated |= raw.stderr_truncated;
        let stage_utf8_lossy = std::str::from_utf8(&raw.stderr).is_err();
        utf8_lossy |= stage_utf8_lossy;

        let mut map = indexmap::IndexMap::new();
        map.insert("index".to_string(), Value::Number(index as f64));
        map.insert(
            "argv".to_string(),
            Value::list(stage.argv.iter().cloned().map(Value::String).collect()),
        );
        map.insert("ok".to_string(), Value::Bool(stage_ok[index]));
        map.insert(
            "exit_code".to_string(),
            raw.natural_code
                .map_or(Value::Nil, |code| Value::Number(code as f64)),
        );
        map.insert(
            "signal".to_string(),
            raw.signal
                .map_or(Value::Nil, |signal| Value::Number(signal as f64)),
        );
        map.insert(
            "duration_ms".to_string(),
            Value::Number(raw.duration_ms as f64),
        );
        map.insert(
            "stderr".to_string(),
            Value::String(String::from_utf8_lossy(&raw.stderr).into_owned()),
        );
        map.insert(
            "stderr_truncated".to_string(),
            Value::Bool(raw.stderr_truncated),
        );
        map.insert("utf8_lossy".to_string(), Value::Bool(stage_utf8_lossy));
        map.insert(
            "accepted_signal".to_string(),
            Value::Bool(accepted_signal[index]),
        );
        stage_values.push(Value::map(map));
    }

    let final_stage = outcome.stages.last().expect("pipeline is non-empty");
    let ok =
        !outcome.timed_out && !outcome.interrupted && stage_ok.iter().all(|stage_ok| *stage_ok);
    let mut map = indexmap::IndexMap::new();
    map.insert("ok".to_string(), Value::Bool(ok));
    map.insert(
        "exit_code".to_string(),
        final_stage
            .natural_code
            .map_or(Value::Nil, |code| Value::Number(code as f64)),
    );
    map.insert(
        "stdout".to_string(),
        Value::String(String::from_utf8_lossy(&outcome.stdout).into_owned()),
    );
    map.insert(
        "stderr".to_string(),
        Value::String(String::from_utf8_lossy(&aggregate_stderr).into_owned()),
    );
    map.insert("timed_out".to_string(), Value::Bool(outcome.timed_out));
    map.insert("interrupted".to_string(), Value::Bool(outcome.interrupted));
    map.insert(
        "signal".to_string(),
        final_stage
            .signal
            .map_or(Value::Nil, |signal| Value::Number(signal as f64)),
    );
    map.insert(
        "duration_ms".to_string(),
        Value::Number(outcome.duration_ms as f64),
    );
    map.insert(
        "stdout_truncated".to_string(),
        Value::Bool(outcome.stdout_truncated),
    );
    map.insert(
        "stderr_truncated".to_string(),
        Value::Bool(stderr_truncated),
    );
    map.insert("utf8_lossy".to_string(), Value::Bool(utf8_lossy));
    map.insert("error_code".to_string(), Value::Nil);
    map.insert("error".to_string(), Value::Nil);
    map.insert("stages".to_string(), Value::list(stage_values));
    Value::map(map)
}

fn run_process(spec: &ProcSpec<'_>) -> MixResult<ProcOutcome> {
    use std::io::Write;
    use std::process::{Command, Stdio};
    use std::time::{Duration, Instant};

    let ProcSpec {
        argv,
        stdin,
        stdout,
        stderr,
        timeout_ms,
        caller,
        cwd,
        env,
        clear_env,
        max_output,
        stream,
    } = spec;
    let (stdin, stdout, stderr, timeout_ms, caller, stream) =
        (*stdin, *stdout, *stderr, *timeout_ms, *caller, *stream);
    let start = Instant::now();
    let timeout = (timeout_ms != 0).then(|| Duration::from_millis(timeout_ms));
    let deadline = timeout.map(|timeout| start + timeout);

    let mut cmd = Command::new(&argv[0]);
    cmd.args(&argv[1..]);

    let stdin_data = match stdin {
        ProcStdin::Null => {
            cmd.stdin(Stdio::null());
            None
        }
        ProcStdin::Data(data) => {
            cmd.stdin(Stdio::piped());
            Some(data)
        }
        ProcStdin::File(path) => {
            cmd.stdin(Stdio::from(open_process_input(caller, path, deadline)?));
            None
        }
    };

    // Non-append file routes are opened without `O_TRUNC` and truncated only
    // once every route has opened, so a later route's failure cannot destroy an
    // earlier route's file before we report `PROCESS_STDIO` and decline to run.
    let mut pending_truncate: Vec<(&'static str, &RunArgvFile, std::fs::File)> = Vec::new();
    let mut merged_stdout_reader = None;
    let capture_stdout = matches!(stdout, ProcOutput::Capture);
    let capture_stderr = matches!(stderr, ProcStderr::Capture);
    if matches!(stderr, ProcStderr::Stdout) {
        match stdout {
            ProcOutput::Capture => {
                let (reader, writer) = process_capture_pipe(caller)?;
                let stderr_writer = writer.try_clone().map_err(|error| {
                    MixError::structured(
                        "PROCESS_STDIO",
                        format!("{caller}: cloning merged stdout pipe failed: {error}"),
                    )
                })?;
                cmd.stdout(Stdio::from(writer));
                cmd.stderr(Stdio::from(stderr_writer));
                merged_stdout_reader = Some(reader);
            }
            ProcOutput::Inherit => {
                cmd.stdout(Stdio::inherit());
                #[cfg(unix)]
                cmd.stderr(Stdio::from(inherited_stdout_handle(caller)?));
                #[cfg(not(unix))]
                cmd.stderr(Stdio::inherit());
            }
            ProcOutput::Null => {
                cmd.stdout(Stdio::null());
                cmd.stderr(Stdio::null());
            }
            ProcOutput::File(route) => {
                let file = open_process_output(caller, "stdout", route, deadline)?;
                let stderr_file = clone_process_stdio(caller, "stdout", &route.path, &file)?;
                pending_truncate.push((
                    "stdout",
                    route,
                    clone_process_stdio(caller, "stdout", &route.path, &file)?,
                ));
                cmd.stdout(Stdio::from(file));
                cmd.stderr(Stdio::from(stderr_file));
            }
        }
    } else {
        match stdout {
            ProcOutput::Capture => {
                cmd.stdout(Stdio::piped());
            }
            ProcOutput::Inherit => {
                cmd.stdout(Stdio::inherit());
            }
            ProcOutput::Null => {
                cmd.stdout(Stdio::null());
            }
            ProcOutput::File(route) => {
                let file = open_process_output(caller, "stdout", route, deadline)?;
                pending_truncate.push((
                    "stdout",
                    route,
                    clone_process_stdio(caller, "stdout", &route.path, &file)?,
                ));
                cmd.stdout(Stdio::from(file));
            }
        }
        match stderr {
            ProcStderr::Capture => {
                cmd.stderr(Stdio::piped());
            }
            ProcStderr::Inherit => {
                cmd.stderr(Stdio::inherit());
            }
            ProcStderr::Null => {
                cmd.stderr(Stdio::null());
            }
            ProcStderr::File(route) => {
                let file = open_process_output(caller, "stderr", route, deadline)?;
                pending_truncate.push((
                    "stderr",
                    route,
                    clone_process_stdio(caller, "stderr", &route.path, &file)?,
                ));
                cmd.stderr(Stdio::from(file));
            }
            ProcStderr::Stdout => unreachable!("stderr merge handled above"),
        }
    }
    for (stream, route, file) in pending_truncate {
        truncate_process_output(caller, stream, route, &file)?;
    }
    if *clear_env {
        cmd.env_clear();
    }
    for (k, v) in env.iter() {
        cmd.env(k, v);
    }
    if let Some(dir) = cwd {
        cmd.current_dir(dir);
    }

    // Put the child in its own process group so the kill path can
    // reach every descendant via `kill(-pgid, sig)`. Without this, a
    // child that forks helpers (the ssh client is the canonical case)
    // leaves orphans holding our stdout/stderr pipe FDs open, which
    // blocks the drain threads' `read_to_end` until the descendants
    // exit naturally — defeating the timeout's wall-clock bound.
    //
    // On Linux, also arm `PR_SET_PDEATHSIG` so the kernel SIGKILLs the
    // direct child if *we* die before the kill path runs. This is a
    // backstop against the leak class where a child traps SIGTERM and
    // outlives an aborted parent: the kill loop below assumes the
    // parent stays alive long enough to escalate to SIGKILL, but a
    // panic, OOM, or external SIGKILL on the parent leaves a
    // TERM-ignoring child reparented to init with no kill in flight.
    //
    // Two scope notes on what PDEATHSIG does *not* fix:
    //
    //   1. Linux delivers PDEATHSIG when the *cloning thread* exits,
    //      not the parent process. Today Mix evaluates a single script
    //      on its calling thread, so "thread exit" == "process exit"
    //      for our purposes. If a future evaluator runs `ssh_run` on
    //      a worker thread that can be joined while the script keeps
    //      executing, the SIGKILL will land on a still-needed child.
    //      Revisit this comment if Mix's threading model changes.
    //   2. PDEATHSIG only protects the direct child. Grandchildren
    //      spawned by the ssh client (or any descendant that detaches
    //      via setsid + double-fork) are unaffected — see the
    //      "Detached descendants" item in the docstring above.
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        // SAFETY: `setpgid`, `prctl`, `getppid`, and `_exit` are thin
        // syscall wrappers safe to call in the post-fork pre-exec
        // window — no locks taken, no allocations, no global state
        // mutated. Returning the OS error via `last_os_error()` reads
        // `errno`, also fork-safe. `Command::spawn` surfaces a non-Ok
        // return as a normal spawn failure.
        // `prctl(PR_SET_PDEATHSIG)` is Linux-only and best-effort —
        // failure on an unsupported kernel (EINVAL) doesn't compromise
        // the PG-kill path, so we ignore its return.
        let parent_pid = std::process::id() as libc::pid_t;
        unsafe {
            cmd.pre_exec(move || {
                if libc::setpgid(0, 0) == -1 {
                    return Err(std::io::Error::last_os_error());
                }
                #[cfg(target_os = "linux")]
                {
                    libc::prctl(
                        libc::PR_SET_PDEATHSIG,
                        libc::SIGKILL as libc::c_ulong,
                        0 as libc::c_ulong,
                        0 as libc::c_ulong,
                        0 as libc::c_ulong,
                    );
                    // PDEATHSIG-arm race: if the parent died between
                    // fork and the prctl above, we're already orphaned
                    // to init and the death signal will never deliver.
                    // Compare getppid() to the captured parent pid; on
                    // mismatch, self-exit so the orphan-busy-loop class
                    // cannot survive any parent-death timing.
                    if libc::getppid() != parent_pid {
                        libc::_exit(0);
                    }
                }
                Ok(())
            });
        }
    }

    let mut child = cmd.spawn().map_err(|e| {
        MixError::structured(
            "PROCESS_SPAWN",
            format!("{caller}: spawn {} failed: {}", argv[0], e),
        )
    })?;
    // `Command` keeps custom Stdio file handles so the same command can be
    // spawned again. Drop it now: otherwise stderr:"stdout" retains a parent
    // copy of the merged pipe's writer and its drain can never observe EOF.
    drop(cmd);

    // Capture pid before we hand the child stdin/stdout off to threads,
    // so the kill path can address it directly and we keep the borrow
    // of `child` available for try_wait/wait/kill. With the spawn-time
    // `setpgid(0, 0)` above, this pid is also the PGID — `kill(-pid,
    // sig)` reaches every process in the group.
    #[cfg(unix)]
    let child_pid = child.id() as i32;

    // Spawn stdin writer concurrently with the drain threads below — a
    // child that emits >64K stdout while we push >64K stdin would dead-
    // lock if the write happened inline.
    let stdin_handle = if let Some(mut writer) = child.stdin.take() {
        let data: Vec<u8> = stdin_data.unwrap_or(&[]).to_vec();
        Some(std::thread::spawn(move || -> std::io::Result<()> {
            writer.write_all(&data)?;
            // `writer` drops here → pipe closes → remote sees EOF.
            Ok(())
        }))
    } else {
        None
    };

    // Drain stdout/stderr in dedicated threads. We can't use
    // `wait_with_output` here because we need to be able to interrupt
    // the wait based on the timeout/interrupt flag — so we own the
    // pipes ourselves and reap the child explicitly.
    let stdout_pipe: Option<Box<dyn std::io::Read + Send>> = if capture_stdout {
        match merged_stdout_reader {
            Some(reader) => Some(Box::new(reader)),
            None => {
                Some(Box::new(child.stdout.take().expect(
                    "captured stdout was configured as Stdio::piped above",
                )))
            }
        }
    } else {
        None
    };
    let stderr_pipe: Option<Box<dyn std::io::Read + Send>> = if capture_stderr {
        Some(Box::new(child.stderr.take().expect(
            "captured stderr was configured as Stdio::piped above",
        )))
    } else {
        None
    };
    let cap = *max_output;
    let stdout_task =
        stdout_pipe.map(|pipe| spawn_capped_drain(pipe, cap, stream.then_some(LiveStream::Stdout)));
    let stderr_task =
        stderr_pipe.map(|pipe| spawn_capped_drain(pipe, cap, stream.then_some(LiveStream::Stderr)));

    // The deadline starts before route opening. Opening a FIFO, device, or
    // remote filesystem must not consume an unbounded pre-command interval.
    let poll_interval = Duration::from_millis(50);

    let mut interrupted = false;
    let mut timed_out = false;
    let mut natural_exit: Option<std::process::ExitStatus> = None;
    let mut try_wait_failed: Option<std::io::Error> = None;

    loop {
        match child.try_wait() {
            Ok(Some(status)) => {
                natural_exit = Some(status);
                break;
            }
            Ok(None) => {}
            // EINTR is expected when SIGINT fires during the syscall —
            // it does NOT mean the child is dead, just that the wait
            // was interrupted. Retry on the next poll, where the
            // interrupt-flag check below will pick up the SIGINT.
            Err(e) if e.kind() == std::io::ErrorKind::Interrupted => {}
            // Other errors are unrecoverable. Don't early-return — that
            // would leak the still-running child and orphan the drain
            // threads. Stash the error, fall through to the kill path,
            // then surface it after the cleanup completes.
            Err(e) => {
                try_wait_failed = Some(e);
                break;
            }
        }
        // Interrupt wins over timeout in the tie-breaker — the user's
        // explicit Ctrl-C should be reported as the cause, not the
        // wall-clock deadline that happened to expire on the same poll.
        if crate::interrupt::is_interrupted() {
            interrupted = true;
            break;
        }
        if let Some(t) = timeout
            && start.elapsed() >= t
        {
            timed_out = true;
            break;
        }
        std::thread::sleep(poll_interval);
    }

    // Kill if we exited the loop without a natural exit. The escalation
    // differs by cause (see function docstring): timeout = SIGKILL to
    // the process group immediately; interrupt = SIGTERM-grace-SIGKILL
    // to the process group; try_wait_failed = SIGKILL to the process
    // group (defensive, we can't trust the child's state).
    if natural_exit.is_none() {
        #[cfg(unix)]
        {
            // Negative pid → kill the whole process group. We placed the
            // child in its own group via `setpgid(0, 0)` at spawn, so
            // `child_pid` is also the PGID. This is the load-bearing
            // change vs. signalling only the direct child: it reaches
            // descendants (ssh helpers / orphaned children) that would
            // otherwise keep our stdout/stderr pipe FDs open and block
            // the drain threads past the timeout.
            //
            // SAFETY: `libc::kill` is async-signal-safe; signalling a
            // stale pgid is not catastrophic (kernel returns ESRCH
            // which we ignore).
            let pgid = -child_pid;
            if timed_out {
                // Hard local deadline — no grace, no cooperation.
                unsafe {
                    libc::kill(pgid, libc::SIGKILL);
                }
            } else if try_wait_failed.is_some() {
                // Defensive: state is unknown, escalate immediately.
                unsafe {
                    libc::kill(pgid, libc::SIGKILL);
                }
            } else {
                // Interrupt path — cooperative SIGTERM, then SIGKILL
                // if the group hasn't exited within the grace window.
                unsafe {
                    libc::kill(pgid, libc::SIGTERM);
                }
                let term_deadline = Instant::now() + Duration::from_secs(2);
                let mut term_grace_failed = false;
                loop {
                    match child.try_wait() {
                        Ok(Some(status)) => {
                            natural_exit = Some(status);
                            break;
                        }
                        Ok(None) => {}
                        // EINTR: another SIGINT arrived during the wait —
                        // retry the poll, don't give up on the grace period.
                        Err(e) if e.kind() == std::io::ErrorKind::Interrupted => {}
                        // Real failure: we can no longer trust try_wait.
                        // Stop grace polling and fall through to SIGKILL.
                        Err(_) => {
                            term_grace_failed = true;
                            break;
                        }
                    }
                    if Instant::now() >= term_deadline || term_grace_failed {
                        unsafe {
                            libc::kill(pgid, libc::SIGKILL);
                        }
                        break;
                    }
                    std::thread::sleep(poll_interval);
                }
                if natural_exit.is_none() && term_grace_failed {
                    unsafe {
                        libc::kill(pgid, libc::SIGKILL);
                    }
                }
            }
        }
        #[cfg(not(unix))]
        {
            // No process-group concept on Windows; signal the direct
            // child only. Descendant cleanup is the OS's problem.
            let _ = child.kill();
        }
        if natural_exit.is_none() {
            // Final blocking wait so the OS releases the direct child's
            // PID slot. The kill above guarantees we won't block forever
            // on the leader; group-mates exit independently.
            natural_exit = child.wait().ok();
        }
    }

    // The child is reaped, but capture completion can still block
    // past the deadline when a DESCENDANT (same process group) inherited
    // our pipe write ends and outlives the leader (`sh -c "sleep 9 &"`).
    // Keep enforcing the deadline while the drains finish; on expiry,
    // SIGKILL the group (closing those write ends) and report timed_out.
    if !timed_out
        && !interrupted
        && let Some(t) = timeout
    {
        // The stdin writer is watched here too: a descendant holding the stdin
        // read end without reading it blocks the writer on a full pipe, and the
        // completion wait. The group SIGKILL normally closes that read end.
        while stdout_task.as_ref().is_some_and(|task| !task.is_done())
            || stderr_task.as_ref().is_some_and(|task| !task.is_done())
            || stdin_handle
                .as_ref()
                .is_some_and(|handle| !handle.is_finished())
        {
            if start.elapsed() >= t {
                #[cfg(unix)]
                unsafe {
                    libc::kill(-child_pid, libc::SIGKILL);
                }
                timed_out = true;
                break;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
    }

    if timed_out || interrupted {
        let drain_deadline = Instant::now() + Duration::from_millis(100);
        while (stdout_task.as_ref().is_some_and(|task| !task.is_done())
            || stderr_task.as_ref().is_some_and(|task| !task.is_done())
            || stdin_handle
                .as_ref()
                .is_some_and(|handle| !handle.is_finished()))
            && Instant::now() < drain_deadline
        {
            std::thread::sleep(Duration::from_millis(5));
        }
    }

    // Now that the child is reaped, drain the writer threads.
    if let Some(h) = stdin_handle {
        if !h.is_finished() && (timed_out || interrupted) {
            drop(h);
        } else {
            match h.join() {
                Ok(Ok(())) => {}
                // The child closed stdin first (e.g. `head -c 1` or it was
                // killed mid-write). Normal end-of-stream — swallow.
                Ok(Err(e)) if e.kind() == std::io::ErrorKind::BrokenPipe => {}
                Ok(Err(e)) => {
                    return Err(MixError::structured(
                        "PROCESS_IO",
                        format!("{caller}: writing stdin failed: {}", e),
                    ));
                }
                Err(_) => {
                    return Err(MixError::structured(
                        "PROCESS_INTERNAL",
                        format!("{caller}: stdin writer thread panicked"),
                    ));
                }
            }
        }
    }
    let (stdout, stdout_truncated) = match stdout_task {
        Some(task) => collect_drain(task, caller, "stdout", timed_out || interrupted)?,
        None => (Vec::new(), false),
    };
    let (stderr, stderr_truncated) = match stderr_task {
        Some(task) => collect_drain(task, caller, "stderr", timed_out || interrupted)?,
        None => (Vec::new(), false),
    };

    // After cleanup, surface a deferred try_wait error if one was
    // captured during the polling loop. The child has been reaped, so
    // it's safe to abandon the outcome and return an error here.
    if let Some(e) = try_wait_failed {
        return Err(MixError::structured(
            "PROCESS_INTERNAL",
            format!("{caller}: try_wait failed: {}", e),
        ));
    }

    // Undecoded facts from the reaped status, reported regardless of
    // cause (a timed-out child shows signal 9 from our own SIGKILL —
    // truthful, and `timed_out` carries the why).
    let mut signal: Option<i32> = None;
    let mut natural_code: Option<i32> = None;
    if let Some(status) = &natural_exit {
        match status.code() {
            Some(c) => natural_code = Some(c),
            None => {
                #[cfg(unix)]
                {
                    use std::os::unix::process::ExitStatusExt;
                    signal = status.signal();
                }
            }
        }
    }
    // Legacy sentinel encoding (run/run_rc/ssh_* consumers + tests).
    let exit_code = if interrupted {
        -2
    } else if timed_out {
        -1
    } else if natural_exit.is_some() {
        match (natural_code, signal) {
            (Some(c), _) => c,
            (None, Some(s)) => 128 + s,
            (None, None) => -3,
        }
    } else {
        -3
    };

    Ok(ProcOutcome {
        stdout,
        stderr,
        exit_code,
        timed_out,
        interrupted,
        signal,
        natural_code,
        stdout_truncated,
        stderr_truncated,
        duration_ms: start.elapsed().as_millis(),
    })
}

/// Compatibility adapter — the pre-0.29 engine signature used by
/// `run`, `run_rc`, and the ssh family (and pinned by their tests).
/// Whole-second timeouts, string stdin, inherited env/cwd, unbounded
/// output — exactly the old behavior, now routed through
/// [`run_process`].
fn run_with_timeout(
    argv: &[String],
    stdin: Option<&str>,
    timeout_s: u64,
    caller: &str,
) -> MixResult<SshOutcome> {
    let outcome = run_process(&ProcSpec {
        argv,
        stdin: stdin.map_or(ProcStdin::Null, |data| ProcStdin::Data(data.as_bytes())),
        stdout: ProcOutput::Capture,
        stderr: ProcStderr::Capture,
        timeout_ms: timeout_s.saturating_mul(1000),
        caller,
        cwd: None,
        env: &[],
        clear_env: false,
        max_output: None,
        stream: false,
    })
    .map_err(|e| match e {
        // Legacy compatibility: pre-0.29 the engine raised plain
        // RuntimeErrors for lifecycle failures; run/run_rc/ssh_*
        // consumers (and their $err.code view) must not observe the
        // new PROCESS_* codes through this adapter.
        MixError::Structured(info)
            if matches!(
                info.code.as_str(),
                "PROCESS_SPAWN" | "PROCESS_IO" | "PROCESS_INTERNAL"
            ) =>
        {
            MixError::RuntimeError {
                span: info.span,
                msg: info.message,
            }
        }
        other => other,
    })?;
    Ok(SshOutcome {
        stdout: outcome.stdout,
        stderr: outcome.stderr,
        exit_code: outcome.exit_code,
        timed_out: outcome.timed_out,
        interrupted: outcome.interrupted,
    })
}

fn builtin_ssh_run(args: Vec<Value>) -> MixResult<Option<Value>> {
    if !(2..=3).contains(&args.len()) {
        return Err(MixError::RuntimeError {
            span: None,
            msg: format!(
                "ssh_run: expected 2 or 3 args (host, command, [opts]), got {}",
                args.len()
            ),
        });
    }
    let host = match &args[0] {
        Value::String(s) => s.clone(),
        _ => {
            return Err(MixError::RuntimeError {
                span: None,
                msg: "ssh_run: host must be a string".into(),
            });
        }
    };
    if host.is_empty() {
        return Err(MixError::RuntimeError {
            span: None,
            msg: "ssh_run: host must not be empty".into(),
        });
    }
    if host.starts_with('-') {
        // A leading dash is never a legitimate hostname/alias and would
        // let ssh parse the host as an option (option injection → local
        // RCE via -oProxyCommand). `build_ssh_argv` also emits `--`
        // before the host; this rejects the value loudly rather than
        // silently running `ssh -V …`.
        return Err(MixError::RuntimeError {
            span: None,
            msg: format!("ssh_run: host must not begin with '-' (got {:?})", host),
        });
    }
    reject_nul("host", &host)?;
    let opts = parse_ssh_opts(args.get(2))?;

    // SECURE ENV PATH (default): env values travel inside a stdin driver,
    // never in argv — see ENV_TRANSPORT_VALUES. The driver owns stdin, so
    // a caller `stdin` opt conflicts loudly (explicit `env_transport:
    // "argv"` restores the legacy combinable-but-ps-visible behavior).
    if !opts.env.is_empty() && opts.env_transport != "argv" {
        if opts.stdin.is_some() {
            return Err(MixError::RuntimeError {
                span: None,
                msg: "ssh_run: `env` and `stdin` conflict — the secure env driver owns \
                      stdin; pass env_transport: \"argv\" to combine them (env values \
                      then appear in ps argv on both ends)"
                    .into(),
            });
        }
        validate_env_keys("ssh_run", &opts.env)?;
        // The command with cwd handling but WITHOUT the env prefix: reuse
        // build_remote_command on an env-cleared copy of the opts.
        let mut cmd_opts = opts.clone();
        cmd_opts.env.clear();
        let base_cmd = build_remote_command(&args[1], &cmd_opts)?;
        let driver = build_env_driver(&opts.env_transport, &opts.env, &base_cmd)?;
        let interpreter = if opts.env_transport == "mix" {
            REMOTE_MIX_STDIN_CMD
        } else {
            "sh -s"
        };
        let argv = build_ssh_argv(&host, interpreter, &opts);
        let started = std::time::Instant::now();
        let outcome = run_with_timeout(&argv, Some(&driver), opts.timeout, "ssh_run")?;
        return Ok(Some(ssh_result_map(&host, outcome, started.elapsed())));
    }

    let remote_cmd = build_remote_command(&args[1], &opts)?;
    let argv = build_ssh_argv(&host, &remote_cmd, &opts);
    let started = std::time::Instant::now();
    let outcome = run_with_timeout(&argv, opts.stdin.as_deref(), opts.timeout, "ssh_run")?;
    Ok(Some(ssh_result_map(&host, outcome, started.elapsed())))
}

/// Inspect an `ssh_run` result map and either return its `stdout`
/// (success) or build the `ssh_must` failure-error message
/// (non-success). Factored out so the formatting can be unit-tested
/// without spawning a real ssh subprocess.
fn ssh_must_from_map(map: &indexmap::IndexMap<String, Value>) -> MixResult<Value> {
    let ok = matches!(map.get("ok"), Some(Value::Bool(true)));
    if ok {
        return Ok(map
            .get("stdout")
            .cloned()
            .unwrap_or(Value::String(String::new())));
    }
    let host = match map.get("host") {
        Some(Value::String(s)) => s.as_str(),
        _ => "?",
    };
    let exit_code = match map.get("exit_code") {
        Some(Value::Number(n)) => *n as i64,
        _ => 0,
    };
    let interrupted = matches!(map.get("interrupted"), Some(Value::Bool(true)));
    let timed_out = matches!(map.get("timed_out"), Some(Value::Bool(true)));
    let disposition = if interrupted {
        "interrupted"
    } else if timed_out {
        "timed out"
    } else {
        "failed"
    };
    let stderr_excerpt = match map.get("stderr") {
        Some(Value::String(s)) => {
            let trimmed = s.trim_end();
            if trimmed.len() > 512 {
                // Truncate on a char boundary to avoid splitting a
                // multi-byte UTF-8 sequence.
                let mut end = 512;
                while end > 0 && !trimmed.is_char_boundary(end) {
                    end -= 1;
                }
                format!("{}…", &trimmed[..end])
            } else {
                trimmed.to_string()
            }
        }
        _ => String::new(),
    };
    let msg = if stderr_excerpt.is_empty() {
        format!(
            "ssh_must: {} on {} (exit_code={})",
            disposition, host, exit_code
        )
    } else {
        format!(
            "ssh_must: {} on {} (exit_code={}): {}",
            disposition, host, exit_code, stderr_excerpt
        )
    };
    Err(MixError::RuntimeError { span: None, msg })
}

/// `ssh_must(host, command, opts?) -> string`
///
/// Calls `ssh_run` with the same arguments. On success (`ok == true`)
/// returns the command's stdout as a string. On any non-success
/// outcome — non-zero exit, timeout, interrupt, or signal-killed —
/// throws a `MixError::RuntimeError` whose message includes the host,
/// the disposition, the exit_code, and the first 512 bytes of stderr.
///
/// Use when failure is genuinely fatal and a try/catch around the
/// call would just clutter; for anything finer than "succeed or
/// abort the script", use `ssh_run` directly and inspect the map.
fn builtin_ssh_must(args: Vec<Value>) -> MixResult<Option<Value>> {
    let mut result = builtin_ssh_run(args)?;
    // `&mut` + `mem::take` instead of a move-destructure: `Value`
    // implements `Drop`, so moving the payload out in a pattern is E0509.
    let map = match result.as_mut() {
        Some(Value::Map(m)) => std::mem::take(m),
        // builtin_ssh_run always returns Some(Value::Map(_)); any
        // other shape would be an internal bug worth surfacing.
        _ => {
            return Err(MixError::RuntimeError {
                span: None,
                msg: "ssh_must: ssh_run returned unexpected value shape".into(),
            });
        }
    };
    ssh_must_from_map(&map).map(Some)
}

/// The remote mix binary `ssh_mix` pipes source into. Canonical install
/// path (CLAUDE.md tooling policy) — a full path so it resolves inside
/// the remote shell-dispatch `/bin/sh` even on UsePAM-off nodes where a
/// bare `mix` is not on PATH.
const REMOTE_MIX_STDIN_CMD: &str = "/opt/cosmix/bin/mix -";

/// `ssh_mix(host, source, opts?) -> map`
///
/// Runs Mix `source` on a remote host by shipping it over the ssh
/// **stdin** byte-channel into `/opt/cosmix/bin/mix -`. Because the
/// source travels as stdin and never as an argv word, it bypasses every
/// shell-quoting layer (local shell, ssh, the remote mix classifier) —
/// arbitrary quotes, `$`, backslashes, and newlines survive intact. This
/// is the discoverable, first-class form of the
/// `ssh_run(host, "mix -", {stdin: source})` idiom.
///
/// Returns the same map as `ssh_run` ({ok, stdout, stderr, exit_code,
/// duration_ms, host, timed_out, interrupted, utf8_lossy}). If `opts`
/// carries `decode: "data"` or `decode: "json"` and the run succeeded
/// (`ok`), the trimmed stdout is parsed and added under `value` (via
/// `data_parse` / `json_parse` respectively) — the common "get a
/// structured value back from the remote" case. (`decode`, not `parse`:
/// `parse` is a reserved keyword and cannot be a bare map key.) Every
/// `ssh_run` opt is accepted **except** `stdin` (the source *is* the
/// stdin; passing both is a conflict and errors). `bindings` maps valid
/// Mix identifier names to strict-data-encoded values assigned before
/// the caller source; no name is reserved and the source may rebind.
fn builtin_ssh_mix(args: Vec<Value>) -> MixResult<Option<Value>> {
    if !(2..=3).contains(&args.len()) {
        return Err(MixError::RuntimeError {
            span: None,
            msg: format!(
                "ssh_mix: expected 2 or 3 args (host, source, [opts]), got {}",
                args.len()
            ),
        });
    }
    let host = args[0].clone();
    let source = match &args[1] {
        Value::String(s) => s.clone(),
        _ => {
            return Err(MixError::RuntimeError {
                span: None,
                msg: "ssh_mix: source must be a string".into(),
            });
        }
    };
    // Build the opts map handed to ssh_run: start from the caller's opts
    // (if any), pull out our own `decode` and `bindings` keys (unknown to
    // ssh_run's strict allowlist), reject a caller `stdin` (we own it),
    // then inject the generated prefix plus source as stdin.
    let mut opts_map = match args.get(2) {
        None => indexmap::IndexMap::new(),
        // CoW: shallow copy out of the Rc payload — this local map is
        // edited (decode key removal, stdin injection) before use.
        Some(Value::Map(m)) => (**m).clone(),
        Some(_) => {
            return Err(MixError::RuntimeError {
                span: None,
                msg: "ssh_mix: opts must be a map".into(),
            });
        }
    };
    if opts_map.contains_key("stdin") {
        return Err(MixError::RuntimeError {
            span: None,
            msg: "ssh_mix: `stdin` opt is not allowed — the source argument is the stdin".into(),
        });
    }
    if opts_map.contains_key("env_transport") {
        return Err(MixError::RuntimeError {
            span: None,
            msg: "ssh_mix: `env_transport` is not applicable — ssh_mix always ships env \
                  as hidden `export` lines inside the stdin source"
                .into(),
        });
    }
    // env: translate into `export KEY = "value"` lines PREPENDED to the
    // shipped source — the source travels on stdin, so the values never
    // touch argv on either end (the ssh_run argv path would have embedded
    // them in the remote command string).
    let env_prefix = match opts_map.shift_remove("env") {
        None => String::new(),
        Some(v) => {
            let env = parse_env_opt(&v)?;
            validate_env_keys("ssh_mix", &env)?;
            mix_export_lines(&env)?
        }
    };
    let bindings_prefix = match opts_map.shift_remove("bindings") {
        None => String::new(),
        Some(bindings) => mix_binding_lines(&bindings)?,
    };
    // `Value` implements Drop, so a by-move pattern bind is E0509 — match
    // on a reference and clone the (tiny) mode string.
    let decode_mode = match opts_map.shift_remove("decode") {
        None => None,
        Some(v) => match &v {
            Value::String(s) if s == "data" || s == "json" => Some(s.clone()),
            _ => {
                return Err(MixError::RuntimeError {
                    span: None,
                    msg: format!("ssh_mix: decode must be \"data\" or \"json\", got {:?}", v),
                });
            }
        },
    };
    opts_map.insert(
        "stdin".into(),
        Value::String(format!("{env_prefix}{bindings_prefix}{source}")),
    );

    let mut result = builtin_ssh_run(vec![
        host,
        Value::String(REMOTE_MIX_STDIN_CMD.into()),
        Value::map(opts_map),
    ])?;

    // Optionally decode stdout into `value` on success.
    if let (Some(mode), Some(Value::Map(m))) = (decode_mode, result.as_mut()) {
        // CoW: freshly built by ssh_run above (sole owner) — in-place.
        let m = Rc::make_mut(m);
        let ok = matches!(m.get("ok"), Some(Value::Bool(true)));
        if ok {
            let stdout = match m.get("stdout") {
                Some(Value::String(s)) => s.trim().to_string(),
                _ => String::new(),
            };
            let parsed = if mode == "data" {
                builtin_data_parse(vec![Value::String(stdout)])?
            } else {
                // decode:"json" needs the json feature; without it, error
                // rather than failing to compile the bare crate (the call
                // was previously unguarded — a no-default-features build
                // of cosmix-lib-mix didn't compile at all).
                #[cfg(feature = "json")]
                {
                    builtin_json_parse(vec![Value::String(stdout)])?
                }
                #[cfg(not(feature = "json"))]
                {
                    return Err(MixError::RuntimeError {
                        span: None,
                        msg: "ssh_mix: decode:\"json\" requires the json feature (use decode:\"data\")"
                            .into(),
                    });
                }
            };
            m.insert("value".into(), parsed.unwrap_or(Value::Nil));
        }
    }
    Ok(result)
}

/// run_argv option keys that ssh_exec routes INTO the remote run_argv
/// call (encoded in the driver source). The rest are SSH transport opts.
const SSH_EXEC_RUN_ARGV_KEYS: &[&str] = &[
    "timeout",
    "stdin",
    "cwd",
    "env",
    "clear_env",
    "max_output",
    "stream",
    "stdout",
    "stderr",
];

/// SSH transport option keys ssh_exec accepts and passes to the ssh
/// layer (via ssh_mix). `transport_timeout` becomes the ssh wall-clock
/// deadline; `remote_mix` overrides the remote binary path.
const SSH_EXEC_TRANSPORT_KEYS: &[&str] = &[
    "connect_timeout",
    "transport_timeout",
    "multiplex",
    "batch",
    "strict_host_key",
    "extra_ssh_args",
    "remote_mix",
];

fn validate_ssh_exec_run_argv_opts(opts: &indexmap::IndexMap<String, Value>) -> MixResult<()> {
    parse_run_argv_opts("ssh_exec", Some(&Value::map(opts.clone())))?;

    if matches!(opts.get("stream"), Some(Value::Bool(true))) {
        return Err(opt_invalid(
            "ssh_exec",
            "stream:true is not allowed for remote execution because ssh stdout carries the result envelope",
        ));
    }
    if matches!(opts.get("stdout"), Some(Value::String(value)) if value == "inherit") {
        return Err(opt_invalid(
            "ssh_exec",
            "stdout:\"inherit\" is not allowed for remote execution because it would corrupt the result envelope",
        ));
    }
    if matches!(opts.get("stderr"), Some(Value::String(value)) if value == "inherit") {
        return Err(opt_invalid(
            "ssh_exec",
            "stderr:\"inherit\" is not allowed for remote execution because successful ssh stderr is not part of the result envelope",
        ));
    }
    // There is deliberately no stdin route named "inherit" — locally OR
    // remotely — so a stdin STRING is always data, including the seven bytes
    // `inherit`. Rejecting it here would give the same value two meanings
    // depending on which side of the ssh hop it is on, and would break existing
    // calls that pass it as payload. `nil` keeps its local meaning too: closed
    // stdin, which the driver carries as data like any other value.
    if let Some(stdin) = opts.get("stdin") {
        match stdin {
            Value::String(_) | Value::Map(_) | Value::Nil => {}
            Value::Bytes(_) | Value::Buffer(_) => {
                return Err(opt_invalid(
                    "ssh_exec",
                    "stdin must be a string or routing map for remote execution (binary stdin cannot cross the strict-data driver) — encode it yourself, e.g. base64, and decode remotely",
                ));
            }
            _ => {
                return Err(opt_invalid(
                    "ssh_exec",
                    "stdin must be nil, a string, {file: string}, or {null: true} for remote execution",
                ));
            }
        }
    }
    Ok(())
}

/// `ssh_exec(host, argv, opts?) -> map` (0.30.0, decision D9).
///
/// Direct-argv remote execution with TRUTHFUL argv semantics: OpenSSH
/// takes a remote command STRING, not an argv array, so a plain wrapper
/// can't promise argv-inertness end to end. ssh_exec instead ships a
/// strict-data Mix DRIVER over the ssh stdin channel into the remote
/// `/opt/cosmix/bin/mix -`; the driver reconstructs argv + options as
/// data and invokes the remote `run_argv`, then prints one strict-data
/// envelope. No shell, no `run_stream`, no quoting fallback anywhere.
///
/// Result = the `run_argv` schema plus `host`. Transport/protocol
/// failures return that schema with `exit_code: nil` and an `SSH_*`
/// `error_code`. A remote binary without `run_argv` (pre-0.29) yields
/// `ok:false`, `error_code: "SSH_REMOTE_UNSUPPORTED"` — the requested
/// command is NOT run.
fn builtin_ssh_exec(args: Vec<Value>) -> MixResult<Option<Value>> {
    if !(2..=3).contains(&args.len()) {
        return Err(MixError::structured(
            "TYPE_MISMATCH",
            format!(
                "ssh_exec: expected 2 or 3 args (host, argv, [opts]), got {}",
                args.len()
            ),
        ));
    }
    let host = args[0].clone();
    // argv validated with the SAME rules as run_argv (non-empty list of
    // NUL-free strings) — before any SSH work.
    let argv = parse_run_argv_argv("ssh_exec", &args[1])?;

    // Partition opts: run_argv opts → the driver; transport opts → ssh.
    let mut run_argv_opts = indexmap::IndexMap::new();
    let mut ssh_opts = indexmap::IndexMap::new();
    let mut remote_mix = "/opt/cosmix/bin/mix -".to_string();
    if let Some(v) = args.get(2) {
        let m = match v {
            Value::Nil => &indexmap::IndexMap::new(),
            Value::Map(m) => m.as_ref(),
            other => {
                return Err(opt_invalid(
                    "ssh_exec",
                    format!("options must be a map, got {}", other.type_name()),
                ));
            }
        };
        for (k, val) in m.iter() {
            if SSH_EXEC_RUN_ARGV_KEYS.contains(&k.as_str()) {
                run_argv_opts.insert(k.clone(), val.clone());
            } else if k == "remote_mix" {
                match val {
                    Value::String(s) => remote_mix = format!("{s} -"),
                    other => {
                        return Err(opt_invalid(
                            "ssh_exec",
                            format!("remote_mix must be a string, got {}", other.type_name()),
                        ));
                    }
                }
            } else if k == "transport_timeout" {
                // ssh wall-clock deadline → ssh_mix's `timeout`.
                ssh_opts.insert("timeout".to_string(), val.clone());
            } else if SSH_EXEC_TRANSPORT_KEYS.contains(&k.as_str()) {
                ssh_opts.insert(k.clone(), val.clone());
            } else {
                return Err(opt_invalid(
                    "ssh_exec",
                    format!(
                        "unknown option '{}' (run_argv: {}; transport: {})",
                        sanitize_for_diag(k),
                        SSH_EXEC_RUN_ARGV_KEYS.join(", "),
                        SSH_EXEC_TRANSPORT_KEYS.join(", ")
                    ),
                ));
            }
        }
    }
    // Validate the run_argv opts locally (fail fast, before SSH) so a
    // bad option is a clean local error, not a remote driver failure.
    validate_ssh_exec_run_argv_opts(&run_argv_opts)?;

    // Default transport deadline generous enough to outlast the remote
    // command: remote timeout (default 30) + connect (default 10) + 5.
    // A DISABLED remote timeout (0 = run indefinitely) must disable the
    // transport ceiling too, or the auto default (0+10+5=15s) would
    // silently cap a deliberately-unbounded remote command.
    if !ssh_opts.contains_key("timeout") {
        let remote_t = match run_argv_opts.get("timeout") {
            Some(Value::Number(n)) => *n,
            _ => 30.0,
        };
        let connect_t = match ssh_opts.get("connect_timeout") {
            Some(Value::Number(n)) => *n,
            _ => 10.0,
        };
        let transport_t = if remote_t == 0.0 {
            0.0 // remote command is unbounded → no ssh wall-clock ceiling
        } else {
            // ssh_run's timeout is WHOLE seconds; a fractional remote
            // timeout (run_argv accepts it) must round UP so the
            // transport never fires before the remote command's own
            // deadline (codex 0.30 review, MAJOR).
            (remote_t + connect_t + 5.0).ceil()
        };
        ssh_opts.insert("timeout".to_string(), Value::Number(transport_t));
    }
    // stdin (the driver source) is set below via ssh_run's stdin opt.

    // Build the driver: reconstruct argv + opts from strict-data
    // literals and invoke the remote run_argv. A pre-0.29 remote lacks
    // run_argv, so the call raises "undefined function 'run_argv'" —
    // caught and reported as unsupported (never silently degraded).
    let argv_data =
        Value::list(argv.into_iter().map(Value::String).collect()).to_mix_data_string()?;
    let opts_data = Value::map(run_argv_opts).to_mix_data_string()?;
    let driver = format!(
        "$__argv = data_parse({argv})\n\
         $__opts = data_parse({opts})\n\
         try\n\
         \x20 $__r = run_argv($__argv, $__opts)\n\
         \x20 print(data_encode({{status: \"ok\", result: $__r}}))\n\
         catch $__m\n\
         \x20 if pos(\"undefined function 'run_argv'\", $__m) > 0 then\n\
         \x20\x20 print(data_encode({{status: \"unsupported\"}}))\n\
         \x20 else\n\
         \x20\x20 print(data_encode({{status: \"driver_error\", error: $__m}}))\n\
         \x20 end\n\
         end\n",
        argv = quote_mix_string(&argv_data),
        opts = quote_mix_string(&opts_data),
    );

    // Ship the driver over ssh's stdin channel into the remote mix
    // (`remote_mix`, honoring a custom path). We call ssh_run directly
    // rather than ssh_mix so `remote_mix` is respected and we own the
    // strict-data decode.
    ssh_opts.insert("stdin".to_string(), Value::String(driver));
    let ssh_result = builtin_ssh_run(vec![
        host.clone(),
        Value::String(remote_mix),
        Value::map(ssh_opts),
    ])?;
    let host_str = host.to_mix_string();
    Ok(Some(ssh_exec_decode(&host_str, ssh_result)))
}

/// Turn the ssh_run result of the driver run into the ssh_exec result
/// map (run_argv schema + host). The remote driver prints ONE
/// strict-data envelope to stdout; we parse it here.
fn ssh_exec_decode(host: &str, ssh_result: Option<Value>) -> Value {
    let map = match &ssh_result {
        Some(Value::Map(m)) => m.as_ref(),
        _ => {
            return ssh_exec_error(
                host,
                "SSH_TRANSPORT",
                "ssh_exec: internal transport error",
                0.0,
            );
        }
    };
    // Real transport wall-clock, propagated into every failure result so
    // a 30s timeout doesn't report duration_ms: 0 (codex 0.30 review).
    let elapsed = match map.get("duration_ms") {
        Some(Value::Number(n)) => *n,
        _ => 0.0,
    };
    let transport_ok = matches!(map.get("ok"), Some(Value::Bool(true)));
    if !transport_ok {
        // The ssh layer itself failed (connect refused, host down,
        // deadline). Map to SSH_TIMEOUT / SSH_INTERRUPTED / SSH_TRANSPORT.
        let (code, msg): (&str, String) = if matches!(map.get("timed_out"), Some(Value::Bool(true)))
        {
            (
                "SSH_TIMEOUT",
                "ssh_exec: ssh transport timed out".to_string(),
            )
        } else if matches!(map.get("interrupted"), Some(Value::Bool(true))) {
            ("SSH_INTERRUPTED", "ssh_exec: interrupted".to_string())
        } else {
            let stderr = match map.get("stderr") {
                Some(Value::String(s)) => s.trim().to_string(),
                _ => String::new(),
            };
            (
                "SSH_TRANSPORT",
                format!("ssh_exec: ssh transport failed: {stderr}"),
            )
        };
        return ssh_exec_error(host, code, &msg, elapsed);
    }
    // Parse the driver's strict-data envelope from stdout. `value`
    // (pre-parsed) is honored when present so the unit tests can inject
    // an envelope directly; otherwise parse `stdout`.
    let envelope_val = match map.get("value") {
        Some(v @ Value::Map(_)) => v.clone(),
        _ => {
            let stdout = match map.get("stdout") {
                Some(Value::String(s)) => s.trim().to_string(),
                _ => String::new(),
            };
            match builtin_data_parse(vec![Value::String(stdout)]) {
                Ok(Some(v)) => v,
                _ => Value::Nil,
            }
        }
    };
    let envelope = match &envelope_val {
        Value::Map(e) => e.as_ref(),
        _ => {
            return ssh_exec_error(
                host,
                "SSH_PROTOCOL",
                "ssh_exec: could not decode remote envelope (remote mix output was not strict-data)",
                elapsed,
            );
        }
    };
    let status = match envelope.get("status") {
        Some(Value::String(s)) => s.as_str(),
        _ => "driver_error",
    };
    match status {
        "ok" => match envelope.get("result") {
            // The remote result must actually have the run_argv schema —
            // a `{status:"ok", result:{}}` is a protocol violation, not
            // a valid process_result (codex 0.30 review).
            Some(Value::Map(r)) if is_process_result_shape(r) => {
                let mut out = (**r).clone();
                out.insert("host".to_string(), Value::String(host.to_string()));
                Value::map(out)
            }
            _ => ssh_exec_error(
                host,
                "SSH_PROTOCOL",
                "ssh_exec: remote result is not a run_argv process_result",
                elapsed,
            ),
        },
        "unsupported" => ssh_exec_error(
            host,
            "SSH_REMOTE_UNSUPPORTED",
            &format!(
                "ssh_exec: {host} runs a mix without run_argv (needs >= 0.29) — command NOT run"
            ),
            elapsed,
        ),
        _ => {
            let e = match envelope.get("error") {
                Some(Value::String(s)) => s.clone(),
                _ => "unknown remote driver error".to_string(),
            };
            ssh_exec_error(
                host,
                "SSH_PROTOCOL",
                &format!("ssh_exec: remote driver error: {e}"),
                elapsed,
            )
        }
    }
}

/// Whether a map is a complete, correctly-typed `run_argv`
/// process_result (the ssh_exec protocol contract). Validates ALL
/// thirteen emitted fields and their nullable types — a well-keyed map
/// with `ok:nil`, `stdout:{}`, or missing `interrupted`/`signal`/
/// truncation/`error_code` fields is a protocol violation, not a valid
/// result (codex final review). Mirrors `run_argv_result_map`.
fn is_process_result_shape(m: &indexmap::IndexMap<String, Value>) -> bool {
    let is_bool = |k: &str| matches!(m.get(k), Some(Value::Bool(_)));
    let is_num = |k: &str| matches!(m.get(k), Some(Value::Number(_)));
    let is_str = |k: &str| matches!(m.get(k), Some(Value::String(_)));
    let is_num_or_nil = |k: &str| matches!(m.get(k), Some(Value::Number(_)) | Some(Value::Nil));
    let is_str_or_nil = |k: &str| matches!(m.get(k), Some(Value::String(_)) | Some(Value::Nil));
    is_bool("ok")
        && is_num_or_nil("exit_code")
        && is_str("stdout")
        && is_str("stderr")
        && is_bool("timed_out")
        && is_bool("interrupted")
        && is_num_or_nil("signal")
        && is_num("duration_ms")
        && is_bool("stdout_truncated")
        && is_bool("stderr_truncated")
        && is_bool("utf8_lossy")
        && is_str_or_nil("error_code")
        && is_str_or_nil("error")
}

/// A failure-shaped ssh_exec result (run_argv schema + host + error).
fn ssh_exec_error(host: &str, code: &str, message: &str, duration_ms: f64) -> Value {
    let mut m = indexmap::IndexMap::new();
    m.insert("ok".to_string(), Value::Bool(false));
    m.insert("exit_code".to_string(), Value::Nil);
    m.insert("stdout".to_string(), Value::String(String::new()));
    m.insert("stderr".to_string(), Value::String(String::new()));
    m.insert("timed_out".to_string(), Value::Bool(code == "SSH_TIMEOUT"));
    m.insert(
        "interrupted".to_string(),
        Value::Bool(code == "SSH_INTERRUPTED"),
    );
    m.insert("signal".to_string(), Value::Nil);
    m.insert("duration_ms".to_string(), Value::Number(duration_ms));
    m.insert("stdout_truncated".to_string(), Value::Bool(false));
    m.insert("stderr_truncated".to_string(), Value::Bool(false));
    m.insert("utf8_lossy".to_string(), Value::Bool(false));
    m.insert("error_code".to_string(), Value::String(code.to_string()));
    m.insert("error".to_string(), Value::String(message.to_string()));
    m.insert("host".to_string(), Value::String(host.to_string()));
    Value::map(m)
}

/// Quote a string as a double-quoted Mix string literal for embedding
/// in generated driver source. Only `\`, `"`, and `$` need escaping
/// inside a Mix double-quoted string (`${...}` is the sole interpolation
/// trigger, so escaping `$` disables it); the strict-data payload here
/// never contains newlines mid-literal (to_mix_data_string single-line).
fn quote_mix_string(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 2);
    out.push('"');
    for c in s.chars() {
        match c {
            '\\' => out.push_str("\\\\"),
            '"' => out.push_str("\\\""),
            '$' => out.push_str("\\$"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            other => out.push(other),
        }
    }
    out.push('"');
    out
}

/// grep_lines(text, pattern) — grep() with the args the consistent
/// (subject-first) way round; the 0.63.0 twin that survives release B.
fn builtin_grep_lines(args: Vec<Value>) -> MixResult<Option<Value>> {
    expect_args("grep_lines", &args, 2)?;
    grep_impl(&args[1].to_mix_string(), &args[0].to_mix_string())
}

/// grep(pattern, text) — return lines from text matching pattern.
/// Uses regex when the regex feature is enabled, otherwise substring match.
fn builtin_grep(args: Vec<Value>) -> MixResult<Option<Value>> {
    expect_args("grep", &args, 2)?;
    grep_impl(&args[0].to_mix_string(), &args[1].to_mix_string())
}

fn grep_impl(pattern: &str, text: &str) -> MixResult<Option<Value>> {
    let pattern = pattern.to_string();
    let text = text.to_string();

    #[cfg(feature = "regex")]
    let re = compile_regex(&pattern)?;

    let matches: Vec<Value> = text
        .lines()
        .filter(|line| {
            #[cfg(feature = "regex")]
            {
                re.is_match(line)
            }
            #[cfg(not(feature = "regex"))]
            {
                line.contains(&pattern)
            }
        })
        .map(|line| Value::String(line.to_string()))
        .collect();
    Ok(Some(Value::list(matches)))
}

/// line_count(path) — count lines in a file without reading it all into a Value.
///
/// Streams the file in buffered chunks counting `\n` bytes, so a multi-GB
/// log costs one 64 KiB buffer, not its own size in RAM. Byte-oriented:
/// unlike `read_lines` it never UTF-8-validates, so it works on any file.
/// Count matches `str::lines` semantics — a trailing newline does not add
/// an empty final line; content after the last newline counts as one line.
fn builtin_line_count(args: Vec<Value>) -> MixResult<Option<Value>> {
    use std::io::BufRead as _;
    expect_args("line_count", &args, 1)?;
    let path = args[0].to_mix_string();
    let err = |e: std::io::Error| MixError::RuntimeError {
        span: None,
        msg: format!("line_count '{}': {}", path, e),
    };
    let file = std::fs::File::open(&path).map_err(err)?;
    let mut reader = std::io::BufReader::with_capacity(64 * 1024, file);
    let mut count: usize = 0;
    let mut last_byte: u8 = b'\n';
    let mut nonempty = false;
    loop {
        let buf = reader.fill_buf().map_err(err)?;
        if buf.is_empty() {
            break;
        }
        count += buf.iter().filter(|&&b| b == b'\n').count();
        last_byte = buf[buf.len() - 1];
        nonempty = true;
        let consumed = buf.len();
        reader.consume(consumed);
    }
    if nonempty && last_byte != b'\n' {
        count += 1;
    }
    Ok(Some(Value::Number(count as f64)))
}

/// Shared optional-N parser for `head`/`tail`: missing → 10 (coreutils
/// default); otherwise require a real non-negative integer Number — no
/// `to_number()` coercion (a bool or numeric string silently becoming a
/// line count is the same surprise class as `read_file_bytes`'s cap).
fn head_tail_n(name: &str, args: &[Value]) -> MixResult<usize> {
    match args.get(1) {
        None => Ok(10),
        Some(value) => {
            let n = extract_number(value, InputPolicy::NumberOnly).ok_or_else(|| {
                MixError::RuntimeError {
                    span: None,
                    msg: format!(
                        "{}(): n must be a non-negative integer, got {}",
                        name,
                        value.to_mix_string()
                    ),
                }
            })?;
            // Context says "n" to match the type-path error above, so both
            // doorways of this builtin speak about the same argument name.
            as_count(&format!("{name}(): n"), n, usize::MAX)
        }
    }
}

/// `head($path[, $n])` — first N lines (default 10) as a list.
///
/// The no-slurp twin of `take(read_lines(p), n)`: reads through a
/// `BufReader` and stops after N lines, so the rest of the file is never
/// touched. Line semantics match `read_lines` (`\n`/`\r\n` stripped);
/// like `read_lines` it errors on invalid UTF-8 within the lines it reads.
fn builtin_head(args: Vec<Value>) -> MixResult<Option<Value>> {
    use std::io::BufRead as _;
    expect_args_between("head", &args, 1, 2)?;
    let path = args[0].to_mix_string();
    let n = head_tail_n("head", &args)?;
    if n == 0 {
        return Ok(Some(Value::list(Vec::new())));
    }
    let file = std::fs::File::open(&path).map_err(|e| MixError::RuntimeError {
        span: None,
        msg: format!("head '{}': {}", path, e),
    })?;
    let reader = std::io::BufReader::new(file);
    let mut lines: Vec<Value> = Vec::new();
    for line in reader.lines().take(n) {
        let line = line.map_err(|e| MixError::RuntimeError {
            span: None,
            msg: format!("head '{}': {}", path, e),
        })?;
        lines.push(Value::String(line));
    }
    Ok(Some(Value::list(lines)))
}

/// `tail($path[, $n])` — last N lines (default 10) as a list.
///
/// The no-slurp twin of `take(read_lines(p), -n)`: seeks to EOF and reads
/// 64 KiB blocks backwards until N+1 newlines are in hand (or the file
/// start is reached), so memory is bounded by the size of the last N
/// lines plus one block — a multi-GB log never gets slurped. The last n
/// lines are selected at the byte level and only they are UTF-8-decoded,
/// so neither a torn multi-byte character at the block boundary nor a
/// binary line in the non-returned overshoot can poison the result.
/// Line semantics match `read_lines`; errors only when the *returned*
/// lines themselves are invalid UTF-8.
fn builtin_tail(args: Vec<Value>) -> MixResult<Option<Value>> {
    use std::io::{Read as _, Seek as _, SeekFrom};
    expect_args_between("tail", &args, 1, 2)?;
    let path = args[0].to_mix_string();
    let n = head_tail_n("tail", &args)?;
    if n == 0 {
        return Ok(Some(Value::list(Vec::new())));
    }
    let err = |e: std::io::Error| MixError::RuntimeError {
        span: None,
        msg: format!("tail '{}': {}", path, e),
    };
    let mut file = std::fs::File::open(&path).map_err(err)?;
    let len = file.seek(SeekFrom::End(0)).map_err(err)?;
    if len == 0 {
        return Ok(Some(Value::list(Vec::new())));
    }
    const CHUNK: u64 = 64 * 1024;
    let mut suffix: Vec<u8> = Vec::new();
    let mut pos = len;
    loop {
        let read_size = CHUNK.min(pos);
        pos -= read_size;
        file.seek(SeekFrom::Start(pos)).map_err(err)?;
        let mut block = vec![0u8; read_size as usize];
        file.read_exact(&mut block).map_err(err)?;
        block.extend_from_slice(&suffix);
        suffix = block;
        // N+1 newlines guarantee at least N complete lines after the
        // first one, whatever the trailing-newline situation at EOF.
        if pos == 0 || suffix.iter().filter(|&&b| b == b'\n').count() > n {
            break;
        }
    }
    if pos > 0 {
        // Reading stopped mid-line: drop the partial first line (also
        // removes any torn multi-byte character at the block boundary).
        if let Some(i) = suffix.iter().position(|&b| b == b'\n') {
            suffix.drain(..=i);
        }
    }
    // Select the last n lines at the BYTE level and decode only those —
    // the block overshoot can contain earlier binary lines that are not
    // returned, and they must not poison the result with a UTF-8 error.
    let ends_with_nl = suffix.ends_with(b"\n");
    let mut raw_lines: Vec<&[u8]> = suffix.split(|&b| b == b'\n').collect();
    if ends_with_nl {
        raw_lines.pop(); // trailing newline, not a phantom empty line
    }
    let start = raw_lines.len().saturating_sub(n);
    let mut lines: Vec<Value> = Vec::with_capacity(raw_lines.len() - start);
    for (i, raw) in raw_lines[start..].iter().enumerate() {
        // Strip a trailing \r only from \n-terminated lines (the \r\n
        // pair) — str::lines preserves a lone \r on an unterminated
        // final line, and read_lines parity must too.
        let nl_terminated = ends_with_nl || start + i < raw_lines.len() - 1;
        let raw: &[u8] = match raw.last() {
            Some(b'\r') if nl_terminated => &raw[..raw.len() - 1],
            _ => raw,
        };
        let s = std::str::from_utf8(raw).map_err(|_| MixError::RuntimeError {
            span: None,
            msg: format!("tail '{}': returned lines are not valid UTF-8", path),
        })?;
        lines.push(Value::String(s.to_string()));
    }
    Ok(Some(Value::list(lines)))
}

fn builtin_lastpos(args: Vec<Value>) -> MixResult<Option<Value>> {
    expect_args("lastpos", &args, 2)?;
    let needle = args[0].to_mix_string();
    let haystack = args[1].to_mix_string();
    // P1: codepoint offset (1-based, 0=not found) — see builtin_pos. `rfind`
    // returns a char-boundary byte index; `byte_lastpos` keeps raw bytes.
    let pos = haystack
        .rfind(&needle)
        .map(|b| haystack[..b].chars().count() + 1)
        .unwrap_or(0);
    Ok(Some(Value::Number(pos as f64)))
}

// --- File I/O builtins ---

fn builtin_read_file(args: Vec<Value>) -> MixResult<Option<Value>> {
    expect_args("read_file", &args, 1)?;
    let path = args[0].to_mix_string();
    let content = std::fs::read_to_string(&path).map_err(|e| MixError::RuntimeError {
        span: None,
        msg: format!("read_file '{}': {}", path, e),
    })?;
    Ok(Some(Value::String(content)))
}

/// `read_file_bytes($path[, $max])` — read a file as raw bytes.
///
/// Companion to `read_file` for binary payloads. `read_file` requires
/// the file to be valid UTF-8 (it uses `read_to_string`, which errors
/// on non-UTF-8); `read_file_bytes` carries the buffer verbatim through
/// `Value::Bytes`, so high-bit bytes survive intact.
///
/// The optional second argument caps the read at `$max` bytes via
/// `File::take`, so header-sniffing (`read_file_bytes(p, 8192)`) never
/// slurps a multi-megabyte attachment to use the first few KiB. `max`
/// must be a finite non-negative integer; `max == 0` reads nothing.
fn builtin_read_file_bytes(args: Vec<Value>) -> MixResult<Option<Value>> {
    use std::io::Read as _;
    expect_args("read_file_bytes", &args, 1)?;
    if args.len() > 2 {
        return Err(MixError::RuntimeError {
            span: None,
            msg: format!("read_file_bytes() expects 1 or 2 args, got {}", args.len()),
        });
    }
    let path = args[0].to_mix_string();

    if let Some(max_val) = args.get(1) {
        // Require a real Number (not `to_number()`, which coerces a bool
        // and a numeric string — a silent surprise for a read cap).
        let n = match extract_number(max_val, InputPolicy::NumberOnly) {
            Some(n) => n,
            None => {
                return Err(MixError::RuntimeError {
                    span: None,
                    msg: format!(
                        "read_file_bytes(): max must be a number, got {}",
                        max_val.type_name()
                    ),
                });
            }
        };
        // Cap ceiling is i64::MAX (was exclusive-2^64 pre-0.59.0; `u64::MAX as
        // f64` rounds UP, which is why the old bound had to be exclusive --
        // the domain layer sidesteps that trap by comparing in i128, and
        // 2^63-1 bytes is already far past any real file). The `as u64` is
        // exact for everything the validator admits.
        let cap = as_exact_integer("read_file_bytes(): argument 2", n, 0, i64::MAX)? as u64;
        let file = std::fs::File::open(&path).map_err(|e| MixError::RuntimeError {
            span: None,
            msg: format!("read_file_bytes '{}': {}", path, e),
        })?;
        let mut bytes = Vec::new();
        file.take(cap)
            .read_to_end(&mut bytes)
            .map_err(|e| MixError::RuntimeError {
                span: None,
                msg: format!("read_file_bytes '{}': {}", path, e),
            })?;
        return Ok(Some(Value::bytes(bytes)));
    }

    let bytes = std::fs::read(&path).map_err(|e| MixError::RuntimeError {
        span: None,
        msg: format!("read_file_bytes '{}': {}", path, e),
    })?;
    Ok(Some(Value::bytes(bytes)))
}

/// `read_stdin_bytes([max])` — the binary twin of `read_stdin` (v0.65.0).
///
/// `read_stdin` decodes to a `String` and **refuses** invalid UTF-8, which
/// makes binary stdin unreadable: a mail body in an 8-bit transfer encoding,
/// an image, a protocol frame. This reads fd 0 to EOF as raw `bytes`, with
/// the same optional cap `read_file_bytes` takes.
///
/// Lives here, next to `read_file_bytes`, so the two share one cap contract;
/// the evaluator calls it from an inline special form because it also has to
/// run the capability (Knob A) and collection-size (Knob D) checks that
/// `read_stdin` runs.
pub(crate) fn read_stdin_bytes_impl(name: &str, args: &[Value]) -> MixResult<Value> {
    use std::io::Read as _;
    if args.len() > 1 {
        return Err(MixError::RuntimeError {
            span: None,
            msg: format!("{name}() expects 0 or 1 args, got {}", args.len()),
        });
    }
    let mut bytes = Vec::new();
    let stdin = std::io::stdin();
    let read = match args.first() {
        None | Some(Value::Nil) => stdin.lock().read_to_end(&mut bytes),
        Some(max_val) => {
            // Same strictness as `read_file_bytes`: a real Number, not
            // `to_number()`, which would coerce a bool or a numeric string
            // into a read cap without saying so.
            let n = match extract_number(max_val, InputPolicy::NumberOnly) {
                Some(n) => n,
                None => {
                    return Err(MixError::RuntimeError {
                        span: None,
                        msg: format!(
                            "{name}(): max must be a number, got {}",
                            max_val.type_name()
                        ),
                    });
                }
            };
            let cap = as_exact_integer(&format!("{name}(): argument 1"), n, 0, i64::MAX)? as u64;
            stdin.lock().take(cap).read_to_end(&mut bytes)
        }
    };
    // Not `.ok()`: `read_stdin`'s own comment records why swallowing this is
    // a bug — git feeds pre-push its ref list on stdin, and an error turned
    // into an empty result reads as "nothing to do". A truncated binary read
    // is the same class of silence.
    read.map_err(|e| MixError::RuntimeError {
        span: None,
        msg: format!("{name}: {e}"),
    })?;
    Ok(Value::bytes(bytes))
}

/// Append one `write_stdout`/`write_stderr` argument to the output buffer.
///
/// `bytes`/`buffer` go out **verbatim** — that is the whole point of the
/// family, and it is also the one place `to_mix_string` would be actively
/// wrong (it renders the `<bytes:N>` placeholder). Everything else gets the
/// exact rendering `print` gives it, so `write_stdout($x)` is `print($x)`
/// without the newline for every value where `print` is meaningful.
pub(crate) fn append_output_bytes(v: &Value, out: &mut Vec<u8>) {
    match v {
        Value::Bytes(b) => out.extend_from_slice(b),
        Value::Buffer(b) => out.extend_from_slice(&b.borrow()),
        other => out.extend_from_slice(other.to_mix_string().as_bytes()),
    }
}

/// `load_data($path)` — read a strict-data `.mix` file and parse it
/// into a Value.
///
/// The non-executing twin of `source` / `include` (which run a file as
/// a script): `load_data` reads the **strict-data** form (bare-key
/// `key: value`, the `zones.mix` / `conf.mix` form — NOT `$x = {...}`)
/// and returns the parsed structure as inert data, never executing it.
/// Use it for substrate-internal data that must not run as code — e.g.
/// the SPEC 13 mesh inventory, so the Mix tooling and the Rust signer
/// read the one authored file through the same parser. Wraps
/// [`crate::parse_data_file`].
fn builtin_load_data(args: Vec<Value>) -> MixResult<Option<Value>> {
    expect_args("load_data", &args, 1)?;
    let path = args[0].to_mix_string();
    // A malformed data file is a runtime condition about someone else's
    // bytes, not a defect in the running program — so it must be
    // catchable, exactly like the missing-file case (already a
    // RuntimeError, raised by `parse_data_file`) and exactly like
    // `data_parse`, this builtin's string twin, which has mapped its
    // parse failures since it was written.
    //
    // A bare LexerError/ParseError/StrictDataViolation is not catchable:
    // `try/catch` refuses those deliberately, because a syntax error in
    // the SCRIPT is not a condition the script can handle. Letting one
    // escape from HERE wore that rule's clothes while meaning the
    // opposite — a one-line typo in a config aborted the interpreter
    // with exit 1, straight through a try/catch written specifically to
    // turn it into something else. Found 2026-07-25: a repo-hygiene gate
    // whose exit contract reserves 1 for "policy violation" answered an
    // unparseable config with 1, and the catch it had carried since its
    // first commit had never once been able to fire.
    //
    // Matched on catchability rather than on a variant list so a future
    // uncatchable variant cannot silently reopen the hole.
    let value = crate::parse_data_file(std::path::Path::new(&path)).map_err(|e| match e {
        keep @ (MixError::RuntimeError { .. }
        | MixError::DieError { .. }
        | MixError::Structured(_)) => keep,
        other => MixError::RuntimeError {
            span: None,
            msg: format!("load_data '{path}': {other}"),
        },
    })?;
    Ok(Some(value))
}

/// `read_lines($path)` — read a text file and return a list of
/// lines. Trailing newline on each line is stripped. A trailing
/// empty line (from a file ending in `\n`) is dropped so callers
/// don't have to filter it out. For binary-safe reading, use
/// `read_file_bytes`.
fn builtin_read_lines(args: Vec<Value>) -> MixResult<Option<Value>> {
    expect_args("read_lines", &args, 1)?;
    let path = args[0].to_mix_string();
    let content = std::fs::read_to_string(&path).map_err(|e| MixError::RuntimeError {
        span: None,
        msg: format!("read_lines '{}': {}", path, e),
    })?;
    let lines: Vec<Value> = content
        .lines()
        .map(|l| Value::String(l.to_string()))
        .collect();
    Ok(Some(Value::list(lines)))
}

/// `write_file($path, $content)` — write to `path`, creating or
/// overwriting. Symlinks are followed (deliberate, standard shell
/// semantics — like `> path` redirection); use `write_new` when the
/// target must be a freshly created regular file.
fn builtin_write_file(args: Vec<Value>) -> MixResult<Option<Value>> {
    expect_args("write_file", &args, 2)?;
    let path = args[0].to_mix_string();
    // `Value::Bytes` is written verbatim — the whole point of the
    // bytes type is that it carries high-bit bytes without going
    // through a UTF-8 round-trip. Every other Value renders through
    // `to_mix_string` (matching the prior behaviour for String,
    // Number, Bool, List, Map, Nil, Function).
    let write_result = match &args[1] {
        Value::Bytes(buf) => std::fs::write(&path, buf.as_slice()),
        // A mutable buffer writes its current bytes verbatim, like Bytes.
        Value::Buffer(b) => std::fs::write(&path, b.borrow().as_slice()),
        other => std::fs::write(&path, other.to_mix_string()),
    };
    write_result.map_err(|e| MixError::RuntimeError {
        span: None,
        msg: format!("write_file '{}': {}", path, e),
    })?;
    Ok(Some(Value::Nil))
}

fn builtin_append_file(args: Vec<Value>) -> MixResult<Option<Value>> {
    expect_args("append_file", &args, 2)?;
    let path = args[0].to_mix_string();
    use std::io::Write;
    let mut file = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&path)
        .map_err(|e| MixError::RuntimeError {
            span: None,
            msg: format!("append_file '{}': {}", path, e),
        })?;
    // Symmetric with `write_file`: `Value::Bytes` appends raw, every
    // other Value goes through `to_mix_string`.
    let write_result = match &args[1] {
        Value::Bytes(buf) => file.write_all(buf),
        Value::Buffer(b) => file.write_all(b.borrow().as_slice()),
        other => file.write_all(other.to_mix_string().as_bytes()),
    };
    write_result.map_err(|e| MixError::RuntimeError {
        span: None,
        msg: format!("append_file '{}': {}", path, e),
    })?;
    Ok(Some(Value::Nil))
}

fn builtin_exists(args: Vec<Value>) -> MixResult<Option<Value>> {
    if args.is_empty() || args.len() > 2 {
        return Err(MixError::RuntimeError {
            span: None,
            msg: format!("exists() expects 1 or 2 args, got {}", args.len()),
        });
    }
    let path = args[0].to_mix_string();

    let mut follow_symlinks = true;
    if let Some(opts) = args.get(1) {
        match opts {
            Value::Nil => {}
            Value::Map(m) => {
                if let Some(v) = m.get("follow_symlinks") {
                    follow_symlinks = v.is_truthy();
                }
            }
            other => {
                return Err(MixError::RuntimeError {
                    span: None,
                    msg: format!(
                        "exists(): options must be a map or nil, got {}",
                        other.type_name()
                    ),
                });
            }
        }
    }

    let p = std::path::Path::new(&path);
    let present = if follow_symlinks {
        p.exists()
    } else {
        std::fs::symlink_metadata(p).is_ok()
    };
    Ok(Some(Value::Bool(present)))
}

fn parse_access_mode(mode: &str) -> MixResult<libc::c_int> {
    if mode.is_empty() {
        return Err(MixError::RuntimeError {
            span: None,
            msg: "access(): mode must not be empty".to_string(),
        });
    }

    let mut seen = 0_u8;
    let mut mask = libc::F_OK;
    for letter in mode.bytes() {
        let (bit, permission) = match letter {
            b'r' => (1, libc::R_OK),
            b'w' => (2, libc::W_OK),
            b'x' => (4, libc::X_OK),
            b'f' => (8, libc::F_OK),
            _ => {
                return Err(MixError::RuntimeError {
                    span: None,
                    msg: format!(
                        "access(): mode must contain only r, w, x, and f; got {:?}",
                        mode
                    ),
                });
            }
        };
        if seen & bit != 0 {
            return Err(MixError::RuntimeError {
                span: None,
                msg: format!(
                    "access(): mode contains repeated letter '{}'",
                    letter as char
                ),
            });
        }
        seen |= bit;
        mask |= permission;
    }
    Ok(mask)
}

/// Ask the kernel whether `path` is accessible to this process. This is not
/// derived from `stat().perm`: `faccessat` applies the filesystem's complete
/// permission decision, including POSIX ACL entries.
///
/// `AT_EACCESS` selects the effective uid/gid rather than the real ids. That is
/// the identity the setuid-free caller we care about (for example git deciding
/// whether to execute a hook) actually runs under. No `AT_SYMLINK_NOFOLLOW`
/// flag is supplied, deliberately: access is about the symlink target.
fn builtin_access(args: Vec<Value>) -> MixResult<Option<Value>> {
    // EXACT arity, not the usual minimum. `expect_args` is a floor, so
    // `access(p, "f", "x")` would otherwise run the F_OK check and silently
    // discard the `"x"` — a permissive answer to a stricter question, which is
    // the precise failure `parse_access_mode` refuses one argument to the left.
    // A predicate that gets used to decide whether a security gate is live has
    // no business being lenient about its own call shape.
    expect_args_between("access", &args, 2, 2)?;
    let path = args[0].to_mix_string();
    let mode = match &args[1] {
        Value::String(mode) => mode,
        other => {
            return Err(MixError::RuntimeError {
                span: None,
                msg: format!("access(): mode must be a string, got {}", other.type_name()),
            });
        }
    };
    let mask = parse_access_mode(mode)?;
    Ok(Some(Value::Bool(access_ok("access", &path, mask)?)))
}

/// The kernel half of `access()`, shared with `which()`. Returns the kernel's
/// yes/no; raises only when the question itself failed. `caller`-agnostic: the
/// errno reasoning below is identical for every asker.
fn access_ok(caller: &str, path: &str, mask: libc::c_int) -> MixResult<bool> {
    use std::ffi::CString;

    let c_path = CString::new(path.as_bytes()).map_err(|_| MixError::RuntimeError {
        span: None,
        // `{caller}()` here and a bare `{caller}` in the errno arms below is
        // not an inconsistency to tidy: it reproduces access()'s existing
        // message text exactly, and these strings are a contract.
        msg: format!("{caller}(): path contains an interior NUL byte"),
    })?;

    // `libc::faccessat` is deliberately NOT what this calls on Linux, and the
    // reason is the entire point of the builtin. The kernel's `faccessat(2)`
    // takes no flags, so glibc implements `AT_EACCESS` inside its wrapper --
    // and on glibc <= 2.32, or on any kernel without `faccessat2(2)` (Linux
    // < 5.8), that wrapper decides the answer from `fstatat(2)` mode bits
    // instead. The man page states the consequence plainly: that emulation
    // "does not take ACLs into account". It is the exact mode-bit arithmetic
    // this builtin exists to replace, it is silent, and it returns a confident
    // `true` for a path an ACL denies. Issuing the syscall directly means the
    // answer is the kernel's or there is no answer -- `ENOSYS` raises below.
    #[cfg(target_os = "linux")]
    let rc = unsafe {
        libc::syscall(
            libc::SYS_faccessat2,
            libc::AT_FDCWD,
            c_path.as_ptr(),
            mask,
            libc::AT_EACCESS,
        )
    } as libc::c_int;
    // Elsewhere the flag is the kernel's own argument rather than a wrapper's
    // emulation, so the portable call is already the honest one.
    #[cfg(not(target_os = "linux"))]
    let rc = unsafe { libc::faccessat(libc::AT_FDCWD, c_path.as_ptr(), mask, libc::AT_EACCESS) };
    if rc == 0 {
        return Ok(true);
    }

    // These errnos are the kernel ANSWERING the question with "no": the path is
    // not there, or it is there and this process may not do that to it.
    // `ETXTBSY` is the one that reads oddly and belongs here all the same --
    // it is `w` refused on a file that is currently being executed, which is a
    // denial and not a malfunction. `EPERM` is here for the same reason (a `w`
    // query against an immutable file) despite being the one genuinely
    // ambiguous case: a seccomp or LSM policy that blocks the syscall outright
    // also reports `EPERM`, and would then read as an ordinary "no". That is
    // the safe direction to be wrong in for every caller this predicate has --
    // "you cannot" fails closed, where raising would take down a report that
    // was only trying to describe a file.
    //
    // Anything else means the question itself failed (`EFAULT`, `EINVAL`,
    // `ENOMEM`, `EIO`) and a `false` there would be a lie dressed as an answer,
    // so it raises.
    let err = std::io::Error::last_os_error();
    match err.raw_os_error() {
        Some(
            libc::ENOENT
            | libc::EACCES
            | libc::ENOTDIR
            | libc::ELOOP
            | libc::ENAMETOOLONG
            | libc::EROFS
            | libc::ETXTBSY
            | libc::EPERM,
        ) => Ok(false),
        // The one errno that is neither an answer nor a malfunction: this
        // kernel predates `faccessat2(2)`. Retrying through glibc's wrapper
        // here would substitute mode-bit arithmetic for the kernel's decision
        // without saying so -- which is precisely what every caller of this
        // builtin is trying to get away from -- so it refuses instead. A
        // caller that genuinely wants the arithmetic can read `stat()["perm"]`
        // and own that choice out loud.
        #[cfg(target_os = "linux")]
        Some(libc::ENOSYS) => Err(MixError::RuntimeError {
            span: None,
            msg: format!(
                "{caller} '{path}': this kernel has no faccessat2(2) (Linux < 5.8), so an \
                 ACL-aware permission check cannot be made here; {caller} will not answer \
                 from mode bits instead"
            ),
        }),
        _ => Err(MixError::RuntimeError {
            span: None,
            msg: format!("{} '{}': {}", caller, path, err),
        }),
    }
}

fn builtin_is_dir(args: Vec<Value>) -> MixResult<Option<Value>> {
    expect_args("is_dir", &args, 1)?;
    let path = args[0].to_mix_string();
    Ok(Some(Value::Bool(std::path::Path::new(&path).is_dir())))
}

fn builtin_is_file(args: Vec<Value>) -> MixResult<Option<Value>> {
    expect_args("is_file", &args, 1)?;
    let path = args[0].to_mix_string();
    Ok(Some(Value::Bool(std::path::Path::new(&path).is_file())))
}

/// Canonicalise a path to its absolute real path, resolving every symlink + `.`/`..`.
/// `None` when it can't be resolved (missing component, symlink loop). This is the ONE
/// canonicalisation in the language: the `realpath` builtin returns it, and require/include
/// dedup their per-path cache with it (so the builtin and the module loader can never drift).
pub(crate) fn canonicalize_path(path: &str) -> Option<String> {
    // into_string() (not to_string_lossy): a Unix symlink target may contain non-UTF-8
    // bytes, and a lossy `U+FFFD` substitution would name a DIFFERENT file than the real
    // canonical target — unsound for a security check. Non-UTF-8 → None (fail closed).
    std::fs::canonicalize(path)
        .ok()
        .and_then(|p| p.into_os_string().into_string().ok())
}

fn builtin_realpath(args: Vec<Value>) -> MixResult<Option<Value>> {
    expect_args("realpath", &args, 1)?;
    let path = args[0].to_mix_string();
    // A missing component or a symlink loop is a normal not-resolvable outcome, so this
    // returns nil rather than raising — the caller decides (an exec-safety check treats
    // nil as "refuse").
    Ok(Some(match canonicalize_path(&path) {
        Some(s) => Value::String(s),
        None => Value::Nil,
    }))
}

fn builtin_glob(args: Vec<Value>) -> MixResult<Option<Value>> {
    expect_args("glob", &args, 1)?;
    let pattern = args[0].to_mix_string();

    // v0.2.1: multi-component glob with `**` recursive descent.
    //
    // Pattern splits on `/`. Each non-empty component is matched
    // against directory entries with `glob_match` (supports `*` and
    // `?`). The special token `**` matches zero or more directory
    // levels — i.e. the remaining pattern is tried at the current
    // directory AND at every descendant, in breadth-first-ish order.
    //
    // Absolute paths (pattern starts with `/`) start from `/`.
    // Relative paths start from `.`. Trailing `/` has no special
    // meaning — scripts that want only directories should filter
    // the result with `is_dir`.
    //
    // This replaces the v0.2.0 implementation which only handled
    // `*` in the final path component. The digest script's
    // hand-rolled two-level walk becomes a single `glob(...)` call.
    use std::path::PathBuf;

    let (start, comps): (PathBuf, Vec<&str>) = if let Some(stripped) = pattern.strip_prefix('/') {
        (PathBuf::from("/"), stripped.split('/').collect())
    } else {
        (PathBuf::from("."), pattern.split('/').collect())
    };

    // Strip empty trailing component from patterns like `foo/`.
    let comps: Vec<&str> = comps.into_iter().filter(|s| !s.is_empty()).collect();

    let mut out: Vec<PathBuf> = Vec::new();
    glob_expand(&start, &comps, &mut out);

    // Normalize `./foo` back to `foo` for relative patterns so
    // callers don't get a stray `./` prefix they didn't ask for.
    let mut results: Vec<Value> = out
        .into_iter()
        .map(|p| {
            let s = p.to_string_lossy().into_owned();
            if let Some(rest) = s.strip_prefix("./") {
                Value::String(rest.to_string())
            } else {
                Value::String(s)
            }
        })
        .collect();
    results.sort_by_key(|a| a.to_mix_string());
    Ok(Some(Value::list(results)))
}

/// Recursive component walker for `glob`. Appends matching paths to
/// `out`. `base` is the directory currently being searched;
/// `components` is the remaining pattern tail.
///
/// Three branches:
/// 1. `components` empty → `base` is a complete match if it exists.
/// 2. First component is `**` → two choices: skip the `**` (zero
///    dirs), OR consume one directory level and retry with `**`
///    still at the head (one-or-more dirs). Recurses breadth-first
///    via the first branch immediately matching at current level.
/// 3. Otherwise → match `glob_match(first, entry_name)` for each
///    entry in `base`; recurse into matches with `rest`.
fn glob_expand(base: &std::path::Path, components: &[&str], out: &mut Vec<std::path::PathBuf>) {
    if components.is_empty() {
        // A complete pattern. `/srv` on `base = /srv` lands here
        // only when the prefix matched; include it if it exists.
        if base.exists() {
            out.push(base.to_path_buf());
        }
        return;
    }

    let first = components[0];
    let rest = &components[1..];

    if first == "**" {
        // Zero-levels case: the remaining pattern applies at base.
        glob_expand(base, rest, out);
        // One-or-more-levels case: for each direct subdir, retry
        // with `**` still leading so the recursion can terminate at
        // any depth.
        if let Ok(entries) = std::fs::read_dir(base) {
            for e in entries.flatten() {
                if let Ok(ft) = e.file_type()
                    && ft.is_dir()
                {
                    glob_expand(&e.path(), components, out);
                }
            }
        }
        return;
    }

    if first.contains('*') || first.contains('?') {
        if let Ok(entries) = std::fs::read_dir(base) {
            for e in entries.flatten() {
                let name = e.file_name().to_string_lossy().into_owned();
                if glob_match(first, &name) {
                    glob_expand(&e.path(), rest, out);
                }
            }
        }
        return;
    }

    // Literal component — join without touching the filesystem so
    // nonexistent intermediate paths still drop cleanly at the
    // terminal-exists check (avoids two different "doesn't exist"
    // paths between literal and wildcard components).
    let next = base.join(first);
    glob_expand(&next, rest, out);
}

/// Simple glob matching supporting * and ?
fn glob_match(pattern: &str, text: &str) -> bool {
    let pat: Vec<char> = pattern.chars().collect();
    let txt: Vec<char> = text.chars().collect();
    glob_match_inner(&pat, &txt)
}

fn glob_match_inner(pat: &[char], txt: &[char]) -> bool {
    match (pat.first(), txt.first()) {
        (None, None) => true,
        (Some('*'), _) => {
            // * matches zero or more characters
            glob_match_inner(&pat[1..], txt)
                || (!txt.is_empty() && glob_match_inner(pat, &txt[1..]))
        }
        (Some('?'), Some(_)) => glob_match_inner(&pat[1..], &txt[1..]),
        (Some(p), Some(t)) if p == t => glob_match_inner(&pat[1..], &txt[1..]),
        _ => false,
    }
}

fn builtin_ls(args: Vec<Value>) -> MixResult<Option<Value>> {
    let dir = if args.is_empty() {
        ".".to_string()
    } else {
        args[0].to_mix_string()
    };
    let mut entries = Vec::new();
    let read_dir = std::fs::read_dir(&dir).map_err(|e| MixError::RuntimeError {
        span: None,
        msg: format!("ls '{}': {}", dir, e),
    })?;
    for entry in read_dir.flatten() {
        entries.push(Value::String(
            entry.file_name().to_string_lossy().to_string(),
        ));
    }
    entries.sort_by_key(|a| a.to_mix_string());
    Ok(Some(Value::list(entries)))
}

/// `mkdir` has always been `create_dir_all`, which is the convenient default and
/// the wrong one for a script that placed the parent deliberately: creating a
/// probe directory under `$TMPDIR` re-creates `$TMPDIR` itself, and every parent
/// above it, if something removed it since the check — so a run that meant to
/// refuse a missing temp directory silently manufactures one instead, and its
/// cleanup removes only the leaf. `{parents: false}` is the single-level form
/// (`create_dir`), which fails with NotFound rather than inventing the parent.
fn builtin_mkdir(args: Vec<Value>) -> MixResult<Option<Value>> {
    if args.is_empty() || args.len() > 2 {
        return Err(MixError::RuntimeError {
            span: None,
            msg: format!("mkdir() expects 1 or 2 args, got {}", args.len()),
        });
    }
    let path = args[0].to_mix_string();

    // This option exists to make a safety boundary explicit, so every way of
    // getting it slightly wrong has to raise rather than fall back to the
    // convenient default. `{parent: false}` (singular) and `{parents: "false"}`
    // (a string, which is truthy) are both plausible typos, and silently
    // ignoring either one re-creates the parent the caller just said not to
    // create — failing open at the exact point the caller was being careful.
    let mut parents = true;
    if let Some(opts) = args.get(1) {
        match opts {
            Value::Nil => {}
            Value::Map(m) => {
                for k in m.keys() {
                    if k.as_str() != "parents" {
                        return Err(MixError::RuntimeError {
                            span: None,
                            msg: format!(
                                "mkdir(): unknown option '{k}' (the only option is 'parents')"
                            ),
                        });
                    }
                }
                if let Some(v) = m.get("parents") {
                    match v {
                        Value::Bool(b) => parents = *b,
                        other => {
                            return Err(MixError::RuntimeError {
                                span: None,
                                msg: format!(
                                    "mkdir(): option 'parents' must be a boolean, got {}",
                                    other.type_name()
                                ),
                            });
                        }
                    }
                }
            }
            other => {
                return Err(MixError::RuntimeError {
                    span: None,
                    msg: format!(
                        "mkdir(): options must be a map or nil, got {}",
                        other.type_name()
                    ),
                });
            }
        }
    }

    let r = if parents {
        std::fs::create_dir_all(&path)
    } else {
        std::fs::create_dir(&path)
    };
    r.map_err(|e| MixError::RuntimeError {
        span: None,
        msg: format!("mkdir '{}': {}", path, e),
    })?;
    Ok(Some(Value::Nil))
}

#[derive(Default)]
struct FlockRegistry {
    /// One owned fd per canonical path. Dropping the `File` closes it, which is
    /// also the kernel's process-exit cleanup path.
    held: std::collections::HashMap<std::path::PathBuf, std::fs::File>,
    /// Serialises the open-file-description lock attempt per path without
    /// holding the registry mutex during a timed wait. Two separate opens in
    /// one process can contend with each other under flock(2), so an in-flight
    /// marker is needed for concurrent callers as well as the ordinary held
    /// lookup.
    acquiring: std::collections::HashSet<std::path::PathBuf>,
}

static FLOCK_REGISTRY: std::sync::LazyLock<(std::sync::Mutex<FlockRegistry>, std::sync::Condvar)> =
    std::sync::LazyLock::new(|| {
        (
            std::sync::Mutex::new(FlockRegistry::default()),
            std::sync::Condvar::new(),
        )
    });

fn flock_runtime_error(msg: impl Into<String>) -> MixError {
    MixError::RuntimeError {
        span: None,
        msg: msg.into(),
    }
}

fn parse_flock_options(args: &[Value]) -> MixResult<(bool, std::time::Duration)> {
    if args.is_empty() || args.len() > 2 {
        return Err(flock_runtime_error(format!(
            "flock() expects 1 or 2 args, got {}",
            args.len()
        )));
    }

    let mut shared = false;
    let mut wait_seconds = 0.0;
    if let Some(opts) = args.get(1) {
        match opts {
            Value::Nil => {}
            Value::Map(m) => {
                for k in m.keys() {
                    if k.as_str() != "shared" && k.as_str() != "wait" {
                        return Err(flock_runtime_error(format!(
                            "flock(): unknown option '{k}' (supported: shared, wait)"
                        )));
                    }
                }
                if let Some(v) = m.get("shared") {
                    match v {
                        Value::Bool(b) => shared = *b,
                        other => {
                            return Err(flock_runtime_error(format!(
                                "flock(): option 'shared' must be a boolean, got {}",
                                other.type_name()
                            )));
                        }
                    }
                }
                if let Some(v) = m.get("wait") {
                    wait_seconds = match extract_number(v, InputPolicy::NumberOnly) {
                        Some(n) => n,
                        None => {
                            return Err(flock_runtime_error(format!(
                                "flock(): option 'wait' must be a number, got {}",
                                v.type_name()
                            )));
                        }
                    };
                    // flock errors keep their established uniform shape.
                    as_duration("flock(): option 'wait'", wait_seconds).map_err(|_| {
                        flock_runtime_error(format!(
                            "flock(): option 'wait' must be a finite non-negative number, got {wait_seconds}"
                        ))
                    })?;
                }
            }
            other => {
                return Err(flock_runtime_error(format!(
                    "flock(): options must be a map or nil, got {}",
                    other.type_name()
                )));
            }
        }
    }

    // Defensive re-map: the identical value was already validated above with
    // flock's uniform error shape, so this arm is unreachable today -- the
    // map_err keeps the shape robust if the two sites are ever reordered.
    let wait = as_duration("flock(): option 'wait'", wait_seconds).map_err(|_| {
        flock_runtime_error(format!(
            "flock(): option 'wait' must be a finite non-negative number, got {wait_seconds}"
        ))
    })?;
    Ok((shared, wait))
}

fn try_flock_fd(
    file: &std::fs::File,
    shared: bool,
    wait: std::time::Duration,
) -> std::io::Result<bool> {
    use std::os::fd::AsRawFd;

    let operation = (if shared { libc::LOCK_SH } else { libc::LOCK_EX }) | libc::LOCK_NB;
    let deadline = std::time::Instant::now().checked_add(wait).ok_or_else(|| {
        std::io::Error::new(std::io::ErrorKind::InvalidInput, "wait is too large")
    })?;

    loop {
        // SAFETY: `file` owns a live fd for the duration of this call; flock(2)
        // reads only that integer and does not access caller-owned memory.
        if unsafe { libc::flock(file.as_raw_fd(), operation) } == 0 {
            return Ok(true);
        }
        let err = std::io::Error::last_os_error();
        if err.kind() != std::io::ErrorKind::WouldBlock {
            return Err(err);
        }
        let now = std::time::Instant::now();
        if now >= deadline {
            return Ok(false);
        }

        // flock(2) has no timed form, and a blocking worker cannot be cancelled
        // safely after the deadline (it could acquire later and retain a lock
        // nobody registered). Retry LOCK_NB with a short sleep instead: 10 ms
        // keeps acquisition latency sane without a busy-spin.
        std::thread::sleep(
            deadline
                .saturating_duration_since(now)
                .min(std::time::Duration::from_millis(10)),
        );
    }
}

/// Open/create first, then canonicalise. Canonicalising before `open` cannot
/// resolve an absent final component, while deriving a key from the parent gets
/// a final symlink wrong: `open` follows it and locks the target file.
fn builtin_flock(args: Vec<Value>) -> MixResult<Option<Value>> {
    use std::os::unix::fs::OpenOptionsExt;

    let (shared, wait) = parse_flock_options(&args)?;
    let path = args[0].to_mix_string();
    let deadline = std::time::Instant::now()
        .checked_add(wait)
        .ok_or_else(|| flock_runtime_error("flock(): option 'wait' is too large"))?;

    // The common idempotent path must not open the file again. Apart from
    // avoiding a transient fd, this means a mode/ACL change after acquisition
    // cannot make this process fail to recognise the lock it already owns.
    if let Ok(canonical) = std::fs::canonicalize(&path) {
        let registry = FLOCK_REGISTRY
            .0
            .lock()
            .map_err(|_| flock_runtime_error("flock(): lock registry is poisoned"))?;
        if registry.held.contains_key(&canonical) {
            return Ok(Some(Value::Bool(true)));
        }
    }

    let file = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .mode(0o644)
        .open(&path)
        .map_err(|e| flock_runtime_error(format!("flock '{}': {}", path, e)))?;
    let canonical = std::fs::canonicalize(&path)
        .map_err(|e| flock_runtime_error(format!("flock '{}': {}", path, e)))?;

    let (registry_mutex, changed) = &*FLOCK_REGISTRY;
    let mut registry = registry_mutex
        .lock()
        .map_err(|_| flock_runtime_error("flock(): lock registry is poisoned"))?;
    loop {
        if registry.held.contains_key(&canonical) {
            return Ok(Some(Value::Bool(true)));
        }
        if registry.acquiring.insert(canonical.clone()) {
            break;
        }
        let remaining = deadline.saturating_duration_since(std::time::Instant::now());
        if remaining.is_zero() {
            return Ok(Some(Value::Bool(false)));
        }
        let (next_registry, timed_out) = changed
            .wait_timeout(registry, remaining)
            .map_err(|_| flock_runtime_error("flock(): lock registry is poisoned"))?;
        registry = next_registry;
        if timed_out.timed_out() && !registry.held.contains_key(&canonical) {
            return Ok(Some(Value::Bool(false)));
        }
    }
    drop(registry);

    // Waiting behind a concurrent caller in this process consumes the same
    // caller-visible deadline as kernel contention; opts.wait is an overall
    // bound, not a fresh allowance at each layer.
    let result = try_flock_fd(
        &file,
        shared,
        deadline.saturating_duration_since(std::time::Instant::now()),
    );
    let mut registry = registry_mutex
        .lock()
        .map_err(|_| flock_runtime_error("flock(): lock registry is poisoned"))?;
    registry.acquiring.remove(&canonical);
    if matches!(result, Ok(true)) {
        registry.held.insert(canonical, file);
    }
    changed.notify_all();
    drop(registry);

    match result {
        Ok(acquired) => Ok(Some(Value::Bool(acquired))),
        Err(e) => Err(flock_runtime_error(format!("flock '{}': {}", path, e))),
    }
}

/// Resolve the ordinary held-file case exactly, but retain the useful
/// path-keyed behaviour after the lock file itself has been unlinked: the
/// canonical parent plus final name is the key `flock` recorded before unlink.
/// If no form can be resolved, there is no registry key to remove, which is the
/// documented not-held `false` case rather than an error.
fn canonical_path_for_funlock(path: &std::path::Path) -> Option<std::path::PathBuf> {
    std::fs::canonicalize(path).ok().or_else(|| {
        let name = path.file_name()?;
        let parent = path
            .parent()
            .filter(|p| !p.as_os_str().is_empty())
            .unwrap_or_else(|| std::path::Path::new("."));
        std::fs::canonicalize(parent).ok().map(|p| p.join(name))
    })
}

fn builtin_funlock(args: Vec<Value>) -> MixResult<Option<Value>> {
    use std::os::fd::AsRawFd;

    expect_args_between("funlock", &args, 1, 1)?;
    let path = args[0].to_mix_string();
    let Some(canonical) = canonical_path_for_funlock(std::path::Path::new(&path)) else {
        return Ok(Some(Value::Bool(false)));
    };

    let (registry_mutex, _) = &*FLOCK_REGISTRY;
    let file = registry_mutex
        .lock()
        .map_err(|_| flock_runtime_error("funlock(): lock registry is poisoned"))?
        .held
        .remove(&canonical);
    let Some(file) = file else {
        return Ok(Some(Value::Bool(false)));
    };

    // SAFETY: `file` owns a live fd until the end of this scope. Regardless of
    // whether LOCK_UN reports an error, dropping it closes the fd and asks the
    // kernel to release every lock associated with that open file description.
    if unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_UN) } != 0 {
        let e = std::io::Error::last_os_error();
        return Err(flock_runtime_error(format!("funlock '{}': {}", path, e)));
    }
    Ok(Some(Value::Bool(true)))
}

fn builtin_copy(args: Vec<Value>) -> MixResult<Option<Value>> {
    expect_args("copy", &args, 2)?;
    let src = args[0].to_mix_string();
    let dst = args[1].to_mix_string();
    std::fs::copy(&src, &dst).map_err(|e| MixError::RuntimeError {
        span: None,
        msg: format!("copy '{}' -> '{}': {}", src, dst, e),
    })?;
    Ok(Some(Value::Nil))
}

/// Recursive filesystem copy backing `copy_tree`. Mirrors `cp -a` for the parts
/// scripts actually need: directories are created, files copied (`std::fs::copy`
/// carries the permission bits), and symlinks recreated as symlinks (target
/// verbatim, no dereference). Merges into an existing `dst`.
fn copy_tree_impl(src: &std::path::Path, dst: &std::path::Path) -> std::io::Result<()> {
    let meta = std::fs::symlink_metadata(src)?;
    let ft = meta.file_type();
    if ft.is_symlink() {
        let target = std::fs::read_link(src)?;
        // Replace any existing entry so a re-run is idempotent.
        let _ = std::fs::remove_file(dst);
        std::os::unix::fs::symlink(target, dst)?;
    } else if ft.is_dir() {
        std::fs::create_dir_all(dst)?;
        for entry in std::fs::read_dir(src)? {
            let entry = entry?;
            let name = entry.file_name();
            copy_tree_impl(&src.join(&name), &dst.join(&name))?;
        }
    } else {
        if let Some(parent) = dst.parent() {
            std::fs::create_dir_all(parent)?;
        }
        std::fs::copy(src, dst)?;
    }
    Ok(())
}

fn builtin_copy_tree(args: Vec<Value>) -> MixResult<Option<Value>> {
    expect_args("copy_tree", &args, 2)?;
    let src = args[0].to_mix_string();
    let dst = args[1].to_mix_string();
    copy_tree_impl(std::path::Path::new(&src), std::path::Path::new(&dst)).map_err(|e| {
        MixError::RuntimeError {
            span: None,
            msg: format!("copy_tree '{}' -> '{}': {}", src, dst, e),
        }
    })?;
    Ok(Some(Value::Nil))
}

/// `rename(src, dst)` — rename(2). The point of exposing this rather than
/// leaving scripts to `copy` + `remove` is the guarantee copy cannot give:
/// replacing an existing `dst` is atomic, so a reader (or a `git` about to exec
/// a hook) sees either the old file or the new one and never a half-written
/// one. Errors are surfaced verbatim, including EXDEV — a cross-filesystem move
/// is a different operation with different failure modes, and quietly falling
/// back to copy+remove would hand back the non-atomicity the caller came here
/// to avoid.
fn builtin_rename(args: Vec<Value>) -> MixResult<Option<Value>> {
    expect_args("rename", &args, 2)?;
    let src = args[0].to_mix_string();
    let dst = args[1].to_mix_string();
    std::fs::rename(&src, &dst).map_err(|e| MixError::RuntimeError {
        span: None,
        msg: format!("rename '{}' -> '{}': {}", src, dst, e),
    })?;
    Ok(Some(Value::Nil))
}

/// `symlink(target, linkpath)` — symlink(2). Mix could already *see* symlinks
/// (`stat().is_symlink`, `read_link`, lstat via `follow_symlinks: false`) and
/// `copy_tree` recreates them, so the create side was the one asymmetry: a
/// script that had to make one had to shell out to `ln -s`. Note the argument
/// order is symlink(2)'s, not `ln`'s reading order — target first, the name to
/// create second — and `target` is stored verbatim, so a relative target is
/// resolved against the link's own directory and a dangling link is a legal
/// thing to create (both are frequently the point).
fn builtin_symlink(args: Vec<Value>) -> MixResult<Option<Value>> {
    expect_args("symlink", &args, 2)?;
    let target = args[0].to_mix_string();
    let linkpath = args[1].to_mix_string();
    std::os::unix::fs::symlink(&target, &linkpath).map_err(|e| MixError::RuntimeError {
        span: None,
        msg: format!("symlink '{}' -> '{}': {}", linkpath, target, e),
    })?;
    Ok(Some(Value::Nil))
}

/// `read_link(path)` — readlink(2). Deliberately NOT `realpath`: this returns
/// the stored target verbatim, so a relative or dangling link reads back as
/// what it literally says. That distinction is the reason to have it — code
/// that wants "where does this actually land" already has `realpath`, and code
/// auditing a link (is this temp file secretly aimed at the hook next to it?)
/// needs the unresolved answer.
fn builtin_read_link(args: Vec<Value>) -> MixResult<Option<Value>> {
    expect_args("read_link", &args, 1)?;
    let path = args[0].to_mix_string();
    let target = std::fs::read_link(&path).map_err(|e| MixError::RuntimeError {
        span: None,
        msg: format!("read_link '{}': {}", path, e),
    })?;
    Ok(Some(Value::String(target.to_string_lossy().into())))
}

fn builtin_remove(args: Vec<Value>) -> MixResult<Option<Value>> {
    expect_args("remove", &args, 1)?;
    let path = args[0].to_mix_string();
    match std::fs::remove_file(&path) {
        Ok(()) => Ok(Some(Value::Nil)),
        // rm -f semantics: a path that is already gone is a no-op, not an error.
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Some(Value::Nil)),
        Err(e) => Err(MixError::RuntimeError {
            span: None,
            msg: format!("remove '{}': {}", path, e),
        }),
    }
}

fn builtin_remove_dir(args: Vec<Value>) -> MixResult<Option<Value>> {
    expect_args("remove_dir", &args, 1)?;
    let path = args[0].to_mix_string();
    match std::fs::remove_dir_all(&path) {
        Ok(()) => Ok(Some(Value::Nil)),
        // rm -rf semantics: already gone is a no-op.
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Some(Value::Nil)),
        Err(e) => Err(MixError::RuntimeError {
            span: None,
            msg: format!("remove_dir '{}': {}", path, e),
        }),
    }
}

/// Parse a Mix mode argument (used by `chmod` and `write_new`) into
/// a Unix mode bitset. Accepts either:
/// - `Value::String("0600" | "750")` — read as octal directly
/// - `Value::Number(0o600)` — the mode VALUE; write it with an octal
///   literal, e.g. `chmod(p, 0o755)` (== mode 493). (Before Mix had octal
///   literals this arm read the decimal *digits* as octal, so `600`->0o600;
///   that hack is retired now that `0o600` is the real value — a number is
///   now just the value.) Out-of-range (>0o7777) and non-integer floats
///   (`600.9`) are rejected.
fn parse_octal_mode(callsite: &str, path: &str, v: &Value) -> MixResult<u32> {
    match v {
        Value::String(s) => {
            let t = s.trim().trim_start_matches('0');
            let t = if t.is_empty() { "0" } else { t };
            let m = u32::from_str_radix(t, 8).map_err(|e| MixError::RuntimeError {
                span: None,
                msg: format!("{} '{}': invalid octal mode '{}': {}", callsite, path, s, e),
            })?;
            // Same 12-bit ceiling as the Number arm — keep both input types
            // consistent (an octal string > 0o7777 is out of range too).
            if m > 0o7777 {
                return Err(MixError::RuntimeError {
                    span: None,
                    msg: format!(
                        "{} '{}': mode '{}' out of range (0..=0o7777)",
                        callsite, path, s
                    ),
                });
            }
            Ok(m)
        }
        Value::Number(n) => {
            // A NUMBER is the mode VALUE — write it with an octal literal:
            // `chmod(p, 0o755)`. (Before octal literals existed this arm read
            // the decimal DIGITS as octal, so `755`->0o755; that hack is gone
            // now that `0o755` is the real value 493. Octal STRINGS like
            // "0755" are still parsed as octal in the String arm above.) A
            // Unix mode is 12 bits: 0..=0o7777.
            Ok(as_exact_integer(&format!("{callsite} '{path}': mode"), *n, 0, 0o7777)? as u32)
        }
        other => Err(MixError::RuntimeError {
            span: None,
            msg: format!(
                "{} '{}': mode must be string or number, got {:?}",
                callsite, path, other
            ),
        }),
    }
}

/// `chmod($path, $mode)` — set permissions. Symlinks are followed
/// (deliberate, standard shell `chmod` semantics): the mode is applied
/// to the link's target, not the link itself.
fn builtin_chmod(args: Vec<Value>) -> MixResult<Option<Value>> {
    use std::os::unix::fs::PermissionsExt;
    expect_args("chmod", &args, 2)?;
    let path = args[0].to_mix_string();
    let mode_u32 = parse_octal_mode("chmod", &path, &args[1])?;
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(mode_u32)).map_err(|e| {
        MixError::RuntimeError {
            span: None,
            msg: format!("chmod '{}': {}", path, e),
        }
    })?;
    Ok(Some(Value::Nil))
}

/// `chown($path, $uid, $gid)` — set a path's owner and group by numeric
/// uid/gid. The natural sibling of the native `chmod`: numeric ids via
/// `std::os::unix::fs::chown` (stable std since 1.73), which follows
/// symlinks. Name resolution is intentionally out of scope — numeric
/// ids are what the substrate's per-mailbox chown-back needs, and a
/// name lookup is a separate concern.
fn builtin_chown(args: Vec<Value>) -> MixResult<Option<Value>> {
    expect_args("chown", &args, 3)?;
    let path = args[0].to_mix_string();
    let uid = parse_id("chown", "uid", &args[1])?;
    let gid = parse_id("chown", "gid", &args[2])?;
    std::os::unix::fs::chown(&path, Some(uid), Some(gid)).map_err(|e| MixError::RuntimeError {
        span: None,
        msg: format!("chown '{}': {}", path, e),
    })?;
    Ok(Some(Value::Nil))
}

/// Parse a numeric uid/gid argument: must be a finite, non-negative
/// integer within `u32` range (the libc id width). Requires a real
/// `Number` — `to_number()` is deliberately NOT used, because it coerces
/// a bool (`true`→1) and a numeric string, and silently turning
/// `chown(p, true, false)` into `uid=1, gid=0` on an ownership syscall
/// is a footgun.
fn parse_id(func: &str, what: &str, v: &Value) -> MixResult<u32> {
    let n = match extract_number(v, InputPolicy::NumberOnly) {
        Some(n) => n,
        None => {
            return Err(MixError::RuntimeError {
                span: None,
                msg: format!(
                    "{}(): {} must be a number, got {}",
                    func,
                    what,
                    v.type_name()
                ),
            });
        }
    };
    Ok(as_exact_integer(&format!("{func}(): {what}"), n, 0, u32::MAX as i64)? as u32)
}

/// `stat($path[, {follow_symlinks}])` — stat a path into a map.
///
/// Returns `{uid, gid, nlink, size, mode, perm, ino, dev, ctime,
/// mtime, atime, ctime_nsec, mtime_nsec, atime_nsec, is_file, is_dir,
/// is_symlink}`. The Python `os.stat`
/// answer for Mix, so `os.stat`-shaped code (per-entry uid/gid/ctime
/// reads) ports without a `find`/`stat(1)` shell-out.
///
/// Field notes (these are a frozen surface — scripts index into them):
/// - `ino` and `dev` are **strings**: they are `u64`, and Mix numbers
///   are `f64`, which loses integer precision above 2^53. An inode used
///   as a dedupe key must survive verbatim, so it is carried as text.
/// - `mode` is the full `st_mode` (type bits included); `perm` is the
///   permission subset `mode & 0o7777`, which round-trips into `chmod`.
/// - `ctime`/`mtime`/`atime` are epoch **seconds** as `f64`; the matching
///   `ctime_nsec`/`mtime_nsec`/`atime_nsec` carry the sub-second component
///   (`0..=999_999_999`) as a separate number (v0.44.0). A change check
///   compares the *pair* — whole seconds alone cannot see a rewrite that
///   replaces equal-length content inside one second. The two are kept
///   apart rather than combined into a nanosecond epoch because that value
///   passes 2^53 (the f64 exact-integer limit) about 104 days after 1970;
///   both halves of the pair are exact.
/// - `uid`/`gid`/`nlink`/`size` are numbers (all comfortably < 2^53 in
///   practice).
///
/// Follows symlinks by default (POSIX `stat`); `stat(path,
/// {follow_symlinks:false})` reports the link itself (POSIX `lstat`).
/// `is_symlink` is always derived from `symlink_metadata`, so it is true
/// for a symlink regardless of the follow mode — when following, the
/// other fields describe the *target* while `is_symlink` flags the path,
/// a deliberate mixed view that matches how callers reason about links.
fn builtin_stat(args: Vec<Value>) -> MixResult<Option<Value>> {
    use std::os::unix::fs::MetadataExt;
    if args.is_empty() || args.len() > 2 {
        return Err(MixError::RuntimeError {
            span: None,
            msg: format!("stat() expects 1 or 2 args, got {}", args.len()),
        });
    }
    let path = args[0].to_mix_string();

    let mut follow_symlinks = true;
    if let Some(opts) = args.get(1) {
        match opts {
            Value::Nil => {}
            Value::Map(m) => {
                if let Some(v) = m.get("follow_symlinks") {
                    follow_symlinks = v.is_truthy();
                }
            }
            other => {
                return Err(MixError::RuntimeError {
                    span: None,
                    msg: format!(
                        "stat(): options must be a map or nil, got {}",
                        other.type_name()
                    ),
                });
            }
        }
    }

    let meta = if follow_symlinks {
        std::fs::metadata(&path)
    } else {
        std::fs::symlink_metadata(&path)
    }
    .map_err(|e| MixError::RuntimeError {
        span: None,
        msg: format!("stat '{}': {}", path, e),
    })?;

    // `is_symlink` always reflects the path itself, even when following.
    // Best-effort: if the lstat fails for some reason we already have a
    // successful stat above, so fall back to "not a symlink".
    let is_symlink = std::fs::symlink_metadata(&path)
        .map(|m| m.file_type().is_symlink())
        .unwrap_or(false);

    let mode = meta.mode();
    let mut map = indexmap::IndexMap::new();
    map.insert("uid".to_string(), Value::Number(meta.uid() as f64));
    map.insert("gid".to_string(), Value::Number(meta.gid() as f64));
    map.insert("nlink".to_string(), Value::Number(meta.nlink() as f64));
    map.insert("size".to_string(), Value::Number(meta.size() as f64));
    map.insert("mode".to_string(), Value::Number(mode as f64));
    map.insert("perm".to_string(), Value::Number((mode & 0o7777) as f64));
    map.insert("ino".to_string(), Value::String(meta.ino().to_string()));
    map.insert("dev".to_string(), Value::String(meta.dev().to_string()));
    map.insert("ctime".to_string(), Value::Number(meta.ctime() as f64));
    map.insert("mtime".to_string(), Value::Number(meta.mtime() as f64));
    map.insert("atime".to_string(), Value::Number(meta.atime() as f64));
    // Sub-second components (`st_?tim.tv_nsec`, 0..=999_999_999).
    //
    // Whole seconds are not enough to answer "did this file change?", which
    // is what a tamper check actually asks: a rewrite that replaces equal
    // bytes inside the same second is invisible at second granularity. That
    // is not hypothetical — cmctl's public-hygiene suite was rewriting nine
    // live git hooks on every run and staying green, because the only
    // evidence was an mtime the interpreter could not see. It had to shell
    // out to GNU `stat --printf=%y` to get this field.
    //
    // Deliberately the sub-second component rather than a combined
    // nanosecond timestamp: Mix numbers are f64, exact only to 2^53, which a
    // full nanosecond epoch value exceeds about 104 days after 1970. Callers
    // compare the (mtime, mtime_nsec) pair, which is exact for both parts.
    // Emitting a single lossy number here would replace a visible gap with a
    // silent one.
    map.insert(
        "ctime_nsec".to_string(),
        Value::Number(meta.ctime_nsec() as f64),
    );
    map.insert(
        "mtime_nsec".to_string(),
        Value::Number(meta.mtime_nsec() as f64),
    );
    map.insert(
        "atime_nsec".to_string(),
        Value::Number(meta.atime_nsec() as f64),
    );
    map.insert("is_file".to_string(), Value::Bool(meta.is_file()));
    map.insert("is_dir".to_string(), Value::Bool(meta.is_dir()));
    map.insert("is_symlink".to_string(), Value::Bool(is_symlink));
    Ok(Some(Value::map(map)))
}

/// `write_new(path, content, mode)` — atomically create a new file at
/// `path` with the given Unix mode and write `content` to it. Fails if
/// `path` already exists (no TOCTOU between an exists() check and the
/// write, as `OpenOptions::create_new(true)` is `O_EXCL` at the syscall
/// level). The mode is applied at creation via `OpenOptions::mode()`,
/// so the file is never briefly umask-permissioned: secret material
/// hits disk at the configured mode from the very first byte.
///
/// Designed for DKIM key writes and similar single-shot secret files.
/// For mutable writes use `write_file`.
fn builtin_write_new(args: Vec<Value>) -> MixResult<Option<Value>> {
    use std::io::Write as _;
    use std::os::unix::fs::OpenOptionsExt;
    expect_args("write_new", &args, 3)?;
    let path = args[0].to_mix_string();
    let mode_u32 = parse_octal_mode("write_new", &path, &args[2])?;
    let mut f = std::fs::OpenOptions::new()
        .create_new(true)
        .write(true)
        .mode(mode_u32)
        .open(&path)
        .map_err(|e| MixError::RuntimeError {
            span: None,
            msg: format!("write_new '{}': {}", path, e),
        })?;
    // Mirror write_file/append_file: a `Value::Bytes` argument writes
    // raw bytes; anything else stringifies. write_new is used for DKIM
    // secrets / single-shot key material, so silently writing the
    // `<bytes:N>` placeholder would be a security-grade silent bug.
    let write_result = match &args[1] {
        Value::Bytes(buf) => f.write_all(buf),
        Value::Buffer(b) => f.write_all(b.borrow().as_slice()),
        other => f.write_all(other.to_mix_string().as_bytes()),
    };
    write_result.map_err(|e| MixError::RuntimeError {
        span: None,
        msg: format!("write_new '{}': {}", path, e),
    })?;
    Ok(Some(Value::Nil))
}

// --- JSON builtins (feature-gated) ---

/// `jq(value, filter)` — run a jq filter over a Mix value. Single-value
/// contract: 0 outputs → `nil`, 1 → that value (unwrapped), >1 → raise.
/// Array-returning filters (`map(...)`, `[ … ]`) emit ONE output and
/// belong here. Design: `src/_doc/mix/jq-builtin-design.md`.
///
/// Exact-2 arity: a trailing arg would otherwise be silently ignored
/// (same reasoning as `read_jsonl`'s bounded arity, not a bare
/// `expect_args` minimum).
#[cfg(feature = "json")]
fn builtin_jq(args: Vec<Value>) -> MixResult<Option<Value>> {
    if args.len() != 2 {
        return Err(MixError::RuntimeError {
            span: None,
            msg: format!(
                "jq() expects exactly 2 args (value, filter), got {}",
                args.len()
            ),
        });
    }
    let filter = args[1].to_mix_string();
    crate::jq::run_jq(&args[0], &filter, crate::jq::JqMode::Single).map(Some)
}

/// `jq_all(value, filter)` — run a jq filter, collect ALL outputs as a
/// list (the stream case: `.items[]`, `.[] | select(...)`). Always a
/// list: 0 outputs → `[]`. Exact-2 arity (see `jq`).
#[cfg(feature = "json")]
fn builtin_jq_all(args: Vec<Value>) -> MixResult<Option<Value>> {
    if args.len() != 2 {
        return Err(MixError::RuntimeError {
            span: None,
            msg: format!(
                "jq_all() expects exactly 2 args (value, filter), got {}",
                args.len()
            ),
        });
    }
    let filter = args[1].to_mix_string();
    crate::jq::run_jq(&args[0], &filter, crate::jq::JqMode::All).map(Some)
}

/// `data_encode(value, [pretty])` — serialize a Mix value to strict-data
/// `.conf.mix` source text. The inverse of [`builtin_data_parse`];
/// the round-trip `data_parse(data_encode(v)) == v` holds for any
/// data-shaped tree.
///
/// This is the builtin behind hand-writing a `default.conf.mix`: it
/// emits the exact `\$` / `\~` / `\\` / `\n` escaping the strict-data
/// lexer round-trips (see `value::write_data_string`), so a script no
/// longer has to reason about — e.g. — a regex ending in `$` needing
/// `\$`, or `\\` collapsing. A value with no strict-data form
/// (non-finite number, `Function`, `Bytes`) raises rather than emits
/// silently-lossy text.
///
/// The optional second arg (truthy) selects a multi-line, 2-space
/// indented layout for human-readable generated config; it parses back
/// identically. Mirrors `json_encode`'s positional `pretty` flag.
fn builtin_data_encode(args: Vec<Value>) -> MixResult<Option<Value>> {
    expect_args("data_encode", &args, 1)?;
    let pretty = args.get(1).map(|v| v.is_truthy()).unwrap_or(false);
    let result = if pretty {
        args[0].to_mix_data_string_pretty()
    } else {
        args[0].to_mix_data_string()
    };
    let s = result.map_err(|e| MixError::RuntimeError {
        span: None,
        msg: format!("data_encode: {e}"),
    })?;
    Ok(Some(Value::String(s)))
}

/// `data_parse(string)` — parse a strict-data `.conf.mix` string into a
/// Mix value. The inverse of [`builtin_data_encode`]; the same
/// strict-data grammar the daemon-side serde reader uses, exposed to
/// scripts so a config written by `data_encode` can be read back.
fn builtin_data_parse(args: Vec<Value>) -> MixResult<Option<Value>> {
    expect_args("data_parse", &args, 1)?;
    let s = args[0].to_mix_string();
    let val = crate::parse_data(&s).map_err(|e| MixError::RuntimeError {
        span: None,
        msg: format!("data_parse: {e}"),
    })?;
    Ok(Some(val))
}

#[cfg(feature = "json")]
fn builtin_json_parse(args: Vec<Value>) -> MixResult<Option<Value>> {
    expect_args("json_parse", &args, 1)?;
    let s = args[0].to_mix_string();
    let json_val: serde_json::Value =
        serde_json::from_str(&s).map_err(|e| MixError::RuntimeError {
            span: None,
            msg: format!("json_parse: {}", e),
        })?;
    Ok(Some(crate::json::json_to_mix(json_val)))
}

/// `read_json($path)` — parse a single-record JSON file into a Mix
/// value. Thin wrapper over `read_file` + `json_parse` that avoids
/// the two-step pattern in every script.
#[cfg(feature = "json")]
fn builtin_read_json(args: Vec<Value>) -> MixResult<Option<Value>> {
    expect_args("read_json", &args, 1)?;
    let path = args[0].to_mix_string();
    let content = std::fs::read_to_string(&path).map_err(|e| MixError::RuntimeError {
        span: None,
        msg: format!("read_json '{}': {}", path, e),
    })?;
    let json_val: serde_json::Value =
        serde_json::from_str(&content).map_err(|e| MixError::RuntimeError {
            span: None,
            msg: format!("read_json '{}': parse error: {}", path, e),
        })?;
    Ok(Some(crate::json::json_to_mix(json_val)))
}

/// `read_jsonl($path, $opts = nil)` — parse a JSON-lines file. Each
/// non-empty line is parsed as an independent JSON record; the
/// result is a list. Strict by default: a single malformed line
/// aborts the read. Pass `{skip_errors: true}` for lenient mode
/// (malformed lines silently dropped).
///
/// The `spamlite-threshold-digest` script reads dozens of
/// `.spamlite-stats.jsonl` files at a time across `/srv/*/msg/*/`;
/// strict-by-default keeps a single bad file from silently
/// distorting aggregated stats, and the opt-in lenient mode is the
/// escape hatch when the script owner knows rotation can leave
/// truncated tail lines.
#[cfg(feature = "json")]
fn builtin_read_jsonl(args: Vec<Value>) -> MixResult<Option<Value>> {
    if args.is_empty() || args.len() > 2 {
        return Err(MixError::RuntimeError {
            span: None,
            msg: format!("read_jsonl() expects 1 or 2 args, got {}", args.len()),
        });
    }
    let path = args[0].to_mix_string();
    let skip_errors = match args.get(1) {
        None | Some(Value::Nil) => false,
        Some(Value::Map(m)) => m.get("skip_errors").map(|v| v.is_truthy()).unwrap_or(false),
        Some(other) => {
            return Err(MixError::RuntimeError {
                span: None,
                msg: format!(
                    "read_jsonl(): options must be a map or nil, got {}",
                    other.type_name()
                ),
            });
        }
    };

    let content = std::fs::read_to_string(&path).map_err(|e| MixError::RuntimeError {
        span: None,
        msg: format!("read_jsonl '{}': {}", path, e),
    })?;

    let mut records = Vec::new();
    for (line_no, line) in content.lines().enumerate() {
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }
        match serde_json::from_str::<serde_json::Value>(trimmed) {
            Ok(json_val) => records.push(crate::json::json_to_mix(json_val)),
            Err(e) => {
                if skip_errors {
                    continue;
                }
                return Err(MixError::RuntimeError {
                    span: None,
                    msg: format!("read_jsonl '{}' line {}: {}", path, line_no + 1, e),
                });
            }
        }
    }
    Ok(Some(Value::list(records)))
}

#[cfg(feature = "json")]
fn builtin_json_encode(args: Vec<Value>) -> MixResult<Option<Value>> {
    expect_args("json_encode", &args, 1)?;
    // Loud on non-finite numbers (NaN/±inf → error, never a silent 0),
    // matching the jq()/strict-data policy.
    let json_val = crate::json::mix_to_json(&args[0]).map_err(|e| MixError::RuntimeError {
        span: None,
        msg: format!("json_encode: {}", e),
    })?;
    let pretty = args.get(1).map(|v| v.is_truthy()).unwrap_or(false);
    let s = if pretty {
        serde_json::to_string_pretty(&json_val)
    } else {
        serde_json::to_string(&json_val)
    }
    .map_err(|e| MixError::RuntimeError {
        span: None,
        msg: format!("json_encode: {}", e),
    })?;
    Ok(Some(Value::String(s)))
}

// --- Regex builtins (feature-gated) ---

#[cfg(feature = "regex")]
fn compile_regex(pattern: &str) -> MixResult<regex::Regex> {
    regex::Regex::new(pattern).map_err(|e| {
        // A swapped call (the SUBJECT passed as the pattern) hands a whole
        // document to the compiler, and both the echo and the regex
        // crate's own report then contain the full text — a 9 KB roster
        // printed ~280 lines before the actual complaint (2026-09-03).
        // For a long or multi-line pattern: truncate the echo, keep only
        // the crate's final "error: …" line (never contains the input),
        // and name the usual cause. Short patterns keep the full report.
        const MAX_ECHO: usize = 80;
        let suspicious = pattern.chars().count() > MAX_ECHO || pattern.contains('\n');
        let msg = if suspicious {
            let head: String = pattern.chars().take(MAX_ECHO).collect();
            let head = head.replace('\n', "\\n");
            let err = e.to_string();
            let last = err
                .lines()
                .rev()
                .find(|l| l.starts_with("error:"))
                .unwrap_or("regex parse error");
            format!(
                "invalid regex '{head}…' ({} chars, truncated): {last}\n  \
                 (argument 1 is the PATTERN — the subject string comes after \
                 it; a swapped argument order is the usual cause of a huge \
                 pattern: see mix man regex)",
                pattern.chars().count()
            )
        } else {
            format!("invalid regex '{pattern}': {e}")
        };
        MixError::RuntimeError { span: None, msg }
    })
}

/// regex_match(pattern, text) — test if text matches pattern (anywhere in text).
#[cfg(feature = "regex")]
fn builtin_regex_match(args: Vec<Value>) -> MixResult<Option<Value>> {
    expect_args("regex_match", &args, 2)?;
    let re = compile_regex(&args[0].to_mix_string())?;
    Ok(Some(Value::Bool(re.is_match(&args[1].to_mix_string()))))
}

/// regex_find(pattern, text) — return list of all matches.
/// Each match is a map with {match, start, end} (or {match, start, end, groups: [...]}).
#[cfg(feature = "regex")]
fn builtin_regex_find(args: Vec<Value>) -> MixResult<Option<Value>> {
    expect_args("regex_find", &args, 2)?;
    let re = compile_regex(&args[0].to_mix_string())?;
    let text = args[1].to_mix_string();
    let mut results = Vec::new();
    for caps in re.captures_iter(&text) {
        let m = caps.get(0).unwrap();
        let mut entry = indexmap::IndexMap::new();
        entry.insert("match".into(), Value::String(m.as_str().to_string()));
        entry.insert("start".into(), Value::Number(m.start() as f64));
        entry.insert("end".into(), Value::Number(m.end() as f64));
        // Include capture groups if any (beyond group 0)
        if caps.len() > 1 {
            let groups: Vec<Value> = (1..caps.len())
                .map(|i| match caps.get(i) {
                    Some(g) => Value::String(g.as_str().to_string()),
                    None => Value::Nil,
                })
                .collect();
            entry.insert("groups".into(), Value::list(groups));
        }
        results.push(Value::map(entry));
    }
    Ok(Some(Value::list(results)))
}

/// regex_replace(pattern, text, replacement) — replace all matches.
/// Supports $1, $2 backreferences in replacement string.
#[cfg(feature = "regex")]
fn builtin_regex_replace(args: Vec<Value>) -> MixResult<Option<Value>> {
    expect_args("regex_replace", &args, 3)?;
    let re = compile_regex(&args[0].to_mix_string())?;
    let text = args[1].to_mix_string();
    let replacement = args[2].to_mix_string();
    Ok(Some(Value::String(
        re.replace_all(&text, replacement.as_str()).into_owned(),
    )))
}

// --- Subject-first regex family (0.63.0). Same engine as the legacy
// pattern-first regex_* names; the argument order matches every literal-
// string builtin (subject first). re_find additionally returns CODEPOINT
// offsets so its start/end compose with substr/slice/index_of — the
// legacy regex_find keeps raw UTF-8 byte offsets until it is deleted.

/// re_match(s, pattern) — subject-first regex test.
#[cfg(feature = "regex")]
fn builtin_re_match(args: Vec<Value>) -> MixResult<Option<Value>> {
    expect_args("re_match", &args, 2)?;
    let re = compile_regex(&args[1].to_mix_string())?;
    Ok(Some(Value::Bool(re.is_match(&args[0].to_mix_string()))))
}

/// re_find(s, pattern) — all matches, {match, start, end[, groups]} in
/// CODEPOINT offsets.
#[cfg(feature = "regex")]
fn builtin_re_find(args: Vec<Value>) -> MixResult<Option<Value>> {
    expect_args("re_find", &args, 2)?;
    let text = args[0].to_mix_string();
    let re = compile_regex(&args[1].to_mix_string())?;
    let mut results = Vec::new();
    // Codepoint offsets, counted INCREMENTALLY: matches arrive in byte
    // order, so advance a (byte, codepoint) cursor instead of recounting
    // the prefix per match — a match-heavy large subject stays O(n), not
    // O(n·matches). Match bounds are char boundaries, so slices are valid.
    let mut cp_cursor = 0usize;
    let mut byte_cursor = 0usize;
    for caps in re.captures_iter(&text) {
        let m = caps.get(0).unwrap();
        cp_cursor += text[byte_cursor..m.start()].chars().count();
        let start_cp = cp_cursor;
        cp_cursor += text[m.start()..m.end()].chars().count();
        byte_cursor = m.end();
        let end_cp = cp_cursor;
        let mut entry = indexmap::IndexMap::new();
        entry.insert("match".into(), Value::String(m.as_str().to_string()));
        entry.insert("start".into(), Value::Number(start_cp as f64));
        entry.insert("end".into(), Value::Number(end_cp as f64));
        if caps.len() > 1 {
            let groups: Vec<Value> = (1..caps.len())
                .map(|i| match caps.get(i) {
                    Some(g) => Value::String(g.as_str().to_string()),
                    None => Value::Nil,
                })
                .collect();
            entry.insert("groups".into(), Value::list(groups));
        }
        results.push(Value::map(entry));
    }
    Ok(Some(Value::list(results)))
}

/// re_replace(s, pattern, replacement) — replace all matches, subject first.
#[cfg(feature = "regex")]
fn builtin_re_replace(args: Vec<Value>) -> MixResult<Option<Value>> {
    expect_args("re_replace", &args, 3)?;
    let text = args[0].to_mix_string();
    let re = compile_regex(&args[1].to_mix_string())?;
    let replacement = args[2].to_mix_string();
    Ok(Some(Value::String(
        re.replace_all(&text, replacement.as_str()).into_owned(),
    )))
}

/// re_split(s, pattern) — split s on each match, subject first.
#[cfg(feature = "regex")]
fn builtin_re_split(args: Vec<Value>) -> MixResult<Option<Value>> {
    expect_args("re_split", &args, 2)?;
    let text = args[0].to_mix_string();
    let re = compile_regex(&args[1].to_mix_string())?;
    let parts: Vec<Value> = re
        .split(&text)
        .map(|s| Value::String(s.to_string()))
        .collect();
    Ok(Some(Value::list(parts)))
}

/// regex_split(pattern, text) — split text on regex pattern.
#[cfg(feature = "regex")]
fn builtin_regex_split(args: Vec<Value>) -> MixResult<Option<Value>> {
    expect_args("regex_split", &args, 2)?;
    let re = compile_regex(&args[0].to_mix_string())?;
    let text = args[1].to_mix_string();
    let parts: Vec<Value> = re
        .split(&text)
        .map(|s| Value::String(s.to_string()))
        .collect();
    Ok(Some(Value::list(parts)))
}

// --- TOML builtins (feature-gated) ---

#[cfg(feature = "toml")]
fn builtin_toml_parse(args: Vec<Value>) -> MixResult<Option<Value>> {
    expect_args("toml_parse", &args, 1)?;
    let s = args[0].to_mix_string();
    let val: toml::Value = s.parse().map_err(|e| MixError::RuntimeError {
        span: None,
        msg: format!("toml_parse: {e}"),
    })?;
    Ok(Some(toml_to_mix(val)))
}

#[cfg(feature = "toml")]
fn builtin_toml_encode(args: Vec<Value>) -> MixResult<Option<Value>> {
    expect_args("toml_encode", &args, 1)?;
    let val = mix_to_toml(&args[0], "$")?;
    let s = toml::to_string_pretty(&val).map_err(|e| MixError::RuntimeError {
        span: None,
        msg: format!("toml_encode: {e}"),
    })?;
    Ok(Some(Value::String(s)))
}

#[cfg(feature = "toml")]
fn toml_to_mix(val: toml::Value) -> Value {
    match val {
        toml::Value::String(s) => Value::String(s),
        toml::Value::Integer(n) => Value::Number(n as f64),
        toml::Value::Float(f) => Value::Number(f),
        toml::Value::Boolean(b) => Value::Bool(b),
        toml::Value::Array(arr) => Value::list(arr.into_iter().map(toml_to_mix).collect()),
        toml::Value::Table(map) => {
            let m: indexmap::IndexMap<String, Value> =
                map.into_iter().map(|(k, v)| (k, toml_to_mix(v))).collect();
            Value::map(m)
        }
        toml::Value::Datetime(dt) => Value::String(dt.to_string()),
    }
}

#[cfg(feature = "toml")]
fn mix_to_toml(val: &Value, path: &str) -> MixResult<toml::Value> {
    Ok(match val {
        Value::String(s) => toml::Value::String(s.clone()),
        Value::Number(n) => {
            // Exclusive upper bound (the json.rs predicate): at exactly 2^63
            // the round-trip check `n == (n as i64) as f64` PASSES because the
            // saturating cast and `i64::MAX as f64`'s round-up cancel — the
            // old form silently emitted Integer(i64::MAX) for a value one past
            // it. Same trap as the ssh env fix in this release.
            if *n == n.floor() && *n >= i64::MIN as f64 && *n < i64::MAX as f64 {
                toml::Value::Integer(*n as i64)
            } else {
                toml::Value::Float(*n)
            }
        }
        Value::Bool(b) => toml::Value::Boolean(*b),
        Value::List(arr) => toml::Value::Array(
            arr.iter()
                .enumerate()
                .map(|(index, value)| mix_to_toml(value, &format!("{path}[{index}]")))
                .collect::<MixResult<Vec<_>>>()?,
        ),
        Value::Map(map) => {
            let t: toml::map::Map<String, toml::Value> = map
                .iter()
                .map(|(key, value)| {
                    let child_path = validate_join_path(path, key);
                    Ok((key.clone(), mix_to_toml(value, &child_path)?))
                })
                .collect::<MixResult<_>>()?;
            toml::Value::Table(t)
        }
        Value::Nil | Value::Function(_) | Value::Bytes(_) | Value::Buffer(_) => {
            let value_type = val.type_name();
            let mut details = indexmap::IndexMap::new();
            details.insert("path".to_string(), Value::String(path.to_string()));
            details.insert("type".to_string(), Value::String(value_type.to_string()));
            return Err(MixError::Structured(Box::new(
                crate::error::ErrorInfo::new(
                    "TOML_UNREPRESENTABLE",
                    format!(
                        "toml_encode: value at {path} has type {value_type}, which has no TOML representation"
                    ),
                )
                .with_details(Value::map(details)),
            )));
        }
    })
}

// --- Date/time builtins (feature-gated) ---

#[cfg(feature = "datetime")]
fn builtin_date_format(args: Vec<Value>) -> MixResult<Option<Value>> {
    expect_args("date_format", &args, 1)?;
    // datetime.md: "sub-second precision is dropped on format" -- fractional
    // timestamps TRUNCATE (date_format(time()) is the canonical composition;
    // time() returns fractional seconds). The domain gate still refuses
    // non-finite and out-of-range, which the same page documents as
    // VALUE_OUT_OF_RANGE.
    let ts = as_timestamp(
        "date_format(): argument 1",
        number_arg("date_format", &args, 0)?.trunc(),
    )?;
    let fmt = args
        .get(1)
        .map(|v| v.to_mix_string())
        .unwrap_or_else(|| "%Y-%m-%d %H:%M:%S".into());
    let dt = chrono::DateTime::from_timestamp(ts, 0).ok_or_else(|| {
        MixError::structured(
            "VALUE_OUT_OF_RANGE",
            format!("date_format(): argument 1 timestamp {ts} is outside the supported range"),
        )
    })?;
    let local = dt.with_timezone(&chrono::Local);
    Ok(Some(Value::String(local.format(&fmt).to_string())))
}

#[cfg(feature = "datetime")]
fn builtin_date_parse(args: Vec<Value>) -> MixResult<Option<Value>> {
    expect_args("date_parse", &args, 1)?;
    let s = args[0].to_mix_string();
    // Try ISO 8601 first, then common formats
    if let Ok(dt) = chrono::DateTime::parse_from_rfc3339(&s) {
        return Ok(Some(Value::Number(dt.timestamp() as f64)));
    }
    if let Ok(dt) = chrono::NaiveDateTime::parse_from_str(&s, "%Y-%m-%d %H:%M:%S") {
        return Ok(Some(Value::Number(dt.and_utc().timestamp() as f64)));
    }
    if let Ok(d) = chrono::NaiveDate::parse_from_str(&s, "%Y-%m-%d") {
        let dt = d.and_hms_opt(0, 0, 0).unwrap();
        return Ok(Some(Value::Number(dt.and_utc().timestamp() as f64)));
    }
    Err(MixError::RuntimeError {
        span: None,
        msg: format!("date_parse: cannot parse '{s}'"),
    })
}

#[cfg(feature = "datetime")]
fn builtin_now_iso(_args: Vec<Value>) -> MixResult<Option<Value>> {
    Ok(Some(Value::String(chrono::Local::now().to_rfc3339())))
}

#[cfg(feature = "datetime")]
fn builtin_duration_format(args: Vec<Value>) -> MixResult<Option<Value>> {
    expect_args("duration_format", &args, 1)?;
    // datetime.md documents BOTH lenient edges: "a float is truncated to
    // whole seconds" and "a negative input clamps to 0s". Only non-finite
    // and beyond-2^53 inputs raise — those were garbage on main (inf
    // saturated to a 213-million-year uptime), not documented behaviour.
    // (Not `.max(0.0)`: f64::max returns the OTHER operand for NaN, which
    // would silently turn NaN into 0s. And the clamp takes only FINITE
    // negatives: bare `< 0.0` also matches -inf, which must reach the
    // domain gate and raise like NaN and +inf do -- round-2 review.)
    let raw = number_arg("duration_format", &args, 0)?;
    let clamped = if raw.is_finite() && raw < 0.0 {
        0.0
    } else {
        raw.trunc()
    };
    let mut secs = as_count("duration_format(): argument 1", clamped, usize::MAX)? as u64;
    let days = secs / 86400;
    secs %= 86400;
    let hours = secs / 3600;
    secs %= 3600;
    let mins = secs / 60;
    secs %= 60;
    let mut parts = Vec::new();
    if days > 0 {
        parts.push(format!("{days}d"));
    }
    if hours > 0 {
        parts.push(format!("{hours}h"));
    }
    if mins > 0 {
        parts.push(format!("{mins}m"));
    }
    if secs > 0 || parts.is_empty() {
        parts.push(format!("{secs}s"));
    }
    Ok(Some(Value::String(parts.join(" "))))
}

#[cfg(feature = "datetime")]
fn builtin_relative_time(args: Vec<Value>) -> MixResult<Option<Value>> {
    expect_args("relative_time", &args, 1)?;
    // Same documented truncation as date_format above.
    let ts = as_timestamp(
        "relative_time(): argument 1",
        number_arg("relative_time", &args, 0)?.trunc(),
    )?;
    let now = chrono::Utc::now().timestamp();
    let diff = now - ts;
    let s = if diff < 0 {
        let d = -diff;
        if d < 60 {
            format!("in {d}s")
        } else if d < 3600 {
            format!("in {}m", d / 60)
        } else if d < 86400 {
            format!("in {}h", d / 3600)
        } else {
            format!("in {}d", d / 86400)
        }
    } else if diff < 60 {
        format!("{diff}s ago")
    } else if diff < 3600 {
        format!("{}m ago", diff / 60)
    } else if diff < 86400 {
        format!("{}h ago", diff / 3600)
    } else {
        format!("{}d ago", diff / 86400)
    };
    Ok(Some(Value::String(s)))
}

// --- Path builtins (stdlib) ---

fn builtin_basename(args: Vec<Value>) -> MixResult<Option<Value>> {
    expect_args("basename", &args, 1)?;
    let p = args[0].to_mix_string();
    let name = std::path::Path::new(&p)
        .file_name()
        .map(|n| n.to_string_lossy().to_string())
        .unwrap_or_default();
    Ok(Some(Value::String(name)))
}

fn builtin_dirname(args: Vec<Value>) -> MixResult<Option<Value>> {
    expect_args("dirname", &args, 1)?;
    let p = args[0].to_mix_string();
    let dir = std::path::Path::new(&p)
        .parent()
        .map(|d| d.to_string_lossy().to_string())
        .unwrap_or_default();
    Ok(Some(Value::String(dir)))
}

fn builtin_extname(args: Vec<Value>) -> MixResult<Option<Value>> {
    expect_args("extname", &args, 1)?;
    let p = args[0].to_mix_string();
    let ext = std::path::Path::new(&p)
        .extension()
        .map(|e| format!(".{}", e.to_string_lossy()))
        .unwrap_or_default();
    Ok(Some(Value::String(ext)))
}

fn builtin_path_join(args: Vec<Value>) -> MixResult<Option<Value>> {
    expect_args("path_join", &args, 2)?;
    let result = std::path::Path::new(&args[0].to_mix_string()).join(args[1].to_mix_string());
    Ok(Some(Value::String(result.to_string_lossy().to_string())))
}

/// `path_parts($path)` — decompose a path into `{dir, base, stem, ext}`.
/// Pure, no filesystem access. `ext` is the extension WITHOUT the
/// leading dot (differs from `extname` which keeps the dot — they're
/// different consumers).
fn builtin_path_parts(args: Vec<Value>) -> MixResult<Option<Value>> {
    expect_args("path_parts", &args, 1)?;
    let p = args[0].to_mix_string();
    let path = std::path::Path::new(&p);
    let dir = path
        .parent()
        .map(|d| d.to_string_lossy().to_string())
        .unwrap_or_default();
    let base = path
        .file_name()
        .map(|b| b.to_string_lossy().to_string())
        .unwrap_or_default();
    let stem = path
        .file_stem()
        .map(|s| s.to_string_lossy().to_string())
        .unwrap_or_default();
    let ext = path
        .extension()
        .map(|e| e.to_string_lossy().to_string())
        .unwrap_or_default();
    let mut map = indexmap::IndexMap::new();
    map.insert("dir".to_string(), Value::String(dir));
    map.insert("base".to_string(), Value::String(base));
    map.insert("stem".to_string(), Value::String(stem));
    map.insert("ext".to_string(), Value::String(ext));
    Ok(Some(Value::map(map)))
}

/// `walk($dir, $opts = nil)` — recursive directory walk. Returns a
/// flat list of paths under `$dir`. Default mode returns files only;
/// an options map can flip defaults:
///
/// ```mix
/// walk("/srv/mail")
/// walk("/srv/mail", {max_depth: 3})
/// walk("/srv/mail", {follow_symlinks: true, include_dirs: true})
/// ```
///
/// Options (all optional):
/// - `follow_symlinks` (bool, default `false`): follow symlink dirs.
///   When true, the walker tracks visited inodes to break loops.
/// - `max_depth` (number, default unlimited): max nesting depth
///   relative to `$dir`. `max_depth: 0` returns only direct children.
/// - `include_dirs` (bool, default `false`): include directory
///   entries in the output list (not just files).
///
/// Errors on an unreadable top-level `$dir`. Silently skips
/// individual entries that fail to stat — a single bad file in a
/// subtree shouldn't abort the whole walk (digest scripts doing
/// `walk(/srv)` would fail constantly otherwise).
fn builtin_walk(args: Vec<Value>) -> MixResult<Option<Value>> {
    if args.is_empty() || args.len() > 2 {
        return Err(MixError::RuntimeError {
            span: None,
            msg: format!("walk() expects 1 or 2 args, got {}", args.len()),
        });
    }
    let root = args[0].to_mix_string();

    let mut follow_symlinks = false;
    let mut max_depth: Option<usize> = None;
    let mut include_dirs = false;

    if let Some(opts) = args.get(1) {
        match opts {
            Value::Nil => {}
            Value::Map(m) => {
                if let Some(v) = m.get("follow_symlinks") {
                    follow_symlinks = v.is_truthy();
                }
                if let Some(v) = m.get("max_depth") {
                    max_depth = Some(as_count(
                        "walk(): argument 2 option max_depth",
                        required_number_value("walk(): argument 2 option max_depth", v)?,
                        usize::MAX,
                    )?);
                }
                if let Some(v) = m.get("include_dirs") {
                    include_dirs = v.is_truthy();
                }
            }
            other => {
                return Err(MixError::RuntimeError {
                    span: None,
                    msg: format!(
                        "walk(): options must be a map or nil, got {}",
                        other.type_name()
                    ),
                });
            }
        }
    }

    let root_path = std::path::PathBuf::from(&root);
    if !root_path.exists() {
        return Err(MixError::RuntimeError {
            span: None,
            msg: format!("walk(): '{}' does not exist", root),
        });
    }

    let mut visited: std::collections::HashSet<(u64, u64)> = std::collections::HashSet::new();
    let mut out: Vec<Value> = Vec::new();

    walk_recursive(
        &root_path,
        0,
        max_depth,
        follow_symlinks,
        include_dirs,
        &mut visited,
        &mut out,
    );

    out.sort_by_key(|a| a.to_mix_string());
    Ok(Some(Value::list(out)))
}

fn walk_recursive(
    dir: &std::path::Path,
    depth: usize,
    max_depth: Option<usize>,
    follow_symlinks: bool,
    include_dirs: bool,
    visited: &mut std::collections::HashSet<(u64, u64)>,
    out: &mut Vec<Value>,
) {
    let entries = match std::fs::read_dir(dir) {
        Ok(e) => e,
        // Unreadable subdirectory — skip silently. Top-level
        // unreadable errors are caught by the exists() check in
        // the caller before we get here.
        Err(_) => return,
    };

    for entry in entries.flatten() {
        let path = entry.path();
        let metadata = if follow_symlinks {
            match std::fs::metadata(&path) {
                Ok(m) => m,
                Err(_) => continue,
            }
        } else {
            match std::fs::symlink_metadata(&path) {
                Ok(m) => m,
                Err(_) => continue,
            }
        };

        let is_dir = metadata.is_dir();

        if !is_dir || include_dirs {
            out.push(Value::String(path.to_string_lossy().into_owned()));
        }

        if is_dir {
            // Depth check: max_depth == Some(0) means "direct
            // children only" — don't descend further.
            let can_descend = match max_depth {
                Some(max) => depth < max,
                None => true,
            };
            if !can_descend {
                continue;
            }

            // Symlink loop protection: only active when following
            // symlinks, since without follow_symlinks we never
            // recurse through them anyway. Track (dev, inode) via
            // os-specific metadata; skip on non-unix.
            #[cfg(unix)]
            {
                use std::os::unix::fs::MetadataExt;
                let key = (metadata.dev(), metadata.ino());
                if follow_symlinks && !visited.insert(key) {
                    continue;
                }
            }

            walk_recursive(
                &path,
                depth + 1,
                max_depth,
                follow_symlinks,
                include_dirs,
                visited,
                out,
            );
        }
    }
}

// --- System builtins (stdlib) ---

fn builtin_hostname(_args: Vec<Value>) -> MixResult<Option<Value>> {
    let name = std::fs::read_to_string("/etc/hostname")
        .unwrap_or_else(|_| "unknown".into())
        .trim()
        .to_string();
    Ok(Some(Value::String(name)))
}

fn builtin_cwd(_args: Vec<Value>) -> MixResult<Option<Value>> {
    let dir = std::env::current_dir()
        .map(|p| p.to_string_lossy().to_string())
        .unwrap_or_default();
    Ok(Some(Value::String(dir)))
}

fn builtin_chdir(args: Vec<Value>) -> MixResult<Option<Value>> {
    expect_args("chdir", &args, 1)?;
    let path = args[0].to_mix_string();
    std::env::set_current_dir(&path).map_err(|e| MixError::RuntimeError {
        span: None,
        msg: format!("chdir '{}': {}", path, e),
    })?;
    Ok(Some(Value::Nil))
}

fn builtin_platform(_args: Vec<Value>) -> MixResult<Option<Value>> {
    let mut map = indexmap::IndexMap::new();
    map.insert("os".into(), Value::String(std::env::consts::OS.into()));
    map.insert("arch".into(), Value::String(std::env::consts::ARCH.into()));
    Ok(Some(Value::map(map)))
}

/// `which` answers "can I run this?", so it must ask the kernel the same
/// question `access()` does — not `is_file()`, which said yes to any regular
/// file on PATH whether or not a single execute bit was set. A caller that
/// branched on `which("foo") != nil` and then ran `foo` got a spawn failure
/// from a probe whose whole job was to prevent one. Both halves are needed:
/// `X_OK` alone is true for a *searchable directory*, so a PATH entry holding
/// a directory named `git` would otherwise be returned as the git binary.
fn builtin_which(args: Vec<Value>) -> MixResult<Option<Value>> {
    expect_args("which", &args, 1)?;
    let cmd = match &args[0] {
        Value::String(s) => s.clone(),
        other => {
            return Err(MixError::structured(
                "TYPE_MISMATCH",
                format!(
                    "which: cmd must be a string, got {} (no coercion — encode explicitly)",
                    other.type_name()
                ),
            ));
        }
    };
    let path_var = std::env::var("PATH").unwrap_or_default();
    for dir in path_var.split(':') {
        let full = std::path::Path::new(dir).join(&cmd);
        if full.is_file() && access_ok("which", &full.to_string_lossy(), libc::X_OK)? {
            return Ok(Some(Value::String(full.to_string_lossy().to_string())));
        }
    }
    Ok(Some(Value::Nil))
}

// --- Formatting builtins (stdlib) ---

fn builtin_format_bytes(args: Vec<Value>) -> MixResult<Option<Value>> {
    expect_args("format_bytes", &args, 1)?;
    let n = number_arg("format_bytes", &args, 0)?;
    let s = if n < 1024.0 {
        format!("{} B", n as u64)
    } else if n < 1024.0 * 1024.0 {
        format!("{:.1} KB", n / 1024.0)
    } else if n < 1024.0 * 1024.0 * 1024.0 {
        format!("{:.1} MB", n / (1024.0 * 1024.0))
    } else if n < 1024.0 * 1024.0 * 1024.0 * 1024.0 {
        format!("{:.1} GB", n / (1024.0 * 1024.0 * 1024.0))
    } else {
        format!("{:.2} TB", n / (1024.0 * 1024.0 * 1024.0 * 1024.0))
    };
    Ok(Some(Value::String(s)))
}

fn builtin_format_number(args: Vec<Value>) -> MixResult<Option<Value>> {
    expect_args("format_number", &args, 1)?;
    let n = number_arg("format_number", &args, 0)?;
    let decimals = if args.len() > 1 {
        as_count(
            "format_number(): argument 2",
            number_arg("format_number", &args, 1)?,
            308,
        )?
    } else {
        0
    };
    let formatted = format!("{:.prec$}", n, prec = decimals);
    // Add thousands separators to the integer part
    let parts: Vec<&str> = formatted.splitn(2, '.').collect();
    let int_part = parts[0];
    let negative = int_part.starts_with('-');
    let digits: String = if negative {
        int_part[1..].to_string()
    } else {
        int_part.to_string()
    };
    let with_commas: String = digits
        .as_bytes()
        .rchunks(3)
        .rev()
        .map(|chunk| std::str::from_utf8(chunk).unwrap())
        .collect::<Vec<_>>()
        .join(",");
    let result = if negative {
        format!("-{}", with_commas)
    } else {
        with_commas
    };
    if parts.len() > 1 {
        Ok(Some(Value::String(format!("{}.{}", result, parts[1]))))
    } else {
        Ok(Some(Value::String(result)))
    }
}

// --- Template & text builtins (stdlib) ---

/// `template($tmpl, $map)` — substitute single-brace `{key}` placeholders
/// (NOT `{{key}}`) with the map's values via `to_mix_string`.
fn builtin_template(args: Vec<Value>) -> MixResult<Option<Value>> {
    expect_args("template", &args, 2)?;
    let tmpl = args[0].to_mix_string();
    let vars = match &args[1] {
        Value::Map(m) => m,
        _ => {
            return Err(MixError::RuntimeError {
                span: None,
                msg: "template: second argument must be a map".into(),
            });
        }
    };
    // SINGLE pass over the template: a substituted VALUE is emitted verbatim
    // and never rescanned, so a value containing "{other_key}" cannot inject
    // a second substitution (the old per-key replace loop rescanned earlier
    // substitutions — untrusted data could pull in any other map key, and
    // the result depended on map iteration order). A `{key}` not in the map,
    // a `{` with no closing `}`, and a nested `{` all stay literal.
    let mut result = String::with_capacity(tmpl.len());
    let mut rest = tmpl.as_str();
    while let Some(open) = rest.find('{') {
        result.push_str(&rest[..open]);
        let after = &rest[open + 1..];
        match after.find(['{', '}']) {
            // A well-formed `{key}` with a known key → substitute, once.
            Some(end) if after.as_bytes()[end] == b'}' && vars.contains_key(&after[..end]) => {
                result.push_str(&vars[&after[..end]].to_mix_string());
                rest = &after[end + 1..];
            }
            // Unknown key, nested `{`, or unterminated → the `{` is literal.
            _ => {
                result.push('{');
                rest = after;
            }
        }
    }
    result.push_str(rest);
    Ok(Some(Value::String(result)))
}

/// printf-style formatter used by `fmt`, `printf`, `eprintf`. `pub`
/// so the evaluator's inline `printf`/`eprintf` handlers can reuse
/// the exact same format grammar as the pure `fmt` builtin without
/// duplicating the parser.
pub fn mix_format_public(name: &str, tmpl: &str, args: &[Value]) -> MixResult<String> {
    mix_format(name, tmpl, args)
}

/// printf-style formatter used by `fmt`, `printf`, `eprintf`.
///
/// Supports the minimal set the v0.2.0 plan spec'd:
///
/// | Spec     | Meaning                                   |
/// |----------|-------------------------------------------|
/// | `%s`     | string (any value via `to_mix_string`)    |
/// | `%d`     | integer (truncates floats)                |
/// | `%f`     | float (default 6 decimals like C printf)  |
/// | `%.Nf`   | float, N decimals                         |
/// | `%Nd`    | integer, min-width N (right-aligned)      |
/// | `%-Ns`   | string, min-width N (left-aligned)        |
/// | `%Ns`    | string, min-width N (right-aligned)       |
/// | `%%`     | literal `%`                               |
///
/// No `{}`-style templates — Mix already has `${...}` interpolation
/// and adding a second template syntax would confuse scripts that
/// mix both. No `%x`/`%o`/`%e`/`%g` — not worth carrying hex/octal
/// paths for a sysadmin-script language; revisit if a real script
/// needs them.
///
/// Arg errors (too few args, unknown specifier) raise `RuntimeError`
/// rather than silently substituting so typos surface loudly.
fn mix_format(name: &str, tmpl: &str, args: &[Value]) -> MixResult<String> {
    let mut out = String::with_capacity(tmpl.len());
    let mut arg_idx = 0usize;
    let bytes = tmpl.as_bytes();
    let mut i = 0;

    let take_arg = |idx: &mut usize, fmt_name: &str, spec: &str| -> MixResult<Value> {
        if *idx >= args.len() {
            return Err(MixError::RuntimeError {
                span: None,
                msg: format!(
                    "{}: not enough arguments for format '{}' (got {}, template needs more)",
                    fmt_name,
                    spec,
                    args.len()
                ),
            });
        }
        let v = args[*idx].clone();
        *idx += 1;
        Ok(v)
    };

    while i < bytes.len() {
        // UTF-8-safe pass-through: anything that isn't ASCII `%` is
        // copied as-is. Multi-byte UTF-8 sequences (em-dash, accented
        // characters, CJK text) must not be decomposed into single
        // bytes — that turns `—` into `â` on display. The `%`
        // dispatch below only inspects ASCII format characters, so
        // byte-indexed spec parsing stays correct after this branch.
        if bytes[i] != b'%' {
            // Advance by one full UTF-8 codepoint. Rust guarantees
            // `tmpl` is valid UTF-8, so every non-continuation byte
            // starts a codepoint. Safe to decode the next char by
            // slicing from `i` to the next codepoint boundary.
            let rest = &tmpl[i..];
            if let Some(c) = rest.chars().next() {
                out.push(c);
                i += c.len_utf8();
            } else {
                break;
            }
            continue;
        }

        // `%` — parse spec. Grammar: `%` `-`? digit* (`.` digit+)? [sdf%]
        let spec_start = i;
        i += 1;
        if i >= bytes.len() {
            return Err(MixError::RuntimeError {
                span: None,
                msg: format!("{}: trailing '%' at end of template", name),
            });
        }

        // Literal %%
        if bytes[i] as char == '%' {
            out.push('%');
            i += 1;
            continue;
        }

        // Flags, accepted in any order: `-` (left-align) and `0` (zero-pad).
        // POSIX semantics: `-` overrides `0`, and `0` applies to the numeric
        // conversions only — for a string, `%0Ns` is undefined in C, so it is
        // ignored here and `lpad(s, n, "0")` is the way to zero-pad text.
        // Unambiguous against width: a width never starts with `0`.
        let mut left_align = false;
        let mut zero_pad = false;
        while i < bytes.len() {
            match bytes[i] as char {
                '-' => {
                    left_align = true;
                    i += 1;
                }
                '0' => {
                    zero_pad = true;
                    i += 1;
                }
                _ => break,
            }
        }

        // Width: literal digits, or `*` taking the width from the next
        // argument (printf convention, 0.63.0) — %*s / %-*s / %*d / %0*d.
        // The width argument must be a non-negative integer; anything
        // else is an error, never a guess.
        let mut width: Option<usize> = None;
        if i < bytes.len() && bytes[i] == b'*' {
            i += 1;
            let spec_so_far = &tmpl[spec_start..i];
            let v = take_arg(&mut arg_idx, name, spec_so_far)?;
            let n = v.to_number().ok_or_else(|| MixError::RuntimeError {
                span: None,
                msg: format!(
                    "{}: '*' width for '{}' expects a number, got {}",
                    name,
                    spec_so_far,
                    v.type_name()
                ),
            })?;
            if n.fract() != 0.0 || !(0.0..=1_000_000.0).contains(&n) {
                return Err(MixError::RuntimeError {
                    span: None,
                    msg: format!(
                        "{}: '*' width for '{}' must be a non-negative integer (≤ 1000000), got {}",
                        name, spec_so_far, n
                    ),
                });
            }
            width = Some(n as usize);
            // `%*5s` would silently drop the '*' context into the
            // unknown-specifier arm; name the actual mistake instead.
            if i < bytes.len() && (bytes[i] as char).is_ascii_digit() {
                return Err(MixError::RuntimeError {
                    span: None,
                    msg: format!(
                        "{}: '*' width cannot be combined with literal digits (in '{}...')",
                        name, spec_so_far
                    ),
                });
            }
        } else {
            let width_start = i;
            while i < bytes.len() && (bytes[i] as char).is_ascii_digit() {
                i += 1;
            }
            if i > width_start {
                width = tmpl[width_start..i].parse().ok();
            }
        }

        let mut precision: Option<usize> = None;
        if i < bytes.len() && bytes[i] as char == '.' {
            i += 1;
            // `%.*f` — dynamic precision is not supported; '*' is
            // width-only. Without this arm the '*' becomes the conversion
            // char and errors as an unhelpful "unknown specifier '%*'".
            if i < bytes.len() && bytes[i] == b'*' {
                return Err(MixError::RuntimeError {
                    span: None,
                    msg: format!(
                        "{}: dynamic precision '.*' is not supported — '*' is width-only \
                         (use a literal precision: %.2f)",
                        name
                    ),
                });
            }
            let prec_start = i;
            while i < bytes.len() && (bytes[i] as char).is_ascii_digit() {
                i += 1;
            }
            if i > prec_start {
                precision = tmpl[prec_start..i].parse().ok();
            }
        }

        if i >= bytes.len() {
            return Err(MixError::RuntimeError {
                span: None,
                msg: format!(
                    "{}: unterminated format specifier starting at {}",
                    name, spec_start
                ),
            });
        }

        let conv = bytes[i] as char;
        i += 1;
        let spec_str = &tmpl[spec_start..i];

        match conv {
            's' => {
                let v = take_arg(&mut arg_idx, name, spec_str)?;
                let s = v.to_mix_string();
                match width {
                    Some(w) if left_align => out.push_str(&format!("{:<width$}", s, width = w)),
                    Some(w) => out.push_str(&format!("{:>width$}", s, width = w)),
                    None => out.push_str(&s),
                }
            }
            'd' => {
                let v = take_arg(&mut arg_idx, name, spec_str)?;
                let n = v.to_number().ok_or_else(|| MixError::RuntimeError {
                    span: None,
                    msg: format!("{}: '%d' expects a number, got {}", name, v.type_name()),
                })?;
                // The table documents "%d (truncates floats)" — printf idiom,
                // kept. What the domain gate refuses is FABRICATION: main
                // printed 9223372036854775807 for fmt("%d", 1e30).
                let n = as_exact_integer(
                    &format!("{name}: '%d' argument {}", arg_idx),
                    n.trunc(),
                    i64::MIN,
                    i64::MAX,
                )?;
                match width {
                    Some(w) if left_align => out.push_str(&format!("{:<width$}", n, width = w)),
                    // Rust's `0` flag is sign-aware: -42 at width 6 is "-00042",
                    // not "000-42" as a fill-char + align would produce.
                    Some(w) if zero_pad => out.push_str(&format!("{:0width$}", n, width = w)),
                    Some(w) => out.push_str(&format!("{:>width$}", n, width = w)),
                    None => out.push_str(&n.to_string()),
                }
            }
            'f' => {
                let v = take_arg(&mut arg_idx, name, spec_str)?;
                let n = v.to_number().ok_or_else(|| MixError::RuntimeError {
                    span: None,
                    msg: format!("{}: '%f' expects a number, got {}", name, v.type_name()),
                })?;
                let prec = precision.unwrap_or(6);
                let formatted = format!("{:.*}", prec, n);
                match width {
                    Some(w) if left_align => {
                        out.push_str(&format!("{:<width$}", formatted, width = w))
                    }
                    // Format the number directly rather than padding `formatted`,
                    // so the sign stays left of the zeros.
                    Some(w) if zero_pad => {
                        out.push_str(&format!("{:0width$.prec$}", n, width = w, prec = prec))
                    }
                    Some(w) => out.push_str(&format!("{:>width$}", formatted, width = w)),
                    None => out.push_str(&formatted),
                }
            }
            other => {
                return Err(MixError::RuntimeError {
                    span: None,
                    msg: format!(
                        "{}: unknown format specifier '%{}' (supported: %s %d %f %.Nf %Nd %-Ns %0Nd %*s %-*s %0*d %%)",
                        name, other
                    ),
                });
            }
        }
    }

    Ok(out)
}

fn builtin_fmt(args: Vec<Value>) -> MixResult<Option<Value>> {
    if args.is_empty() {
        return Err(MixError::RuntimeError {
            span: None,
            msg: "fmt() requires at least a template string".to_string(),
        });
    }
    let tmpl = args[0].to_mix_string();
    let formatted = mix_format("fmt", &tmpl, &args[1..])?;
    Ok(Some(Value::String(formatted)))
}

// printf / eprintf are implemented inline in evaluator.rs so they
// can write through the captured `self.globals.stdout` /
// `self.globals.stderr` handles that the test harness replaces with
// SharedBuf. They share `mix_format_public` with the pure `fmt`
// builtin — only the I/O routing differs.

fn builtin_word_wrap(args: Vec<Value>) -> MixResult<Option<Value>> {
    expect_args("word_wrap", &args, 2)?;
    let text = args[0].to_mix_string();
    let width = number_arg("word_wrap", &args, 1)? as usize;
    // P1: the wrap budget counts CODEPOINTS, not bytes, so `width` matches the
    // codepoint `length` an author reasons with. `word_wrap_w` is the display-cell
    // variant (a CJK glyph budgets as 2). Both share `wrap_text`.
    Ok(Some(Value::String(wrap_text(&text, width, |w| {
        w.chars().count()
    }))))
}

fn builtin_word_wrap_w(args: Vec<Value>) -> MixResult<Option<Value>> {
    expect_args("word_wrap_w", &args, 2)?;
    let text = args[0].to_mix_string();
    let width = number_arg("word_wrap_w", &args, 1)? as usize;
    // Display-width budget (UAX #11): a CJK/emoji glyph counts as 2 cells, so the
    // wrap matches how a terminal renders the line. Otherwise identical to word_wrap.
    Ok(Some(Value::String(wrap_text(&text, width, |w| {
        UnicodeWidthStr::width(w)
    }))))
}

/// Greedy word wrap shared by `word_wrap` (codepoint budget) and `word_wrap_w`
/// (display-cell budget). `measure` returns the budget cost of a word; the single
/// separator space costs 1 in both metrics. `line_w` tracks the running width to
/// keep this O(n) — no per-word re-scan of the accumulated line. The "line already
/// has a word" test is `!line.is_empty()`, NOT `line_w != 0`: under the display
/// measure a word can be zero-width (a lone combining mark), and using the width as
/// the emptiness sentinel would drop the separator before the following word.
fn wrap_text(text: &str, width: usize, measure: impl Fn(&str) -> usize) -> String {
    let mut lines = Vec::new();
    for paragraph in text.lines() {
        let mut line = String::new();
        let mut line_w = 0usize;
        for word in paragraph.split_whitespace() {
            let word_w = measure(word);
            if !line.is_empty() && line_w + 1 + word_w > width {
                lines.push(std::mem::take(&mut line));
                line_w = 0;
            }
            if !line.is_empty() {
                line.push(' ');
                line_w += 1;
            }
            line.push_str(word);
            line_w += word_w;
        }
        lines.push(line);
    }
    lines.join("\n")
}

fn builtin_markdown_escape(args: Vec<Value>) -> MixResult<Option<Value>> {
    expect_args("markdown_escape", &args, 1)?;
    let text = args[0].to_mix_string();
    let escaped = text
        .replace('\\', "\\\\")
        .replace('*', "\\*")
        .replace('_', "\\_")
        .replace('[', "\\[")
        .replace(']', "\\]")
        .replace('(', "\\(")
        .replace(')', "\\)")
        .replace('#', "\\#")
        .replace('`', "\\`")
        .replace('|', "\\|")
        .replace('>', "\\>")
        .replace('-', "\\-");
    Ok(Some(Value::String(escaped)))
}

/// `html_escape(s)`: escape the five HTML-significant characters so a
/// value can be safely interpolated into HTML **element text** or an
/// **ordinary quoted attribute value** (single- or double-quoted).
///
/// This prevents *syntactic breakout* only. It is NOT sufficient for
/// "dangerous" attribute contexts where the value is interpreted as
/// code/URL: event handlers (`onclick=…`), `style=…`, `srcdoc`, or
/// URL-valued attributes (`href`/`src` — an escaped `javascript:…`
/// still runs); those need scheme/context validation, not just entity
/// escaping. It is also NOT for inline `<script>`/JSON contexts
/// (U+2028/U+2029, `</script>` — a separate concern; see the webd-cms
/// ADR). For ordinary element text and quoted attributes it is the
/// correct escaper — unlike `markdown_escape` (not HTML) and
/// `json_encode` (does not HTML-escape). Single-pass; `'` → `&#x27;`
/// (HTML5-universal, vs the HTML4-invalid `&apos;`).
/// `markdown(s)`: render CommonMark + GFM markdown to HTML. Raw HTML in the
/// source is escaped (emitted as literal text, not active markup) and link/
/// image URLs carrying a `javascript:`/`data:`/`vbscript:` scheme are
/// neutralised to `#` — safe-by-default for rendering author content into a
/// page, matching the JS `md()` renderer's posture.
#[cfg(feature = "markdown")]
fn builtin_markdown(args: Vec<Value>) -> MixResult<Option<Value>> {
    use pulldown_cmark::{Event, Options, Parser, Tag, html};
    expect_args("markdown", &args, 1)?;
    let src = args[0].to_mix_string();

    fn safe_url(u: pulldown_cmark::CowStr<'_>) -> pulldown_cmark::CowStr<'_> {
        // Extract the scheme — the chars before the first `:`, but ONLY if that
        // `:` comes before any `/?#` (otherwise it's a relative URL with no
        // scheme, e.g. `data/img.png`). Strip ASCII whitespace/control first,
        // since browsers ignore those *inside* a scheme (`java\tscript:` still
        // executes). Relative/`https`/`mailto` pass through untouched.
        let mut scheme = String::new();
        let mut has_scheme = false;
        for c in u.chars() {
            if c == ':' {
                has_scheme = true;
                break;
            }
            if c == '/' || c == '?' || c == '#' {
                break; // relative URL — no scheme
            }
            if !c.is_ascii_whitespace() && !c.is_ascii_control() {
                scheme.push(c.to_ascii_lowercase());
            }
        }
        if has_scheme && matches!(scheme.as_str(), "javascript" | "data" | "vbscript") {
            pulldown_cmark::CowStr::Borrowed("#")
        } else {
            u
        }
    }

    let mut opts = Options::empty();
    opts.insert(Options::ENABLE_TABLES);
    opts.insert(Options::ENABLE_STRIKETHROUGH);
    opts.insert(Options::ENABLE_TASKLISTS);
    opts.insert(Options::ENABLE_FOOTNOTES);

    let parser = Parser::new_ext(&src, opts).map(|ev| match ev {
        // Raw HTML in the source becomes escaped text (no active markup).
        Event::Html(s) => Event::Text(s),
        Event::InlineHtml(s) => Event::Text(s),
        Event::Start(Tag::Link {
            link_type,
            dest_url,
            title,
            id,
        }) => Event::Start(Tag::Link {
            link_type,
            dest_url: safe_url(dest_url),
            title,
            id,
        }),
        Event::Start(Tag::Image {
            link_type,
            dest_url,
            title,
            id,
        }) => Event::Start(Tag::Image {
            link_type,
            dest_url: safe_url(dest_url),
            title,
            id,
        }),
        other => other,
    });

    let mut out = String::with_capacity(src.len() + src.len() / 2);
    html::push_html(&mut out, parser);
    Ok(Some(Value::String(out)))
}

// --- Datastar SSE builtins (feature-gated) ---
//
// The pure half of the Datastar SDK: construct/serialize SSE events that
// the Datastar JS client interprets to patch the DOM or the client signal
// store. We lean on the `datastar` crate's framework-agnostic core
// (`PatchElements`/`PatchSignals` → `DatastarEvent: Display`) so the wire
// format tracks upstream and never drifts. These builtins FRAME content;
// they do NOT sanitise it — a handler MUST `html_escape()` untrusted HTML
// before passing it to `ds_patch_elements`, exactly as for any rendered
// page. See `docs/mix/datastar.md` and `tests/datastar.rs`.

// --- SSE frame-injection guards ---
//
// A browser's EventSource parser splits the stream into lines on \r, \n, or
// \r\n. The datastar SDK re-prefixes every `\n`-split line of `elements` and
// `signals` with its field name, so an embedded `\n` there cannot forge a
// frame — but a LONE `\r` slips past `str::lines()` (which only splits on
// `\n` and strips a trailing `\r`), stays embedded in a data line, and the
// browser then breaks the frame at it. So:
//   * `elements` / verbatim `signals`  → normalise CR→LF (every break then
//     gets re-prefixed; injection-proof, line structure preserved).
//   * `selector`                       → un-split single data line, so reject
//     ANY line terminator (a CSS selector never legitimately carries one).
//   * map-path `signals`               → serde_json escapes control chars, so
//     its output is provably single-line; no guard needed.

/// Normalise CRLF and lone CR to LF so a value the SDK splits per-line can't
/// hide a frame break from the field-name re-prefixing.
#[cfg(feature = "datastar")]
fn ds_normalize_newlines(s: &str) -> String {
    // CRLF first, then any remaining lone CR — never collapse the pair into a
    // doubled `\n` (which would otherwise read as a blank-line separator).
    s.replace("\r\n", "\n").replace('\r', "\n")
}

/// Reject a value destined for a single, un-split `data:` line (`selector`)
/// if it carries any line terminator — fail closed against frame injection.
#[cfg(feature = "datastar")]
fn ds_reject_line_terminators(field: &str, s: &str) -> MixResult<()> {
    if s.contains('\n') || s.contains('\r') {
        return Err(MixError::RuntimeError {
            span: None,
            msg: format!(
                "ds_patch_elements: {field} must not contain a line terminator \
                 (CR/LF) — SSE frame-injection guard"
            ),
        });
    }
    Ok(())
}

/// Map a Mix mode string to the SDK's `ElementPatchMode`.
#[cfg(feature = "datastar")]
fn ds_parse_mode(s: &str) -> MixResult<datastar::consts::ElementPatchMode> {
    use datastar::consts::ElementPatchMode;
    Ok(match s {
        "outer" => ElementPatchMode::Outer,
        "inner" => ElementPatchMode::Inner,
        "remove" => ElementPatchMode::Remove,
        "replace" => ElementPatchMode::Replace,
        "prepend" => ElementPatchMode::Prepend,
        "append" => ElementPatchMode::Append,
        "before" => ElementPatchMode::Before,
        "after" => ElementPatchMode::After,
        bad => {
            return Err(MixError::RuntimeError {
                span: None,
                msg: format!(
                    "ds_patch_elements: unknown mode '{bad}' (expected one of \
                     outer/inner/remove/replace/prepend/append/before/after)"
                ),
            });
        }
    })
}

/// ds_patch_elements(html, [{selector, mode, view_transition}]) → SSE event string.
#[cfg(feature = "datastar")]
fn builtin_ds_patch_elements(args: Vec<Value>) -> MixResult<Option<Value>> {
    use datastar::consts::ElementPatchMode;
    use datastar::prelude::PatchElements;
    expect_args("ds_patch_elements", &args, 1)?;
    // Normalise CR→LF so a lone `\r` in the (possibly untrusted) HTML can't
    // escape the SDK's per-line `data: elements` re-prefixing.
    let html = ds_normalize_newlines(&args[0].to_mix_string());

    // Optional options map (nil/absent → defaults).
    let mut selector: Option<String> = None;
    let mut mode = ElementPatchMode::default();
    let mut view_transition: Option<bool> = None;
    match args.get(1) {
        None | Some(Value::Nil) => {}
        Some(Value::Map(m)) => {
            if let Some(v) = m.get("selector")
                && !matches!(v, Value::Nil)
            {
                selector = Some(v.to_mix_string());
            }
            if let Some(v) = m.get("mode")
                && !matches!(v, Value::Nil)
            {
                mode = ds_parse_mode(&v.to_mix_string())?;
            }
            if let Some(v) = m.get("view_transition") {
                view_transition = Some(v.is_truthy());
            }
        }
        Some(other) => {
            return Err(MixError::RuntimeError {
                span: None,
                msg: format!(
                    "ds_patch_elements: second argument must be an options map, got {}",
                    other.type_name()
                ),
            });
        }
    }

    // A selector goes into a single un-split `data: selector …` line, so a CR/LF
    // there would forge extra lines/events — reject before it reaches the SDK.
    if let Some(sel) = &selector {
        ds_reject_line_terminators("selector", sel)?;
    }

    let mut ev = if mode == ElementPatchMode::Remove {
        let sel = selector.ok_or_else(|| MixError::RuntimeError {
            span: None,
            msg: "ds_patch_elements: mode 'remove' requires a 'selector' in the options map"
                .to_string(),
        })?;
        PatchElements::new_remove(sel)
    } else {
        let mut e = PatchElements::new(html).mode(mode);
        if let Some(sel) = selector {
            e = e.selector(sel);
        }
        e
    };
    if let Some(vt) = view_transition {
        ev = ev.use_view_transition(vt);
    }
    Ok(Some(Value::String(ev.into_datastar_event().to_string())))
}

/// ds_patch_signals(signals_map_or_json, [{only_if_missing}]) → SSE event string.
#[cfg(feature = "datastar")]
fn builtin_ds_patch_signals(args: Vec<Value>) -> MixResult<Option<Value>> {
    use datastar::prelude::PatchSignals;
    expect_args("ds_patch_signals", &args, 1)?;

    // A map/list is JSON-encoded; a string is used verbatim (already-JSON,
    // e.g. from json_encode()). The SDK splits `signals` per `\n`-line and
    // re-prefixes each, so a verbatim string only needs CR→LF normalisation
    // (a lone `\r` would otherwise escape the re-prefixing); the encoded-map
    // path is provably single-line because serde escapes control chars.
    let signals_json = match &args[0] {
        Value::String(s) => ds_normalize_newlines(s),
        Value::Map(_) | Value::List(_) => {
            let jv = crate::json::mix_to_json(&args[0]).map_err(|e| MixError::RuntimeError {
                span: None,
                msg: format!("ds_patch_signals: encode signals: {e}"),
            })?;
            serde_json::to_string(&jv).map_err(|e| MixError::RuntimeError {
                span: None,
                msg: format!("ds_patch_signals: encode signals: {e}"),
            })?
        }
        other => {
            return Err(MixError::RuntimeError {
                span: None,
                msg: format!(
                    "ds_patch_signals: first argument must be a signals map or JSON string, got {}",
                    other.type_name()
                ),
            });
        }
    };

    let only_if_missing = match args.get(1) {
        None | Some(Value::Nil) => false,
        Some(Value::Map(m)) => m
            .get("only_if_missing")
            .map(Value::is_truthy)
            .unwrap_or(false),
        Some(other) => {
            return Err(MixError::RuntimeError {
                span: None,
                msg: format!(
                    "ds_patch_signals: second argument must be an options map, got {}",
                    other.type_name()
                ),
            });
        }
    };

    let mut ev = PatchSignals::new(signals_json);
    if only_if_missing {
        ev = ev.only_if_missing(true);
    }
    Ok(Some(Value::String(ev.into_datastar_event().to_string())))
}

/// ds_sse(event | [events]) → full text/event-stream body.
#[cfg(feature = "datastar")]
fn builtin_ds_sse(args: Vec<Value>) -> MixResult<Option<Value>> {
    expect_args("ds_sse", &args, 1)?;
    let mut body = String::new();
    match &args[0] {
        Value::String(s) => body.push_str(s),
        Value::List(items) => {
            for (i, it) in items.iter().enumerate() {
                match it {
                    Value::String(s) => body.push_str(s),
                    other => {
                        return Err(MixError::RuntimeError {
                            span: None,
                            msg: format!(
                                "ds_sse: list element {i} must be an SSE event string, got {}",
                                other.type_name()
                            ),
                        });
                    }
                }
            }
        }
        other => {
            return Err(MixError::RuntimeError {
                span: None,
                msg: format!(
                    "ds_sse: argument must be an SSE event string or a list of them, got {}",
                    other.type_name()
                ),
            });
        }
    }
    Ok(Some(Value::String(body)))
}

fn builtin_html_escape(args: Vec<Value>) -> MixResult<Option<Value>> {
    expect_args("html_escape", &args, 1)?;
    let s = args[0].to_mix_string();
    let mut out = String::with_capacity(s.len() + s.len() / 8);
    for ch in s.chars() {
        match ch {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            '\'' => out.push_str("&#x27;"),
            _ => out.push(ch),
        }
    }
    Ok(Some(Value::String(out)))
}

fn builtin_sanitize(args: Vec<Value>) -> MixResult<Option<Value>> {
    expect_args("sanitize", &args, 1)?;
    let s = args[0].to_mix_string();
    Ok(Some(Value::String(sanitize_for_diag(&s))))
}

// --- CSV/INI parsing (stdlib) ---

fn builtin_csv_parse(args: Vec<Value>) -> MixResult<Option<Value>> {
    expect_args("csv_parse", &args, 1)?;
    let text = args[0].to_mix_string();
    let delim = args
        .get(1)
        .map(|v| v.to_mix_string())
        .unwrap_or_else(|| ",".into());
    let delim = delim.chars().next().unwrap_or(',');
    let mut lines = text.lines();
    let headers: Vec<String> = match lines.next() {
        Some(h) => h.split(delim).map(|s| s.trim().to_string()).collect(),
        None => return Ok(Some(Value::list(Vec::new()))),
    };
    let mut rows = Vec::new();
    for line in lines {
        if line.trim().is_empty() {
            continue;
        }
        let mut map = indexmap::IndexMap::new();
        for (i, field) in line.split(delim).enumerate() {
            let key = headers.get(i).cloned().unwrap_or_else(|| format!("col{i}"));
            map.insert(key, Value::String(field.trim().to_string()));
        }
        rows.push(Value::map(map));
    }
    Ok(Some(Value::list(rows)))
}

fn builtin_ini_parse(args: Vec<Value>) -> MixResult<Option<Value>> {
    expect_args("ini_parse", &args, 1)?;
    let text = args[0].to_mix_string();
    let mut result: indexmap::IndexMap<String, Value> = indexmap::IndexMap::new();
    let mut current_section = String::new();
    let mut section_map: indexmap::IndexMap<String, Value> = indexmap::IndexMap::new();

    for line in text.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') || line.starts_with(';') {
            continue;
        }
        if line.starts_with('[') && line.ends_with(']') {
            if !current_section.is_empty() || !section_map.is_empty() {
                result.insert(
                    if current_section.is_empty() {
                        "_global".into()
                    } else {
                        current_section.clone()
                    },
                    Value::map(section_map),
                );
            }
            current_section = line[1..line.len() - 1].trim().to_string();
            section_map = indexmap::IndexMap::new();
        } else if let Some((key, val)) = line.split_once('=') {
            section_map.insert(
                key.trim().to_string(),
                Value::String(val.trim().to_string()),
            );
        }
    }
    if !section_map.is_empty() {
        result.insert(
            if current_section.is_empty() {
                "_global".into()
            } else {
                current_section
            },
            Value::map(section_map),
        );
    }
    Ok(Some(Value::map(result)))
}

// --- XML builtin (feature-gated) ---

/// Nesting cap for `xml_parse`. The parse is iterative (explicit stack, no
/// Rust recursion on input depth), so untrusted input can't overflow the
/// call stack during the parse; the cap bounds the *post-parse* walks
/// (Value build + drop glue recurse per level) and memory abuse. SOAP runs
/// ~5 levels deep; 256 is far beyond any sane document.
#[cfg(feature = "xml")]
const XML_PARSE_MAX_DEPTH: usize = 256;

#[cfg(feature = "xml")]
fn xml_err(msg: impl std::fmt::Display) -> MixError {
    MixError::RuntimeError {
        span: None,
        msg: format!("xml_parse: {msg}"),
    }
}

/// `ns:local` → `local` — simple mode strips namespace prefixes so SOAP
/// consumers navigate by local name (`.Envelope.Body.…`), immune to the
/// arbitrary prefixes each server picks.
#[cfg(feature = "xml")]
fn xml_local_name(raw: &str) -> &str {
    raw.rsplit(':').next().unwrap_or(raw)
}

#[cfg(feature = "xml")]
struct XmlNode {
    name: String,
    attrs: indexmap::IndexMap<String, String>,
    children: Vec<XmlChild>,
}

#[cfg(feature = "xml")]
enum XmlChild {
    Element(XmlNode),
    Text(String),
}

#[cfg(feature = "xml")]
fn xml_node_from_start(e: &quick_xml::events::BytesStart) -> Result<XmlNode, MixError> {
    let name = String::from_utf8_lossy(e.name().as_ref()).into_owned();
    let mut attrs = indexmap::IndexMap::new();
    for attr in e.attributes() {
        let attr = attr.map_err(xml_err)?;
        let key = String::from_utf8_lossy(attr.key.as_ref()).into_owned();
        let val = attr
            .normalized_value(quick_xml::XmlVersion::Implicit1_0)
            .map_err(xml_err)?
            .into_owned();
        attrs.insert(key, val);
    }
    Ok(XmlNode {
        name,
        attrs,
        children: Vec::new(),
    })
}

/// Strict single-pass pull parse into an `XmlNode` tree. Comments, the XML
/// declaration, processing instructions and DOCTYPE are skipped; CDATA joins
/// text; predefined + numeric character entities are resolved. Errors on
/// malformed input, mismatched/unclosed tags, multiple roots, text outside
/// the root, and nesting past `XML_PARSE_MAX_DEPTH`.
#[cfg(feature = "xml")]
fn xml_parse_document(s: &str) -> Result<XmlNode, MixError> {
    use quick_xml::events::Event;

    fn attach(
        node: XmlNode,
        stack: &mut [XmlNode],
        root: &mut Option<XmlNode>,
    ) -> Result<(), MixError> {
        match stack.last_mut() {
            Some(parent) => parent.children.push(XmlChild::Element(node)),
            None => {
                if root.is_some() {
                    return Err(xml_err("multiple root elements"));
                }
                *root = Some(node);
            }
        }
        Ok(())
    }

    let mut reader = quick_xml::Reader::from_str(s);
    let mut stack: Vec<XmlNode> = Vec::new();
    let mut root: Option<XmlNode> = None;
    loop {
        match reader.read_event().map_err(xml_err)? {
            Event::Start(e) => {
                if stack.len() >= XML_PARSE_MAX_DEPTH {
                    return Err(xml_err(format!(
                        "nesting deeper than {XML_PARSE_MAX_DEPTH} levels"
                    )));
                }
                if root.is_some() && stack.is_empty() {
                    return Err(xml_err("multiple root elements"));
                }
                stack.push(xml_node_from_start(&e)?);
            }
            Event::Empty(e) => {
                // Same cap as Start: a self-closing element at the cap would
                // otherwise attach one level past XML_PARSE_MAX_DEPTH.
                if stack.len() >= XML_PARSE_MAX_DEPTH {
                    return Err(xml_err(format!(
                        "nesting deeper than {XML_PARSE_MAX_DEPTH} levels"
                    )));
                }
                attach(xml_node_from_start(&e)?, &mut stack, &mut root)?
            }
            Event::End(e) => {
                let node = stack
                    .pop()
                    .ok_or_else(|| xml_err("unexpected closing tag"))?;
                // Backstop only: quick-xml's default `check_end_names` errors
                // on mismatched tags before we get here; this guards against
                // that config default ever changing.
                let end_name = String::from_utf8_lossy(e.name().as_ref()).into_owned();
                if node.name != end_name {
                    return Err(xml_err(format!(
                        "mismatched closing tag: expected </{}>, got </{end_name}>",
                        node.name
                    )));
                }
                attach(node, &mut stack, &mut root)?;
            }
            Event::Text(t) => {
                let text = t.decode().map_err(xml_err)?;
                match stack.last_mut() {
                    Some(parent) => parent.children.push(XmlChild::Text(text.into_owned())),
                    // Inter-document whitespace is legal; real text outside
                    // the root element is not.
                    None if text.trim().is_empty() => {}
                    None => return Err(xml_err("text content outside the root element")),
                }
            }
            // quick-xml 0.41 emits `&amp;` / `&#65;` as separate GeneralRef
            // events between Text events — resolve them or entity content
            // would be silently dropped.
            Event::GeneralRef(r) => {
                let resolved = if let Some(ch) = r.resolve_char_ref().map_err(xml_err)? {
                    ch.to_string()
                } else {
                    let name = r.decode().map_err(xml_err)?;
                    match quick_xml::escape::resolve_predefined_entity(&name) {
                        Some(s) => s.to_string(),
                        // No DTD support — a custom entity has no expansion.
                        None => return Err(xml_err(format!("unknown entity &{name};"))),
                    }
                };
                match stack.last_mut() {
                    Some(parent) => parent.children.push(XmlChild::Text(resolved)),
                    None => return Err(xml_err("entity reference outside the root element")),
                }
            }
            Event::CData(c) => {
                let text = c.decode().map_err(xml_err)?.into_owned();
                match stack.last_mut() {
                    Some(parent) => parent.children.push(XmlChild::Text(text)),
                    None => return Err(xml_err("CDATA outside the root element")),
                }
            }
            Event::Eof => break,
            _ => {}
        }
    }
    if let Some(open) = stack.last() {
        return Err(xml_err(format!("unclosed element <{}>", open.name)));
    }
    root.ok_or_else(|| xml_err("no root element"))
}

/// Simple-mode collapse (recursion bounded by `XML_PARSE_MAX_DEPTH`):
/// attributes become `@name` keys (xmlns declarations dropped), child
/// elements become keys by local name (repeats collapse to a list), a leaf
/// element's trimmed text IS its value, and mixed text lands under `#text`.
/// Lossy by design (child order across different names, prefixes, mixed-
/// content interleaving) — `{mode:"tree"}` is the full-fidelity escape hatch.
#[cfg(feature = "xml")]
fn xml_node_to_simple_value(node: XmlNode) -> Value {
    let mut m: indexmap::IndexMap<String, Value> = indexmap::IndexMap::new();
    for (k, v) in node.attrs {
        if k == "xmlns" || k.starts_with("xmlns:") {
            continue;
        }
        m.insert(format!("@{}", xml_local_name(&k)), Value::String(v));
    }
    let mut text = String::new();
    let mut children: indexmap::IndexMap<String, Vec<Value>> = indexmap::IndexMap::new();
    for child in node.children {
        match child {
            XmlChild::Text(t) => text.push_str(&t),
            XmlChild::Element(n) => {
                let key = xml_local_name(&n.name).to_string();
                let v = xml_node_to_simple_value(n);
                children.entry(key).or_default().push(v);
            }
        }
    }
    let text = text.trim();
    if m.is_empty() && children.is_empty() {
        return Value::String(text.to_string());
    }
    for (k, mut vs) in children {
        let v = if vs.len() == 1 {
            vs.pop().expect("len checked")
        } else {
            Value::list(vs)
        };
        m.insert(k, v);
    }
    if !text.is_empty() {
        m.insert("#text".into(), Value::String(text.to_string()));
    }
    Value::map(m)
}

/// Tree-mode conversion (recursion bounded by `XML_PARSE_MAX_DEPTH`): every
/// element is `{name, attrs, children}` verbatim — prefixes and xmlns kept,
/// child order preserved, text children as plain strings (whitespace-only
/// text between elements dropped).
#[cfg(feature = "xml")]
fn xml_node_to_tree_value(node: XmlNode) -> Value {
    let XmlNode {
        name,
        attrs,
        children,
    } = node;
    let mut m: indexmap::IndexMap<String, Value> = indexmap::IndexMap::new();
    m.insert("name".into(), Value::String(name));
    m.insert(
        "attrs".into(),
        Value::map(
            attrs
                .into_iter()
                .map(|(k, v)| (k, Value::String(v)))
                .collect(),
        ),
    );
    let kids: Vec<Value> = children
        .into_iter()
        .filter_map(|c| match c {
            XmlChild::Element(n) => Some(xml_node_to_tree_value(n)),
            XmlChild::Text(t) if t.trim().is_empty() => None,
            XmlChild::Text(t) => Some(Value::String(t)),
        })
        .collect();
    m.insert("children".into(), Value::list(kids));
    Value::map(m)
}

#[cfg(feature = "xml")]
fn builtin_xml_parse(args: Vec<Value>) -> MixResult<Option<Value>> {
    expect_args_between("xml_parse", &args, 1, 2)?;
    let text = match &args[0] {
        Value::String(s) => s.clone(),
        // An http_get/http_post `bytes` payload parses directly (strict
        // UTF-8 — XML in another encoding needs decoding first).
        Value::Bytes(b) => match std::str::from_utf8(b.as_slice()) {
            Ok(s) => s.to_string(),
            Err(e) => return Err(xml_err(format!("input bytes are not valid UTF-8: {e}"))),
        },
        other => other.to_mix_string(),
    };
    let mut mode = String::from("simple");
    if let Some(opts) = args.get(1) {
        match opts {
            Value::Nil => {}
            Value::Map(o) => {
                if let Some(v) = o.get("mode") {
                    mode = v.to_mix_string();
                }
            }
            other => {
                return Err(xml_err(format!(
                    "options must be a map or nil, got {}",
                    other.type_name()
                )));
            }
        }
    }
    if mode != "simple" && mode != "tree" {
        return Err(xml_err(format!(
            "unknown mode {mode:?} (expected \"simple\" or \"tree\")"
        )));
    }
    let root = xml_parse_document(&text)?;
    let value = if mode == "tree" {
        xml_node_to_tree_value(root)
    } else {
        let key = xml_local_name(&root.name).to_string();
        let v = xml_node_to_simple_value(root);
        let mut m = indexmap::IndexMap::new();
        m.insert(key, v);
        Value::map(m)
    };
    Ok(Some(value))
}

// --- URL builtins (feature-gated) ---

#[cfg(feature = "url")]
fn builtin_url_parse(args: Vec<Value>) -> MixResult<Option<Value>> {
    expect_args("url_parse", &args, 1)?;
    let s = args[0].to_mix_string();
    let u = url::Url::parse(&s).map_err(|e| MixError::RuntimeError {
        span: None,
        msg: format!("url_parse: {e}"),
    })?;
    let mut map = indexmap::IndexMap::new();
    map.insert("scheme".into(), Value::String(u.scheme().into()));
    map.insert(
        "host".into(),
        Value::String(u.host_str().unwrap_or("").into()),
    );
    map.insert(
        "port".into(),
        u.port()
            .map(|p| Value::Number(p as f64))
            .unwrap_or(Value::Nil),
    );
    map.insert("path".into(), Value::String(u.path().into()));
    map.insert(
        "query".into(),
        u.query()
            .map(|q| Value::String(q.into()))
            .unwrap_or(Value::Nil),
    );
    map.insert(
        "fragment".into(),
        u.fragment()
            .map(|f| Value::String(f.into()))
            .unwrap_or(Value::Nil),
    );
    Ok(Some(Value::map(map)))
}

// --- URL percent-coding (always available; no `url` crate) ---
//
// Hand-rolled so a build without the `url` feature (e.g. cosmix-webd's
// `json`-only cosmix-lib-mix) can still decode form/query input and
// encode output. Operates on bytes and returns UTF-8 lossily — correct
// for the ASCII-dominant form/query case and never panics on a stray
// `%`/`%X`/non-UTF-8 sequence.

/// `url_decode(s)`: percent-decode `%XX`, treat `+` as space (the
/// `application/x-www-form-urlencoded` convention). Invalid/truncated
/// `%` escapes are passed through literally.
/// Percent/'+' decode one form-urlencoded component (shared by
/// `url_decode` and `parse_query`/`parse_form`). `+` → space, `%XX` →
/// byte; a truncated/invalid `%` escape passes through literally.
/// UTF-8-lossy on the decoded bytes.
fn url_decode_str(s: &str) -> String {
    let bytes = s.as_bytes();
    let mut out: Vec<u8> = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        match bytes[i] {
            b'+' => {
                out.push(b' ');
                i += 1;
            }
            b'%' if i + 2 < bytes.len() => {
                let hi = (bytes[i + 1] as char).to_digit(16);
                let lo = (bytes[i + 2] as char).to_digit(16);
                match (hi, lo) {
                    (Some(h), Some(l)) => {
                        out.push((h * 16 + l) as u8);
                        i += 3;
                    }
                    // Not a valid %XX — pass the '%' through literally.
                    _ => {
                        out.push(b'%');
                        i += 1;
                    }
                }
            }
            b => {
                out.push(b);
                i += 1;
            }
        }
    }
    String::from_utf8_lossy(&out).into_owned()
}

fn builtin_url_decode(args: Vec<Value>) -> MixResult<Option<Value>> {
    expect_args("url_decode", &args, 1)?;
    Ok(Some(Value::String(url_decode_str(
        &args[0].to_mix_string(),
    ))))
}

/// `parse_query(s)` / `parse_form(s)`: parse an
/// `application/x-www-form-urlencoded` string (a URL query string or a
/// form POST body) into a map. Splits on `&`, each pair on the first
/// `=`; both key and value are `url_decode`d. A pair with no `=` maps
/// the key to `""`; empty pairs (`&&`, leading/trailing `&`) are
/// skipped. **Last value wins** on a repeated key (every value is a
/// String — a multi-value variant can come later if needed). Insertion
/// order is preserved.
fn builtin_parse_urlencoded(args: Vec<Value>, name: &str) -> MixResult<Option<Value>> {
    expect_args(name, &args, 1)?;
    let s = args[0].to_mix_string();
    let mut map: indexmap::IndexMap<String, Value> = indexmap::IndexMap::new();
    for pair in s.split('&') {
        if pair.is_empty() {
            continue;
        }
        let (k, v) = match pair.find('=') {
            Some(idx) => (&pair[..idx], &pair[idx + 1..]),
            None => (pair, ""),
        };
        map.insert(url_decode_str(k), Value::String(url_decode_str(v)));
    }
    Ok(Some(Value::map(map)))
}

/// `url_encode(s)`: percent-encode everything except the unreserved set
/// (`A-Za-z0-9-_.~`), encoding space as `%20`. Suitable for a single
/// query-value or path segment.
fn builtin_url_encode(args: Vec<Value>) -> MixResult<Option<Value>> {
    expect_args("url_encode", &args, 1)?;
    let s = args[0].to_mix_string();
    let mut out = String::with_capacity(s.len() * 3);
    for &b in s.as_bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(b as char);
            }
            _ => {
                out.push('%');
                out.push(
                    char::from_digit((b >> 4) as u32, 16)
                        .unwrap()
                        .to_ascii_uppercase(),
                );
                out.push(
                    char::from_digit((b & 0x0f) as u32, 16)
                        .unwrap()
                        .to_ascii_uppercase(),
                );
            }
        }
    }
    Ok(Some(Value::String(out)))
}

// --- Crypto builtins (feature-gated) ---

/// Borrow a `Value`'s raw byte view for crypto / encoding builtins.
///
/// `Value::Bytes` returns a borrowed slice directly; every other type
/// is stringified through `to_mix_string` (legacy behaviour). The
/// dedicated `Bytes` arm is what keeps the EF BF BD / `<bytes:N>` bug
/// class out of hash / base64 / future digest builtins — without it,
/// `hash_sha256(read_file_bytes(...))` silently hashes the display
/// placeholder rather than the file.
#[cfg(feature = "crypto")]
fn value_as_crypto_bytes(v: &Value) -> std::borrow::Cow<'_, [u8]> {
    match v {
        Value::Bytes(b) => std::borrow::Cow::Borrowed(b.as_slice()),
        // A mutable buffer hashes/encodes its current bytes, like Bytes —
        // `base64_encode($buf)` / `hash_sha256($buf)` reach the payload.
        Value::Buffer(b) => std::borrow::Cow::Owned(b.borrow().clone()),
        other => std::borrow::Cow::Owned(other.to_mix_string().into_bytes()),
    }
}

#[cfg(feature = "crypto")]
fn builtin_base64_encode(args: Vec<Value>) -> MixResult<Option<Value>> {
    expect_args("base64_encode", &args, 1)?;
    use base64::Engine;
    // `Value::Bytes` is the documented JSON/Bus/TOML escape hatch —
    // `base64_encode($bytes)` must encode the raw buffer, not the
    // `<bytes:N>` placeholder that `to_mix_string` produces.
    // `base64_encode(base64_decode($s))` round-trips only if this
    // branch exists.
    let buf = value_as_crypto_bytes(&args[0]);
    let encoded = base64::engine::general_purpose::STANDARD.encode(buf.as_ref());
    Ok(Some(Value::String(encoded)))
}

#[cfg(feature = "crypto")]
fn builtin_base64_decode(args: Vec<Value>) -> MixResult<Option<Value>> {
    expect_args("base64_decode", &args, 1)?;
    use base64::Engine;
    let input = value_as_crypto_bytes(&args[0]);
    let bytes = base64::engine::general_purpose::STANDARD
        .decode(input.as_ref())
        .map_err(|e| MixError::RuntimeError {
            span: None,
            msg: format!("base64_decode: {e}"),
        })?;
    // Return raw bytes — base64 is the standard envelope for binary
    // payloads, and the previous `from_utf8_lossy` collapse silently
    // corrupted any non-UTF-8 byte (the same EF BF BD bug class that
    // motivated Value::Bytes). Callers that want a string can wrap in
    // `bytes_to_string($v)`, which errors loudly on non-UTF-8 rather
    // than silently substituting U+FFFD.
    Ok(Some(Value::bytes(bytes)))
}

/// `bytes_len($v)` — length in bytes.
///
/// Strict: argument must be `Value::Bytes`. Use `length($s)` for
/// strings (which counts chars, not bytes — different contract).
fn builtin_bytes_len(args: Vec<Value>) -> MixResult<Option<Value>> {
    expect_args("bytes_len", &args, 1)?;
    match &args[0] {
        Value::Bytes(b) => Ok(Some(Value::Number(b.len() as f64))),
        // Also accepts a mutable buffer — one length builtin for both
        // byte types (O(1), no copy).
        Value::Buffer(b) => Ok(Some(Value::Number(b.borrow().len() as f64))),
        other => Err(MixError::RuntimeError {
            span: None,
            msg: format!(
                "bytes_len: expected bytes or buffer, got {}",
                other.type_name()
            ),
        }),
    }
}

/// `string_to_bytes($s)` — UTF-8 encoding of a string as a bytes
/// buffer. Strict: rejects non-string arguments rather than letting
/// `to_mix_string` silently encode placeholders like `<bytes:N>` or
/// `[a, b]` — the same lossy-coercion bug class `Value::Bytes` was
/// added to escape.
fn builtin_string_to_bytes(args: Vec<Value>) -> MixResult<Option<Value>> {
    expect_args("string_to_bytes", &args, 1)?;
    match &args[0] {
        Value::String(s) => Ok(Some(Value::bytes(s.as_bytes().to_vec()))),
        other => Err(MixError::RuntimeError {
            span: None,
            msg: format!(
                "string_to_bytes: expected string, got {}",
                other.type_name()
            ),
        }),
    }
}

/// `bytes_to_string($v[, {lossy: true}])` — decode a bytes buffer as UTF-8.
///
/// Default is strict: errors loudly on non-UTF-8 input rather than
/// silent-substituting U+FFFD (the bug class that motivated
/// `Value::Bytes`). Pass `{lossy: true}` to opt into a
/// `String::from_utf8_lossy` decode, where invalid byte sequences become
/// U+FFFD. That is the explicit, caller-acknowledged escape hatch for
/// sniffing not-quite-UTF-8 data — e.g. capping `read_file_bytes` at a
/// message's first 8 KiB to regex out an ASCII header, where raw 8-bit
/// bytes elsewhere in the block would otherwise reject the whole decode.
fn builtin_bytes_to_string(args: Vec<Value>) -> MixResult<Option<Value>> {
    if args.is_empty() || args.len() > 2 {
        return Err(MixError::RuntimeError {
            span: None,
            msg: format!("bytes_to_string expects 1 or 2 args, got {}", args.len()),
        });
    }
    let buf: std::borrow::Cow<[u8]> = match &args[0] {
        Value::Bytes(b) => std::borrow::Cow::Borrowed(b.as_slice()),
        // A mutable buffer decodes the same way (snapshot under a dropped
        // borrow so we never hold it across the decode).
        Value::Buffer(b) => std::borrow::Cow::Owned(b.borrow().clone()),
        other => {
            return Err(MixError::RuntimeError {
                span: None,
                msg: format!(
                    "bytes_to_string: expected bytes or buffer, got {}",
                    other.type_name()
                ),
            });
        }
    };
    let mut lossy = false;
    if let Some(opts) = args.get(1) {
        match opts {
            Value::Nil => {}
            Value::Map(m) => {
                if let Some(v) = m.get("lossy") {
                    lossy = v.is_truthy();
                }
            }
            other => {
                return Err(MixError::RuntimeError {
                    span: None,
                    msg: format!(
                        "bytes_to_string(): options must be a map or nil, got {}",
                        other.type_name()
                    ),
                });
            }
        }
    }
    if lossy {
        Ok(Some(Value::String(
            String::from_utf8_lossy(buf.as_ref()).into_owned(),
        )))
    } else {
        match std::str::from_utf8(buf.as_ref()) {
            Ok(s) => Ok(Some(Value::String(s.to_string()))),
            Err(e) => Err(MixError::RuntimeError {
                span: None,
                msg: format!("bytes_to_string: not valid UTF-8 ({e})"),
            }),
        }
    }
}

// --- bytes-as-a-sequence builtins (v0.64.0) ---
//
// NAMING (settled 0.64.0, see docs/mix/io.md "byte_* vs bytes_*"):
//   `byte_*`  operates on a STRING in byte offsets (byte_length, byte_pos,
//             byte_lastpos, byte_index_of) — the subject is text.
//   `bytes_*` operates on a `bytes`/`buffer` VALUE — the subject is a byte
//             sequence, and the argument type is checked, never coerced.
// They are NOT duplicates and must not be merged: `byte_length($b)` on a
// bytes value measures the `<bytes:N>` PLACEHOLDER string, which is why
// every builtin here rejects a non-bytes subject instead of stringifying it.

/// Snapshot the byte-sequence SUBJECT of a `bytes_*` builtin (argument 1).
///
/// Accepts `bytes` (borrowed, no copy) and `buffer` (copied under a borrow
/// that is dropped here — so `bytes_find($buf, $buf)` and friends can never
/// hold two live borrows of the same RefCell). Strictly rejects everything
/// else: coercing a string here would silently answer questions about the
/// `<bytes:N>` placeholder, the exact bug class `Value::Bytes` exists to
/// avoid.
fn subject_bytes<'a>(name: &str, v: &'a Value) -> MixResult<std::borrow::Cow<'a, [u8]>> {
    match v {
        Value::Bytes(b) => Ok(std::borrow::Cow::Borrowed(b.as_slice())),
        Value::Buffer(b) => Ok(std::borrow::Cow::Owned(b.borrow().clone())),
        other => Err(MixError::RuntimeError {
            span: None,
            msg: format!(
                "{name}(): expected bytes or buffer, got {}",
                other.type_name()
            ),
        }),
    }
}

/// Snapshot a byte-sequence OPERAND (a needle, separator, or prefix).
///
/// Wider than [`subject_bytes`] on purpose: a literal needle is almost
/// always written as a string (`bytes_find($b, "\r\n\r\n")`) or a single
/// byte number (`bytes_split($b, 10)`), and neither spelling can be
/// mistaken for a placeholder — a String encodes as its own UTF-8 and a
/// Number must be an exact 0-255. Lists, maps and nil are still refused.
fn operand_bytes(name: &str, what: &str, v: &Value) -> MixResult<Vec<u8>> {
    match v {
        Value::Bytes(b) => Ok(b.to_vec()),
        Value::Buffer(b) => Ok(b.borrow().clone()),
        Value::String(s) => Ok(s.as_bytes().to_vec()),
        Value::Number(n) => Ok(vec![as_exact_integer(
            &format!("{name}(): {what}"),
            *n,
            0,
            255,
        )? as u8]),
        other => Err(MixError::RuntimeError {
            span: None,
            msg: format!(
                "{name}(): {what} must be bytes, buffer, string or a byte number 0-255, got {}",
                other.type_name()
            ),
        }),
    }
}

/// First index at which `needle` occurs in `hay` at or after `from`.
/// Plain O(n*m) scan — the byte sequences these builtins see are message
/// bodies and protocol frames, not corpora, and a naive scan avoids
/// pulling a substring-search dependency into a no-default-features build.
fn find_subslice(hay: &[u8], needle: &[u8], from: usize) -> Option<usize> {
    if needle.is_empty() || needle.len() > hay.len() || from > hay.len() {
        return None;
    }
    let last = hay.len() - needle.len();
    (from..=last).find(|&i| &hay[i..i + needle.len()] == needle)
}

/// `bytes_find($b, needle[, from])` — 0-based byte offset of `needle`,
/// `-1` when absent. The 0-based/-1 convention (and its condition trap)
/// is `index_of`'s, not `pos`'s; `bytes_find` is registered in the
/// analyzer's `MINUS_ONE_SENTINEL_BUILTINS` so a bare use in a condition
/// is linted (MIX-W2305).
///
/// The optional `from` is what makes a scanning parser linear: without it
/// every step has to `slice` the remainder and add offsets back by hand.
/// It takes signed indices like `slice` and clamps, and the returned index
/// is ABSOLUTE (into `$b`), never relative to `from`.
fn builtin_bytes_find(args: Vec<Value>) -> MixResult<Option<Value>> {
    expect_args_between("bytes_find", &args, 2, 3)?;
    let hay = subject_bytes("bytes_find", &args[0])?;
    let needle = operand_bytes("bytes_find", "needle", &args[1])?;
    if needle.is_empty() {
        return Err(MixError::RuntimeError {
            span: None,
            msg: "bytes_find(): needle must not be empty".to_string(),
        });
    }
    let from = match args.get(2) {
        None | Some(Value::Nil) => 0,
        Some(v) => {
            let n = required_number_value("bytes_find(): argument 3", v)? as i64;
            clamp_signed_index(n, hay.len())
        }
    };
    let pos = find_subslice(&hay, &needle, from)
        .map(|i| i as f64)
        .unwrap_or(-1.0);
    Ok(Some(Value::Number(pos)))
}

/// `bytes_starts_with($b, prefix)` — bool. An empty prefix is true, as it
/// is for the string `starts_with`.
fn builtin_bytes_starts_with(args: Vec<Value>) -> MixResult<Option<Value>> {
    expect_args_between("bytes_starts_with", &args, 2, 2)?;
    let subject = subject_bytes("bytes_starts_with", &args[0])?;
    let prefix = operand_bytes("bytes_starts_with", "prefix", &args[1])?;
    Ok(Some(Value::Bool(subject.starts_with(&prefix))))
}

/// `bytes_split($b, sep)` → list of `bytes`.
///
/// Splitting rules are the string `split`'s: a separator that never occurs
/// yields a one-element list holding the whole input, a leading or
/// trailing separator yields an empty piece at that end, and an empty
/// input yields one empty piece. The one deliberate divergence is an EMPTY
/// separator, which `split` treats as "between every char" — meaningless
/// on raw bytes (and a silent infinite-piece trap), so it raises instead.
fn builtin_bytes_split(args: Vec<Value>) -> MixResult<Option<Value>> {
    expect_args_between("bytes_split", &args, 2, 2)?;
    let subject = subject_bytes("bytes_split", &args[0])?;
    let sep = operand_bytes("bytes_split", "separator", &args[1])?;
    if sep.is_empty() {
        return Err(MixError::RuntimeError {
            span: None,
            msg: "bytes_split(): separator must not be empty".to_string(),
        });
    }
    let mut out: Vec<Value> = Vec::new();
    let mut cursor = 0usize;
    while let Some(hit) = find_subslice(&subject, &sep, cursor) {
        out.push(Value::bytes(subject[cursor..hit].to_vec()));
        cursor = hit + sep.len();
    }
    out.push(Value::bytes(subject[cursor..].to_vec()));
    Ok(Some(Value::list(out)))
}

/// `bytes_concat($a, $b, ...)` → one new `bytes`.
///
/// Variadic like the list `concat`, but accepts 1+ arguments rather than
/// 2+ (a one-argument call is a copy, which is what generated/looping code
/// wants at the base case) and accepts a string as well as bytes/buffer —
/// a string joins as its own UTF-8, the same encoding `string_to_bytes`
/// gives. Passing a LIST raises and names `bytes_from`, which is the
/// list-shaped constructor.
fn builtin_bytes_concat(args: Vec<Value>) -> MixResult<Option<Value>> {
    expect_args("bytes_concat", &args, 1)?;
    let mut out: Vec<u8> = Vec::new();
    for (i, a) in args.iter().enumerate() {
        match a {
            Value::List(_) => {
                return Err(MixError::RuntimeError {
                    span: None,
                    msg: format!(
                        "bytes_concat(): argument {} is a list — use bytes_from(list) to build bytes from a list",
                        i + 1
                    ),
                });
            }
            _ => out.extend_from_slice(&operand_bytes(
                "bytes_concat",
                &format!("argument {}", i + 1),
                a,
            )?),
        }
    }
    Ok(Some(Value::bytes(out)))
}

/// `bytes_from($list)` → `bytes`. The list-shaped constructor, with the
/// same item vocabulary as `buffer([items])`: an int 0-255 is one byte, a
/// string contributes its UTF-8, and a bytes/buffer is flat-spliced. Any
/// other item — including a nested list — raises rather than being
/// stringified.
fn builtin_bytes_from(args: Vec<Value>) -> MixResult<Option<Value>> {
    expect_args_between("bytes_from", &args, 1, 1)?;
    match &args[0] {
        Value::List(items) => {
            let mut out: Vec<u8> = Vec::new();
            for (i, item) in items.iter().enumerate() {
                out.extend_from_slice(&buffer_item_bytes(
                    item,
                    &format!("bytes_from(): item {}", i + 1),
                )?);
            }
            Ok(Some(Value::bytes(out)))
        }
        other => Err(MixError::RuntimeError {
            span: None,
            msg: format!(
                "bytes_from(): expected a list, got {} — for a single value use string_to_bytes()/freeze()",
                other.type_name()
            ),
        }),
    }
}

/// `bytes_to_hex($b)` → lowercase hex, two characters per byte, no
/// separator and no prefix. Deliberately option-free so that
/// `bytes_from_hex(bytes_to_hex($b))` is an exact round trip; a separated
/// form is one `join` away and cannot then be fed back in by mistake.
fn builtin_bytes_to_hex(args: Vec<Value>) -> MixResult<Option<Value>> {
    expect_args_between("bytes_to_hex", &args, 1, 1)?;
    let buf = subject_bytes("bytes_to_hex", &args[0])?;
    // Shares `hex_encode` with the whole `hash_*` family, so a digest's hex
    // and `bytes_to_hex(hash_sha256($x, {raw:true}))` can never disagree.
    Ok(Some(Value::String(hex_encode(&buf))))
}

/// `bytes_from_hex($s)` → `bytes`. Strict: an even number of characters,
/// each `[0-9a-fA-F]`. Whitespace and separators are NOT stripped — a
/// caller that has `de:ad:be:ef` knows it does and can say so, whereas a
/// lenient decoder would silently accept a truncated or corrupted digest.
fn builtin_bytes_from_hex(args: Vec<Value>) -> MixResult<Option<Value>> {
    expect_args_between("bytes_from_hex", &args, 1, 1)?;
    let s = match &args[0] {
        Value::String(s) => s.as_str(),
        other => {
            return Err(MixError::RuntimeError {
                span: None,
                msg: format!("bytes_from_hex(): expected string, got {}", other.type_name()),
            });
        }
    };
    if !s.len().is_multiple_of(2) {
        return Err(MixError::RuntimeError {
            span: None,
            msg: format!(
                "bytes_from_hex(): hex string must have an even length, got {}",
                s.len()
            ),
        });
    }
    let raw = s.as_bytes();
    let mut out = Vec::with_capacity(raw.len() / 2);
    for pair in raw.chunks(2) {
        let hi = hex_nibble(pair[0])?;
        let lo = hex_nibble(pair[1])?;
        out.push(hi << 4 | lo);
    }
    Ok(Some(Value::bytes(out)))
}

/// One hex character → its 0-15 value; anything else raises and quotes the
/// offending character so the caller can see WHICH byte was wrong.
fn hex_nibble(c: u8) -> MixResult<u8> {
    match c {
        b'0'..=b'9' => Ok(c - b'0'),
        b'a'..=b'f' => Ok(c - b'a' + 10),
        b'A'..=b'F' => Ok(c - b'A' + 10),
        _ => Err(MixError::RuntimeError {
            span: None,
            msg: format!(
                "bytes_from_hex(): '{}' is not a hex digit",
                (c as char).escape_debug()
            ),
        }),
    }
}

// --- Buffer builtins (reference-semantic mutable byte buffers) ---

/// Snapshot one `buffer` / `buffer_push` source item into owned bytes:
/// an int 0-255 (one byte), a String (UTF-8), a `Bytes` (copy), or a
/// `Buffer` (copy of its current content under a borrow dropped here).
/// Snapshotting to an OWNED `Vec` before the target's `borrow_mut` is
/// what makes `buffer_push($b, $b)` (self-append) safe — the source
/// borrow is released before the mutable borrow is taken.
fn buffer_item_bytes(v: &Value, ctx: &str) -> MixResult<Vec<u8>> {
    match v {
        Value::Number(n) => Ok(vec![as_exact_integer(ctx, *n, 0, 255)? as u8]),
        Value::String(s) => Ok(s.as_bytes().to_vec()),
        Value::Bytes(b) => Ok(b.to_vec()),
        Value::Buffer(b) => Ok(b.borrow().clone()),
        other => Err(MixError::RuntimeError {
            span: None,
            msg: format!(
                "{ctx}: each item must be an int 0-255, string, bytes, or buffer, got {}",
                other.type_name()
            ),
        }),
    }
}

/// `buffer([init]) -> buffer` — create a reference-semantic mutable byte
/// buffer. No arg / `nil` → empty; `Number n` → n zero bytes (n a
/// non-negative integer — the "allocate N bytes" form); `String` → its
/// UTF-8 bytes; `Bytes` / `Buffer` → an INDEPENDENT copy of the current
/// content (a fresh backing store); `List` → a FLAT splice of each
/// element (int 0-255 / string / bytes / buffer), so
/// `buffer(["MThd", 0, 0, 0, 6])` mixes ASCII magic and raw bytes.
/// (To make a one-byte buffer holding the value 5, use `buffer([5])`;
/// `buffer(5)` allocates 5 zero bytes.)
fn builtin_buffer(args: Vec<Value>) -> MixResult<Option<Value>> {
    if args.len() > 1 {
        return Err(MixError::RuntimeError {
            span: None,
            msg: format!("buffer expects 0 or 1 args, got {}", args.len()),
        });
    }
    let bytes: Vec<u8> = match args.first() {
        None | Some(Value::Nil) => Vec::new(),
        Some(Value::Number(n)) => {
            // Allocate fallibly: a huge (or f64-saturated) size must
            // surface a catchable Mix error, not abort the process (and
            // the login shell) with an OOM. `try_reserve_exact` returns
            // Err instead of aborting; `resize` then won't reallocate.
            let size = as_count("buffer(): size", *n, usize::MAX)?;
            let mut v: Vec<u8> = Vec::new();
            v.try_reserve_exact(size)
                .map_err(|_| MixError::RuntimeError {
                    span: None,
                    msg: format!("buffer(n): cannot allocate {size} bytes"),
                })?;
            v.resize(size, 0);
            v
        }
        Some(Value::String(s)) => s.as_bytes().to_vec(),
        Some(Value::Bytes(b)) => b.to_vec(),
        Some(Value::Buffer(b)) => b.borrow().clone(),
        Some(Value::List(items)) => {
            let mut out = Vec::with_capacity(items.len());
            for item in items.iter() {
                out.extend(buffer_item_bytes(item, "buffer(list)")?);
            }
            out
        }
        Some(other) => {
            return Err(MixError::RuntimeError {
                span: None,
                msg: format!(
                    "buffer: init must be a size (number), string, bytes, buffer, or list of ints, got {}",
                    other.type_name()
                ),
            });
        }
    };
    Ok(Some(Value::Buffer(Rc::new(RefCell::new(bytes)))))
}

/// `buffer_push(buf, item, ...) -> nil` — append bytes to a buffer IN
/// PLACE. Reference-semantic: every alias of the buffer sees the growth
/// (unlike value-semantic `Bytes`). Each item is an int 0-255, a String
/// (UTF-8), a `Bytes`, or a `Buffer`. O(1) amortized per byte — this is
/// the fix for the O(n²) value-semantic append. Self-append-safe.
fn builtin_buffer_push(args: Vec<Value>) -> MixResult<Option<Value>> {
    if args.len() < 2 {
        return Err(MixError::RuntimeError {
            span: None,
            msg: format!(
                "buffer_push expects at least 2 args (buffer, item...), got {}",
                args.len()
            ),
        });
    }
    let target = match &args[0] {
        Value::Buffer(b) => b,
        other => {
            return Err(MixError::RuntimeError {
                span: None,
                msg: format!(
                    "buffer_push: first arg must be a buffer, got {}",
                    other.type_name()
                ),
            });
        }
    };
    // Snapshot every source into an owned Vec FIRST (each source borrow
    // released here) so appending a buffer to itself can't double-borrow
    // the same RefCell.
    let mut to_append: Vec<u8> = Vec::new();
    for item in &args[1..] {
        to_append.extend(buffer_item_bytes(item, "buffer_push")?);
    }
    target.borrow_mut().extend_from_slice(&to_append);
    Ok(Some(Value::Nil))
}

/// `buffer_get(buf, i) -> num` — the byte at 0-based index `i` as a
/// number 0-255, or `nil` if `i` is out of range.
fn builtin_buffer_get(args: Vec<Value>) -> MixResult<Option<Value>> {
    expect_args("buffer_get", &args, 2)?;
    let buf = match &args[0] {
        Value::Buffer(b) => b,
        other => {
            return Err(MixError::RuntimeError {
                span: None,
                msg: format!(
                    "buffer_get: first arg must be a buffer, got {}",
                    other.type_name()
                ),
            });
        }
    };
    let i = buffer_index(&args[1], "buffer_get")?;
    let b = buf.borrow();
    match b.get(i) {
        Some(byte) => Ok(Some(Value::Number(*byte as f64))),
        None => Ok(Some(Value::Nil)),
    }
}

/// `buffer_set(buf, i, byte) -> nil` — write `byte` (0-255) at 0-based
/// index `i`, in place. Errors if `i` is out of range (grow with
/// `buffer_push` first).
fn builtin_buffer_set(args: Vec<Value>) -> MixResult<Option<Value>> {
    expect_args("buffer_set", &args, 3)?;
    let buf = match &args[0] {
        Value::Buffer(b) => b,
        other => {
            return Err(MixError::RuntimeError {
                span: None,
                msg: format!(
                    "buffer_set: first arg must be a buffer, got {}",
                    other.type_name()
                ),
            });
        }
    };
    let i = buffer_index(&args[1], "buffer_set")?;
    let byte = match extract_number(&args[2], InputPolicy::NumberOnly) {
        Some(n) => as_exact_integer("buffer_set(): argument 3", n, 0, 255)? as u8,
        None => {
            return Err(MixError::RuntimeError {
                span: None,
                msg: format!(
                    "buffer_set: value must be an int 0-255, got {}",
                    args[2].to_mix_string()
                ),
            });
        }
    };
    let mut b = buf.borrow_mut();
    if i >= b.len() {
        return Err(MixError::RuntimeError {
            span: None,
            msg: format!(
                "buffer_set: index {i} out of range (buffer length {})",
                b.len()
            ),
        });
    }
    b[i] = byte;
    Ok(Some(Value::Nil))
}

/// Parse a non-negative integer index argument for `buffer_get`/`_set`.
fn buffer_index(v: &Value, ctx: &str) -> MixResult<usize> {
    match v {
        Value::Number(n) if n.fract() == 0.0 && *n >= 0.0 => Ok(*n as usize),
        other => Err(MixError::RuntimeError {
            span: None,
            msg: format!(
                "{ctx}: index must be a non-negative integer, got {}",
                other.to_mix_string()
            ),
        }),
    }
}

/// `freeze(buf) -> bytes` — snapshot a buffer to a value-semantic
/// `Bytes` (a copy of the current content). The bridge from the mutable
/// buffer into the value-semantic byte sinks (write_file / hash /
/// base64 / http).
fn builtin_freeze(args: Vec<Value>) -> MixResult<Option<Value>> {
    expect_args("freeze", &args, 1)?;
    match &args[0] {
        Value::Buffer(b) => Ok(Some(Value::bytes(b.borrow().clone()))),
        other => Err(MixError::RuntimeError {
            span: None,
            msg: format!("freeze: expected a buffer, got {}", other.type_name()),
        }),
    }
}

/// Lowercase hex of a byte slice — the one renderer every digest and
/// `bytes_to_hex` share, so no two of them can drift on case or padding.
pub(crate) fn hex_encode(bytes: &[u8]) -> String {
    let mut s = String::with_capacity(bytes.len() * 2);
    for b in bytes {
        use std::fmt::Write as _;
        let _ = write!(s, "{b:02x}");
    }
    s
}

/// The digest algorithms the `hash_*` family and `hash_file` share
/// (v0.66.0). One enum so a name added here reaches both surfaces — the
/// alternative left `hash_file` able to do sha256/blake3 while the
/// in-memory calls grew md5/sha1, which is exactly the half-surface that
/// generates the next bug report.
#[cfg(feature = "crypto")]
#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) enum DigestAlgo {
    Md5,
    Sha1,
    Sha256,
    Blake3,
}

#[cfg(feature = "crypto")]
impl DigestAlgo {
    /// Accepted spellings, as they appear in `hash_file(path, algo)`.
    pub(crate) fn from_name(name: &str) -> Option<Self> {
        match name {
            "md5" => Some(Self::Md5),
            "sha1" => Some(Self::Sha1),
            "sha256" => Some(Self::Sha256),
            "blake3" => Some(Self::Blake3),
            _ => None,
        }
    }

    /// **Cryptographically broken.** MD5 (collisions since 2004) and SHA-1
    /// (SHAttered, 2017) must never carry a security decision — signatures,
    /// integrity against an adversary, password derivation. They exist here
    /// for LEGACY INTEROP with formats and tools that already chose them.
    ///
    /// Not decoration: `broken_digests_carry_the_warning` asserts that every
    /// algorithm answering `true` here has the warning in the registry
    /// description a user actually reads, so adding a future weak algorithm
    /// without saying so fails the build. Test-only — the runtime never
    /// branches on it, because refusing a broken hash a caller explicitly
    /// asked for would be the wrong kind of paternalism.
    #[cfg(test)]
    pub(crate) fn is_broken(self) -> bool {
        matches!(self, Self::Md5 | Self::Sha1)
    }

    /// The spelling `hash_file(path, algo)` accepts — the inverse of
    /// [`from_name`](Self::from_name).
    pub(crate) fn name(self) -> &'static str {
        match self {
            Self::Md5 => "md5",
            Self::Sha1 => "sha1",
            Self::Sha256 => "sha256",
            Self::Blake3 => "blake3",
        }
    }

    /// Every algorithm, in one place, so `hash_file`'s "expected …" error
    /// and the tests are both DERIVED from the enum rather than repeating a
    /// list that goes stale the next time one is added.
    pub(crate) const ALL: &'static [DigestAlgo] =
        &[Self::Md5, Self::Sha1, Self::Sha256, Self::Blake3];

    /// `"md5", "sha1", "sha256" or "blake3"` — the accepted-values half of
    /// an error message, built from `ALL`.
    pub(crate) fn accepted_list() -> String {
        let names: Vec<String> = Self::ALL.iter().map(|a| format!("\"{}\"", a.name())).collect();
        match names.split_last() {
            Some((last, rest)) if !rest.is_empty() => format!("{} or {last}", rest.join(", ")),
            _ => names.join(""),
        }
    }

    /// One-shot digest of a whole buffer.
    pub(crate) fn digest(self, data: &[u8]) -> Vec<u8> {
        match self {
            Self::Md5 => {
                use md5::Digest as _;
                md5::Md5::digest(data).to_vec()
            }
            Self::Sha1 => {
                use sha1::Digest as _;
                sha1::Sha1::digest(data).to_vec()
            }
            Self::Sha256 => {
                use sha2::Digest as _;
                sha2::Sha256::digest(data).to_vec()
            }
            Self::Blake3 => blake3::hash(data).as_bytes().to_vec(),
        }
    }
}

/// Read the trailing `{raw: true}` option shared by every `hash_*` builtin
/// and render the digest accordingly (v0.66.0).
///
/// Before this, `hash_sha256("abc", {raw: true})` **silently returned the
/// hex string** — a surplus argument accepted and discarded, which is the
/// worst answer available: the caller asked for bytes, got text, and was
/// told nothing. Unknown keys and a non-map option now raise rather than
/// being ignored, for the same reason.
#[cfg(feature = "crypto")]
fn hash_output(name: &str, digest: Vec<u8>, opts: Option<&Value>) -> MixResult<Option<Value>> {
    let mut raw = false;
    match opts {
        None | Some(Value::Nil) => {}
        Some(Value::Map(m)) => {
            for key in m.keys() {
                if key != "raw" {
                    return Err(MixError::structured(
                        "OPTION_INVALID",
                        format!("{name}(): unknown option '{key}' (only 'raw' is accepted)"),
                    ));
                }
            }
            if let Some(v) = m.get("raw") {
                // STRICT bool, deliberately unlike `bytes_to_string`'s `lossy`,
                // which takes `is_truthy`. `raw` decides the RETURN TYPE, so a
                // `{raw: "false"}` read out of a config string would silently
                // hand back bytes and fail somewhere else entirely — the same
                // accepted-but-wrong shape this release exists to remove. A
                // flag that only changes a decode mode can afford truthiness;
                // one that changes the type cannot.
                match v {
                    Value::Bool(b) => raw = *b,
                    other => {
                        return Err(MixError::structured(
                            "OPTION_INVALID",
                            format!(
                                "{name}(): option 'raw' must be true or false, got {} — it \
                                 selects the RETURN TYPE (bytes vs hex string), so a truthy \
                                 non-bool is refused rather than guessed",
                                other.type_name()
                            ),
                        ));
                    }
                }
            }
        }
        Some(other) => {
            return Err(MixError::structured(
                "OPTION_INVALID",
                format!(
                    "{name}(): options must be a map or nil, got {}",
                    other.type_name()
                ),
            ));
        }
    }
    if raw {
        Ok(Some(Value::bytes(digest)))
    } else {
        Ok(Some(Value::String(hex_encode(&digest))))
    }
}

#[cfg(feature = "crypto")]
fn builtin_hash_blake3(args: Vec<Value>) -> MixResult<Option<Value>> {
    hash_value("hash_blake3", DigestAlgo::Blake3, args)
}

#[cfg(feature = "crypto")]
fn builtin_hash_sha256(args: Vec<Value>) -> MixResult<Option<Value>> {
    hash_value("hash_sha256", DigestAlgo::Sha256, args)
}

/// `hash_md5($v[, {raw: true}])` — ⚠ BROKEN hash, legacy interop only.
#[cfg(feature = "crypto")]
fn builtin_hash_md5(args: Vec<Value>) -> MixResult<Option<Value>> {
    hash_value("hash_md5", DigestAlgo::Md5, args)
}

/// `hash_sha1($v[, {raw: true}])` — ⚠ BROKEN hash, legacy interop only.
#[cfg(feature = "crypto")]
fn builtin_hash_sha1(args: Vec<Value>) -> MixResult<Option<Value>> {
    hash_value("hash_sha1", DigestAlgo::Sha1, args)
}

/// Shared body of every in-memory `hash_*` builtin: one arity rule, one
/// input coercion, one option surface. Named `hash_<algo>` rather than a
/// bare `md5`/`sha256` on purpose — the existing family is `hash_*`, and a
/// bare spelling beside it would recreate the two-families-one-letter-apart
/// split that `byte_*` vs `bytes_*` already cost a doc note (v0.64.0).
#[cfg(feature = "crypto")]
fn hash_value(name: &str, algo: DigestAlgo, args: Vec<Value>) -> MixResult<Option<Value>> {
    expect_args_between(name, &args, 1, 2)?;
    let buf = value_as_crypto_bytes(&args[0]);
    hash_output(name, algo.digest(buf.as_ref()), args.get(1))
}

/// `hmac_sha256(key, msg)` — RFC 2104 HMAC over the existing sha2 dep (no
/// extra crate for one construction): K' = pad-or-hash key to the 64-byte
/// block, then H((K'⊕opad) ‖ H((K'⊕ipad) ‖ msg)). Primary consumer: webhook
/// signature verification (Stripe-Signature v1 = HMAC-SHA256 of
/// "<timestamp>.<payload>" with the endpoint secret).
#[cfg(feature = "crypto")]
fn builtin_hmac_sha256(args: Vec<Value>) -> MixResult<Option<Value>> {
    expect_args_between("hmac_sha256", &args, 2, 3)?;
    use sha2::Digest;
    const BLOCK: usize = 64;
    let key_in = value_as_crypto_bytes(&args[0]);
    let msg = value_as_crypto_bytes(&args[1]);
    let mut key = [0u8; BLOCK];
    if key_in.as_ref().len() > BLOCK {
        let mut h = sha2::Sha256::new();
        h.update(key_in.as_ref());
        key[..32].copy_from_slice(&h.finalize());
    } else {
        key[..key_in.as_ref().len()].copy_from_slice(key_in.as_ref());
    }
    let mut inner = sha2::Sha256::new();
    let ipad: Vec<u8> = key.iter().map(|b| b ^ 0x36).collect();
    inner.update(&ipad);
    inner.update(msg.as_ref());
    let inner_hash = inner.finalize();
    let mut outer = sha2::Sha256::new();
    let opad: Vec<u8> = key.iter().map(|b| b ^ 0x5c).collect();
    outer.update(&opad);
    outer.update(inner_hash);
    // `{raw: true}` (v0.66.0) returns the 32 MAC bytes rather than hex, so a
    // caller can `constant_time_eq` them against a decoded signature without
    // a hex round trip in between.
    hash_output("hmac_sha256", outer.finalize().to_vec(), args.get(2))
}

/// `constant_time_eq(a, b)` — length-checked, full-scan equality with no
/// data-dependent early exit. Plain `==` short-circuits on the first
/// differing byte, which leaks a timing oracle when comparing a computed MAC
/// against an attacker-supplied signature; this is the verification-side
/// companion to `hmac_sha256`. (A length mismatch returns false immediately —
/// MAC lengths are public.)
#[cfg(feature = "crypto")]
fn builtin_constant_time_eq(args: Vec<Value>) -> MixResult<Option<Value>> {
    expect_args("constant_time_eq", &args, 2)?;
    let a = value_as_crypto_bytes(&args[0]);
    let b = value_as_crypto_bytes(&args[1]);
    let (a, b) = (a.as_ref(), b.as_ref());
    if a.len() != b.len() {
        return Ok(Some(Value::Bool(false)));
    }
    let mut diff = 0u8;
    for i in 0..a.len() {
        diff |= a[i] ^ b[i];
    }
    Ok(Some(Value::Bool(diff == 0)))
}

/// `hash_file($path[, $algo])` — hex digest of a file's contents, read as a
/// **stream**.
///
/// The string twins `hash_sha256`/`hash_blake3` hash an in-memory value, so
/// hashing a file through them means `hash_sha256(read_file(p))` — which
/// slurps the whole file into a Mix string first (and `read_file` rejects
/// non-UTF-8 outright). `hash_file` reads in 64 KiB chunks, so a
/// multi-hundred-MB artifact (a rootfs tarball, a release image) is hashed
/// with bounded memory and no encoding constraint. This is the factory's
/// "manifest hashing" primitive (name every release artifact + its digest,
/// then sign the manifest) — hence a builtin, not a `sha256sum` shell-out.
///
/// `$algo` defaults to `"sha256"`; `"blake3"` mirrors the string pair. The
/// sha256 output is byte-for-byte what `hash_sha256(read_file(p))` returns
/// for a UTF-8 file (same lowercase hex encoding). Capability: **FsRead** —
/// it opens a path, so a Pure-only sandbox denies it.
#[cfg(feature = "crypto")]
fn builtin_hash_file(args: Vec<Value>) -> MixResult<Option<Value>> {
    use std::io::Read as _;
    expect_args_between("hash_file", &args, 1, 3)?;
    let path = args[0].to_mix_string();
    let algo = match args.get(1) {
        None | Some(Value::Nil) => "sha256".to_string(),
        // A map in the ALGO slot is almost always the options map put one
        // position early. Stringifying it would report `unknown algorithm
        // '{raw: true}'`, which sends the caller looking for a typo in an
        // algorithm name they never wrote.
        Some(Value::Map(_)) => {
            return Err(MixError::structured(
                "OPTION_INVALID",
                format!(
                    "hash_file(): argument 2 is the ALGORITHM ({}), not the options map — \
                     write hash_file(path, \"sha256\", {{raw: true}})",
                    DigestAlgo::accepted_list()
                ),
            ));
        }
        Some(v) => v.to_mix_string(),
    };
    // Validate the algorithm BEFORE touching the filesystem: an unknown algo
    // is a script bug and should report it as such, not open (and possibly
    // block on) the path first. The name set is `DigestAlgo`'s, shared with
    // the in-memory `hash_*` builtins (v0.66.0) so the two surfaces cannot
    // offer different algorithms.
    let which = match DigestAlgo::from_name(&algo) {
        Some(a) => a,
        None => {
            return Err(MixError::RuntimeError {
                span: None,
                msg: format!(
                    "hash_file: unknown algorithm '{}' (expected {})",
                    algo,
                    DigestAlgo::accepted_list()
                ),
            });
        }
    };
    let mut f = std::fs::File::open(&path).map_err(|e| MixError::RuntimeError {
        span: None,
        msg: format!("hash_file '{}': {}", path, e),
    })?;
    // Streaming, so the file is never held in memory: the point of hash_file
    // over `hash_sha256(read_file_bytes(p))` is a fixed 64 KiB working set
    // whatever the file's size. `StreamHasher` holds exactly ONE hasher —
    // instantiating all four and using one would carry blake3's ~2 KB state
    // on every call regardless of the algorithm asked for.
    let mut hasher = StreamHasher::new(which);
    let mut buf = [0u8; 65536];
    loop {
        let n = f.read(&mut buf).map_err(|e| MixError::RuntimeError {
            span: None,
            msg: format!("hash_file '{}': {}", path, e),
        })?;
        if n == 0 {
            break;
        }
        hasher.update(&buf[..n]);
    }
    hash_output("hash_file", hasher.finalize(), args.get(2))
}

/// One incremental hasher, selected at construction — the streaming twin of
/// [`DigestAlgo::digest`].
///
/// Kept beside `DigestAlgo` so `hash_file` and the in-memory `hash_*` family
/// can never end up supporting different algorithm sets: adding a variant to
/// the enum makes THIS match non-exhaustive, so the compiler asks for the
/// streaming arm at the same time.
#[cfg(feature = "crypto")]
enum StreamHasher {
    Md5(Box<md5::Md5>),
    Sha1(Box<sha1::Sha1>),
    Sha256(Box<sha2::Sha256>),
    Blake3(Box<blake3::Hasher>),
}

#[cfg(feature = "crypto")]
impl StreamHasher {
    fn new(algo: DigestAlgo) -> Self {
        match algo {
            DigestAlgo::Md5 => Self::Md5(Box::default()),
            DigestAlgo::Sha1 => Self::Sha1(Box::default()),
            DigestAlgo::Sha256 => Self::Sha256(Box::default()),
            DigestAlgo::Blake3 => Self::Blake3(Box::new(blake3::Hasher::new())),
        }
    }

    fn update(&mut self, chunk: &[u8]) {
        match self {
            Self::Md5(h) => {
                use md5::Digest as _;
                h.update(chunk);
            }
            Self::Sha1(h) => {
                use sha1::Digest as _;
                h.update(chunk);
            }
            Self::Sha256(h) => {
                use sha2::Digest as _;
                h.update(chunk);
            }
            Self::Blake3(h) => {
                h.update(chunk);
            }
        }
    }

    fn finalize(self) -> Vec<u8> {
        match self {
            Self::Md5(h) => {
                use md5::Digest as _;
                h.finalize().to_vec()
            }
            Self::Sha1(h) => {
                use sha1::Digest as _;
                h.finalize().to_vec()
            }
            Self::Sha256(h) => {
                use sha2::Digest as _;
                h.finalize().to_vec()
            }
            Self::Blake3(h) => h.finalize().as_bytes().to_vec(),
        }
    }
}

#[cfg(feature = "crypto")]
fn builtin_uuid(_args: Vec<Value>) -> MixResult<Option<Value>> {
    Ok(Some(Value::String(uuid::Uuid::new_v4().to_string())))
}

// --- HTTP builtins (feature-gated) ---

/// Extract an optional trailing headers map (`{name: value, ...}`) into a
/// flat list. A non-map arg (or `None`) yields no headers — callers never
/// error on a missing/ill-typed headers slot, matching the prior lenient
/// `if let Some(Value::Map(_))` behaviour of `http_get`/`http_post`.
///
/// `Value::Bytes` header values are rejected (not stringified) so the
/// `<bytes:N>` placeholder never ships over the network as a header
/// value. HTTP headers are text; a script wanting binary header
/// metadata should `base64_encode($v)` explicitly.
#[cfg(feature = "http")]
fn http_headers_from(arg: Option<&Value>) -> MixResult<Vec<(String, String)>> {
    match arg {
        Some(Value::Map(headers)) => headers
            .iter()
            .map(|(k, v)| match v {
                Value::Bytes(_) | Value::Buffer(_) => Err(MixError::RuntimeError {
                    span: None,
                    msg: format!(
                        "http: header `{k}` does not accept bytes/buffer; \
                         base64_encode($v) first"
                    ),
                }),
                other => Ok((k.clone(), other.to_mix_string())),
            })
            .collect(),
        _ => Ok(Vec::new()),
    }
}

/// Shared HTTP core for `http_get` / `http_post` / `http_request`.
///
/// Always returns a `Value::Map`, never a `MixError` — network/protocol
/// failures are reported in-band so scripts branch on `status` rather than
/// wrapping calls in `try`.
///
/// An HTTP error *status* (4xx/5xx) is a response, not a transport failure:
/// it yields `{status: <code>, body: <text>}` so REST callers can read the
/// real code and any error payload. Only genuine transport errors (DNS,
/// TLS, connect refused) collapse to `{status: 0, error: <msg>}`.
///
/// NOTE: this surfacing of 4xx/5xx as a real `status` is a behavioural
/// change for the pre-existing `http_get`/`http_post`, which previously
/// reported every `ureq` error — including HTTP error statuses — as
/// `{status: 0, error}`. The old behaviour made a 404 indistinguishable
/// from a DNS failure; the new behaviour is strictly more informative and
/// a `status == 200` (or 2xx-range) check, the documented success test,
/// is unaffected.
/// True iff every byte is an RFC 7230 `tchar` (the method-name grammar).
/// `ureq` 2.12.1 writes the method into the request line unvalidated, so a
/// script-supplied method containing SP/CR/LF would corrupt or inject the
/// request. Methods are upper-cased before this check, but case is
/// irrelevant — the point is rejecting non-token bytes.
#[cfg(feature = "http")]
fn is_http_token(s: &str) -> bool {
    !s.is_empty()
        && s.bytes()
            .all(|b| b.is_ascii_alphanumeric() || b"!#$%&'*+-.^_`|~".contains(&b))
}

/// Decorate the response map with `status` / `body` / `bytes` from a
/// drained byte buffer. Pure: no I/O, no `ureq` types — split out from
/// `finalise_http_response` so the UTF-8-or-Nil branch is unit-testable
/// without a live HTTP server.
///
/// `body` is `Value::String(_)` when `buf` is valid UTF-8 and
/// `Value::Nil` otherwise — non-UTF-8 responses (images, archives,
/// anything binary) used to silently corrupt through `into_string`'s
/// lossy decode, turning every high-bit byte into `EF BF BD` (U+FFFD).
/// `bytes` always carries the raw buffer as `Value::Bytes`, so binary
/// callers reach for `.bytes` and the text path stays unchanged for
/// JSON/text responses where `.body` is still a `String`.
#[cfg(feature = "http")]
fn http_body_into_map(status: u16, buf: Vec<u8>, map: &mut indexmap::IndexMap<String, Value>) {
    map.insert("status".into(), Value::Number(status as f64));
    map.insert(
        "body".into(),
        match std::str::from_utf8(&buf) {
            Ok(s) => Value::String(s.to_string()),
            Err(_) => Value::Nil,
        },
    );
    map.insert("bytes".into(), Value::bytes(buf));
}

/// Drain an `ureq::Response` body into a `Vec<u8>` and decorate the
/// response map with `status` / `body` / `bytes`. See `http_body_into_map`
/// for the body/bytes semantics.
///
/// A mid-stream read error after a header-OK response collapses the
/// whole call to the transport-error shape — the partial buffer would
/// be misleading.
/// Hard cap on an HTTP response body: 64 MiB. A huge/streaming response
/// must collapse to the transport-error shape, not OOM an embedding
/// daemon. Read via `.take(CAP + 1)` so an over-cap body is detected
/// after buffering at most one byte past the cap.
#[cfg(feature = "http")]
const MAX_HTTP_BODY_BYTES: u64 = 67_108_864;

#[cfg(feature = "http")]
fn finalise_http_response(
    method: &str,
    status: u16,
    resp: ureq::Response,
    map: &mut indexmap::IndexMap<String, Value>,
) {
    use std::io::Read;
    // A HEAD response mirrors the headers a GET would return — including
    // `Content-Encoding: gzip` — but the server MUST NOT send a body
    // (RFC 9110 §9.3.2). `ureq` correctly forces a HEAD body to zero length
    // (`std::io::empty()`), but then wraps that empty reader in a gzip/brotli
    // decoder because of the `Content-Encoding` header; draining it makes the
    // decoder read a compression header that never arrives, so `read_to_end`
    // returns `UnexpectedEof` and an otherwise-fine HEAD collapses to the
    // transport-error shape (`{status: 0, error: "http: body read failed:
    // unexpected end of file"}`). Real servers (example.com, github.com) send
    // `Content-Encoding` on HEAD and trip this; a plain Content-Length HEAD
    // does not. A bodyless response is an empty body, not a failure: skip the
    // drain for HEAD and report the real status with an empty body. `method`
    // reaches here already upper-cased by every caller.
    if method == "HEAD" {
        http_body_into_map(status, Vec::new(), map);
        return;
    }
    let mut buf: Vec<u8> = Vec::new();
    match resp
        .into_reader()
        .take(MAX_HTTP_BODY_BYTES + 1)
        .read_to_end(&mut buf)
    {
        Ok(_) if buf.len() as u64 > MAX_HTTP_BODY_BYTES => {
            map.insert("status".into(), Value::Number(0.0));
            map.insert(
                "error".into(),
                Value::String(format!(
                    "http: response body exceeds the {} byte cap (64 MiB)",
                    MAX_HTTP_BODY_BYTES
                )),
            );
            map.insert("error_code".into(), Value::String("HTTP_BODY_LIMIT".into()));
        }
        Ok(_) => http_body_into_map(status, buf, map),
        Err(e) => {
            map.insert("status".into(), Value::Number(0.0));
            // A timeout while draining the body is a timeout, not a
            // generic body error (codex convergence review).
            let code = if e.kind() == std::io::ErrorKind::TimedOut {
                "HTTP_TIMEOUT"
            } else {
                "HTTP_BODY"
            };
            map.insert(
                "error".into(),
                Value::String(format!("http: body read failed: {e}")),
            );
            map.insert("error_code".into(), Value::String(code.into()));
        }
    }
}

/// HTTP request body — `Text` sends as UTF-8 string, `Bytes` sends raw
/// bytes via `ureq::send_bytes`. Constructed by the caller from a
/// `Value::Bytes` (raw upload) or any other value coerced via
/// `to_mix_string` (text upload). The split prevents `Value::Bytes`
/// silently going through `to_mix_string`, which would send the
/// `<bytes:N>` placeholder instead of the buffer.
#[cfg(feature = "http")]
enum HttpBody<'a> {
    Text(&'a str),
    Bytes(&'a [u8]),
}

#[cfg(feature = "http")]
fn http_body_from(value: &Value) -> (Option<String>, Option<Vec<u8>>) {
    match value {
        Value::Bytes(b) => (None, Some(b.to_vec())),
        // A mutable buffer sends its current bytes raw, like Bytes.
        Value::Buffer(b) => (None, Some(b.borrow().clone())),
        other => (Some(other.to_mix_string()), None),
    }
}

/// Default wall-clock bound for the `http_*` builtins, seconds. A stalled
/// server used to hang the evaluator (and a login shell) FOREVER — ureq has
/// no timeout unless one is set. Override per call with `{timeout: N}`
/// (0 disables the deadline for a deliberately long transfer).
#[cfg(feature = "http")]
const HTTP_DEFAULT_TIMEOUT_S: u64 = 30;

/// A cached ureq agent that skips TLS certificate + hostname verification,
/// for `http_*(..., {ssl_verify: false})`. The TLS handshake still runs —
/// signatures are checked against the ring provider's algorithms — only the
/// certificate-chain / hostname trust decision is bypassed, exactly like
/// `curl -k`. Built once, lazily, on the first insecure call. Used for
/// self-signed internal endpoints (e.g. Proxmox/PBS APIs) where pinning a CA
/// is impractical; never the default.
#[cfg(feature = "http")]
fn insecure_http_agent() -> ureq::Agent {
    use std::sync::{Arc, OnceLock};
    use ureq::rustls;

    #[derive(Debug)]
    struct NoCertVerify {
        algs: rustls::crypto::WebPkiSupportedAlgorithms,
    }

    impl rustls::client::danger::ServerCertVerifier for NoCertVerify {
        fn verify_server_cert(
            &self,
            _end_entity: &rustls::pki_types::CertificateDer<'_>,
            _intermediates: &[rustls::pki_types::CertificateDer<'_>],
            _server_name: &rustls::pki_types::ServerName<'_>,
            _ocsp_response: &[u8],
            _now: rustls::pki_types::UnixTime,
        ) -> Result<rustls::client::danger::ServerCertVerified, rustls::Error> {
            Ok(rustls::client::danger::ServerCertVerified::assertion())
        }

        fn verify_tls12_signature(
            &self,
            message: &[u8],
            cert: &rustls::pki_types::CertificateDer<'_>,
            dss: &rustls::DigitallySignedStruct,
        ) -> Result<rustls::client::danger::HandshakeSignatureValid, rustls::Error> {
            rustls::crypto::verify_tls12_signature(message, cert, dss, &self.algs)
        }

        fn verify_tls13_signature(
            &self,
            message: &[u8],
            cert: &rustls::pki_types::CertificateDer<'_>,
            dss: &rustls::DigitallySignedStruct,
        ) -> Result<rustls::client::danger::HandshakeSignatureValid, rustls::Error> {
            rustls::crypto::verify_tls13_signature(message, cert, dss, &self.algs)
        }

        fn supported_verify_schemes(&self) -> Vec<rustls::SignatureScheme> {
            self.algs.supported_schemes()
        }
    }

    static AGENT: OnceLock<ureq::Agent> = OnceLock::new();
    AGENT
        .get_or_init(|| {
            let provider = rustls::crypto::ring::default_provider();
            let algs = provider.signature_verification_algorithms;
            let config = rustls::ClientConfig::builder_with_provider(Arc::new(provider))
                .with_safe_default_protocol_versions()
                .expect("ring provider supports rustls default protocol versions")
                .dangerous()
                .with_custom_certificate_verifier(Arc::new(NoCertVerify { algs }))
                .with_no_client_auth();
            ureq::builder().tls_config(Arc::new(config)).build()
        })
        .clone()
}

/// A HTTP_TLS structured error (private-CA loading/parsing problems —
/// the pre-request configuration class; handshake failures at request
/// time stay in the returned `{status: 0, error}` shape).
#[cfg(feature = "http")]
fn http_tls_err(name: &str, msg: impl std::fmt::Display) -> MixError {
    MixError::structured("HTTP_TLS", format!("{name}: {msg}"))
}

/// Cap on `ca_file`/`ca_pem` input (D8): 4 MiB is generous for any CA
/// bundle and stops an accidental huge-file read.
#[cfg(feature = "http")]
const HTTP_CA_MAX_BYTES: u64 = 4 * 1024 * 1024;

/// Build an agent whose root store = the default webpki (Mozilla)
/// roots PLUS the caller's PEM certificates — private-CA trust is
/// ADDITIVE (D8): normal chain building and hostname verification
/// still run; this never weakens verification for public hosts.
#[cfg(feature = "http")]
fn ca_http_agent(name: &str, pem: &[u8]) -> MixResult<ureq::Agent> {
    use std::sync::Arc;
    use ureq::rustls;

    if pem.is_empty() {
        return Err(http_tls_err(name, "ca certificate input is empty"));
    }
    let mut roots = rustls::RootCertStore::empty();
    roots.extend(webpki_roots::TLS_SERVER_ROOTS.iter().cloned());
    let mut added = 0usize;
    for cert in rustls_pemfile::certs(&mut &pem[..]) {
        let cert = cert.map_err(|e| http_tls_err(name, format!("invalid PEM: {e}")))?;
        roots
            .add(cert)
            .map_err(|e| http_tls_err(name, format!("invalid certificate: {e}")))?;
        added += 1;
    }
    if added == 0 {
        return Err(http_tls_err(name, "no certificates found in PEM input"));
    }
    let provider = rustls::crypto::ring::default_provider();
    let config = rustls::ClientConfig::builder_with_provider(Arc::new(provider))
        .with_safe_default_protocol_versions()
        .expect("ring provider supports rustls default protocol versions")
        .with_root_certificates(roots)
        .with_no_client_auth();
    Ok(ureq::builder().tls_config(Arc::new(config)).build())
}

#[cfg(feature = "http")]
fn http_dispatch(
    method: &str,
    url: &str,
    body: Option<HttpBody<'_>>,
    headers: &[(String, String)],
    timeout_s: u64,
    insecure: bool,
    ca_agent: Option<&ureq::Agent>,
) -> Value {
    if !is_http_token(method) {
        let mut map = indexmap::IndexMap::new();
        map.insert("status".into(), Value::Number(0.0));
        map.insert(
            "error".into(),
            Value::String(format!(
                "http: invalid request method {:?} (must be an RFC 7230 token)",
                method
            )),
        );
        // Keep the v2 shape consistent even on this pre-flight reject.
        map.insert("error_code".into(), Value::String("HTTP_PROTOCOL".into()));
        map.insert("duration_ms".into(), Value::Number(0.0));
        return Value::map(map);
    }
    let mut req = if insecure {
        insecure_http_agent().request(method, url)
    } else if let Some(agent) = ca_agent {
        agent.request(method, url)
    } else {
        ureq::request(method, url)
    };
    if timeout_s > 0 {
        // Total-request deadline (connect + transfer), like ssh_run's
        // wall-clock bound.
        req = req.timeout(std::time::Duration::from_secs(timeout_s));
    }
    for (k, v) in headers {
        req = req.set(k, v);
    }
    let started = std::time::Instant::now();
    let result = match body {
        Some(HttpBody::Text(b)) => req.send_string(b),
        Some(HttpBody::Bytes(b)) => req.send_bytes(b),
        None => req.call(),
    };
    let mut map = indexmap::IndexMap::new();
    match result {
        Ok(resp) => {
            let status = resp.status();
            http_response_meta_into_map(&resp, &mut map);
            finalise_http_response(method, status, resp, &mut map);
        }
        Err(ureq::Error::Status(code, resp)) => {
            // A 4xx/5xx is a real HTTP response — keep its status, body,
            // headers, and final_url (D8: error:nil, not a transport
            // failure).
            http_response_meta_into_map(&resp, &mut map);
            finalise_http_response(method, code, resp, &mut map);
        }
        Err(e) => {
            // Transport failure (connect refused, TLS, timeout, DNS).
            map.insert("status".into(), Value::Number(0.0));
            map.insert("error".into(), Value::String(e.to_string()));
            map.insert(
                "error_code".into(),
                Value::String(http_transport_error_code(&e).into()),
            );
        }
    }
    // v2 additive fields present on every path (0.30.0).
    map.entry("duration_ms".into())
        .or_insert_with(|| Value::Number(started.elapsed().as_millis() as f64));
    map.entry("error_code".into()).or_insert(Value::Nil);
    map.entry("error".into()).or_insert(Value::Nil);
    Value::map(map)
}

/// Extract response headers (lowercase names → list-of-string values,
/// preserving repeated fields) and the final URL (after redirects) into
/// the result map — the D8 v2 response-completeness additions. Called
/// BEFORE `finalise_http_response` consumes the response reader.
#[cfg(feature = "http")]
fn http_response_meta_into_map(resp: &ureq::Response, map: &mut indexmap::IndexMap<String, Value>) {
    map.insert(
        "final_url".into(),
        Value::String(resp.get_url().to_string()),
    );
    let mut headers = indexmap::IndexMap::new();
    for name in resp.headers_names() {
        let lname = name.to_ascii_lowercase();
        // ureq 2 exposes multi-valued headers via `all(name)`.
        let values: Vec<Value> = resp
            .all(&name)
            .into_iter()
            .map(|v| Value::String(v.to_string()))
            .collect();
        headers.insert(lname, Value::list(values));
    }
    map.insert("headers".into(), Value::map(headers));
}

/// Classify a ureq transport error into a stable HTTP_* code (D8) from
/// the TYPED `ErrorKind` rather than display text — a ureq/rustls
/// wording change can't silently reclassify a failure (codex 0.30
/// review, MAJOR). Only the TLS-vs-generic-Io split falls back to the
/// source chain, because ureq wraps rustls errors inside `Io` with no
/// dedicated kind.
#[cfg(feature = "http")]
fn http_transport_error_code(e: &ureq::Error) -> &'static str {
    use ureq::ErrorKind;
    // Timeouts and TLS are recognised by TYPE across the source chain,
    // before the kind-based fallback — a rustls handshake failure
    // surfaces as ConnectionFailed in ureq 2.12, so both branches must
    // consult these (codex convergence review, MAJOR).
    if source_is_timeout(e) {
        return "HTTP_TIMEOUT";
    }
    if source_is_tls(e) {
        return "HTTP_TLS";
    }
    match e.kind() {
        ErrorKind::Dns | ErrorKind::ConnectionFailed | ErrorKind::ProxyConnect => "HTTP_CONNECT",
        ErrorKind::TooManyRedirects => "HTTP_REDIRECT",
        ErrorKind::InvalidUrl
        | ErrorKind::UnknownScheme
        | ErrorKind::BadStatus
        | ErrorKind::BadHeader => "HTTP_PROTOCOL",
        _ => "HTTP_TRANSPORT",
    }
}

/// Whether a rustls TLS error appears anywhere in the source chain —
/// a TYPED downcast (`ureq::rustls::Error`), not a text/type-name
/// guess. Returns false (→ generic classification) when the concrete
/// type isn't reachable, so it degrades safely.
#[cfg(feature = "http")]
fn source_is_tls(e: &(dyn std::error::Error + 'static)) -> bool {
    let mut cur: Option<&(dyn std::error::Error + 'static)> = Some(e);
    while let Some(err) = cur {
        if err.downcast_ref::<ureq::rustls::Error>().is_some() {
            return true;
        }
        cur = err.source();
    }
    false
}

/// Walk the error source chain for a TimedOut io error.
#[cfg(feature = "http")]
fn source_is_timeout(e: &(dyn std::error::Error + 'static)) -> bool {
    let mut cur: Option<&(dyn std::error::Error + 'static)> = Some(e);
    while let Some(err) = cur {
        if let Some(io) = err.downcast_ref::<std::io::Error>()
            && io.kind() == std::io::ErrorKind::TimedOut
        {
            return true;
        }
        cur = err.source();
    }
    false
}

/// Resolve the trailing `[headers], [opts]` slots of an `http_*` builtin.
/// When only ONE trailing map is passed and its sole key is `timeout`, it is
/// the opts map — `http_get(url, {timeout: 1})` must apply the deadline, not
/// silently send a `timeout` header and leave the call unbounded. To send a
/// literal `timeout` HTTP header, pass the opts slot explicitly:
/// `http_get(url, {timeout: "60s"}, {})`.
#[cfg(feature = "http")]
const HTTP_OPT_KEYS: [&str; 4] = ["timeout", "ssl_verify", "ca_file", "ca_pem"];

/// True when a lone trailing map is the OPTS map (not headers): non-empty and
/// every key is a recognised http opt. Lets `http_get(url, {ssl_verify: false})`
/// or `http_get(url, {timeout: 5})` be read as options, while any map carrying a
/// non-opt key is treated as request headers. To send a literal `timeout` /
/// `ssl_verify` HTTP header, pass the opts slot explicitly (`http_get(url, hdrs, {})`).
#[cfg(feature = "http")]
fn http_map_is_opts(m: &indexmap::IndexMap<String, Value>) -> bool {
    !m.is_empty() && m.keys().all(|k| HTTP_OPT_KEYS.contains(&k.as_str()))
}

/// Parsed http opts: wall-clock timeout, `curl -k`-style verification
/// bypass, and an optional private-CA agent (`ca_file`/`ca_pem`, 0.29.0).
#[cfg(feature = "http")]
#[derive(Debug)]
struct HttpOpts {
    timeout_s: u64,
    insecure: bool,
    ca_agent: Option<ureq::Agent>,
}

/// Parse the http opts map `{timeout, ssl_verify, ca_file, ca_pem}`.
/// `ca_file` and `ca_pem` are mutually exclusive with each other and
/// with `ssl_verify: false` (a pin-or-CA option combined with disabled
/// verification would be self-contradictory — D8). CA input problems
/// raise HTTP_TLS; option-shape problems raise OPTION_INVALID.
#[cfg(feature = "http")]
fn parse_http_opts(name: &str, v: Option<&Value>) -> MixResult<HttpOpts> {
    let defaults = HttpOpts {
        timeout_s: HTTP_DEFAULT_TIMEOUT_S,
        insecure: false,
        ca_agent: None,
    };
    let map = match v {
        None | Some(Value::Nil) => return Ok(defaults),
        Some(Value::Map(m)) => m,
        Some(other) => {
            return Err(MixError::structured(
                "OPTION_INVALID",
                format!(
                    "{name}: opts must be a map like {{timeout: 30, ssl_verify: false}}, got {}",
                    other.type_name()
                ),
            ));
        }
    };
    for key in map.keys() {
        if !HTTP_OPT_KEYS.contains(&key.as_str()) {
            return Err(MixError::structured(
                "OPTION_INVALID",
                format!(
                    "{name}: unknown opt {key:?} (supported: {})",
                    HTTP_OPT_KEYS.join(", ")
                ),
            ));
        }
    }
    let timeout_s = match map.get("timeout") {
        Some(v) => parse_nonneg_int_opt(name, "timeout", v)?,
        None => HTTP_DEFAULT_TIMEOUT_S,
    };
    let insecure = match map.get("ssl_verify") {
        Some(Value::Bool(b)) => !b,
        None => false,
        Some(_) => {
            return Err(MixError::structured(
                "OPTION_INVALID",
                format!("{name}: ssl_verify must be a boolean"),
            ));
        }
    };
    let ca_file = map.get("ca_file");
    let ca_pem = map.get("ca_pem");
    if ca_file.is_some() && ca_pem.is_some() {
        return Err(MixError::structured(
            "OPTION_INVALID",
            format!("{name}: ca_file and ca_pem are mutually exclusive"),
        ));
    }
    if insecure && (ca_file.is_some() || ca_pem.is_some()) {
        return Err(MixError::structured(
            "OPTION_INVALID",
            format!(
                "{name}: ssl_verify: false cannot be combined with ca_file/ca_pem — \
                 a private CA proves the chain, disabling verification discards it"
            ),
        ));
    }
    let pem_bytes: Option<Vec<u8>> = match (ca_file, ca_pem) {
        (Some(Value::String(path)), _) => {
            // Read the ONE opened descriptor through a hard byte cap
            // (codex release review, BLOCKER): a metadata-then-read
            // pair is TOCTOU-racy, and a special file like /dev/zero
            // reports length 0 but never reaches EOF — an unbounded
            // read/alloc. `take(cap + 1)` bounds the read regardless;
            // one extra byte means over-cap.
            use std::io::Read;
            let file = std::fs::File::open(path)
                .map_err(|e| http_tls_err(name, format!("ca_file {path}: {e}")))?;
            let mut buf = Vec::new();
            file.take(HTTP_CA_MAX_BYTES + 1)
                .read_to_end(&mut buf)
                .map_err(|e| http_tls_err(name, format!("ca_file {path}: {e}")))?;
            if buf.len() as u64 > HTTP_CA_MAX_BYTES {
                return Err(http_tls_err(
                    name,
                    format!("ca_file {path} exceeds {HTTP_CA_MAX_BYTES} bytes"),
                ));
            }
            Some(buf)
        }
        (Some(other), _) => {
            return Err(MixError::structured(
                "OPTION_INVALID",
                format!(
                    "{name}: ca_file must be a string path, got {}",
                    other.type_name()
                ),
            ));
        }
        (None, Some(v)) => {
            let bytes: Vec<u8> = match v {
                Value::String(s) => s.as_bytes().to_vec(),
                Value::Bytes(b) => b.as_ref().clone(),
                Value::Buffer(b) => b.borrow().clone(),
                other => {
                    return Err(MixError::structured(
                        "OPTION_INVALID",
                        format!(
                            "{name}: ca_pem must be a string, bytes, or buffer, got {}",
                            other.type_name()
                        ),
                    ));
                }
            };
            if bytes.len() as u64 > HTTP_CA_MAX_BYTES {
                return Err(http_tls_err(
                    name,
                    format!(
                        "ca_pem is {} bytes (max {})",
                        bytes.len(),
                        HTTP_CA_MAX_BYTES
                    ),
                ));
            }
            Some(bytes)
        }
        (None, None) => None,
    };
    let ca_agent = match pem_bytes {
        Some(bytes) => Some(ca_http_agent(name, &bytes)?),
        None => None,
    };
    Ok(HttpOpts {
        timeout_s,
        insecure,
        ca_agent,
    })
}

/// Resolved trailing slots of an `http_*` call: request headers plus
/// the parsed opts.
#[cfg(feature = "http")]
type HttpHeadersOpts = (Vec<(String, String)>, HttpOpts);

#[cfg(feature = "http")]
fn http_headers_and_timeout(
    name: &str,
    headers_arg: Option<&Value>,
    opts_arg: Option<&Value>,
) -> MixResult<HttpHeadersOpts> {
    if opts_arg.is_none()
        && let Some(Value::Map(m)) = headers_arg
        && http_map_is_opts(m)
    {
        let opts = parse_http_opts(name, headers_arg)?;
        return Ok((Vec::new(), opts));
    }
    let headers = http_headers_from(headers_arg)?;
    let opts = parse_http_opts(name, opts_arg)?;
    Ok((headers, opts))
}

#[cfg(feature = "http")]
fn builtin_http_get(args: Vec<Value>) -> MixResult<Option<Value>> {
    expect_args_between("http_get", &args, 1, 3)?;
    let url = args[0].to_mix_string();
    let (headers, opts) = http_headers_and_timeout("http_get", args.get(1), args.get(2))?;
    Ok(Some(http_dispatch(
        "GET",
        &url,
        None,
        &headers,
        opts.timeout_s,
        opts.insecure,
        opts.ca_agent.as_ref(),
    )))
}

#[cfg(feature = "http")]
fn builtin_http_post(args: Vec<Value>) -> MixResult<Option<Value>> {
    expect_args_between("http_post", &args, 2, 4)?;
    let url = args[0].to_mix_string();
    let (text, bytes) = http_body_from(&args[1]);
    let body = match (&text, &bytes) {
        (Some(s), _) => Some(HttpBody::Text(s.as_str())),
        (_, Some(b)) => Some(HttpBody::Bytes(b.as_slice())),
        _ => None,
    };
    let (headers, opts) = http_headers_and_timeout("http_post", args.get(2), args.get(3))?;
    Ok(Some(http_dispatch(
        "POST",
        &url,
        body,
        &headers,
        opts.timeout_s,
        opts.insecure,
        opts.ca_agent.as_ref(),
    )))
}

/// `http_request(method, url, [body], [headers])` — any-verb HTTP.
///
/// `method` is upper-cased so `"get"`, `"Get"`, `"GET"` are equivalent.
/// The body slot is optional and `nil`-tolerant: an absent or `nil` 3rd
/// arg sends no body (a bare request via `.call()`), matching how
/// body-less verbs (GET/HEAD/DELETE/OPTIONS) are expected to behave.
/// A `Value::Bytes` body is sent as raw bytes; anything else is
/// stringified.
#[cfg(feature = "http")]
fn builtin_http_request(args: Vec<Value>) -> MixResult<Option<Value>> {
    expect_args_between("http_request", &args, 2, 5)?;
    let method = args[0].to_mix_string().to_uppercase();
    let url = args[1].to_mix_string();
    // Same trailing-opts rule as http_get/http_post, one slot earlier: a
    // SOLE trailing map whose only key is `timeout` is the opts map, not a
    // request body — `http_request("GET", url, {timeout: 1})` must apply
    // the deadline, never send a stringified-map body. An intentional body
    // of that exact shape needs the later slots spelled out.
    if args.len() == 3
        && let Some(Value::Map(m)) = args.get(2)
        && http_map_is_opts(m)
    {
        let opts = parse_http_opts("http_request", args.get(2))?;
        return Ok(Some(http_dispatch(
            &method,
            &url,
            None,
            &[],
            opts.timeout_s,
            opts.insecure,
            opts.ca_agent.as_ref(),
        )));
    }
    let (text, bytes) = match args.get(2) {
        None | Some(Value::Nil) => (None, None),
        Some(v) => http_body_from(v),
    };
    let body = match (&text, &bytes) {
        (Some(s), _) => Some(HttpBody::Text(s.as_str())),
        (_, Some(b)) => Some(HttpBody::Bytes(b.as_slice())),
        _ => None,
    };
    let (headers, opts) = http_headers_and_timeout("http_request", args.get(3), args.get(4))?;
    Ok(Some(http_dispatch(
        &method,
        &url,
        body,
        &headers,
        opts.timeout_s,
        opts.insecure,
        opts.ca_agent.as_ref(),
    )))
}

// --- DNS builtins (stdlib) ---

fn builtin_dns_lookup(args: Vec<Value>) -> MixResult<Option<Value>> {
    expect_args("dns_lookup", &args, 1)?;
    let host = args[0].to_mix_string();
    let addr = format!("{}:0", host);
    match std::net::ToSocketAddrs::to_socket_addrs(&addr.as_str()) {
        Ok(addrs) => {
            let ips: Vec<Value> = addrs.map(|a| Value::String(a.ip().to_string())).collect();
            Ok(Some(Value::list(ips)))
        }
        Err(e) => Err(MixError::RuntimeError {
            span: None,
            msg: format!("dns_lookup '{}': {}", host, e),
        }),
    }
}

// --- Help ---

fn builtin_help(_args: Vec<Value>) -> MixResult<Option<Value>> {
    println!("Mix — scripting language and shell for the Cosmix stack");
    println!();
    println!("String:    length len upper lower left right substr pos lastpos");
    println!("           strip trim replace split join starts_with ends_with");
    println!("           contains repeat reverse words word lpad rpad");
    println!("Type:      type to_number to_string is_number is_empty");
    println!("List:      push pop shift sort index_of unique range flat concat");
    println!("Map:       keys values has_key merge delete");
    println!("I/O:       print eprintf read_file read_file_bytes write_file append_file");
    println!("           read_stdin read_stdin_bytes");
    println!("           write_stdout write_stderr (aliases print_raw eprint_raw) — v0.65.0:");
    println!("           fd 1/2 as they are, no newline, no separator, bytes verbatim");
    println!("           exists is_dir is_file glob ls mkdir chmod chown stat");
    println!("System:    env time pid args exit sleep run run_rc hostname");
    println!("           cwd chdir platform which");
    println!("Process:   spawn kill process_alive");
    println!("Text:      grep line_count head tail template word_wrap markdown_escape");
    println!("Format:    format_bytes format_number duration_format");
    println!("Path:      basename dirname extname path_join");
    println!("Date:      date_format date_parse now_iso relative_time");
    println!(
        "Parse:     csv_parse ini_parse xml_parse toml_parse toml_encode data_parse data_encode"
    );
    println!("JSON:      json_parse json_encode jq jq_all");
    println!("Regex:     regex_match regex_find regex_replace regex_split");
    println!(
        "Crypto:    hash_blake3 hash_sha256 hmac_sha256 constant_time_eq hash_file base64_encode base64_decode uuid\n\
         \x20          hash_md5 hash_sha1 (BROKEN hashes — legacy interop only)\n\
         \x20          all hash_* take {{raw:true}} for the digest as bytes (v0.66.0)"
    );
    println!("Network:   http_get http_post http_request dns_lookup");
    println!("Bytes:     bytes_len string_to_bytes bytes_to_string bytes_find bytes_starts_with");
    println!("           bytes_split bytes_concat bytes_from bytes_to_hex bytes_from_hex");
    println!("           (also $b[i], length, slice and `for each` — v0.64.0)");
    println!("SQL:       sqlopen sqlexec sqlclose");
    println!("URL:       url_parse");
    println!("Bus:       send address emit port_exists");
    println!();
    println!("Keywords:  if/then/else/end  for/to/step/next  for each/in/next");
    println!("           while/done  loop/done  select/when/otherwise/end");
    println!("           function/return  try/catch/end  parse/with");
    println!("           export  alias  source  sh  die  break  continue");
    println!();
    println!("Prelude:   lines chars sum max min abs read_lines avg clamp");
    Ok(Some(Value::Nil))
}

// --- SQLite ---

#[cfg(feature = "sqlite")]
mod sqlite_builtins {
    use super::*;
    use std::collections::HashMap;
    use std::sync::Mutex;
    use std::sync::atomic::{AtomicU64, Ordering};

    static NEXT_ID: AtomicU64 = AtomicU64::new(1);

    static DB_MAP: std::sync::LazyLock<Mutex<HashMap<u64, rusqlite::Connection>>> =
        std::sync::LazyLock::new(|| Mutex::new(HashMap::new()));

    /// sqlopen(path) or sqlopen(path, "rw") → numeric handle
    pub fn builtin_sqlopen(args: Vec<Value>) -> MixResult<Option<Value>> {
        expect_args("sqlopen", &args, 1)?;
        let path = args[0].to_mix_string();
        let read_write = args.get(1).is_some_and(|v| v.to_mix_string() == "rw");

        let conn = if read_write {
            rusqlite::Connection::open(&path)
        } else {
            rusqlite::Connection::open_with_flags(
                &path,
                rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY
                    | rusqlite::OpenFlags::SQLITE_OPEN_NO_MUTEX,
            )
        }
        .map_err(|e| MixError::RuntimeError {
            span: None,
            msg: format!("sqlopen '{}': {}", path, e),
        })?;

        // Enable WAL and busy timeout for read-write connections
        if read_write {
            conn.execute_batch("PRAGMA journal_mode=WAL; PRAGMA busy_timeout=5000;")
                .map_err(|e| MixError::RuntimeError {
                    span: None,
                    msg: format!("sqlopen pragma: {}", e),
                })?;
        }

        let id = NEXT_ID.fetch_add(1, Ordering::Relaxed);
        DB_MAP.lock().unwrap().insert(id, conn);
        Ok(Some(Value::Number(id as f64)))
    }

    /// sqlexec(handle, sql) or sqlexec(handle, sql, params_list) → List of Maps
    pub fn builtin_sqlexec(args: Vec<Value>) -> MixResult<Option<Value>> {
        expect_args("sqlexec", &args, 2)?;
        let id = args[0]
            .to_mix_string()
            .parse::<u64>()
            .map_err(|_| MixError::RuntimeError {
                span: None,
                msg: "sqlexec: first argument must be a database handle from sqlopen()".into(),
            })?;
        let sql = args[1].to_mix_string();

        // Collect optional bind parameters, TYPED: nil→NULL, bool→INTEGER
        // 0/1, whole finite number→INTEGER, other number→REAL, string→TEXT,
        // bytes→BLOB. The old code bound EVERY param as TEXT via
        // to_mix_string, so nil arrived as the 3-char string "nil"
        // (corrupting NULL columns) and numbers/bools as text (breaking
        // typed comparisons and sort order). A list/map/function param has
        // no SQL representation → loud error, not a stringified blob.
        fn bind_value(v: &Value) -> MixResult<rusqlite::types::Value> {
            use rusqlite::types::Value as Sv;
            Ok(match v {
                Value::Nil => Sv::Null,
                Value::Bool(b) => Sv::Integer(*b as i64),
                Value::Number(n) => {
                    // Exclusive upper bound: `i64::MAX as f64` is exactly 2^63
                    // (one past i64::MAX), and a 2^63 f64 would saturate to
                    // i64::MAX via `as` — bind it as REAL instead (same range
                    // rule as json.rs mix_to_json).
                    if n.is_finite()
                        && *n == n.floor()
                        && *n >= i64::MIN as f64
                        && *n < i64::MAX as f64
                    {
                        Sv::Integer(*n as i64)
                    } else {
                        Sv::Real(*n)
                    }
                }
                Value::String(s) => Sv::Text(s.clone()),
                Value::Bytes(b) => Sv::Blob(b.to_vec()),
                Value::Buffer(b) => Sv::Blob(b.borrow().clone()),
                other => {
                    return Err(MixError::RuntimeError {
                        span: None,
                        msg: format!(
                            "sqlexec: cannot bind a {} parameter (no SQL representation)",
                            other.type_name()
                        ),
                    });
                }
            })
        }
        let params: Vec<rusqlite::types::Value> = match args.get(2) {
            Some(Value::List(list)) => list.iter().map(bind_value).collect::<MixResult<_>>()?,
            Some(v) => vec![bind_value(v)?],
            None => Vec::new(),
        };

        let map = DB_MAP.lock().unwrap();
        let conn = map.get(&id).ok_or_else(|| MixError::RuntimeError {
            span: None,
            msg: format!("sqlexec: no open database with handle {}", id),
        })?;

        let param_refs: Vec<&dyn rusqlite::types::ToSql> = params
            .iter()
            .map(|s| s as &dyn rusqlite::types::ToSql)
            .collect();
        let mut stmt = conn.prepare(&sql).map_err(|e| MixError::RuntimeError {
            span: None,
            msg: format!("sqlexec: {}", e),
        })?;

        // Write-vs-read is decided by sqlite3_stmt_readonly (via
        // Statement::readonly()), not a keyword prefix match — the old
        // uppercase-prefix check missed REPLACE, WITH...INSERT, and
        // PRAGMA setters. A non-readonly statement with no result
        // columns takes the execute path ({affected}); anything that
        // returns columns (SELECT, row-returning PRAGMAs) keeps the
        // rows shape.
        if !stmt.readonly() && stmt.column_count() == 0 {
            let affected =
                stmt.execute(param_refs.as_slice())
                    .map_err(|e| MixError::RuntimeError {
                        span: None,
                        msg: format!("sqlexec: {}", e),
                    })?;
            let mut result = indexmap::IndexMap::new();
            result.insert("affected".into(), Value::Number(affected as f64));
            return Ok(Some(Value::map(result)));
        }

        // Read-only (or row-returning) statement — return rows as List of Maps

        let col_count = stmt.column_count();
        let col_names: Vec<String> = (0..col_count)
            .map(|i| stmt.column_name(i).unwrap_or("?").to_string())
            .collect();

        let rows = stmt
            .query_map(param_refs.as_slice(), |row| {
                let mut map = indexmap::IndexMap::new();
                for (i, name) in col_names.iter().enumerate() {
                    let val: rusqlite::types::Value = row.get(i)?;
                    let mix_val = match val {
                        rusqlite::types::Value::Null => Value::Nil,
                        rusqlite::types::Value::Integer(n) => Value::Number(n as f64),
                        rusqlite::types::Value::Real(f) => Value::Number(f),
                        rusqlite::types::Value::Text(s) => Value::String(s),
                        rusqlite::types::Value::Blob(b) => {
                            Value::String(format!("<blob {} bytes>", b.len()))
                        }
                    };
                    map.insert(name.clone(), mix_val);
                }
                Ok(Value::map(map))
            })
            .map_err(|e| MixError::RuntimeError {
                span: None,
                msg: format!("sqlexec: {}", e),
            })?;

        let mut result = Vec::new();
        for row in rows {
            result.push(row.map_err(|e| MixError::RuntimeError {
                span: None,
                msg: format!("sqlexec row: {}", e),
            })?);
        }
        Ok(Some(Value::list(result)))
    }

    /// sqlclose(handle) → Nil
    pub fn builtin_sqlclose(args: Vec<Value>) -> MixResult<Option<Value>> {
        expect_args("sqlclose", &args, 1)?;
        let id = args[0]
            .to_mix_string()
            .parse::<u64>()
            .map_err(|_| MixError::RuntimeError {
                span: None,
                msg: "sqlclose: argument must be a database handle from sqlopen()".into(),
            })?;
        let removed = DB_MAP.lock().unwrap().remove(&id);
        if removed.is_none() {
            return Err(MixError::RuntimeError {
                span: None,
                msg: format!("sqlclose: no open database with handle {}", id),
            });
        }
        Ok(Some(Value::Nil))
    }
}

#[cfg(feature = "sqlite")]
use sqlite_builtins::{builtin_sqlclose, builtin_sqlexec, builtin_sqlopen};

// --- DKIM keygen (feature-gated) ---
//
// Mix-side surface for the Phase 2 key-management script. Returns a
// map ready to write to disk + paste into a DNS TXT record:
//   {algorithm, private_pem, public_b64, dns_txt_record}
//
// Uses `mail-auth` directly rather than `cosmix-maild-auth::keygen`
// to avoid a cargo dep cycle (cosmix-maild-auth ↔ cosmix-lib-mix via
// optional cosmix-lib-client → cosmix-lib-config → cosmix-lib-mix
// edges; cargo's cycle check is feature-blind across optional deps).
// The PEM envelope + DNS record shape is intentionally identical to
// `cosmix-maild-auth::keygen` so files generated here load straight
// into `MailAuthSigner` via `PrivateKeyDer::from_pem_slice`.

#[cfg(feature = "dkim")]
fn builtin_dkim_keygen(args: Vec<Value>) -> MixResult<Option<Value>> {
    use mail_auth::dkim::generate::DkimKeyPair;

    if args.is_empty() {
        return Err(MixError::RuntimeError {
            span: None,
            msg: "dkim_keygen: expected at least 1 argument (algorithm)".into(),
        });
    }
    let algo = args[0].to_mix_string();

    enum Spec {
        Rsa(usize),
        Ed25519,
    }
    let spec = match algo.to_ascii_lowercase().as_str() {
        "rsa" => {
            // Default 2048; second arg overrides. Cap at 4096 to keep
            // a single keygen well under a second; refuse < 1024 to
            // match cosmix-maild-auth::keygen's floor (anything weaker
            // is rejected by modern receivers anyway).
            let bits = if args.len() >= 2 {
                let n =
                    extract_number(&args[1], InputPolicy::StandardCoercion).ok_or_else(|| {
                        MixError::RuntimeError {
                            span: None,
                            msg: format!(
                                "dkim_keygen: rsa bits must be a number, got {:?}",
                                args[1]
                            ),
                        }
                    })?;
                // The domain gate filters entry; the pinned DKIM diagnostics
                // (floor / cap / non-integer, each naming the remedy) are the
                // established contract and survive it.
                as_exact_integer("dkim_keygen(): argument 2", n, 1024, 4096).map_err(|_| {
                    let msg = if n.fract() != 0.0 || !n.is_finite() {
                        format!(
                            "dkim_keygen: rsa bits {n} must be an integer; non-integer values silently truncate"
                        )
                    } else if n < 1024.0 {
                        format!("dkim_keygen: rsa bits {n} below 1024 floor")
                    } else {
                        format!("dkim_keygen: rsa bits {n} exceeds 4096 cap (keygen would be slow)")
                    };
                    MixError::RuntimeError { span: None, msg }
                })? as usize
            } else {
                2048
            };
            Spec::Rsa(bits)
        }
        "ed25519" => Spec::Ed25519,
        other => {
            return Err(MixError::RuntimeError {
                span: None,
                msg: format!(
                    "dkim_keygen: unknown algorithm {other:?} (expected \"rsa\" or \"ed25519\")"
                ),
            });
        }
    };

    let (algorithm, private_pem, public_b64, dns_txt_record) = match spec {
        Spec::Rsa(bits) => {
            let pair = DkimKeyPair::generate_rsa(bits).map_err(|e| MixError::RuntimeError {
                span: None,
                msg: format!("dkim_keygen: rsa: {e}"),
            })?;
            let pem = dkim_pem_encode("RSA PRIVATE KEY", pair.private_key());
            let pubb64 = pair.encoded_public_key();
            (
                "rsa-sha256",
                pem,
                pubb64.clone(),
                format!("v=DKIM1; k=rsa; p={pubb64}"),
            )
        }
        Spec::Ed25519 => {
            let pair = DkimKeyPair::generate_ed25519().map_err(|e| MixError::RuntimeError {
                span: None,
                msg: format!("dkim_keygen: ed25519: {e}"),
            })?;
            let pem = dkim_pem_encode("PRIVATE KEY", pair.private_key());
            let pubb64 = pair.encoded_public_key();
            (
                "ed25519-sha256",
                pem,
                pubb64.clone(),
                format!("v=DKIM1; k=ed25519; p={pubb64}"),
            )
        }
    };

    let mut map = indexmap::IndexMap::new();
    map.insert("algorithm".into(), Value::String(algorithm.into()));
    map.insert("private_pem".into(), Value::String(private_pem));
    map.insert("public_b64".into(), Value::String(public_b64));
    map.insert("dns_txt_record".into(), Value::String(dns_txt_record));
    Ok(Some(Value::map(map)))
}

/// Wrap DER bytes in a 64-char-wrapped PEM envelope matching OpenSSL's
/// output. `label` is the `-----BEGIN <label>-----` content (e.g.
/// `"RSA PRIVATE KEY"` for PKCS1, `"PRIVATE KEY"` for PKCS8). Must
/// stay byte-identical to `cosmix-maild-auth::keygen::pem_encode` so
/// files produced here re-load via the same PEM decoder path.
#[cfg(feature = "dkim")]
fn dkim_pem_encode(label: &str, der: &[u8]) -> String {
    use std::fmt::Write as _;
    let b64 = dkim_base64_encode(der);
    let mut out = String::with_capacity(b64.len() + 64);
    writeln!(out, "-----BEGIN {label}-----").unwrap();
    for chunk in b64.as_bytes().chunks(64) {
        out.push_str(std::str::from_utf8(chunk).unwrap());
        out.push('\n');
    }
    writeln!(out, "-----END {label}-----").unwrap();
    out
}

#[cfg(feature = "dkim")]
fn dkim_base64_encode(bytes: &[u8]) -> String {
    const ALPHA: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::with_capacity(bytes.len().div_ceil(3) * 4);
    let chunks = bytes.chunks_exact(3);
    let remainder = chunks.remainder();
    for c in chunks {
        let n = (u32::from(c[0]) << 16) | (u32::from(c[1]) << 8) | u32::from(c[2]);
        for shift in [18, 12, 6, 0] {
            out.push(ALPHA[((n >> shift) & 0x3f) as usize] as char);
        }
    }
    match remainder.len() {
        1 => {
            let n = u32::from(remainder[0]) << 16;
            out.push(ALPHA[((n >> 18) & 0x3f) as usize] as char);
            out.push(ALPHA[((n >> 12) & 0x3f) as usize] as char);
            out.push_str("==");
        }
        2 => {
            let n = (u32::from(remainder[0]) << 16) | (u32::from(remainder[1]) << 8);
            out.push(ALPHA[((n >> 18) & 0x3f) as usize] as char);
            out.push(ALPHA[((n >> 12) & 0x3f) as usize] as char);
            out.push(ALPHA[((n >> 6) & 0x3f) as usize] as char);
            out.push('=');
        }
        _ => {}
    }
    out
}

#[cfg(test)]
mod ssh_helpers_tests {
    use super::{
        build_env_driver, call_builtin, mix_binding_lines, quote_mix_string, random_password,
        run_with_timeout, shell_quote, sql_quote, ssh_exec_decode, ssh_must_from_map,
        validate_ssh_exec_run_argv_opts,
    };
    use crate::value::Value;

    fn raw_m(pairs: &[(&str, Value)]) -> indexmap::IndexMap<String, Value> {
        let mut map = indexmap::IndexMap::new();
        for (k, v) in pairs {
            map.insert((*k).to_string(), v.clone());
        }
        map
    }

    fn m(pairs: &[(&str, Value)]) -> Value {
        Value::map(raw_m(pairs))
    }

    #[test]
    fn quote_mix_string_disables_interpolation_and_escapes() {
        assert_eq!(quote_mix_string("a\"b"), "\"a\\\"b\"");
        // `$` is escaped so `${...}` in the payload never interpolates.
        assert_eq!(quote_mix_string("x${y}"), "\"x\\${y}\"");
        assert_eq!(quote_mix_string("a\\b"), "\"a\\\\b\"");
    }

    #[test]
    fn ssh_exec_argv_validated_before_ssh() {
        // Empty / non-string argv raise TYPE_MISMATCH with no ssh work.
        let err = super::builtin_ssh_exec(vec![Value::String("h".into()), Value::list(vec![])])
            .unwrap_err();
        assert_eq!(err.info().map(|i| i.code.as_str()), Some("TYPE_MISMATCH"));
        let err = super::builtin_ssh_exec(vec![
            Value::String("h".into()),
            Value::list(vec![Value::Number(42.0)]),
        ])
        .unwrap_err();
        assert_eq!(err.info().map(|i| i.code.as_str()), Some("TYPE_MISMATCH"));
    }

    #[test]
    fn ssh_exec_rejects_binary_stdin() {
        // Binary stdin can't cross the strict-data driver — clean local
        // OPTION_INVALID, not an uncatchable DataSerializeError.
        let err = super::builtin_ssh_exec(vec![
            Value::String("h".into()),
            Value::list(vec![Value::String("cat".into())]),
            m(&[("stdin", Value::bytes(vec![1, 2, 3]))]),
        ])
        .unwrap_err();
        assert_eq!(err.info().map(|i| i.code.as_str()), Some("OPTION_INVALID"));
    }

    #[test]
    fn ssh_exec_unknown_option_rejected() {
        let err = super::builtin_ssh_exec(vec![
            Value::String("h".into()),
            Value::list(vec![Value::String("echo".into())]),
            m(&[("bogus", Value::Number(1.0))]),
        ])
        .unwrap_err();
        assert_eq!(err.info().map(|i| i.code.as_str()), Some("OPTION_INVALID"));
    }

    #[test]
    fn ssh_exec_remote_stdio_allowlist_accepts_protocol_safe_routes() {
        let file = m(&[
            ("file", Value::String("/var/tmp/output".into())),
            ("append", Value::Bool(true)),
            ("mode", Value::Number(0o640 as f64)),
        ]);
        for (key, value) in [
            ("stdout", Value::String("capture".into())),
            ("stdout", Value::String("null".into())),
            ("stdout", file.clone()),
            ("stderr", Value::String("capture".into())),
            ("stderr", Value::String("null".into())),
            ("stderr", Value::String("stdout".into())),
            ("stderr", file.clone()),
            ("stdin", Value::String("payload".into())),
            // No stdin route is named "inherit" on either side of the hop, so
            // these seven bytes are payload remotely exactly as they are
            // locally; `nil` keeps meaning closed stdin. Both were valid
            // ssh_exec calls before the allowlist existed and must stay valid.
            ("stdin", Value::String("inherit".into())),
            ("stdin", Value::Nil),
            (
                "stdin",
                m(&[("file", Value::String("/var/tmp/input".into()))]),
            ),
            ("stdin", m(&[("null", Value::Bool(true))])),
            ("stream", Value::Bool(false)),
        ] {
            validate_ssh_exec_run_argv_opts(&raw_m(&[(key, value)]))
                .unwrap_or_else(|error| panic!("{key} safe route rejected: {error}"));
        }
    }

    #[test]
    fn ssh_exec_rejects_protocol_unsafe_routes_before_ssh() {
        for (key, value, needle) in [
            ("stdout", Value::String("inherit".into()), "stdout"),
            ("stderr", Value::String("inherit".into()), "stderr"),
            ("stream", Value::Bool(true), "stream:true"),
        ] {
            let err = super::builtin_ssh_exec(vec![
                Value::String("192.0.2.1".into()),
                Value::list(vec![Value::String("true".into())]),
                Value::map(raw_m(&[(key, value)])),
            ])
            .unwrap_err();
            let info = err.info().expect("structured option error");
            assert_eq!(info.code, "OPTION_INVALID");
            assert!(info.message.contains(needle), "got: {}", info.message);
        }
    }

    #[test]
    fn ssh_exec_rejects_non_allowlisted_remote_stdin_forms() {
        for value in [
            Value::bytes(vec![1, 2]),
            Value::Bool(true),
            Value::Number(1.0),
        ] {
            let err = validate_ssh_exec_run_argv_opts(&raw_m(&[("stdin", value)]))
                .expect_err("unsafe remote stdin accepted");
            assert_eq!(
                err.info().map(|info| info.code.as_str()),
                Some("OPTION_INVALID")
            );
        }
    }

    #[test]
    fn ssh_exec_decode_maps_every_envelope() {
        // Transport failure (ok:false) → SSH_TRANSPORT.
        let r = ssh_exec_decode("h", Some(m(&[("ok", Value::Bool(false))])));
        let rm = match &r {
            Value::Map(m) => m,
            _ => panic!(),
        };
        assert_eq!(
            rm.get("error_code"),
            Some(&Value::String("SSH_TRANSPORT".into()))
        );
        assert_eq!(rm.get("host"), Some(&Value::String("h".into())));

        // Transport timeout → SSH_TIMEOUT.
        let r = ssh_exec_decode(
            "h",
            Some(m(&[
                ("ok", Value::Bool(false)),
                ("timed_out", Value::Bool(true)),
            ])),
        );
        let rm = match &r {
            Value::Map(m) => m,
            _ => panic!(),
        };
        assert_eq!(
            rm.get("error_code"),
            Some(&Value::String("SSH_TIMEOUT".into()))
        );

        // Unsupported remote (no run_argv) → SSH_REMOTE_UNSUPPORTED.
        let env = m(&[("status", Value::String("unsupported".into()))]);
        let r = ssh_exec_decode("h", Some(m(&[("ok", Value::Bool(true)), ("value", env)])));
        let rm = match &r {
            Value::Map(m) => m,
            _ => panic!(),
        };
        assert_eq!(
            rm.get("error_code"),
            Some(&Value::String("SSH_REMOTE_UNSUPPORTED".into()))
        );
        assert_eq!(rm.get("ok"), Some(&Value::Bool(false)));

        // A well-formed status:ok with a NON-process_result body →
        // SSH_PROTOCOL (schema enforced).
        let bad_env = m(&[
            ("status", Value::String("ok".into())),
            ("result", m(&[("ok", Value::Bool(true))])),
        ]);
        let r = ssh_exec_decode(
            "h",
            Some(m(&[("ok", Value::Bool(true)), ("value", bad_env)])),
        );
        let rm = match &r {
            Value::Map(m) => m,
            _ => panic!(),
        };
        assert_eq!(
            rm.get("error_code"),
            Some(&Value::String("SSH_PROTOCOL".into()))
        );

        // Failure carries the real transport duration, not 0.
        let r = ssh_exec_decode(
            "h",
            Some(m(&[
                ("ok", Value::Bool(false)),
                ("duration_ms", Value::Number(1234.0)),
            ])),
        );
        let rm = match &r {
            Value::Map(m) => m,
            _ => panic!(),
        };
        assert_eq!(rm.get("duration_ms"), Some(&Value::Number(1234.0)));

        // A result missing later process_result fields (only the first
        // six) is ALSO SSH_PROTOCOL — the full 13-field contract is
        // enforced (codex final review).
        let partial = m(&[
            ("ok", Value::Bool(true)),
            ("exit_code", Value::Number(0.0)),
            ("stdout", Value::String("hi\n".into())),
            ("stderr", Value::String("".into())),
            ("timed_out", Value::Bool(false)),
            ("duration_ms", Value::Number(5.0)),
        ]);
        let env = m(&[("status", Value::String("ok".into())), ("result", partial)]);
        let r = ssh_exec_decode("h", Some(m(&[("ok", Value::Bool(true)), ("value", env)])));
        let rm = match &r {
            Value::Map(m) => m,
            _ => panic!(),
        };
        assert_eq!(
            rm.get("error_code"),
            Some(&Value::String("SSH_PROTOCOL".into()))
        );

        // Success → run_argv result + host (COMPLETE process_result).
        let result = m(&[
            ("ok", Value::Bool(true)),
            ("exit_code", Value::Number(0.0)),
            ("stdout", Value::String("hi\n".into())),
            ("stderr", Value::String("".into())),
            ("timed_out", Value::Bool(false)),
            ("interrupted", Value::Bool(false)),
            ("signal", Value::Nil),
            ("duration_ms", Value::Number(5.0)),
            ("stdout_truncated", Value::Bool(false)),
            ("stderr_truncated", Value::Bool(false)),
            ("utf8_lossy", Value::Bool(false)),
            ("error_code", Value::Nil),
            ("error", Value::Nil),
        ]);
        let env = m(&[("status", Value::String("ok".into())), ("result", result)]);
        let r = ssh_exec_decode(
            "pve3",
            Some(m(&[("ok", Value::Bool(true)), ("value", env)])),
        );
        let rm = match &r {
            Value::Map(m) => m,
            _ => panic!(),
        };
        assert_eq!(rm.get("ok"), Some(&Value::Bool(true)));
        assert_eq!(rm.get("stdout"), Some(&Value::String("hi\n".into())));
        assert_eq!(rm.get("host"), Some(&Value::String("pve3".into())));

        // Undecodable envelope → SSH_PROTOCOL.
        let r = ssh_exec_decode("h", Some(m(&[("ok", Value::Bool(true))])));
        let rm = match &r {
            Value::Map(m) => m,
            _ => panic!(),
        };
        assert_eq!(
            rm.get("error_code"),
            Some(&Value::String("SSH_PROTOCOL".into()))
        );
    }

    #[test]
    fn shell_quote_empty_and_simple() {
        assert_eq!(shell_quote(""), "''");
        assert_eq!(shell_quote("hello"), "'hello'");
    }

    #[test]
    fn shell_quote_escapes_internal_single_quote() {
        assert_eq!(shell_quote("it's"), r#"'it'\''s'"#);
        assert_eq!(shell_quote("''"), r#"''\'''\'''"#);
    }

    #[test]
    fn shell_quote_passthrough_metachars() {
        // dollar / backtick / backslash / double-quote / newline are
        // all literal inside single quotes — should appear verbatim
        // between the wrapper quotes with no extra escaping.
        assert_eq!(shell_quote("$x `id` \\ \"y\""), "'$x `id` \\ \"y\"'");
        assert_eq!(shell_quote("a\nb"), "'a\nb'");
    }

    /// Round-trip: feeding the quoted form through `bash -c "printf '%s' …"`
    /// must reproduce the original byte-for-byte. `printf '%s'` is the
    /// safe choice — `echo` mangles leading `-` and backslash escapes.
    #[test]
    fn shell_quote_bash_round_trip() {
        let cases = [
            "",
            "plain",
            "with space",
            "it's a test",
            "$HOME and `id`",
            "a\nb\tc",
            "trailing'",
            "'leading",
            "''",
            "back\\slash",
            "\"double\"",
            "-rf /",
            "emoji 🚀 ok",
            "non-bmp 🧬 ok",
            "nbsp\u{00A0}between\u{00A0}words",
        ];
        for case in cases {
            let quoted = shell_quote(case);
            // Build: printf '%s' <quoted>
            let script = format!("printf '%s' {}", quoted);
            let out = std::process::Command::new("bash")
                .arg("-c")
                .arg(&script)
                .output()
                .expect("spawn bash");
            assert!(
                out.status.success(),
                "bash failed for {:?}: stderr={}",
                case,
                String::from_utf8_lossy(&out.stderr)
            );
            let got = String::from_utf8(out.stdout).expect("utf8");
            assert_eq!(got, case, "round-trip mismatch for input {:?}", case);
        }
    }

    #[test]
    fn sql_quote_basic() {
        assert_eq!(sql_quote(""), "");
        assert_eq!(sql_quote("hello"), "hello");
        assert_eq!(sql_quote("it's"), "it''s");
        assert_eq!(sql_quote("a'b'c"), "a''b''c");
        // Pre-doubled quotes get doubled again — sql_quote is stateless.
        assert_eq!(sql_quote("it''s"), "it''''s");
        // Backslash is escaped (MySQL/MariaDB default mode treats it as
        // an escape character; quote-doubling alone was injectable there).
        assert_eq!(sql_quote("100% \\ done"), "100% \\\\ done");
        assert_eq!(sql_quote("a\\'b"), "a\\\\''b");
        // NUL bytes are stripped — they truncate in C-based clients.
        assert_eq!(sql_quote("a\0b"), "ab");
        // Other metacharacters are untouched: the helper only handles
        // the SQL string-literal quoting concern, not LIKE escaping.
        assert_eq!(sql_quote("100% _done_"), "100% _done_");
    }

    /// Class-diversity contract: every output must contain at least one
    /// uppercase letter, one lowercase letter, and one digit. Run the
    /// generator 1000× to make a single bad path likely to show up.
    #[test]
    fn random_password_class_diversity_1000() {
        for _ in 0..1000 {
            let pw = random_password(16);
            assert_eq!(pw.len(), 16, "len mismatch: {:?}", pw);
            assert!(
                pw.chars().any(|c| c.is_ascii_uppercase()),
                "no uppercase in {:?}",
                pw
            );
            assert!(
                pw.chars().any(|c| c.is_ascii_lowercase()),
                "no lowercase in {:?}",
                pw
            );
            assert!(
                pw.chars().any(|c| c.is_ascii_digit()),
                "no digit in {:?}",
                pw
            );
            // No O/o by construction — alphabet excludes them up front.
            assert!(
                !pw.contains('O') && !pw.contains('o'),
                "found forbidden O/o in {:?}",
                pw
            );
            // ASCII alphanumeric only.
            assert!(
                pw.chars().all(|c| c.is_ascii_alphanumeric()),
                "non-alphanumeric in {:?}",
                pw
            );
        }
    }

    /// `len = 3` is the corner case: one of each class, no fill. The
    /// shuffle must still succeed and the diversity contract must hold.
    #[test]
    fn random_password_len_3_corner_case() {
        for _ in 0..200 {
            let pw = random_password(3);
            assert_eq!(pw.len(), 3);
            assert!(pw.chars().any(|c| c.is_ascii_uppercase()));
            assert!(pw.chars().any(|c| c.is_ascii_lowercase()));
            assert!(pw.chars().any(|c| c.is_ascii_digit()));
        }
    }

    /// Maximum allowed length must succeed.
    #[test]
    fn random_password_len_1024_accept() {
        let pw = random_password(1024);
        assert_eq!(pw.len(), 1024);
        assert!(pw.chars().any(|c| c.is_ascii_uppercase()));
        assert!(pw.chars().any(|c| c.is_ascii_lowercase()));
        assert!(pw.chars().any(|c| c.is_ascii_digit()));
    }

    /// Dispatch-layer validation: out-of-range lengths must error,
    /// per spec §9. The pure helper would `assert!` on these, but the
    /// builtin entry point converts them to a Mix runtime error
    /// before they reach the helper.
    #[test]
    fn random_password_dispatch_rejects_out_of_range() {
        for bad_len in [0i64, 1, 2, 1025, -1, 100_000] {
            let res = call_builtin("random_password", vec![Value::Number(bad_len as f64)]);
            assert!(
                res.is_err(),
                "len={} should have been rejected, got {:?}",
                bad_len,
                res
            );
        }
    }

    /// Dispatch-layer validation: non-integer / non-finite numbers
    /// must error rather than silently truncating.
    #[test]
    fn random_password_dispatch_rejects_non_integer() {
        for bad in [3.5_f64, 16.1, f64::NAN, f64::INFINITY, -f64::INFINITY] {
            let res = call_builtin("random_password", vec![Value::Number(bad)]);
            assert!(
                res.is_err(),
                "non-integer/non-finite {} should have been rejected, got {:?}",
                bad,
                res
            );
        }
    }

    /// Dispatch-layer validation: passing more than one argument is a
    /// contract violation — `random_password` takes 0 or 1 args.
    #[test]
    fn random_password_dispatch_rejects_extra_args() {
        let res = call_builtin(
            "random_password",
            vec![Value::Number(16.0), Value::Number(32.0)],
        );
        assert!(
            res.is_err(),
            "extra args should have been rejected, got {:?}",
            res
        );
    }

    /// Dispatch-layer happy path: default len = 16, returns a String.
    #[test]
    fn random_password_dispatch_default_len_16() {
        let res = call_builtin("random_password", vec![]).expect("call_builtin err");
        match &res {
            Some(Value::String(s)) => assert_eq!(s.len(), 16),
            other => panic!("expected Some(String) of len 16, got {:?}", other),
        }
    }

    /// Position-uniformity smoke test: across many samples, every
    /// character class should appear at every position with non-trivial
    /// frequency. A bug like "always upper at position 0" would make
    /// some position/class cell stick at zero.
    #[test]
    fn random_password_position_uniformity() {
        const N: usize = 2000;
        const LEN: usize = 8;
        let mut upper_at = [0u32; LEN];
        let mut lower_at = [0u32; LEN];
        let mut digit_at = [0u32; LEN];
        for _ in 0..N {
            let pw = random_password(LEN);
            for (i, c) in pw.chars().enumerate() {
                if c.is_ascii_uppercase() {
                    upper_at[i] += 1;
                } else if c.is_ascii_lowercase() {
                    lower_at[i] += 1;
                } else if c.is_ascii_digit() {
                    digit_at[i] += 1;
                }
            }
        }
        // Each class should hit each position at least a few hundred
        // times out of 2000 if the shuffle is uniform. A vacuous bound
        // here would still catch a "always X at position Y" bug.
        for i in 0..LEN {
            assert!(
                upper_at[i] > 100,
                "position {}: upper count {} too low",
                i,
                upper_at[i]
            );
            assert!(
                lower_at[i] > 100,
                "position {}: lower count {} too low",
                i,
                lower_at[i]
            );
            assert!(
                digit_at[i] > 50,
                "position {}: digit count {} too low",
                i,
                digit_at[i]
            );
        }
    }

    // ---- random() ---------------------------------------------------------

    /// `random()` (0 args) stays in [0.0, 1.0) and actually varies.
    #[test]
    fn random_no_args_in_unit_interval() {
        let mut lo = f64::INFINITY;
        let mut hi = f64::NEG_INFINITY;
        for _ in 0..5000 {
            match call_builtin("random", vec![]).expect("call_builtin err") {
                Some(Value::Number(x)) => {
                    assert!((0.0..1.0).contains(&x), "random() out of [0,1): {}", x);
                    lo = lo.min(x);
                    hi = hi.max(x);
                }
                other => panic!("expected Some(Number), got {:?}", other),
            }
        }
        // A stuck/constant RNG would leave lo == hi; over 5000 draws two
        // distinct values are a near-certainty.
        assert!(
            lo < hi,
            "random() never varied over 5000 draws (lo={}, hi={})",
            lo,
            hi
        );
    }

    /// `random(min, max)` returns whole numbers in the inclusive range, and
    /// both endpoints are actually reachable (a half-open bug would miss max).
    #[test]
    fn random_two_args_inclusive_endpoints_reachable() {
        let mut saw_lo = false;
        let mut saw_hi = false;
        for _ in 0..1000 {
            match call_builtin("random", vec![Value::Number(1.0), Value::Number(6.0)])
                .expect("call_builtin err")
            {
                Some(Value::Number(x)) => {
                    assert!((1.0..=6.0).contains(&x), "random(1,6) out of range: {}", x);
                    assert_eq!(x.fract(), 0.0, "random(1,6) not an integer: {}", x);
                    saw_lo |= x == 1.0;
                    saw_hi |= x == 6.0;
                }
                other => panic!("expected Some(Number), got {:?}", other),
            }
        }
        assert!(
            saw_lo && saw_hi,
            "random(1,6) endpoints not both observed over 1000 draws (lo={}, hi={})",
            saw_lo,
            saw_hi
        );
    }

    /// A single-point range and a negative range both behave.
    #[test]
    fn random_single_point_and_negative_ranges() {
        for _ in 0..10 {
            match call_builtin("random", vec![Value::Number(5.0), Value::Number(5.0)])
                .expect("call_builtin err")
            {
                Some(Value::Number(x)) => assert_eq!(x, 5.0),
                other => panic!("expected 5, got {:?}", other),
            }
        }
        for _ in 0..200 {
            match call_builtin("random", vec![Value::Number(-3.0), Value::Number(-1.0)])
                .expect("call_builtin err")
            {
                Some(Value::Number(x)) => {
                    assert!(
                        (-3.0..=-1.0).contains(&x),
                        "random(-3,-1) out of range: {}",
                        x
                    )
                }
                other => panic!("expected Some(Number), got {:?}", other),
            }
        }
    }

    /// Reversed range, non-integer bound, and wrong arity are all rejected.
    #[test]
    fn random_rejects_bad_input() {
        assert!(
            call_builtin("random", vec![Value::Number(6.0), Value::Number(1.0)]).is_err(),
            "reversed range should error"
        );
        assert!(
            call_builtin("random", vec![Value::Number(1.5), Value::Number(3.0)]).is_err(),
            "non-integer bound should error"
        );
        assert!(
            call_builtin("random", vec![Value::Number(5.0)]).is_err(),
            "1 arg should error"
        );
        assert!(
            call_builtin(
                "random",
                vec![Value::Number(1.0), Value::Number(2.0), Value::Number(3.0)]
            )
            .is_err(),
            "3 args should error"
        );
    }

    /// Bounds beyond the f64 exact-integer range (±2^53) are rejected so a
    /// returned number is never a lossy/aliased integer, and `i64::MAX as f64`
    /// rounding can't silently admit an out-of-range bound.
    #[test]
    fn random_rejects_bounds_beyond_exact_int_range() {
        let two_53 = 9_007_199_254_740_992.0_f64; // 2^53
        let max_safe = 9_007_199_254_740_991.0_f64; // 2^53 - 1
        // M-1 tightened the ceiling from 2^53 to 2^53 - 1. 2^53 is itself
        // representable, but it is ALSO what 2^53 + 1 rounds to, so accepting
        // it accepts an aliased bound -- precisely what this test guards.
        // 2^53 - 1 is the largest bound with no such twin, and is accepted
        // (single-point range returns it unchanged).
        match call_builtin(
            "random",
            vec![Value::Number(max_safe), Value::Number(max_safe)],
        )
        .expect("call_builtin err")
        {
            Some(Value::Number(x)) => assert_eq!(x, max_safe),
            other => panic!("expected 2^53 - 1, got {:?}", other),
        }
        // 2^53 itself is now rejected, structurally.
        let err = call_builtin("random", vec![Value::Number(0.0), Value::Number(two_53)])
            .expect_err("2^53 has an integer twin and must be refused");
        assert_eq!(
            err.info().map(|i| i.code.as_str()),
            Some("VALUE_OUT_OF_RANGE")
        );
        // 2^54 is a whole f64 but past the exact-int ceiling — reject.
        assert!(
            call_builtin(
                "random",
                vec![Value::Number(0.0), Value::Number(two_53 * 2.0)]
            )
            .is_err(),
            "bound beyond +2^53 should error"
        );
        assert!(
            call_builtin(
                "random",
                vec![Value::Number(two_53 * -2.0), Value::Number(0.0)]
            )
            .is_err(),
            "bound beyond -2^53 should error"
        );
    }

    // ---- ssh_run helpers --------------------------------------------------
    use super::{
        SshOpts, build_remote_command, build_ssh_argv, builtin_ssh_mix, builtin_ssh_run,
        is_valid_env_key, parse_env_opt, parse_ssh_opts,
    };
    use crate::error::MixError;
    use indexmap::IndexMap;

    /// Env values format through the i64 round-trip guard: a whole f64 whose
    /// `as i64` cast would SATURATE must export its true decimal rendering,
    /// never the saturated 9223372036854775807 the old floor()-only test
    /// produced (a fabricated value crossing execve).
    #[test]
    fn env_number_beyond_i64_exports_true_value() {
        let mut m = IndexMap::new();
        m.insert("N".to_string(), Value::Number(1e30));
        m.insert("SMALL".to_string(), Value::Number(42.0));
        m.insert("FRAC".to_string(), Value::Number(2.5));
        // Exactly 2^63 -- the value where a round-trip check LIES (saturate
        // then round-up cancel). The exclusive bound must refuse the integer
        // path; Display then renders the SHORTEST decimal that round-trips
        // to this exact f64 (9223372036854776000 parses back to 2^63) --
        // faithful, unlike the saturated 9223372036854775807, which is a
        // DIFFERENT number.
        m.insert(
            "EDGE".to_string(),
            Value::Number(9_223_372_036_854_775_808.0),
        );
        let pairs = parse_env_opt(&Value::map(m)).expect("valid env map");
        let get = |k: &str| {
            pairs
                .iter()
                .find(|(key, _)| key == k)
                .map(|(_, v)| v.clone())
                .unwrap()
        };
        assert_eq!(get("N"), "1000000000000000000000000000000");
        assert_eq!(get("SMALL"), "42");
        assert_eq!(get("FRAC"), "2.5");
        assert_eq!(get("EDGE"), "9223372036854776000");
    }

    fn map_of(pairs: &[(&str, Value)]) -> Value {
        let mut m = IndexMap::new();
        for (k, v) in pairs {
            m.insert((*k).into(), v.clone());
        }
        Value::map(m)
    }

    #[test]
    fn is_valid_env_key_accepts_posix_names() {
        assert!(is_valid_env_key("HOME"));
        assert!(is_valid_env_key("_x"));
        assert!(is_valid_env_key("PATH"));
        assert!(is_valid_env_key("FOO_BAR_2"));
        assert!(is_valid_env_key("a"));
    }

    #[test]
    fn is_valid_env_key_rejects_bad_names() {
        assert!(!is_valid_env_key(""));
        assert!(!is_valid_env_key("1NUM"));
        assert!(!is_valid_env_key("FOO BAR"));
        assert!(!is_valid_env_key("FOO-BAR"));
        assert!(!is_valid_env_key("FOO=BAR"));
        assert!(!is_valid_env_key("FOO\nBAR"));
        // Non-ASCII letters are rejected — POSIX env names are ASCII.
        assert!(!is_valid_env_key("Ñ"));
    }

    #[test]
    fn build_remote_command_single_string_passes_through() {
        let opts = SshOpts::default();
        let v = Value::String("echo hi".into());
        assert_eq!(build_remote_command(&v, &opts).unwrap(), "echo hi");
    }

    #[test]
    fn build_remote_command_list_joined_with_double_bus() {
        let opts = SshOpts::default();
        let v = Value::list(vec![
            Value::String("cd /tmp".into()),
            Value::String("ls -1".into()),
            Value::String("echo done".into()),
        ]);
        assert_eq!(
            build_remote_command(&v, &opts).unwrap(),
            "cd /tmp && ls -1 && echo done"
        );
    }

    #[test]
    fn build_remote_command_empty_list_errors() {
        let opts = SshOpts::default();
        let v = Value::list(vec![]);
        assert!(build_remote_command(&v, &opts).is_err());
    }

    #[test]
    fn build_remote_command_nul_in_command_errors() {
        let opts = SshOpts::default();
        let v = Value::String("echo \0hi".into());
        assert!(build_remote_command(&v, &opts).is_err());
    }

    #[test]
    fn build_remote_command_non_string_list_element_errors() {
        let opts = SshOpts::default();
        let v = Value::list(vec![Value::String("ok".into()), Value::Number(5.0)]);
        assert!(build_remote_command(&v, &opts).is_err());
    }

    #[test]
    fn build_remote_command_with_env_prefix() {
        let opts = SshOpts {
            env: vec![("FOO".into(), "bar baz".into()), ("X".into(), "$y".into())],
            ..SshOpts::default()
        };
        let v = Value::String("printenv FOO".into());
        // `export` (not `KEY=val cmd`) is required so the variable is
        // visible to every command in a chain — see build_remote_command's
        // comment for the rationale.
        assert_eq!(
            build_remote_command(&v, &opts).unwrap(),
            "export FOO='bar baz'; export X='$y'; printenv FOO"
        );
    }

    /// env must apply to every command in a chain, not just the first
    /// one — that is the whole point of using `export` instead of the
    /// `KEY=val cmd` shell idiom.
    #[test]
    fn build_remote_command_env_visible_across_chain() {
        let opts = SshOpts {
            env: vec![("LC_ALL".into(), "C".into())],
            ..SshOpts::default()
        };
        let v = Value::list(vec![
            Value::String("first".into()),
            Value::String("second".into()),
            Value::String("third".into()),
        ]);
        let out = build_remote_command(&v, &opts).unwrap();
        assert!(
            out.starts_with("export LC_ALL='C'; "),
            "expected leading export, got {:?}",
            out
        );
        assert!(out.contains("first && second && third"));
        // No trailing `LC_ALL=C` decoration on individual commands —
        // the export covers them.
        assert!(!out.contains("LC_ALL='C' first"));
    }

    /// The dispatch-layer pipeline (parse_ssh_opts → build_remote_command)
    /// must compose `export` before `cd`, so cwd-changes happen with
    /// env already in scope.
    #[test]
    fn build_remote_command_export_precedes_cd() {
        let opts = SshOpts {
            env: vec![("FOO".into(), "bar".into())],
            cwd: Some("/tmp".into()),
            ..SshOpts::default()
        };
        let v = Value::String("pwd".into());
        let out = build_remote_command(&v, &opts).unwrap();
        let exp = out.find("export FOO=").expect("export present");
        let cd = out.find("cd ").expect("cd present");
        assert!(exp < cd, "export must come before cd in {:?}", out);
    }

    #[test]
    fn build_remote_command_invalid_env_key_errors() {
        let opts = SshOpts {
            env: vec![("FOO BAR".into(), "ok".into())],
            ..SshOpts::default()
        };
        let v = Value::String("true".into());
        let err = build_remote_command(&v, &opts).unwrap_err();
        match err {
            MixError::RuntimeError { msg, .. } => {
                assert!(msg.contains("invalid env key"))
            }
            _ => panic!("wrong error variant"),
        }
    }

    #[test]
    fn build_remote_command_with_cwd_and_env_and_chain() {
        let opts = SshOpts {
            env: vec![("LC_ALL".into(), "C".into())],
            cwd: Some("/var/spool/it's/here".into()),
            ..SshOpts::default()
        };
        let v = Value::list(vec![
            Value::String("ls -1".into()),
            Value::String("wc -l".into()),
        ]);
        // export (env) → cd (cwd) → joined commands; cwd quoted via
        // shell_quote so the embedded apostrophe survives.
        assert_eq!(
            build_remote_command(&v, &opts).unwrap(),
            "export LC_ALL='C'; cd '/var/spool/it'\\''s/here' && ls -1 && wc -l"
        );
    }

    #[test]
    fn build_ssh_argv_default_shape() {
        let opts = SshOpts::default();
        let argv = build_ssh_argv("lab", "echo hi", &opts);
        assert_eq!(argv[0], "ssh");
        assert!(argv.contains(&"BatchMode=yes".to_string()));
        assert!(argv.contains(&"ConnectTimeout=10".to_string()));
        assert!(argv.contains(&"StrictHostKeyChecking=accept-new".to_string()));
        // Non-multiplex default explicitly disables any user-config
        // ControlPath so the spawned ssh is standalone — otherwise a
        // pre-existing mux master from the user's `~/.ssh/config`
        // would intercept the connection (FD-passing the stdout/stderr
        // pipes), and our PG kill on timeout would not reach it.
        assert!(
            argv.contains(&"ControlPath=none".to_string()),
            "default argv must disable user ControlPath: {:?}",
            argv
        );
        assert_eq!(argv[argv.len() - 2], "lab");
        assert_eq!(argv[argv.len() - 1], "echo hi");
        // `--` must immediately precede the host so a leading-dash host
        // can never be parsed by ssh as an option (option-injection RCE).
        assert_eq!(
            argv[argv.len() - 3],
            "--",
            "`--` must sit directly before the host: {:?}",
            argv
        );
    }

    #[test]
    fn build_ssh_argv_host_after_end_of_options() {
        // Even a hostile dash-leading host lands strictly after `--`,
        // so ssh treats it as a hostname operand, never an option.
        let opts = SshOpts::default();
        let argv = build_ssh_argv("-oProxyCommand=touch /tmp/pwn", "true", &opts);
        let dd = argv
            .iter()
            .position(|s| s == "--")
            .expect("`--` present in argv");
        let host_pos = argv
            .iter()
            .position(|s| s == "-oProxyCommand=touch /tmp/pwn")
            .expect("host present in argv");
        assert_eq!(
            host_pos,
            dd + 1,
            "host must sit immediately after `--`: {:?}",
            argv
        );
    }

    fn ssh_mix_err(args: Vec<Value>) -> String {
        match builtin_ssh_mix(args) {
            Err(MixError::RuntimeError { msg, .. }) => msg,
            other => panic!("expected RuntimeError, got {other:?}"),
        }
    }

    #[test]
    fn ssh_mix_validates_args() {
        // Wrong arity.
        assert!(ssh_mix_err(vec![Value::String("h".into())]).contains("expected 2 or 3"));
        // Non-string source.
        assert!(
            ssh_mix_err(vec![Value::String("h".into()), Value::Number(1.0)])
                .contains("source must be a string")
        );
        // Non-map opts.
        assert!(
            ssh_mix_err(vec![
                Value::String("h".into()),
                Value::String("print(1)".into()),
                Value::Bool(true),
            ])
            .contains("opts must be a map")
        );
    }

    #[test]
    fn ssh_mix_rejects_stdin_opt() {
        let mut opts = IndexMap::new();
        opts.insert("stdin".into(), Value::String("x".into()));
        let msg = ssh_mix_err(vec![
            Value::String("h".into()),
            Value::String("print(1)".into()),
            Value::map(opts),
        ]);
        assert!(msg.contains("`stdin` opt is not allowed"), "got: {msg}");
    }

    #[test]
    fn ssh_mix_validates_decode_mode() {
        let mut opts = IndexMap::new();
        opts.insert("decode".into(), Value::String("yaml".into()));
        let msg = ssh_mix_err(vec![
            Value::String("h".into()),
            Value::String("print(1)".into()),
            Value::map(opts),
        ]);
        assert!(msg.contains("decode must be"), "got: {msg}");
    }

    #[test]
    fn ssh_mix_bindings_accept_and_reject_identifier_names_locally() {
        let valid = map_of(&[
            ("a", Value::Nil),
            ("A1", Value::Nil),
            ("_private", Value::Nil),
            ("args", Value::Nil),
            ("argv", Value::Nil),
        ]);
        let prefix = mix_binding_lines(&valid).expect("valid binding identifiers");
        for name in ["a", "A1", "_private", "args", "argv"] {
            assert!(
                prefix.contains(&format!("${name} = nil\n")),
                "missing binding {name} in {prefix:?}"
            );
        }

        for name in ["", "1bad", "bad-name", "has space", "$sigil", "naïve"] {
            let err = mix_binding_lines(&map_of(&[(name, Value::Nil)]))
                .expect_err("invalid binding identifier must fail locally");
            assert_eq!(
                err.info().map(|info| info.code.as_str()),
                Some("OPTION_INVALID"),
                "wrong code for {name:?}: {err}"
            );
            assert!(
                err.to_string().contains("invalid bindings key"),
                "wrong message for {name:?}: {err}"
            );
        }
    }

    #[test]
    fn ssh_mix_bindings_reject_bytes_and_buffers_locally() {
        let binaries = [
            Value::bytes(vec![0, 1, 2]),
            Value::Buffer(std::rc::Rc::new(std::cell::RefCell::new(vec![3, 4]))),
            Value::list(vec![Value::bytes(vec![5, 6])]),
        ];
        for binary in binaries {
            let err = super::builtin_ssh_mix(vec![
                Value::String("example.com".into()),
                Value::String("print(1)".into()),
                map_of(&[("bindings", map_of(&[("payload", binary)]))]),
            ])
            .expect_err("binary binding must fail before ssh");
            assert_eq!(
                err.info().map(|info| info.code.as_str()),
                Some("OPTION_INVALID")
            );
            assert!(
                err.to_string()
                    .contains("binary values cannot cross the strict-data driver"),
                "wrong binary diagnostic: {err}"
            );
        }
    }

    #[test]
    fn ssh_mix_bindings_requires_a_map_locally() {
        let err = super::builtin_ssh_mix(vec![
            Value::String("example.com".into()),
            Value::String("print(1)".into()),
            map_of(&[("bindings", Value::list(vec![]))]),
        ])
        .expect_err("non-map bindings must fail before ssh");
        assert_eq!(
            err.info().map(|info| info.code.as_str()),
            Some("OPTION_INVALID")
        );
        assert!(err.to_string().contains("bindings must be a map"));
    }

    #[tokio::test(flavor = "current_thread")]
    async fn ssh_mix_bindings_round_trip_every_strict_data_value_type() {
        let nested_map = map_of(&[
            ("inner", Value::String("value".into())),
            ("items", Value::list(vec![Value::Number(2.0), Value::Nil])),
        ]);
        let bindings = map_of(&[
            (
                "text",
                Value::String("quotes: \"hello\"\nUnicode: λ 🦀 and $literal".into()),
            ),
            ("number", Value::Number(42.5)),
            ("flag", Value::Bool(true)),
            ("nothing", Value::Nil),
            (
                "items",
                Value::list(vec![Value::String("nested".into()), nested_map.clone()]),
            ),
            ("record", nested_map),
        ]);
        let prefix = mix_binding_lines(&bindings).expect("encode typed bindings");
        let generated = format!(
            "{prefix}print(data_encode({{text: $text, number: $number, flag: $flag, \
             nothing: $nothing, items: $items, record: $record}}))\n"
        );
        let (_, stdout, stderr) = crate::run_capturing(&generated)
            .await
            .unwrap_or_else(|error| panic!("generated source failed: {error}\n{generated}"));
        assert!(stderr.is_empty(), "unexpected stderr: {stderr:?}");
        let decoded = crate::parse_data(stdout.trim()).expect("decode generated result");
        assert_eq!(
            decoded.to_mix_data_string().expect("re-encode result"),
            bindings
                .to_mix_data_string()
                .expect("encode expected bindings")
        );
    }

    #[tokio::test(flavor = "current_thread")]
    async fn ssh_mix_binding_source_looking_text_stays_data() {
        let payload = "\nprint(\"SHOULD_NOT_RUN\")\n$owned = true\nend\n";
        let bindings = map_of(&[("payload", Value::String(payload.into()))]);
        let prefix = mix_binding_lines(&bindings).expect("encode source-looking payload");
        let generated = format!("{prefix}print(data_encode({{payload: $payload}}))\n");
        let (_, stdout, stderr) = crate::run_capturing(&generated)
            .await
            .unwrap_or_else(|error| panic!("generated source failed: {error}\n{generated}"));
        assert!(stderr.is_empty(), "unexpected stderr: {stderr:?}");
        assert_eq!(
            stdout.lines().count(),
            1,
            "payload executed as source: {stdout:?}"
        );
        let decoded = crate::parse_data(stdout.trim()).expect("decode payload result");
        assert_eq!(
            decoded.to_mix_data_string().expect("re-encode result"),
            bindings
                .to_mix_data_string()
                .expect("encode expected payload")
        );
    }

    #[tokio::test(flavor = "current_thread")]
    async fn ssh_mix_bindings_have_no_reserved_names_and_caller_may_rebind() {
        let bindings = map_of(&[
            ("args", Value::String("bound args".into())),
            ("argv", Value::String("bound argv".into())),
        ]);
        let prefix = mix_binding_lines(&bindings).expect("args and argv are valid bindings");
        let generated = format!(
            "{prefix}$args = \"caller args\"\n$argv = 7\nprint(data_encode([$args, $argv]))\n"
        );
        let (_, stdout, stderr) = crate::run_capturing(&generated)
            .await
            .unwrap_or_else(|error| panic!("generated source failed: {error}\n{generated}"));
        assert!(stderr.is_empty(), "unexpected stderr: {stderr:?}");
        assert_eq!(stdout.trim(), "[\"caller args\", 7]");
    }

    #[test]
    fn ssh_mix_still_enforces_strict_opt_allowlist() {
        // Stripping `decode` must not open a hole: an unknown opt still
        // hits ssh_run's strict SSH_OPT_KEYS allowlist.
        let mut opts = IndexMap::new();
        opts.insert("bogus".into(), Value::Number(1.0));
        let msg = ssh_mix_err(vec![
            Value::String("h".into()),
            Value::String("print(1)".into()),
            Value::map(opts),
        ]);
        assert!(msg.contains("unknown opts key"), "got: {msg}");
    }

    #[test]
    fn ssh_mix_inherits_empty_and_nonstring_host_guards() {
        // Empty host and non-string host are both rejected by the
        // delegated builtin_ssh_run before any ssh spawn.
        assert!(
            ssh_mix_err(vec![
                Value::String(String::new()),
                Value::String("print(1)".into()),
            ])
            .contains("must not be empty")
        );
        assert!(
            ssh_mix_err(vec![Value::Number(1.0), Value::String("print(1)".into())])
                .contains("host must be a string")
        );
    }

    #[test]
    fn ssh_mix_inherits_host_guard() {
        // The leading-dash host guard in builtin_ssh_run must fire for
        // ssh_mix too (it delegates), before any ssh spawn.
        let msg = ssh_mix_err(vec![
            Value::String("-oProxyCommand=touch /tmp/pwn".into()),
            Value::String("print(1)".into()),
        ]);
        assert!(msg.contains("must not begin with '-'"), "got: {msg}");
    }

    #[test]
    fn ssh_run_rejects_leading_dash_host() {
        let err = builtin_ssh_run(vec![
            Value::String("-oProxyCommand=touch /tmp/pwn".into()),
            Value::String("true".into()),
        ])
        .expect_err("leading-dash host must be rejected before spawning ssh");
        match err {
            MixError::RuntimeError { msg, .. } => {
                assert!(msg.contains("must not begin with '-'"), "got: {msg}")
            }
            other => panic!("unexpected error variant: {other:?}"),
        }
    }

    #[test]
    fn build_ssh_argv_multiplex_does_not_disable_control_path() {
        // multiplex=true uses our own ControlPath; ControlPath=none
        // would defeat that.
        let opts = SshOpts {
            multiplex: true,
            ..SshOpts::default()
        };
        let argv = build_ssh_argv("lab", "echo hi", &opts);
        assert!(
            !argv.contains(&"ControlPath=none".to_string()),
            "multiplex=true must NOT include ControlPath=none: {:?}",
            argv
        );
    }

    #[test]
    fn build_ssh_argv_no_batch_omits_flag() {
        let opts = SshOpts {
            batch: false,
            ..SshOpts::default()
        };
        let argv = build_ssh_argv("lab", "echo hi", &opts);
        assert!(!argv.contains(&"BatchMode=yes".to_string()));
    }

    #[test]
    fn build_ssh_argv_multiplex_adds_control_options() {
        let opts = SshOpts {
            multiplex: true,
            ..SshOpts::default()
        };
        let argv = build_ssh_argv("lab", "echo hi", &opts);
        assert!(argv.contains(&"ControlMaster=auto".to_string()));
        assert!(argv.contains(&"ControlPersist=60s".to_string()));
        assert!(
            argv.iter().any(|s| s.starts_with("ControlPath=")),
            "expected ControlPath=… in {:?}",
            argv
        );
    }

    #[test]
    fn build_ssh_argv_extra_args_inserted_before_host() {
        let opts = SshOpts {
            extra_ssh_args: vec!["-i".into(), "/tmp/key".into()],
            ..SshOpts::default()
        };
        let argv = build_ssh_argv("lab", "true", &opts);
        let host_idx = argv.len() - 2;
        let i_pos = argv.iter().position(|s| s == "-i").expect("-i present");
        let key_pos = argv
            .iter()
            .position(|s| s == "/tmp/key")
            .expect("/tmp/key present");
        assert!(i_pos < host_idx);
        assert_eq!(key_pos, i_pos + 1);
    }

    #[test]
    fn parse_ssh_opts_none_returns_default() {
        let o = parse_ssh_opts(None).unwrap();
        assert_eq!(o.timeout, 30);
        assert_eq!(o.connect_timeout, 10);
        assert!(o.batch);
        assert!(!o.multiplex);
        assert_eq!(o.strict_host_key, "accept-new");
        assert!(o.env.is_empty());
        // Secure-by-default: env values travel via the stdin driver.
        assert_eq!(o.env_transport, "mix");
        assert!(o.cwd.is_none());
        assert!(o.stdin.is_none());
        assert!(o.extra_ssh_args.is_empty());
    }

    #[test]
    fn parse_ssh_opts_env_transport_values() {
        for v in ["mix", "sh", "argv"] {
            let m = map_of(&[("env_transport", Value::String(v.into()))]);
            assert_eq!(parse_ssh_opts(Some(&m)).unwrap().env_transport, v);
        }
        let m = map_of(&[("env_transport", Value::String("tls".into()))]);
        let e = parse_ssh_opts(Some(&m)).unwrap_err().to_string();
        assert!(e.contains("env_transport"), "got: {e}");
        let m = map_of(&[("env_transport", Value::Number(1.0))]);
        assert!(parse_ssh_opts(Some(&m)).is_err());
    }

    #[test]
    fn env_driver_mix_hides_values_and_propagates_exit() {
        let env = vec![("SECRET".to_string(), "s3k\"r$t\nx".to_string())];
        let d = build_env_driver("mix", &env, "cd '/tmp' && echo hi").unwrap();
        // export line present, value data-escaped (quote/dollar/newline inert)
        assert!(d.contains("export SECRET = "), "driver:\n{d}");
        assert!(
            !d.contains("s3k\"r$t\nx"),
            "raw value must be escaped:\n{d}"
        );
        // command runs via run_stream with the exit code propagated
        assert!(
            d.contains("exit(run_stream([\"sh\", \"-c\", "),
            "driver:\n{d}"
        );
        // The driver itself must be a parseable Mix program.
        let mut lexer = crate::lexer::Lexer::new(&d);
        let tokens = lexer.tokenize().expect("driver lexes");
        let mut parser = crate::parser::Parser::new(tokens, &d);
        parser.parse_program().expect("driver parses as Mix");
    }

    #[test]
    fn env_driver_sh_quotes_values() {
        let env = vec![("TOK".to_string(), "a'b c".to_string())];
        let d = build_env_driver("sh", &env, "echo ok && printenv TOK").unwrap();
        assert!(d.starts_with("export TOK="), "driver:\n{d}");
        // shell_quote wraps the hostile value; the command follows verbatim.
        assert!(d.contains("echo ok && printenv TOK"), "driver:\n{d}");
        assert!(!d.contains("a'b c\n"), "value must be shell-quoted:\n{d}");
    }

    #[test]
    fn ssh_run_env_plus_stdin_conflicts_unless_argv() {
        let mut m = indexmap::IndexMap::new();
        let mut env = indexmap::IndexMap::new();
        env.insert("A".to_string(), Value::String("b".into()));
        m.insert("env".to_string(), Value::map(env));
        m.insert("stdin".to_string(), Value::String("data".into()));
        let e = builtin_ssh_run(vec![
            Value::String("zqx-no-such-host".into()),
            Value::String("true".into()),
            Value::map(m.clone()),
        ])
        .unwrap_err()
        .to_string();
        assert!(e.contains("conflict"), "got: {e}");
        // With explicit argv transport the combination is ALLOWED — the
        // conflict gate must not fire. Prove it WITHOUT spawning ssh: a
        // NUL byte in the command fails build_remote_command, which runs
        // strictly AFTER the gate — so reaching the NUL error means the
        // gate passed the env+stdin combination through.
        m.insert("env_transport".to_string(), Value::String("argv".into()));
        let e = builtin_ssh_run(vec![
            Value::String("zqx-no-such-host".into()),
            Value::String("true\0".into()),
            Value::map(m),
        ])
        .unwrap_err()
        .to_string();
        assert!(
            e.contains("NUL") && !e.contains("conflict"),
            "argv transport must accept env+stdin (expected the post-gate NUL error), got: {e}"
        );
    }

    #[test]
    fn parse_ssh_opts_unknown_key_errors() {
        let m = map_of(&[("nope", Value::Bool(true))]);
        assert!(parse_ssh_opts(Some(&m)).is_err());
    }

    #[test]
    fn parse_ssh_opts_non_map_errors() {
        let v = Value::String("oops".into());
        assert!(parse_ssh_opts(Some(&v)).is_err());
    }

    #[test]
    fn parse_ssh_opts_strict_host_key_rejects_unknown() {
        let m = map_of(&[("strict_host_key", Value::String("maybe".into()))]);
        assert!(parse_ssh_opts(Some(&m)).is_err());
    }

    #[test]
    fn parse_ssh_opts_strict_host_key_accepts_known_values() {
        for v in ["yes", "no", "accept-new", "ask"] {
            let m = map_of(&[("strict_host_key", Value::String(v.into()))]);
            let o = parse_ssh_opts(Some(&m)).unwrap();
            assert_eq!(o.strict_host_key, v);
        }
    }

    #[test]
    fn parse_ssh_opts_timeout_must_be_nonneg_int() {
        let m = map_of(&[("timeout", Value::Number(-1.0))]);
        assert!(parse_ssh_opts(Some(&m)).is_err());
        let m = map_of(&[("timeout", Value::Number(1.5))]);
        assert!(parse_ssh_opts(Some(&m)).is_err());
        let m = map_of(&[("timeout", Value::String("30".into()))]);
        assert!(parse_ssh_opts(Some(&m)).is_err());
        let m = map_of(&[("timeout", Value::Number(30.0))]);
        assert_eq!(parse_ssh_opts(Some(&m)).unwrap().timeout, 30);
    }

    #[test]
    fn parse_ssh_opts_env_coerces_simple_types() {
        let mut env_map = IndexMap::new();
        env_map.insert("S".into(), Value::String("hi".into()));
        env_map.insert("N".into(), Value::Number(7.0));
        env_map.insert("B".into(), Value::Bool(true));
        let m = map_of(&[("env", Value::map(env_map))]);
        let o = parse_ssh_opts(Some(&m)).unwrap();
        // env preserves insertion order (IndexMap).
        assert_eq!(
            o.env,
            vec![
                ("S".into(), "hi".into()),
                ("N".into(), "7".into()),
                ("B".into(), "true".into()),
            ]
        );
    }

    #[test]
    fn parse_ssh_opts_env_rejects_complex_value() {
        let mut env_map = IndexMap::new();
        env_map.insert("BAD".into(), Value::list(vec![]));
        let m = map_of(&[("env", Value::map(env_map))]);
        assert!(parse_ssh_opts(Some(&m)).is_err());
    }

    #[test]
    fn parse_ssh_opts_env_rejects_nul_in_value() {
        let mut env_map = IndexMap::new();
        env_map.insert("X".into(), Value::String("ok\0nope".into()));
        let m = map_of(&[("env", Value::map(env_map))]);
        assert!(parse_ssh_opts(Some(&m)).is_err());
    }

    #[test]
    fn parse_ssh_opts_extra_ssh_args_strict_strings() {
        let m = map_of(&[(
            "extra_ssh_args",
            Value::list(vec![
                Value::String("-i".into()),
                Value::String("/tmp/key".into()),
            ]),
        )]);
        let o = parse_ssh_opts(Some(&m)).unwrap();
        assert_eq!(o.extra_ssh_args, vec!["-i", "/tmp/key"]);

        let m_bad = map_of(&[("extra_ssh_args", Value::list(vec![Value::Number(7.0)]))]);
        assert!(parse_ssh_opts(Some(&m_bad)).is_err());
    }

    #[test]
    fn ssh_run_dispatch_arity_and_host_validation() {
        // Wrong arity.
        assert!(call_builtin("ssh_run", vec![]).is_err());
        assert!(call_builtin("ssh_run", vec![Value::String("h".into())]).is_err());
        // Empty host.
        assert!(
            call_builtin(
                "ssh_run",
                vec![Value::String("".into()), Value::String("true".into())]
            )
            .is_err()
        );
        // Non-string host.
        assert!(
            call_builtin(
                "ssh_run",
                vec![Value::Number(1.0), Value::String("true".into())]
            )
            .is_err()
        );
        // NUL in host.
        assert!(
            call_builtin(
                "ssh_run",
                vec![Value::String("ho\0st".into()), Value::String("true".into())]
            )
            .is_err()
        );
    }

    fn make_ssh_map(
        host: &str,
        ok: bool,
        exit_code: i64,
        stdout: &str,
        stderr: &str,
        timed_out: bool,
        interrupted: bool,
    ) -> indexmap::IndexMap<String, Value> {
        let mut m = indexmap::IndexMap::new();
        m.insert("host".into(), Value::String(host.into()));
        m.insert("ok".into(), Value::Bool(ok));
        m.insert("exit_code".into(), Value::Number(exit_code as f64));
        m.insert("stdout".into(), Value::String(stdout.into()));
        m.insert("stderr".into(), Value::String(stderr.into()));
        m.insert("timed_out".into(), Value::Bool(timed_out));
        m.insert("interrupted".into(), Value::Bool(interrupted));
        m
    }

    #[test]
    fn ssh_must_from_map_returns_stdout_on_ok() {
        let m = make_ssh_map("h1", true, 0, "hello\n", "", false, false);
        let v = ssh_must_from_map(&m).expect("ok branch");
        match &v {
            Value::String(s) => assert_eq!(s, "hello\n"),
            _ => panic!("expected stdout string"),
        }
    }

    #[test]
    fn ssh_must_from_map_failed_includes_host_and_exit_code() {
        let m = make_ssh_map("box.local", false, 17, "", "boom", false, false);
        let err = ssh_must_from_map(&m).unwrap_err();
        let msg = format!("{}", err);
        assert!(msg.contains("box.local"), "msg={}", msg);
        assert!(msg.contains("17"), "msg={}", msg);
        assert!(msg.contains("failed"), "msg={}", msg);
        assert!(msg.contains("boom"), "msg={}", msg);
    }

    #[test]
    fn ssh_must_from_map_timeout_disposition() {
        let m = make_ssh_map("h", false, -1, "", "", true, false);
        let err = ssh_must_from_map(&m).unwrap_err();
        let msg = format!("{}", err);
        assert!(msg.contains("timed out"), "msg={}", msg);
        assert!(msg.contains("-1"), "msg={}", msg);
    }

    #[test]
    fn ssh_must_from_map_interrupt_wins_over_timeout() {
        // Both bools set — interrupt label takes precedence, matching
        // the run_with_timeout tie-breaker.
        let m = make_ssh_map("h", false, -2, "", "", true, true);
        let err = ssh_must_from_map(&m).unwrap_err();
        let msg = format!("{}", err);
        assert!(msg.contains("interrupted"), "msg={}", msg);
        assert!(!msg.contains("timed out"), "msg={}", msg);
    }

    #[test]
    fn ssh_must_from_map_truncates_long_stderr() {
        // 800-byte stderr; only the first 512 bytes (plus an ellipsis)
        // should appear in the error message.
        let big = "a".repeat(800);
        let m = make_ssh_map("h", false, 1, "", &big, false, false);
        let err = ssh_must_from_map(&m).unwrap_err();
        let msg = format!("{}", err);
        assert!(msg.contains("…"), "expected truncation marker: {}", msg);
        // Spot-check that we didn't include the tail.
        let tail = "a".repeat(700);
        assert!(!msg.contains(&tail), "stderr was not truncated");
    }

    #[test]
    fn ssh_must_from_map_truncates_on_char_boundary() {
        // Construct stderr where byte 512 falls in the middle of a
        // multi-byte UTF-8 sequence; truncation must back off to the
        // nearest char boundary (no panic, no replacement char in the
        // wrong place).
        let mut s = "a".repeat(511);
        s.push('é'); // é = 0xC3 0xA9 — straddles byte 512
        s.push_str(&"b".repeat(400));
        let m = make_ssh_map("h", false, 1, "", &s, false, false);
        let err = ssh_must_from_map(&m).unwrap_err();
        let _msg = format!("{}", err); // must not panic
    }

    #[test]
    fn ssh_must_from_map_omits_stderr_section_when_empty() {
        let m = make_ssh_map("h", false, 7, "", "", false, false);
        let err = ssh_must_from_map(&m).unwrap_err();
        let msg = format!("{}", err);
        assert!(msg.contains("(exit_code=7)"), "msg={}", msg);
        // No trailing ": " followed by content when stderr is empty.
        assert!(!msg.ends_with(": "), "msg={}", msg);
    }

    #[test]
    fn ssh_must_dispatch_validates_args() {
        // ssh_must inherits ssh_run's argument validation by delegating
        // to it — these calls should fail before we ever spawn ssh.
        assert!(call_builtin("ssh_must", vec![]).is_err());
        assert!(call_builtin("ssh_must", vec![Value::String("h".into())]).is_err());
        assert!(
            call_builtin(
                "ssh_must",
                vec![Value::String("".into()), Value::String("true".into())]
            )
            .is_err()
        );
        assert!(
            call_builtin(
                "ssh_must",
                vec![Value::String("ho\0st".into()), Value::String("true".into())]
            )
            .is_err()
        );
    }

    // ---- run_with_timeout polling / interrupt / timeout tests ----
    //
    // These tests exercise the timeout and interrupt cooperation paths
    // without going through ssh_run dispatch. INTERRUPT_FLAG is process-
    // wide, so the tests that mutate it are serialised through the
    // shared `interrupt::TEST_LOCK` (also held by `interrupt::tests`)
    // and clear the flag before *and* after each run.
    //
    // We invoke real `sh -c '...'` children rather than mocking, which
    // is the only way to test the kill-escalation path end-to-end.

    fn sh(script: &str) -> Vec<String> {
        vec!["sh".into(), "-c".into(), script.into()]
    }

    #[test]
    fn run_with_timeout_completes_normally() {
        let _g = crate::interrupt::TEST_LOCK.lock().unwrap();
        crate::interrupt::_test_clear();
        let argv = sh("printf 'hi'; exit 7");
        let outcome = run_with_timeout(&argv, None, 5, "test").expect("spawn ok");
        assert_eq!(outcome.exit_code, 7);
        assert!(!outcome.timed_out);
        assert!(!outcome.interrupted);
        assert_eq!(outcome.stdout, b"hi");
    }

    #[test]
    fn run_with_timeout_fires_on_deadline() {
        let _g = crate::interrupt::TEST_LOCK.lock().unwrap();
        crate::interrupt::_test_clear();
        let argv = sh("printf 'pre'; sleep 5");
        let started = std::time::Instant::now();
        let outcome = run_with_timeout(&argv, None, 1, "test").expect("spawn ok");
        let elapsed = started.elapsed();
        assert!(outcome.timed_out, "expected timed_out");
        assert!(!outcome.interrupted, "interrupt must not be set");
        assert_eq!(outcome.exit_code, -1, "timeout sentinel");
        assert!(
            outcome.stdout.starts_with(b"pre"),
            "stdout drained pre-kill"
        );
        // Timeout path is SIGKILL-immediate to the process group, so
        // wall-clock should be ~1s (the deadline) + slack — well below
        // sleep 5. Anything close to 5s would mean the kill never
        // landed.
        assert!(
            elapsed < std::time::Duration::from_secs(4),
            "wall-clock leaked past timeout: {:?}",
            elapsed
        );
        crate::interrupt::_test_clear();
    }

    #[test]
    fn run_with_timeout_interrupt_wins_no_deadline() {
        let _g = crate::interrupt::TEST_LOCK.lock().unwrap();
        // Pre-arm the interrupt flag — the very first poll will see it.
        let f = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(true));
        let _ = crate::interrupt::init(f.clone());
        // If a previous test won init(), set the canonical flag.
        crate::interrupt::INTERRUPT_FLAG
            .get()
            .expect("flag wired")
            .store(true, std::sync::atomic::Ordering::SeqCst);
        let argv = sh("sleep 5");
        let outcome = run_with_timeout(&argv, None, 0, "test").expect("spawn ok");
        assert!(outcome.interrupted, "expected interrupted");
        assert!(!outcome.timed_out, "timed_out must be false");
        assert_eq!(outcome.exit_code, -2, "interrupt sentinel");
        crate::interrupt::_test_clear();
    }

    #[test]
    fn run_with_timeout_interrupt_beats_timeout() {
        let _g = crate::interrupt::TEST_LOCK.lock().unwrap();
        let f = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(true));
        let _ = crate::interrupt::init(f.clone());
        crate::interrupt::INTERRUPT_FLAG
            .get()
            .expect("flag wired")
            .store(true, std::sync::atomic::Ordering::SeqCst);
        // With both interrupt pre-set AND a 1s timeout configured,
        // the tie-breaker says interrupt wins.
        let argv = sh("sleep 5");
        let outcome = run_with_timeout(&argv, None, 1, "test").expect("spawn ok");
        assert!(outcome.interrupted);
        assert!(!outcome.timed_out);
        assert_eq!(outcome.exit_code, -2);
        crate::interrupt::_test_clear();
    }

    #[test]
    fn run_with_timeout_zero_disables_deadline() {
        let _g = crate::interrupt::TEST_LOCK.lock().unwrap();
        crate::interrupt::_test_clear();
        // Quick child; verifies that timeout_s=0 doesn't somehow trip
        // the deadline check on the first poll.
        let argv = sh("exit 0");
        let outcome = run_with_timeout(&argv, None, 0, "test").expect("spawn ok");
        assert_eq!(outcome.exit_code, 0);
        assert!(!outcome.timed_out);
        assert!(!outcome.interrupted);
    }

    /// Regression: timeout must bound wall-clock even when the child
    /// has descendants that inherit the stdout pipe.
    ///
    /// Setup: `sh` traps SIGTERM (ignores it) and waits on a 5s
    /// foreground `sleep`. Without process-group kill:
    ///   1. SIGTERM → sh ignores → 2s grace → SIGKILL → sh dies.
    ///   2. `sleep 5` is orphaned, reparented to init, keeps running
    ///      with the stdout FD it inherited from sh.
    ///   3. `child.wait()` reaps sh promptly, but the drain thread's
    ///      `read_to_end` on stdout blocks until sleep exits naturally
    ///      because the pipe still has an open writer.
    ///   4. Wall-clock drifts toward `sleep 5` (~5s), not the 1s
    ///      timeout the caller asked for.
    ///
    /// With process-group kill (`setpgid(0,0)` at spawn + `kill(-pgid,
    /// SIGKILL)` on timeout), the orphaned descendant dies with the
    /// leader, the pipe FD closes, and wall-clock is bounded.
    ///
    /// This is the substrate-level guarantee — `ssh` exhibits the
    /// same descendant-FD pattern (the local ssh client and any
    /// helpers it forks), which is how the bug originally surfaced
    /// when flex-testing against a real remote.
    #[test]
    #[cfg(unix)]
    fn run_with_timeout_kills_process_group() {
        let _g = crate::interrupt::TEST_LOCK.lock().unwrap();
        crate::interrupt::_test_clear();
        let argv = sh("trap '' TERM; sleep 5");
        let started = std::time::Instant::now();
        let outcome = run_with_timeout(&argv, None, 1, "test").expect("spawn ok");
        let elapsed = started.elapsed();
        assert!(outcome.timed_out, "expected timed_out");
        assert!(!outcome.interrupted, "interrupt must not be set");
        assert_eq!(outcome.exit_code, -1, "timeout sentinel");
        // SIGKILL-immediate to the process group must bound this
        // tightly. Allow some slack for poll interval + scheduler
        // jitter, but anything close to the natural 5s sleep means
        // the descendant kept the pipe alive (the original bug).
        assert!(
            elapsed < std::time::Duration::from_millis(2500),
            "wall-clock leaked past timeout: {:?} — process group not killed?",
            elapsed
        );
        crate::interrupt::_test_clear();
    }

    #[test]
    fn run_with_timeout_drains_stderr_separately() {
        let _g = crate::interrupt::TEST_LOCK.lock().unwrap();
        crate::interrupt::_test_clear();
        let argv = sh("printf 'OUT'; printf 'ERR' >&2; exit 0");
        let outcome = run_with_timeout(&argv, None, 5, "test").expect("spawn ok");
        assert_eq!(outcome.exit_code, 0);
        assert_eq!(outcome.stdout, b"OUT");
        assert_eq!(outcome.stderr, b"ERR");
    }

    #[test]
    fn run_with_timeout_writes_stdin_through() {
        let _g = crate::interrupt::TEST_LOCK.lock().unwrap();
        crate::interrupt::_test_clear();
        let argv = sh("cat");
        let outcome = run_with_timeout(&argv, Some("hello stdin"), 5, "test").expect("spawn ok");
        assert_eq!(outcome.exit_code, 0);
        assert_eq!(outcome.stdout, b"hello stdin");
    }

    /// SIGTERM-grace escalation only fires on the **interrupt** path
    /// (Ctrl-C). The timeout path is now SIGKILL-immediate per the
    /// "Process-group discipline" docstring: graceful teardown on a
    /// hard deadline would re-introduce the wall-clock drift the
    /// deadline exists to bound.
    ///
    /// This test uses a busy-loop shell with `trap '' TERM` so SIGTERM
    /// to the process group is genuinely ignored — sleep-based traps
    /// don't work because PG-kill also signals the inner `sleep`,
    /// which has no trap and dies promptly. A CPU spin is ugly but is
    /// the only portable way to keep the *whole* PG alive past TERM.
    ///
    /// The interrupt flag must be flipped *after* sh has installed the
    /// trap; pre-arming the flag races against the trap setup (spawn
    /// → first poll → SIGTERM happens in <1ms, before sh runs the
    /// `trap` builtin) and SIGTERM kills sh by default. A sidecar
    /// thread with a small delay sidesteps the race.
    ///
    /// Expected timing: ~200ms (sidecar delay) + ~2000ms (grace) +
    /// slack = [1800ms, 3500ms].
    #[test]
    #[cfg(unix)]
    fn run_with_timeout_interrupt_escalates_through_sigterm_grace() {
        let _g = crate::interrupt::TEST_LOCK.lock().unwrap();
        crate::interrupt::_test_clear();
        let f = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let _ = crate::interrupt::init(f.clone());
        // Sidecar: flip the interrupt flag after 200ms so sh has time
        // to execute `trap '' TERM` before the kill path runs.
        let sidecar = std::thread::spawn(|| {
            std::thread::sleep(std::time::Duration::from_millis(200));
            crate::interrupt::INTERRUPT_FLAG
                .get()
                .expect("flag wired")
                .store(true, std::sync::atomic::Ordering::SeqCst);
        });
        let argv = sh("trap '' TERM; while :; do :; done");
        let started = std::time::Instant::now();
        let outcome = run_with_timeout(&argv, None, 0, "test").expect("spawn ok");
        let elapsed = started.elapsed();
        sidecar.join().unwrap();
        assert!(outcome.interrupted, "expected interrupted=true");
        assert!(!outcome.timed_out, "timed_out must be false");
        assert_eq!(outcome.exit_code, -2, "interrupt sentinel preserved");
        // Lower bound: 2s grace must actually elapse before SIGKILL.
        // If the grace shortcut got broken (e.g. went straight to
        // SIGKILL), elapsed would be ~200ms (sidecar delay only).
        assert!(
            elapsed >= std::time::Duration::from_millis(1800),
            "SIGTERM grace window did not fire: elapsed={:?}",
            elapsed
        );
        // Upper bound: 200ms sidecar + 2s grace + slack. Anything
        // close to "infinity" would mean SIGKILL escalation never
        // fired.
        assert!(
            elapsed < std::time::Duration::from_millis(3500),
            "SIGKILL escalation took too long: {:?}",
            elapsed
        );
        crate::interrupt::_test_clear();
    }

    #[test]
    fn run_with_timeout_signal_killed_exit_code() {
        let _g = crate::interrupt::TEST_LOCK.lock().unwrap();
        crate::interrupt::_test_clear();
        // Self-kill via SIGTERM. The child has no `code()`; the
        // outcome should map to 128 + SIGTERM = 143 (Unix), not the
        // -1/-2/-3 sentinels reserved for timeout/interrupt/unknown.
        let argv = sh("kill -TERM $$ ; sleep 1");
        let outcome = run_with_timeout(&argv, None, 5, "test").expect("spawn ok");
        assert!(!outcome.timed_out);
        assert!(!outcome.interrupted);
        #[cfg(unix)]
        {
            assert_eq!(outcome.exit_code, 128 + 15, "SIGTERM=15 → 143");
        }
    }

    fn run_stream(argv: Vec<&str>) -> crate::error::MixResult<Option<Value>> {
        let list = Value::list(argv.into_iter().map(|s| Value::String(s.into())).collect());
        call_builtin("run_stream", vec![list])
    }

    #[test]
    fn run_stream_returns_zero_for_true() {
        let got = run_stream(vec!["true"]).expect("spawn ok");
        assert_eq!(got, Some(Value::Number(0.0)));
    }

    #[test]
    fn run_stream_returns_nonzero_exit_code() {
        // `sh -c 'exit 3'` exits 3; run_stream spawns sh directly (no wrapping
        // shell of its own) and surfaces the child's code verbatim.
        let got = run_stream(vec!["sh", "-c", "exit 3"]).expect("spawn ok");
        assert_eq!(got, Some(Value::Number(3.0)));
    }

    #[test]
    fn run_stream_rejects_empty_argv() {
        let err = call_builtin("run_stream", vec![Value::list(vec![])]).unwrap_err();
        assert!(format!("{err}").contains("empty"), "got: {err}");
    }

    #[test]
    fn run_stream_rejects_non_list_arg() {
        let err = call_builtin("run_stream", vec![Value::String("ls".into())]).unwrap_err();
        assert!(format!("{err}").contains("list of strings"), "got: {err}");
    }

    #[test]
    fn run_stream_rejects_non_string_element() {
        // A non-string argv element is rejected with its index — no implicit
        // stringification of the Number into an argument.
        let argv = Value::list(vec![Value::String("echo".into()), Value::Number(1.0)]);
        let err = call_builtin("run_stream", vec![argv]).unwrap_err();
        let msg = format!("{err}");
        assert!(
            msg.contains("argv[1]") && msg.contains("string"),
            "got: {msg}"
        );
    }

    #[test]
    fn run_stream_spawn_failure_is_catchable_error() {
        // A nonexistent program → spawn error surfaced as a RuntimeError, not
        // a panic; the message names the program.
        let err = run_stream(vec!["/nonexistent/cosmix/run_stream_probe"]).unwrap_err();
        assert!(format!("{err}").contains("run_stream_probe"), "got: {err}");
    }

    /// run_stream with an options map. The child inherits stdio, so a test
    /// cannot read its output — every assertion below is made by the CHILD
    /// (`test …` succeeds → 0, fails → 1) and read back as the exit code.
    /// That is what makes these falsifiable: the paired no-opts case must
    /// come back 1, or the probe proves nothing about the option.
    fn run_stream_opts(
        argv: Vec<&str>,
        opts: Vec<(&str, Value)>,
    ) -> crate::error::MixResult<Option<Value>> {
        let list = Value::list(argv.into_iter().map(|s| Value::String(s.into())).collect());
        let mut m = indexmap::IndexMap::new();
        for (k, v) in opts {
            m.insert(k.to_string(), v);
        }
        call_builtin("run_stream", vec![list, Value::map(m)])
    }

    fn env_map(pairs: Vec<(&str, &str)>) -> Value {
        let mut m = indexmap::IndexMap::new();
        for (k, v) in pairs {
            m.insert(k.to_string(), Value::String(v.into()));
        }
        Value::map(m)
    }

    #[test]
    fn run_stream_env_reaches_the_child() {
        let probe = vec!["/bin/sh", "-c", "test \"$MIX_RS_PROBE\" = ok"];
        assert_eq!(
            run_stream_opts(
                probe.clone(),
                vec![("env", env_map(vec![("MIX_RS_PROBE", "ok")]))]
            )
            .expect("spawn ok"),
            Some(Value::Number(0.0)),
            "child did not see MIX_RS_PROBE=ok"
        );
        // Falsifier: the same probe without the option must FAIL, or the
        // assertion above would pass on an inherited variable.
        assert_eq!(
            run_stream(probe).expect("spawn ok"),
            Some(Value::Number(1.0)),
            "MIX_RS_PROBE was already set in the parent — probe is vacuous"
        );
    }

    #[test]
    fn run_stream_env_value_is_not_word_split_or_expanded() {
        // The value goes through execve, not a shell: spaces, globs and a
        // literal `$HOME` survive verbatim.
        let raw = "a b * $HOME";
        let probe = vec!["/bin/sh", "-c", "test \"$MIX_RS_RAW\" = 'a b * $HOME'"];
        // Falsifier first: without the option the probe must FAIL, so a green
        // assertion below cannot come from a variable the parent already had.
        assert_eq!(
            run_stream_opts(probe.clone(), vec![]).expect("spawn ok"),
            Some(Value::Number(1.0)),
            "parent already carries MIX_RS_RAW — the test below would pass vacuously"
        );
        assert_eq!(
            run_stream_opts(probe, vec![("env", env_map(vec![("MIX_RS_RAW", raw)]))])
                .expect("spawn ok"),
            Some(Value::Number(0.0))
        );
    }

    #[test]
    fn run_stream_clear_env_drops_inherited_variables() {
        // HOME is set in every environment this suite runs in; with
        // clear_env the child must not see it. argv[0] is ABSOLUTE so the
        // probe tests the environment, not PATH resolution (which the
        // options can also move — see run_stream_path_comes_from_the_child).
        assert_eq!(
            run_stream_opts(
                vec!["/bin/sh", "-c", "test -z \"$HOME\""],
                vec![("clear_env", Value::Bool(true))]
            )
            .expect("spawn ok"),
            Some(Value::Number(0.0)),
            "clear_env left HOME visible to the child"
        );
        assert!(
            std::env::var_os("HOME").is_some(),
            "HOME unset in the parent — the probe above is vacuous"
        );
    }

    #[test]
    fn run_stream_clear_env_then_env_layers_exactly_the_given_pairs() {
        // Order is clear-then-layer: the explicit pair survives the clear,
        // and nothing else does.
        assert_eq!(
            run_stream_opts(
                vec![
                    "/bin/sh",
                    "-c",
                    "test \"$MIX_RS_ONLY\" = yes && test -z \"$HOME\""
                ],
                vec![
                    ("clear_env", Value::Bool(true)),
                    ("env", env_map(vec![("MIX_RS_ONLY", "yes")])),
                ]
            )
            .expect("spawn ok"),
            Some(Value::Number(0.0))
        );
    }

    #[test]
    fn run_stream_cwd_sets_the_child_working_directory() {
        // `/` is the one directory guaranteed not to be a symlink, so
        // `pwd -P` comparing equal is unambiguous.
        assert_eq!(
            run_stream_opts(
                vec!["/bin/sh", "-c", "test \"$(pwd -P)\" = /"],
                vec![("cwd", Value::String("/".into()))]
            )
            .expect("spawn ok"),
            Some(Value::Number(0.0))
        );
        // Falsifier: without cwd the child inherits the test runner's
        // directory, which is the crate dir, not `/`.
        assert_eq!(
            run_stream(vec!["/bin/sh", "-c", "test \"$(pwd -P)\" = /"]).expect("spawn ok"),
            Some(Value::Number(1.0)),
            "test process is already at / — cwd probe is vacuous"
        );
    }

    #[test]
    fn run_stream_rejects_run_argv_only_opts_by_name() {
        // The D5 motivating case: `timeout` used to be a silently-ignored
        // surplus argument. It is now named and refused, with the runner
        // that does honour it in the message.
        for (key, val, hint) in [
            ("timeout", Value::Number(5.0), "deadline"),
            ("stdin", Value::String("x".into()), "stdin"),
            ("max_output", Value::Number(1.0), "capture"),
            ("stream", Value::Bool(true), "capture"),
        ] {
            let err = run_stream_opts(vec!["true"], vec![(key, val)]).unwrap_err();
            let msg = format!("{err:?}");
            assert!(msg.contains("OPTION_INVALID"), "{key}: got {msg}");
            assert!(msg.contains("not supported"), "{key}: got {msg}");
            assert!(msg.contains("run_argv"), "{key}: got {msg}");
            assert!(msg.contains(hint), "{key}: got {msg}");
        }
    }

    #[test]
    fn run_stream_rejects_unknown_opt_and_non_map_opts() {
        let err = run_stream_opts(vec!["true"], vec![("bogus", Value::Number(1.0))]).unwrap_err();
        let msg = format!("{err:?}");
        assert!(msg.contains("OPTION_INVALID"), "got: {msg}");
        assert!(msg.contains("unknown option 'bogus'"), "got: {msg}");
        assert!(msg.contains("env, clear_env, cwd"), "got: {msg}");

        let err = call_builtin(
            "run_stream",
            vec![
                Value::list(vec![Value::String("true".into())]),
                Value::String("notamap".into()),
            ],
        )
        .unwrap_err();
        assert!(
            format!("{err:?}").contains("options must be a map"),
            "got: {err:?}"
        );
    }

    #[test]
    fn run_stream_rejects_bad_env_names_and_values_before_spawning() {
        // Delegated to parse_run_argv_opts, so the rules are the same ones
        // run_argv enforces — a bad name never reaches execve.
        let err =
            run_stream_opts(vec!["true"], vec![("env", env_map(vec![("2BAD", "x")]))]).unwrap_err();
        assert!(
            format!("{err:?}").contains("not a valid name"),
            "got: {err:?}"
        );

        let mut bad = indexmap::IndexMap::new();
        bad.insert("OK".to_string(), Value::list(vec![]));
        let err = run_stream_opts(vec!["true"], vec![("env", Value::map(bad))]).unwrap_err();
        assert!(
            format!("{err:?}").contains("must be a string, number, or bool"),
            "got: {err:?}"
        );
    }

    #[test]
    fn run_stream_path_comes_from_the_child_not_the_parent() {
        // Surprising but load-bearing, and the reason the probes above use an
        // absolute argv[0]: a bare program name is resolved against the PATH
        // the CHILD will get. Overriding it makes a program that is plainly on
        // the parent's PATH fail to spawn.
        let err = run_stream_opts(
            vec!["sh", "-c", "true"],
            vec![("env", env_map(vec![("PATH", "/nonexistent")]))],
        )
        .unwrap_err();
        assert!(
            format!("{err}").contains("failed to spawn sh"),
            "got: {err}"
        );
        // Falsifier: the identical call without the PATH override spawns.
        assert_eq!(
            run_stream(vec!["sh", "-c", "true"]).expect("spawn ok"),
            Some(Value::Number(0.0))
        );
    }

    #[test]
    fn run_stream_nil_opts_is_the_no_opts_call() {
        let got = call_builtin(
            "run_stream",
            vec![Value::list(vec![Value::String("true".into())]), Value::Nil],
        )
        .expect("spawn ok");
        assert_eq!(got, Some(Value::Number(0.0)));
        // nil means "no options", NOT "an empty environment" (nil and `{}`
        // both mean no options): the child must still inherit the environment.
        // Without this arm the test above passes just as well against a nil
        // that silently implied clear_env. Neither arm distinguishes the new
        // contract from the old one — a pre-0.51.0 build ignores the surplus
        // nil and passes both; strict_arity.rs pins the arity change itself.
        assert_eq!(
            call_builtin(
                "run_stream",
                vec![
                    Value::list(vec![
                        Value::String("/bin/sh".into()),
                        Value::String("-c".into()),
                        Value::String("test -n \"$HOME\"".into()),
                    ]),
                    Value::Nil,
                ],
            )
            .expect("spawn ok"),
            Some(Value::Number(0.0))
        );
    }
}

#[cfg(test)]
mod run_tests {
    use super::{builtin_run, sanitize_for_diag};
    use crate::error::MixError;
    use crate::value::Value;

    fn run(cmd: &str) -> Result<Option<Value>, MixError> {
        builtin_run(vec![Value::String(cmd.to_string())])
    }

    #[test]
    fn run_succeeds_returns_trimmed_stdout() {
        let v = run("echo hi").expect("ok").expect("some");
        match &v {
            Value::String(s) => assert_eq!(s, "hi"),
            other => panic!("expected string, got {:?}", other),
        }
    }

    #[test]
    fn run_nonzero_throws_die_error_with_rc_and_cmd() {
        let err = run("false").expect_err("expected die");
        match err {
            MixError::DieError { msg } => {
                assert!(msg.contains("rc=1"), "missing rc=1 in {msg:?}");
                assert!(msg.contains("false"), "missing cmd in {msg:?}");
            }
            other => panic!("expected DieError, got {:?}", other),
        }
    }

    #[test]
    fn run_includes_stderr_tail_in_die_message() {
        let err = run("sh -c 'echo boom 1>&2; exit 2'").expect_err("expected die");
        match err {
            MixError::DieError { msg } => {
                assert!(msg.contains("rc=2"), "missing rc=2 in {msg:?}");
                assert!(msg.contains("boom"), "missing stderr tail in {msg:?}");
            }
            other => panic!("expected DieError, got {:?}", other),
        }
    }

    #[cfg(unix)]
    #[test]
    fn run_signal_killed_throws_die_with_signal() {
        // Signal exits report shell-convention 128+sig (SIGTERM → rc=143)
        // since the run_with_timeout unification — was "signal=15" before.
        let err = run("kill -TERM $$").expect_err("expected die");
        match err {
            MixError::DieError { msg } => {
                assert!(
                    msg.contains("rc=143"),
                    "missing rc=143 (128+SIGTERM) in {msg:?}"
                );
            }
            other => panic!("expected DieError, got {:?}", other),
        }
    }

    #[test]
    fn run_sanitizes_control_chars_in_die_message() {
        // stderr contains: ANSI escape, newline, U+2028 line sep, U+202E RLO,
        // U+200B ZWSP, single quote, plus survivable content.
        let cmd = "printf '\\033[31mred\\033[0m\\nLINE2\\xe2\\x80\\xa8LF\\xe2\\x80\\xaertl\\xe2\\x80\\x8b'\\''end' 1>&2; exit 3";
        let err = run(cmd).expect_err("expected die");
        match err {
            MixError::DieError { msg } => {
                // surviving printable content
                for needle in ["red", "LINE2", "LF", "rtl", "end"] {
                    assert!(msg.contains(needle), "missing {needle:?} in {msg:?}");
                }
                // escape (C0) gone
                assert!(!msg.contains('\x1b'), "ESC must be sanitized: {msg:?}");
                // raw newlines collapsed to spaces
                assert!(!msg.contains('\n'), "newline must be sanitized: {msg:?}");
                // unicode line/format controls gone
                assert!(
                    !msg.contains('\u{2028}'),
                    "U+2028 must be sanitized: {msg:?}"
                );
                assert!(
                    !msg.contains('\u{202E}'),
                    "U+202E must be sanitized: {msg:?}"
                );
                assert!(
                    !msg.contains('\u{200B}'),
                    "U+200B must be sanitized: {msg:?}"
                );
                // command excerpt is wrapped in single quotes; embedded ' escaped as \'
                assert!(
                    msg.contains("\\'"),
                    "embedded single-quote must be escaped: {msg:?}"
                );
            }
            other => panic!("expected DieError, got {:?}", other),
        }
    }

    #[test]
    fn sanitize_for_diag_replaces_invisible_format_chars() {
        // ZWSP, ZWNJ, ZWJ, BOM, word joiner — all invisible spoofing chars.
        let s = "a\u{200B}b\u{200C}c\u{200D}d\u{FEFF}e\u{2060}f";
        let out = sanitize_for_diag(s);
        assert_eq!(out, "a?b?c?d?e?f");
    }

    #[test]
    fn sanitize_for_diag_collapses_unicode_line_separators() {
        let s = "a\u{2028}b\u{2029}c";
        let out = sanitize_for_diag(s);
        assert_eq!(out, "a b c");
    }
}

#[cfg(all(test, feature = "dkim"))]
mod dkim_tests {
    use super::builtin_dkim_keygen;
    use crate::value::Value;

    fn unwrap_map(mut v: Value) -> indexmap::IndexMap<String, Value> {
        match &mut v {
            Value::Map(m) => std::rc::Rc::unwrap_or_clone(std::mem::take(m)),
            other => panic!("expected map, got {other:?}"),
        }
    }
    fn s(v: &Value) -> &str {
        match v {
            Value::String(s) => s.as_str(),
            other => panic!("expected string, got {other:?}"),
        }
    }

    #[test]
    fn dkim_keygen_ed25519_returns_expected_shape() {
        let out = builtin_dkim_keygen(vec![Value::String("ed25519".into())])
            .expect("ok")
            .expect("some");
        let m = unwrap_map(out);
        assert_eq!(s(&m["algorithm"]), "ed25519-sha256");
        assert!(s(&m["private_pem"]).starts_with("-----BEGIN PRIVATE KEY-----"));
        assert!(
            s(&m["private_pem"])
                .trim_end()
                .ends_with("-----END PRIVATE KEY-----")
        );
        assert!(!s(&m["public_b64"]).is_empty());
        assert!(s(&m["dns_txt_record"]).starts_with("v=DKIM1; k=ed25519; p="));
    }

    #[test]
    fn dkim_keygen_rsa_default_2048_shape() {
        // 2048-bit RSA keygen takes ~1–3 s in debug; run it once.
        let out = builtin_dkim_keygen(vec![Value::String("rsa".into())])
            .expect("ok")
            .expect("some");
        let m = unwrap_map(out);
        assert_eq!(s(&m["algorithm"]), "rsa-sha256");
        assert!(s(&m["private_pem"]).starts_with("-----BEGIN RSA PRIVATE KEY-----"));
        assert!(s(&m["dns_txt_record"]).starts_with("v=DKIM1; k=rsa; p="));
    }

    #[test]
    fn dkim_keygen_rejects_unknown_algo() {
        let err = builtin_dkim_keygen(vec![Value::String("dsa".into())]).unwrap_err();
        let msg = format!("{err}");
        assert!(msg.contains("unknown algorithm"), "msg: {msg}");
    }

    #[test]
    fn dkim_keygen_rejects_rsa_below_floor() {
        let err = builtin_dkim_keygen(vec![Value::String("rsa".into()), Value::Number(512.0)])
            .unwrap_err();
        let msg = format!("{err}");
        assert!(msg.contains("below 1024 floor"), "msg: {msg}");
    }

    #[test]
    fn dkim_keygen_rejects_rsa_above_cap() {
        let err = builtin_dkim_keygen(vec![Value::String("rsa".into()), Value::Number(8192.0)])
            .unwrap_err();
        let msg = format!("{err}");
        assert!(msg.contains("exceeds 4096 cap"), "msg: {msg}");
    }

    #[test]
    fn dkim_keygen_no_args_errors_cleanly() {
        let err = builtin_dkim_keygen(vec![]).unwrap_err();
        let msg = format!("{err}");
        assert!(msg.contains("at least 1 argument"), "msg: {msg}");
    }

    #[test]
    fn dkim_keygen_rejects_non_integer_rsa_bits() {
        let err = builtin_dkim_keygen(vec![Value::String("rsa".into()), Value::Number(2047.9)])
            .unwrap_err();
        let msg = format!("{err}");
        assert!(msg.contains("must be an integer"), "msg: {msg}");
    }
}

#[cfg(test)]
mod chmod_tests {
    use super::builtin_chmod;
    use crate::value::Value;
    use std::os::unix::fs::PermissionsExt;

    fn tmpfile(suffix: &str) -> std::path::PathBuf {
        let mut p = std::env::temp_dir();
        p.push(format!(
            "cosmix-mix-chmod-{}-{}",
            std::process::id(),
            suffix
        ));
        std::fs::write(&p, b"x").unwrap();
        p
    }

    fn mode_of(p: &std::path::Path) -> u32 {
        std::fs::metadata(p).unwrap().permissions().mode() & 0o7777
    }

    #[test]
    fn chmod_string_octal_sets_mode() {
        let p = tmpfile("oct-str");
        builtin_chmod(vec![
            Value::String(p.to_string_lossy().into()),
            Value::String("0600".into()),
        ])
        .expect("ok");
        assert_eq!(mode_of(&p), 0o600);
        let _ = std::fs::remove_file(&p);
    }

    #[test]
    fn chmod_string_no_leading_zero_still_octal() {
        let p = tmpfile("oct-nolead");
        builtin_chmod(vec![
            Value::String(p.to_string_lossy().into()),
            Value::String("750".into()),
        ])
        .expect("ok");
        assert_eq!(mode_of(&p), 0o750);
        let _ = std::fs::remove_file(&p);
    }

    #[test]
    fn chmod_number_is_the_mode_value() {
        // A number is the mode VALUE (v0.11.0). The Mix literal 0o644 lexes to
        // 420.0, which is mode 0o644 — so `chmod(p, 0o644)` does the right
        // thing. (Rust's 0o644 below is likewise 420.)
        let p = tmpfile("num-val");
        builtin_chmod(vec![
            Value::String(p.to_string_lossy().into()),
            Value::Number(0o644 as f64),
        ])
        .expect("ok");
        assert_eq!(mode_of(&p), 0o644);
        let _ = std::fs::remove_file(&p);

        // A BARE DECIMAL is the value, NOT its digits read as octal: 644 is
        // mode 0o1204, not 0o644 (the old digits-as-octal hack is retired).
        let p2 = tmpfile("num-dec");
        builtin_chmod(vec![
            Value::String(p2.to_string_lossy().into()),
            Value::Number(644.0),
        ])
        .expect("ok");
        assert_eq!(mode_of(&p2), 0o1204);
        let _ = std::fs::remove_file(&p2);
    }

    #[test]
    fn chmod_rejects_out_of_range_octal_string() {
        // The String arm enforces the same 0..=0o7777 ceiling as the Number arm.
        let p = tmpfile("oor-str");
        let _ = std::fs::write(&p, "x");
        let err = builtin_chmod(vec![
            Value::String(p.to_string_lossy().into()),
            Value::String("17777".into()),
        ])
        .unwrap_err();
        assert!(format!("{err}").contains("out of range"), "msg: {err}");
        let _ = std::fs::remove_file(&p);
    }

    #[test]
    fn chmod_rejects_bad_octal_string() {
        let p = tmpfile("bad");
        let err = builtin_chmod(vec![
            Value::String(p.to_string_lossy().into()),
            Value::String("9xx".into()),
        ])
        .unwrap_err();
        let msg = format!("{err}");
        assert!(msg.contains("invalid octal"), "msg: {msg}");
        let _ = std::fs::remove_file(&p);
    }

    #[test]
    fn chmod_rejects_out_of_range_number() {
        let p = tmpfile("oor");
        let err = builtin_chmod(vec![
            Value::String(p.to_string_lossy().into()),
            Value::Number(99999.0),
        ])
        .unwrap_err();
        // M-1: the domain layer rejects this, so the stable contract is the
        // CODE, not the prose. 99999 is outside the mode domain.
        assert_eq!(
            err.info().map(|i| i.code.as_str()),
            Some("VALUE_OUT_OF_RANGE")
        );
        let _ = std::fs::remove_file(&p);
    }

    #[test]
    fn chmod_rejects_non_integer_number() {
        let p = tmpfile("frac");
        let err = builtin_chmod(vec![
            Value::String(p.to_string_lossy().into()),
            Value::Number(600.9),
        ])
        .unwrap_err();
        // M-1: fractional-where-whole-required is a domain failure.
        assert_eq!(
            err.info().map(|i| i.code.as_str()),
            Some("VALUE_OUT_OF_RANGE")
        );
        let _ = std::fs::remove_file(&p);
    }
}

#[cfg(test)]
mod write_new_tests {
    use super::builtin_write_new;
    use crate::value::Value;
    use std::os::unix::fs::PermissionsExt;

    fn tmpdir(suffix: &str) -> std::path::PathBuf {
        let mut p = std::env::temp_dir();
        p.push(format!(
            "cosmix-mix-write-new-{}-{}",
            std::process::id(),
            suffix
        ));
        std::fs::create_dir_all(&p).unwrap();
        p
    }

    #[test]
    fn write_new_creates_file_with_exact_mode() {
        let d = tmpdir("create-mode");
        let p = d.join("secret.pem");
        builtin_write_new(vec![
            Value::String(p.to_string_lossy().into()),
            Value::String("BODY".into()),
            Value::String("0600".into()),
        ])
        .expect("ok");
        let body = std::fs::read_to_string(&p).unwrap();
        assert_eq!(body, "BODY");
        let mode = std::fs::metadata(&p).unwrap().permissions().mode() & 0o7777;
        // O_CREAT applies (mode & ~umask) — under the typical 022 umask
        // the visible mode is 0600 regardless. The point is the file is
        // NEVER world-readable: it cannot be looser than what we asked
        // for, only equal-or-tighter via the umask.
        assert!(
            mode & 0o077 == 0,
            "secret leaked group/other bits: {:o}",
            mode
        );
        assert!(mode & 0o600 == 0o600, "owner missing rw: {:o}", mode);
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn write_new_refuses_existing_file() {
        let d = tmpdir("excl");
        let p = d.join("k.pem");
        std::fs::write(&p, b"old").unwrap();
        let err = builtin_write_new(vec![
            Value::String(p.to_string_lossy().into()),
            Value::String("new".into()),
            Value::String("0600".into()),
        ])
        .unwrap_err();
        // Real errno EEXIST — proves O_EXCL is in play, not a pre-check.
        let msg = format!("{err}");
        assert!(
            msg.contains("exists") || msg.contains("File exists"),
            "msg: {msg}"
        );
        // Original content survives.
        assert_eq!(std::fs::read_to_string(&p).unwrap(), "old");
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn copy_and_remove_roundtrip() {
        use super::{builtin_copy, builtin_copy_tree, builtin_remove, builtin_remove_dir};
        let d = tmpdir("copytree");
        let src = d.join("src");
        std::fs::create_dir_all(src.join("sub")).unwrap();
        std::fs::write(src.join("a.txt"), b"AAA").unwrap();
        std::fs::write(src.join("sub/b.txt"), b"BBB").unwrap();
        std::os::unix::fs::symlink("a.txt", src.join("link")).unwrap();
        let dst = d.join("dst");

        builtin_copy_tree(vec![
            Value::String(src.to_string_lossy().into()),
            Value::String(dst.to_string_lossy().into()),
        ])
        .expect("copy_tree");
        assert_eq!(std::fs::read_to_string(dst.join("a.txt")).unwrap(), "AAA");
        assert_eq!(
            std::fs::read_to_string(dst.join("sub/b.txt")).unwrap(),
            "BBB"
        );
        assert!(
            std::fs::symlink_metadata(dst.join("link"))
                .unwrap()
                .file_type()
                .is_symlink(),
            "symlink copied as a real symlink"
        );

        // single-file copy
        let one_dst = d.join("one.txt");
        builtin_copy(vec![
            Value::String(src.join("a.txt").to_string_lossy().into()),
            Value::String(one_dst.to_string_lossy().into()),
        ])
        .expect("copy");
        assert_eq!(std::fs::read_to_string(&one_dst).unwrap(), "AAA");

        // remove (rm -f) — file, then a no-op on the now-missing path
        builtin_remove(vec![Value::String(one_dst.to_string_lossy().into())]).expect("remove");
        assert!(!one_dst.exists());
        builtin_remove(vec![Value::String(one_dst.to_string_lossy().into())])
            .expect("remove of a missing path is a no-op");

        // remove_dir (rm -rf) — tree, then a no-op on the now-missing path
        builtin_remove_dir(vec![Value::String(dst.to_string_lossy().into())]).expect("remove_dir");
        assert!(!dst.exists());
        builtin_remove_dir(vec![Value::String(dst.to_string_lossy().into())])
            .expect("remove_dir of a missing path is a no-op");

        let _ = std::fs::remove_dir_all(&d);
    }

    /// The pair exists so a script can both make a link and audit one. The
    /// assertions that matter are the ones separating `read_link` from
    /// `realpath`: a relative target comes back relative, and a target that
    /// does not exist comes back anyway rather than raising.
    #[test]
    fn symlink_creates_and_read_link_reports_the_stored_target() {
        use super::{builtin_read_link, builtin_symlink};
        let d = std::env::temp_dir().join(format!("mix-symlink-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        let link = d.join("link");
        let s = |p: &std::path::Path| Value::String(p.to_string_lossy().into());

        builtin_symlink(vec![Value::String("../elsewhere/absent".into()), s(&link)])
            .expect("a relative, dangling target is a legal link");
        assert!(
            std::fs::symlink_metadata(&link).unwrap().is_symlink(),
            "the link exists even though its target does not"
        );
        let got = builtin_read_link(vec![s(&link)]).unwrap().unwrap();
        assert_eq!(
            got.to_mix_string(),
            "../elsewhere/absent",
            "verbatim, not resolved"
        );

        let err = builtin_symlink(vec![Value::String("whatever".into()), s(&link)])
            .expect_err("EEXIST over an existing link");
        assert!(format!("{err:?}").contains("symlink"), "{err:?}");

        let plain = d.join("plain");
        std::fs::write(&plain, "x").unwrap();
        let err = builtin_read_link(vec![s(&plain)]).expect_err("not a symlink");
        assert!(format!("{err:?}").contains("read_link"), "{err:?}");

        let _ = std::fs::remove_dir_all(&d);
    }

    // The distinction that cost a git-hook installer three separate defects:
    // "can I open something here" and "is this name taken" are different
    // questions, and they disagree on exactly one input — a dangling link.
    #[test]
    fn exists_follows_by_default_and_lstats_when_told_not_to() {
        use super::builtin_exists;
        let d = std::env::temp_dir().join(format!("mix-exists-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        let link = d.join("dangling");
        std::os::unix::fs::symlink("nowhere", &link).unwrap();
        let s = |p: &std::path::Path| Value::String(p.to_string_lossy().into());
        let lstat = || {
            let mut m = indexmap::IndexMap::new();
            m.insert("follow_symlinks".to_string(), Value::Bool(false));
            Value::Map(std::rc::Rc::new(m))
        };

        assert!(
            !builtin_exists(vec![s(&link)]).unwrap().unwrap().is_truthy(),
            "the default follows the link, and it goes nowhere"
        );
        assert!(
            builtin_exists(vec![s(&link), lstat()])
                .unwrap()
                .unwrap()
                .is_truthy(),
            "the name is taken, whatever the target does"
        );

        let plain = d.join("plain");
        std::fs::write(&plain, "x").unwrap();
        for opts in [vec![s(&plain)], vec![s(&plain), lstat()]] {
            assert!(
                builtin_exists(opts).unwrap().unwrap().is_truthy(),
                "both forms agree on a regular file"
            );
        }
        let absent = d.join("absent");
        for opts in [vec![s(&absent)], vec![s(&absent), lstat()]] {
            assert!(
                !builtin_exists(opts).unwrap().unwrap().is_truthy(),
                "and on nothing at all"
            );
        }

        let err = builtin_exists(vec![s(&plain), Value::String("nope".into())])
            .expect_err("options must be a map or nil");
        assert!(format!("{err:?}").contains("exists()"), "{err:?}");

        let _ = std::fs::remove_dir_all(&d);
    }

    /// `rename` is here for the atomic-replace guarantee, so the test pins the
    /// two properties a caller relies on: the destination ends up holding the
    /// source's bytes even when it already existed, and the source is gone
    /// afterwards (it moved, it was not copied). The missing-source case is
    /// pinned too — unlike `remove`, this one raises rather than no-opping,
    /// because "rename something that isn't there" is always a caller bug.
    #[test]
    fn rename_replaces_atomically_and_consumes_the_source() {
        use super::builtin_rename;
        let d = std::env::temp_dir().join(format!("mix-rename-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        let src = d.join("new");
        let dst = d.join("live");
        std::fs::write(&src, "NEW").unwrap();
        std::fs::write(&dst, "OLD").unwrap();

        builtin_rename(vec![
            Value::String(src.to_string_lossy().into()),
            Value::String(dst.to_string_lossy().into()),
        ])
        .expect("rename over an existing destination");
        assert_eq!(std::fs::read_to_string(&dst).unwrap(), "NEW");
        assert!(!src.exists(), "the source is consumed, not copied");

        let err = builtin_rename(vec![
            Value::String(d.join("absent").to_string_lossy().into()),
            Value::String(dst.to_string_lossy().into()),
        ])
        .expect_err("renaming a missing source raises");
        assert!(format!("{err:?}").contains("rename"), "{err:?}");

        let _ = std::fs::remove_dir_all(&d);
    }

    /// Discriminator for the "mode is applied at O_CREAT, not via a
    /// separate fchmod" guarantee. Under `umask 0o077`:
    ///   - If write_new uses `OpenOptions::mode()` (umask-masked):
    ///     a `0o644` request lands at `0o644 & ~0o077 == 0o600`.
    ///   - If write_new used `write_file` + `set_permissions(0o644)`
    ///     (fchmod bypasses umask): the file would end at `0o644`.
    ///
    /// Observing `0o600` proves we're going through the O_CREAT mode
    /// path. umask is process-global so this test serializes with the
    /// other write_new tests via a mutex.
    #[test]
    fn write_new_mode_flows_through_o_creat_not_post_chmod() {
        use std::sync::Mutex;
        static UMASK_LOCK: Mutex<()> = Mutex::new(());
        let _g = UMASK_LOCK.lock().unwrap();

        // SAFETY: libc::umask is process-global and not thread-safe with
        // other syscalls that consult umask; the static Mutex above
        // serializes the only tests in this module that touch it. We
        // restore the prior umask before returning so other test files
        // are unaffected.
        let prev = unsafe { libc::umask(0o077) };
        let d = tmpdir("umask-disc");
        let p = d.join("k.txt");
        let result = builtin_write_new(vec![
            Value::String(p.to_string_lossy().into()),
            Value::String("BODY".into()),
            Value::String("0644".into()),
        ]);
        let mode = std::fs::metadata(&p)
            .ok()
            .map(|m| m.permissions().mode() & 0o7777);
        unsafe { libc::umask(prev) };
        result.expect("ok");
        assert_eq!(
            mode,
            Some(0o600),
            "umask 0o077 should mask 0o644 → 0o600; got {:?}. \
             If this is 0o644, write_new is using a post-creation fchmod \
             that bypasses umask — re-introducing the early-window mode race.",
            mode.map(|m| format!("{:o}", m))
        );
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn write_new_rejects_bad_mode() {
        let d = tmpdir("badmode");
        let p = d.join("k.pem");
        let err = builtin_write_new(vec![
            Value::String(p.to_string_lossy().into()),
            Value::String("BODY".into()),
            Value::String("9xx".into()),
        ])
        .unwrap_err();
        let msg = format!("{err}");
        assert!(msg.contains("invalid octal"), "msg: {msg}");
        assert!(!p.exists(), "file should not have been created on bad mode");
        let _ = std::fs::remove_dir_all(&d);
    }
}

#[cfg(test)]
mod flock_tests {
    use super::*;
    use std::os::fd::AsRawFd;
    use std::sync::Mutex;

    static TEST_LOCK: Mutex<()> = Mutex::new(());
    const CHILD_PATH_ENV: &str = "COSMIX_MIX_FLOCK_TEST_PATH";
    const CHILD_READY_ENV: &str = "COSMIX_MIX_FLOCK_TEST_READY";
    const CHILD_EXIT_ENV: &str = "COSMIX_MIX_FLOCK_TEST_EXIT";
    const CHILD_SHARED_ENV: &str = "COSMIX_MIX_FLOCK_TEST_SHARED";

    fn tmpdir(suffix: &str) -> std::path::PathBuf {
        let nonce = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let p = std::env::temp_dir().join(format!(
            "cosmix-mix-flock-{}-{nonce}-{suffix}",
            std::process::id()
        ));
        std::fs::create_dir_all(&p).unwrap();
        p
    }

    fn s(path: &std::path::Path) -> Value {
        Value::String(path.to_string_lossy().into_owned())
    }

    fn wait_opts(seconds: f64) -> Value {
        let mut opts = indexmap::IndexMap::new();
        opts.insert("wait".into(), Value::Number(seconds));
        Value::Map(std::rc::Rc::new(opts))
    }

    fn shared_opts() -> Value {
        let mut opts = indexmap::IndexMap::new();
        opts.insert("shared".into(), Value::Bool(true));
        Value::Map(std::rc::Rc::new(opts))
    }

    fn bool_result(result: MixResult<Option<Value>>) -> bool {
        match result.expect("builtin raised") {
            Some(Value::Bool(v)) => v,
            other => panic!("expected bool, got {other:?}"),
        }
    }

    struct ChildLockHolder {
        child: Option<std::process::Child>,
        exit_path: std::path::PathBuf,
    }

    impl ChildLockHolder {
        fn finish(mut self) -> std::process::ExitStatus {
            std::fs::write(&self.exit_path, b"exit").unwrap();
            self.child.take().unwrap().wait().unwrap()
        }
    }

    impl Drop for ChildLockHolder {
        fn drop(&mut self) {
            if let Some(mut child) = self.child.take() {
                let _ = std::fs::write(&self.exit_path, b"exit");
                let _ = child.wait();
            }
        }
    }

    fn spawn_lock_holder(
        path: &std::path::Path,
        signals: &std::path::Path,
        shared: bool,
    ) -> ChildLockHolder {
        let ready = signals.join("ready");
        let exit_path = signals.join("exit");
        let child = std::process::Command::new(std::env::current_exe().unwrap())
            .arg("flock_child_process")
            .arg("--nocapture")
            .arg("--test-threads=1")
            .env(CHILD_PATH_ENV, path)
            .env(CHILD_READY_ENV, &ready)
            .env(CHILD_EXIT_ENV, &exit_path)
            .env(CHILD_SHARED_ENV, if shared { "1" } else { "0" })
            .spawn()
            .expect("spawn flock child test process");
        let mut holder = ChildLockHolder {
            child: Some(child),
            exit_path,
        };

        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        while !ready.exists() {
            if let Some(status) = holder.child.as_mut().unwrap().try_wait().unwrap() {
                panic!("flock child exited before signalling ready: {status}");
            }
            assert!(
                std::time::Instant::now() < deadline,
                "flock child did not acquire the lock within five seconds"
            );
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
        holder
    }

    /// A subprocess-only helper. The normal unit-test run has none of these
    /// environment variables and returns immediately; parent tests re-exec the
    /// current test binary with this test-name filter to get a real second
    /// process holding the kernel lock.
    #[test]
    fn flock_child_process() {
        let Ok(path) = std::env::var(CHILD_PATH_ENV) else {
            return;
        };
        let ready = std::path::PathBuf::from(std::env::var_os(CHILD_READY_ENV).unwrap());
        let exit_path = std::path::PathBuf::from(std::env::var_os(CHILD_EXIT_ENV).unwrap());

        let args = if std::env::var(CHILD_SHARED_ENV).as_deref() == Ok("1") {
            vec![Value::String(path), shared_opts()]
        } else {
            vec![Value::String(path)]
        };
        assert!(bool_result(builtin_flock(args)));
        std::fs::write(ready, b"ready").unwrap();
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
        while !exit_path.exists() {
            assert!(
                std::time::Instant::now() < deadline,
                "parent never asked flock child to exit"
            );
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
        // Deliberately do not call funlock: returning from the only selected
        // test exits this helper process, proving fd-close-at-exit semantics.
    }

    #[test]
    fn flock_acquire_succeeds() {
        let _guard = TEST_LOCK.lock().unwrap();
        let d = tmpdir("acquire");
        let path = d.join("lock");
        assert!(bool_result(builtin_flock(vec![s(&path)])));
        assert!(bool_result(builtin_funlock(vec![s(&path)])));
        let _ = std::fs::remove_dir_all(d);
    }

    #[test]
    fn second_acquire_in_same_process_is_true_without_a_second_retained_fd() {
        let _guard = TEST_LOCK.lock().unwrap();
        let d = tmpdir("idempotent");
        let path = d.join("lock");
        assert!(bool_result(builtin_flock(vec![s(&path)])));

        let canonical = std::fs::canonicalize(&path).unwrap();
        let first_fd = FLOCK_REGISTRY
            .0
            .lock()
            .unwrap()
            .held
            .get(&canonical)
            .expect("first acquire registered")
            .as_raw_fd();
        assert!(bool_result(builtin_flock(vec![s(&path)])));
        let registry = FLOCK_REGISTRY.0.lock().unwrap();
        assert_eq!(
            registry.held.len(),
            1,
            "second acquire retained another registry entry"
        );
        assert_eq!(
            registry.held.get(&canonical).unwrap().as_raw_fd(),
            first_fd,
            "second acquire replaced the retained fd"
        );
        drop(registry);

        assert!(bool_result(builtin_funlock(vec![s(&path)])));
        let _ = std::fs::remove_dir_all(d);
    }

    #[test]
    fn funlock_returns_true_then_false() {
        let _guard = TEST_LOCK.lock().unwrap();
        let d = tmpdir("unlock");
        let path = d.join("lock");
        assert!(bool_result(builtin_flock(vec![s(&path)])));
        assert!(bool_result(builtin_funlock(vec![s(&path)])));
        assert!(!bool_result(builtin_funlock(vec![s(&path)])));
        let _ = std::fs::remove_dir_all(d);
    }

    #[test]
    fn contention_from_a_second_process_returns_false() {
        let _guard = TEST_LOCK.lock().unwrap();
        let d = tmpdir("contended");
        let path = d.join("lock");
        let holder = spawn_lock_holder(&path, &d, false);
        assert!(!bool_result(builtin_flock(vec![s(&path)])));
        assert!(holder.finish().success());
        let _ = std::fs::remove_dir_all(d);
    }

    #[test]
    fn process_exit_releases_the_lock() {
        let _guard = TEST_LOCK.lock().unwrap();
        let d = tmpdir("exit-release");
        let path = d.join("lock");
        let holder = spawn_lock_holder(&path, &d, false);
        assert!(
            !bool_result(builtin_flock(vec![s(&path)])),
            "the child must demonstrably hold the lock before it exits"
        );
        assert!(holder.finish().success());
        assert!(
            bool_result(builtin_flock(vec![s(&path)])),
            "child exit did not close and release its retained fd"
        );
        assert!(bool_result(builtin_funlock(vec![s(&path)])));
        let _ = std::fs::remove_dir_all(d);
    }

    #[test]
    fn wait_returns_false_after_roughly_the_requested_time() {
        let _guard = TEST_LOCK.lock().unwrap();
        let d = tmpdir("wait");
        let path = d.join("lock");
        let holder = spawn_lock_holder(&path, &d, false);
        let started = std::time::Instant::now();
        assert!(!bool_result(builtin_flock(vec![s(&path), wait_opts(0.2)])));
        let elapsed = started.elapsed();
        assert!(
            elapsed >= std::time::Duration::from_millis(150),
            "wait returned too early after {elapsed:?}"
        );
        assert!(
            elapsed < std::time::Duration::from_secs(2),
            "wait overshot unreasonably: {elapsed:?}"
        );
        assert!(holder.finish().success());
        let _ = std::fs::remove_dir_all(d);
    }

    #[test]
    fn shared_option_allows_a_real_second_process_to_acquire() {
        let _guard = TEST_LOCK.lock().unwrap();
        let d = tmpdir("shared");
        let path = d.join("lock");
        assert!(bool_result(builtin_flock(vec![s(&path), shared_opts()])));
        let holder = spawn_lock_holder(&path, &d, true);
        assert!(holder.finish().success());
        assert!(bool_result(builtin_funlock(vec![s(&path)])));
        let _ = std::fs::remove_dir_all(d);
    }

    #[test]
    fn directory_path_raises_instead_of_reporting_contention() {
        let _guard = TEST_LOCK.lock().unwrap();
        let d = tmpdir("directory");
        let err = builtin_flock(vec![s(&d)]).expect_err("opening a directory must raise");
        assert!(format!("{err}").contains("flock"), "wrong error: {err}");
        let _ = std::fs::remove_dir_all(d);
    }
}

#[cfg(test)]
mod fs_meta_tests {
    use super::*;

    fn tmpdir(suffix: &str) -> std::path::PathBuf {
        let mut p = std::env::temp_dir();
        p.push(format!(
            "cosmix-mix-fsmeta-{}-{}",
            std::process::id(),
            suffix
        ));
        std::fs::create_dir_all(&p).unwrap();
        p
    }

    fn s(p: &std::path::Path) -> Value {
        Value::String(p.to_string_lossy().into())
    }

    fn map_of(v: Option<Value>) -> indexmap::IndexMap<String, Value> {
        // `Value` implements `Drop`, so a field can't be moved out of it
        // in a match arm — clone the map out of the borrowed value.
        match &v {
            Some(Value::Map(m)) => (**m).clone(),
            other => panic!("expected a map, got {other:?}"),
        }
    }

    fn bool_of(v: Option<Value>) -> bool {
        match v {
            Some(Value::Bool(b)) => b,
            other => panic!("expected a bool, got {other:?}"),
        }
    }

    #[test]
    fn access_checks_execute_and_read_permissions() {
        let d = tmpdir("access-mode");
        let p = d.join("hook");
        std::fs::write(&p, b"#!/bin/sh\nexit 0\n").unwrap();

        // chmod is exact and therefore independent of the ambient umask.
        builtin_chmod(vec![s(&p), Value::Number(0o755 as f64)]).expect("chmod 0755");
        assert!(bool_of(
            builtin_access(vec![s(&p), Value::String("x".into())]).expect("access x")
        ));

        builtin_chmod(vec![s(&p), Value::Number(0o644 as f64)]).expect("chmod 0644");
        assert!(!bool_of(
            builtin_access(vec![s(&p), Value::String("x".into())]).expect("access x")
        ));
        assert!(bool_of(
            builtin_access(vec![s(&p), Value::String("r".into())]).expect("access r")
        ));

        // The assertions above remain valid under euid 0: root bypasses
        // ordinary r/w mode checks, but X_OK still requires at least one
        // execute bit, and 0644 deliberately has none. A denied `w` is the one
        // case root WOULD contradict, so it is asserted only when this process
        // is not root.
        assert!(bool_of(
            builtin_access(vec![s(&p), Value::String("w".into())]).expect("access w on 0644")
        ));
        builtin_chmod(vec![s(&p), Value::Number(0o444 as f64)]).expect("chmod 0444");
        if unsafe { libc::geteuid() } != 0 {
            assert!(!bool_of(
                builtin_access(vec![s(&p), Value::String("w".into())]).expect("access w on 0444")
            ));
            // A multi-letter mode is one conjunctive question, not a fold of
            // separate ones: "rw" on a readable-but-not-writable file is false
            // even though the `r` half holds.
            assert!(!bool_of(
                builtin_access(vec![s(&p), Value::String("rw".into())]).expect("access rw on 0444")
            ));
        }
        assert!(bool_of(
            builtin_access(vec![s(&p), Value::String("rf".into())]).expect("access rf on 0444")
        ));

        // What is NOT tested here, deliberately: that a POSIX ACL denial is
        // honoured -- the property that motivated calling the kernel instead of
        // doing mode arithmetic. It is not reachable from a test that owns the
        // file. An owner matches ACL_USER_OBJ, which named-user entries and the
        // mask cannot restrict, so no ACL this process is able to set on its
        // own file can deny this process. Demonstrating it needs a second uid,
        // which a unit test does not have. The guarantee rests on faccessat
        // being the kernel's own decision procedure rather than a reimplementation
        // of it, which is exactly why the reimplementation was removed.
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn access_missing_and_existence_mode_return_false_without_raising() {
        let d = tmpdir("access-exists");
        let present = d.join("present");
        let missing = d.join("missing");
        std::fs::write(&present, b"").unwrap();

        assert!(bool_of(
            builtin_access(vec![s(&present), Value::String("f".into())]).expect("f on present")
        ));
        assert!(!bool_of(
            builtin_access(vec![s(&missing), Value::String("f".into())])
                .expect("f on missing must not raise")
        ));
        assert!(!bool_of(
            builtin_access(vec![s(&missing), Value::String("x".into())])
                .expect("x on missing must not raise")
        ));

        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn access_rejects_bad_modes_and_interior_nul() {
        for mode in ["", "rr", "q", "rwxz"] {
            let err = builtin_access(vec![
                Value::String("/bin/sh".into()),
                Value::String(mode.into()),
            ])
            .expect_err("bad mode must raise");
            assert!(
                format!("{err}").contains("access()"),
                "wrong error for mode {mode:?}: {err}"
            );
        }

        let err = builtin_access(vec![
            Value::String("/tmp/bad\0path".into()),
            Value::String("f".into()),
        ])
        .expect_err("interior NUL must raise");
        assert!(
            format!("{err}").contains("interior NUL"),
            "wrong NUL error: {err}"
        );
    }

    /// The bitwise family exists so that a caller asking about *some* bits of a
    /// mode does not have to compare a whole mode for equality. The setgid case
    /// below is the one that motivated it: a filesystem that forces `02755` on
    /// a file chmod'ed `0755` honoured the request, and an equality test calls
    /// that a refusal — the exact misdiagnosis this replaced.
    #[test]
    fn bitwise_ops_answer_about_permission_bits() {
        let call = |name: &str, args: Vec<f64>| {
            let v = call_builtin(name, args.into_iter().map(Value::Number).collect())
                .expect("bitwise call ok")
                .expect("bitwise returns a value");
            match v {
                Value::Number(x) => x,
                other => panic!("{name} returned {}", other.type_name()),
            }
        };
        // Some execute bit, whatever else the filesystem added of its own.
        assert_eq!(call("band", vec![0o755 as f64, 0o111 as f64]), 73.0);
        assert_eq!(call("band", vec![0o2755 as f64, 0o111 as f64]), 73.0);
        assert_eq!(call("band", vec![0o644 as f64, 0o111 as f64]), 0.0);
        assert_eq!(call("bor", vec![0o700 as f64, 0o055 as f64]), 493.0);
        assert_eq!(call("bxor", vec![6.0, 3.0]), 5.0);
        assert_eq!(call("bnot", vec![0.0]), -1.0);
        assert_eq!(call("bshl", vec![1.0, 10.0]), 1024.0);
        // Arithmetic, not logical: the sign bit is replicated.
        assert_eq!(call("bshr", vec![-8.0, 1.0]), -4.0);
    }

    /// Every way of asking a bitwise question that `f64` cannot answer exactly
    /// must raise. A silent truncation here would hand back bits the caller
    /// never wrote, which is worse than no builtin at all.
    #[test]
    fn bitwise_ops_refuse_inexact_and_out_of_range_arguments() {
        for (name, args) in [
            ("band", vec![1.5, 1.0]),
            ("band", vec![f64::INFINITY, 1.0]),
            ("band", vec![f64::NAN, 1.0]),
            ("band", vec![9.007_199_254_740_993e15, 1.0]),
            ("bshl", vec![1.0, 64.0]),
            ("bshl", vec![1.0, -1.0]),
            // Bits shifted past the exactly-representable range.
            ("bshl", vec![1.0, 60.0]),
            // Bits shifted past i64 itself. These two are the regression: the
            // first implementation wrapped in i64 and then range-checked the
            // wrapped value, so 2^52 << 12 came back as a serene `0` and
            // (2^53 - 1) << 12 as `-4096`. Both are inside the exact range and
            // both are fabrications. Cold review round 32 measured them.
            ("bshl", vec![4_503_599_627_370_496.0, 12.0]),
            ("bshl", vec![9_007_199_254_740_991.0, 12.0]),
        ] {
            let err = call_builtin(name, args.iter().copied().map(Value::Number).collect())
                .expect_err("an inexact bitwise argument must raise");
            assert!(
                format!("{err}").contains(name),
                "{name}({args:?}) error should name the builtin: {err}"
            );
        }
    }

    /// The property `uid()` exists for: it must agree with what `stat()` reports
    /// for a file this process just created, because the whole point is to let a
    /// script decide "is this path mine?" WITHOUT creating that file. If these
    /// two ever disagree the builtin is worse than useless — callers would go
    /// back to the write-probe it replaced.
    ///
    /// What this does NOT prove is the EFFECTIVE part of the contract. File
    /// creation uses the filesystem uid, which follows the effective uid, so on
    /// an ordinary process — where real, effective and filesystem ids are all
    /// the same — a `getuid()` implementation would pass this test unchanged.
    /// Separating them needs a setuid binary or a `setresuid()` this test
    /// cannot perform on itself; the effective semantics are held by the
    /// `geteuid`/`getegid` call in the implementation and stated on the man
    /// page, not measured here. Named accordingly, so nobody reads more into a
    /// green tick than it earned.
    #[test]
    fn uid_and_gid_match_the_owner_of_a_file_this_process_creates() {
        let d = tmpdir("uid-agrees");
        let p = d.join("owned.txt");
        std::fs::write(&p, b"").unwrap();
        let m = map_of(builtin_stat(vec![s(&p)]).expect("stat ok"));

        assert_eq!(m.get("uid"), builtin_uid(vec![]).expect("uid ok").as_ref());
        assert_eq!(m.get("gid"), builtin_gid(vec![]).expect("gid ok").as_ref());
        // ...and they are plain non-negative integers, not a float artefact.
        match builtin_uid(vec![]).expect("uid ok") {
            Some(Value::Number(n)) => assert!(n >= 0.0 && n.fract() == 0.0, "uid={n}"),
            other => panic!("expected a number, got {other:?}"),
        }
        let _ = std::fs::remove_dir_all(&d);
    }

    /// `groups()` exists so a script can tell which permission CLASS the kernel
    /// will apply to someone else's file, which needs the supplementary set and
    /// not just `getegid()`. Two properties are load-bearing and both are
    /// checkable without root: the effective gid is always in the list (POSIX
    /// lets `getgroups()` omit it, so the builtin adds it), and every entry is a
    /// plain non-negative integer a caller can compare against a `stat()` map's
    /// `gid` with `==`. The *contents* beyond that are the test runner's
    /// identity and cannot be asserted.
    #[test]
    fn groups_always_contains_the_effective_gid() {
        let got = builtin_groups(vec![]).expect("groups ok").expect("a value");
        let list = match &got {
            Value::List(l) => l.clone(),
            other => panic!("expected a list, got {other:?}"),
        };
        let egid = builtin_gid(vec![]).expect("gid ok").expect("a value");
        assert!(
            list.contains(&egid),
            "groups()={list:?} does not contain gid()={egid:?}"
        );
        for g in list.iter() {
            match g {
                Value::Number(n) => assert!(n >= &0.0 && n.fract() == 0.0, "gid={n}"),
                other => panic!("expected a number, got {other:?}"),
            }
        }
    }

    /// The manual documents the result as a set containing the effective gid.
    /// Duplicates can only come from a process configured with the same gid
    /// twice, which no unit test can arrange without setgroups privileges — so
    /// this drives the normalisation half directly with an input the kernel here
    /// will not produce. Asserting the property through `groups()` instead was
    /// hollow: this workstation's group list is already unique, so deleting the
    /// `dedup()` left the test green.
    #[test]
    fn normalize_group_set_is_a_sorted_set_including_egid() {
        assert_eq!(normalize_group_set(vec![7, 7, 42, 42], 42), vec![7, 42]);
        assert_eq!(normalize_group_set(vec![9, 3, 9], 100), vec![3, 9, 100]);
        // The egid is added, never assumed present.
        assert_eq!(normalize_group_set(vec![], 5), vec![5]);
        // And adding it must not reintroduce a duplicate of itself.
        assert_eq!(normalize_group_set(vec![5], 5), vec![5]);
    }

    /// The whole-builtin end of the same property, which also pins that the
    /// syscall half feeds the normaliser rather than bypassing it.
    #[test]
    fn groups_returns_no_duplicates() {
        let got = builtin_groups(vec![]).expect("groups ok").expect("a value");
        let list = match &got {
            Value::List(l) => l.clone(),
            other => panic!("expected a list, got {other:?}"),
        };
        let mut seen: Vec<String> = list.iter().map(|g| g.to_mix_string()).collect();
        let before = seen.len();
        seen.sort();
        seen.dedup();
        assert_eq!(before, seen.len(), "groups()={list:?} contains a duplicate");
    }

    /// `{parents: false}` is a safety boundary, so every near-miss spelling has to
    /// raise rather than fall through to the recursive default. Both of these
    /// silently created parents before: the singular key was ignored outright, and
    /// the string "false" is truthy.
    #[test]
    fn mkdir_rejects_malformed_safety_options() {
        let d = tmpdir("mkdir-badopts");
        let leaf = format!("{}/missing-parent/leaf", d.display());

        let mut typo = indexmap::IndexMap::new();
        typo.insert("parent".to_string(), Value::Bool(false));
        let e = builtin_mkdir(vec![
            Value::String(leaf.clone()),
            Value::Map(std::rc::Rc::new(typo)),
        ])
        .expect_err("a misspelt option must raise");
        assert!(
            format!("{e:?}").contains("unknown option"),
            "wrong error: {e:?}"
        );

        let mut stringy = indexmap::IndexMap::new();
        stringy.insert("parents".to_string(), Value::String("false".to_string()));
        let e = builtin_mkdir(vec![
            Value::String(leaf.clone()),
            Value::Map(std::rc::Rc::new(stringy)),
        ])
        .expect_err("a non-boolean parents must raise");
        assert!(
            format!("{e:?}").contains("must be a boolean"),
            "wrong error: {e:?}"
        );

        // The point of raising is that nothing was created. If either call had
        // fallen through to create_dir_all, the parent would be on disk now.
        assert!(
            !std::path::Path::new(&format!("{}/missing-parent", d.display())).exists(),
            "a rejected option still created the parent"
        );
    }

    /// `mkdir` is `create_dir_all` by default, and that default hides the
    /// removal of a parent a script placed deliberately. `{parents: false}` must
    /// fail on a missing parent WITHOUT creating it — "did not create the leaf"
    /// is not the property; "did not create the parent either" is.
    #[test]
    fn mkdir_without_parents_refuses_and_creates_nothing() {
        let d = tmpdir("mkdir-parents");
        let parent = d.join("gone");
        let leaf = parent.join("leaf");

        let mut opts = indexmap::IndexMap::new();
        opts.insert("parents".to_string(), Value::Bool(false));
        let strict = Value::map(opts);

        let err = builtin_mkdir(vec![s(&leaf), strict.clone()]);
        assert!(err.is_err(), "expected a raise, got {err:?}");
        assert!(!parent.exists(), "the missing parent was created anyway");
        assert!(!leaf.exists(), "the leaf was created anyway");

        // ...and the default is still the recursive form, so nothing else that
        // calls mkdir() changed behaviour.
        builtin_mkdir(vec![s(&leaf)]).expect("recursive mkdir ok");
        assert!(leaf.is_dir(), "the default form did not create the tree");

        // An existing parent is the ordinary success path for the strict form.
        let sib = parent.join("sib");
        builtin_mkdir(vec![s(&sib), strict]).expect("strict mkdir under an existing parent ok");
        assert!(sib.is_dir(), "the strict form did not create its directory");

        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn stat_reports_known_file_metadata() {
        let d = tmpdir("stat-file");
        let p = d.join("f.txt");
        std::fs::write(&p, b"hello").unwrap();
        // Set an exact mode via builtin_chmod (set_permissions bypasses
        // umask, so perm is deterministic regardless of test env umask).
        builtin_chmod(vec![s(&p), Value::Number(0o640 as f64)]).expect("chmod ok");

        let m = map_of(builtin_stat(vec![s(&p)]).expect("stat ok"));
        assert_eq!(m.get("is_file"), Some(&Value::Bool(true)));
        assert_eq!(m.get("is_dir"), Some(&Value::Bool(false)));
        assert_eq!(m.get("is_symlink"), Some(&Value::Bool(false)));
        assert_eq!(m.get("size"), Some(&Value::Number(5.0)));
        assert_eq!(m.get("perm"), Some(&Value::Number(0o640 as f64)));
        // uid matches the running euid.
        let euid = unsafe { libc::geteuid() };
        assert_eq!(m.get("uid"), Some(&Value::Number(euid as f64)));
        // ino is a NON-EMPTY NUMERIC STRING (the f64-precision guard).
        match m.get("ino") {
            Some(Value::String(ino)) => {
                assert!(!ino.is_empty(), "ino empty");
                assert!(
                    ino.chars().all(|c| c.is_ascii_digit()),
                    "ino not numeric: {ino}"
                );
            }
            other => panic!("ino must be a string, got {other:?}"),
        }
        assert!(
            matches!(m.get("dev"), Some(Value::String(_))),
            "dev must be a string"
        );
        // ctime is a positive epoch second.
        match m.get("ctime") {
            Some(Value::Number(n)) => assert!(*n > 0.0, "ctime not positive: {n}"),
            other => panic!("ctime must be a number, got {other:?}"),
        }
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn stat_reports_subsecond_components_in_range() {
        let d = tmpdir("stat-nsec-range");
        let p = d.join("f.txt");
        std::fs::write(&p, b"hello").unwrap();

        let m = map_of(builtin_stat(vec![s(&p)]).expect("stat ok"));
        for key in ["ctime_nsec", "mtime_nsec", "atime_nsec"] {
            match m.get(key) {
                Some(Value::Number(n)) => {
                    // A tv_nsec is the sub-second remainder, never a whole
                    // timestamp — if this ever exceeded a second it would mean
                    // someone folded the seconds back in, which is the lossy
                    // shape the split exists to avoid.
                    assert!(
                        *n >= 0.0 && *n <= 999_999_999.0,
                        "{key} outside tv_nsec range: {n}"
                    );
                    assert_eq!(n.fract(), 0.0, "{key} not an integer: {n}");
                }
                other => panic!("{key} must be a number, got {other:?}"),
            }
        }

        // A range check alone would accept an implementation that hard-coded
        // zero, so compare against the same syscall's own answer. This is the
        // assertion that says the field carries a real tv_nsec.
        //
        // All three, not just the two that are easy to reason about: a check
        // that covered mtime and ctime would still pass an implementation that
        // lied about atime alone, and "the two interesting ones are verified"
        // is exactly the reasoning that leaves one field unguarded.
        use std::os::unix::fs::MetadataExt as _;
        let raw = std::fs::metadata(&p).unwrap();
        for (key, want) in [
            ("mtime_nsec", raw.mtime_nsec()),
            ("ctime_nsec", raw.ctime_nsec()),
            ("atime_nsec", raw.atime_nsec()),
        ] {
            assert_eq!(
                m.get(key),
                Some(&Value::Number(want as f64)),
                "{key} does not match MetadataExt::{key}"
            );
        }
        let _ = std::fs::remove_dir_all(&d);
    }

    /// Set a path's mtime to an exact `(seconds, nanoseconds)`, leaving atime
    /// alone. Deliberately not the wall clock: two ordinary writes are not
    /// guaranteed distinct timestamps (granularity is a filesystem property,
    /// not a POSIX promise), so a test that raced the clock could fail against
    /// a perfectly correct implementation.
    ///
    /// Returns whether the filesystem stored the *sub-second* part it was
    /// handed. A `false` means one specific thing — the filesystem is coarse —
    /// and the caller skips on it, so this must not also be how a broken
    /// `utimensat` reports itself.
    ///
    /// They are told apart by comparing against the timestamp the file had
    /// *before* the call, which is why it is read here rather than assumed.
    /// Checking only that the whole seconds match the request is not enough:
    /// the second call in a same-second pair asks for seconds the file already
    /// has, so a call that did nothing would satisfy that check — and that is
    /// the very call the test depends on. The honest question is whether the
    /// stored pair moved at all.
    fn set_mtime_exact(path: &std::path::Path, secs: i64, nsecs: i64) -> bool {
        use std::ffi::CString;
        use std::os::unix::fs::MetadataExt as _;
        let pre = std::fs::metadata(path).unwrap();
        let before = (pre.mtime(), pre.mtime_nsec());
        assert_ne!(
            before,
            (secs, nsecs),
            "the requested mtime is the one the file already has, so this call \
             cannot be observed and would prove nothing — pick another value"
        );
        let c = CString::new(path.as_os_str().as_encoded_bytes()).unwrap();
        let times = [
            libc::timespec {
                tv_sec: 0,
                tv_nsec: libc::UTIME_OMIT,
            },
            libc::timespec {
                tv_sec: secs as libc::time_t,
                tv_nsec: nsecs as libc::c_long,
            },
        ];
        let rc = unsafe { libc::utimensat(libc::AT_FDCWD, c.as_ptr(), times.as_ptr(), 0) };
        assert_eq!(
            rc,
            0,
            "utimensat failed: {}",
            std::io::Error::last_os_error()
        );
        let raw = std::fs::metadata(path).unwrap();
        assert_ne!(
            (raw.mtime(), raw.mtime_nsec()),
            before,
            "utimensat reported success but the stored mtime did not move at \
             all: it did nothing, which is not the same as a coarse filesystem \
             and must not be skipped over"
        );
        assert_eq!(
            raw.mtime(),
            secs,
            "utimensat moved the mtime but not to the requested whole seconds"
        );
        raw.mtime_nsec() == nsecs
    }

    #[test]
    fn stat_nsec_pair_sees_a_same_second_rewrite() {
        // The reason the field exists: replacing a file's bytes with *equal*
        // bytes inside one second leaves whole-second mtime identical, so a
        // content-and-mtime tamper check reads it as untouched. cmctl's
        // public-hygiene suite did exactly that to nine live git hooks and
        // stayed green. The (mtime, mtime_nsec) pair is what catches it.
        //
        // The two timestamps are set explicitly rather than raced for, so the
        // test measures what stat() reports and never what the clock happened
        // to do.
        let d = tmpdir("stat-nsec-rewrite");
        let p = d.join("hook");
        std::fs::write(&p, b"#!/bin/sh\nexit 0\n").unwrap();

        // Whole tenths of a second, not a digit pattern like 111_111_111.
        // Sub-second resolution varies — nanosecond, 100ns, microsecond,
        // millisecond — and a value only representable at full nanosecond
        // resolution would be rounded by a filesystem that nonetheless stores
        // perfectly usable sub-second times, so the skip below would fire on a
        // filesystem this test can actually run on. 100ms is exact at every one
        // of those resolutions; only genuine whole-second storage loses it,
        // which is precisely what the skip is meant to mean.
        const SECS: i64 = 1_700_000_000;
        if !set_mtime_exact(&p, SECS, 100_000_000) {
            // A filesystem that truncates to whole seconds cannot demonstrate
            // this at all, and no stat-based check is sound there. Say so
            // rather than pass on evidence that was never gathered.
            eprintln!("filesystem does not store sub-second mtimes; case not exercised");
            let _ = std::fs::remove_dir_all(&d);
            return;
        }
        let before = map_of(builtin_stat(vec![s(&p)]).expect("stat ok"));

        // Identical content, so no content check can see this write; same
        // whole second, so no mtime check can either.
        std::fs::write(&p, b"#!/bin/sh\nexit 0\n").unwrap();
        assert!(set_mtime_exact(&p, SECS, 200_000_000));
        let after = map_of(builtin_stat(vec![s(&p)]).expect("stat ok"));

        assert_eq!(
            before.get("size"),
            after.get("size"),
            "the rewrite was supposed to be byte-identical"
        );
        assert_eq!(
            before.get("mtime"),
            after.get("mtime"),
            "the rewrite was supposed to stay inside one second"
        );
        // The exact values, not merely "different": this is what refuses an
        // implementation that reports a plausible-looking constant.
        assert_eq!(
            before.get("mtime_nsec"),
            Some(&Value::Number(100_000_000.0)),
            "stat() did not report the mtime_nsec that was set"
        );
        assert_eq!(
            after.get("mtime_nsec"),
            Some(&Value::Number(200_000_000.0)),
            "stat() did not report the mtime_nsec that was set"
        );
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn stat_follow_vs_lstat_on_symlink() {
        let d = tmpdir("stat-link");
        let target = d.join("target.txt");
        let link = d.join("link");
        std::fs::write(&target, b"0123456789").unwrap(); // 10 bytes
        std::os::unix::fs::symlink(&target, &link).unwrap();

        // Default follows: the link resolves to the 10-byte regular file,
        // but is_symlink still flags the path.
        let followed = map_of(builtin_stat(vec![s(&link)]).expect("stat follow ok"));
        assert_eq!(followed.get("is_file"), Some(&Value::Bool(true)));
        assert_eq!(followed.get("is_symlink"), Some(&Value::Bool(true)));
        assert_eq!(followed.get("size"), Some(&Value::Number(10.0)));

        // No-follow (lstat): the link itself, not a regular file.
        let mut opts = indexmap::IndexMap::new();
        opts.insert("follow_symlinks".to_string(), Value::Bool(false));
        let linked =
            map_of(builtin_stat(vec![s(&link), Value::map(opts)]).expect("stat nofollow ok"));
        assert_eq!(linked.get("is_file"), Some(&Value::Bool(false)));
        assert_eq!(linked.get("is_symlink"), Some(&Value::Bool(true)));
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn stat_missing_path_errors() {
        let d = tmpdir("stat-missing");
        let err = builtin_stat(vec![s(&d.join("nope"))]).unwrap_err();
        assert!(format!("{err}").contains("stat"), "got: {err}");
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn chown_self_is_a_noop_that_succeeds() {
        // Chowning a file to its CURRENT uid/gid needs no privilege, so
        // this exercises the syscall path under a non-root test runner.
        let d = tmpdir("chown-self");
        let p = d.join("f");
        std::fs::write(&p, b"x").unwrap();
        let meta = std::fs::metadata(&p).unwrap();
        use std::os::unix::fs::MetadataExt;
        let r = builtin_chown(vec![
            s(&p),
            Value::Number(meta.uid() as f64),
            Value::Number(meta.gid() as f64),
        ])
        .expect("self-chown ok");
        assert_eq!(r, Some(Value::Nil));
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn chown_rejects_bad_ids() {
        let d = tmpdir("chown-bad");
        let p = d.join("f");
        std::fs::write(&p, b"x").unwrap();
        // Negative id.
        assert!(builtin_chown(vec![s(&p), Value::Number(-1.0), Value::Number(0.0)]).is_err());
        // Non-numeric id.
        assert!(
            builtin_chown(vec![
                s(&p),
                Value::String("root".into()),
                Value::Number(0.0)
            ])
            .is_err()
        );
        // Fractional id.
        assert!(builtin_chown(vec![s(&p), Value::Number(1.5), Value::Number(0.0)]).is_err());
        // Bool must NOT coerce to 1/0 — that was the footgun.
        assert!(builtin_chown(vec![s(&p), Value::Bool(true), Value::Number(0.0)]).is_err());
        // Above u32 range.
        assert!(
            builtin_chown(vec![
                s(&p),
                Value::Number(u32::MAX as f64 + 1.0),
                Value::Number(0.0)
            ])
            .is_err()
        );
        // Non-finite.
        assert!(builtin_chown(vec![s(&p), Value::Number(f64::NAN), Value::Number(0.0)]).is_err());
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn read_file_bytes_cap_limits_the_read() {
        let d = tmpdir("rfb-cap");
        let p = d.join("big");
        let payload: Vec<u8> = (0..100).collect();
        std::fs::write(&p, &payload).unwrap();

        // Cap below file size → exactly that many bytes.
        let v = builtin_read_file_bytes(vec![s(&p), Value::Number(8.0)]).unwrap();
        assert_eq!(v, Some(Value::bytes((0..8).collect())));
        // Cap of 0 → empty.
        let v0 = builtin_read_file_bytes(vec![s(&p), Value::Number(0.0)]).unwrap();
        assert_eq!(v0, Some(Value::bytes(vec![])));
        // Cap above file size → whole file.
        let vall = builtin_read_file_bytes(vec![s(&p), Value::Number(1000.0)]).unwrap();
        assert_eq!(vall, Some(Value::bytes(payload.clone())));
        // No cap → whole file (back-compat).
        let vnocap = builtin_read_file_bytes(vec![s(&p)]).unwrap();
        assert_eq!(vnocap, Some(Value::bytes(payload)));
        // Bad arity / bad max.
        assert!(
            builtin_read_file_bytes(vec![s(&p), Value::Number(1.0), Value::Number(2.0)]).is_err()
        );
        assert!(builtin_read_file_bytes(vec![s(&p), Value::Number(-1.0)]).is_err());
        assert!(builtin_read_file_bytes(vec![s(&p), Value::Number(1.5)]).is_err());
        // Bool/string max must NOT coerce.
        assert!(builtin_read_file_bytes(vec![s(&p), Value::Bool(true)]).is_err());
        assert!(builtin_read_file_bytes(vec![s(&p), Value::String("8".into())]).is_err());
        // Exactly 2^64 must be rejected (the `as u64` saturation edge),
        // not silently treated as an unbounded read.
        assert!(builtin_read_file_bytes(vec![s(&p), Value::Number(2.0_f64.powi(64))]).is_err());
        let _ = std::fs::remove_dir_all(&d);
    }
}

#[cfg(test)]
mod bytes_tests {
    use super::*;

    fn tmpfile(suffix: &str) -> std::path::PathBuf {
        let mut p = std::env::temp_dir();
        p.push(format!(
            "cosmix-mix-bytes-{}-{}",
            std::process::id(),
            suffix
        ));
        p
    }

    #[test]
    fn bytes_len_on_bytes() {
        let v = builtin_bytes_len(vec![Value::bytes(vec![0xFF, 0xD8, 0xFF, 0xE0])]).unwrap();
        assert_eq!(v, Some(Value::Number(4.0)));
    }

    #[test]
    fn bytes_len_rejects_string() {
        let err = builtin_bytes_len(vec![Value::String("hi".into())]).unwrap_err();
        let msg = format!("{err}");
        assert!(msg.contains("expected bytes"), "msg: {msg}");
        assert!(msg.contains("string"), "msg: {msg}");
    }

    // --- bytes as a sequence (v0.64.0, T1) ---

    fn b(v: &[u8]) -> Value {
        Value::bytes(v.to_vec())
    }

    fn as_bytes(v: Option<Value>) -> Vec<u8> {
        match &v {
            Some(Value::Bytes(x)) => x.to_vec(),
            other => panic!("expected bytes, got {other:?}"),
        }
    }

    #[test]
    fn len_counts_bytes_not_chars() {
        // "héllo" is 5 codepoints / 6 bytes: len() must answer 6 on the
        // bytes value and 5 on the string, or the two units have blurred.
        let raw = "héllo".as_bytes().to_vec();
        assert_eq!(raw.len(), 6);
        assert_eq!(
            builtin_len(vec![Value::bytes(raw)]).unwrap(),
            Some(Value::Number(6.0))
        );
        assert_eq!(
            builtin_len(vec![Value::String("héllo".into())]).unwrap(),
            Some(Value::Number(5.0))
        );
    }

    #[test]
    fn slice_on_bytes_clamps_and_reverses_empty() {
        let v = b(&[1, 2, 3, 4, 5]);
        assert_eq!(
            as_bytes(builtin_slice(vec![v.clone(), Value::Number(1.0), Value::Number(3.0)]).unwrap()),
            vec![2, 3]
        );
        // Omitted end runs to the end; negative indices count back.
        assert_eq!(
            as_bytes(builtin_slice(vec![v.clone(), Value::Number(-2.0)]).unwrap()),
            vec![4, 5]
        );
        // Out of range clamps rather than raising...
        assert_eq!(
            as_bytes(
                builtin_slice(vec![v.clone(), Value::Number(-100.0), Value::Number(100.0)]).unwrap()
            ),
            vec![1, 2, 3, 4, 5]
        );
        // ...and a reversed range is empty, not a panic and not a wrap.
        assert_eq!(
            as_bytes(builtin_slice(vec![v, Value::Number(4.0), Value::Number(1.0)]).unwrap()),
            Vec::<u8>::new()
        );
    }

    #[test]
    fn bytes_find_offsets_are_absolute() {
        let v = b(b"hello, world");
        // Plain find, string needle.
        assert_eq!(
            builtin_bytes_find(vec![v.clone(), Value::String("world".into())]).unwrap(),
            Some(Value::Number(7.0))
        );
        // Absent is -1, never nil.
        assert_eq!(
            builtin_bytes_find(vec![v.clone(), Value::String("zz".into())]).unwrap(),
            Some(Value::Number(-1.0))
        );
        // `from` skips the first 'l' at 2 and finds the one at 3 — the
        // returned index is into the WHOLE value, not the suffix.
        assert_eq!(
            builtin_bytes_find(vec![
                v.clone(),
                Value::String("l".into()),
                Value::Number(3.0)
            ])
            .unwrap(),
            Some(Value::Number(3.0))
        );
        assert_eq!(
            builtin_bytes_find(vec![v.clone(), Value::String("l".into()), Value::Number(4.0)])
                .unwrap(),
            Some(Value::Number(10.0))
        );
        // A single byte number is a legal needle (0x2C = ',').
        assert_eq!(
            builtin_bytes_find(vec![v, Value::Number(44.0)]).unwrap(),
            Some(Value::Number(5.0))
        );
    }

    #[test]
    fn bytes_find_rejects_string_subject() {
        // The whole point of the strict subject: a string would otherwise
        // be answered about as text, or worse, its `<bytes:N>` placeholder.
        let err =
            builtin_bytes_find(vec![Value::String("hello".into()), Value::String("l".into())])
                .unwrap_err();
        assert!(
            format!("{err}").contains("expected bytes or buffer"),
            "{err}"
        );
    }

    #[test]
    fn bytes_split_matches_string_split_piece_rules() {
        let pieces = |s: &str, sep: &str| -> Vec<Vec<u8>> {
            match &builtin_bytes_split(vec![b(s.as_bytes()), Value::String(sep.into())]).unwrap() {
                Some(Value::List(l)) => l
                    .iter()
                    .map(|v| match v {
                        Value::Bytes(x) => x.to_vec(),
                        other => panic!("piece is not bytes: {other:?}"),
                    })
                    .collect(),
                other => panic!("expected list, got {other:?}"),
            }
        };
        // Same shapes `split()` produces for the same inputs.
        assert_eq!(pieces("a,b,,c", ","), vec![b"a".to_vec(), b"b".to_vec(), vec![], b"c".to_vec()]);
        assert_eq!(pieces(",a,", ","), vec![vec![], b"a".to_vec(), vec![]]);
        assert_eq!(pieces("abc", "x"), vec![b"abc".to_vec()]);
        assert_eq!(pieces("", ","), vec![Vec::<u8>::new()]);
        // Overlapping-separator case: the scan resumes AFTER the match.
        assert_eq!(
            pieces("aaaa", "aa"),
            vec![Vec::<u8>::new(), Vec::new(), Vec::new()]
        );
        // Empty separator raises rather than emulating split's per-char form.
        let err = builtin_bytes_split(vec![b(b"abc"), Value::String(String::new())]).unwrap_err();
        assert!(format!("{err}").contains("must not be empty"), "{err}");
    }

    #[test]
    fn bytes_concat_and_from_build_the_same_value() {
        assert_eq!(
            as_bytes(
                builtin_bytes_concat(vec![
                    Value::String("ab".into()),
                    b(&[99]),
                    Value::Number(100.0)
                ])
                .unwrap()
            ),
            b"abcd".to_vec()
        );
        assert_eq!(
            as_bytes(
                builtin_bytes_from(vec![Value::list(vec![
                    Value::String("ab".into()),
                    b(&[99]),
                    Value::Number(100.0),
                ])])
                .unwrap()
            ),
            b"abcd".to_vec()
        );
        // A list handed to bytes_concat names the right builtin instead of
        // being stringified into "[1, 2]".
        let err = builtin_bytes_concat(vec![Value::list(vec![Value::Number(1.0)])]).unwrap_err();
        assert!(format!("{err}").contains("bytes_from"), "{err}");
    }

    #[test]
    fn hex_round_trip_is_exact_and_strict() {
        let raw: Vec<u8> = (0u8..=255).collect();
        let hex = match &builtin_bytes_to_hex(vec![Value::bytes(raw.clone())]).unwrap() {
            Some(Value::String(s)) => s.clone(),
            other => panic!("expected string, got {other:?}"),
        };
        assert_eq!(hex.len(), 512);
        assert!(hex.starts_with("000102"), "{hex}");
        assert!(hex.ends_with("fdfeff"), "{hex}");
        assert_eq!(
            as_bytes(builtin_bytes_from_hex(vec![Value::String(hex)]).unwrap()),
            raw
        );
        // Uppercase decodes; odd length and non-hex characters raise.
        assert_eq!(
            as_bytes(builtin_bytes_from_hex(vec![Value::String("DEADbeef".into())]).unwrap()),
            vec![0xDE, 0xAD, 0xBE, 0xEF]
        );
        let err = builtin_bytes_from_hex(vec![Value::String("abc".into())]).unwrap_err();
        assert!(format!("{err}").contains("even length"), "{err}");
        let err = builtin_bytes_from_hex(vec![Value::String("ab:cd0".into())]).unwrap_err();
        assert!(format!("{err}").contains("not a hex digit"), "{err}");
    }

    #[test]
    fn bytes_starts_with_prefix_forms() {
        let v = b(b"hello");
        for (prefix, want) in [
            (Value::String("hell".into()), true),
            (Value::String("world".into()), false),
            (Value::Number(104.0), true),
            (Value::Number(105.0), false),
            (b(&[]), true),
        ] {
            assert_eq!(
                builtin_bytes_starts_with(vec![v.clone(), prefix.clone()]).unwrap(),
                Some(Value::Bool(want)),
                "prefix {prefix:?}"
            );
        }
    }

    #[test]
    fn string_to_bytes_utf8() {
        let v = builtin_string_to_bytes(vec![Value::String("héllo".into())]).unwrap();
        // "héllo" is 6 UTF-8 bytes (é = 0xC3 0xA9).
        match &v {
            Some(Value::Bytes(b)) => {
                assert_eq!(b.as_slice(), b"h\xc3\xa9llo");
            }
            other => panic!("expected Bytes, got {:?}", other),
        }
    }

    #[test]
    fn bytes_to_string_roundtrip() {
        let bytes = builtin_string_to_bytes(vec![Value::String("hello".into())])
            .unwrap()
            .unwrap();
        let s = builtin_bytes_to_string(vec![bytes]).unwrap();
        assert_eq!(s, Some(Value::String("hello".into())));
    }

    #[test]
    fn string_to_bytes_rejects_non_string() {
        // The whole point of the strict variant: a Bytes argument must
        // not silently re-encode the placeholder `<bytes:N>`.
        let err = builtin_string_to_bytes(vec![Value::bytes(vec![0xFF, 0xD8])]).unwrap_err();
        let msg = format!("{err}");
        assert!(msg.contains("expected string"), "msg: {msg}");
        assert!(msg.contains("bytes"), "msg: {msg}");
        let err = builtin_string_to_bytes(vec![Value::Number(7.0)]).unwrap_err();
        let msg = format!("{err}");
        assert!(msg.contains("expected string"), "msg: {msg}");
        assert!(msg.contains("number"), "msg: {msg}");
    }

    #[test]
    fn bytes_to_string_rejects_non_utf8() {
        // 0xFF is never a valid UTF-8 start byte.
        let err = builtin_bytes_to_string(vec![Value::bytes(vec![0xFF, 0xFE])]).unwrap_err();
        let msg = format!("{err}");
        assert!(msg.contains("not valid UTF-8"), "msg: {msg}");
    }

    #[test]
    fn bytes_to_string_rejects_string_input() {
        let err = builtin_bytes_to_string(vec![Value::String("hi".into())]).unwrap_err();
        let msg = format!("{err}");
        assert!(msg.contains("expected bytes"), "msg: {msg}");
    }

    #[test]
    fn bytes_to_string_lossy_keeps_ascii_around_bad_byte() {
        // The real use case: an ASCII header sniffed out of a byte block
        // that has a stray non-UTF-8 byte elsewhere (raw email). Strict
        // decode rejects the whole buffer; lossy keeps the ASCII intact
        // and substitutes U+FFFD for the bad byte.
        let mut buf = b"X-Spam-Status: GOOD 0.494770\n".to_vec();
        buf.push(0xFF); // never a valid UTF-8 start byte
        buf.extend_from_slice(b"\nSubject: hi\n");

        // Strict still errors.
        assert!(builtin_bytes_to_string(vec![Value::bytes(buf.clone())]).is_err());

        // {lossy: true} decodes; the ASCII header survives verbatim.
        let mut opts = indexmap::IndexMap::new();
        opts.insert("lossy".into(), Value::Bool(true));
        let s = builtin_bytes_to_string(vec![Value::bytes(buf), Value::map(opts)])
            .unwrap()
            .unwrap();
        match &s {
            Value::String(s) => {
                assert!(s.contains("X-Spam-Status: GOOD 0.494770"), "s: {s}");
                assert!(s.contains('\u{FFFD}'), "expected replacement char, s: {s}");
            }
            other => panic!("expected String, got {other:?}"),
        }
    }

    #[test]
    fn bytes_to_string_nil_option_is_strict() {
        // An explicit nil second arg must NOT silently turn on lossy.
        let err =
            builtin_bytes_to_string(vec![Value::bytes(vec![0xFF, 0xFE]), Value::Nil]).unwrap_err();
        let msg = format!("{err}");
        assert!(msg.contains("not valid UTF-8"), "msg: {msg}");
    }

    #[test]
    #[cfg(feature = "crypto")]
    fn base64_decode_returns_bytes_preserving_high_bytes() {
        // JPEG/JFIF magic: FF D8 FF E0 — exactly the high-bit pattern
        // that the old `from_utf8_lossy` decode corrupted to U+FFFD.
        use base64::Engine;
        let encoded = base64::engine::general_purpose::STANDARD.encode([0xFF, 0xD8, 0xFF, 0xE0]);
        let v = builtin_base64_decode(vec![Value::String(encoded)]).unwrap();
        assert_eq!(v, Some(Value::bytes(vec![0xFF, 0xD8, 0xFF, 0xE0])));
    }

    #[test]
    fn write_file_writes_bytes_verbatim() {
        let p = tmpfile("write-bytes");
        let payload: Vec<u8> = (0u8..=0xFFu8).collect();
        builtin_write_file(vec![
            Value::String(p.to_string_lossy().into()),
            Value::bytes(payload.clone()),
        ])
        .unwrap();
        let actual = std::fs::read(&p).unwrap();
        assert_eq!(actual, payload);
        let _ = std::fs::remove_file(&p);
    }

    #[test]
    fn read_file_bytes_roundtrip_with_write_file_bytes() {
        let p = tmpfile("rt");
        // Include 0xFF/0xD8 — the JPEG magic that originally triggered
        // the corruption.
        let payload: Vec<u8> = vec![0xFF, 0xD8, 0xFF, 0xE0, 0x00, 0x10, 0x4A, 0x46, 0x49, 0x46];
        builtin_write_file(vec![
            Value::String(p.to_string_lossy().into()),
            Value::bytes(payload.clone()),
        ])
        .unwrap();
        let v = builtin_read_file_bytes(vec![Value::String(p.to_string_lossy().into())]).unwrap();
        assert_eq!(v, Some(Value::bytes(payload)));
        let _ = std::fs::remove_file(&p);
    }

    #[test]
    fn append_file_appends_bytes_verbatim() {
        let p = tmpfile("append-bytes");
        // Seed file with two known bytes via write_file, then append
        // two binary bytes via append_file, and confirm the on-disk
        // payload is exactly the concatenation.
        builtin_write_file(vec![
            Value::String(p.to_string_lossy().into()),
            Value::bytes(vec![0xAA, 0xBB]),
        ])
        .unwrap();
        builtin_append_file(vec![
            Value::String(p.to_string_lossy().into()),
            Value::bytes(vec![0xCC, 0xDD]),
        ])
        .unwrap();
        let actual = std::fs::read(&p).unwrap();
        assert_eq!(actual, vec![0xAA, 0xBB, 0xCC, 0xDD]);
        let _ = std::fs::remove_file(&p);
    }

    #[test]
    #[cfg(feature = "http")]
    fn http_body_into_map_utf8_response() {
        let mut map = indexmap::IndexMap::new();
        http_body_into_map(200, b"hello world".to_vec(), &mut map);
        assert_eq!(map.get("status"), Some(&Value::Number(200.0)));
        assert_eq!(map.get("body"), Some(&Value::String("hello world".into())));
        assert_eq!(
            map.get("bytes"),
            Some(&Value::bytes(b"hello world".to_vec()))
        );
    }

    #[test]
    #[cfg(feature = "http")]
    fn http_body_into_map_binary_response() {
        let mut map = indexmap::IndexMap::new();
        // JPEG SOI + APP0 marker — guaranteed not valid UTF-8.
        let jpeg: Vec<u8> = vec![0xFF, 0xD8, 0xFF, 0xE0, 0x00, 0x10];
        http_body_into_map(200, jpeg.clone(), &mut map);
        assert_eq!(map.get("status"), Some(&Value::Number(200.0)));
        assert_eq!(
            map.get("body"),
            Some(&Value::Nil),
            "body must be Nil on non-UTF-8 — was the lossy-decode regression reintroduced?"
        );
        assert_eq!(map.get("bytes"), Some(&Value::bytes(jpeg)));
    }

    #[cfg(feature = "http")]
    fn opts_map(pairs: &[(&str, Value)]) -> Value {
        let mut m = indexmap::IndexMap::new();
        for (k, v) in pairs {
            m.insert((*k).into(), v.clone());
        }
        Value::map(m)
    }

    #[test]
    #[cfg(feature = "http")]
    fn http_opts_default_verifies() {
        // No opts → default timeout, verification ON (insecure == false).
        let opts = parse_http_opts("http_get", None).unwrap();
        assert_eq!(opts.timeout_s, 30);
        assert!(!opts.insecure);
        assert!(opts.ca_agent.is_none());
        let opts = parse_http_opts(
            "http_get",
            Some(&opts_map(&[("timeout", Value::Number(5.0))])),
        )
        .unwrap();
        assert!(!opts.insecure);
    }

    #[test]
    #[cfg(feature = "http")]
    fn http_opts_ssl_verify_false_sets_insecure() {
        let opts = parse_http_opts(
            "http_get",
            Some(&opts_map(&[
                ("timeout", Value::Number(15.0)),
                ("ssl_verify", Value::Bool(false)),
            ])),
        )
        .unwrap();
        assert_eq!(opts.timeout_s, 15);
        assert!(opts.insecure, "ssl_verify:false must set insecure");
        // ssl_verify:true is the explicit form of the default.
        let opts = parse_http_opts(
            "http_get",
            Some(&opts_map(&[("ssl_verify", Value::Bool(true))])),
        )
        .unwrap();
        assert!(!opts.insecure);
    }

    #[test]
    #[cfg(feature = "http")]
    fn http_opts_rejects_bad_types_and_unknown_keys() {
        // Non-bool ssl_verify.
        let err = parse_http_opts(
            "http_get",
            Some(&opts_map(&[("ssl_verify", Value::String("yes".into()))])),
        )
        .unwrap_err();
        assert!(format!("{err}").contains("ssl_verify must be a boolean"));
        // Unknown opt.
        let err = parse_http_opts(
            "http_get",
            Some(&opts_map(&[("verify", Value::Bool(false))])),
        )
        .unwrap_err();
        assert!(format!("{err}").contains("unknown opt"));
    }

    #[test]
    #[cfg(feature = "http")]
    fn http_map_is_opts_distinguishes_opts_from_headers() {
        // Pure opts map → treated as opts.
        if let Value::Map(ref m) = opts_map(&[("ssl_verify", Value::Bool(false))]) {
            assert!(http_map_is_opts(m));
        }
        if let Value::Map(ref m) = opts_map(&[
            ("timeout", Value::Number(5.0)),
            ("ssl_verify", Value::Bool(true)),
        ]) {
            assert!(http_map_is_opts(m));
        }
        // A map carrying a real header name is headers, not opts.
        if let Value::Map(ref m) = opts_map(&[("Authorization", Value::String("Bearer x".into()))])
        {
            assert!(!http_map_is_opts(m));
        }
        // Empty map is not an opts map (it's headers-or-nothing).
        if let Value::Map(ref m) = opts_map(&[]) {
            assert!(!http_map_is_opts(m));
        }
    }

    #[test]
    fn bytes_truthy_and_type_name() {
        assert!(!Value::bytes(vec![]).is_truthy());
        assert!(Value::bytes(vec![0]).is_truthy());
        assert_eq!(Value::bytes(vec![]).type_name(), "bytes");
    }

    #[test]
    fn bytes_eq_is_bytewise() {
        assert_eq!(Value::bytes(vec![1, 2, 3]), Value::bytes(vec![1, 2, 3]));
        assert_ne!(Value::bytes(vec![1, 2, 3]), Value::bytes(vec![1, 2, 4]));
        // No silent coercion to/from String even when the bytes happen
        // to be valid UTF-8 — callers must `bytes_to_string` explicitly.
        assert_ne!(Value::bytes(b"hi".to_vec()), Value::String("hi".into()));
    }

    #[test]
    fn bytes_strict_data_is_rejected() {
        // Strict-data has no syntax for bytes (yet). The serializer
        // refuses rather than emitting an unparseable form.
        let err = Value::bytes(vec![1, 2, 3])
            .to_mix_data_string()
            .unwrap_err();
        let msg = format!("{err}");
        assert!(msg.contains("bytes value"), "msg: {msg}");
        assert!(msg.contains("strict-data"), "msg: {msg}");
    }
}

#[cfg(test)]
mod data_serde_tests {
    use super::*;
    use indexmap::IndexMap;

    fn encode(v: Value) -> String {
        match &call_builtin("data_encode", vec![v]).expect("data_encode err") {
            Some(Value::String(s)) => s.clone(),
            other => panic!("data_encode: expected string, got {other:?}"),
        }
    }

    fn parse(s: &str) -> Value {
        call_builtin("data_parse", vec![Value::String(s.into())])
            .expect("data_parse err")
            .expect("data_parse returned None")
    }

    // `Value`'s PartialEq has no Map/List arm (`_ => false`), so map
    // round-trips are checked by destructuring inner String values and
    // by encode-idempotency (string equality), never `assert_eq!` on maps.
    #[test]
    fn round_trips_regex_with_dollar_and_backslash() {
        // The maild-rules case the writer was built for: a regex ending
        // in `$` (an interpolation sigil in strict-data → must emit
        // `\$`) alongside a literal backslash escape (`\d`, whose single
        // backslash is doubled to `\\d` and collapses back on re-parse).
        let mut m = IndexMap::new();
        m.insert(
            "all_uppercase_subject".to_string(),
            Value::String("^[^a-z]*$".into()),
        );
        m.insert("digits".to_string(), Value::String(r"^\d+$".into()));
        let original = Value::map(m);

        let text = encode(original.clone());
        // `$` escaped so the lexer won't treat it as interpolation; the
        // backslash doubled. These are exactly the hand-escaping rules a
        // script no longer has to get right by itself.
        assert!(text.contains(r"\$"), "expected escaped dollar in: {text}");
        assert!(
            text.contains(r"\\d"),
            "expected doubled backslash in: {text}"
        );
        assert!(
            !text.contains(r"\\$"),
            "the trailing $ must be \\$ (escaped dollar), not \\\\$ (escaped backslash): {text}"
        );

        // Destructure the parsed map: inner String == String works.
        match &parse(&text) {
            Value::Map(back) => {
                assert_eq!(
                    back.get("all_uppercase_subject"),
                    Some(&Value::String("^[^a-z]*$".into()))
                );
                assert_eq!(back.get("digits"), Some(&Value::String(r"^\d+$".into())));
            }
            other => panic!("expected map, got {other:?}"),
        }

        // Encode is idempotent on the re-parsed tree (byte-identical):
        // a strong round-trip assertion that sidesteps Map PartialEq.
        assert_eq!(encode(parse(&text)), text);
    }

    #[test]
    fn data_parse_accepts_literal_heredoc_and_rejects_interpolation() {
        let v = parse("note: <<E\nhello\n\nE\n");
        let Value::Map(m) = &v else {
            panic!("data_parse should return a map");
        };
        assert_eq!(m["note"], Value::String("hello\n".into()));

        let err = call_builtin(
            "data_parse",
            vec![Value::String("note: <<E\n${name}\nE\n".into())],
        )
        .unwrap_err();
        assert!(
            err.to_string().contains("string interpolation"),
            "interpolating heredoc must remain invalid strict-data: {err}"
        );
    }

    #[test]
    fn round_trips_tilde_and_newline() {
        // Leading `~/` (HOME-expansion sigil → `\~`) and an embedded
        // newline (`\n`) are the other two escapes a hand-writer trips on.
        let mut m = IndexMap::new();
        m.insert("home".to_string(), Value::String("~/mail".into()));
        m.insert("multi".to_string(), Value::String("a\nb".into()));
        let text = encode(Value::map(m));
        assert!(text.contains(r"\~"), "expected escaped tilde in: {text}");
        match &parse(&text) {
            Value::Map(back) => {
                assert_eq!(back.get("home"), Some(&Value::String("~/mail".into())));
                assert_eq!(back.get("multi"), Some(&Value::String("a\nb".into())));
            }
            other => panic!("expected map, got {other:?}"),
        }
    }

    #[test]
    fn encode_rejects_bytes() {
        // No strict-data form for bytes — surfaced through the builtin's
        // `data_encode:` prefix rather than emitting unparseable text.
        let err = call_builtin("data_encode", vec![Value::bytes(vec![1, 2, 3])]).unwrap_err();
        let msg = format!("{err}");
        assert!(msg.contains("data_encode"), "msg: {msg}");
        assert!(msg.contains("strict-data"), "msg: {msg}");
    }

    #[test]
    fn parse_rejects_garbage() {
        let err =
            call_builtin("data_parse", vec![Value::String("{ not valid".into())]).unwrap_err();
        let msg = format!("{err}");
        assert!(msg.contains("data_parse"), "msg: {msg}");
    }

    #[test]
    fn arity_enforced() {
        assert!(call_builtin("data_encode", vec![]).is_err());
        assert!(call_builtin("data_parse", vec![]).is_err());
    }

    #[test]
    fn both_names_are_registered_builtins() {
        // Guards the three-site registration (table / dispatch /
        // is_builtin): a name absent from is_builtin is never probed.
        assert!(is_builtin("data_encode"));
        assert!(is_builtin("data_parse"));
    }

    fn encode_pretty(v: Value) -> String {
        match &call_builtin("data_encode", vec![v, Value::Bool(true)]).expect("data_encode err") {
            Some(Value::String(s)) => s.clone(),
            other => panic!("data_encode(pretty): expected string, got {other:?}"),
        }
    }

    #[test]
    fn pretty_emits_multiline_indented_and_round_trips() {
        // Nested map → one entry per line, 2-space indent per depth, and
        // the same escaping as compact (the regex `$` is still `\$`).
        let mut server = IndexMap::new();
        server.insert("port".to_string(), Value::Number(25.0));
        server.insert("re".to_string(), Value::String("^[^a-z]*$".into()));
        let mut top = IndexMap::new();
        top.insert("server".to_string(), Value::map(server));
        let original = Value::map(top);

        let text = encode_pretty(original.clone());
        assert!(text.contains('\n'), "expected multi-line output: {text:?}");
        // depth-1 key at 2 spaces, depth-2 keys at 4 spaces.
        assert!(
            text.contains("\n  \"server\""),
            "depth-1 indent wrong: {text:?}"
        );
        assert!(
            text.contains("\n    \"port\""),
            "depth-2 indent wrong: {text:?}"
        );
        // Escaping is mode-independent.
        assert!(text.contains(r"\$"), "expected escaped dollar in: {text:?}");

        // Pretty layout parses back to the same tree (whitespace is
        // insignificant to the strict-data parser).
        match &parse(&text) {
            Value::Map(back) => match back.get("server") {
                Some(Value::Map(s)) => {
                    assert_eq!(s.get("port"), Some(&Value::Number(25.0)));
                    assert_eq!(s.get("re"), Some(&Value::String("^[^a-z]*$".into())));
                }
                other => panic!("expected nested map, got {other:?}"),
            },
            other => panic!("expected map, got {other:?}"),
        }

        // Idempotent in pretty mode too.
        assert_eq!(encode_pretty(parse(&text)), text);
    }

    #[test]
    fn pretty_and_compact_agree_on_value() {
        // The two layouts differ only in whitespace: re-encoding either
        // parse compactly yields identical text.
        let mut m = IndexMap::new();
        m.insert("a".to_string(), Value::String(r"^\d+$".into()));
        m.insert(
            "b".to_string(),
            Value::list(vec![Value::Number(1.0), Value::Number(2.0)]),
        );
        let original = Value::map(m);

        let compact = encode(original.clone());
        let pretty = encode_pretty(original);
        assert_ne!(compact, pretty, "pretty must differ from compact");
        assert_eq!(
            encode(parse(&pretty)),
            compact,
            "pretty must parse to the same value"
        );
    }

    #[test]
    fn pretty_empty_collections_stay_compact() {
        // No dangling `{\n}` / `[\n]` for empties.
        assert_eq!(encode_pretty(Value::map(IndexMap::new())), "{}");
        assert_eq!(encode_pretty(Value::list(vec![])), "[]");
    }

    #[test]
    fn compact_default_is_unchanged_byte_for_byte() {
        // The pretty refactor must not perturb the default (pretty=false)
        // output — explicit single-line guard.
        let mut m = IndexMap::new();
        m.insert("x".to_string(), Value::Number(1.0));
        m.insert(
            "y".to_string(),
            Value::list(vec![Value::Bool(true), Value::Nil]),
        );
        assert_eq!(encode(Value::map(m)), r#"{"x": 1, "y": [true, nil]}"#);
    }
}

#[cfg(test)]
mod char_aware_tests {
    // P0 of _doc/planned/2026-06-02-mix-char-aware-strings.md. The plan's
    // "verified count table" is ground truth; THIS module is that verification,
    // run against the linked unicode-segmentation + unicode-width versions. If a
    // cell here disagrees with the doc, the crate wins — fix the doc.
    use super::call_builtin;
    use crate::value::Value;

    // The six rows of the count table. Decomposed é = "e" + COMBINING ACUTE.
    const CAFE: &str = "café"; // precomposed é (U+00E9)
    const E_DECOMP: &str = "e\u{0301}"; // e + U+0301 combining acute
    const FAMILY: &str = "👨‍👩‍👧"; // man ZWJ woman ZWJ girl
    const FLAG: &str = "🇦🇺"; // regional indicators A + U
    const SKIN: &str = "👍🏽"; // thumbs-up + medium skin tone
    const CJK: &str = "日本語";

    fn s(x: &str) -> Value {
        Value::String(x.into())
    }
    fn n(x: f64) -> Value {
        Value::Number(x)
    }

    fn num(name: &str, args: Vec<Value>) -> f64 {
        match call_builtin(name, args).expect("builtin call ok") {
            Some(Value::Number(v)) => v,
            other => panic!("{name} returned {other:?}, expected a number"),
        }
    }
    fn text(name: &str, args: Vec<Value>) -> String {
        match &call_builtin(name, args).expect("builtin call ok") {
            Some(Value::String(v)) => v.clone(),
            other => panic!("{name} returned {other:?}, expected a string"),
        }
    }
    fn num1(name: &str, x: &str) -> f64 {
        num(name, vec![s(x)])
    }

    #[test]
    fn byte_length_matches_table() {
        assert_eq!(num1("byte_length", CAFE), 5.0);
        assert_eq!(num1("byte_length", E_DECOMP), 3.0);
        assert_eq!(num1("byte_length", FAMILY), 18.0);
        assert_eq!(num1("byte_length", FLAG), 8.0);
        assert_eq!(num1("byte_length", SKIN), 8.0);
        assert_eq!(num1("byte_length", CJK), 9.0);
    }

    #[test]
    fn grapheme_count_matches_table() {
        assert_eq!(num1("grapheme_count", CAFE), 4.0);
        assert_eq!(num1("grapheme_count", E_DECOMP), 1.0);
        assert_eq!(num1("grapheme_count", FAMILY), 1.0);
        assert_eq!(num1("grapheme_count", FLAG), 1.0);
        assert_eq!(num1("grapheme_count", SKIN), 1.0);
        assert_eq!(num1("grapheme_count", CJK), 3.0);
    }

    #[test]
    fn display_width_matches_table() {
        assert_eq!(num1("display_width", CAFE), 4.0);
        assert_eq!(num1("display_width", E_DECOMP), 1.0);
        assert_eq!(num1("display_width", FAMILY), 2.0);
        assert_eq!(num1("display_width", FLAG), 2.0);
        assert_eq!(num1("display_width", SKIN), 2.0);
        assert_eq!(num1("display_width", CJK), 6.0);
    }

    /// P1: `length` (string) is now the CODEPOINT count — the "P1 length" column
    /// of the plan's table. (`byte_length` keeps the byte column, asserted above.)
    #[test]
    fn length_is_codepoints() {
        assert_eq!(num1("length", CAFE), 4.0); // c a f é
        assert_eq!(num1("length", E_DECOMP), 2.0); // e + combining acute
        assert_eq!(num1("length", FAMILY), 5.0); // man ZWJ woman ZWJ girl
        assert_eq!(num1("length", FLAG), 2.0); // 2 regional indicators
        assert_eq!(num1("length", SKIN), 2.0); // thumb + skin modifier
        assert_eq!(num1("length", CJK), 3.0); // 日 本 語
        // length != byte_length exactly where the bytes are multibyte.
        assert_ne!(num1("length", CAFE), num1("byte_length", CAFE));
        // List length is UNCHANGED (element count, not codepoints).
        assert_eq!(
            num("length", vec![Value::list(vec![n(1.0), n(2.0), n(3.0)])]),
            3.0
        );
    }

    /// P1: `pos`/`lastpos`/`index_of` (string) now return CODEPOINT offsets;
    /// the `byte_*` twins keep BYTE offsets. They diverge whenever a multibyte
    /// char precedes the needle — "日本語" is the discriminating case.
    #[test]
    fn pos_family_is_codepoints_byte_twins_are_bytes() {
        // 日本語: 語 is codepoint 3 (1-based) but byte 7 (each CJK char = 3 bytes).
        assert_eq!(num("pos", vec![s("語"), s(CJK)]), 3.0);
        assert_eq!(num("byte_pos", vec![s("語"), s(CJK)]), 7.0);
        assert_eq!(num("index_of", vec![s(CJK), s("語")]), 2.0); // 0-based codepoint
        assert_eq!(num("byte_index_of", vec![s(CJK), s("語")]), 6.0); // 0-based byte
        // café: only ASCII precedes é, so codepoint == byte here (both 4 / 3).
        assert_eq!(num("pos", vec![s("é"), s(CAFE)]), 4.0);
        assert_eq!(num("byte_pos", vec![s("é"), s(CAFE)]), 4.0);
        // The composition that P1 fixes: substr from a string index_of no longer
        // corrupts on a multibyte prefix. substr(CJK, index_of(CJK,"語"), 1) == "語".
        let idx = num("index_of", vec![s(CJK), s("語")]);
        assert_eq!(text("substr", vec![s(CJK), n(idx), n(1.0)]), "語");
    }

    /// P1 edge cases from the plan (verified): empty needle, not-found sentinels,
    /// and lastpos on ASCII.
    #[test]
    fn pos_edge_cases() {
        assert_eq!(num("pos", vec![s(""), s("abc")]), 1.0); // empty needle → 1
        assert_eq!(num("pos", vec![s("x"), s("abc")]), 0.0); // not found → 0
        assert_eq!(num("lastpos", vec![s("a"), s("banana")]), 6.0); // ASCII
        assert_eq!(num("byte_lastpos", vec![s("a"), s("banana")]), 6.0);
        assert_eq!(num("index_of", vec![s("abc"), s("z")]), -1.0); // not found → -1
        // lastpos on a multibyte string: 本 is the 2nd codepoint (byte 3).
        assert_eq!(num("lastpos", vec![s("本"), s(CJK)]), 2.0);
        assert_eq!(num("byte_lastpos", vec![s("本"), s(CJK)]), 4.0);
    }

    /// P1: word_wrap's budget is codepoints, not bytes — a line of N multibyte
    /// chars fits a width-N wrap (byte budget would have split it early).
    #[test]
    fn word_wrap_budget_is_codepoints() {
        // Six CJK words (1 codepoint / 3 bytes each); width 11 fits all six on
        // one line by codepoints (6 words + 5 spaces = 11) — bytes would be 23.
        let words = "日 本 語 中 文 字";
        assert_eq!(text("word_wrap", vec![s(words), n(11.0)]), words);
        // width 5 forces a wrap after 3 words (3 + 2 spaces = 5).
        assert_eq!(
            text("word_wrap", vec![s(words), n(5.0)]),
            "日 本 語\n中 文 字"
        );
    }

    /// P2: lpad_w/rpad_w pad to display CELLS — a CJK/emoji column lines up,
    /// where the codepoint-counting lpad/rpad fall one cell short per wide char.
    #[test]
    fn width_padding_uses_display_cells() {
        // 日本 = 2 codepoints / 4 cells. Pad to 6 cells → exactly 2 spaces.
        assert_eq!(text("rpad_w", vec![s("日本"), n(6.0)]), "日本  ");
        assert_eq!(text("lpad_w", vec![s("日本"), n(6.0)]), "  日本");
        // The misalignment _w fixes: rpad counts 2 codepoints → 4 spaces (8 cells).
        assert_eq!(text("rpad", vec![s("日本"), n(6.0)]), "日本    ");
        // ASCII: cells == codepoints, so _w matches the plain version exactly.
        assert_eq!(
            text("rpad_w", vec![s("ab"), n(5.0)]),
            text("rpad", vec![s("ab"), n(5.0)])
        );
        assert_eq!(text("lpad_w", vec![s("ab"), n(5.0)]), "   ab");
        // Emoji: 👍🏽 = 2 cells → pad to 4 = 2 spaces (a plain rpad would add 2 for
        // the 2 codepoints too here, but rpad_w is correct by construction).
        assert_eq!(text("rpad_w", vec![s("👍🏽"), n(4.0)]), "👍🏽  ");
        // Saturating: content already wider than target → returned unchanged.
        assert_eq!(text("rpad_w", vec![s("日本語"), n(2.0)]), "日本語");
        assert_eq!(text("lpad_w", vec![s("日本語"), n(2.0)]), "日本語");
    }

    /// P2: word_wrap_w budgets by display cells (CJK glyph = 2), so it wraps
    /// sooner than the codepoint-budget word_wrap on the same width.
    #[test]
    fn word_wrap_w_budget_is_display_cells() {
        let words = "日 本 語 中";
        // cells, width 6: 日(2)+sp+本(2)=5 fits; +語 would be 8>6 → wrap.
        assert_eq!(text("word_wrap_w", vec![s(words), n(6.0)]), "日 本\n語 中");
        // codepoints, width 6: 日 本 語 = 5 fits; +中 would be 7>6 → wrap later.
        assert_eq!(text("word_wrap", vec![s(words), n(6.0)]), "日 本 語\n中");
        // Regression (cold-review MAJOR): a zero-DISPLAY-WIDTH first word (a lone
        // combining mark, width 0) must NOT drop the separator before the next
        // word — emptiness, not width==0, is the "line has content" sentinel.
        assert_eq!(
            text("word_wrap_w", vec![s("\u{0301} a"), n(10.0)]),
            "\u{0301} a"
        );
    }

    #[test]
    fn grapheme_substr_keeps_clusters_whole() {
        // The whole family emoji is grapheme 0.
        assert_eq!(
            text("grapheme_substr", vec![s(FAMILY), n(0.0), n(1.0)]),
            FAMILY
        );
        assert_eq!(
            text("grapheme_substr", vec![s(CAFE), n(0.0), n(3.0)]),
            "caf"
        );
        // start past end → empty; missing len → to end.
        assert_eq!(text("grapheme_substr", vec![s(CAFE), n(99.0), n(2.0)]), "");
        assert_eq!(text("grapheme_substr", vec![s(CAFE), n(3.0)]), "é");
    }

    #[test]
    fn grapheme_reverse_does_not_split_emoji() {
        // A char-level reverse would float the skin-tone modifier off the thumb;
        // grapheme_reverse keeps "👍🏽" intact.
        assert_eq!(text("grapheme_reverse", vec![s("a👍🏽b")]), "b👍🏽a");
        assert_eq!(text("grapheme_reverse", vec![s(CAFE)]), "éfac");
    }

    #[test]
    fn load_data_parses_strict_data_and_rejects_executable_form() {
        let dir = std::env::temp_dir();

        // Happy path: a strict-data file (comma-separated entries, comments ok).
        let ok = dir.join("mix_load_data_ok.mix");
        std::fs::write(
            &ok,
            "mesh: {\n  -- a comment\n  name: \"bus\",\n  epoch: 7,\n  \
             members: [ { name: \"alpha\", bus: true } ]\n}\n",
        )
        .unwrap();
        let v = call_builtin("load_data", vec![s(ok.to_str().unwrap())])
            .expect("load_data ok")
            .expect("returns a value");
        let Value::Map(top) = &v else {
            panic!("load_data should return a map");
        };
        let Value::Map(mesh) = &top["mesh"] else {
            panic!("mesh is a map");
        };
        assert_eq!(mesh["name"], Value::String("bus".into()));
        assert_eq!(mesh["epoch"], Value::Number(7.0));

        // Literal heredocs are strict-data strings (legacy config
        // compatibility); interpolation keeps the file executable-shaped
        // and must remain rejected.
        let heredoc = dir.join("mix_load_data_heredoc.mix");
        std::fs::write(&heredoc, "note: <<E\nhello\n\nE\n").unwrap();
        let v = call_builtin("load_data", vec![s(heredoc.to_str().unwrap())])
            .expect("load_data literal heredoc")
            .expect("returns a value");
        let Value::Map(top) = &v else {
            panic!("load_data should return a map");
        };
        assert_eq!(top["note"], Value::String("hello\n".into()));

        let interpolating = dir.join("mix_load_data_interpolating_heredoc.mix");
        std::fs::write(&interpolating, "note: <<E\n${name}\nE\n").unwrap();
        let err = call_builtin("load_data", vec![s(interpolating.to_str().unwrap())]).unwrap_err();
        assert!(
            err.to_string().contains("string interpolation"),
            "interpolating heredoc must remain invalid strict-data: {err}"
        );

        // Missing file → a catchable error, never a panic.
        assert!(call_builtin("load_data", vec![s("/no/such/inventory.mix")]).is_err());

        // The executable form ($x = ...) MUST be rejected, never executed —
        // this is the safety property that makes load_data the inert-data twin
        // of source/include.
        let exec = dir.join("mix_load_data_exec.mix");
        std::fs::write(&exec, "$mesh = { a: 1 }\n").unwrap();
        let err = call_builtin("load_data", vec![s(exec.to_str().unwrap())]).unwrap_err();
        let msg = err.to_string().to_lowercase();
        assert!(
            msg.contains("strict-data") || msg.contains("non-identifier"),
            "executable form must be a strict-data violation, got: {err}"
        );

        let _ = std::fs::remove_file(&ok);
        let _ = std::fs::remove_file(&heredoc);
        let _ = std::fs::remove_file(&interpolating);
        let _ = std::fs::remove_file(&exec);
    }

    /// Every way `load_data` can fail must be CATCHABLE. The evaluator's
    /// `try/catch` catches DieError | RuntimeError | Structured and nothing
    /// else, so a bare LexerError/ParseError/StrictDataViolation escaping
    /// this builtin walks straight past a `catch` and exits the interpreter
    /// — with status 1, which callers with an exit contract reserve for
    /// something else entirely. `is_err()` is not the property under test;
    /// the VARIANT is.
    #[test]
    fn load_data_failures_are_all_catchable() {
        let dir = std::env::temp_dir();
        use crate::MixError;
        let catchable = |e: &MixError| {
            matches!(
                e,
                MixError::RuntimeError { .. } | MixError::DieError { .. } | MixError::Structured(_)
            )
        };

        // Missing: was already catchable; pinned so it stays that way.
        let err = call_builtin("load_data", vec![s("/no/such/inventory.mix")]).unwrap_err();
        assert!(
            catchable(&err),
            "missing file must be catchable, got: {err:?}"
        );

        // Malformed: a syntax error in the DATA, which is not a syntax error
        // in the script and must not be treated as one.
        let bad = dir.join("mix_load_data_catchable_bad.mix");
        std::fs::write(&bad, "this is not conf.mix\n").unwrap();
        let err = call_builtin("load_data", vec![s(bad.to_str().unwrap())]).unwrap_err();
        assert!(
            catchable(&err),
            "parse failure must be catchable, got: {err:?}"
        );
        assert!(
            err.to_string().contains(bad.to_str().unwrap()),
            "the message must name the file that failed, got: {err}"
        );

        // Executable form: rejected as before, but reachable by a handler.
        let exec = dir.join("mix_load_data_catchable_exec.mix");
        std::fs::write(&exec, "$mesh = { a: 1 }\n").unwrap();
        let err = call_builtin("load_data", vec![s(exec.to_str().unwrap())]).unwrap_err();
        assert!(
            catchable(&err),
            "strict-data violation must be catchable, got: {err:?}"
        );
        assert!(
            err.to_string().to_lowercase().contains("strict-data"),
            "wrapping must not lose why it failed, got: {err}"
        );

        let _ = std::fs::remove_file(&bad);
        let _ = std::fs::remove_file(&exec);
    }

    /// The 8 P0 names must pass the evaluator's `is_builtin` gate — `call_builtin`
    /// alone (what the other tests use) bypasses it, so a registry/dispatch entry
    /// with no matching `is_builtin` arm yields "undefined function" at runtime.
    #[test]
    fn p0_names_pass_the_is_builtin_gate() {
        for name in [
            "byte_length",
            "byte_pos",
            "byte_lastpos",
            "byte_index_of",
            "grapheme_count",
            "grapheme_substr",
            "grapheme_reverse",
            "display_width",
        ] {
            assert!(super::is_builtin(name), "{name} missing from is_builtin()");
        }
    }

    /// Historical class guard against parallel-list drift. `is_builtin()` used
    /// to be a SECOND hand-maintained `matches!` list that silently drifted from
    /// the registry (the P0 trap: builtin compiles, unit-tests green via
    /// `call_builtin`, dies with "undefined function" at runtime). Since 0.29.0
    /// the gate is GENERATED from `BUILTIN_NAMES` minus `EVAL_SPECIAL_BUILTINS`,
    /// so the registry half holds by construction; what this still guards is the
    /// EVAL_SPECIAL partition — every special name must be a registry name that
    /// the gate deliberately excludes (its dispatch lives in `evaluator.rs`
    /// `if name == …` arms, never `call_builtin`).
    ///
    /// SCOPE (per the cold review): this does NOT prove a `call_builtin`
    /// dispatch arm exists, nor that a listed evaluator-special branch still
    /// survives in `evaluator.rs`. P0 pure-builtin dispatch is covered by the
    /// per-builtin `call_builtin` tests above.
    #[test]
    fn every_registry_name_passes_is_builtin_gate() {
        for &name in super::BUILTIN_NAMES {
            assert!(
                super::is_builtin(name) || super::EVAL_SPECIAL_BUILTINS.contains(&name),
                "'{name}' is in BUILTIN_NAMES but neither is_builtin() nor an \
                 evaluator-special — the evaluator would reject it at runtime"
            );
        }
        for &name in super::EVAL_SPECIAL_BUILTINS {
            assert!(
                super::BUILTIN_NAMES.contains(&name),
                "'{name}' is EVAL_SPECIAL but not in the registry — it would be \
                 undiscoverable"
            );
            assert!(
                !super::is_builtin(name),
                "'{name}' is EVAL_SPECIAL but passes is_builtin() — call_builtin \
                 would receive a name it has no arm for"
            );
        }
    }

    /// A weak digest must SAY it is weak, in the description a user reads.
    ///
    /// `mix what hash_md5` and `docs/mix/builtins.md` are generated from the
    /// registry description, so that string is where a caller meets the
    /// algorithm. Tying it to `DigestAlgo::is_broken` means a future weak
    /// algorithm cannot be added with a neutral one-liner: the test fails
    /// until the warning is there. The converse arm matters too — labelling
    /// SHA-256 "broken" would be its own kind of wrong.
    #[cfg(feature = "crypto")]
    #[test]
    fn broken_digests_carry_the_warning() {
        for &algo in super::DigestAlgo::ALL {
            let name = format!("hash_{}", algo.name());
            let name = name.as_str();
            let info = super::builtin_info_of(name)
                .unwrap_or_else(|| panic!("'{name}' is not in the registry"));
            let desc = info.description;
            if algo.is_broken() {
                assert!(
                    desc.contains("BROKEN"),
                    "'{name}' is a broken digest but its description does not say so: {desc}"
                );
                assert!(
                    desc.contains("hash_sha256") || desc.contains("hash_blake3"),
                    "'{name}' warns but names no safe alternative: {desc}"
                );
            } else {
                assert!(
                    !desc.contains("BROKEN"),
                    "'{name}' is not a broken digest but its description says it is: {desc}"
                );
            }
        }
        // hash_file reaches the same set, so its error message must list all
        // of them — a name accepted but unlisted is undiscoverable.
        let file_desc = super::builtin_info_of("hash_file").unwrap().description;
        for &algo in super::DigestAlgo::ALL {
            let spelling = algo.name();
            assert!(
                file_desc.contains(spelling),
                "hash_file accepts '{spelling}' but does not list it: {file_desc}"
            );
            assert!(
                super::DigestAlgo::from_name(spelling).is_some(),
                "'{spelling}' must round-trip through from_name"
            );
        }
        // The error message a caller sees on a typo is DERIVED from the enum,
        // not a repeated literal — so it cannot omit a newly added algorithm.
        let accepted = super::DigestAlgo::accepted_list();
        for &algo in super::DigestAlgo::ALL {
            assert!(
                accepted.contains(algo.name()),
                "accepted_list() omits '{}': {accepted}",
                algo.name()
            );
        }
    }

    /// The third leg of the inline-special-form invariant, and the one that
    /// used to fail SILENTLY.
    ///
    /// Registering a name in `EVAL_SPECIAL_BUILTINS` but not in
    /// `INLINE_SPECIAL_FORMS` leaves the bareword call working while the UFCS
    /// spelling breaks: `parser.rs`'s `method_desugars_to_ufcs` consults only
    /// `INLINE_SPECIAL_FORMS`, so `$x.write_stdout()` becomes a `MethodCall`,
    /// and the MethodCall arm dispatches extensions → user functions → address
    /// blocks → map members and never reaches a builtin. Best case that is
    /// `FUNCTION_UNDEFINED`; worst case, with a user function of the same name
    /// in scope, one name quietly means two different things depending on how
    /// it is spelled. Nothing else in the tree ties the two lists together.
    #[test]
    fn eval_special_names_are_all_inline_special_forms() {
        for &name in super::EVAL_SPECIAL_BUILTINS {
            assert!(
                crate::evaluator::INLINE_SPECIAL_FORMS.contains(&name),
                "'{name}' is EVAL_SPECIAL but missing from INLINE_SPECIAL_FORMS \
                 — the bareword call would work while `$x.{name}()` silently \
                 resolved somewhere else"
            );
        }
    }

    /// Table-wide invariants of the structured contracts (0.29.0): required
    /// args precede optional args, at most one variadic arg and it is last,
    /// `terminates` effect implies `Terminates` failure mode, and every
    /// derived signature renders. The DSL cannot express these constraints
    /// in the type system, so they are pinned here.
    #[test]
    fn contract_invariants_hold_table_wide() {
        use crate::builtin_info::OperationalFailure;
        for info in super::BUILTINS
            .iter()
            .chain(crate::builtins_hof::HOFS.iter())
        {
            let args = info.contract.args;
            let mut seen_optional = false;
            for (i, a) in args.iter().enumerate() {
                if a.variadic {
                    assert_eq!(
                        i,
                        args.len() - 1,
                        "{}: variadic arg '{}' is not last",
                        info.name,
                        a.name
                    );
                }
                if a.required {
                    assert!(
                        !seen_optional,
                        "{}: required arg '{}' follows an optional arg",
                        info.name, a.name
                    );
                } else if !a.variadic {
                    seen_optional = true;
                }
            }
            assert!(
                args.iter().filter(|a| a.variadic).count() <= 1,
                "{}: more than one variadic arg",
                info.name
            );
            if info.contract.effects.terminates {
                assert_eq!(
                    info.contract.failure,
                    OperationalFailure::Terminates,
                    "{}: terminates effect without Terminates failure mode",
                    info.name
                );
            }
            // ReturnsResult means "failure is encoded in the returned
            // value" — a discarded result silently swallows exactly
            // that failure, so every such entry must be must_use
            // (codex C1 review finding).
            if info.contract.failure == OperationalFailure::ReturnsResult {
                assert!(
                    info.contract.effects.must_use,
                    "{}: returns-result failure without must_use",
                    info.name
                );
            }
            // exact_arities entries must each fall within the declared
            // min..=max range (they refine the range, never extend it).
            if let Some(set) = info.contract.exact_arities {
                assert!(!set.is_empty(), "{}: empty arities[] set", info.name);
                for &n in set {
                    assert!(
                        n >= info.contract.arity_min()
                            && info.contract.arity_max().is_none_or(|m| n <= m),
                        "{}: arities[] entry {} outside declared {}..{:?}",
                        info.name,
                        n,
                        info.contract.arity_min(),
                        info.contract.arity_max()
                    );
                }
            }
            assert!(!info.signature().is_empty(), "{}", info.name);
        }
    }

    /// SECURITY (COSMIX-002): `capability_category` FAILS OPEN — any name it
    /// doesn't explicitly match defaults to `CapabilityClass::Pure`, which a
    /// `CategoryAllowList` (webd handlers, maild filters) always allows. So a
    /// NEW builtin that touches the filesystem / network / a process but is
    /// forgotten in `capability_category` silently bypasses every embedder's
    /// sandbox. `sensitive_builtins_stay_categorized` (tests/limits.rs) guards
    /// against *de*-categorising a KNOWN sensitive builtin; this guards the
    /// other direction — a NEW builtin left at the Pure default.
    ///
    /// Fail-open is now impossible by construction — every `builtin_table!`
    /// entry MUST declare a `CapabilityClass` (the macro requires the field),
    /// and `capability_category` reads it from the table. This test is the
    /// PARITY guard: it pins the exact Pure/non-Pure partition so a future
    /// table edit that flips a builtin's class (e.g. silently marks a
    /// filesystem builtin Pure) is caught. A name is Pure IFF it is in
    /// `KNOWN_PURE`; the complement (the sensitive set) is checked for its
    /// specific class by `sensitive_builtins_stay_categorized`
    /// (tests/limits.rs).
    ///
    /// `KNOWN_PURE` is the set of builtins that are *genuinely* Pure (no host
    /// authority that matters for sandboxing — string/number/collection/
    /// in-memory-data/codec/crypto-keygen/clock/entropy ops). (NB: `sleep` is
    /// Pure — it grants no fs/net/process authority — but webd denies it
    /// separately as a thread-pin DoS, not via the capability class.)
    #[test]
    fn pure_builtins_match_known_pure() {
        use super::{BUILTIN_NAMES, CapabilityClass, capability_category};
        const KNOWN_PURE: &[&str] = &[
            "args",
            "band",
            "bnot",
            "bor",
            "bshl",
            "bshr",
            "bxor",
            "base64_decode",
            "base64_encode",
            "basename",
            "byte_index_of",
            "byte_lastpos",
            "byte_length",
            "byte_pos",
            "buffer",
            "buffer_get",
            "buffer_push",
            "buffer_set",
            "bytes_concat",
            "bytes_find",
            "bytes_from",
            "bytes_from_hex",
            "bytes_len",
            "bytes_split",
            "bytes_starts_with",
            "bytes_to_hex",
            "bytes_to_string",
            "concat",
            "constant_time_eq",
            "contains",
            "csv_parse",
            "data_encode",
            "data_parse",
            "date_format",
            "date_parse",
            "delete",
            "dirname",
            "display_width",
            "dkim_keygen",
            "drop",
            "ds_patch_elements",
            "ds_patch_signals",
            "ds_sse",
            "duration_format",
            "ends_with",
            "eprintf",
            "extname",
            "flat",
            "fmt",
            "format_bytes",
            "format_number",
            "freeze",
            "getopt",
            "grapheme_count",
            "grapheme_reverse",
            "grapheme_substr",
            "grep",
            // Subject-first string helpers + regex family + deep_eq (0.63.0)
            "before",
            "after",
            "before_last",
            "after_last",
            "split_once",
            "rsplit_once",
            "between",
            "strip_prefix",
            "strip_suffix",
            "replace_first",
            "count_of",
            "ltrim",
            "rtrim",
            "lines",
            "fields",
            "chars",
            "last_index_of",
            "deep_eq",
            "re_match",
            "re_find",
            "re_replace",
            "re_split",
            "grep_lines",
            "hash_blake3",
            // md5/sha1 are BROKEN hashes but they are still Pure in the
            // capability sense — they touch no host authority. "Pure" is not
            // "safe"; the warning lives in the description, not the class.
            "hash_md5",
            "hash_sha1",
            "hash_sha256",
            "has_key",
            "hmac_sha256",
            "help",
            "html_escape",
            "index_of",
            "ini_parse",
            "is_empty",
            "is_number",
            "join",
            "jq",
            "jq_all",
            "json_encode",
            "json_parse",
            "keys",
            "lastpos",
            "left",
            "len",
            "length",
            "lower",
            "lpad",
            "lpad_w",
            "markdown",
            "markdown_escape",
            "merge",
            "now_iso",
            "parse_form",
            "parse_query",
            "path_join",
            "path_parts",
            "pop",
            "pos",
            // The byte-exact stdio family (v0.65.0) is Pure for the same
            // reason `printf` is: writing to the fds the process was HANDED
            // grants no authority it did not already have. Reading stdin is
            // different — `read_stdin_bytes` is Env, like `read_stdin`.
            "eprint_raw",
            "print_raw",
            "printf",
            "push",
            "raise",
            "random_password",
            "range",
            "regex_find",
            "regex_match",
            // Validation family (0.29.0) — pure boundary checks.
            "require_key",
            "expect_type",
            "nonblank",
            "get_or",
            "validate",
            "regex_replace",
            "regex_split",
            "relative_time",
            "repeat",
            "replace",
            "reverse",
            "right",
            "rpad",
            "rpad_w",
            "sanitize",
            "shell_quote",
            "shift",
            "sleep",
            "slice",
            "sort",
            "split",
            "sql_quote",
            "starts_with",
            "string_to_bytes",
            "strip",
            "substr",
            "take",
            "template",
            "time",
            "toml_encode",
            "toml_parse",
            "to_number",
            "to_string",
            "trim",
            "type",
            "unique",
            "upper",
            "url_decode",
            "url_encode",
            "url_parse",
            "uuid",
            "values",
            "word",
            "words",
            "word_wrap",
            "word_wrap_w",
            "write_stderr",
            "write_stdout",
            "xml_parse",
            "zip",
            // Math (v0.19.0) — pure f64 numerics, no host authority.
            "abs",
            "acos",
            "asin",
            "atan",
            "atan2",
            "cbrt",
            "ceil",
            "clamp",
            "cos",
            "e",
            "exp",
            "floor",
            "hypot",
            "ln",
            "log",
            "log10",
            "log2",
            "max",
            "min",
            "pi",
            "pow",
            "random",
            "round",
            "sign",
            "sin",
            "sqrt",
            "tan",
            "trunc",
        ];
        for &name in BUILTIN_NAMES {
            let is_pure = capability_category(name) == CapabilityClass::Pure;
            assert_eq!(
                is_pure,
                KNOWN_PURE.contains(&name),
                "builtin '{name}' classifies Pure={is_pure} but KNOWN_PURE membership \
                 disagrees — its table `CapabilityClass` changed. If it now touches the \
                 filesystem/network/a process/host env it must be a sensitive class \
                 (and removed from KNOWN_PURE); if it is genuinely Pure, add it to \
                 KNOWN_PURE."
            );
        }
    }
}

#[cfg(test)]
mod getopt_tests {
    use super::call_builtin;
    use crate::value::Value;
    use indexmap::IndexMap;

    fn s(x: &str) -> Value {
        Value::String(x.into())
    }
    fn list(xs: &[&str]) -> Value {
        Value::list(xs.iter().map(|x| s(x)).collect())
    }
    fn flag(short: &str) -> Value {
        let mut m = IndexMap::new();
        m.insert("short".into(), s(short));
        Value::map(m)
    }
    fn valopt(short: &str) -> Value {
        let mut m = IndexMap::new();
        m.insert("short".into(), s(short));
        m.insert("arg".into(), Value::Bool(true));
        Value::map(m)
    }
    // The standard spec used across cases: -a/--all (flag), -o/--output (value),
    // -v/--verbose (flag).
    fn spec() -> Value {
        let mut m = IndexMap::new();
        m.insert("all".into(), flag("a"));
        m.insert("output".into(), valopt("o"));
        m.insert("verbose".into(), flag("v"));
        Value::map(m)
    }
    // Parse and return the {opts, rest, errors} map.
    fn parse(argv: &[&str]) -> IndexMap<String, Value> {
        let mut res = call_builtin("getopt", vec![list(argv), spec()]).expect("getopt ok");
        match res.as_mut() {
            Some(Value::Map(m)) => std::rc::Rc::unwrap_or_clone(std::mem::take(m)),
            other => panic!("getopt returned {other:?}, expected a map"),
        }
    }
    fn opts(m: &IndexMap<String, Value>) -> &IndexMap<String, Value> {
        match &m["opts"] {
            Value::Map(o) => o,
            other => panic!("opts is {other:?}"),
        }
    }
    fn rest(m: &IndexMap<String, Value>) -> Vec<String> {
        match &m["rest"] {
            Value::List(l) => l.iter().map(|v| v.to_mix_string()).collect(),
            other => panic!("rest is {other:?}"),
        }
    }
    fn errors(m: &IndexMap<String, Value>) -> Vec<String> {
        match &m["errors"] {
            Value::List(l) => l.iter().map(|v| v.to_mix_string()).collect(),
            other => panic!("errors is {other:?}"),
        }
    }

    #[test]
    fn long_flag_short_value_and_positionals() {
        let m = parse(&["--all", "-o", "out.txt", "pos1", "pos2"]);
        let o = opts(&m);
        assert_eq!(o["all"], Value::Bool(true));
        assert_eq!(o["output"], s("out.txt"));
        assert_eq!(o["verbose"], Value::Bool(false)); // declared, defaults false
        assert_eq!(rest(&m), vec!["pos1", "pos2"]);
        assert!(errors(&m).is_empty());
    }

    #[test]
    fn short_flag_and_inline_long_value() {
        let m = parse(&["-a", "--output=x.log", "-v"]);
        let o = opts(&m);
        assert_eq!(o["all"], Value::Bool(true));
        assert_eq!(o["output"], s("x.log"));
        assert_eq!(o["verbose"], Value::Bool(true));
        assert!(rest(&m).is_empty());
        assert!(errors(&m).is_empty());
    }

    #[test]
    fn unset_defaults() {
        // No args: every declared option present; flags false, value-opts nil.
        let m = parse(&[]);
        let o = opts(&m);
        assert_eq!(o["all"], Value::Bool(false));
        assert_eq!(o["output"], Value::Nil);
        assert_eq!(o["verbose"], Value::Bool(false));
        assert!(rest(&m).is_empty());
        assert!(errors(&m).is_empty());
    }

    #[test]
    fn double_dash_terminator() {
        // Everything after `--` is positional, even option-looking tokens.
        let m = parse(&["-a", "--", "-o", "--output", "x"]);
        assert_eq!(opts(&m)["all"], Value::Bool(true));
        assert_eq!(opts(&m)["output"], Value::Nil); // -o after -- is NOT consumed
        assert_eq!(rest(&m), vec!["-o", "--output", "x"]);
        assert!(errors(&m).is_empty());
    }

    #[test]
    fn collect_all_errors() {
        // Unknown long, unknown short, and a value-option at end with no value
        // are all collected; opts stay at defaults.
        let m = parse(&["--bogus", "-z", "-o"]);
        let errs = errors(&m);
        assert_eq!(errs.len(), 3, "got: {errs:?}");
        assert!(errs[0].contains("unknown option: --bogus"));
        assert!(errs[1].contains("unknown option: -z"));
        assert!(errs[2].contains("option -o requires a value"));
        assert_eq!(opts(&m)["output"], Value::Nil);
    }

    #[test]
    fn flag_with_value_and_long_missing_value() {
        // A flag given `=value` errors; a long value-opt at end errors.
        let m = parse(&["--all=nope", "--output"]);
        let errs = errors(&m);
        assert_eq!(errs.len(), 2, "got: {errs:?}");
        assert!(errs[0].contains("option --all takes no value"));
        assert!(errs[1].contains("option --output requires a value"));
    }

    #[test]
    fn bare_dash_is_positional() {
        let m = parse(&["-", "-a"]);
        assert_eq!(rest(&m), vec!["-"]);
        assert_eq!(opts(&m)["all"], Value::Bool(true));
    }

    #[test]
    fn no_bundling_is_an_error() {
        // `-av` is not split into -a -v (minimal: no bundling).
        let m = parse(&["-av"]);
        let errs = errors(&m);
        assert_eq!(errs.len(), 1, "got: {errs:?}");
        assert!(errs[0].contains("unsupported short-option token: -av"));
        assert_eq!(opts(&m)["all"], Value::Bool(false));
    }

    #[test]
    fn malformed_spec_dies() {
        // A non-map spec entry is a script bug -> error, not a collected string.
        let mut bad = IndexMap::new();
        bad.insert("all".into(), Value::Bool(true)); // not a map
        let r = call_builtin("getopt", vec![list(&["--all"]), Value::map(bad)]);
        assert!(r.is_err(), "expected a spec error");
    }

    #[test]
    fn invalid_long_name_dies() {
        // Long names the grammar can't address are script bugs, not user errors.
        for bad in ["", "-x", "foo=bar", "two words"] {
            let mut sp = IndexMap::new();
            sp.insert(bad.into(), flag("a"));
            let r = call_builtin("getopt", vec![list(&[]), Value::map(sp)]);
            assert!(
                r.is_err(),
                "expected an invalid-long-name error for {bad:?}"
            );
        }
    }

    #[test]
    fn duplicate_short_dies() {
        let mut sp = IndexMap::new();
        sp.insert("all".into(), flag("a"));
        sp.insert("append".into(), flag("a")); // collides on -a
        let r = call_builtin("getopt", vec![list(&[]), Value::map(sp)]);
        assert!(r.is_err(), "expected a duplicate-short error");
    }

    #[cfg(feature = "crypto")]
    #[test]
    fn hash_file_matches_string_hash_and_streams() {
        // hash_file(path) is just a streaming source for the SAME digest the
        // string builtins produce — the sha256 output must be byte-identical
        // to hash_sha256(content), and blake3 to hash_blake3(content).
        let mut p = std::env::temp_dir();
        p.push(format!("cosmix-mix-hashfile-{}", std::process::id()));
        let content = "hello mix\nmanifest line\n";
        std::fs::write(&p, content).unwrap();
        let ps = Value::String(p.to_string_lossy().into());

        // `Value` implements Drop, so match on a reference and clone the digest.
        let str_hash = |b: &str, s: &str| match &call_builtin(b, vec![Value::String(s.into())]) {
            Ok(Some(Value::String(h))) => h.clone(),
            other => panic!("{b}: {other:?}"),
        };
        let file_hash = |args: Vec<Value>| match &call_builtin("hash_file", args) {
            Ok(Some(Value::String(h))) => h.clone(),
            other => panic!("hash_file: {other:?}"),
        };

        // default algo = sha256
        assert_eq!(
            file_hash(vec![ps.clone()]),
            str_hash("hash_sha256", content)
        );
        assert_eq!(
            file_hash(vec![ps.clone(), Value::String("sha256".into())]),
            str_hash("hash_sha256", content)
        );
        assert_eq!(
            file_hash(vec![ps.clone(), Value::String("blake3".into())]),
            str_hash("hash_blake3", content)
        );
        // md5 and sha1 became valid algos in v0.66.0 — this assertion used to
        // be `hash_file(p, "md5")` ERRORS, and the change is deliberate. Each
        // is checked against its own in-memory builtin, so the streaming path
        // and the one-shot path cannot disagree on the same bytes.
        assert_eq!(
            file_hash(vec![ps.clone(), Value::String("md5".into())]),
            str_hash("hash_md5", content)
        );
        assert_eq!(
            file_hash(vec![ps.clone(), Value::String("sha1".into())]),
            str_hash("hash_sha1", content)
        );
        // nil algo falls back to sha256 (a `?? "sha256"` caller passes Nil)
        assert_eq!(
            file_hash(vec![ps.clone(), Value::Nil]),
            str_hash("hash_sha256", content)
        );

        // {raw: true} on the streaming path yields the SAME digest as the hex
        // spelling, just as bytes — pinned through bytes_to_hex so a wrong
        // byte order or a truncated digest would show.
        let raw = call_builtin(
            "hash_file",
            vec![
                ps.clone(),
                Value::String("sha256".into()),
                Value::map(IndexMap::from([("raw".to_string(), Value::Bool(true))])),
            ],
        )
        .unwrap();
        match &raw {
            Some(Value::Bytes(b)) => {
                assert_eq!(b.len(), 32);
                assert_eq!(super::hex_encode(b), str_hash("hash_sha256", content));
            }
            other => panic!("expected bytes from {{raw:true}}, got {other:?}"),
        }

        // A genuinely unknown algo and a missing path both error, not silently
        // return a bogus digest.
        assert!(
            call_builtin("hash_file", vec![ps.clone(), Value::String("sha512".into())]).is_err()
        );
        assert!(
            call_builtin(
                "hash_file",
                vec![Value::String("/no/such/mix/hashfile".into())]
            )
            .is_err()
        );
        assert!(call_builtin("hash_file", vec![]).is_err());

        let _ = std::fs::remove_file(&p);
    }

    #[cfg(feature = "markdown")]
    #[test]
    fn markdown_renders_gfm_and_is_safe() {
        let md = |s: &str| match &call_builtin("markdown", vec![Value::String(s.into())]) {
            Ok(Some(Value::String(h))) => h.clone(),
            other => panic!("unexpected markdown result: {other:?}"),
        };
        // headings / bold / em / lists
        let h = md("# Title\n\n**bold** and *em*\n\n- a\n- b");
        assert!(h.contains("<h1>Title</h1>"), "{h}");
        assert!(h.contains("<strong>bold</strong>") && h.contains("<em>em</em>"));
        assert!(h.contains("<li>a</li>"));
        // GFM: tables + strikethrough
        assert!(md("| A | B |\n|---|---|\n| 1 | 2 |").contains("<table>"));
        assert!(md("~~gone~~").contains("<del>gone</del>"));
        // raw HTML in source is escaped, never active markup
        let x = md("<script>alert(1)</script>");
        assert!(
            !x.contains("<script>") && x.contains("&lt;script&gt;"),
            "{x}"
        );
        // dangerous URL schemes neutralised; relative/normal URLs pass through
        assert!(!md("[x](javascript:alert(1))").contains("javascript:"));
        assert!(
            md("[d](data/img.png)").contains("data/img.png"),
            "relative not blocked"
        );
        assert!(md("[h](https://example.com)").contains("https://example.com"));
    }
}

#[cfg(all(test, feature = "crypto"))]
mod hmac_tests {
    use super::call_builtin;
    use crate::value::Value;

    fn hmac_v(key: Value, msg: &str) -> String {
        match &call_builtin("hmac_sha256", vec![key, Value::String(msg.into())]) {
            Ok(Some(Value::String(h))) => h.clone(),
            other => panic!("hmac_sha256 failed: {other:?}"),
        }
    }

    fn hmac(key: &str, msg: &str) -> String {
        hmac_v(Value::String(key.into()), msg)
    }

    /// RFC 4231 test vectors (cases 1, 2 and 7 — short key, "Jefe", and a
    /// key longer than the block size, which exercises the hash-the-key path).
    #[test]
    fn rfc4231_vectors() {
        // case 2: key "Jefe", data "what do ya want for nothing?"
        assert_eq!(
            hmac("Jefe", "what do ya want for nothing?"),
            "5bdcc146bf60754e6a042426089575c75a003f089d2739839dec58b964ec3843"
        );
        // case 1: key = 20×0x0b, data "Hi There" (bytes key via Value::bytes)
        assert_eq!(
            hmac_v(Value::bytes(vec![0x0b; 20]), "Hi There"),
            "b0344c61d8db38535ca8afceaf0bf12b881dc200c9833da726e9376c2e32cff7"
        );
        // case 7: 131-byte key (0xaa ×131) > block size → key is hashed first
        assert_eq!(
            hmac_v(
                Value::bytes(vec![0xaa; 131]),
                "This is a test using a larger than block-size key and a larger than block-size data. The key needs to be hashed before being used by the HMAC algorithm."
            ),
            "9b09ffa71b942fcb27635fbcd5b0e944bfdc63644f0713938a7f51535c3a35e2"
        );
    }

    #[test]
    fn constant_time_eq_semantics() {
        let cte = |a: &str, b: &str| match &call_builtin(
            "constant_time_eq",
            vec![Value::String(a.into()), Value::String(b.into())],
        ) {
            Ok(Some(Value::Bool(r))) => *r,
            other => panic!("constant_time_eq failed: {other:?}"),
        };
        assert!(cte("deadbeef", "deadbeef"));
        assert!(!cte("deadbeef", "deadbeee"));
        assert!(!cte("deadbeef", "deadbee"));
        assert!(cte("", ""));
    }
}

#[cfg(all(test, feature = "xml"))]
mod xml_tests {
    use super::call_builtin;
    use crate::value::Value;

    fn parse(xml: &str) -> Value {
        match call_builtin("xml_parse", vec![Value::String(xml.into())]) {
            Ok(Some(v)) => v,
            other => panic!("xml_parse failed: {other:?}"),
        }
    }

    fn parse_err(xml: &str) -> String {
        match call_builtin("xml_parse", vec![Value::String(xml.into())]) {
            Err(e) => format!("{e:?}"),
            other => panic!("expected xml_parse error, got {other:?}"),
        }
    }

    fn get<'a>(v: &'a Value, k: &str) -> &'a Value {
        match v {
            Value::Map(m) => m
                .get(k)
                .unwrap_or_else(|| panic!("missing key {k:?} in {v:?}")),
            other => panic!("expected map to look up {k:?}, got {other:?}"),
        }
    }

    fn text(v: &Value) -> &str {
        match v {
            Value::String(s) => s,
            other => panic!("expected string, got {other:?}"),
        }
    }

    /// The consumer this builtin was built for: a namespace-prefixed SOAP
    /// response (Synergy-Wholesale-shaped, placeholder values) navigated by
    /// plain map access in simple mode.
    #[test]
    fn simple_mode_soap_response() {
        let xml = r#"<?xml version="1.0" encoding="UTF-8"?>
<SOAP-ENV:Envelope xmlns:SOAP-ENV="http://schemas.xmlsoap.org/soap/envelope/" xmlns:ns1="urn:Api">
  <SOAP-ENV:Body>
    <ns1:domainInfoResponse>
      <return>
        <status>OK</status>
        <domainName>example.com</domainName>
        <domainExpiry>2027-01-01</domainExpiry>
        <nameServers>
          <string>ns1.example.net</string>
          <string>ns2.example.net</string>
        </nameServers>
      </return>
    </ns1:domainInfoResponse>
  </SOAP-ENV:Body>
</SOAP-ENV:Envelope>"#;
        let v = parse(xml);
        // prefixes stripped: Envelope/Body/domainInfoResponse by local name
        let ret = get(
            get(get(get(&v, "Envelope"), "Body"), "domainInfoResponse"),
            "return",
        );
        assert_eq!(text(get(ret, "status")), "OK");
        assert_eq!(text(get(ret, "domainName")), "example.com");
        assert_eq!(text(get(ret, "domainExpiry")), "2027-01-01");
        // repeated siblings collapse to a list
        match get(get(ret, "nameServers"), "string") {
            Value::List(l) => {
                assert_eq!(l.len(), 2);
                assert_eq!(text(&l[0]), "ns1.example.net");
                assert_eq!(text(&l[1]), "ns2.example.net");
            }
            other => panic!("expected list of nameservers, got {other:?}"),
        }
        // xmlns declarations dropped, so Envelope carries no @xmlns keys
        match get(&v, "Envelope") {
            Value::Map(m) => assert!(m.keys().all(|k| !k.starts_with('@')), "xmlns leaked: {m:?}"),
            other => panic!("expected map, got {other:?}"),
        }
    }

    #[test]
    fn simple_mode_attrs_text_entities_cdata() {
        // attributes become @keys; a single child stays scalar; mixed text
        // lands under #text
        let v = parse(r#"<a id="1" ns:x="2"><b/>hi &amp; &#65;<![CDATA[<raw>]]></a>"#);
        let a = get(&v, "a");
        assert_eq!(text(get(a, "@id")), "1");
        assert_eq!(text(get(a, "@x")), "2", "attr prefix not stripped");
        assert_eq!(text(get(a, "b")), "", "empty element is empty string");
        assert_eq!(text(get(a, "#text")), "hi & A<raw>");
        // pure leaf: text IS the value; entity in attribute resolves
        let v = parse(r#"<p q="a&amp;b">  spaced  </p>"#);
        let p = get(&v, "p");
        assert_eq!(text(get(p, "@q")), "a&b");
        assert_eq!(text(get(p, "#text")), "spaced", "leaf text is trimmed");
    }

    #[test]
    fn tree_mode_full_fidelity() {
        let v = match call_builtin(
            "xml_parse",
            vec![Value::String("<ns:a x=\"1\">t<b/>u</ns:a>".into()), {
                let mut m = indexmap::IndexMap::new();
                m.insert("mode".to_string(), Value::String("tree".into()));
                Value::map(m)
            }],
        ) {
            Ok(Some(v)) => v,
            other => panic!("tree parse failed: {other:?}"),
        };
        assert_eq!(text(get(&v, "name")), "ns:a", "tree keeps prefixes");
        assert_eq!(text(get(get(&v, "attrs"), "x")), "1");
        match get(&v, "children") {
            Value::List(l) => {
                assert_eq!(l.len(), 3, "text, element, text: {l:?}");
                assert_eq!(text(&l[0]), "t");
                assert_eq!(text(get(&l[1], "name")), "b");
                assert_eq!(text(&l[2]), "u");
            }
            other => panic!("expected children list, got {other:?}"),
        }
    }

    #[test]
    fn malformed_and_hostile_inputs_error() {
        // quick-xml's check_end_names catches this (our own stack check is
        // the backstop if that config ever changes)
        assert!(parse_err("<a><b></a>").contains("expected `</b>`"));
        assert!(parse_err("<a/><b/>").contains("multiple root elements"));
        assert!(parse_err("<a><b>").contains("unclosed element"));
        assert!(parse_err("").contains("no root element"));
        assert!(parse_err("junk before <a/>").contains("outside the root"));
        assert!(parse_err("<a>&nosuch;</a>").contains("unknown entity"));
        // depth cap (parse is iterative, so this errors instead of blowing
        // the stack; keep the probe above the 256 cap but small)
        let deep = "<d>".repeat(300) + &"</d>".repeat(300);
        assert!(parse_err(&deep).contains("nesting deeper"));
        // a self-closing element AT the cap must hit the same wall
        let deep_empty = "<d>".repeat(256) + "<x/>" + &"</d>".repeat(256);
        assert!(parse_err(&deep_empty).contains("nesting deeper"));
        // bad mode / bad opts
        let e = match call_builtin(
            "xml_parse",
            vec![Value::String("<a/>".into()), Value::String("tree".into())],
        ) {
            Err(e) => format!("{e:?}"),
            other => panic!("expected opts error, got {other:?}"),
        };
        assert!(e.contains("options must be a map"));
    }

    #[test]
    fn bytes_input_parses() {
        let v = match call_builtin("xml_parse", vec![Value::bytes(b"<a>ok</a>".to_vec())]) {
            Ok(Some(v)) => v,
            other => panic!("bytes parse failed: {other:?}"),
        };
        assert_eq!(text(get(&v, "a")), "ok");
    }
}

#[cfg(test)]
mod math_tests {
    use super::{call_builtin, is_builtin};
    use crate::value::Value;

    // Call a builtin and require a finite-or-not f64 back.
    fn num(name: &str, args: &[f64]) -> f64 {
        let v = call_builtin(name, args.iter().map(|n| Value::Number(*n)).collect())
            .unwrap_or_else(|e| panic!("{name} errored: {e:?}"))
            .unwrap_or_else(|| panic!("{name} returned no value"));
        match v {
            Value::Number(n) => n,
            other => panic!("{name} returned non-number {other:?}"),
        }
    }

    #[test]
    fn rounding_family_whole() {
        assert_eq!(num("round", &[2.5]), 3.0); // half away from zero
        assert_eq!(num("round", &[-2.5]), -3.0);
        assert_eq!(num("round", &[2.4]), 2.0);
        assert_eq!(num("floor", &[2.9]), 2.0);
        assert_eq!(num("floor", &[-2.1]), -3.0);
        assert_eq!(num("ceil", &[2.1]), 3.0);
        assert_eq!(num("ceil", &[-2.9]), -2.0);
        assert_eq!(num("trunc", &[2.9]), 2.0);
        assert_eq!(num("trunc", &[-2.9]), -2.0);
    }

    #[test]
    fn rounding_to_decimals_and_negative_places() {
        assert_eq!(num("round", &[2.34567, 2.0]), 2.35);
        assert_eq!(num("floor", &[3.999, 2.0]), 3.99);
        assert_eq!(num("ceil", &[3.001, 2.0]), 3.01);
        assert_eq!(num("trunc", &[3.789, 1.0]), 3.7);
        // negative places round to tens/hundreds
        assert_eq!(num("round", &[1234.0, -2.0]), 1200.0);
        assert_eq!(num("round", &[1250.0, -2.0]), 1300.0);
    }

    #[test]
    fn rounding_handles_non_finite_and_huge_places() {
        assert!(num("round", &[f64::NAN, 2.0]).is_nan());
        assert!(num("round", &[f64::INFINITY, 2.0]).is_infinite());
        // absurd precision must not produce NaN via factor overflow/underflow:
        // huge +nd is a no-op (x unchanged); huge -nd rounds to 0, not 0/0=NaN
        assert_eq!(num("round", &[5.0, 400.0]), 5.0);
        assert_eq!(num("round", &[5.0, -400.0]), 0.0);
        // a fractional value with absurd +precision is unchanged, not rounded
        // to an integer (the scale-up overflow must return x, not op(x))
        assert_eq!(num("round", &[2.5, 400.0]), 2.5);
    }

    #[test]
    fn coarse_negative_place_rounding_beyond_1e15() {
        // the divide-regime makes coarse rounding work past 10^15 scale
        // (a naïve x * 10^nd would underflow to 0/0 = NaN here)
        assert_eq!(num("round", &[1.5e18, -18.0]), 2e18); // half away from zero
        assert_eq!(num("floor", &[1.5e18, -18.0]), 1e18);
        assert_eq!(num("ceil", &[1.1e18, -18.0]), 2e18);
        // ordinary coarse rounding still correct
        assert_eq!(num("round", &[1234.0, -3.0]), 1000.0);
        assert_eq!(num("round", &[1567.0, -3.0]), 2000.0);
    }

    #[test]
    fn non_finite_decimal_places_errors() {
        // NaN/inf as the *places* argument is a misuse — must error, not
        // silently round to 0 places
        assert!(call_builtin("round", vec![Value::Number(1.9), Value::Number(f64::NAN)]).is_err());
        assert!(
            call_builtin(
                "floor",
                vec![Value::Number(1.9), Value::Number(f64::INFINITY)]
            )
            .is_err()
        );
    }

    #[test]
    fn abs_sign_basics() {
        assert_eq!(num("abs", &[-7.0]), 7.0);
        assert_eq!(num("abs", &[7.0]), 7.0);
        assert_eq!(num("sign", &[-3.0]), -1.0);
        assert_eq!(num("sign", &[3.0]), 1.0);
        assert_eq!(num("sign", &[0.0]), 0.0);
        assert_eq!(num("sign", &[-0.0]), 0.0); // signed zero → 0, not -1
        assert!(num("sign", &[f64::NAN]).is_nan());
    }

    #[test]
    fn powers_roots_logs() {
        assert_eq!(num("sqrt", &[9.0]), 3.0);
        assert!(num("sqrt", &[-1.0]).is_nan());
        assert_eq!(num("cbrt", &[-27.0]), -3.0);
        assert_eq!(num("pow", &[2.0, 10.0]), 1024.0);
        assert_eq!(num("pow", &[0.0, 0.0]), 1.0);
        assert!((num("ln", &[std::f64::consts::E]) - 1.0).abs() < 1e-12);
        assert_eq!(num("log10", &[1000.0]), 3.0);
        assert_eq!(num("log2", &[8.0]), 3.0);
        assert_eq!(num("log", &[81.0, 3.0]), 4.0);
        assert!(num("ln", &[0.0]).is_infinite() && num("ln", &[0.0]) < 0.0);
    }

    #[test]
    fn trig_and_constants() {
        assert!((num("pi", &[]) - std::f64::consts::PI).abs() < 1e-12);
        assert!((num("e", &[]) - std::f64::consts::E).abs() < 1e-12);
        assert!(num("sin", &[0.0]).abs() < 1e-12);
        assert!((num("cos", &[0.0]) - 1.0).abs() < 1e-12);
        assert!((num("atan2", &[1.0, 1.0]) - std::f64::consts::FRAC_PI_4).abs() < 1e-12);
        assert_eq!(num("hypot", &[3.0, 4.0]), 5.0);
    }

    #[test]
    fn min_max_variadic_list_and_nan_skip() {
        assert_eq!(num("min", &[3.0, 1.0, 2.0]), 1.0);
        assert_eq!(num("max", &[3.0, 1.0, 2.0]), 3.0);
        // NaN is skipped, not propagated
        assert_eq!(num("max", &[1.0, f64::NAN, 5.0]), 5.0);
        assert_eq!(num("min", &[f64::NAN, 2.0]), 2.0);
        // single list argument
        let list = Value::list(vec![
            Value::Number(7.0),
            Value::Number(2.0),
            Value::Number(9.0),
        ]);
        let got = call_builtin("max", vec![list]).unwrap().unwrap();
        assert!(matches!(got, Value::Number(n) if n == 9.0));
    }

    #[test]
    fn min_max_string_lexicographic_superset() {
        // strings order lexicographically, matching the `<`/`>` operator and
        // the prelude shim this replaces
        let got = call_builtin(
            "max",
            vec![
                Value::String("apple".into()),
                Value::String("banana".into()),
            ],
        )
        .unwrap()
        .unwrap();
        assert!(matches!(got, Value::String(ref s) if s == "banana"));
        // numeric strings coerce and compare numerically ("5" vs "10")
        let got = call_builtin(
            "min",
            vec![Value::String("5".into()), Value::String("10".into())],
        )
        .unwrap()
        .unwrap();
        assert!(matches!(got, Value::Number(n) if n == 5.0));
    }

    #[test]
    fn min_max_mixed_errors_and_empty_list_errors() {
        let mixed = call_builtin("min", vec![Value::Number(1.0), Value::String("abc".into())]);
        assert!(mixed.is_err(), "mixed numeric/non-numeric should error");
        let empty = call_builtin("max", vec![Value::list(vec![])]);
        assert!(empty.is_err(), "max of empty list should error");
    }

    #[test]
    fn clamp_bounds_and_bad_range() {
        assert_eq!(num("clamp", &[5.0, 0.0, 10.0]), 5.0);
        assert_eq!(num("clamp", &[-3.0, 0.0, 10.0]), 0.0);
        assert_eq!(num("clamp", &[42.0, 0.0, 10.0]), 10.0);
        let bad = call_builtin(
            "clamp",
            vec![Value::Number(5.0), Value::Number(10.0), Value::Number(0.0)],
        );
        assert!(bad.is_err(), "lo > hi should error");
        // A NaN x must fall through both comparisons and return as-is — pins the
        // manual-compare against a regression to f64::clamp (which panics on a
        // NaN bound and orders NaN unexpectedly).
        assert!(num("clamp", &[f64::NAN, 0.0, 10.0]).is_nan());
        // A NaN *bound* is a malformed range — must error, not silently mask it.
        assert!(
            call_builtin(
                "clamp",
                vec![
                    Value::Number(5.0),
                    Value::Number(f64::NAN),
                    Value::Number(10.0)
                ]
            )
            .is_err(),
            "NaN lower bound should error"
        );
        assert!(
            call_builtin(
                "clamp",
                vec![
                    Value::Number(5.0),
                    Value::Number(0.0),
                    Value::Number(f64::NAN)
                ]
            )
            .is_err(),
            "NaN upper bound should error"
        );
        // ±inf bounds remain legitimate
        assert_eq!(num("clamp", &[5.0, 0.0, f64::INFINITY]), 5.0);
        assert_eq!(num("clamp", &[-5.0, 0.0, f64::INFINITY]), 0.0);
    }

    #[test]
    fn min_max_list_string_and_mixed_paths() {
        // string list → lexicographic (the list-unwrap feeding the all-string arm)
        let got = call_builtin(
            "max",
            vec![Value::list(vec![
                Value::String("apple".into()),
                Value::String("cherry".into()),
                Value::String("banana".into()),
            ])],
        )
        .unwrap()
        .unwrap();
        assert!(matches!(got, Value::String(ref s) if s == "cherry"));
        // mixed list (number + non-numeric string) → error
        let mixed = call_builtin(
            "min",
            vec![Value::list(vec![
                Value::Number(1.0),
                Value::String("abc".into()),
            ])],
        );
        assert!(mixed.is_err(), "mixed-type list should error");
    }

    #[test]
    fn non_numeric_argument_errors() {
        let r = call_builtin("sqrt", vec![Value::String("hello".into())]);
        assert!(r.is_err(), "sqrt of a non-numeric string should error");
    }

    #[test]
    fn realpath_resolves_symlinks_and_nils_the_missing() {
        let dir = std::env::temp_dir().join(format!("mix_realpath_{}", std::process::id()));
        let _ = std::fs::create_dir_all(&dir);
        let target = dir.join("target.txt");
        std::fs::write(&target, b"x").unwrap();
        let link = dir.join("link.txt");
        let _ = std::fs::remove_file(&link);
        #[cfg(unix)]
        std::os::unix::fs::symlink(&target, &link).unwrap();
        // a symlink resolves to the canonical target
        let r = call_builtin(
            "realpath",
            vec![Value::String(link.to_string_lossy().into())],
        )
        .unwrap()
        .unwrap();
        let canon_target = std::fs::canonicalize(&target).unwrap();
        assert!(matches!(r, Value::String(ref s) if s == &canon_target.to_string_lossy()));
        // a missing path is nil, never an error
        let r = call_builtin(
            "realpath",
            vec![Value::String(dir.join("nope").to_string_lossy().into())],
        )
        .unwrap()
        .unwrap();
        assert!(matches!(r, Value::Nil));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn numeric_string_and_bool_coercion() {
        // to_number-style coercion, consistent with the rest of Mix
        let r = call_builtin("sqrt", vec![Value::String("16".into())])
            .unwrap()
            .unwrap();
        assert!(matches!(r, Value::Number(n) if n == 4.0));
        let r = call_builtin("abs", vec![Value::Bool(true)])
            .unwrap()
            .unwrap();
        assert!(matches!(r, Value::Number(n) if n == 1.0));
    }

    #[test]
    fn every_math_name_passes_the_is_builtin_gate() {
        // The evaluator gates dispatch on is_builtin(); a name missing there
        // compiles + unit-tests green but dies "undefined function" at runtime.
        for name in [
            "round", "floor", "ceil", "trunc", "abs", "sign", "sqrt", "cbrt", "pow", "exp", "ln",
            "log10", "log2", "log", "min", "max", "clamp", "hypot", "sin", "cos", "tan", "asin",
            "acos", "atan", "atan2", "pi", "e",
        ] {
            assert!(is_builtin(name), "{name} missing from is_builtin()");
        }
    }
}

#[cfg(test)]
mod loud_numeric_argument_tests {
    use super::call_builtin;
    use crate::value::Value;

    fn bad() -> Value {
        Value::String("2x".into())
    }

    /// pid/signal domain failures keep the TYPE_MISMATCH code documented
    /// "strict since v0.52.0" — the domain gate filters entry, the pinned
    /// public code survives it (0.59.0 review, finding M4). Fragment-level
    /// message pins elsewhere cannot catch a code-only recode; this can.
    #[test]
    fn pid_and_signal_domain_failures_stay_type_mismatch() {
        let err = super::pid_int_arg("kill", "signal", &Value::Number(9.5)).unwrap_err();
        assert_eq!(err.info().map(|i| i.code.as_str()), Some("TYPE_MISMATCH"));
        let err = super::pid_int_arg("kill", "pid", &Value::Number(1e18)).unwrap_err();
        assert_eq!(err.info().map(|i| i.code.as_str()), Some("TYPE_MISMATCH"));
    }

    /// duration_format's documented leniency is FINITE-only: negatives clamp
    /// to 0s and floats truncate (datetime.md), but every non-finite raises —
    /// a bare `< 0.0` clamp would swallow -inf (0.59.0 review round 2).
    #[cfg(feature = "datetime")]
    #[test]
    fn duration_format_clamps_finite_negatives_only() {
        let ok = call_builtin("duration_format", vec![Value::Number(-5.0)])
            .unwrap()
            .unwrap();
        assert_eq!(ok, Value::String("0s".into()));
        for bad in [f64::NEG_INFINITY, f64::INFINITY, f64::NAN] {
            let err = call_builtin("duration_format", vec![Value::Number(bad)]).unwrap_err();
            assert_eq!(
                err.info().map(|i| i.code.as_str()),
                Some("VALUE_OUT_OF_RANGE"),
                "input {bad}"
            );
        }
    }

    fn assert_type_error(name: &str, args: Vec<Value>, position: usize) {
        let err = call_builtin(name, args).expect_err("non-numeric argument must raise");
        let info = err
            .info()
            .expect("numeric argument error must be structured");
        assert_eq!(info.code, "TYPE_MISMATCH", "{name}: {info:?}");
        assert!(
            info.message.contains(&format!("{name}()")),
            "{name}: {}",
            info.message
        );
        assert!(
            info.message
                .contains(&format!("argument {position} must be a number")),
            "{name}: {}",
            info.message
        );
        assert!(info.message.contains("\"2x\""), "{name}: {}", info.message);
        assert!(info.message.contains("string"), "{name}: {}", info.message);
    }

    #[test]
    fn every_former_numeric_fallback_rejects_an_unparseable_value() {
        for (name, args, position) in [
            ("left", vec![Value::String("abcdef".into()), bad()], 2),
            ("right", vec![Value::String("abcdef".into()), bad()], 2),
            ("substr", vec![Value::String("abcdef".into()), bad()], 2),
            (
                "substr",
                vec![Value::String("abcdef".into()), Value::Number(1.0), bad()],
                3,
            ),
            (
                "grapheme_substr",
                vec![Value::String("abcdef".into()), bad()],
                2,
            ),
            (
                "grapheme_substr",
                vec![Value::String("abcdef".into()), Value::Number(1.0), bad()],
                3,
            ),
            ("repeat", vec![Value::String("x".into()), bad()], 2),
            ("lpad", vec![Value::String("x".into()), bad()], 2),
            ("rpad", vec![Value::String("x".into()), bad()], 2),
            ("lpad_w", vec![Value::String("x".into()), bad()], 2),
            ("rpad_w", vec![Value::String("x".into()), bad()], 2),
            ("word", vec![Value::String("a b".into()), bad()], 2),
            ("range", vec![bad(), Value::Number(2.0)], 1),
            ("range", vec![Value::Number(1.0), bad()], 2),
            (
                "range",
                vec![Value::Number(1.0), Value::Number(2.0), bad()],
                3,
            ),
            ("format_bytes", vec![bad()], 1),
            ("format_number", vec![bad()], 1),
            ("format_number", vec![Value::Number(1234.0), bad()], 2),
            ("word_wrap", vec![Value::String("a b".into()), bad()], 2),
            ("word_wrap_w", vec![Value::String("a b".into()), bad()], 2),
            ("exit", vec![bad()], 1),
        ] {
            assert_type_error(name, args, position);
        }
    }

    #[cfg(feature = "datetime")]
    #[test]
    fn datetime_numeric_arguments_reject_unparseable_values() {
        for name in ["date_format", "duration_format", "relative_time"] {
            assert_type_error(name, vec![bad()], 1);
        }
    }

    #[test]
    fn numeric_strings_still_coerce_for_representative_builtins() {
        let left = call_builtin(
            "left",
            vec![Value::String("abcdef".into()), Value::String("2".into())],
        )
        .unwrap()
        .unwrap();
        assert_eq!(left, Value::String("ab".into()));

        let range = call_builtin(
            "range",
            vec![Value::String("1".into()), Value::String("3".into())],
        )
        .unwrap()
        .unwrap();
        let Value::List(ref items) = range else {
            panic!("range must return a list");
        };
        assert_eq!(
            items.as_slice(),
            &[Value::Number(1.0), Value::Number(2.0), Value::Number(3.0)]
        );

        let formatted = call_builtin(
            "format_number",
            vec![Value::String("1234.5".into()), Value::String("1".into())],
        )
        .unwrap()
        .unwrap();
        assert_eq!(formatted, Value::String("1,234.5".into()));
    }

    #[test]
    fn omitted_numeric_arguments_keep_their_documented_defaults() {
        assert!(matches!(
            call_builtin("exit", vec![]),
            Err(crate::error::MixError::ExitRequest { code: 0 })
        ));
        let formatted = call_builtin("format_number", vec![Value::Number(1234.5)])
            .unwrap()
            .unwrap();
        assert_eq!(formatted, Value::String("1,234".into()));
    }

    #[test]
    fn walk_rejects_an_unparseable_max_depth_instead_of_walking_unlimited() {
        let mut opts = indexmap::IndexMap::new();
        opts.insert("max_depth".into(), bad());
        let err = call_builtin("walk", vec![Value::String(".".into()), Value::map(opts)])
            .expect_err("invalid max_depth must fail before walking");
        let info = err.info().expect("max_depth failure must be structured");
        assert_eq!(info.code, "TYPE_MISMATCH");
        assert_eq!(
            info.message,
            "walk(): argument 2 option max_depth must be a number, got \"2x\" (string)"
        );
    }

    // M-1 put a domain check in front of chrono. The two rejections are
    // different branches and BOTH stay covered: the domain range is
    // +-2^53, chrono's is roughly +-8.2e12 seconds, so a value between
    // them still reaches -- and must still fail -- the chrono check.
    #[cfg(feature = "datetime")]
    #[test]
    fn date_format_rejects_a_timestamp_outside_the_numeric_domain() {
        let err = call_builtin("date_format", vec![Value::Number(1e18)]).unwrap_err();
        let info = err.info().expect("range failure must be structured");
        assert_eq!(info.code, "VALUE_OUT_OF_RANGE");
        assert!(
            info.message.contains("whole timestamp"),
            "msg: {}",
            info.message
        );
    }

    #[cfg(feature = "datetime")]
    #[test]
    fn date_format_rejects_an_unrepresentable_timestamp() {
        // Inside the exact-integer domain, outside what chrono can build.
        let err = call_builtin("date_format", vec![Value::Number(1e14)]).unwrap_err();
        let info = err.info().expect("range failure must be structured");
        assert_eq!(info.code, "VALUE_OUT_OF_RANGE");
        assert!(
            info.message.contains("outside the supported range"),
            "msg: {}",
            info.message
        );
    }
}

#[cfg(all(test, feature = "toml"))]
mod strict_toml_encode_tests {
    use super::call_builtin;
    use crate::value::Value;
    use indexmap::IndexMap;
    use std::cell::RefCell;
    use std::rc::Rc;

    fn nested(key: &str, value: Value) -> Value {
        let mut map = IndexMap::new();
        map.insert(key.to_string(), value);
        Value::map(map)
    }

    fn assert_unrepresentable(value: Value, path: &str, value_type: &str) {
        let err = call_builtin("toml_encode", vec![value])
            .expect_err("unrepresentable TOML value must raise");
        let info = err.info().expect("TOML failure must be structured");
        assert_eq!(info.code, "TOML_UNREPRESENTABLE");
        let Value::Map(details) = &info.details else {
            panic!("TOML error details must be a map: {info:?}");
        };
        assert_eq!(details.get("path"), Some(&Value::String(path.into())));
        assert_eq!(details.get("type"), Some(&Value::String(value_type.into())));
        assert!(info.message.contains(path), "{}", info.message);
        assert!(info.message.contains(value_type), "{}", info.message);
    }

    #[test]
    fn nil_bytes_and_buffer_report_the_exact_path_and_type() {
        assert_unrepresentable(
            nested("outer", Value::list(vec![Value::Bool(true), Value::Nil])),
            "$.outer[1]",
            "nil",
        );
        assert_unrepresentable(
            nested("payload", Value::bytes(vec![1, 2])),
            "$.payload",
            "bytes",
        );
        assert_unrepresentable(
            nested("bad.key", Value::Buffer(Rc::new(RefCell::new(vec![1, 2])))),
            "$[\"bad.key\"]",
            "buffer",
        );
    }

    #[tokio::test]
    async fn function_reports_its_path_and_type() {
        let err = crate::run("$f = fn($x) = $x\ntoml_encode({payload: $f})\n")
            .await
            .expect_err("function has no TOML representation");
        let info = err.info().expect("TOML failure must be structured");
        assert_eq!(info.code, "TOML_UNREPRESENTABLE");
        let Value::Map(details) = &info.details else {
            panic!("TOML error details must be a map: {info:?}");
        };
        assert_eq!(
            details.get("path"),
            Some(&Value::String("$.payload".into()))
        );
        assert_eq!(details.get("type"), Some(&Value::String("function".into())));
    }

    #[test]
    fn representable_values_still_encode() {
        let value = nested(
            "server",
            nested(
                "ports",
                Value::list(vec![Value::Number(25.0), Value::Number(143.0)]),
            ),
        );
        let encoded = call_builtin("toml_encode", vec![value]).unwrap().unwrap();
        let Value::String(ref encoded) = encoded else {
            panic!("toml_encode must return a string");
        };
        assert!(encoded.contains("ports = ["), "{encoded}");
        assert!(encoded.contains("25"), "{encoded}");
        assert!(encoded.contains("143"), "{encoded}");
    }
}
