//! `mix explain MIX-XXXX` — the rustc-style diagnostics explainer.
//!
//! One embedded record per lint code: the code, a one-line summary (what it
//! flags), and the full rationale (why it exists, the shape it catches, the
//! fix). The prose is the authoritative text from `docs/mix/lint.md`, embedded
//! here so an agent hitting a code it has never seen gets the whole story in one
//! `mix explain` call without leaving the terminal.
//!
//! Anti-drift, in two halves: this module's own test greps `analyzer.rs` for
//! every `MIX-####` code it emits and asserts each has a record; the codes
//! assigned in the binary's lint driver (`E1001`–`E1003`, from
//! `cosmix-mix/src/lint.rs`) are covered by that crate's
//! `tests/lint_explain_coverage.rs`. Between them, neither the analyzer nor the
//! lint driver can ship a code with no explanation.

/// One explainable lint code.
pub struct LintDoc {
    /// The stable code, e.g. `MIX-W2305`.
    pub code: &'static str,
    /// One-line "what it flags" — the compact table form.
    pub summary: &'static str,
    /// The full rationale: why the rule exists, the shape it catches, the fix.
    pub detail: &'static str,
}

/// Look up a code (case-insensitively), tolerating a missing `MIX-` prefix so
/// `mix explain W2305` works like `mix explain MIX-W2305`.
pub fn explain(code: &str) -> Option<&'static LintDoc> {
    let c = code.trim();
    LINT_DOCS.iter().find(|d| {
        d.code.eq_ignore_ascii_case(c)
            || d.code
                .strip_prefix("MIX-")
                .is_some_and(|bare| bare.eq_ignore_ascii_case(c))
    })
}

/// Does `s` have a lint-code SHAPE — an optional case-insensitive `MIX-` prefix
/// then a letter and four digits (`MIX-E1101`, `W2305`, `d3013`, even a
/// malformed `MIX-Z9999`)? Used by the CLI to route `mix explain <arg>` to this
/// explainer vs. the builtin explainer; a shape-valid but unknown code lands
/// here so the explainer can say "unknown, here are the codes" rather than
/// sending it to the AI builtin-explainer.
pub fn looks_like_code(s: &str) -> bool {
    let t = s.trim();
    // Strip a case-insensitive `MIX-` prefix (MIX-/mix-/Mix-), matching
    // `explain`'s own case-insensitivity so routing and lookup agree.
    let bare = if t.len() >= 4 && t[..4].eq_ignore_ascii_case("mix-") {
        &t[4..]
    } else {
        t
    };
    let mut chars = bare.chars();
    match chars.next() {
        Some(c) if c.is_ascii_alphabetic() => {
            let rest: String = chars.collect();
            rest.len() == 4 && rest.chars().all(|c| c.is_ascii_digit())
        }
        _ => false,
    }
}

/// All code prefixes, for `mix explain` with no/invalid arg to hint the shape.
pub fn all_codes() -> impl Iterator<Item = &'static str> {
    LINT_DOCS.iter().map(|d| d.code)
}

