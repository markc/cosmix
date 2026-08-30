use std::env;
use std::fs::{File, OpenOptions};
use std::io;
use std::process::{Child, Command, ExitStatus, Stdio};

use cosmix_mix::evaluator::ShellVarResolver;

use crate::jobs::JobTable;

/// Reap a backgrounded (`&`) child on a detached thread. Used wherever a
/// background child is spawned but no `JobTable` is reachable (sourced
/// files, chain pieces outside the REPL): the thread blocks in `wait()`,
/// so a long-lived host process (REPL / `--serve`) never accumulates
/// zombies. In a one-shot (`-c`) process the thread is harmless — the
/// process exits and init adopts the child either way.
pub fn reap_detached(mut child: Child) {
    std::thread::spawn(move || {
        let _ = child.wait();
    });
}

/// A `ShellVarResolver` that resolves nothing — used for STRUCTURAL parsing
/// (classification / validation) where `$VAR` values are irrelevant and the
/// caller only needs to know how the literal command line is shaped.
pub struct NoVars;

impl ShellVarResolver for NoVars {
    fn resolve(&self, name: &str) -> Option<String> {
        // Structural mode: resolve to a non-empty placeholder (the bare name,
        // always identifier chars, never an operator) so a command whose
        // required token is a variable — e.g. `echo hi > $OUT` — still parses
        // structurally. The value is irrelevant to classification/validation.
        Some(name.to_string())
    }

    fn command_subst(&self, _cmd: &str) -> Option<String> {
        // Structural mode: NEVER spawn. Resolve to a non-empty placeholder
        // (like `resolve` above) so a word containing `$(...)` stays
        // structurally present for classification/validation instead of
        // dropping out — the substituted text is never executed here.
        Some("x".to_string())
    }
}

#[derive(Debug, PartialEq, Eq)]
pub enum Redirect {
    StdoutOverwrite(String), // >  file   /  1>  file
    StdoutAppend(String),    // >> file   /  1>> file
    StdinFrom(String),       // <  file
    StderrOverwrite(String), // 2>  file
    StderrAppend(String),    // 2>> file
    StderrToStdout,          // 2>&1  (fd 2 := fd 1, after stdout is set up)
    StdoutToStderr,          // 1>&2  (fd 1 := fd 2)
    BothOverwrite(String),   // &>  file   ≡  >file 2>&1
    BothAppend(String),      // &>> file   ≡  >>file 2>&1
}

#[derive(Debug)]
pub struct PipeSegment {
    pub program: String,
    pub args: Vec<String>,
    /// Per-arg flag: true if the argument was quoted (skip glob expansion).
    pub quoted: Vec<bool>,
    pub redirects: Vec<Redirect>,
    pub env_vars: Vec<(String, String)>,
}

/// A tokenized word/operator. `op`/`assign` are determined from LITERAL input
/// during tokenization — NOT from a token's final string value — so `$VAR`
/// expansion (which fills `value`) can never turn data into a redirect operator,
/// a `name=` env assignment, or an fd prefix (`2>`, `2>&1`).
/// `PartialEq` is for the phase-targeted tokenizer tests (test-only use).
#[derive(Debug, Clone, PartialEq)]
struct Tok {
    value: String,
    /// Any part of the word was quoted (drives glob-skip downstream).
    quoted: bool,
    /// This token is a literal redirect operator. `value` carries the exact
    /// form: `>` `>>` `<` `1>` `1>>` `2>` `2>>` (file targets) or `2>&1` `1>&2`
    /// (fd dups, no target). Decided from LITERAL input only.
    op: bool,
    /// This word began with a literal `name=` assignment prefix.
    assign: bool,
}

#[derive(Debug)]
pub struct Pipeline {
    pub segments: Vec<PipeSegment>,
    pub background: bool,
}

/// Control connector preceding a command-list item.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Connector {
    /// First item, or after `;` — always run.
    Always,
    /// After `&&` — run only if the previous item succeeded.
    And,
    /// After `||` — run only if the previous item failed.
    Or,
}

/// Parse a raw command line into a Pipeline.
pub fn parse_pipeline(line: &str, vars: &dyn ShellVarResolver) -> Result<Pipeline, String> {
    let trimmed = line.trim();
    let (line, background) = if let Some(stripped) = trimmed.strip_suffix('&') {
        (stripped.trim(), true)
    } else {
        (trimmed, false)
    };

    let pipe_parts = split_on_pipes(line);
    let mut segments = Vec::new();

    for part in pipe_parts {
        let seg = parse_segment(part.trim(), vars)?;
        segments.push(seg);
    }

    if segments.is_empty() {
        return Err("empty command".to_string());
    }

    Ok(Pipeline {
        segments,
        background,
    })
}

/// Shell-style exit code for a finished child: its exit code if it exited
/// normally, else `128 + signal` on Unix (so a signal-killed command reports
/// non-zero), else 1.
pub fn exit_code(status: ExitStatus) -> i32 {
    if let Some(code) = status.code() {
        return code;
    }
    #[cfg(unix)]
    {
        use std::os::unix::process::ExitStatusExt;
        if let Some(sig) = status.signal() {
            return 128 + sig;
        }
    }
    1
}

/// Split a command line into pipelines separated by the shell control
/// operators `&&`, `||`, and `;`, each tagged with the connector that
/// precedes it (the first is `Always`). The split is on LITERAL text only —
/// `|` pipes and a trailing `&` stay inside each pipeline (handled by
/// `parse_pipeline`). Callers expand `$VAR`s PER-PIECE after this split so a
/// variable's value can never introduce a control operator (no injection).
/// A single trailing `;` is allowed (drops its empty tail); any other empty
/// piece (leading/interior, or a dangling `&&`/`||`) is an error.
pub fn parse_command_list(
    line: &str,
    vars: &dyn ShellVarResolver,
) -> Result<Vec<(Connector, Pipeline)>, String> {
    // `parse_pipeline` tokenizes the LITERAL piece and expands `$VAR`s into arg
    // tokens via `vars` — control operators and pipes are split from literal
    // text only, so a variable value can never inject one. NOTE: this expands
    // EVERY piece eagerly (running any `$(...)`), so it is for STRUCTURAL
    // callers passing `NoVars`, or single-shot uses — execution paths with a
    // live resolver must use `split_command_list` + `execute_command_list_outcome`
    // so a `$(...)` in a short-circuited branch never runs.
    split_command_list(line)?
        .into_iter()
        .map(|(conn, part)| Ok((conn, parse_pipeline(part, vars)?)))
        .collect()
}

/// Split a command line into `&&`/`||`/`;` list items WITHOUT resolving or
/// expanding anything — the structural half of `parse_command_list`. Each item
/// is the trimmed RAW piece text plus its preceding connector. A single
/// trailing `;` is allowed (its empty tail is dropped); any other empty piece
/// (leading/interior, or a dangling `&&`/`||`) is an error. Because no resolver
/// runs here, this is the safe entry for execution paths: callers expand and
/// run each piece via `execute_command_list_outcome` only when its connector
/// selects it, so a `$(...)` in a skipped branch is never executed.
pub fn split_command_list(line: &str) -> Result<Vec<(Connector, &str)>, String> {
    let mut pieces = split_on_control_ops(line);
    // Allow a single trailing `;` (its tail piece is empty with an `Always`
    // connector). A dangling `&&`/`||` leaves an `And`/`Or` empty tail -> error.
    if pieces.len() > 1
        && let Some(&(Connector::Always, tail)) = pieces.last()
        && tail.trim().is_empty()
    {
        pieces.pop();
    }
    let mut out = Vec::new();
    for (conn, part) in pieces {
        let part = part.trim();
        if part.is_empty() {
            return Err("empty command in list".to_string());
        }
        out.push((conn, part));
    }
    if out.is_empty() {
        return Err("empty command".to_string());
    }
    Ok(out)
}

/// Split on unquoted top-level `&&`, `||`, and `;`. Single `&` (background)
/// and `|` (pipe) are NOT split points. Backslash escapes are honoured only
/// OUTSIDE single quotes (shell single quotes are fully literal); inside any
/// quotes, operators are not split points.
/// Given `bytes` and the index `open` of the first byte INSIDE a `$( ... )`
/// command substitution (i.e. just past the `$(`), return the index one past
/// the matching `)`, scanning QUOTE-AWARELY exactly like `lex_dollar`: parens
/// inside `'...'`/`"..."` are literal, and a backslash escapes the next byte
/// outside single quotes. An unterminated span returns `bytes.len()`. Shared by
/// `lex_dollar`'s sibling line-splitters so all three agree on where a `$(...)`
/// span ends — a quoted `)` (`$(echo ")" ; x)`) must NOT close it early, or a
/// following `;`/`|`/`&&` would mis-split a command that is really one span.
/// Delimiters are ASCII (<0x80), so byte-wise scanning is correct for UTF-8.
fn cmdsubst_span_end(bytes: &[u8], open: usize) -> usize {
    let mut depth = 1usize;
    let mut in_single = false;
    let mut in_double = false;
    let mut escaped = false;
    let mut i = open;
    while i < bytes.len() {
        if escaped {
            escaped = false;
            i += 1;
            continue;
        }
        match bytes[i] as char {
            '\\' if !in_single => escaped = true,
            '\'' if !in_double => in_single = !in_single,
            '"' if !in_single => in_double = !in_double,
            '(' if !in_single && !in_double => depth += 1,
            ')' if !in_single && !in_double => {
                depth -= 1;
                if depth == 0 {
                    return i + 1;
                }
            }
            _ => {}
        }
        i += 1;
    }
    bytes.len()
}

pub fn split_on_control_ops(s: &str) -> Vec<(Connector, &str)> {
    let mut parts = Vec::new();
    let mut start = 0;
    let mut conn = Connector::Always;
    let mut in_single = false;
    let mut in_double = false;
    let mut escape = false;
    let bytes = s.as_bytes();
    let mut i = 0;

    while i < bytes.len() {
        if escape {
            // Previous char was a backslash (only set outside single quotes).
            escape = false;
            i += 1;
            continue;
        }
        let ch = bytes[i] as char;
        if in_single {
            // Inside single quotes everything is literal except the close.
            if ch == '\'' {
                in_single = false;
            }
            i += 1;
            continue;
        }
        match ch {
            '\\' => escape = true,
            '\'' if !in_double => in_single = true,
            '"' => in_double = !in_double,
            // Consume a whole `$(...)` span quote-awarely so a `;`/`&&`/`||`
            // inside it (or inside a quoted `)` within it) never splits. `$((`
            // is arithmetic, NOT command substitution — leave its parens to
            // scan as ordinary non-split chars, matching `lex_dollar`.
            '$' if bytes.get(i + 1) == Some(&b'(') && bytes.get(i + 2) != Some(&b'(') => {
                i = cmdsubst_span_end(bytes, i + 2);
                continue;
            }
            ';' if !in_double => {
                parts.push((conn, &s[start..i]));
                conn = Connector::Always;
                start = i + 1;
            }
            '&' if !in_double && bytes.get(i + 1) == Some(&b'&') => {
                parts.push((conn, &s[start..i]));
                conn = Connector::And;
                start = i + 2;
                i += 2;
                continue;
            }
            '|' if !in_double && bytes.get(i + 1) == Some(&b'|') => {
                parts.push((conn, &s[start..i]));
                conn = Connector::Or;
                start = i + 2;
                i += 2;
                continue;
            }
            _ => {}
        }
        i += 1;
    }
    parts.push((conn, &s[start..]));
    parts
}

/// The canonical in-process `cd` — the ONE implementation behind the REPL,
/// `mix -c`, sourced files, and `&&`/`||`/`;` chains (they diverged for a
/// while; this unifies them). Semantics:
/// - no arg → `$HOME` (falling back to `/`)
/// - `-` → `$OLDPWD`, an error (exit 1) when OLDPWD is unset
/// - `~` / `~/rest` → `$HOME`-expanded
/// - `~user/...` stays LITERAL so `set_current_dir` fails with ENOENT instead
///   of silently targeting `$HOMEuser/...` (named-user lookup unsupported)
///
/// `PWD`/`OLDPWD` are mutated only after a successful `set_current_dir`, so a
/// failed `cd` leaves the env untouched. Returns the shell exit code (0/1).
pub fn builtin_cd(args: &[String]) -> i32 {
    let target = if args.is_empty() {
        env::var("HOME").unwrap_or_else(|_| "/".to_string())
    } else {
        let arg = args[0].as_str();
        if arg == "-" {
            match env::var("OLDPWD") {
                Ok(p) => p,
                Err(_) => {
                    eprintln!("cd: OLDPWD not set");
                    return 1;
                }
            }
        } else if arg == "~" {
            env::var("HOME").unwrap_or_default()
        } else if let Some(rest) = arg.strip_prefix("~/") {
            let home = env::var("HOME").unwrap_or_default();
            format!("{}/{}", home, rest)
        } else {
            arg.to_string()
        }
    };

    let old = env::current_dir()
        .map(|p| p.to_string_lossy().to_string())
        .unwrap_or_default();

    if let Err(e) = env::set_current_dir(&target) {
        eprintln!("cd: {}: {}", target, e);
        return 1;
    }

    // SAFETY: Mix is single-threaded.
    unsafe {
        env::set_var("OLDPWD", &old);
        if let Ok(new) = env::current_dir() {
            env::set_var("PWD", new.to_string_lossy().as_ref());
        }
    }
    0
}

/// Execute a structurally-split command list (`split_command_list` output) with
/// `&&`/`||`/`;` short-circuit semantics, returning the exit code of the LAST
/// EXECUTED pipeline. `Always` runs unconditionally; `And` runs only if the
/// previous item succeeded; `Or` only if it failed. A skipped item carries the
/// prior success forward (bash semantics).
///
/// Each selected piece is parsed and expanded — and thus any `$(...)` in it is
/// RUN — only at the moment it is chosen, so a substitution in a short-circuited
/// branch (`false && echo $(side_effect)`) never executes. To preserve the
/// old eager-parse "syntax error anywhere aborts the whole list" behavior, all
/// pieces are first parsed structurally with `NoVars` (no resolution, no spawn);
/// a malformed pipeline there returns exit 2 having run nothing. A per-command
/// spawn error on a selected piece is reported as exit 127 (so
/// `nosuch || echo fallback` runs the fallback), not a fatal abort.
/// What a lazily-executed command list actually did.
pub struct ListOutcome {
    /// Exit code of the last EXECUTED piece.
    pub code: i32,
    /// A piece the connectors actually SELECTED was spawned into the background.
    ///
    /// Must come from execution, not from scanning the pieces for a trailing `&`:
    /// a `&` in a branch that short-circuits away never runs, so
    /// `false && sleep 5 &` backgrounds NOTHING and stays honestly timeable. The
    /// `time` modifier reads this to decide whether it has a real duration to
    /// report or only a spawn latency.
    pub backgrounded: bool,
    /// First segment of every pipeline selected by list control flow.
    pub commands: Vec<String>,
}

