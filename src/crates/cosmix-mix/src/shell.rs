use std::collections::{HashMap, HashSet};
use std::env;
use std::path::Path;

use cosmix_mix::MixError;
use cosmix_mix::ast::Stmt;
use cosmix_mix::lexer::Lexer;
use cosmix_mix::parser::Parser;

use crate::exec;

/// Result of classifying a line of REPL input.
pub enum InputKind {
    MixCode(Vec<Stmt>),
    /// Shell-dispatch source after explicit physical-line continuations have
    /// been spliced. Carry it so callers execute the same logical line the
    /// classifier saw rather than the original physical input.
    ExternalCommand(String),
    /// A bareword invocation of a defined user function — `sc restart nginx`.
    /// The head is a known Mix function, so the line dispatches as
    /// `sc("restart", "nginx")` instead of hitting `/bin/sh`. Carries the
    /// function name and its shell-split string arguments; the caller runs it
    /// via `Evaluator::call_function_by_name_with_args`. This is what gives the
    /// ported bash toolkit (`sc`, `sx`, `f`, `health`, `newpw` …) bash parity:
    /// call them bareword, no parens.
    FunctionCommand {
        name: String,
        args: Vec<String>,
    },
    Empty,
    Incomplete,
    /// Input that was definitively Mix (structurally) but failed to lex/parse.
    /// Carries the real lexer/parser error so the caller surfaces it instead of
    /// masking it as a missing external command (the `print(0755)` footgun: a
    /// glued keyword+paren is not on PATH, so the old shell fallback exec'd it
    /// and reported "No such file or directory" rather than the lex error).
    ParseError(String),
}

/// Mix language keywords that should always be parsed as Mix code.
///
/// `true`/`false`/`nil` are deliberately NOT here: as a line HEAD they are
/// shell commands (`/usr/bin/true`, `/usr/bin/false`), so `false` at the
/// prompt exits 1 like every other shell — forcing them to Mix made a bare
/// `false` evaluate to the literal (exit 0), breaking the universal shell
/// idiom. The LEXER still recognizes them as literals everywhere inside Mix
/// source (`$x = true`, `if $a == nil`); this list only routes line heads.
/// `nil` has no external binary, so it falls through to the Mix parse anyway.
pub const MIX_KEYWORDS: &[&str] = &[
    "if", "for", "while", "loop", "function", "fn", "return", "select", "print", "eprint", "die",
    "try", "parse", "export", "alias", "break", "continue", "send", "address", "emit", "source",
    "sh", "label",
];

/// Shell builtins handled by the REPL.
pub const SHELL_BUILTINS: &[&str] = &[
    "cd", "pushd", "popd", "history", "exit", "which", "type", "unalias", "jobs", "fg", "bg", "mix",
];

/// Classify a line of input: Mix code, external command, or incomplete.
///
/// Convenience wrapper with an empty user-function set, so bareword function
/// dispatch is disabled. Used by the classifier's own unit/property tests and
/// any caller without a live evaluator; the interactive REPL and `-c` paths
/// call [`classify_input_fns`] with `Evaluator::function_names()`.
#[cfg_attr(not(test), allow(dead_code))]
pub fn classify_input(line: &str, aliases: &HashMap<String, String>) -> InputKind {
    classify_input_fns(line, aliases, &HashSet::new())
}