pub const LINT_DOCS: &[LintDoc] = &[
    // ---- Errors (MIX-E1xxx) ----
    LintDoc {
        code: "MIX-E1001",
        summary: "lexical error",
        detail: "The lexer could not tokenise the source — an unterminated string or heredoc, a stray character, a malformed number. This is a hard error from the layer below the analyzer; `mix --check` reports it too. Fix the token the message points at.",
    },
    LintDoc {
        code: "MIX-E1002",
        summary: "script parse error",
        detail: "The tokens did not form a valid Mix program — a missing `end`/`next`/`done`, an assignment where an expression was expected, a chained assignment. Reported by the parser before semantic analysis runs. The assignment-chain case points its column at the offending `&&`/`||`.",
    },
    LintDoc {
        code: "MIX-E1003",
        summary: "strict-data parse error",
        detail: "The source was recognisably intended as strict data (the bare-key `k: v` form `load_data()` reads) but failed the literal-data grammar — distinct from a broken executable script (MIX-E1002). A valid data file is recognised by CONTENT under any filename; the strict-data suffix is only a tiebreak when neither grammar succeeds.",
    },
    LintDoc {
        code: "MIX-E1101",
        summary: "undefined variable",
        detail: "A `$name` read whose name is bound NOWHERE in its visible universe. Function bodies see their params, their own binders, and everything bound anywhere at file level (Mix has no block scoping and no read-before-assign rule, so lexical order is deliberately ignored). Never flagged: `${name}` interpolation (falls back to the process environment), `$1`-style positionals, the runtime-injected `rc`/`result`/`status`/`event`/`_`, and (conservatively) a name that matches any known callable even if no variable binds it. Fix: assign it, use `env(\"NAME\")` for environment values, or pass `--allow-global NAME`.",
    },
    LintDoc {
        code: "MIX-E1102",
        summary: "undefined function",
        detail: "A bareword call that resolves against nothing: not a builtin, HOF, evaluator special form, `function` definition in the file, the embedded prelude, an `--allow-function` name, or an assigned variable (a bareword call can dispatch to a function-valued variable). Calls inside `address … end` blocks are sends and never flagged; `MethodCall`/`ValueCall` are dynamic dispatch and skipped. A deleted legacy name (e.g. `grep`) gets this AND its MIX-D30xx rename pointer.",
    },
    LintDoc {
        code: "MIX-E1201",
        summary: "builtin arity mismatch",
        detail: "A builtin call outside its documented arity, checked against the structured contract metadata (`mix builtins --json`), including non-contiguous exact-arity sets — `random(1)` is an error, `random()`/`random(min, max)` are not. The contract is the documented surface; some older builtins tolerate surplus arguments at runtime, and lint is deliberately stricter (`mix --strict-arity` makes the runtime agree).",
    },
    LintDoc {
        code: "MIX-E1202",
        summary: "user-function arity mismatch",
        detail: "A call to a `function` defined in the file with the wrong number of arguments, checked against that definition's parameter count. Checked only when the name has exactly ONE definition (an overloaded name is ambiguous), and skipped when a same-named variable exists (the call may dispatch through it).",
    },
    LintDoc {
        code: "MIX-E1301",
        summary: "duplicate function parameter",
        detail: "A `function` (or a `fn(...)` lambda, reported as `<lambda>`) declares the same parameter name twice — the second binding would silently shadow the first. A definition-time error.",
    },
    LintDoc {
        code: "MIX-E1302",
        summary: "duplicate function definition in one scope",
        detail: "Two `function` definitions with the same name in one scope — the later wins and the earlier is dead. A definition-time error (distinct from MIX-W2403, which is about a function whose name collides with a BUILTIN).",
    },
    LintDoc {
        code: "MIX-E1401",
        summary: "require() target missing/unreadable",
        detail: "A `require(\"path\")` with a literal path that does not exist or cannot be read. `require` is the isolated, statically-resolvable module loader, so lint verifies literal-path targets — unlike `source`/`include` (see MIX-W2401).",
    },
    LintDoc {
        code: "MIX-E1402",
        summary: "require() target invalid Mix",
        detail: "A `require(\"path\")` whose literal target exists but does not parse as Mix. Lint parses literal-path modules so a broken dependency is caught at authoring time, not at run time.",
    },
    LintDoc {
        code: "MIX-E1501",
        summary: "dead mutation (write is lost)",
        detail: "A discarded `push`/`pop`/`shift` whose first argument is NOT a bare variable — `push($m[\"a\"], $v)`, `push($m.a, $v)`, `$m[\"a\"].push($v)`. These mutate through the variable slot, so given any other expression they append to a temporary copy and the write is lost in silence. An ERROR, not a warning: the statement does nothing while reading as though it did. The FIX DIFFERS BY BUILTIN: `push` returns the appended list, so assign it back — `$m[\"a\"] = push($m[\"a\"], $v)`. `pop`/`shift` return the REMOVED ELEMENT, not the list, so assigning that back would replace the list with the element (data corruption) — hoist first instead: `$l = $m[$k]; $x = pop($l); $m[$k] = $l`. A by-value parameter is a bare variable, handled by its own dead-push warning, so it is not double-reported.",
    },
    LintDoc {
        code: "MIX-E1502",
        summary: "discarded pure transform",
        detail: "A discarded `delete`/`merge` — both are PURE (they return a new container and change nothing in place), so a bare call is a no-op. Assign it back: `$m = delete($m, \"k\")`.",
    },
    // ---- Warnings (MIX-W2xxx) ----
    LintDoc {
        code: "MIX-W2101",
        summary: "unreachable statement",
        detail: "A statement that can never run because control flow leaves the block before reaching it — code after an unconditional `return`/`break`/`continue`, a `die`, or an `exit()`/`panic()` call in the same straight-line block.",
    },
    LintDoc {
        code: "MIX-W2201",
        summary: "discarded must-use result",
        detail: "An operation whose failure signal lives in its RETURN VALUE (`effects.must_use`: `run_rc`, `run_argv`, `run_pipeline`, `run_parallel`, `ssh_run`, `ssh_exec`, `ssh_mix`, `http_*`, `kill`, `run_stream`) used as a bare expression statement — the bug class where a failed remote step silently vanishes. The last statement of a block is exempt (it may be the block's value). Fix: bind the result and branch on it (check `.ok`/`.rc`/`.exit_code`); some have a fail-fast twin that raises instead (`run_argv`→`run_argv_must`, `run_pipeline`→`run_pipeline_must`, `ssh_run`→`ssh_must`).",
    },
    LintDoc {
        code: "MIX-W2301",
        summary: "`+` stringifies a proven list",
        detail: "`+` coerces lists to strings; it does not append or concatenate list VALUES. Fires for a list literal operand, or a variable proven by straight-line analysis to hold a directly assigned list literal. Use `concat(list_a, list_b)` or `push(list, value)`.",
    },
    LintDoc {
        code: "MIX-W2302",
        summary: "used implicit-nil function result",
        detail: "The result of a uniquely-defined named function is consumed, but its block body's final statement is a bare expression and the body has no value-returning `return` — block functions implicitly return `nil`. Add `return`. Silent for: a discarded call, mixed-return bodies, a terminating final expression, and calls whose name can be redirected through a variable.",
    },
    LintDoc {
        code: "MIX-W2303",
        summary: "assignment operand in hand-built chain AST",
        detail: "Defence-in-depth for Rust embedders that construct the public AST directly and pass it to `analyze()`: warns if any operand of a hand-built `StmtKind::Chain` is an assignment. Ordinary Mix source cannot reach this — the parser rejects the same shape first as MIX-E1002. Reserved for the public-API path, never repurposed.",
    },
    LintDoc {
        code: "MIX-W2304",
        summary: "unknown builtin-result key",
        detail: "A literal field/index key checked against the builtin's documented result-map fields (`mix builtins --json`). Works on a direct builtin call or a variable proven by straight-line assignment to hold that result. The hint names the closest documented key (e.g. `exit_code` rather than `code`). Dynamic keys, generic maps, and result shapes without declared fields stay silent.",
    },
    LintDoc {
        code: "MIX-W2305",
        summary: "-1-sentinel builtin as a truth value",
        detail: "`index_of()` / `byte_index_of()` / `bytes_find()` used BARE as a truth value. They return `-1` for not-found and `0` for found-at-first-position, and Mix treats `0` as falsy and every non-zero number — `-1` included — as truthy. So a bare call in a condition is wrong on BOTH branches: `if index_of(\"abc\", \"z\")` reads absent as present (-1 is truthy); `if index_of(\"abc\", \"a\")` reads found-at-0 as absent (0 is falsy). Compare explicitly (`index_of(..) >= 0`), or use `contains()` for the yes/no question — EXCEPT for `bytes_find`, whose bytes/buffer subject `contains()` rejects, so a `bytes_find` finding takes the `>= 0` comparison, not `contains()`. The 1-based twins `pos`/`lastpos`/`byte_pos`/`byte_lastpos` are safe here (not-found sentinel is `0`, falsy) — that asymmetry is exactly the trap. Fires in `if`/`elif`, `while`, `break if`/`continue if`, expression-position `if`, the ternary condition, and through `not`/`and`/`or`. Any explicit comparison stays silent.",
    },
    LintDoc {
        code: "MIX-W2306",
        summary: "escaped quotes in ssh command source",
        detail: "A literal command passed to `ssh_run`/`ssh_must` whose source spelling contains `\\\"` — the high-signal mark of nested Mix source the remote shell will parse again. Ship the source verbatim with `ssh_mix` + a heredoc. Simple command strings, computed commands, `ssh_exec`, `ssh_mix`, and single-quoted strings containing ordinary `\"` stay quiet.",
    },
    LintDoc {
        code: "MIX-W2401",
        summary: "source/include defeats analysis",
        detail: "One `source`/`include` anywhere disables the undefined-name checks for the whole file (the loaded file can define anything) — reported once so you know analysis is degraded. Prefer `require()`: it is isolated, statically resolvable, and MIX-E1401/E1402 verify literal-path modules parse.",
    },
    LintDoc {
        code: "MIX-W2402",
        summary: "bare bound variable in heredoc",
        detail: "A heredoc literal contains bare `$NAME` where `NAME` is bound somewhere in the same visible universe. Heredocs interpolate `${NAME}`, not `$NAME`, so the bare form often means a generated config was silently corrupted. Does not fire for `${NAME}`, `$(` command substitution, escaped `\\$NAME`, all-digit names like `$1`, unknown names, or ordinary double-quoted strings. Lint-only: bare `$NAME` still evaluates to literal `$NAME`, and intentional literal output needs no change.",
    },
    LintDoc {
        code: "MIX-W2403",
        summary: "function name shadows a builtin",
        detail: "At the DEFINITION of a function whose name is a builtin: the builtin wins at every call site (a builtin-named dot-call even desugars at parse time), so the definition is unreachable by name (only an extracted function value or an exports-map index still reaches it). The worst shape is a script that keeps running while its own function quietly stops being called — every release that adds a builtin name arms it again for older scripts. Deliberately a warning, never an error: a compat shim written for an older mix that lacks the builtin is legitimate, but on the mix doing the linting it is dead, and the author should know.",
    },
    // ---- Deprecations / release-transition advisories (MIX-D3xxx, severity note) ----
    LintDoc {
        code: "MIX-D3001",
        summary: "`regex_match` is pattern-first legacy",
        detail: "One of the five pattern-first legacy regex/grep names — use the subject-first twin `re_match(s, pattern)`. The legacy names were DELETED in release B (0.73.0) after the fleet-wide inventory read zero, so a surviving call also gets MIX-E1102 (undefined function) and fails at runtime; this note stays as the pointer to the replacement. Severity `note` — never gates.",
    },
    LintDoc {
        code: "MIX-D3002",
        summary: "`regex_find` is pattern-first legacy",
        detail: "Pattern-first legacy — use `re_find(s, pattern)`. NOTE: `re_find` returns CODEPOINT offsets where `regex_find` returned byte offsets; adjust offset arithmetic when migrating. Deleted in release B (0.73.0); a surviving call also gets MIX-E1102 and fails at runtime.",
    },
    LintDoc {
        code: "MIX-D3003",
        summary: "`regex_replace` is pattern-first legacy",
        detail: "Pattern-first legacy — use `re_replace(s, pattern, replacement)`. Deleted in release B (0.73.0); a surviving call also gets MIX-E1102 and fails at runtime.",
    },
    LintDoc {
        code: "MIX-D3004",
        summary: "`regex_split` is pattern-first legacy",
        detail: "Pattern-first legacy — use `re_split(s, pattern)`. Deleted in release B (0.73.0); a surviving call also gets MIX-E1102 and fails at runtime.",
    },
    LintDoc {
        code: "MIX-D3005",
        summary: "`grep` is pattern-first legacy",
        detail: "Pattern-first legacy — use `grep_lines(text, pattern)`. Deleted in release B (0.73.0); a surviving call also gets MIX-E1102 and fails at runtime.",
    },
    LintDoc {
        code: "MIX-D3008",
        summary: "`pos` REXX-style needle-first legacy",
        detail: "One of the REXX-style 1-based needle-first search family (`pos lastpos byte_pos byte_lastpos`), declared legacy — with a sharper message when composed as `substr(.., pos(..))` in one expression (the 1-based/0-based off-by-one). These stay notes until their own fleet count reads zero; they are NOT deleted in release B.",
    },
    LintDoc {
        code: "MIX-D3009",
        summary: "`lastpos` REXX-style needle-first legacy",
        detail: "REXX-style 1-based needle-first legacy (see MIX-D3008). Declared legacy, not deleted; stays a note until its fleet count reads zero.",
    },
    LintDoc {
        code: "MIX-D3010",
        summary: "`byte_pos` REXX-style needle-first legacy",
        detail: "REXX-style 1-based needle-first legacy (see MIX-D3008). Declared legacy, not deleted; stays a note until its fleet count reads zero.",
    },
    LintDoc {
        code: "MIX-D3011",
        summary: "`byte_lastpos` REXX-style needle-first legacy",
        detail: "REXX-style 1-based needle-first legacy (see MIX-D3008). Declared legacy, not deleted; stays a note until its fleet count reads zero.",
    },
    LintDoc {
        code: "MIX-D3012",
        summary: "ssh_mix body that could not be analysed",
        detail: "An `ssh_mix` body (its second argument) that lint could not analyse — a non-literal argument (a variable, a concatenation, an interpolated string, a `read_file`), or a literal that does not parse as Mix. Says so explicitly rather than passing silently, because an unreadable body counted as clean is exactly how an inventory reads zero while live sites exist. Ship the remote half as a literal heredoc so lint (and inventories built from it) can see inside.",
    },
    LintDoc {
        code: "MIX-D3013",
        summary: "hand-rolled padding loop",
        detail: "A hand-rolled padding loop — `while len($o) < $n … $o = $o .. \" \"` — pointing at `lpad`/`rpad` (and the display-cell `lpad_w`/`rpad_w`). Four independent sessions wrote this loop while the builtins sat in the binary; the note is the discoverability fix that reaches the author at authoring time. Narrow by design: only a `<`/`<=` comparison of `len`/`length` of the same variable the body self-appends a string literal to.",
    },
];

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn lookup_tolerates_missing_prefix_and_case() {
        assert_eq!(explain("MIX-W2305").unwrap().code, "MIX-W2305");
        assert_eq!(explain("w2305").unwrap().code, "MIX-W2305");
        assert_eq!(explain("mix-e1101").unwrap().code, "MIX-E1101");
        assert!(explain("MIX-Z9999").is_none());
        assert!(explain("length").is_none());
    }

    #[test]
    fn looks_like_code_is_precise() {
        // Shape-valid (routes to the explainer), incl. an unknown-namespace
        // code like X1234/MIX-Z9999 so the explainer can say "unknown".
        for yes in ["MIX-E1101", "W2305", "d3013", "mix-w2403", "Mix-W2305", "X1234", "MIX-Z9999"] {
            assert!(looks_like_code(yes), "{yes} should look like a code");
        }
        // Wrong shape → treated as a builtin name.
        for no in ["length", "E110", "W23055", "MIX-", "run_argv", "12345"] {
            assert!(!looks_like_code(no), "{no} should NOT look like a code");
        }
    }

    #[test]
    fn no_duplicate_codes() {
        let mut seen = std::collections::HashSet::new();
        for d in LINT_DOCS {
            assert!(seen.insert(d.code), "duplicate LintDoc code {}", d.code);
        }
    }

    /// Anti-drift: every `MIX-####` code the analyzer can emit must have a
    /// record here, so a new diagnostic cannot ship without its explanation.
    /// Greps the analyzer source at test-build time. (Lexer/parser codes
    /// E1001–E1003 are covered by explicit records; they are not emitted from
    /// analyzer.rs, so they are added to the expected set here.)
    #[test]
    fn every_analyzer_code_has_a_record() {
        let src = include_str!("analyzer.rs");
        let documented: std::collections::HashSet<&str> =
            LINT_DOCS.iter().map(|d| d.code).collect();
        // Retired codes are intentionally undocumented (permanently spent).
        const RETIRED: &[&str] = &["MIX-D3006", "MIX-D3007"];
        let mut missing = Vec::new();
        let mut i = 0;
        while let Some(pos) = src[i..].find("MIX-") {
            let start = i + pos;
            // Extract MIX-<L><4 digits> if that's the shape here.
            let code: String = src[start..].chars().take(9).collect();
            i = start + 4;
            if code.len() == 9
                && code.as_bytes()[4].is_ascii_uppercase()
                && code.as_bytes()[5..9].iter().all(u8::is_ascii_digit)
                && !documented.contains(code.as_str())
                && !RETIRED.contains(&code.as_str())
            {
                missing.push(code);
            }
        }
        missing.sort();
        missing.dedup();
        assert!(
            missing.is_empty(),
            "analyzer emits codes with no LintDoc record (add them to lint_docs.rs): {missing:?}",
        );
    }
}