pub fn execute_command_list_outcome(
    items: &[(Connector, &str)],
    vars: &dyn ShellVarResolver,
    mut jobs: Option<&mut JobTable>,
) -> ListOutcome {
    // Up-front structural validation (no resolver → never spawns a `$(...)`),
    // matching the old behavior where parsing the whole list preceded running
    // any of it. A bad pipeline in ANY branch (even a skipped one) aborts.
    for (_, piece) in items {
        if let Err(e) = parse_pipeline(piece, &NoVars) {
            eprintln!("mix: {}", e);
            return ListOutcome {
                code: 2,
                backgrounded: false,
                commands: Vec::new(),
            };
        }
    }

    let mut last_code = 0;
    let mut last_success = true;
    let mut backgrounded = false;
    let mut commands = Vec::new();

    for (conn, piece) in items {
        let run = match conn {
            Connector::Always => true,
            Connector::And => last_success,
            Connector::Or => !last_success,
        };
        if !run {
            continue;
        }
        // Parse + expand (running any `$(...)`) ONLY now, for a selected piece.
        // Structural validation above already proved it parses, so an Err here
        // is unexpected; treat it like a spawn failure for chain control flow.
        let pipeline = match parse_pipeline(piece, vars) {
            Ok(p) => p,
            Err(e) => {
                eprintln!("mix: {}", e);
                last_code = 127;
                last_success = false;
                continue;
            }
        };
        if let Some(first) = pipeline.segments.first() {
            commands.push(first.program.clone());
        }
        // In-process `cd` inside a chain (`cd x && cmd`) — there is no external
        // `cd` binary, so without this a chain piece spawned-and-failed with
        // 127 while the lone-command paths all intercepted it. Only a PLAIN
        // single-segment foreground `cd` qualifies: a piped, backgrounded,
        // redirected, or env-prefixed `cd` keeps the old spawn path rather
        // than silently dropping the redirect/env (`cd /nope 2>err` must not
        // leak the diagnostic to the shell's own stderr).
        if let [seg] = pipeline.segments.as_slice()
            && !pipeline.background
            && seg.program == "cd"
            && seg.redirects.is_empty()
            && seg.env_vars.is_empty()
        {
            last_code = builtin_cd(&seg.args);
            last_success = last_code == 0;
            continue;
        }
        last_code = match execute_pipeline(&pipeline) {
            Ok(PipelineResult::Done(status)) => exit_code(status),
            // Backgrounded (`&`) inside a list: track it in the caller's job
            // table when one exists (the REPL), else reap it on a detached
            // thread — never leave a child un-waited. Treated as launched-ok
            // for chain control flow either way.
            Ok(PipelineResult::Background(child)) => {
                backgrounded = true;
                match jobs.as_deref_mut() {
                    Some(table) => {
                        table.add(piece.trim().to_string(), child);
                    }
                    None => reap_detached(child),
                }
                0
            }
            Err(e) => {
                let prog = pipeline
                    .segments
                    .first()
                    .map(|s| s.program.as_str())
                    .unwrap_or("command");
                eprintln!("mix: {}: {}", prog, e);
                127
            }
        };
        last_success = last_code == 0;
    }

    ListOutcome {
        code: last_code,
        backgrounded,
        commands,
    }
}

/// Split on unquoted pipe characters.
fn split_on_pipes(s: &str) -> Vec<&str> {
    let mut parts = Vec::new();
    let mut start = 0;
    let mut in_single = false;
    let mut in_double = false;
    let mut escape = false;
    let bytes = s.as_bytes();
    let mut i = 0;

    while i < bytes.len() {
        if escape {
            escape = false;
            i += 1;
            continue;
        }
        let ch = bytes[i] as char;
        // Inside single quotes everything is literal (no escape) — only the
        // closing quote matters (parity with split_on_control_ops).
        if in_single {
            if ch == '\'' {
                in_single = false;
            }
            i += 1;
            continue;
        }
        match ch {
            '\\' => escape = true,
            '\'' => in_single = true,
            '"' => in_double = !in_double,
            // A `|` inside an open `$( ... )` belongs to the inner command, so
            // consume the whole span quote-awarely (see `cmdsubst_span_end`).
            // `$((` is arithmetic, not command substitution.
            '$' if bytes.get(i + 1) == Some(&b'(') && bytes.get(i + 2) != Some(&b'(') => {
                i = cmdsubst_span_end(bytes, i + 2);
                continue;
            }
            '|' if !in_double => {
                parts.push(&s[start..i]);
                start = i + 1;
            }
            _ => {}
        }
        i += 1;
    }
    parts.push(&s[start..]);
    parts
}

/// Parse a single command segment (program + args + redirects + env). Token
/// roles (redirect op, `name=` assignment, `2>&1`) come from the typed tokens
/// — decided from LITERAL input — so an expanded `$VAR` value is always a word
/// (arg/value), never reinterpreted as syntax.
fn parse_segment(s: &str, vars: &dyn ShellVarResolver) -> Result<PipeSegment, String> {
    let tokens = shell_tokenize(s, vars);
    let mut words: Vec<Tok> = Vec::new();
    let mut redirects: Vec<Redirect> = Vec::new();
    let mut i = 0;

    while i < tokens.len() {
        let t = &tokens[i];
        if t.op {
            // File-target ops consume the next token; fd-dup ops (`2>&1`,
            // `1>&2`) take no target.
            let target = || tokens.get(i + 1).map(|n| n.value.clone());
            match t.value.as_str() {
                ">" | "1>" => {
                    redirects.push(Redirect::StdoutOverwrite(
                        target().ok_or("expected filename after >")?,
                    ));
                    i += 1;
                }
                ">>" | "1>>" => {
                    redirects.push(Redirect::StdoutAppend(
                        target().ok_or("expected filename after >>")?,
                    ));
                    i += 1;
                }
                "<" => {
                    redirects.push(Redirect::StdinFrom(
                        target().ok_or("expected filename after <")?,
                    ));
                    i += 1;
                }
                "2>" => {
                    redirects.push(Redirect::StderrOverwrite(
                        target().ok_or("expected filename after 2>")?,
                    ));
                    i += 1;
                }
                "2>>" => {
                    redirects.push(Redirect::StderrAppend(
                        target().ok_or("expected filename after 2>>")?,
                    ));
                    i += 1;
                }
                "&>" => {
                    redirects.push(Redirect::BothOverwrite(
                        target().ok_or("expected filename after &>")?,
                    ));
                    i += 1;
                }
                "&>>" => {
                    redirects.push(Redirect::BothAppend(
                        target().ok_or("expected filename after &>>")?,
                    ));
                    i += 1;
                }
                "2>&1" => redirects.push(Redirect::StderrToStdout),
                "1>&2" => redirects.push(Redirect::StdoutToStderr),
                // Self-dup (`>&1` ≡ `1>&1`, `2>&2`): fd onto itself is a no-op,
                // so no redirect is emitted — but recognise it explicitly rather
                // than letting it fall through as an unhandled op.
                "1>&1" | "2>&2" => {}
                // Any other recognised fd-dup shape (`>&3`, `>&-`, `1>&3`, …) is
                // an UNSUPPORTED target — Mix dups only fds 1/2. Fail loudly
                // rather than silently no-op'ing or creating a junk file.
                other if other.contains(">&") => {
                    return Err(format!(
                        "unsupported fd in redirect '{other}' — Mix dups only fds 1 and 2"
                    ));
                }
                _ => {}
            }
        } else {
            words.push(t.clone());
        }
        i += 1;
    }

    if words.is_empty() {
        return Err("empty command segment".to_string());
    }

    // Leading LITERAL `name=value` words are environment assignments. `assign`
    // was set at tokenize-time only for a literal identifier-then-`=` prefix,
    // so an expanded `$X` = "PATH=/evil" is a plain word, never an assignment.
    let mut env_vars: Vec<(String, String)> = Vec::new();
    let mut start = 0;
    while start < words.len() && words[start].assign {
        let w = &words[start].value;
        if let Some(eq) = w.find('=') {
            env_vars.push((w[..eq].to_string(), w[eq + 1..].to_string()));
        }
        start += 1;
    }

    let rest = &words[start..];
    if rest.is_empty() {
        return Err("empty command segment (only env vars, no program)".to_string());
    }

    let program = rest[0].value.clone();
    let args: Vec<String> = rest[1..].iter().map(|t| t.value.clone()).collect();
    let quoted: Vec<bool> = rest[1..].iter().map(|t| t.quoted).collect();
    Ok(PipeSegment {
        program,
        args,
        quoted,
        redirects,
        env_vars,
    })
}

/// Assemble a redirect operator string, called with the `>` already consumed.
/// `prefix` is the literal fd prefix taken from the current word — `""` (none),
/// `"1"`, `"2"`, or `"&"` — and is decided by the caller from LITERAL input ONLY
/// (an expanded `$VAR` value never reaches here, which is what keeps redirect
/// recognition injection-safe). This is the single place every redirect operator
/// is shaped, so a future operator is a change here rather than another branch
/// grafted onto the scan loop. Consumes a trailing `>` (append) and an `&N`
/// fd-dup target. The dup source is the explicit numeric prefix (`2>&1`, `1>&2`)
/// or — for a BARE `>` with no prefix — the implicit fd 1, so `>&2` / `>&1` are
/// the bash shorthands for `1>&2` / `1>&1` (the common `cmd >&2` idiom). A `&`
/// prefix never forms a dup (`&>&1` is not a thing), so it falls through to `&>`.
fn lex_redirect_after_gt(prefix: &str, chars: &mut std::iter::Peekable<std::str::Chars>) -> String {
    if chars.peek() == Some(&'>') {
        chars.next();
        return format!("{prefix}>>");
    }
    // fd dup target `&<fd>`: a numeric prefix dups from that fd; a bare `>`
    // (empty prefix) dups from the implicit fd 1. The `&` prefix is excluded —
    // `&>&N` is not valid. A digit or `-` (close) target is recognised AS THE
    // OPERATOR here (so it is never mistaken for a filename `&3`); whether the
    // target is actually SUPPORTED — Mix dups only fds 1/2 — is decided in
    // `parse_segment`, which errors on the rest rather than leaving junk.
    if (prefix.is_empty() || prefix == "1" || prefix == "2") && chars.peek() == Some(&'&') {
        let mut look = chars.clone();
        look.next(); // the `&`
        // Target is a FULL digit run (so `>&10` is the fd 10, not `>&1` + "0")
        // or a single `-` (fd close). Both are recognised as the operator here;
        // `parse_segment` accepts only fds 1/2 and errors on the rest.
        let target: String = match look.peek() {
            Some('-') => "-".to_string(),
            Some(c) if c.is_ascii_digit() => {
                let mut s = String::new();
                while let Some(&d) = look.peek() {
                    if d.is_ascii_digit() {
                        s.push(d);
                        look.next();
                    } else {
                        break;
                    }
                }
                s
            }
            _ => String::new(),
        };
        if !target.is_empty() {
            chars.next(); // consume `&`
            for _ in 0..target.len() {
                chars.next(); // consume each target byte (ASCII digits / `-`)
            }
            let src = if prefix.is_empty() { "1" } else { prefix };
            return format!("{src}>&{target}");
        }
    }
    format!("{prefix}>")
}

// ===========================================================================
// Phased shell tokenizer (lex -> brace-expand -> expand). Phase 1
// (`lex_literal`) decides ALL structure — word boundaries, quotes,
// operators/redirects, `name=` prefixes — from LITERAL text with NO resolver in
// sight, emitting words as lists of SEGMENTS that remember their rawness. Phase
// 1.5 (`expand_braces`) multiplies words from brace groups found ONLY in raw
// literal chars (brace expansion precedes value resolution, like bash). Phase 2
// (`expand`) is the ONLY pass that touches variable values: it resolves each
// Var segment, expands a leading Tilde, concatenates a word's segments, drops
// an unquoted word that expanded to empty, and stamps the `quoted`/`op`/
// `assign` flags. Because operators and brace groups are fixed in passes that
// never see a value, an expanded value CANNOT introduce an operator, redirect,
// assignment, fd prefix, word split, or brace expansion — the injection
// invariant the old `current_dynamic` flag enforced by hand is now structural.
// See _doc/planned/mix-shell-tokenizer-two-phase.md in the cosmix hub.
// ===========================================================================

/// Where a segment's text came from. Only `Raw` literal text is eligible to form
/// an fd prefix (`1`/`2` before `>`) or a `name=` assignment KEY; `Escaped`,
/// `SingleQuoted`, and `DoubleQuoted` text is DATA — never syntax. The two quoted
/// variants drive the glob-skip `Tok.quoted` flag (an `Escaped` or `Raw` segment
/// leaves it false, so `\*` and an expanded `$X`→`*` stay glob-eligible).
#[derive(Debug, Clone, PartialEq)]
enum SegSource {
    Raw,
    Escaped,
    SingleQuoted,
    DoubleQuoted,
}

impl SegSource {
    /// True for the two quoted variants (drives glob-skip).
    fn is_quoted(&self) -> bool {
        matches!(self, SegSource::SingleQuoted | SegSource::DoubleQuoted)
    }
}

/// One piece of a word. `Literal` text is taken verbatim; `Var` is resolved in
/// phase 2 (an empty `name`, only reachable via `${}`, resolves to nothing and
/// does not start a word); `Tilde` expands to `$HOME` (or stays `~` when `HOME`
/// is empty) in phase 2.
#[derive(Debug, Clone, PartialEq)]
enum SegKind {
    Literal(String),
    Var {
        name: String,
        braced: bool,
    },
    Tilde,
    /// A `$(...)` command substitution. `cmd` is the inner command text
    /// (parens balanced, the outer `$(` / `)` stripped); it is resolved in
    /// phase 2 via `ShellVarResolver::command_subst`. Like `Var`, it is a
    /// VALUE — never an fd prefix or assignment key (see `WordAcc::raw_text`),
    /// so it cannot inject structure.
    CmdSubst {
        cmd: String,
    },
}

#[derive(Debug, Clone, PartialEq)]
struct Segment {
    kind: SegKind,
    source: SegSource,
}

/// Phase-1 output: either a structural operator (decided from literal text) or a
/// word as its segment list plus the literal `name=` assignment-prefix flag.
#[derive(Debug, Clone, PartialEq)]
enum LiteralToken {
    Word {
        segments: Vec<Segment>,
        assign: bool,
    },
    Op(String),
}

/// The in-progress word during phase-1 lexing. Replaces the old scanner's
/// interleaved `current`/`quoted`/`word_started`/`key_ok`/`assign`/
/// `current_dynamic`/`at_word_start` flags: rawness lives in the segments,
/// `at_word_start` is `segs.is_empty()`, and `current_dynamic == false` is
/// `raw_text().is_some()`.
struct WordAcc {
    segs: Vec<Segment>,
    /// A literal `name=` assignment prefix is still possible (cleared by any
    /// quote / `$` / `~` / `\` / non-identifier char, exactly as the old `key_ok`).
    key_ok: bool,
    /// A literal `name=` prefix was seen.
    assign: bool,
}

impl WordAcc {
    fn new() -> Self {
        WordAcc {
            segs: Vec::new(),
            key_ok: true,
            assign: false,
        }
    }

    /// `true` at the start of a fresh word — the old `at_word_start`, used for
    /// tilde eligibility.
    fn at_word_start(&self) -> bool {
        self.segs.is_empty()
    }

    /// Append literal `text` with `source`, coalescing into a trailing `Literal`
    /// of the SAME source (so a run of raw chars is one segment). An empty `text`
    /// with a fresh source still pushes a zero-length segment — that is how an
    /// opening quote (`""`/`''`) or a trailing `\` records that the word started
    /// and is no longer a pure-`Raw` run.
    fn push_literal(&mut self, text: &str, source: SegSource) {
        if let Some(Segment {
            kind: SegKind::Literal(s),
            source: prev,
        }) = self.segs.last_mut()
            && *prev == source
        {
            s.push_str(text);
            return;
        }
        self.segs.push(Segment {
            kind: SegKind::Literal(text.to_string()),
            source,
        });
    }

    fn push_var(&mut self, name: String, braced: bool, source: SegSource) {
        self.segs.push(Segment {
            kind: SegKind::Var { name, braced },
            source,
        });
    }