/// Classify a line, honouring `functions` — the names of user-defined Mix
/// functions currently in scope — for bash-style bareword dispatch. A line
/// whose head is a defined function and whose body is a simple command (plain
/// word args, no pipes/redirects/chains) routes to [`InputKind::FunctionCommand`]
/// rather than `/bin/sh`, so `sc restart nginx` runs `sc("restart", "nginx")`.
/// The function check sits AFTER Mix keywords and shell builtins but BEFORE the
/// PATH probe, so a defined function shadows a same-named PATH binary — matching
/// bash's alias → keyword → builtin → function → PATH resolution order.
pub fn classify_input_fns(
    line: &str,
    aliases: &HashMap<String, String>,
    functions: &HashSet<String>,
) -> InputKind {
    let logical = match cosmix_mix::continuation::splice_explicit_continuations(line) {
        Ok(source) => source,
        Err(error) if error.is_incomplete_input() => return InputKind::Incomplete,
        Err(error) => return InputKind::ParseError(error.to_string()),
    };
    let trimmed = logical.trim();

    // EVERY line must be blank-or-comment, not just the first. `starts_with`
    // is right for a REPL line — the input IS one line — but this classifier
    // also receives whole `-c` programs, and a program whose FIRST line is a
    // comment was classified Empty and silently discarded with exit 0:
    //
    //     mix -c '-- set up
    //     print("RAN")'      ->  no output, exit 0
    //
    // Comments are the normal way to open a generated script, so this hit the
    // machine-authored case squarely. Silent discard reporting success is the
    // worst failure shape available: a script that never ran looks exactly like
    // one that ran and did nothing.
    let all_comment_or_blank = trimmed.lines().all(|l| {
        let t = l.trim();
        t.is_empty() || t.starts_with('#') || t.starts_with("--")
    });
    if trimmed.is_empty() || all_comment_or_blank {
        return InputKind::Empty;
    }

    // Expand aliases on the first word
    let expanded = expand_alias(trimmed, aliases);
    let work = expanded.trim();

    // NOTE: `&&`/`||`/`;` are NOT used to force the shell path here. Mix's own
    // grammar has `&&`/`||` statement chaining (Stmt::Chain), so a Mix-keyword
    // head like `send … && print …` must stay Mix. The first-word routing
    // below decides: an external/aliased head with a chain (e.g. the Debian
    // `u` = `sudo … && …`) lands on the external path, which parses the chain
    // via exec::parse_command_list; a Mix head uses Mix's native chaining.

    // Lines starting with $ are Mix code (with shell-chain fallback).
    if work.starts_with('$') {
        return try_mix_then_external(work);
    }

    // Skip leading KEY=VALUE env prefixes to find the actual command
    let first_word = work
        .split_whitespace()
        .find(|w| {
            if let Some(eq) = w.find('=') {
                let key = &w[..eq];
                key.is_empty() || !key.chars().all(|c| c.is_ascii_alphanumeric() || c == '_')
            } else {
                true
            }
        })
        .unwrap_or("");

    // Mix keywords → Mix (with shell-chain fallback for `false || cmd` etc.)
    if MIX_KEYWORDS.contains(&first_word) {
        return try_mix_then_external(work);
    }

    // A definitive assignment-chain error means the parser SAW a real Mix
    // assignment used as a `&&`/`||` operand. Once it has, no head shape may
    // reclassify the line as a shell chain. `true && $x = false || cmd` is the
    // case that forces this: `true` is BOTH a PATH binary and a valid Mix
    // literal, so the line parses far enough to type the error, yet the
    // shell-first returns below fired first and ran `cmd` at exit 0 with the
    // assignment silently gone. Every return below this point precedes any
    // parse, so this is the only place the typed error can win.
    //
    // The `&&`/`||` prefilter is a fast path, not a heuristic: the error is
    // only ever constructed at the chain production, so a line containing
    // neither operator cannot produce it. A head Mix genuinely cannot parse
    // (`/usr/bin/true`, a redirect, an env prefix) still fails earlier with a
    // different error and keeps its shell reading.
    // The probe is speculative — its `Ok` is discarded and the line may still
    // be shell — so it must not emit parser diagnostics of its own.
    if (work.contains("&&") || work.contains("||"))
        && let Err(error) = probe_parse_mix_result(work)
        && error.is_assignment_chain_parse_error()
    {
        return InputKind::ParseError(error.to_string());
    }

    // Shell builtins → external command path
    if SHELL_BUILTINS.contains(&first_word) {
        match exec::parse_command_list(work, &exec::NoVars) {
            Ok(_) => return InputKind::ExternalCommand(work.to_string()),
            Err(_) => return try_parse_mix(work),
        }
    }

    // Bash-style bareword function dispatch. A defined Mix function invoked as
    // a bare command (`sc restart nginx`) routes to Mix as `sc("restart",
    // "nginx")` — the ported bash toolkit works with no parens. Placed BEFORE
    // the PATH probe so a function shadows a same-named binary (bash order:
    // alias → keyword → builtin → function → PATH). Only a *simple* command
    // qualifies: the head must equal `first_word` (no `KEY=val` env prefix) and
    // the args must split cleanly with no shell metacharacters — anything with
    // pipes/redirects/chains/substitution falls through to the shell path.
    if functions.contains(first_word)
        && let Some(args) = split_function_args(work, first_word)
    {
        return InputKind::FunctionCommand {
            name: first_word.to_string(),
            args,
        };
    }

    // A tight hyphen in a command-shaped statement head belongs to the shell,
    // not the Mix subtraction parser. Without this pre-parse discriminator,
    // `cosmix-comp --nested` is the valid Mix expression `"cosmix" - "comp"`
    // (the option is then a comment) and dies with a numeric-conversion error.
    // Keep the rule deliberately head-only: spaced subtraction and
    // `$`/numeric/expression-led lines all remain Mix. Barewords never resolve
    // to live `$` variables, so classifier state must not affect this decision.
    if is_tight_hyphenated_command_head(first_word) {
        match exec::parse_command_list(work, &exec::NoVars) {
            Ok(_) => return InputKind::ExternalCommand(work.to_string()),
            Err(e) => return InputKind::ParseError(e.to_string()),
        }
    }

    // Shell-first: if first word is found on PATH or is a path itself,
    // treat as external command (design principle #8)
    if is_external_command(first_word) {
        match exec::parse_command_list(work, &exec::NoVars) {
            Ok(_) => return InputKind::ExternalCommand(work.to_string()),
            // The head is a real program but the command line is malformed
            // (e.g. a bad redirect). Surface the shell tokenizer error.
            Err(e) => return InputKind::ParseError(e.to_string()),
        }
    }

    // Not on PATH — try parsing as Mix (function call, expression, etc.)
    match try_parse_mix_result(work) {
        Ok(stmts) => {
            // Preserve the shell-first meaning of an unknown command-like
            // semicolon list. With executable-Mix `;`, `zqxfoo; zqxbar`
            // otherwise becomes two valid discarded String expressions and
            // silently flips from two command attempts (exit 127) to Mix
            // success. `$`/keyword/call heads have already routed elsewhere;
            // this guard owns only the command-like last-resort branch.
            if is_command_like_semicolon_list(work, first_word) {
                InputKind::ExternalCommand(work.to_string())
            } else {
                InputKind::MixCode(stmts)
            }
        }
        Err(error) => {
            let msg = error.to_string();
            let typed_incomplete = error.is_incomplete_input();
            // Preserve the pre-existing legacy block/string accumulator at
            // its original priority. The typed concat signal is newer and
            // must still pass through the command-like tie-break below:
            // `zqxfoo ..` was an attempted shell command before 0.56.0.
            if !typed_incomplete && is_incomplete_error(&error) {
                return InputKind::Incomplete;
            }
            // Assignment-led `&&`/`||` is a definitive Mix error, and the
            // parser has already told us the line contains a real Mix
            // assignment. No head shape may reclassify it as a shell chain:
            // that is precisely the silent false-green (`nil && $x = false ||
            // cmd` running `cmd` and exiting 0) the parser rejection exists to
            // kill. Must precede the command-like tie-break below.
            if error.is_assignment_chain_parse_error() {
                return InputKind::ParseError(msg);
            }
            // Mix parse failed definitively. Tie-break: a genuinely
            // command-like head (`gti status`, `./build`) keeps the shell path
            // so it reports the familiar "command not found"; a Mix-expression
            // head — a number (`1 +`), a lexer failure (`0755`, `print(0755)`),
            // an operator/paren — means the line was meant as Mix, so surface
            // its real lex/parse error instead of exec'ing a missing binary.
            if head_is_command_like(work, first_word)
                && exec::parse_command_list(work, &exec::NoVars).is_ok()
            {
                return InputKind::ExternalCommand(work.to_string());
            }
            if typed_incomplete {
                return InputKind::Incomplete;
            }
            InputKind::ParseError(msg)
        }
    }
}

/// Whether the first semantic word is a tight-hyphenated command head.
///
/// The accepted shape is intentionally narrower than a general Unix filename:
/// it starts with the same ASCII letter/underscore rule as a Mix bareword, has
/// at least one `-`, ends in an alphanumeric/underscore, and otherwise contains
/// only alphanumerics, underscores, and hyphens. Other command punctuation
/// already reaches the existing failed-parse command tie-break; excluding it
/// here prevents this successful-parse override leaking into calls, indexing,
/// field access, assignments, or parenthesised expressions.
fn is_tight_hyphenated_command_head(first_word: &str) -> bool {
    let Some(first) = first_word.chars().next() else {
        return false;
    };
    (first.is_ascii_alphabetic() || first == '_')
        && first_word.contains('-')
        && first_word
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-')
        && first_word
            .chars()
            .last()
            .is_some_and(|c| c.is_ascii_alphanumeric() || c == '_')
}