    fn push_tilde(&mut self) {
        self.segs.push(Segment {
            kind: SegKind::Tilde,
            source: SegSource::Raw,
        });
    }

    fn push_cmdsubst(&mut self, cmd: String, source: SegSource) {
        self.segs.push(Segment {
            kind: SegKind::CmdSubst { cmd },
            source,
        });
    }

    /// The concatenated text IFF every segment is a `Raw` literal — i.e. the word
    /// is a pure-literal run with no quote/var/tilde/escape (the old
    /// `!current_dynamic`). `Some` here is the ONLY thing eligible to be an fd
    /// prefix or an assignment key; any other segment returns `None`, which is the
    /// structural form of injection safety (a value can never reach this).
    fn raw_text(&self) -> Option<String> {
        let mut out = String::new();
        for seg in &self.segs {
            match (&seg.kind, &seg.source) {
                (SegKind::Literal(s), SegSource::Raw) => out.push_str(s),
                _ => return None,
            }
        }
        Some(out)
    }
}

/// Read a `$name` / `${name}` reference (the `$` already consumed) and push the
/// matching segment with `source` (Raw when unquoted, DoubleQuoted inside `"`).
/// A `${...}` is always a Var (an empty `${}` becomes an empty-name Var that
/// phase 2 drops to nothing); a bare `$` not followed by a name is a literal `$`.
/// NO resolution happens here — that is phase 2's job.
fn lex_dollar(
    w: &mut WordAcc,
    chars: &mut std::iter::Peekable<std::str::Chars>,
    source: SegSource,
) {
    if chars.peek() == Some(&'(') {
        chars.next(); // consume the first '('
        // `$((...))` arithmetic is NOT command substitution — keep it
        // literal (unchanged behavior). Emitting the `$((` prefix and
        // letting the rest lex normally reproduces the prior literal text.
        if chars.peek() == Some(&'(') {
            chars.next();
            w.push_literal("$((", source);
            return;
        }
        // Command substitution: read to the matching `)`, counting nested
        // parens QUOTE-AWARELY, like bash: a paren inside a `'...'`/`"..."`
        // word of the inner command is literal text (so `$(echo ")")` spans
        // to the real closer), and a backslash escapes the next char outside
        // single quotes (`$(echo \))`). This only decides WHERE the literal
        // span ends — the body stays unexamined data for phase 2 (phase 1
        // remains value-independent). An UNTERMINATED `$(` keeps the
        // literal text and never executes a partial command.
        let mut depth = 1usize;
        let mut cmd = String::new();
        let mut closed = false;
        let mut in_single = false;
        let mut in_double = false;
        let mut escaped = false;
        for c in chars.by_ref() {
            if escaped {
                escaped = false;
                cmd.push(c);
                continue;
            }
            match c {
                '\\' if !in_single => {
                    escaped = true;
                    cmd.push(c);
                }
                '\'' if !in_double => {
                    in_single = !in_single;
                    cmd.push(c);
                }
                '"' if !in_single => {
                    in_double = !in_double;
                    cmd.push(c);
                }
                '(' if !in_single && !in_double => {
                    depth += 1;
                    cmd.push(c);
                }
                ')' if !in_single && !in_double => {
                    depth -= 1;
                    if depth == 0 {
                        closed = true;
                        break;
                    }
                    cmd.push(c);
                }
                _ => cmd.push(c),
            }
        }
        if closed {
            w.push_cmdsubst(cmd, source);
        } else {
            w.push_literal(&format!("$({}", cmd), source);
        }
    } else if chars.peek() == Some(&'{') {
        chars.next();
        let mut name = String::new();
        while let Some(&c) = chars.peek() {
            chars.next();
            if c == '}' {
                break;
            }
            name.push(c);
        }
        w.push_var(name, true, source);
    } else {
        let mut name = String::new();
        while let Some(&c) = chars.peek() {
            if c.is_ascii_alphanumeric() || c == '_' {
                name.push(c);
                chars.next();
            } else {
                break;
            }
        }
        if name.is_empty() {
            w.push_literal("$", source);
        } else {
            w.push_var(name, false, source);
        }
    }
}

/// Flush the in-progress word into `tokens` if it has any segments, then reset.
/// Whether the word actually SURVIVES (a var-only word that resolves to empty is
/// dropped) is decided in `expand`; phase 1 only knows structure, so it emits any
/// non-empty segment list and lets phase 2 judge existence by value.
fn flush_literal_word(tokens: &mut Vec<LiteralToken>, w: &mut WordAcc) {
    if !w.segs.is_empty() {
        tokens.push(LiteralToken::Word {
            segments: std::mem::take(&mut w.segs),
            assign: w.assign,
        });
    }
    *w = WordAcc::new();
}

/// Phase 1: lex the literal command line into structural tokens, WITHOUT any
/// resolver. Operators, fd prefixes, and `name=` prefixes are decided here from
/// raw literal text only; variables become Var segments to be filled in phase 2.
fn lex_literal(s: &str) -> Vec<LiteralToken> {
    let mut tokens: Vec<LiteralToken> = Vec::new();
    let mut w = WordAcc::new();
    let mut chars = s.chars().peekable();
    let mut in_single = false;
    let mut in_double = false;

    while let Some(&ch) = chars.peek() {
        if in_single {
            chars.next();
            if ch == '\'' {
                in_single = false;
            } else {
                w.push_literal(&ch.to_string(), SegSource::SingleQuoted);
            }
        } else if in_double {
            chars.next();
            if ch == '"' {
                in_double = false;
            } else if ch == '$' {
                lex_dollar(&mut w, &mut chars, SegSource::DoubleQuoted);
            } else if ch == '\\' {
                if let Some(&next) = chars.peek() {
                    if matches!(next, '"' | '\\' | '$' | '`') {
                        chars.next();
                        w.push_literal(&next.to_string(), SegSource::DoubleQuoted);
                    } else {
                        w.push_literal("\\", SegSource::DoubleQuoted);
                    }
                }
            } else {
                w.push_literal(&ch.to_string(), SegSource::DoubleQuoted);
            }
        } else {
            match ch {
                '\'' => {
                    chars.next();
                    in_single = true;
                    w.key_ok = false;
                    // Record the opened quote even if empty (the `''` rule).
                    w.push_literal("", SegSource::SingleQuoted);
                }
                '"' => {
                    chars.next();
                    in_double = true;
                    w.key_ok = false;
                    w.push_literal("", SegSource::DoubleQuoted);
                }
                '$' => {
                    chars.next();
                    w.key_ok = false;
                    lex_dollar(&mut w, &mut chars, SegSource::Raw);
                }
                // Leading `~` is special ONLY at word start and only when the next
                // char is end / `/` / space / tab (the eligibility rule). HOME-
                // emptiness is decided in phase 2, so an eligible `~` is always a
                // Tilde segment here. A non-eligible `~` is a plain literal.
                '~' if w.at_word_start() => {
                    chars.next();
                    match chars.peek() {
                        None | Some('/') | Some(' ') | Some('\t') => w.push_tilde(),
                        _ => w.push_literal("~", SegSource::Raw),
                    }
                    w.key_ok = false;
                }
                '\\' => {
                    chars.next();
                    if let Some(&next) = chars.peek() {
                        chars.next();
                        w.push_literal(&next.to_string(), SegSource::Escaped);
                    } else {
                        // A trailing `\` consumes nothing but still starts the
                        // word (matches the old `echo \` -> one empty arg).
                        w.push_literal("", SegSource::Escaped);
                    }
                    w.key_ok = false;
                }
                '=' => {
                    chars.next();
                    // A literal `name=` prefix marks an env assignment word. The
                    // key must be a RAW identifier ([A-Za-z_][A-Za-z0-9_]*) — so
                    // `2=x` is not an assignment, and a quoted/escaped/expanded key
                    // never qualifies (`key_ok` was cleared, or `raw_text` is None).
                    if w.key_ok
                        && let Some(key) = w.raw_text()
                        && key
                            .chars()
                            .next()
                            .is_some_and(|c| c.is_ascii_alphabetic() || c == '_')
                        && key.chars().all(|c| c.is_ascii_alphanumeric() || c == '_')
                    {
                        w.assign = true;
                    }
                    w.key_ok = false;
                    w.push_literal("=", SegSource::Raw);
                }
                ' ' | '\t' => {
                    chars.next();
                    flush_literal_word(&mut tokens, &mut w);
                }
                '>' => {
                    chars.next();
                    // An fd prefix is recognised ONLY from a pure-`Raw` `1`/`2`
                    // (`raw_text`); a quoted/escaped/expanded `2` is `None` here, so
                    // it can never become a `2>` redirect (injection safety).
                    let raw = w.raw_text();
                    let fd_prefix = match raw.as_deref() {
                        Some("1") => Some("1"),
                        Some("2") => Some("2"),
                        _ => None,
                    };
                    let op_value = if let Some(pfx) = fd_prefix {
                        // The prefix chars ARE the operator, not a word — discard.
                        w = WordAcc::new();
                        lex_redirect_after_gt(pfx, &mut chars)
                    } else {
                        flush_literal_word(&mut tokens, &mut w);
                        lex_redirect_after_gt("", &mut chars)
                    };
                    tokens.push(LiteralToken::Op(op_value));
                }
                '&' => {
                    chars.next();
                    // `&>` / `&>>` only when a LITERAL `&` is immediately followed
                    // by `>`; `&` is then a word-breaking metachar. A lone `&` is a
                    // literal word char (`a&b` stays one word; trailing `&` and
                    // `&&` are split out before tokenizing).
                    if chars.peek() == Some(&'>') {
                        chars.next();
                        flush_literal_word(&mut tokens, &mut w);
                        let op_value = lex_redirect_after_gt("&", &mut chars);
                        tokens.push(LiteralToken::Op(op_value));
                    } else {
                        w.key_ok = false;
                        w.push_literal("&", SegSource::Raw);
                    }
                }
                '<' => {
                    chars.next();
                    flush_literal_word(&mut tokens, &mut w);
                    tokens.push(LiteralToken::Op("<".to_string()));
                }
                _ => {
                    chars.next();
                    // A non-identifier literal char before `=` rules out a `name=`
                    // assignment prefix.
                    if w.key_ok && !(ch.is_ascii_alphanumeric() || ch == '_') {
                        w.key_ok = false;
                    }
                    w.push_literal(&ch.to_string(), SegSource::Raw);
                }
            }
        }
    }

    flush_literal_word(&mut tokens, &mut w);
    tokens
}

// ===========================================================================
// Phase 1.5: bash-style brace expansion. Runs BETWEEN lex and expand, on
// `LiteralToken`s, so it sees structure but never values — brace syntax is
// recognised ONLY in `Raw` literal chars. A quoted/escaped `{`, a `${X}`, and
// any text a Var/CmdSubst resolves to are all inert (exactly like bash, where
// brace expansion precedes every other expansion), so a value can never
// multiply words: the phase-1 injection invariant extends to this pass for
// free. Verified against bash 5.3: `{a,b}` alternation, `{1..5}` / `{a..e}`
// sequences with optional `..incr` (abs value, 0 → 1) and sign-aware zero
// padding, nested groups, leftmost-valid-group recursion; `{}`, `{a}`,
// unmatched braces, and malformed sequences stay literal. A leading
// assignment-prefix word (`x={a,b} cmd`) and a redirect target are NOT
// expanded (bash skips the former; the latter would only ever be a bash
// "ambiguous redirect" error, so it stays literal like a glob target).
// ===========================================================================

/// Hardening caps (the 0.16.3 pattern — a clean fallback, never an OOM or
/// stack overflow): a word may produce at most this many expanded words, a
/// single `{x..y}` sequence at most this many elements, and groups may nest
/// or chain at most `BRACE_DEPTH_CAP` deep. Exceeding any cap leaves the
/// whole word UNEXPANDED (literal braces), never a partial expansion.
const BRACE_EXPANSION_CAP: usize = 10_000;
const BRACE_DEPTH_CAP: usize = 64;

/// One atom of a word under brace expansion: a single RAW literal char (the
/// only text brace syntax can be built from) or an opaque segment — quoted /
/// escaped literal, Var, Tilde, CmdSubst — carried through expansion as data.
#[derive(Debug, Clone, PartialEq)]
enum BraceAtom {
    Raw(char),
    Opaque(Segment),
}

fn explode_for_braces(segments: &[Segment]) -> Vec<BraceAtom> {
    let mut out = Vec::new();
    for seg in segments {
        match (&seg.kind, &seg.source) {
            (SegKind::Literal(s), SegSource::Raw) => {
                out.extend(s.chars().map(BraceAtom::Raw));
            }
            _ => out.push(BraceAtom::Opaque(seg.clone())),
        }
    }
    out
}

/// Rebuild a segment list from expanded atoms, coalescing raw-char runs the
/// same way `WordAcc::push_literal` does. An empty atom list yields an empty
/// segment list — phase 2's unquoted-empty-drop then removes the word, which
/// is exactly bash's `{a,}` behaviour (the empty alternative disappears; a
/// quoted-empty alternative `{a,""}` survives via its opaque segment).
fn rebuild_from_atoms(atoms: &[BraceAtom]) -> Vec<Segment> {
    let mut out: Vec<Segment> = Vec::new();
    for atom in atoms {
        match atom {
            BraceAtom::Raw(c) => {
                if let Some(Segment {
                    kind: SegKind::Literal(s),
                    source: SegSource::Raw,
                }) = out.last_mut()
                {
                    s.push(*c);
                } else {
                    out.push(Segment {
                        kind: SegKind::Literal(c.to_string()),
                        source: SegSource::Raw,
                    });
                }
            }
            BraceAtom::Opaque(seg) => out.push(seg.clone()),
        }
    }
    out
}

/// A parsed `{...}` group: the index of its matching close brace and the
/// alternative atom lists it expands to.
struct BraceGroup {
    close: usize,
    alternatives: Vec<Vec<BraceAtom>>,
}

/// Outcome of trying to parse a brace group at one `{`. The distinction
/// matters: `NoGroup` (malformed — no close, no comma, bad sequence) means
/// "keep scanning, a later/inner group may still expand" (bash parity for
/// `a{b{c,d}}`), while `OverCap` means "abandon expansion of the WHOLE word"
/// — treating an over-cap sequence as merely invalid would let a later group
/// in the same word expand, i.e. exactly the partial expansion the caps
/// promise never to produce.
enum BraceParse {
    NoGroup,
    OverCap,
    Group(BraceGroup),
}