/// Split a simple bareword function command into its string args, or `None` if
/// the line isn't a plain command the function-call ABI can take directly.
///
/// `work` is the whole (alias-expanded) line; `head` is the verified first word
/// (the function name). Returns the args AFTER the head. Bails to `None` — so
/// the caller falls through to normal shell/Mix classification — when:
///   - the head isn't literally the first token (a `KEY=val` env prefix), or
///   - an UNQUOTED shell metacharacter appears (`| & ; < > ( )`, a backtick, or
///     a `$(` substitution) — those mean a pipeline/redirect/subshell, which is
///     the shell's job, not a simple function call, or
///   - a quote is left unbalanced.
///
/// Quotes (`'…'`, `"…"`) group and are stripped; a backslash inside `"…"`
/// escapes the next char. NO variable or glob expansion happens — bareword
/// function args are literal (use the paren call form, `sc("restart", $svc)`,
/// when you need interpolation).
fn split_function_args(work: &str, head: &str) -> Option<Vec<String>> {
    #[derive(PartialEq)]
    enum Quote {
        None,
        Single,
        Double,
    }

    let mut words: Vec<String> = Vec::new();
    let mut cur = String::new();
    let mut in_word = false;
    let mut quote = Quote::None;
    let mut chars = work.chars().peekable();

    while let Some(c) = chars.next() {
        match quote {
            Quote::None => match c {
                ' ' | '\t' => {
                    if in_word {
                        words.push(std::mem::take(&mut cur));
                        in_word = false;
                    }
                }
                '\'' => {
                    quote = Quote::Single;
                    in_word = true;
                }
                '"' => {
                    quote = Quote::Double;
                    in_word = true;
                }
                '|' | '&' | ';' | '<' | '>' | '(' | ')' | '`' => return None,
                '$' if chars.peek() == Some(&'(') => return None,
                _ => {
                    cur.push(c);
                    in_word = true;
                }
            },
            Quote::Single => match c {
                '\'' => quote = Quote::None,
                _ => cur.push(c),
            },
            Quote::Double => match c {
                '"' => quote = Quote::None,
                '\\' => {
                    if let Some(n) = chars.next() {
                        cur.push(n);
                    }
                }
                '`' => return None,
                '$' if chars.peek() == Some(&'(') => return None,
                _ => cur.push(c),
            },
        }
    }

    if quote != Quote::None {
        return None; // unbalanced quote
    }
    if in_word {
        words.push(cur);
    }

    // The head must be the literal first token (no `KEY=val` env prefix);
    // drop it and return the remaining args.
    match words.split_first() {
        Some((first, rest)) if first.as_str() == head => Some(rest.to_vec()),
        _ => None,
    }
}

/// Whether the head of a line that FAILED to parse as Mix is genuinely a shell
/// command target rather than a Mix expression. Used only as the tie-break that
/// decides between surfacing the real Mix lex/parse error and reporting "command
/// not found". Decided from the HEAD's leading shape + call/index markers, so a
/// later non-Mix character (`foo@bar`) or a lexer-poisoning argument (`gti 0755`)
/// cannot flip the verdict — only `first_word` is examined.
///
/// - A path PREFIX (`/`, `./`, `../`, `~/`) is always a command target.
/// - A Mix call/index opener glued to OR following the head (`foo(…`, `foo (…`,
///   `foo[…`, `foo […`) is a Mix expression, so its parse error surfaces.
/// - A bareword-led head (letter/`_` start: `gti`, `foo:bar`, `foo@bar`,
///   `bin/sh`, `script.sh`) is a command word. `Dot` is deliberately fine: a
///   bare `foo.bar` is valid Mix field access that parses as MixCode and never
///   reaches this tie-break, while dotted command/file names that DON'T parse
///   (`script.sh` — `sh` is a keyword, `python3.11`) keep reporting cmd-not-found.
/// - A digit-led head is arithmetic (`0755`, `1 +`, `1/0`) → Mix, EXCEPT a
///   relative path with a numeric leading directory and a bareword segment
///   (`2026/tool`), which is a command target.
/// - Any other lead (operator, quote, `$`, …) is a Mix expression.
///
/// Keyword heads never reach here — they route through `try_mix_then_external`.
fn head_is_command_like(work: &str, first_word: &str) -> bool {
    if first_word.starts_with('/')
        || first_word.starts_with("./")
        || first_word.starts_with("../")
        || first_word.starts_with("~/")
    {
        return true;
    }
    if head_starts_call_or_index(work, first_word) {
        return false;
    }
    match first_word.chars().next() {
        // Bareword/identifier-led: a command word (matches the lexer's
        // identifier rule, `is_ascii_alphabetic() || '_'`).
        Some(c) if c.is_ascii_alphabetic() || c == '_' => true,
        // Digit-led: a command only as a slash path with a bareword segment
        // (`2026/tool`); pure-numeric shapes (`0755`, `1/0`, `1/`) are Mix.
        Some(c) if c.is_ascii_digit() => {
            first_word.contains('/') && has_nonnumeric_path_segment(first_word)
        }
        // Operator / quote / `$` / other non-bareword lead: a Mix expression.
        _ => false,
    }
}

/// Whether a last-resort, non-PATH command-like head contains a top-level,
/// unquoted shell semicolon. Such a line was a shell command list before Mix
/// gained `;`; keep it on that path instead of letting individually-valid
/// bareword String expressions turn it into a silent Mix no-op.
///
/// `split_on_control_ops` is the shell parser's quote / command-substitution
/// aware scanner. `Connector::Always` after the first piece uniquely denotes
/// `;` (`&&`/`||` produce And/Or), so this guard does not change today's bare
/// word or `&&`/`||` classifications.
pub(crate) fn is_command_like_semicolon_list(work: &str, first_word: &str) -> bool {
    if !head_is_command_like(work, first_word) {
        return false;
    }
    exec::split_on_control_ops(work)
        .iter()
        .skip(1)
        .any(|(connector, _)| matches!(connector, exec::Connector::Always))
}

/// Whether a Mix call/index opener is glued to or follows the head — `foo(`,
/// `foo (`, `foo[`, `foo [`. A `(`/`[` in `first_word` is glued; otherwise the
/// next whitespace-delimited word after the head opening with `(`/`[` is a
/// spaced call/index. Matched on whole words, so an env-prefix value equal to
/// the head (`FOO=foo foo (1,)`) cannot be mistaken for the head itself.
fn head_starts_call_or_index(work: &str, first_word: &str) -> bool {
    if first_word.contains('(') || first_word.contains('[') {
        return true;
    }
    work.split_whitespace()
        .skip_while(|w| *w != first_word)
        .nth(1)
        .is_some_and(|w| w.starts_with('(') || w.starts_with('['))
}

/// Whether a slash-containing head has at least one non-numeric path segment —
/// the signal that distinguishes a path (`2026/tool`) from pure arithmetic
/// (`1/0`, `1/`, `2026/2027`).
fn has_nonnumeric_path_segment(word: &str) -> bool {
    word.split('/')
        .any(|seg| !seg.is_empty() && !seg.chars().all(|c| c.is_ascii_digit()))
}

/// Check if a command name can be found on PATH or is an absolute/relative path.
pub fn is_external_command(name: &str) -> bool {
    // Absolute or relative paths
    if name.contains('/') {
        return Path::new(name).exists();
    }

    // Search PATH
    let path_var = env::var("PATH").unwrap_or_default();
    for dir in path_var.split(':') {
        let full = format!("{}/{}", dir, name);
        if Path::new(&full).is_file() {
            return true;
        }
    }
    false
}

fn try_parse_mix(input: &str) -> InputKind {
    match try_parse_mix_result(input) {
        Ok(stmts) => InputKind::MixCode(stmts),
        Err(error) => {
            let msg = error.to_string();
            if is_incomplete_error(&error) {
                InputKind::Incomplete
            } else {
                InputKind::ParseError(msg)
            }
        }
    }
}

/// Mix-first, shell-fallback for a head that looks like Mix (a keyword or a
/// `$`-line). Mix has native `&&`/`||` chaining, so a valid Mix chain
/// (`send … && print …`) stays Mix. But a keyword/`$` head that FAILS Mix
/// parse AND is structurally a multi-command shell list (top-level `&&`/`||`/
/// `;`) is a shell chain (e.g. `false || cmd`, `true && cmd`) — route it to
/// the external path. A non-chain Mix parse failure is a real Mix error.
fn try_mix_then_external(work: &str) -> InputKind {
    match try_parse_mix_result(work) {
        Ok(stmts) => InputKind::MixCode(stmts),
        Err(error) => {
            let msg = error.to_string();
            if is_incomplete_error(&error) {
                return InputKind::Incomplete;
            }
            // Assignment-led `&&`/`||` is a definitive Mix error. Letting the
            // normal command-list fallback see it would recreate the exact
            // false-green bug the parser rejection prevents under `mix -c`.
            if error.is_assignment_chain_parse_error() {
                return InputKind::ParseError(msg);
            }
            if let Ok(list) = exec::parse_command_list(work, &exec::NoVars)
                && list.len() > 1
            {
                return InputKind::ExternalCommand(work.to_string());
            }
            InputKind::ParseError(msg)
        }
    }
}

fn try_parse_mix_result(input: &str) -> Result<Vec<Stmt>, MixError> {
    let mut lexer = Lexer::new(input);
    let tokens = lexer.tokenize()?;
    let mut parser = Parser::new(tokens, input);
    parser.parse_program()
}

/// Speculative twin of `try_parse_mix_result` for classifier probes, whose
/// `Ok` result is discarded. Silences parser diagnostics: a probe runs before
/// the line is known to be Mix at all, so its warnings would land on lines
/// that then run as shell — and land twice on lines that really are Mix and
/// get reparsed for execution.
fn probe_parse_mix_result(input: &str) -> Result<Vec<Stmt>, MixError> {
    let mut lexer = Lexer::new(input);
    let tokens = lexer.tokenize()?;
    let mut parser = Parser::new_speculative(tokens, input);
    parser.parse_program()
}

pub(crate) fn is_incomplete_error(error: &MixError) -> bool {
    if error.is_incomplete_input() {
        return true;
    }

    // Compatibility for the pre-existing block/string REPL accumulator
    // contract. New parser productions must use IncompleteInput above rather
    // than extending this legacy diagnostic matcher.
    let msg = error.to_string();
    msg.contains("expected End")
        || msg.contains("expected Next")
        || msg.contains("expected Done")
        || msg.contains("expected Catch")
        || msg.contains("unterminated")
}

/// Expand aliases on the first word of the line (recursive, depth-limited).
///
/// The tail is sliced from the first word's REAL byte offset (leading
/// whitespace skipped). The old `result[first_word.len()..]` sliced from the
/// string start, which duplicated the head on a whitespace-led line
/// (`"  ll" → "ls -l ll"`) and PANICKED mid-char when the skipped prefix put
/// the fixed index inside a multibyte first word (`" é"`).
pub fn expand_alias(line: &str, aliases: &HashMap<String, String>) -> String {
    let mut result = line.to_string();
    for _ in 0..10 {
        let head_start = result.len() - result.trim_start().len();
        let first_word = result[head_start..].split_whitespace().next().unwrap_or("");
        if let Some(expansion) = aliases.get(first_word) {
            let rest = result[head_start + first_word.len()..].to_string();
            result = format!("{}{}", expansion, rest);
        } else {
            break;
        }
    }
    result
}

/// Strip one leading whitespace-delimited word `word` from `s`.
///
/// `Some(remainder)` when `s`'s first word is EXACTLY `word` (`Some("")` when
/// `s` is exactly `word`); `None` otherwise. The exactness matters: it is what
/// keeps `timeout 5 foo` and `time()` off the modifier path — a glued suffix
/// means the head is a different word, not `word` with an argument.
fn strip_word<'a>(s: &'a str, word: &str) -> Option<&'a str> {
    let rest = s.trim().strip_prefix(word)?;
    let Some(next) = rest.chars().next() else {
        return Some(""); // `s` is exactly `word`
    };
    if next.is_whitespace() {
        return Some(rest.trim());
    }
    // A shell metacharacter also ENDS the head — `time;` / `time&` / `time|` /
    // `time>/dev/null` are a bare `time` plus a control or redirection operator,
    // not a command called `time;`. They have nothing to run, so they report usage
    // like a bare `time` rather than hunting PATH for a `time` binary that does
    // not exist. (`(` is NOT in this set: `time()` is a call to the time()
    // builtin, which must keep parsing as Mix.)
    if matches!(next, ';' | '&' | '|' | '<' | '>') {
        return Some("");
    }
    // Anything else glued to the head is a DIFFERENT word (`timeout`, `time()`).
    None
}