/// Try to parse a VALID brace group whose `{` is at `open`. Valid means: a
/// matching `}` exists (raw chars only, depth-counted) AND the content either
/// has a top-level comma (alternation) or is a well-formed `{x..y[..incr]}`
/// sequence of all-raw chars.
fn parse_brace_group(atoms: &[BraceAtom], open: usize) -> BraceParse {
    let mut depth = 1usize;
    let mut commas: Vec<usize> = Vec::new();
    let mut close = None;
    for (j, atom) in atoms.iter().enumerate().skip(open + 1) {
        if let BraceAtom::Raw(c) = atom {
            match c {
                '{' => depth += 1,
                '}' => {
                    depth -= 1;
                    if depth == 0 {
                        close = Some(j);
                        break;
                    }
                }
                ',' if depth == 1 => commas.push(j),
                _ => {}
            }
        }
    }
    let Some(close) = close else {
        return BraceParse::NoGroup;
    };
    if !commas.is_empty() {
        let mut alternatives = Vec::with_capacity(commas.len() + 1);
        let mut start = open + 1;
        for &c in &commas {
            alternatives.push(atoms[start..c].to_vec());
            start = c + 1;
        }
        alternatives.push(atoms[start..close].to_vec());
        return BraceParse::Group(BraceGroup {
            close,
            alternatives,
        });
    }
    // No comma: only a sequence can make this group valid, and a sequence is
    // pure raw text (a quoted char or `$X` inside disqualifies it).
    let Some(content) = atoms[open + 1..close]
        .iter()
        .map(|a| match a {
            BraceAtom::Raw(c) => Some(*c),
            BraceAtom::Opaque(_) => None,
        })
        .collect::<Option<String>>()
    else {
        return BraceParse::NoGroup;
    };
    match expand_sequence(&content) {
        SequenceParse::Invalid => BraceParse::NoGroup,
        SequenceParse::OverCap => BraceParse::OverCap,
        SequenceParse::Elems(elems) => BraceParse::Group(BraceGroup {
            close,
            alternatives: elems
                .into_iter()
                .map(|s| s.chars().map(BraceAtom::Raw).collect())
                .collect(),
        }),
    }
}

/// Outcome of parsing a `{x..y[..incr]}` sequence body: malformed (the group
/// is not a sequence — stays literal, scanning continues), over the element
/// cap (the whole WORD must stay unexpanded), or the elements.
enum SequenceParse {
    Invalid,
    OverCap,
    Elems(Vec<String>),
}

/// Expand a `{x..y[..incr]}` sequence body (braces stripped). Endpoints are
/// both i64 integers (optional sign; an explicit leading zero turns on
/// bash's sign-aware zero padding to the widest endpoint) or both single
/// ASCII letters. The increment's absolute value is used and 0 means 1
/// (bash parity).
fn expand_sequence(s: &str) -> SequenceParse {
    let parts: Vec<&str> = s.split("..").collect();
    if parts.len() < 2 || parts.len() > 3 {
        return SequenceParse::Invalid;
    }
    let step = match parts.get(2) {
        Some(p) => match p.parse::<i64>() {
            // i64::MIN has no i64 absolute value — `unsigned_abs() as i64`
            // would wrap to a NEGATIVE step and walk the wrong way.
            Ok(i64::MIN) | Err(_) => return SequenceParse::Invalid,
            Ok(0) => 1,
            Ok(raw) => raw.unsigned_abs(),
        },
        None => 1,
    };
    let (a, b) = (parts[0], parts[1]);
    if let (Ok(x), Ok(y)) = (a.parse::<i64>(), b.parse::<i64>()) {
        // `abs_diff` can't overflow; reject over-cap BEFORE allocating.
        if x.abs_diff(y) / step >= BRACE_EXPANSION_CAP as u64 {
            return SequenceParse::OverCap;
        }
        let has_pad = |t: &str| {
            let d = t.strip_prefix(['-', '+']).unwrap_or(t);
            d.len() > 1 && d.starts_with('0')
        };
        let width = if has_pad(a) || has_pad(b) {
            a.len().max(b.len())
        } else {
            0
        };
        let mut out = Vec::new();
        let mut v = x;
        loop {
            out.push(format!("{v:0width$}"));
            let next = if x <= y {
                v.checked_add(step as i64)
            } else {
                v.checked_sub(step as i64)
            };
            match next {
                Some(n) if (x <= y && n <= y) || (x > y && n >= y) => v = n,
                _ => break,
            }
        }
        return SequenceParse::Elems(out);
    }
    // Letter sequence: both endpoints a single ASCII letter.
    let single_alpha = |t: &str| -> Option<u8> {
        let mut it = t.chars();
        match (it.next(), it.next()) {
            (Some(c), None) if c.is_ascii_alphabetic() => Some(c as u8),
            _ => None,
        }
    };
    let (Some(x), Some(y)) = (single_alpha(a), single_alpha(b)) else {
        return SequenceParse::Invalid;
    };
    let step = u8::try_from(step.min(255)).unwrap_or(1);
    let mut out = Vec::new();
    let mut v = x;
    loop {
        out.push((v as char).to_string());
        let next = if x <= y {
            v.checked_add(step)
        } else {
            v.checked_sub(step)
        };
        match next {
            Some(n) if (x <= y && n <= y) || (x > y && n >= y) => v = n,
            _ => break,
        }
    }
    SequenceParse::Elems(out)
}

/// Recursively expand the LEFTMOST valid brace group: for each alternative,
/// the alternative-plus-suffix is expanded again (handling nested groups and
/// later groups in one pass), then the shared prefix is prepended. No valid
/// group → the input is the single result. `None` means a cap was exceeded —
/// the caller abandons expansion and keeps the word literal.
fn brace_expand_atoms(
    atoms: &[BraceAtom],
    budget: &mut usize,
    depth: usize,
) -> Option<Vec<Vec<BraceAtom>>> {
    if depth > BRACE_DEPTH_CAP {
        return None;
    }
    let mut i = 0;
    while i < atoms.len() {
        if atoms[i] == BraceAtom::Raw('{') {
            let group = match parse_brace_group(atoms, i) {
                // Malformed at this `{` — a later/inner `{` may still open a
                // valid group, keep scanning (`a{b{c,d}}` parity).
                BraceParse::NoGroup => {
                    i += 1;
                    continue;
                }
                // An over-cap sequence anywhere fails the WHOLE word — never
                // expand its siblings into a partial result.
                BraceParse::OverCap => return None,
                BraceParse::Group(g) => g,
            };
            let prefix = &atoms[..i];
            let suffix = &atoms[group.close + 1..];
            let mut out = Vec::new();
            for alt in group.alternatives {
                let mut tail = alt;
                tail.extend_from_slice(suffix);
                for expanded_tail in brace_expand_atoms(&tail, budget, depth + 1)? {
                    *budget = budget.checked_sub(1)?;
                    let mut word = prefix.to_vec();
                    word.extend(expanded_tail);
                    out.push(word);
                }
            }
            return Some(out);
        }
        i += 1;
    }
    Some(vec![atoms.to_vec()])
}

/// True for an Op that consumes the NEXT word as a file target (`>`, `2>>`,
/// `<`, `&>`, …). The fd-dup shapes (`2>&1`, `>&2`, …) take no target.
fn op_takes_file_target(op: &str) -> bool {
    !op.contains(">&")
}

/// Phase 1.5 entry point: brace-expand each eligible word into 1..N words.
/// Skips a word in the leading assignment prefix (`x={a,b} cmd` keeps the
/// braces, bash parity) and a redirect-target word (stays literal, like a
/// glob pattern target). A word a cap rejects passes through unexpanded. A
/// word the expansion DID multiply gets the leading-tilde eligibility
/// re-check phase 1 could not do (`{~/x,y}` → a `~/x` word whose `~` must
/// expand, bash parity); pass-through words keep their phase-1 decision.
fn expand_braces(tokens: Vec<LiteralToken>) -> Vec<LiteralToken> {
    let mut out: Vec<LiteralToken> = Vec::with_capacity(tokens.len());
    let mut in_assign_prefix = true;
    let mut next_word_is_target = false;
    for tok in tokens {
        match tok {
            LiteralToken::Op(v) => {
                next_word_is_target = op_takes_file_target(&v);
                out.push(LiteralToken::Op(v));
            }
            LiteralToken::Word { segments, assign } => {
                let is_target = std::mem::take(&mut next_word_is_target);
                if is_target || (assign && in_assign_prefix) {
                    out.push(LiteralToken::Word { segments, assign });
                    continue;
                }
                in_assign_prefix = false;
                let has_raw_brace = segments.iter().any(|seg| {
                    matches!(
                        (&seg.kind, &seg.source),
                        (SegKind::Literal(s), SegSource::Raw) if s.contains('{')
                    )
                });
                if !has_raw_brace {
                    out.push(LiteralToken::Word { segments, assign });
                    continue;
                }
                let atoms = explode_for_braces(&segments);
                let mut budget = BRACE_EXPANSION_CAP;
                match brace_expand_atoms(&atoms, &mut budget, 0) {
                    Some(words) if words.len() != 1 || words[0] != atoms => {
                        for mut word in words {
                            // Re-run phase 1's leading-tilde eligibility on the
                            // GENERATED word: `~` alone or `~/...` at its start.
                            if word.first() == Some(&BraceAtom::Raw('~'))
                                && matches!(word.get(1), None | Some(BraceAtom::Raw('/')))
                            {
                                word[0] = BraceAtom::Opaque(Segment {
                                    kind: SegKind::Tilde,
                                    source: SegSource::Raw,
                                });
                            }
                            out.push(LiteralToken::Word {
                                segments: rebuild_from_atoms(&word),
                                assign: false,
                            });
                        }
                    }
                    // Cap exceeded (None) or no valid group: keep the original.
                    _ => out.push(LiteralToken::Word { segments, assign }),
                }
            }
        }
    }
    out
}

/// Phase 2: resolve the structural tokens into `Tok`s. This is the ONLY pass that
/// reads variable values or `HOME`. It resolves each Var, expands a leading
/// Tilde, concatenates a word's segments, drops an unquoted word that resolved to
/// empty, and stamps `quoted`/`op`/`assign`. It NEVER reconsiders structure — the
/// operator/assign decisions were fixed in phase 1 from literal text alone.
fn expand(tokens: Vec<LiteralToken>, vars: &dyn ShellVarResolver) -> Vec<Tok> {
    let home = std::env::var("HOME").unwrap_or_default();
    let mut out: Vec<Tok> = Vec::new();
    for tok in tokens {
        match tok {
            LiteralToken::Op(value) => out.push(Tok {
                value,
                quoted: false,
                op: true,
                assign: false,
            }),
            LiteralToken::Word { segments, assign } => {
                let mut value = String::new();
                let mut quoted = false;
                // The word survives iff something gave it content: any literal
                // segment (incl. an empty quoted/escaped one), a non-empty Var, or
                // a Tilde. A var-only word whose vars all resolve empty is dropped
                // (unquoted-empty-drop), matching the old `word_started`.
                let mut word_started = false;
                for seg in segments {
                    if seg.source.is_quoted() {
                        quoted = true;
                    }
                    match seg.kind {
                        SegKind::Literal(text) => {
                            value.push_str(&text);
                            word_started = true;
                        }
                        SegKind::Var { name, .. } => {
                            if !name.is_empty() {
                                let v = vars.resolve(&name).unwrap_or_default();
                                if !v.is_empty() {
                                    word_started = true;
                                }
                                value.push_str(&v);
                            }
                        }
                        SegKind::CmdSubst { cmd } => {
                            // `None` (default / non-live resolver) suppresses the
                            // substitution entirely; `Some` splices captured stdout
                            // as DATA. Like `Var`, a non-empty result starts the
                            // word so a bare `$(cmd)` survives the empty-drop.
                            if let Some(out) = vars.command_subst(&cmd) {
                                if !out.is_empty() {
                                    word_started = true;
                                }
                                value.push_str(&out);
                            }
                        }
                        SegKind::Tilde => {
                            if home.is_empty() {
                                value.push('~');
                            } else {
                                value.push_str(&home);
                            }
                            word_started = true;
                        }
                    }
                }
                if word_started {
                    out.push(Tok {
                        value,
                        quoted,
                        op: false,
                        assign,
                    });
                }
            }
        }
    }
    out
}

/// Tokenize a single command segment into typed `Tok`s via the phased
/// pipeline: `lex_literal` decides all structure from literal text (no resolver),
/// `expand_braces` multiplies words whose RAW literal text carries a valid brace
/// group, then `expand` fills in variable/tilde values. The signature and
/// `Vec<Tok>` contract are unchanged, so `parse_segment` and everything
/// downstream are untouched. This is the injection-safe boundary — redirect
/// operators, fd prefixes, `name=` assignments, AND brace groups are recognised
/// ONLY from literal input, so a variable's value is always DATA and can never
/// become syntax or multiply words. Single quotes are fully literal; an unquoted
/// empty expansion produces no word.
fn shell_tokenize(s: &str, vars: &dyn ShellVarResolver) -> Vec<Tok> {
    expand(expand_braces(lex_literal(s)), vars)
}

/// Result of executing a pipeline.
pub enum PipelineResult {
    /// Foreground command completed with exit status.
    Done(ExitStatus),
    /// Background command spawned — return the child process.
    Background(Child),
}

/// Execute a pipeline.
/// Build a `Command` for a segment's program, self-resolving the `mix`
/// command to THIS executable. A mix-login-shell node runs ssh commands via
/// `mix -c`, and `/opt/cosmix/bin` is not always on the non-interactive PATH
/// (e.g. a node whose sshd has `UsePAM` off, so `/etc/environment` is not
/// applied), so a bare `mix` would not be found. Resolving to current_exe
/// makes `ssh host 'mix status'` / `mix --version` re-enter this binary
/// regardless of PATH. The `-c`/pipeline spawn paths reach here for `mix`,
/// including a REPL `mix <subcmd>` line carrying plumbing (pipe/redirect/&/
/// env prefix), which since 0.61.1 deliberately falls through to external
/// execution; only a bare non-plumbed `mix <subcmd>` is intercepted by the
/// REPL's in-process meta arm, and the sourced-file ShellHandler rejects
/// `mix` (UNSUPPORTED_BUILTINS).
fn command_for(program: &str) -> Command {
    if program == "mix"
        && let Ok(exe) = std::env::current_exe()
    {
        return Command::new(exe);
    }
    Command::new(program)
}

pub fn execute_pipeline(pipeline: &Pipeline) -> io::Result<PipelineResult> {
    let n = pipeline.segments.len();

    if n == 1 {
        return execute_single(&pipeline.segments[0], pipeline.background);
    }

    // Multi-segment pipeline
    let mut children: Vec<Child> = Vec::new();

    // Spawn every segment inside one fallible block: when a LATER segment
    // fails (bad redirect path, missing binary), the already-spawned
    // upstream children must not be dropped un-waited — in a long-lived
    // REPL/login shell each dropped `Child` is a zombie forever.
    let spawned = (|| -> io::Result<()> {
        let mut prev_stdout: Option<Stdio> = None;

        for (i, seg) in pipeline.segments.iter().enumerate() {
            let is_last = i == n - 1;
            let expanded = expand_args_globs(&seg.args, &seg.quoted);
            let mut cmd = command_for(&seg.program);
            cmd.args(&expanded);
            for (k, v) in &seg.env_vars {
                cmd.env(k, v);
            }

            // Stdin: pipe from previous command, or redirect from file
            if let Some(prev) = prev_stdout.take() {
                cmd.stdin(prev);
            } else {
                for redir in &seg.redirects {
                    if let Redirect::StdinFrom(path) = redir {
                        let f = File::open(path)?;
                        cmd.stdin(Stdio::from(f));
                    }
                }
            }

            // Non-last segments pipe stdout to the next command; the last inherits.
            // All stdout/stderr file + fd-dup redirects then replay in parse order
            // in the child — overriding the pipe fd on a non-last segment when an
            // explicit `>f` is present (so `a >f | b` sends a's stdout to f).
            if !is_last {
                cmd.stdout(Stdio::piped());
            }
            apply_output_redirects(&mut cmd, &seg.redirects)?;

            let mut child = cmd.spawn()?;

            if !is_last && let Some(stdout) = child.stdout.take() {
                prev_stdout = Some(Stdio::from(stdout));
            }

            children.push(child);
        }
        Ok(())
    })();

    if let Err(e) = spawned {
        // Kill + reap the upstream children before surfacing the error, so
        // a failed pipeline never accumulates zombies in the shell process.
        for child in &mut children {
            let _ = child.kill();
            let _ = child.wait();
        }
        return Err(e);
    }

    if pipeline.background {
        // Return last child for job tracking; wait on the rest
        let last = children.pop().unwrap();
        for mut child in children {
            let _ = child.wait();
        }
        return Ok(PipelineResult::Background(last));
    }

    // Wait for all children, return last exit status
    let mut last_status = None;
    for mut child in children {
        last_status = Some(child.wait()?);
    }
    Ok(PipelineResult::Done(last_status.unwrap()))
}