/// Strip a leading `time` modifier, returning the line it wraps.
///
/// `time` is a shell KEYWORD in bash, not a program — which is why `time cmd`
/// used to die with `time: No such file or directory` here: the classifier saw
/// an unknown head, probed PATH, and found nothing (GNU `/usr/bin/time` is a
/// different, usually-absent tool that could not time a Mix expression or a
/// bareword Mix function anyway). Mix models it the same way bash does: a
/// modifier resolved BEFORE dispatch, wrapping whatever the rest of the line
/// turns out to be — external command, pipeline, chain, bareword function call,
/// or Mix code.
///
/// `Some("")` means a bare `time` with nothing to run; the caller reports usage
/// rather than dispatching. Callers must strip BEFORE alias expansion so the
/// wrapped head still expands (`time ll` → `time ls -l`).
///
/// `mix time EXPR` — the REPL meta-command spelling — is accepted as the same
/// modifier, so there is one timing semantic rather than two that disagree on
/// whether a shell command can be timed.
pub fn strip_time_prefix(line: &str) -> Option<&str> {
    if let Some(rest) = strip_word(line, "mix")
        && let Some(inner) = strip_word(rest, "time")
    {
        return Some(inner);
    }
    strip_word(line, "time")
}

/// Render an elapsed duration for the `time` modifier.
///
/// Sub-second stays in milliseconds — the `Elapsed: 0.012ms` shape `mix time`
/// has always printed, and the resolution that matters when timing a Mix
/// expression. Longer runs promote to seconds/minutes so that timing a real
/// command does not read as `Elapsed: 65000.000ms`.
///
/// Each unit is ROUNDED TO ITS PRINTED PRECISION BEFORE the promotion test.
/// Choosing the unit from the raw value instead lets a duration that rounds UP
/// across the boundary print in the unit it just left: 999.9999ms would render
/// `Elapsed: 1000.000ms`, and 59.9999s `Elapsed: 60.000s`.
pub fn format_elapsed(d: std::time::Duration) -> String {
    // Round to 3 decimal places in the candidate unit, then test the boundary.
    let round3 = |v: f64| (v * 1000.0).round() / 1000.0;

    let ms = round3(d.as_secs_f64() * 1000.0);
    if ms < 1000.0 {
        return format!("Elapsed: {:.3}ms", ms);
    }

    let secs = round3(d.as_secs_f64());
    if secs < 60.0 {
        return format!("Elapsed: {:.3}s", secs);
    }

    let mins = (secs / 60.0).floor();
    format!("Elapsed: {}m{:.3}s", mins as u64, secs - mins * 60.0)
}

/// Prints the `time` modifier's elapsed line when the timed line finishes,
/// whichever way it exits — `None` = the line was not timed (no output).
///
/// A drop guard rather than a print after the dispatch `match`, because BOTH
/// dispatch sites leave that match early on paths that really did run the
/// command: `-c` returns straight out of the lone-pipeline arm (`return match
/// exec::execute_pipeline(…)` — the `time shwho` case itself), and the REPL
/// `continue`s out of the chain, `cd`, and job-control arms. A trailing print
/// silently skips exactly those. Like bash, it also reports a line that FAILED,
/// so a command-not-found still gets an elapsed.
///
/// Not fired when the process is REPLACED or torn down mid-line — Mix's `exit()`
/// builtin calls `process::exit()`, and `mix build`/claude-resume `exec()` into a
/// new binary; neither unwinds, so no destructor runs. Timing a line that never
/// returns has nothing to report to.
pub struct TimeGuard(Option<std::time::Instant>);

impl TimeGuard {
    /// Arm the guard iff `timed` — `TimeGuard::armed(false)` is a silent no-op,
    /// so callers can construct one unconditionally.
    pub fn armed(timed: bool) -> Self {
        TimeGuard(timed.then(std::time::Instant::now))
    }

    /// Cancel the report: the line finished without producing a duration worth
    /// printing. Used for a BACKGROUNDED command (`time sleep 5 &`), where the
    /// dispatch returns as soon as the child is spawned — reporting there would
    /// print the spawn latency (a fraction of a millisecond) as if it were the
    /// command's runtime, which is worse than printing nothing.
    pub fn disarm(&mut self) {
        self.0 = None;
    }
}

impl Drop for TimeGuard {
    fn drop(&mut self) {
        if let Some(start) = self.0 {
            eprintln!("{}", format_elapsed(start.elapsed()));
        }
    }
}

#[cfg(test)]
mod time_modifier_tests {
    use super::*;
    use std::time::Duration;

    #[test]
    fn strips_time_from_an_external_command() {
        assert_eq!(
            strip_time_prefix("time shwho example.org"),
            Some("shwho example.org")
        );
    }

    #[test]
    fn strips_time_from_mix_code_and_pipelines() {
        assert_eq!(
            strip_time_prefix("time sum([1, 2, 3])"),
            Some("sum([1, 2, 3])")
        );
        assert_eq!(strip_time_prefix("time ls | wc -l"), Some("ls | wc -l"));
        assert_eq!(strip_time_prefix("  time   ls -la  "), Some("ls -la"));
    }

    /// The REPL meta spelling collapses onto the same modifier.
    #[test]
    fn strips_the_mix_time_meta_spelling() {
        assert_eq!(
            strip_time_prefix("mix time sum([1, 2])"),
            Some("sum([1, 2])")
        );
        assert_eq!(strip_time_prefix("mix time shwho"), Some("shwho"));
    }

    /// A bare `time` has nothing to wrap — the caller prints usage.
    #[test]
    fn bare_time_reports_empty_remainder() {
        assert_eq!(strip_time_prefix("time"), Some(""));
        assert_eq!(strip_time_prefix("  time  "), Some(""));
        assert_eq!(strip_time_prefix("mix time"), Some(""));
    }

    /// The head must be exactly `time`: a glued suffix is a different command,
    /// and `time()` stays a call to the `time()` builtin.
    #[test]
    fn does_not_strip_a_different_head() {
        assert_eq!(strip_time_prefix("timeout 5 curl example.org"), None);
        assert_eq!(strip_time_prefix("time()"), None);
        assert_eq!(strip_time_prefix("$t = time()"), None);
        assert_eq!(strip_time_prefix("print(time())"), None);
        assert_eq!(strip_time_prefix(""), None);
    }