fn execute_single(seg: &PipeSegment, background: bool) -> io::Result<PipelineResult> {
    let expanded = expand_args_globs(&seg.args, &seg.quoted);
    let mut cmd = command_for(&seg.program);
    cmd.args(&expanded);
    for (k, v) in &seg.env_vars {
        cmd.env(k, v);
    }

    for redir in &seg.redirects {
        if let Redirect::StdinFrom(path) = redir {
            let f = File::open(path)?;
            cmd.stdin(Stdio::from(f));
        }
    }

    apply_output_redirects(&mut cmd, &seg.redirects)?;

    if background {
        let child = cmd.spawn()?;
        return Ok(PipelineResult::Background(child));
    }

    let status = cmd.status()?;
    Ok(PipelineResult::Done(status))
}

/// Expand glob patterns (`*`, `?`, `[`) in arguments.
/// Quoted arguments are never expanded. If a pattern matches nothing, it's kept as-is.
fn expand_args_globs(args: &[String], quoted: &[bool]) -> Vec<String> {
    let mut expanded = Vec::new();
    for (i, arg) in args.iter().enumerate() {
        let is_quoted = quoted.get(i).copied().unwrap_or(false);
        if !is_quoted && (arg.contains('*') || arg.contains('?') || arg.contains('[')) {
            match glob::glob(arg) {
                Ok(paths) => {
                    let mut matches: Vec<String> = paths
                        .filter_map(|p| p.ok())
                        .map(|p| p.to_string_lossy().to_string())
                        .collect();
                    if matches.is_empty() {
                        expanded.push(arg.clone());
                    } else {
                        matches.sort();
                        expanded.extend(matches);
                    }
                }
                Err(_) => expanded.push(arg.clone()),
            }
        } else {
            expanded.push(arg.clone());
        }
    }
    expanded
}

/// Apply ALL stdout/stderr redirects (file targets AND fd dups) for one segment
/// in a single `pre_exec` `dup2` sequence run in PARSE ORDER in the child. This
/// honours ordering exactly like a real shell: `>f 2>&1` sends both to `f`, the
/// unusual `2>&1 >f` keeps stderr on the original stdout, and a later `2>err`
/// overrides an earlier `2>&1`.
///
/// Files are opened in the PARENT (and owned by the op list moved into the
/// closure, so their descriptors stay valid until the dups complete); the child
/// runs ONLY async-signal-safe `dup2`/`as_raw_fd` — never `open` or alloc —
/// which is required because the evaluator parent is multi-threaded (no
/// malloc-after-fork). `<file` stdin and the pipeline pipe fds stay on `Command`;
/// this governs only fds 1/2, so on a non-last pipeline segment an explicit `>f`
/// correctly overrides the pipe `Command` set on fd 1.
fn apply_output_redirects(cmd: &mut Command, redirects: &[Redirect]) -> io::Result<()> {
    // Each op is `dup2(oldfd, newfd)` in order. A file op owns its opened File
    // so the source descriptor lives until the child has dup'd it.
    enum Op {
        Dup { newfd: i32, oldfd: i32 },
        File { newfd: i32, file: File },
    }
    let mut ops: Vec<Op> = Vec::new();
    for redir in redirects {
        match redir {
            Redirect::StdoutOverwrite(path) => ops.push(Op::File {
                newfd: 1,
                file: File::create(path)?,
            }),
            Redirect::StdoutAppend(path) => ops.push(Op::File {
                newfd: 1,
                file: OpenOptions::new().append(true).create(true).open(path)?,
            }),
            Redirect::StderrOverwrite(path) => ops.push(Op::File {
                newfd: 2,
                file: File::create(path)?,
            }),
            Redirect::StderrAppend(path) => ops.push(Op::File {
                newfd: 2,
                file: OpenOptions::new().append(true).create(true).open(path)?,
            }),
            Redirect::StderrToStdout => ops.push(Op::Dup { newfd: 2, oldfd: 1 }), // 2>&1
            Redirect::StdoutToStderr => ops.push(Op::Dup { newfd: 1, oldfd: 2 }), // 1>&2
            // `&>file` ≡ `>file 2>&1`: point fd 1 at the file, then dup fd 2 onto
            // it. `&>>file` is the append form. Both push two ops so the parse-
            // order dup replay keeps stderr following stdout to the same target.
            Redirect::BothOverwrite(path) => {
                ops.push(Op::File {
                    newfd: 1,
                    file: File::create(path)?,
                });
                ops.push(Op::Dup { newfd: 2, oldfd: 1 });
            }
            Redirect::BothAppend(path) => {
                ops.push(Op::File {
                    newfd: 1,
                    file: OpenOptions::new().append(true).create(true).open(path)?,
                });
                ops.push(Op::Dup { newfd: 2, oldfd: 1 });
            }
            Redirect::StdinFrom(_) => {} // handled on Command
        }
    }
    if ops.is_empty() {
        return Ok(());
    }
    use std::os::unix::io::AsRawFd;
    use std::os::unix::process::CommandExt;
    // SAFETY: runs in the forked child before exec; only async-signal-safe
    // `dup2`/`as_raw_fd` (no alloc/locks). `ops` is moved in, so its owned Files
    // keep the source descriptors open across the dups; iterating it never allocs.
    unsafe {
        cmd.pre_exec(move || {
            for op in &ops {
                let (newfd, oldfd) = match op {
                    Op::Dup { newfd, oldfd } => (*newfd, *oldfd),
                    Op::File { newfd, file } => (*newfd, file.as_raw_fd()),
                };
                if libc::dup2(oldfd, newfd) < 0 {
                    return Err(std::io::Error::last_os_error());
                }
            }
            Ok(())
        });
    }
    Ok(())
}

#[cfg(test)]
mod redirect_tests {
    use super::*;

    /// Resolver that expands every `$VAR` to the literal string "2" — used to
    /// prove an expanded value can never become a redirect fd.
    struct TwoVars;
    impl ShellVarResolver for TwoVars {
        fn resolve(&self, _name: &str) -> Option<String> {
            Some("2".to_string())
        }
    }

    fn redirs(line: &str) -> Vec<Redirect> {
        let p = parse_pipeline(line, &NoVars).expect("parse");
        assert_eq!(p.segments.len(), 1, "single segment expected for: {line}");
        p.segments.into_iter().next().unwrap().redirects
    }

    #[test]
    fn stderr_to_file_overwrite() {
        // Regression: `2` must NOT leak as an arg; this used to parse as
        // `echo hi 2 >/dev/null`.
        let p = parse_pipeline("echo hi 2>/dev/null", &NoVars).unwrap();
        let seg = &p.segments[0];
        assert_eq!(seg.program, "echo");
        assert_eq!(seg.args, vec!["hi".to_string()]); // no stray "2"
        assert_eq!(
            seg.redirects,
            vec![Redirect::StderrOverwrite("/dev/null".to_string())]
        );
    }

    #[test]
    fn stderr_append() {
        assert_eq!(
            redirs("cmd 2>>log"),
            vec![Redirect::StderrAppend("log".to_string())]
        );
    }

    #[test]
    fn stderr_to_stdout_dup() {
        // The originally-reported case — used to hard-error "not yet supported".
        assert_eq!(redirs("ls /nope 2>&1"), vec![Redirect::StderrToStdout]);
    }

    #[test]
    fn stdout_to_stderr_dup() {
        assert_eq!(redirs("echo x 1>&2"), vec![Redirect::StdoutToStderr]);
    }

    #[test]
    fn bare_dup_to_stderr_is_implicit_fd1() {
        // `>&2` is the common `cmd >&2` idiom — shorthand for `1>&2`. Regression:
        // `&2` must NOT become a filename (it used to create a junk file `&2`).
        let p = parse_pipeline("echo hi >&2", &NoVars).unwrap();
        let seg = &p.segments[0];
        assert_eq!(seg.program, "echo");
        assert_eq!(seg.args, vec!["hi".to_string()]); // no stray "&2"
        assert_eq!(seg.redirects, vec![Redirect::StdoutToStderr]);
    }

    #[test]
    fn bare_dup_to_stdout_is_a_noop() {
        // `>&1` ≡ `1>&1`: stdout onto itself — a no-op, so no redirect and no
        // stray `&1` arg / junk file.
        let p = parse_pipeline("echo hi >&1", &NoVars).unwrap();
        assert_eq!(p.segments[0].args, vec!["hi".to_string()]);
        assert!(p.segments[0].redirects.is_empty());
    }

    #[test]
    fn bare_dup_then_file_order() {
        // `>f >&2` keeps parse order: stdout to f, then stdout(=fd1) dup'd to fd2.
        assert_eq!(
            redirs("cmd >f >&2"),
            vec![
                Redirect::StdoutOverwrite("f".to_string()),
                Redirect::StdoutToStderr,
            ]
        );
    }

    #[test]
    fn unsupported_fd_dup_errors_not_junk() {
        // `>&3` / `>&-` / `1>&3` are recognised as fd-dup operators with an
        // unsupported target — they must ERROR, never create a junk file `&3`
        // or silently no-op.
        for line in [
            "echo hi >&3",
            "echo hi >&-",
            "echo hi 1>&3",
            "cmd 2>&3",
            "echo hi >&10", // multi-digit: must consume the whole run, then error
            "cmd 2>&20",
        ] {
            let r = parse_pipeline(line, &NoVars);
            assert!(r.is_err(), "{line:?} should be a parse error, got {r:?}");
            assert!(
                r.unwrap_err().contains("unsupported fd"),
                "{line:?} should report an unsupported fd"
            );
        }
    }

    #[test]
    fn explicit_one_prefix_is_stdout() {
        assert_eq!(
            redirs("cmd 1>out"),
            vec![Redirect::StdoutOverwrite("out".to_string())]
        );
    }

    #[test]
    fn plain_stdout_redirect_unchanged() {
        assert_eq!(
            redirs("echo hi >f"),
            vec![Redirect::StdoutOverwrite("f".to_string())]
        );
    }

    #[test]
    fn redirect_then_dup_both_to_file() {
        // `>f 2>&1` → stdout to file, then stderr follows stdout.
        assert_eq!(
            redirs("cmd >f 2>&1"),
            vec![
                Redirect::StdoutOverwrite("f".to_string()),
                Redirect::StderrToStdout,
            ]
        );
    }

    #[test]
    fn expanded_fd_is_not_a_redirect_prefix() {
        // `$X` → "2" via expansion, but `current_dynamic` blocks fd-prefix
        // recognition: `$X>f` is a plain stdout redirect with "2" as an arg,
        // never a `2>` stderr redirect.
        let p = parse_pipeline("echo $X>f", &TwoVars).unwrap();
        let seg = &p.segments[0];
        assert_eq!(seg.args, vec!["2".to_string()]);
        assert_eq!(
            seg.redirects,
            vec![Redirect::StdoutOverwrite("f".to_string())]
        );
    }

    #[test]
    fn dup_in_pipeline_segment() {
        let p = parse_pipeline("ls /nope 2>&1 | head", &NoVars).unwrap();
        assert_eq!(p.segments.len(), 2);
        assert_eq!(p.segments[0].redirects, vec![Redirect::StderrToStdout]);
    }

    // Redirects must keep PARSE ORDER so `apply_output_redirects` replays them
    // like a real shell (a later same-fd redirect overrides an earlier one).
    #[test]
    fn order_preserved_dup_then_file() {
        assert_eq!(
            redirs("cmd 2>&1 2>err"),
            vec![
                Redirect::StderrToStdout,
                Redirect::StderrOverwrite("err".to_string()),
            ]
        );
    }

    #[test]
    fn order_preserved_dup_then_stdout_file() {
        assert_eq!(
            redirs("cmd 2>&1 >f"),
            vec![
                Redirect::StderrToStdout,
                Redirect::StdoutOverwrite("f".to_string()),
            ]
        );
    }

    // P0 property-based coverage (mix tokenizer fuzz corpus — see
    // _doc/planned/mix-tokenizer-fuzz-corpus.md in the cosmix hub): tokenizer
    // robustness (no panic) + the injection-safety invariant (an expanded
    // `$VAR` value can never become syntax), generalized from the hand cases.
    use proptest::prelude::*;

    /// A resolver that expands every `$VAR` to one fixed value — used to prove
    /// the parsed STRUCTURE is independent of what a variable expands to.
    struct ConstVar(String);
    impl ShellVarResolver for ConstVar {
        fn resolve(&self, _name: &str) -> Option<String> {
            Some(self.0.clone())
        }
    }

    /// Deterministic command-substitution mock: resolves `$VAR` to "V" and a
    /// `$(cmd)` to the inner command text wrapped as `<cmd>`, so tests can
    /// assert splicing WITHOUT spawning a real process (the live `Evaluator`
    /// runs `/bin/sh`; structural/test resolvers must not).
    struct SubstVar;
    impl ShellVarResolver for SubstVar {
        fn resolve(&self, _name: &str) -> Option<String> {
            Some("V".to_string())
        }
        fn command_subst(&self, cmd: &str) -> Option<String> {
            Some(format!("<{cmd}>"))
        }
    }

    /// Per-segment skeleton: parse-time arg count, the sequence of redirect KINDS
    /// (discriminant only — the file-target string legitimately carries an
    /// expanded value), and the env-var count.
    type SegSkel = (usize, Vec<std::mem::Discriminant<Redirect>>, usize);
    /// Pipeline skeleton: the `background` flag (trailing `&`) plus the segment
    /// skeletons. This is the parse-time syntactic shape an expanded `$VAR` value
    /// must NEVER be able to change — including whether the pipeline backgrounds.
    type PipeSkel = (bool, Vec<SegSkel>);

    fn skeleton(p: &Pipeline) -> PipeSkel {
        (
            p.background,
            p.segments
                .iter()
                .map(|s| {
                    (
                        s.args.len(),
                        s.redirects.iter().map(std::mem::discriminant).collect(),
                        s.env_vars.len(),
                    )
                })
                .collect(),
        )
    }

    /// Skeleton of a parsed command LIST: each item's connector + pipeline
    /// skeleton. An expanded value must never add/remove a list item or change a
    /// connector — i.e. cannot smuggle a `;` / `&&` / `||` to chain a command.
    fn list_skeleton(list: &[(Connector, Pipeline)]) -> Vec<(Connector, PipeSkel)> {
        list.iter().map(|(c, p)| (*c, skeleton(p))).collect()
    }

    /// Values a `$VAR` might expand to that an attacker would WANT to become
    /// syntax — known-dangerous shell tokens plus random non-empty strings.
    /// EXCLUDED, deliberately: empty/whitespace (an unquoted empty expansion
    /// legitimately drops a word) and the glob metacharacters `* ? [` (unquoted
    /// glob expansion is a separate EXECUTE-time step — bash-consistent, expands
    /// only to existing filenames, never an operator or command). The invariant
    /// under test is parse-time syntax, so those are out of scope here.
    fn hostile_value() -> impl Strategy<Value = String> {
        prop_oneof![
            Just("&>".to_string()),
            Just("&>>".to_string()),
            Just("2>&1".to_string()),
            Just("| cat".to_string()),
            Just(";rm".to_string()),
            Just("&& rm".to_string()),
            Just("|| rm -rf /".to_string()),
            Just("$(touch pwned)".to_string()),
            Just("> /etc/passwd".to_string()),
            // Assignment-shaped values, to prove an expanded value in a leading
            // position never becomes an env assignment (the key must be literal).
            Just("a=b".to_string()),
            Just("PATH".to_string()),
            Just("PATH=/evil".to_string()),
            "[^\\s*?\\[]{1,40}",
        ]
    }

    // `$X` sitting in operator-adjacent positions — including LEADING positions
    // where a forged assignment would land: the value must never form a redirect
    // operator, an env assignment, a `|` segment split, or a parse-time word.
    const INJECTION_TEMPLATES: &[&str] = &[
        "echo $X",
        "echo $X foo",
        "echo foo$X",
        "echo $X > out",
        "cmd $X file",
        "a=$X echo hi",
        "$X echo hi",     // expanded value as the program — never an assignment
        "$X=bar echo hi", // expanded key — `PATH=bar` here must stay the program
        "echo $X | wc",
        "cmd >$X",
        "echo \"$X\" foo",
    ];

    proptest! {
        // Robustness: neither parser may panic on arbitrary input.
        #[test]
        fn parse_never_panics(s in ".{0,200}") {
            let _ = parse_pipeline(&s, &NoVars);
            let _ = parse_command_list(&s, &NoVars);
        }

        // Injection safety (pipeline): a hostile expanded value cannot change the
        // parse-time SYNTACTIC structure — it never introduces or removes a
        // redirect operator, an env assignment, a `|` segment split, or a
        // parse-time word (it is always one data word). The benign parse MUST
        // succeed for every template, so a template that silently stops parsing
        // fails loudly rather than passing vacuously.
        #[test]
        fn expansion_cannot_change_structure(v in hostile_value()) {
            for tmpl in INJECTION_TEMPLATES {
                let safe = parse_pipeline(tmpl, &ConstVar("SAFE".to_string()));
                prop_assert!(safe.is_ok(), "template {:?} must parse with a benign value", tmpl);
                let hostile = parse_pipeline(tmpl, &ConstVar(v.clone()));
                prop_assert!(
                    hostile.is_ok(),
                    "template {:?} failed to parse under value {:?}",
                    tmpl,
                    v
                );
                prop_assert_eq!(
                    skeleton(&hostile.unwrap()),
                    skeleton(&safe.unwrap()),
                    "template {:?} changed structure under value {:?}",
                    tmpl,
                    v
                );
            }
        }

        // Injection safety (command list): a hostile expanded value cannot add or
        // remove a list item or change a connector — the literal `;`/`&&`/`||`
        // split runs BEFORE per-piece expansion, so a value's separators stay
        // inert data. This is the command-chaining injection class, which the
        // pipeline-only property above does not reach.
        #[test]
        fn expansion_cannot_inject_command_list_items(v in hostile_value()) {
            const LIST_TEMPLATES: &[&str] = &[
                "echo $X",
                "echo $X; echo done",
                "true && echo $X",
                "echo $X || echo fallback",
                "a=$X cmd; cmd2",
                "echo $X | wc; ls",
            ];
            for tmpl in LIST_TEMPLATES {
                let safe = parse_command_list(tmpl, &ConstVar("SAFE".to_string()));
                prop_assert!(safe.is_ok(), "list template {:?} must parse with a benign value", tmpl);
                let hostile = parse_command_list(tmpl, &ConstVar(v.clone()));
                prop_assert!(
                    hostile.is_ok(),
                    "list template {:?} failed to parse under value {:?}",
                    tmpl,
                    v
                );
                prop_assert_eq!(
                    list_skeleton(&hostile.unwrap()),
                    list_skeleton(&safe.unwrap()),
                    "list template {:?} changed structure under value {:?}",
                    tmpl,
                    v
                );
            }
        }

        // Quote integrity: arbitrary content inside single quotes is one fully
        // literal argument — no operator recognized, no expansion, no split.
        #[test]
        fn single_quoted_is_one_literal_word(content in "[^'\\n\\r]{0,60}") {
            let line = format!("echo '{content}'");
            let p = parse_pipeline(&line, &NoVars).expect("single-quoted parse");
            prop_assert_eq!(p.segments.len(), 1);
            prop_assert_eq!(&p.segments[0].args, &vec![content.clone()]);
            prop_assert!(p.segments[0].redirects.is_empty());
        }
    }

    // ===================================================================
    // P2 phase-targeted tests (mix shell tokenizer two-phase —
    // _doc/planned/mix-shell-tokenizer-two-phase.md in the cosmix hub). With the
    // legacy oracle deleted, these pin the two passes directly: `lex_literal`
    // structure from LITERAL text (no resolver), `expand` over hand-built token
    // streams (resolution / empty-drop / glob-skip), plus the two structural
    // properties — operators are resolver-independent (injection safety stated
    // cleanly), and `name=` eligibility comes only from a pure-`Raw` literal run.
    // ===================================================================

    fn lit(t: &str, src: SegSource) -> Segment {
        Segment {
            kind: SegKind::Literal(t.to_string()),
            source: src,
        }
    }
    fn var(name: &str, braced: bool, src: SegSource) -> Segment {
        Segment {
            kind: SegKind::Var {
                name: name.to_string(),
                braced,
            },
            source: src,
        }
    }
    fn word(segments: Vec<Segment>, assign: bool) -> LiteralToken {
        LiteralToken::Word { segments, assign }
    }

    // --- lex_literal: all structure decided from LITERAL text, no resolver ---
    #[test]
    fn lex_literal_structures() {
        use SegSource::*;
        assert_eq!(
            lex_literal("echo hi"),
            vec![
                word(vec![lit("echo", Raw)], false),
                word(vec![lit("hi", Raw)], false),
            ]
        );
        // an unquoted var is a Var segment — NOT resolved in phase 1
        assert_eq!(
            lex_literal("echo $X"),
            vec![
                word(vec![lit("echo", Raw)], false),
                word(vec![var("X", false, Raw)], false),
            ]
        );
        // literal `name=` prefix flags the word; key + `=` coalesce into one Raw seg
        assert_eq!(
            lex_literal("name=v cmd"),
            vec![
                word(vec![lit("name=v", Raw)], true),
                word(vec![lit("cmd", Raw)], false),
            ]
        );
        // raw `2>` is the fd-prefixed stderr op — the `2` is consumed, not a word
        assert_eq!(
            lex_literal("cmd 2>f"),
            vec![
                word(vec![lit("cmd", Raw)], false),
                LiteralToken::Op("2>".to_string()),
                word(vec![lit("f", Raw)], false),
            ]
        );
        // escaped `\2` is Escaped DATA — not an fd prefix, so a plain `>`
        assert_eq!(
            lex_literal("\\2>f"),
            vec![
                word(vec![lit("2", Escaped)], false),
                LiteralToken::Op(">".to_string()),
                word(vec![lit("f", Raw)], false),
            ]
        );
        // double-quoted var: an empty opening Quoted seg, then the Var (both Double)
        assert_eq!(
            lex_literal("\"$X\""),
            vec![word(
                vec![lit("", DoubleQuoted), var("X", false, DoubleQuoted)],
                false
            )]
        );
        // `${}` is an empty-name braced Var (phase 2 drops it to nothing)
        assert_eq!(
            lex_literal("${}"),
            vec![word(vec![var("", true, Raw)], false)]
        );
    }

    fn cmdsub(cmd: &str, src: SegSource) -> Segment {
        Segment {
            kind: SegKind::CmdSubst {
                cmd: cmd.to_string(),
            },
            source: src,
        }
    }

    // --- lex_literal: `$(...)` command substitution structure ---
    #[test]
    fn lex_literal_cmdsubst() {
        use SegSource::*;
        // bare `$(cmd)` → a Raw CmdSubst segment carrying the inner text
        assert_eq!(
            lex_literal("echo $(hostname)"),
            vec![
                word(vec![lit("echo", Raw)], false),
                word(vec![cmdsub("hostname", Raw)], false),
            ]
        );
        // nested parens balance into the inner command text
        assert_eq!(
            lex_literal("$(a $(b) c)"),
            vec![word(vec![cmdsub("a $(b) c", Raw)], false)]
        );
        // inside double quotes the subst is DoubleQuoted (quoted word, no split).
        // The opening-quote empty literal coalesces with the following `x`.
        assert_eq!(
            lex_literal("\"x$(a)y\""),
            vec![word(
                vec![
                    lit("x", DoubleQuoted),
                    cmdsub("a", DoubleQuoted),
                    lit("y", DoubleQuoted),
                ],
                false
            )]
        );
        // inside single quotes `$(...)` is fully literal — NOT a CmdSubst
        // (the opening-quote empty literal coalesces with the content).
        assert_eq!(
            lex_literal("'$(a)'"),
            vec![word(vec![lit("$(a)", SingleQuoted)], false)]
        );
        // `$((...))` arithmetic stays literal text (unchanged behavior)
        assert_eq!(
            lex_literal("$((1+2))"),
            vec![word(vec![lit("$((1+2))", Raw)], false)]
        );
        // an UNTERMINATED `$(` keeps the literal text — never a CmdSubst
        assert_eq!(
            lex_literal("$(oops"),
            vec![word(vec![lit("$(oops", Raw)], false)]
        );
    }

    // --- lex_literal: the `$(...)` span scan is quote-aware ---
    // Only the DELIMITING of the literal span changes here; the body stays
    // unexamined data (phase 1 is value-independent).
    #[test]
    fn lex_literal_cmdsubst_quote_aware_span() {
        use SegSource::*;
        // a `)` inside a double-quoted word does NOT close the subst
        // (used to terminate early at the quoted paren)
        assert_eq!(
            lex_literal("$(echo \")\")"),
            vec![word(vec![cmdsub("echo \")\"", Raw)], false)]
        );
        assert_eq!(
            lex_literal("$(echo \"a)b\")"),
            vec![word(vec![cmdsub("echo \"a)b\"", Raw)], false)]
        );
        // ... a single-quoted word likewise
        assert_eq!(
            lex_literal("$(echo ')')"),
            vec![word(vec![cmdsub("echo ')'", Raw)], false)]
        );
        // a backslash-escaped `)` is literal text, not the closer
        assert_eq!(
            lex_literal("$(echo \\))"),
            vec![word(vec![cmdsub("echo \\)", Raw)], false)]
        );
        // a quoted `(` must not inflate the depth count
        assert_eq!(
            lex_literal("$(echo \"(\") x"),
            vec![
                word(vec![cmdsub("echo \"(\"", Raw)], false),
                word(vec![lit("x", Raw)], false),
            ]
        );
        // nesting still balances via UNQUOTED parens (preserved behavior)
        assert_eq!(
            lex_literal("$(echo $(echo hi))"),
            vec![word(vec![cmdsub("echo $(echo hi)", Raw)], false)]
        );
        // inside an outer double-quoted word, the subst's inner quotes are a
        // fresh context — they don't disturb the outer quote state
        assert_eq!(
            lex_literal("\"x$(echo \")\")y\""),
            vec![word(
                vec![
                    lit("x", DoubleQuoted),
                    cmdsub("echo \")\"", DoubleQuoted),
                    lit("y", DoubleQuoted),
                ],
                false
            )]
        );
        // unterminated with an open quote stays literal — never a CmdSubst
        assert_eq!(
            lex_literal("$(echo \"a"),
            vec![word(vec![lit("$(echo \"a", Raw)], false)]
        );
    }

    // --- expand: command substitution splices captured text as DATA ---
    #[test]
    fn expand_cmdsubst_splices() {
        use SegSource::*;
        // a bare `$(cmd)` resolves via `command_subst` and starts the word
        assert_eq!(
            expand(vec![word(vec![cmdsub("hi", Raw)], false)], &SubstVar),
            vec![tok("<hi>", false, false, false)]
        );
        // adjacent literal + subst concatenate into one word
        assert_eq!(
            expand(
                vec![word(vec![lit("p", Raw), cmdsub("x", Raw)], false)],
                &SubstVar
            ),
            vec![tok("p<x>", false, false, false)]
        );
        // `None` from command_subst (default / structural) suppresses the
        // substitution: a subst-only word drops out entirely.
        assert!(
            expand(
                vec![word(vec![cmdsub("x", Raw)], false)],
                &ConstVar("v".to_string())
            )
            .is_empty()
        );
    }