    /// A shell metacharacter ends the head too, so `time;` is a bare `time`
    /// (→ usage) rather than a hunt for a `time;` binary on PATH.
    #[test]
    fn a_metacharacter_ends_the_head() {
        assert_eq!(strip_time_prefix("time; /bin/true"), Some(""));
        assert_eq!(strip_time_prefix("time&"), Some(""));
        assert_eq!(strip_time_prefix("time| wc -l"), Some(""));
        // Redirections end the head too.
        assert_eq!(strip_time_prefix("time>/dev/null"), Some(""));
        assert_eq!(strip_time_prefix("time</dev/null"), Some(""));
        // …but a paren does NOT: `time()` stays a call to the time() builtin.
        assert_eq!(strip_time_prefix("time()"), None);
    }

    /// Other `mix` meta-commands keep their own dispatch.
    #[test]
    fn does_not_strip_other_mix_metacommands() {
        assert_eq!(strip_time_prefix("mix"), None);
        assert_eq!(strip_time_prefix("mix status"), None);
        assert_eq!(strip_time_prefix("mix trace on"), None);
    }

    #[test]
    fn formats_elapsed_by_magnitude() {
        assert_eq!(
            format_elapsed(Duration::from_micros(12)),
            "Elapsed: 0.012ms"
        );
        assert_eq!(
            format_elapsed(Duration::from_millis(412)),
            "Elapsed: 412.000ms"
        );
        assert_eq!(
            format_elapsed(Duration::from_millis(1500)),
            "Elapsed: 1.500s"
        );
        assert_eq!(format_elapsed(Duration::from_secs(65)), "Elapsed: 1m5.000s");
    }

    /// A duration that ROUNDS UP across a unit boundary must promote, not print
    /// in the unit it just left (`Elapsed: 1000.000ms` / `Elapsed: 60.000s`).
    #[test]
    fn promotes_a_duration_that_rounds_up_across_the_boundary() {
        assert_eq!(
            format_elapsed(Duration::from_nanos(999_999_999)),
            "Elapsed: 1.000s"
        );
        assert_eq!(
            format_elapsed(Duration::from_nanos(59_999_999_999)),
            "Elapsed: 1m0.000s"
        );
        // Just BELOW the rounding boundary stays in the smaller unit.
        assert_eq!(
            format_elapsed(Duration::from_micros(999_499)),
            "Elapsed: 999.499ms"
        );
    }
}

// P1 of the mix tokenizer fuzz/property corpus
// (_doc/planned/mix-tokenizer-fuzz-corpus.md in the cosmix hub): the REPL
// shell-vs-Mix classifier. It branches across PATH probes, alias expansion, and
// Mix-vs-shell parse attempts, so the meaningful properties are that it never
// panics and that its classification DECISION is deterministic.
#[cfg(test)]
mod prop_tests {
    use super::{InputKind, classify_input};
    use proptest::prelude::*;
    use std::collections::HashMap;

    /// Variant tag of an `InputKind` (it carries a non-`PartialEq` `Vec<Stmt>`,
    /// so compare the discriminant — the classification decision — not contents).
    fn disc(k: &InputKind) -> std::mem::Discriminant<InputKind> {
        std::mem::discriminant(k)
    }

    /// Asserts a Mix-expression input that fails to parse surfaces its real
    /// lex/parse error rather than masking it as "No such file"/command-not-found.
    fn assert_surfaces_mix_error(input: &str, expect_substr: &str) {
        match classify_input(input, &HashMap::new()) {
            InputKind::ParseError(msg) => {
                assert!(
                    msg.contains(expect_substr),
                    "{input:?}: expected real Mix error containing {expect_substr:?}, got: {msg}"
                );
                assert!(
                    !msg.contains("No such file"),
                    "{input:?}: must not be the masking shell error: {msg}"
                );
            }
            other => panic!(
                "{input:?} should classify as ParseError, got {:?}",
                disc(&other)
            ),
        }
    }

    /// The reported footgun plus the residual cases Codex flagged: a glued
    /// keyword+paren (`print(0755)`, a lexer failure), a bare bad number
    /// (`0755`), and a number-led broken expression (`1 +`) all previously
    /// masked the real Mix error as command-not-found. All must surface it now.
    #[test]
    fn mix_expression_lex_parse_errors_are_surfaced_not_masked() {
        assert_surfaces_mix_error("print(0755)", "leading-zero");
        assert_surfaces_mix_error("0755", "leading-zero");
        assert_surfaces_mix_error("1 +", "");
    }

    /// A genuinely command-like head that fails Mix parse keeps the shell path
    /// so it still reports "command not found" — the tie-break must not
    /// over-trigger. `gti status` (two barewords), `foo:bar` (a `:`-bearing
    /// head), `gti 0755` (a lexer-poisoning ARGUMENT) and `foo@bar` (a non-Mix
    /// char IN the head) all stay external. (`zqx…` is unlikely on a test PATH.)
    #[test]
    fn command_like_heads_stay_external() {
        for line in [
            "zqx-no-such-cmd some args",
            "zqxfoo:zqxbar",
            "zqxcmd 0755",
            "zqxfoo@zqxbar",
        ] {
            assert!(
                matches!(
                    classify_input(line, &HashMap::new()),
                    InputKind::ExternalCommand(_)
                ),
                "{line:?} should stay ExternalCommand (head is a bareword)"
            );
        }
    }

    #[test]
    fn unknown_command_like_semicolon_lists_stay_external() {
        for line in ["zqxfoo; zqxbar", "zqxfoo;", "zqxfoo;; zqxbar"] {
            assert!(
                matches!(
                    classify_input(line, &HashMap::new()),
                    InputKind::ExternalCommand(_)
                ),
                "{line:?} was a shell list before Mix gained `;`"
            );
        }
        // Do not broaden the guard to other control operators: these are
        // pre-existing Mix string-expression no-ops and stay byte-compatible.
        assert!(matches!(
            classify_input("zqxfoo && zqxbar", &HashMap::new()),
            InputKind::MixCode(_)
        ));
    }

    #[test]
    fn sigil_and_call_semicolon_sequences_are_mix() {
        for line in ["$a = 1; print($a)", "foo(); bar()"] {
            assert!(
                matches!(classify_input(line, &HashMap::new()), InputKind::MixCode(_)),
                "{line:?} should be executable Mix"
            );
        }
    }

    #[test]
    fn trailing_concat_classifies_as_incomplete_then_accumulated_mix() {
        for line in [
            "$s = \"a\" ..",
            "$s = \"a\" ..\n",
            "$s = \"a\" .. -- comment",
        ] {
            assert!(
                matches!(classify_input(line, &HashMap::new()), InputKind::Incomplete),
                "{line:?} must retain the REPL accumulator"
            );
        }
        assert!(matches!(
            classify_input("$s = \"a\" ..\n\"b\"", &HashMap::new()),
            InputKind::MixCode(_)
        ));
    }