    // --- splitters treat a `$(...)` span as opaque ---
    #[test]
    fn cmdsubst_span_is_opaque_to_splitters() {
        // inner `|` belongs to the subst, not the outer pipeline
        assert_eq!(split_on_pipes("echo $(a | b)"), vec!["echo $(a | b)"]);
        // outer `|` still splits when it is NOT inside a subst
        assert_eq!(split_on_pipes("echo $(a) | b"), vec!["echo $(a) ", " b"]);
        // inner `;` / `&&` belong to the subst
        assert_eq!(
            split_on_control_ops("echo $(a; b)")
                .iter()
                .map(|(_, s)| *s)
                .collect::<Vec<_>>(),
            vec!["echo $(a; b)"]
        );
        // outer `;` still splits
        assert_eq!(
            split_on_control_ops("echo $(a) ; b")
                .iter()
                .map(|(_, s)| *s)
                .collect::<Vec<_>>(),
            vec!["echo $(a) ", " b"]
        );
        // QUOTE-AWARE span end: a `)` inside quotes within the subst must NOT
        // close it early and let a following `;`/`|` mis-split one real span.
        assert_eq!(
            split_on_pipes(r#"echo $(echo ")" x)"#),
            vec![r#"echo $(echo ")" x)"#]
        );
        assert_eq!(
            split_on_control_ops(r#"echo $(echo ");" x)"#)
                .iter()
                .map(|(_, s)| *s)
                .collect::<Vec<_>>(),
            vec![r#"echo $(echo ");" x)"#]
        );
        // single-quoted `)` and an escaped `)` likewise stay inside the span
        assert_eq!(
            split_on_pipes(r#"echo $(echo ')' | x)"#),
            vec![r#"echo $(echo ')' | x)"#]
        );
        assert_eq!(
            split_on_pipes(r"echo $(echo \) | x)"),
            vec![r"echo $(echo \) | x)"]
        );
        // nested substitution: the inner `$(...)` is consumed within the outer
        assert_eq!(
            split_on_pipes("a $(b $(c | d) e) | f"),
            vec!["a $(b $(c | d) e) ", " f"]
        );
    }

    // --- lazy list execution never runs a `$(...)` in a skipped branch ---
    #[test]
    fn lazy_command_list_skips_substitution_in_short_circuited_branch() {
        use std::cell::Cell;
        use std::path::Path;
        // `/bin/true` + `/bin/false` give a deterministic exit without PATH.
        if !(Path::new("/bin/true").exists() && Path::new("/bin/false").exists()) {
            return;
        }
        struct SpyVars {
            ran: Cell<bool>,
        }
        impl ShellVarResolver for SpyVars {
            fn resolve(&self, _name: &str) -> Option<String> {
                Some("x".to_string())
            }
            fn command_subst(&self, _cmd: &str) -> Option<String> {
                self.ran.set(true);
                Some(String::new())
            }
        }

        // `false && echo $(side)` — the AND branch is skipped, so its `$(...)`
        // must NOT be expanded (no spawn). Exit code is `false`'s (1).
        let spy = SpyVars {
            ran: Cell::new(false),
        };
        let items = split_command_list("/bin/false && echo $(side)").unwrap();
        let code = execute_command_list_outcome(&items, &spy, None).code;
        assert!(
            !spy.ran.get(),
            "a `$(...)` in a short-circuited && branch must not run"
        );
        assert_eq!(code, 1);

        // `true && echo $(side)` — the AND branch IS selected, so its `$(...)`
        // is expanded exactly once.
        let spy2 = SpyVars {
            ran: Cell::new(false),
        };
        let items2 = split_command_list("/bin/true && echo $(side)").unwrap();
        let _ = execute_command_list_outcome(&items2, &spy2, None);
        assert!(
            spy2.ran.get(),
            "a `$(...)` in a selected && branch must run"
        );
    }

    // --- `cd` is intercepted in-process inside &&/||/; chains ---
    // Uses only FAILING cd targets so the test never mutates the process cwd
    // or PWD/OLDPWD (process-global; the harness runs tests in parallel). A
    // failed cd returning 1 (not 127) proves the in-process interception —
    // the old behavior spawned a nonexistent external `cd` and reported 127.
    // The success path is covered live: `mix -c 'cd /tmp && pwd'`.
    #[test]
    fn cd_in_chain_is_builtin_not_spawn_failure() {
        use std::path::Path;
        if !Path::new("/bin/true").exists() {
            return;
        }
        // Failed cd → 1 (builtin), and && short-circuits on it.
        let items = split_command_list("cd /zqx-no-such-dir && /bin/true").unwrap();
        assert_eq!(execute_command_list_outcome(&items, &NoVars, None).code, 1);
        // || takes the fallback branch after a failed cd.
        let items = split_command_list("cd /zqx-no-such-dir || /bin/true").unwrap();
        assert_eq!(execute_command_list_outcome(&items, &NoVars, None).code, 0);
        // A piped cd is NOT intercepted (keeps the spawn path → 127).
        let items = split_command_list("cd /zqx-no-such-dir | /bin/true ; /bin/false").unwrap();
        let _ = execute_command_list_outcome(&items, &NoVars, None);
        // A REDIRECTED cd is not intercepted either — interception would
        // silently drop the redirect (`2>err` must capture the diagnostic,
        // not leak it to the shell's stderr). Spawn path → 127.
        let items = split_command_list("cd /zqx-no-such-dir 2>/dev/null && /bin/true").unwrap();
        assert_eq!(
            execute_command_list_outcome(&items, &NoVars, None).code,
            127
        );
        // An env-prefixed cd likewise keeps the spawn path.
        let items = split_command_list("ZQX=1 cd /zqx-no-such-dir && /bin/true").unwrap();
        assert_eq!(
            execute_command_list_outcome(&items, &NoVars, None).code,
            127
        );
    }

    // --- structural resolvers never spawn: NoVars yields a placeholder ---
    #[test]
    fn novars_cmdsubst_is_placeholder_no_spawn() {
        // NoVars resolves `$(...)` to a non-empty placeholder so the word stays
        // structurally present for classify/validate — and crucially does NOT
        // run the command.
        let p = parse_pipeline("echo $(rm -rf /nonexistent)", &NoVars).unwrap();
        assert_eq!(p.segments.len(), 1);
        // program `echo` + one placeholder arg (the subst), no redirects
        assert_eq!(p.segments[0].program, "echo");
        assert_eq!(p.segments[0].args.len(), 1);
        assert!(p.segments[0].redirects.is_empty());
    }

    /// Resolve a single hand-built word with `ConstVar(val)` and return the Tok(s).
    fn expand_word(segments: Vec<Segment>, assign: bool, val: &str) -> Vec<Tok> {
        expand(vec![word(segments, assign)], &ConstVar(val.to_string()))
    }
    fn tok(value: &str, quoted: bool, op: bool, assign: bool) -> Tok {
        Tok {
            value: value.to_string(),
            quoted,
            op,
            assign,
        }
    }

    // --- expand: resolution, empty-drop, quoted-empty-kept, concatenation ---
    #[test]
    fn expand_resolution_and_drops() {
        use SegSource::*;
        // a var resolves to its value
        assert_eq!(
            expand_word(vec![var("X", false, Raw)], false, "val"),
            vec![tok("val", false, false, false)]
        );
        // an unquoted var resolving to empty DROPS the word
        assert!(expand_word(vec![var("X", false, Raw)], false, "").is_empty());
        // an empty-name var (`${}`) contributes nothing and drops
        assert!(expand_word(vec![var("", true, Raw)], false, "anything").is_empty());
        // a QUOTED empty word is KEPT (quoted=true, value "")
        assert_eq!(
            expand_word(vec![lit("", DoubleQuoted)], false, ""),
            vec![tok("", true, false, false)]
        );
        // segments concatenate
        assert_eq!(
            expand_word(
                vec![lit("a", Raw), var("X", false, Raw), lit("b", Raw)],
                false,
                "Z"
            ),
            vec![tok("aZb", false, false, false)]
        );
        // assign flag is carried through to the Tok
        assert_eq!(
            expand_word(vec![lit("k=", Raw), var("X", false, Raw)], true, "v"),
            vec![tok("k=v", false, false, true)]
        );
    }

    // --- glob-skip (Tok.quoted) is set by QUOTED sources only ---
    #[test]
    fn expand_glob_skip_from_quotes_only() {
        use SegSource::*;
        let q = |segs: Vec<Segment>, val: &str| expand_word(segs, false, val)[0].quoted;
        assert!(q(vec![lit("*", SingleQuoted)], "")); // single-quoted → skip
        assert!(q(vec![lit("*", DoubleQuoted)], "")); // double-quoted → skip
        assert!(!q(vec![lit("*", Escaped)], "")); // escaped → glob-eligible
        assert!(!q(vec![lit("*", Raw)], "")); // raw → glob-eligible
        assert!(!q(vec![var("X", false, Raw)], "*")); // expanded `*` → glob-eligible
    }

    // --- an Op token passes straight through phase 2 ---
    #[test]
    fn expand_op_passthrough() {
        assert_eq!(
            expand(vec![LiteralToken::Op(">".to_string())], &NoVars),
            vec![tok(">", false, true, false)]
        );
    }

    // --- a Tilde segment expands to HOME in phase 2 (or stays `~` if HOME empty) ---
    #[test]
    fn expand_tilde() {
        use SegSource::*;
        let home = std::env::var("HOME").unwrap_or_default();
        let toks = expand(
            vec![word(
                vec![
                    Segment {
                        kind: SegKind::Tilde,
                        source: Raw,
                    },
                    lit("/x", Raw),
                ],
                false,
            )],
            &NoVars,
        );
        let expected = if home.is_empty() {
            "~/x".to_string()
        } else {
            format!("{home}/x")
        };
        assert_eq!(toks, vec![tok(&expected, false, false, false)]);
    }

    // --- Phase 1.5: brace expansion (oracle: bash 5.3) ---

    /// Tokenize with no live resolver and return the Tok values.
    fn brace_values(line: &str) -> Vec<String> {
        shell_tokenize(line, &NoVars)
            .into_iter()
            .map(|t| t.value)
            .collect()
    }

    #[test]
    fn brace_alternation_and_sequences() {
        assert_eq!(brace_values("echo {a,b}"), ["echo", "a", "b"]);
        assert_eq!(brace_values("x{a,b}y"), ["xay", "xby"]);
        assert_eq!(brace_values("{1..5}"), ["1", "2", "3", "4", "5"]);
        assert_eq!(
            brace_values("{05..10}"),
            ["05", "06", "07", "08", "09", "10"]
        );
        // sign-aware zero padding to the widest endpoint
        assert_eq!(brace_values("{-03..3..2}"), ["-03", "-01", "001", "003"]);
        assert_eq!(brace_values("{a..e..2}"), ["a", "c", "e"]);
        assert_eq!(brace_values("{z..x}"), ["z", "y", "x"]);
        assert_eq!(brace_values("{10..1..3}"), ["10", "7", "4", "1"]);
        // a 0 increment means 1; a negative increment is abs'd
        assert_eq!(brace_values("{1..3..0}"), ["1", "2", "3"]);
        assert_eq!(brace_values("{a..b..-2}"), ["a"]);
        // `+` signs parse; output is unsigned
        assert_eq!(brace_values("{+1..3}"), ["1", "2", "3"]);
        // adjacent groups cross-multiply left-to-right
        assert_eq!(brace_values("{a,b}{1..2}c"), ["a1c", "a2c", "b1c", "b2c"]);
        // nested group inside an alternative
        assert_eq!(brace_values("{a,{b,c}d}"), ["a", "bd", "cd"]);
    }

    #[test]
    fn brace_invalid_groups_stay_literal() {
        for line in [
            "{}",
            "{a}",
            "{a..}",
            "{1...3}",
            "{1..2..3..4}",
            "{a..5}",
            "{ab..cd}",
        ] {
            assert_eq!(brace_values(line), [line], "expected literal: {line}");
        }
        // an invalid OUTER group still lets an inner group expand
        assert_eq!(brace_values("a{b{c,d}}"), ["a{bc}", "a{bd}"]);
        // an unmatched outer `{` is literal; the matched inner group expands
        assert_eq!(brace_values("{x{a,b}"), ["{xa", "{xb"]);
    }

    #[test]
    fn brace_quoting_and_values_are_inert() {
        // quoted / escaped braces are data
        assert_eq!(brace_values("'{a,b}'"), ["{a,b}"]);
        assert_eq!(brace_values("\"{a,b}\""), ["{a,b}"]);
        assert_eq!(brace_values("\\{a,b\\}"), ["{a,b}"]);
        // the injection invariant extends to braces: a VALUE containing a brace
        // group must never multiply words
        assert_eq!(
            shell_tokenize("echo $X", &ConstVar("{a,b}".to_string()))
                .into_iter()
                .map(|t| t.value)
                .collect::<Vec<_>>(),
            ["echo", "{a,b}"]
        );
        // `${X}` braces are var syntax, not a brace group
        assert_eq!(
            shell_tokenize("${X}", &ConstVar("v".to_string()))
                .into_iter()
                .map(|t| t.value)
                .collect::<Vec<_>>(),
            ["v"]
        );
        // a quoted alternative carries its content AND its quoted (glob-skip) flag
        let toks = shell_tokenize("{a,'b c'}", &NoVars);
        assert_eq!(
            toks.iter().map(|t| t.value.as_str()).collect::<Vec<_>>(),
            ["a", "b c"]
        );
        assert!(!toks[0].quoted);
        assert!(toks[1].quoted);
        // a comma inside quotes is not an alternative separator
        assert_eq!(brace_values("{a,\"b,c\"}"), ["a", "b,c"]);
    }

    #[test]
    fn brace_empty_alternatives() {
        // an unquoted-empty result word is dropped (bash removes it too)
        assert_eq!(brace_values("{a,}"), ["a"]);
        assert_eq!(brace_values("{,x}"), ["x"]);
        assert_eq!(brace_values("x{a,}"), ["xa", "x"]);
        // a QUOTED empty alternative survives as an empty arg
        let toks = shell_tokenize("{a,\"\"}", &NoVars);
        assert_eq!(
            toks.iter().map(|t| t.value.as_str()).collect::<Vec<_>>(),
            ["a", ""]
        );
        assert!(toks[1].quoted);
    }

    #[test]
    fn brace_assignment_prefix_and_redirect_target_skip() {
        // a leading assignment keeps its braces (bash parity) ...
        let toks = shell_tokenize("x={a,b} cmd {c,d}", &NoVars);
        assert_eq!(
            toks.iter().map(|t| t.value.as_str()).collect::<Vec<_>>(),
            ["x={a,b}", "cmd", "c", "d"]
        );
        assert!(toks[0].assign);
        // ... including after a redirect, whose target also stays literal
        let toks = shell_tokenize("> f{1,2} x={a,b} cmd", &NoVars);
        assert_eq!(
            toks.iter().map(|t| t.value.as_str()).collect::<Vec<_>>(),
            [">", "f{1,2}", "x={a,b}", "cmd"]
        );
        assert!(toks[2].assign);
        // an assignment-shaped word in ARG position expands (bash parity)
        assert_eq!(brace_values("cmd x={a,b}"), ["cmd", "x=a", "x=b"]);
    }

    #[test]
    fn brace_generated_word_regains_tilde_eligibility() {
        use SegSource::*;
        let tilde = || Segment {
            kind: SegKind::Tilde,
            source: Raw,
        };
        // `{~/x,y}` — the generated `~/x` word starts with an eligible tilde,
        // which phase 1 could not see (its `~` was mid-word); `{~,y}` likewise
        assert_eq!(
            expand_braces(lex_literal("{~/x,y}")),
            vec![
                word(vec![tilde(), lit("/x", Raw)], false),
                word(vec![lit("y", Raw)], false),
            ]
        );
        assert_eq!(
            expand_braces(lex_literal("{~,y}")),
            vec![word(vec![tilde()], false), word(vec![lit("y", Raw)], false)]
        );
        // `~{a,b}` — the tilde prefix is NOT eligible (bash leaves it literal too)
        assert_eq!(
            expand_braces(lex_literal("~{a,b}")),
            vec![
                word(vec![lit("~a", Raw)], false),
                word(vec![lit("~b", Raw)], false),
            ]
        );
    }

    #[test]
    fn brace_caps_leave_word_unexpanded() {
        // an over-cap sequence stays literal — never a partial expansion
        assert_eq!(brace_values("{1..999999}"), ["{1..999999}"]);
        // ... and it must also suppress a VALID sibling group in the same
        // word (over-cap ≠ invalid: scanning on would partially expand)
        assert_eq!(brace_values("{1..999999}{a,b}"), ["{1..999999}{a,b}"]);
        // an i64::MIN increment can't be abs'd — must stay literal, not
        // wrap into a negative step
        assert_eq!(
            brace_values("{0..1..-9223372036854775808}"),
            ["{0..1..-9223372036854775808}"]
        );
        // an over-budget alternation product (2^14 > 10_000) stays literal
        let wide = "{a,b}".repeat(14);
        assert_eq!(brace_values(&wide), std::slice::from_ref(&wide));
        // chaining past the depth cap stays literal (and must not overflow
        // the native stack — the 0.16.3 hardening class)
        let deep = "{a,}".repeat(BRACE_DEPTH_CAP + 5);
        assert_eq!(brace_values(&deep), std::slice::from_ref(&deep));
    }

    /// Fragments that put a variable / quote / escape ADJACENT to operator- and
    /// assignment-looking literals — exactly the structure the two properties below
    /// need to exercise. Unconstrained random text almost never produces an operator
    /// next to a `$VAR` or an assignment-shaped word, so the properties would pass
    /// near-vacuously without this.
    fn struct_fragment() -> impl Strategy<Value = &'static str> {
        prop_oneof![
            Just("echo"),
            Just("$X"),
            Just("${X}"),
            Just("$X>f"),   // expanded value adjacent to a redirect
            Just("$X=v"),   // expanded value in assignment-key position
            Just("a=$X"),   // var as an assignment VALUE
            Just("name=v"), // a real assignment
            Just("A=v"),
            Just("2>f"),
            Just("1>f"),
            Just(">f"),
            Just("<in"),
            Just("2>&1"),
            Just("1>&2"),
            Just("&>out"),
            Just(">&2"),
            Just("\"$X\""), // quoted var (no fd prefix / no assignment)
            Just("\\2>f"),  // escaped `2` is data, not an fd prefix
            Just("\\A=v"),  // escaped key is not an assignment
            Just("\"2\">f"),
            Just("foo$X"),
            Just("2"),
            Just("|"),
        ]
    }

    /// Build a line from structured fragments joined by an optional space, so both
    /// space-separated and glued forms appear.
    fn struct_line() -> impl Strategy<Value = String> {
        prop::collection::vec((struct_fragment(), prop::bool::ANY), 0..6).prop_map(|parts| {
            let mut line = String::new();
            for (frag, space) in parts {
                line.push_str(frag);
                if space {
                    line.push(' ');
                }
            }
            line
        })
    }

    /// The structured generator OR pure random bytes — the former guarantees the
    /// properties reach their assertions on operator/assignment structure, the
    /// latter keeps arbitrary-input robustness coverage.
    fn tokenizer_line() -> impl Strategy<Value = String> {
        prop_oneof![struct_line(), ".{0,80}"]
    }

    proptest! {
        // Injection safety, made structural: `lex_literal` takes NO resolver, so
        // the operators it emits are fixed before any value exists. The observable
        // consequence — expanding the SAME literal tokens with wildly different
        // resolvers can never change which tokens are operators or their values.
        #[test]
        fn operators_are_resolver_independent(s in tokenizer_line()) {
            let toks = lex_literal(&s);
            let ops_safe: Vec<String> = expand(toks.clone(), &ConstVar("SAFE".into()))
                .into_iter().filter(|t| t.op).map(|t| t.value).collect();
            let ops_hostile: Vec<String> = expand(toks.clone(), &ConstVar("; rm | $(x) &> /etc/passwd".into()))
                .into_iter().filter(|t| t.op).map(|t| t.value).collect();
            let ops_novars: Vec<String> = expand(toks, &NoVars)
                .into_iter().filter(|t| t.op).map(|t| t.value).collect();
            prop_assert_eq!(&ops_safe, &ops_hostile);
            prop_assert_eq!(&ops_safe, &ops_novars);
        }

        // Eligibility: an `assign` word's key comes ONLY from a pure-`Raw` literal
        // identifier run + a raw `=` — never quoted/escaped/expanded. The key and
        // its `=` coalesce into the FIRST segment, which must be a Raw literal
        // whose text before the first `=` is a valid identifier.
        #[test]
        fn assign_only_from_raw_identifier(s in tokenizer_line()) {
            for t in lex_literal(&s) {
                if let LiteralToken::Word { segments, assign: true } = t {
                    let first = segments.first().expect("assign word has a key segment");
                    prop_assert_eq!(&first.source, &SegSource::Raw);
                    match &first.kind {
                        SegKind::Literal(text) => {
                            let eq = text.find('=').expect("assign key segment contains '='");
                            let key = &text[..eq];
                            prop_assert!(!key.is_empty());
                            prop_assert!(key.chars().next().is_some_and(|c| c.is_ascii_alphabetic() || c == '_'));
                            prop_assert!(key.chars().all(|c| c.is_ascii_alphanumeric() || c == '_'));
                        }
                        _ => prop_assert!(false, "assign word's first segment must be a Literal"),
                    }
                }
            }
        }
    }

    #[test]
    fn both_streams_overwrite() {
        assert_eq!(
            redirs("cmd &>f"),
            vec![Redirect::BothOverwrite("f".to_string())]
        );
    }

    #[test]
    fn both_streams_append() {
        assert_eq!(
            redirs("cmd &>>log"),
            vec![Redirect::BothAppend("log".to_string())]
        );
    }

    #[test]
    fn amp_redirect_space_separated() {
        assert_eq!(
            redirs("cmd &> out.txt"),
            vec![Redirect::BothOverwrite("out.txt".to_string())]
        );
    }

    #[test]
    fn amp_redirect_no_stray_arg() {
        // Regression for the `&>` collision: `&` must NOT leak as an argument,
        // and the redirect must cover BOTH streams (was: `&` arg + plain `>`).
        let p = parse_pipeline("echo hi &>out", &NoVars).unwrap();
        let seg = &p.segments[0];
        assert_eq!(seg.program, "echo");
        assert_eq!(seg.args, vec!["hi".to_string()]); // no stray "&"
        assert_eq!(
            seg.redirects,
            vec![Redirect::BothOverwrite("out".to_string())]
        );
    }

    #[test]
    fn amp_redirect_glued_to_word() {
        // `&` is a word-breaking metacharacter: `arg&>out` is `arg` + `&>` + `out`,
        // not `arg&` + plain `>` (the bug Codex flagged on the first pass).
        let p = parse_pipeline("cmd arg&>out", &NoVars).unwrap();
        let seg = &p.segments[0];
        assert_eq!(seg.program, "cmd");
        assert_eq!(seg.args, vec!["arg".to_string()]); // no "arg&"
        assert_eq!(
            seg.redirects,
            vec![Redirect::BothOverwrite("out".to_string())]
        );
    }

    #[test]
    fn lone_ampersand_stays_literal() {
        // A `&` not followed by `>` keeps its prior behaviour (literal word char).
        let p = parse_pipeline("echo a&b", &NoVars).unwrap();
        let seg = &p.segments[0];
        assert_eq!(seg.args, vec!["a&b".to_string()]);
        assert!(seg.redirects.is_empty());
    }

    #[test]
    fn amp_dup_target_is_not_a_thing() {
        // `&>&1` is not a valid fd dup: the `&` prefix never enters the dup
        // branch, so `&>` takes the following word `&1` as a (literal) file
        // target. The point is it stays a safe file redirect — never a dup,
        // never a parse error, and `&` never leaks as a standalone arg.
        let p = parse_pipeline("cmd &>&1", &NoVars).unwrap();
        assert_eq!(p.segments[0].args, Vec::<String>::new());
        assert_eq!(
            p.segments[0].redirects,
            vec![Redirect::BothOverwrite("&1".to_string())]
        );
    }

    #[test]
    fn expanded_amp_is_not_a_redirect() {
        // `$X` → "&>" via expansion must stay DATA — an expanded value can never
        // form a `&>` operator (same injection guard as the numeric-fd case).
        struct AmpVar;
        impl ShellVarResolver for AmpVar {
            fn resolve(&self, _n: &str) -> Option<String> {
                Some("&>".to_string())
            }
        }
        let p = parse_pipeline("echo $X file", &AmpVar).unwrap();
        let seg = &p.segments[0];
        assert_eq!(seg.args, vec!["&>".to_string(), "file".to_string()]);
        assert!(seg.redirects.is_empty());
    }

    #[test]
    fn both_streams_land_in_file_exec() {
        // End-to-end proof of the dup semantics: a command that writes to BOTH
        // stdout and stderr must have both captured by `&>`.
        use std::io::Read;
        let path = std::env::temp_dir().join(format!("mix_amp_redir_{}.txt", std::process::id()));
        let pstr = path.to_string_lossy().to_string();
        let line = format!("sh -c 'echo OUT; echo ERR 1>&2' &>{pstr}");
        let pipeline = parse_pipeline(&line, &NoVars).unwrap();
        match execute_pipeline(&pipeline).unwrap() {
            PipelineResult::Done(_) => {}
            _ => panic!("expected foreground completion"),
        }
        let mut s = String::new();
        File::open(&path).unwrap().read_to_string(&mut s).unwrap();
        let _ = std::fs::remove_file(&path);
        assert!(s.contains("OUT"), "stdout missing from &> target: {s:?}");
        assert!(s.contains("ERR"), "stderr missing from &> target: {s:?}");
    }
}

// P0 characterization of the CURRENT shell tokenizer — the baseline for the
// two-phase (lex → expand) rewrite (_doc/planned/mix-shell-tokenizer-two-phase.md
// in the cosmix hub). These snapshot today's EXACT PipeSegment output across the
// rawness / quoting / expansion / tilde / assignment / redirect matrix and must
// pass against the CURRENT implementation; the rewrite must keep them UNCHANGED.
// Behaviour here is ground truth, quirks included (an escaped `\*` still globs
// because an escape leaves `quoted = false`).
#[cfg(test)]
mod characterization {
    use super::*;

    /// Resolver returning one fixed value for every `$VAR`.
    struct Var(&'static str);
    impl ShellVarResolver for Var {
        fn resolve(&self, _n: &str) -> Option<String> {
            Some(self.0.to_string())
        }
    }

    /// First segment of `line`, with `$X` resolving to `xval`.
    fn seg(line: &str, xval: &'static str) -> PipeSegment {
        parse_pipeline(line, &Var(xval))
            .expect("parse")
            .segments
            .into_iter()
            .next()
            .expect("a segment")
    }
    /// First segment via the structural `NoVars` resolver (classification path).
    fn seg_novars(line: &str) -> PipeSegment {
        parse_pipeline(line, &NoVars)
            .expect("parse")
            .segments
            .into_iter()
            .next()
            .expect("a segment")
    }
    fn s(v: &str) -> String {
        v.to_string()
    }

    // --- quotes: shell tokenizer expands $name AND ${name} in double quotes;
    //     single quotes are fully literal (distinct from the Mix string lexer) ---
    #[test]
    fn double_quote_expands_both_forms_single_is_literal() {
        assert_eq!(seg("echo \"$X\"", "world").args, vec![s("world")]);
        assert_eq!(seg("echo \"${X}\"", "world").args, vec![s("world")]);
        assert_eq!(seg("echo '$X'", "world").args, vec![s("$X")]);
        // the double/single-quoted arg is glob-skip (quoted=true)
        assert_eq!(seg("echo \"$X\"", "world").quoted, vec![true]);
        assert_eq!(seg("echo '$X'", "world").quoted, vec![true]);
    }

    // --- unquoted empty expansion DROPS its word; a quoted empty is KEPT ---
    #[test]
    fn unquoted_empty_drops_quoted_empty_kept() {
        assert_eq!(seg("echo $X foo", "").args, vec![s("foo")]);
        assert_eq!(seg("echo $X foo", "bar").args, vec![s("bar"), s("foo")]);
        assert_eq!(seg("echo \"$X\" foo", "").args, vec![s(""), s("foo")]);
        assert_eq!(seg("echo \"$X\" foo", "").quoted, vec![true, false]);
        assert_eq!(seg("echo \"\"", "").args, vec![s("")]);
        assert_eq!(seg("echo ''", "").args, vec![s("")]);
        assert_eq!(seg("echo \"\"", "").quoted, vec![true]);
    }

    // --- expanded values never word-split; a hostile value is one inert word ---
    #[test]
    fn expanded_value_is_one_inert_word() {
        assert_eq!(seg("echo $X", "a b c").args, vec![s("a b c")]);
        assert_eq!(seg("echo $X", "; rm -rf /").args, vec![s("; rm -rf /")]);
        assert_eq!(seg("echo $X", "; rm -rf /").redirects.len(), 0);
    }

    // --- assignment prefix: literal `name=` only; expanded/escaped/quoted keys
    //     are NOT assignments ---
    #[test]
    fn assignment_prefix_literal_only() {
        let g = seg("GREETZ=hi cmd", "");
        assert_eq!(g.env_vars, vec![(s("GREETZ"), s("hi"))]);
        assert_eq!(g.program, "cmd");
        assert_eq!(seg("name=$X cmd", "v").env_vars, vec![(s("name"), s("v"))]);
        // expanded key: `$X=v` (X="A") is NOT an assignment
        let x = seg("$X=v cmd", "A");
        assert!(x.env_vars.is_empty());
        assert_eq!(x.program, "A=v");
        // escaped key: `\A=v` is NOT an assignment
        let e = seg("\\A=v cmd", "");
        assert!(e.env_vars.is_empty());
        assert_eq!(e.program, "A=v");
        // empty quote breaks the key run: `A""=v` is NOT an assignment
        let q = seg("A\"\"=v cmd", "");
        assert!(q.env_vars.is_empty());
        assert_eq!(q.program, "A=v");
    }

    // --- raw-vs-data fd prefix: only a RAW literal `1`/`2` before `>` is an fd
    //     prefix; escaped/quoted/expanded `2` is data ---
    #[test]
    fn fd_prefix_is_raw_literal_only() {
        // raw `2>` is the stderr redirect
        assert_eq!(
            seg("echo hi 2>f", "").redirects,
            vec![Redirect::StderrOverwrite(s("f"))]
        );
        // escaped `\2>f`: `2` is a plain arg, `>` is a stdout redirect
        let esc = seg("echo A \\2>f", "");
        assert_eq!(esc.args, vec![s("A"), s("2")]);
        assert_eq!(esc.redirects, vec![Redirect::StdoutOverwrite(s("f"))]);
        // quoted `"2">f`: same — `2` is data, plain `>`
        let qd = seg("echo \"2\">f", "");
        assert_eq!(qd.args, vec![s("2")]);
        assert_eq!(qd.redirects, vec![Redirect::StdoutOverwrite(s("f"))]);
        // expanded `$X>f` (X="2"): plain `>`
        let xp = seg("echo $X>f", "2");
        assert_eq!(xp.args, vec![s("2")]);
        assert_eq!(xp.redirects, vec![Redirect::StdoutOverwrite(s("f"))]);
    }

    // --- glob-skip (Tok.quoted) is set by QUOTES only; escaped/var/tilde leave
    //     quoted=false (so `\*` and an expanded `*` stay glob-eligible) ---
    #[test]
    fn glob_skip_from_quotes_only() {
        assert_eq!(seg("echo \\*", "").args, vec![s("*")]);
        assert_eq!(seg("echo \\*", "").quoted, vec![false]); // glob-eligible
        assert_eq!(seg("echo \"*\"", "").quoted, vec![true]); // glob-skip
        assert_eq!(seg("echo '*'", "").quoted, vec![true]);
        assert_eq!(seg("echo $X", "*").quoted, vec![false]); // expanded `*` glob-eligible
    }

    // --- tilde: expands only unquoted, at word start, HOME set, next ∈
    //     {end, /, space, tab}; otherwise literal ---
    #[test]
    fn tilde_expansion_rule() {
        let home = std::env::var("HOME").unwrap_or_default();
        if home.is_empty() {
            // With HOME unset the tokenizer leaves `~` literal.
            assert_eq!(seg("echo ~", "").args, vec![s("~")]);
            return;
        }
        assert_eq!(seg("echo ~", "").args, vec![home.clone()]);
        assert_eq!(seg("echo ~/x", "").args, vec![format!("{home}/x")]);
        // not eligible → literal
        assert_eq!(seg("echo ~x", "").args, vec![s("~x")]);
        assert_eq!(seg("echo a~", "").args, vec![s("a~")]);
        assert_eq!(seg("echo \"~\"", "").args, vec![s("~")]);
    }

    // --- redirect target VALUES (not just kinds) are preserved ---
    #[test]
    fn redirect_targets_preserved() {
        assert_eq!(
            seg("cmd >out", "").redirects,
            vec![Redirect::StdoutOverwrite(s("out"))]
        );
        assert_eq!(
            seg("cmd 2>>log", "").redirects,
            vec![Redirect::StderrAppend(s("log"))]
        );
        // redirect target can be an expanded value
        assert_eq!(
            seg("cmd >$X", "/tmp/o").redirects,
            vec![Redirect::StdoutOverwrite(s("/tmp/o"))]
        );
    }

    // --- NoVars structural path (REPL classification / sourced-file validation):
    //     `$VAR` resolves to its bare name, so structure is observable w/o values ---
    #[test]
    fn novars_structural_shape() {
        assert_eq!(seg_novars("echo $X foo").args, vec![s("X"), s("foo")]);
        assert_eq!(seg_novars("echo $X").args, vec![s("X")]);
        // structure (segment count, redirect) is independent of values
        let p = parse_command_list("a && b | c", &NoVars).unwrap();
        assert_eq!(p.len(), 2);
        assert_eq!(p[1].1.segments.len(), 2);
    }
}