    #[test]
    fn explicit_command_continuation_is_spliced_before_classification() {
        assert!(matches!(
            classify_input("echo one \\", &HashMap::new()),
            InputKind::Incomplete
        ));
        match classify_input("echo one \\\ntwo", &HashMap::new()) {
            InputKind::ExternalCommand(command) => assert_eq!(command, "echo one two"),
            other => panic!("expected external command, got {:?}", disc(&other)),
        }
        match classify_input(r"echo one \\", &HashMap::new()) {
            InputKind::ExternalCommand(command) => assert_eq!(command, r"echo one \\"),
            other => panic!(
                "even backslashes must stay external, got {:?}",
                disc(&other)
            ),
        }
    }

    #[test]
    fn unknown_command_ending_in_dotdot_keeps_shell_first_classification() {
        assert!(matches!(
            classify_input("zqxfoo ..", &HashMap::new()),
            InputKind::ExternalCommand(_)
        ));
    }

    #[test]
    fn dotdot_path_heads_and_external_arguments_are_not_continuations() {
        assert!(matches!(
            classify_input("../relscript.sh", &HashMap::new()),
            InputKind::ExternalCommand(_)
        ));
        assert!(matches!(
            classify_input("/usr/bin/printf ..", &HashMap::new()),
            InputKind::ExternalCommand(_)
        ));
    }

    #[test]
    fn head_is_command_like_uses_leading_shape_robust_to_later_chars() {
        // (work, first_word): single-word heads pass the same string for both.
        let cmd =
            |s: &str| super::head_is_command_like(s, s.split_whitespace().next().unwrap_or(""));
        // Barewords + paths are command-like (only the head is inspected, and a
        // later non-Mix char does not poison the verdict).
        assert!(cmd("gti"));
        assert!(cmd("foo:bar"));
        assert!(cmd("foo@bar")); // non-Mix char in head
        assert!(cmd("./build"));
        assert!(cmd("../tool"));
        assert!(cmd("/usr/bin/true"));
        assert!(cmd("~/bin/x"));
        assert!(cmd("bin/sh")); // bareword-led relative path
        assert!(cmd("2026/tool")); // numeric-led relative path
        // Dotted command/file names that don't parse as Mix stay command-like
        // (Dot alone is not a disqualifier — they are command-not-found today).
        assert!(cmd("script.sh"));
        assert!(cmd("python3.11"));
        // A bareword head with ordinary args stays command-like.
        assert!(cmd("gti status"));
        assert!(cmd("gti 0755"));
        // A pure-numeric slash head is arithmetic, not a path.
        assert!(!cmd("1/"));
        assert!(!cmd("1/0"));
        assert!(!cmd("1/0/2"));
        // Number / operator heads and lex failures are Mix expressions.
        assert!(!cmd("0755"));
        assert!(!cmd("1"));
        assert!(!cmd("print(0755)"));
        // A Mix call/index opener glued to OR following the head is Mix —
        // including after a field-access dot, and across whitespace.
        assert!(!cmd("foo(1,)"));
        assert!(!cmd("foo[1,]"));
        assert!(!cmd("foo.bar(1,)"));
        assert!(!cmd("foo.bar[1,]"));
        assert!(!cmd("foo (1,)")); // spaced call
        assert!(!cmd("foo [1,]")); // spaced index
        // An env-prefix value equal to the head must not be mistaken for it
        // (the real env-skip resolves first_word to `foo`, not `FOO=foo`).
        assert!(!super::head_is_command_like("FOO=foo foo (1,)", "foo"));
    }

    /// `false`/`true` as a line HEAD route to the external binaries so the
    /// universal shell idiom works (`false` exits 1); they were previously
    /// forced to Mix, where `false` evaluated to the literal and exited 0.
    /// `nil` (no external binary) still lands on the Mix parse.
    #[test]
    fn true_false_route_to_shell_nil_stays_mix() {
        use super::is_external_command;
        if is_external_command("true") && is_external_command("false") {
            for line in ["true", "false", "true && zqx", "false || zqx"] {
                assert!(
                    matches!(
                        classify_input(line, &HashMap::new()),
                        InputKind::ExternalCommand(_)
                    ),
                    "{line:?} should classify as ExternalCommand"
                );
            }
        }
        assert!(matches!(
            classify_input("nil", &HashMap::new()),
            InputKind::MixCode(_)
        ));
        // Inside Mix source the lexer still owns the literals.
        assert!(matches!(
            classify_input("$x = true", &HashMap::new()),
            InputKind::MixCode(_)
        ));
        assert!(matches!(
            classify_input("if true then print(1) end", &HashMap::new()),
            InputKind::MixCode(_)
        ));
    }

    /// expand_alias slices the tail from the first word's real byte offset:
    /// a whitespace-led line must not duplicate the head, and a multibyte
    /// first word after skipped whitespace must not panic mid-char.
    #[test]
    fn expand_alias_leading_whitespace_and_multibyte() {
        use super::expand_alias;
        let mut aliases = HashMap::new();
        aliases.insert("ll".to_string(), "ls -l".to_string());
        aliases.insert("é".to_string(), "echo accent".to_string());
        // Leading whitespace: head replaced once, not duplicated.
        assert_eq!(expand_alias("  ll -a", &aliases), "ls -l -a");
        assert_eq!(expand_alias("ll -a", &aliases), "ls -l -a");
        // Multibyte alias head behind leading whitespace: panicked before.
        assert_eq!(expand_alias(" é x", &aliases), "echo accent x");
        assert_eq!(expand_alias("é x", &aliases), "echo accent x");
        // Non-alias lines come back unchanged (whitespace preserved).
        assert_eq!(expand_alias("  plain x", &aliases), "  plain x");
        // Self-recursive alias terminates at the depth cap.
        let mut looping = HashMap::new();
        looping.insert("a".to_string(), "a b".to_string());
        let out = expand_alias("a", &looping);
        assert!(out.starts_with('a'));
    }

    proptest! {
        // Robustness: classification never panics on arbitrary input.
        #[test]
        fn classify_never_panics(s in "(?s).{0,300}") {
            let _ = classify_input(&s, &HashMap::new());
        }

        // Alias expansion never panics on arbitrary (incl. multibyte) input —
        // classification runs it on every line, with aliases loaded.
        #[test]
        fn expand_alias_never_panics(s in "(?s).{0,200}") {
            let mut aliases = HashMap::new();
            aliases.insert("é".to_string(), "echo accent".to_string());
            aliases.insert("ll".to_string(), "ls -l".to_string());
            let _ = super::expand_alias(&s, &aliases);
            let _ = classify_input(&s, &aliases);
        }

        // Determinism: the same line classifies to the same KIND every time
        // (the PATH/FS probes are stable within a run). Guards against a future
        // change that makes classification depend on hidden mutable state.
        #[test]
        fn classify_is_deterministic(s in "(?s).{0,200}") {
            let aliases = HashMap::new();
            let a = classify_input(&s, &aliases);
            let b = classify_input(&s, &aliases);
            prop_assert_eq!(disc(&a), disc(&b), "classification of {:?} not deterministic", s);
        }
    }
}

#[cfg(test)]
mod hyphenated_head_tests {
    use super::{InputKind, classify_input};
    use std::collections::HashMap;

    fn classify(line: &str) -> InputKind {
        classify_input(line, &HashMap::new())
    }

    fn assert_mix(line: &str) {
        assert!(
            matches!(classify(line), InputKind::MixCode(_)),
            "{line:?} must stay Mix"
        );
    }

    fn assert_external(line: &str) {
        assert!(
            matches!(classify(line), InputKind::ExternalCommand(_)),
            "{line:?} must be one external command head"
        );
    }

    #[test]
    fn tight_hyphenated_statement_heads_are_commands() {
        for line in [
            "cosmix-comp --nested",
            "systemd-nspawn",
            "weston-simple-dmabuf-egl",
            "docker-compose up",
            "alpha-no-such-command --flag",
        ] {
            assert_external(line);
        }
    }

    #[test]
    fn subtraction_shapes_stay_mix() {
        for line in ["a - b", "$a - $b", "1 - 2", "$x-1", "$a-$b"] {
            assert_mix(line);
        }
        // A tight bareword is never variable arithmetic: bare `a` is the
        // string "a", not `$a`, so `a-b` is a command regardless of scope.
        assert_external("a-b");
    }

    #[test]
    fn tight_hyphen_rule_is_statement_head_only() {
        for line in [
            "print(alpha-beta)",
            "$x = alpha-beta",
            "(alpha-beta)",
            "1+alpha-beta",
            "alpha.beta-gamma",
        ] {
            assert_mix(line);
        }
    }

    #[test]
    fn mix_meta_head_keeps_its_hyphenated_argument() {
        assert_external("mix not-a-command");
    }
}

#[cfg(test)]
mod function_dispatch_tests {
    use super::{InputKind, classify_input_fns, split_function_args};
    use std::collections::{HashMap, HashSet};

    fn fns(names: &[&str]) -> HashSet<String> {
        names.iter().map(|s| s.to_string()).collect()
    }

    /// A bareword line whose head is a defined function routes to
    /// `FunctionCommand` with shell-split string args, instead of `/bin/sh`.
    #[test]
    fn bareword_defined_function_dispatches() {
        let f = fns(&["sc", "health", "newpw"]);
        match classify_input_fns("sc restart nginx", &HashMap::new(), &f) {
            InputKind::FunctionCommand { name, args } => {
                assert_eq!(name, "sc");
                assert_eq!(args, vec!["restart".to_string(), "nginx".to_string()]);
            }
            other => panic!(
                "expected FunctionCommand, got {:?}",
                std::mem::discriminant(&other)
            ),
        }
    }

    /// A zero-arg function invoked bareword (`health`) dispatches with no args.
    #[test]
    fn zero_arg_function_dispatches() {
        let f = fns(&["health"]);
        match classify_input_fns("health", &HashMap::new(), &f) {
            InputKind::FunctionCommand { name, args } => {
                assert_eq!(name, "health");
                assert!(args.is_empty());
            }
            other => panic!(
                "expected FunctionCommand, got {:?}",
                std::mem::discriminant(&other)
            ),
        }
    }

    /// Quotes group and are stripped; no var/glob expansion.
    #[test]
    fn quoted_args_group_and_strip() {
        let f = fns(&["f"]);
        match classify_input_fns("f \"a b\" 'c|d'", &HashMap::new(), &f) {
            InputKind::FunctionCommand { name, args } => {
                assert_eq!(name, "f");
                assert_eq!(args, vec!["a b".to_string(), "c|d".to_string()]);
            }
            other => panic!(
                "expected FunctionCommand, got {:?}",
                std::mem::discriminant(&other)
            ),
        }
    }

    /// Without the function in scope, the same line stays a shell command —
    /// no dispatch to a non-existent Mix function.
    #[test]
    fn undefined_function_is_not_dispatched() {
        let f = fns(&["health"]);
        assert!(!matches!(
            classify_input_fns("sc restart nginx", &HashMap::new(), &f),
            InputKind::FunctionCommand { .. }
        ));
    }

    /// The paren-call form is unaffected — it parses as Mix, not FunctionCommand.
    #[test]
    fn paren_call_still_parses_as_mix() {
        let f = fns(&["sc"]);
        assert!(matches!(
            classify_input_fns("sc(\"restart\", \"nginx\")", &HashMap::new(), &f),
            InputKind::MixCode(_)
        ));
    }

    /// Mix keywords are never shadowed by a same-named function head — `print`
    /// stays Mix even if a `print` function somehow exists.
    #[test]
    fn keyword_head_beats_function() {
        let f = fns(&["print"]);
        assert!(!matches!(
            classify_input_fns("print hi", &HashMap::new(), &f),
            InputKind::FunctionCommand { .. }
        ));
    }

    /// A pipeline / redirect / chain with a function head bails to `None` so the
    /// whole line falls through to the normal shell path (function-in-pipeline is
    /// the shell's job, not a simple call).
    #[test]
    fn shell_metacharacters_bail_out() {
        assert!(split_function_args("sc status nginx | grep x", "sc").is_none());
        assert!(split_function_args("sc status > out.txt", "sc").is_none());
        assert!(split_function_args("sc a && sc b", "sc").is_none());
        assert!(split_function_args("sc a; sc b", "sc").is_none());
        assert!(split_function_args("sc $(hostname)", "sc").is_none());
    }

    /// An env prefix (`FOO=bar sc …`) is shell semantics — not a simple call.
    #[test]
    fn env_prefix_bails_out() {
        assert!(split_function_args("FOO=bar sc restart nginx", "sc").is_none());
    }

    /// A metacharacter INSIDE quotes is literal, not structural.
    #[test]
    fn quoted_metacharacter_is_literal() {
        assert_eq!(
            split_function_args("f \"a|b\"", "f"),
            Some(vec!["a|b".to_string()])
        );
    }
}
