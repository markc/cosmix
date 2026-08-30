//! `ShellHandler` implementation bridging the REPL's shell primitives
//! into the library evaluator's `source` statement. Registered on the
//! `Evaluator` by `run_repl` and `run_source` so that `.mixrc`-style
//! files mixing bareword shell lines with Mix code load instead of
//! failing the whole-file Mix parse.
//!
//! Strictness contract: unlike the REPL path (whose caller surfaces an
//! unparseable line via `InputKind::ParseError` and keeps the session),
//! this handler returns `Err` on unparseable lines so the evaluator can
//! abort the `source`.
//!
//! REPL-only builtins (`pushd`, `popd`, `history`, `unalias`,
//! `jobs`, `fg`, `bg`, `type`) are intentionally rejected with a
//! "REPL builtin not supported in sourced files" error. The two that
//! make sense in sourced files — `cd` (cwd change) and `exit`
//! (script abort) — are implemented inline in `execute_shell` so they
//! don't get spawned as external programs by `exec::execute_pipeline`.

use std::collections::HashMap;

use cosmix_mix::MixError;
use cosmix_mix::evaluator::{ShellExecResult, ShellHandler, ShellLineKind, ShellVarResolver};

use crate::exec::{self, PipelineResult};
use crate::shell;

/// REPL builtins that need REPL-only state (job table, pushd stack,
/// readline history, meta dispatcher) and have no sensible meaning
/// when a script is being loaded by `source`. Must mirror every
/// entry in `shell::SHELL_BUILTINS` that the REPL's
/// `ExternalCommand` arm intercepts before `exec::execute_pipeline`
/// — leaving any of them to fall through to a PATH lookup risks
/// silently invoking a like-named binary (e.g. `mix` is our own
/// release binary; `which` is `/usr/bin/which`) whose semantics
/// don't match the REPL's. Classified as `Shell` so they enter
/// `execute_shell`, which returns a precise error.
const UNSUPPORTED_BUILTINS: &[&str] = &[
    "pushd", "popd", "history", "unalias", "jobs", "fg", "bg", "type", "which", "mix",
];

/// REPL builtins that DO make sense in sourced files. Recognized in
/// both `classify` (so they route to `Shell`) and `execute_shell`
/// (which implements them directly rather than spawning).
const SUPPORTED_BUILTINS: &[&str] = &["cd", "exit"];

/// Outcome of probing a sourced line against the Mix parser.
enum MixProbe {
    /// Incomplete Mix input seen on a single line (must live in a pure-Mix
    /// file taking the whole-file parse path).
    Incomplete,
    /// A definitive Mix lex/parse failure, carrying the real error message.
    Error(MixError),
}

/// Stateless adapter; aliases are passed in per-call by the evaluator
/// so changes made by `alias` statements earlier in the same sourced
/// file are visible to later shell-fallback lines.
pub struct ReplShellHandler;

impl ReplShellHandler {
    pub fn new() -> Self {
        Self
    }

    /// Mix-side parser probe. `Ok(())` if the line parses as Mix;
    /// `Err(MixProbe::Incomplete)` for incomplete Mix input;
    /// `Err(MixProbe::Error(error))` for a definitive Mix parse failure,
    /// retaining the typed error so assignment-chain rejection cannot be
    /// confused with user-controlled diagnostic prose.
    ///
    /// Speculative: the `Ok` payload is discarded and the line may still be
    /// routed to the shell, so the probe must not emit parser diagnostics —
    /// the authoritative parse that runs the line emits them instead.
    fn try_parse_as_mix(line: &str) -> Result<(), MixProbe> {
        match cosmix_mix::lexer::Lexer::new(line).tokenize() {
            Ok(tokens) => {
                match cosmix_mix::parser::Parser::new_speculative(tokens, line).parse_program() {
                    Ok(_) => Ok(()),
                    Err(e) => {
                        if shell::is_incomplete_error(&e) {
                            Err(MixProbe::Incomplete)
                        } else {
                            Err(MixProbe::Error(e))
                        }
                    }
                }
            }
            Err(e) => Err(MixProbe::Error(e)),
        }
    }

    /// Mix-first / shell-chain fallback for a sourced line whose head looks
    /// like Mix (a keyword or `$`). Valid Mix (including native `&&`/`||`
    /// chaining) stays Mix; a Mix-parse failure that is structurally a
    /// `&&`/`||`/`;` command list is a shell chain (e.g. `false || cmd`,
    /// `nosuchcmd || rescue`); anything else is a real error.
    fn mix_or_chain(work: &str, trimmed: &str) -> Result<ShellLineKind, String> {
        match Self::try_parse_as_mix(work) {
            Ok(()) => Ok(ShellLineKind::Mix),
            Err(MixProbe::Incomplete) => Err(format!(
                "incomplete Mix input on a single line: `{}` — multi-line Mix input must live in a pure-Mix file",
                trimmed
            )),
            Err(MixProbe::Error(error)) => {
                let msg = error.to_string();
                if error.is_assignment_chain_parse_error() {
                    return Err(format!(
                        "not valid Mix and not a known shell command: `{}` (mix: {})",
                        trimmed, msg
                    ));
                }
                if exec::parse_command_list(work, &exec::NoVars)
                    .map(|l| l.len() > 1)
                    .unwrap_or(false)
                {
                    Ok(ShellLineKind::Shell)
                } else {
                    // Surface the real Mix lex/parse error, not just the generic
                    // "not valid Mix" — so a sourced-file abort names the cause.
                    Err(format!(
                        "not valid Mix and not a known shell command: `{}` (mix: {})",
                        trimmed, msg
                    ))
                }
            }
        }
    }

    fn execute_shell_with_commands(
        &self,
        line: &str,
        aliases: &HashMap<String, String>,
        vars: &dyn ShellVarResolver,
    ) -> Result<(ShellExecResult, Vec<String>), String> {
        let aliased = shell::expand_alias(line, aliases);
        // Split the command list only after alias expansion. Each selected
        // branch reports the same expanded command head as the `-c` path.
        let items = exec::split_command_list(&aliased).map_err(|e| e.to_string())?;

        if items.len() == 1 {
            let pipeline = exec::parse_pipeline(items[0].1, vars).map_err(|e| e.to_string())?;
            let commands = pipeline
                .segments
                .first()
                .map(|first| vec![first.program.clone()])
                .unwrap_or_default();
            if let Some(first) = pipeline.segments.first() {
                let cmd = first.program.as_str();
                if UNSUPPORTED_BUILTINS.contains(&cmd) {
                    return Err(format!(
                        "REPL builtin `{}` is not supported in sourced files",
                        cmd
                    ));
                }
                match cmd {
                    "cd" => {
                        return Ok((
                            ShellExecResult::Status(exec::builtin_cd(&first.args)),
                            commands,
                        ));
                    }
                    "exit" => {
                        let code = first
                            .args
                            .first()
                            .and_then(|s| s.parse::<i32>().ok())
                            .unwrap_or(0);
                        return Ok((ShellExecResult::ExitRequested(code), commands));
                    }
                    _ => {}
                }
            }
            return match exec::execute_pipeline(&pipeline) {
                Ok(PipelineResult::Done(status)) => {
                    Ok((ShellExecResult::Status(exec::exit_code(status)), commands))
                }
                // No JobTable is reachable through the ShellHandler trait;
                // detach a reaper so long-lived source callers do not leak.
                Ok(PipelineResult::Background(child)) => {
                    exec::reap_detached(child);
                    Ok((ShellExecResult::Status(0), commands))
                }
                Err(e) => Err(format!("shell: {}", e)),
            };
        }

        let outcome = exec::execute_command_list_outcome(&items, vars, None);
        Ok((ShellExecResult::Status(outcome.code), outcome.commands))
    }
}

impl Default for ReplShellHandler {
    fn default() -> Self {
        Self::new()
    }
}

impl ShellHandler for ReplShellHandler {
    fn classify(
        &self,
        line: &str,
        aliases: &HashMap<String, String>,
    ) -> Result<ShellLineKind, String> {
        let trimmed = line.trim();
        if trimmed.is_empty() || trimmed.starts_with('#') || trimmed.starts_with("--") {
            return Ok(ShellLineKind::Empty);
        }

        let expanded = shell::expand_alias(trimmed, aliases);
        let work = expanded.trim();

        // `$VAR = ...` and friends are Mix (with shell-chain fallback).
        if work.starts_with('$') {
            return Self::mix_or_chain(work, trimmed);
        }

        // Match `shell::classify_input`'s env-prefix skip so
        // `FOO=bar cmd args` finds `cmd` as the first word.
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

        // Mix keyword → Mix (with shell-chain fallback for `false || cmd`).
        if shell::MIX_KEYWORDS.contains(&first_word) {
            return Self::mix_or_chain(work, trimmed);
        }

        // Mirrors `shell::classify_input_fns`: a typed assignment-chain error
        // means the parser saw a real Mix assignment as a `&&`/`||` operand,
        // and no head shape below may then reclassify the line as shell. See
        // that function for why the operator prefilter is sound rather than a
        // text heuristic.
        if (work.contains("&&") || work.contains("||"))
            && let Err(MixProbe::Error(error)) = Self::try_parse_as_mix(work)
            && error.is_assignment_chain_parse_error()
        {
            return Err(format!(
                "not valid Mix and not a known shell command: `{}` (mix: {})",
                trimmed, error
            ));
        }

        // REPL-only builtins (need REPL state): classify as Shell so
        // `execute_shell` can return a precise error. We deliberately
        // do NOT silently fall through to PATH (e.g. `type` on PATH
        // is bash's, semantically unlike the REPL's `type`).
        if UNSUPPORTED_BUILTINS.contains(&first_word) {
            return Ok(ShellLineKind::Shell);
        }

        // Source-supported builtins (`cd`, `exit`): always Shell.
        if SUPPORTED_BUILTINS.contains(&first_word) {
            return Ok(ShellLineKind::Shell);
        }

        // On PATH → Shell (external program).
        if shell::is_external_command(first_word) {
            return Ok(ShellLineKind::Shell);
        }

        // Mirror the REPL/-c preservation guard: an unknown command-like
        // `foo; bar` line was a shell list before executable Mix gained `;`.
        // Do not let the successful parse of two discarded String expressions
        // turn a sourced shell line into a silent Mix no-op.
        //
        // The guard's whole premise is a line that DOES parse as Mix — it
        // exists to stop a successful parse eating a shell list. In `-c` it
        // sits on the parser's `Ok` arm for exactly that reason; here it runs
        // before any parse, so a definitive language error was reaching the
        // shell anyway: `zqxfoo; $x = false || cmd` ran `cmd` and sourced
        // clean. Consult the parser first and let a definitive
        // assignment-chain error win, which restores the `-c` asymmetry.
        if shell::is_command_like_semicolon_list(work, first_word) {
            if let Err(MixProbe::Error(error)) = Self::try_parse_as_mix(work)
                && error.is_assignment_chain_parse_error()
            {
                return Err(format!(
                    "not valid Mix and not a known shell command: `{}` (mix: {})",
                    trimmed, error
                ));
            }
            return Ok(ShellLineKind::Shell);
        }

        // Last resort: try Mix-parse. A Mix-parseable line is Mix; an
        // incomplete Mix input becomes an error here (multi-line
        // input must live in a pure-Mix file, taking the whole-file
        // parse path in `exec_source`); a definitive Mix parse
        // failure becomes an error so `source` aborts instead of
        // silently skipping the line.
        Self::mix_or_chain(work, trimmed)
    }

    fn execute_shell(
        &self,
        line: &str,
        aliases: &HashMap<String, String>,
        vars: &dyn ShellVarResolver,
    ) -> Result<ShellExecResult, String> {
        self.execute_shell_with_commands(line, aliases, vars)
            .map(|(result, _)| result)
    }

    fn execute_shell_tracked(
        &self,
        line: &str,
        aliases: &HashMap<String, String>,
        vars: &dyn ShellVarResolver,
    ) -> Result<(ShellExecResult, Vec<String>), String> {
        self.execute_shell_with_commands(line, aliases, vars)
    }

    fn validate_shell(&self, line: &str, aliases: &HashMap<String, String>) -> Result<(), String> {
        // Mirror `execute_shell`'s pre-spawn checks WITHOUT expanding
        // `$VAR` (which would need a live resolver and runtime values)
        // and WITHOUT spawning processes. Catches the two failure
        // modes that survive `classify` and only surface at run time:
        // (1) UNSUPPORTED_BUILTINS that classify as Shell but
        // `execute_shell` rejects, and (2) malformed pipelines that
        // `exec::parse_pipeline` rejects.
        let aliased = shell::expand_alias(line, aliases);
        let items = exec::parse_command_list(&aliased, &exec::NoVars).map_err(|e| e.to_string())?;
        if let Some(first) = items.first().and_then(|(_, p)| p.segments.first()) {
            let cmd = first.program.as_str();
            if UNSUPPORTED_BUILTINS.contains(&cmd) {
                return Err(format!(
                    "REPL builtin `{}` is not supported in sourced files",
                    cmd
                ));
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::Path;

    struct EmptyVars;
    impl ShellVarResolver for EmptyVars {
        fn resolve(&self, _name: &str) -> Option<String> {
            None
        }
    }

    fn aliases() -> HashMap<String, String> {
        HashMap::new()
    }

    #[test]
    fn classify_blank_and_comments_empty() {
        let h = ReplShellHandler::new();
        assert!(matches!(
            h.classify("", &aliases()).unwrap(),
            ShellLineKind::Empty
        ));
        assert!(matches!(
            h.classify("   ", &aliases()).unwrap(),
            ShellLineKind::Empty
        ));
        assert!(matches!(
            h.classify("# comment", &aliases()).unwrap(),
            ShellLineKind::Empty
        ));
        assert!(matches!(
            h.classify("-- mix comment", &aliases()).unwrap(),
            ShellLineKind::Empty
        ));
    }

    #[test]
    fn classify_sigil_mix() {
        let h = ReplShellHandler::new();
        assert!(matches!(
            h.classify("$x = 1", &aliases()).unwrap(),
            ShellLineKind::Mix
        ));
    }

    #[test]
    fn trailing_concat_is_incomplete_mix_not_shell_fallback() {
        let h = ReplShellHandler::new();
        let error = match h.classify("$x = \"a\" ..", &aliases()) {
            Err(error) => error,
            Ok(_) => panic!("sourced per-line fallback must reject incomplete Mix"),
        };
        assert!(
            error.contains("incomplete Mix"),
            "unexpected error: {error}"
        );
    }

    #[test]
    fn classify_semicolon_sigil_sequence_as_mix() {
        let h = ReplShellHandler::new();
        assert!(matches!(
            h.classify("$x = 1; print($x)", &aliases()).unwrap(),
            ShellLineKind::Mix
        ));
    }

    #[test]
    fn classify_assignment_chain_error_never_falls_back_to_shell() {
        let h = ReplShellHandler::new();
        let err = match h.classify(
            "$x = true || /usr/bin/printf SOURCED_FALSE_GREEN",
            &aliases(),
        ) {
            Err(error) => error,
            Ok(_) => panic!("assignment chain must abort sourced input"),
        };
        assert!(
            err.contains("assignment cannot be chained with `||`"),
            "unexpected classifier error: {err}"
        );
    }

    #[test]
    fn classify_user_prose_matching_old_sentinel_still_falls_back_to_shell() {
        let h = ReplShellHandler::new();
        assert!(matches!(
            h.classify(
                "sh \"\" \"assignment cannot be chained with\"; /usr/bin/printf SHOULD_RUN",
                &aliases()
            )
            .unwrap(),
            ShellLineKind::Shell
        ));
    }

    #[test]
    fn classify_unknown_semicolon_list_as_shell() {
        let h = ReplShellHandler::new();
        assert!(matches!(
            h.classify("zqxfoo; zqxbar", &aliases()).unwrap(),
            ShellLineKind::Shell
        ));
    }

    #[test]
    fn classify_mix_keyword() {
        let h = ReplShellHandler::new();
        assert!(matches!(
            h.classify("if true then print(1) end", &aliases()).unwrap(),
            ShellLineKind::Mix
        ));
    }

    #[test]
    fn classify_cd_is_shell() {
        let h = ReplShellHandler::new();
        // `cd` is a supported builtin — classified as Shell so
        // execute_shell intercepts it before exec::execute_pipeline.
        assert!(matches!(
            h.classify("cd /tmp", &aliases()).unwrap(),
            ShellLineKind::Shell
        ));
    }

    #[test]
    fn classify_pushd_is_shell_unsupported_at_exec() {
        // pushd is UNSUPPORTED_BUILTINS: classified Shell so
        // execute_shell can return a precise error.
        let h = ReplShellHandler::new();
        assert!(matches!(
            h.classify("pushd /tmp", &aliases()).unwrap(),
            ShellLineKind::Shell
        ));
        let err = h
            .execute_shell("pushd /tmp", &aliases(), &EmptyVars)
            .unwrap_err();
        assert!(err.contains("pushd"), "msg should name the builtin: {err}");
        assert!(err.contains("sourced files"), "msg should explain: {err}");
    }

    #[test]
    fn validate_shell_rejects_unsupported_builtin() {
        let h = ReplShellHandler::new();
        let err = h.validate_shell("pushd /tmp", &aliases()).unwrap_err();
        assert!(err.contains("pushd"), "msg should name the builtin: {err}");
        assert!(err.contains("sourced files"), "msg should explain: {err}");
    }

    #[test]
    fn validate_shell_accepts_external_command() {
        let h = ReplShellHandler::new();
        // No spawn — pure static check.
        assert!(h.validate_shell("/usr/bin/true", &aliases()).is_ok());
        assert!(h.validate_shell("echo hello", &aliases()).is_ok());
    }

    #[test]
    fn validate_shell_accepts_supported_builtin() {
        let h = ReplShellHandler::new();
        assert!(h.validate_shell("cd /tmp", &aliases()).is_ok());
        assert!(h.validate_shell("exit 0", &aliases()).is_ok());
    }

    #[test]
    fn validate_shell_rejects_malformed_pipeline() {
        let h = ReplShellHandler::new();
        // Trailing pipe with no following command is a parse error.
        assert!(h.validate_shell("echo hi |", &aliases()).is_err());
    }

    #[test]
    fn classify_surfaces_real_mix_error_on_abort() {
        // A definitively-broken Mix line in a sourced file must abort with the
        // real lexer/parser message, not just the generic "not valid Mix" text.
        let h = ReplShellHandler::new();
        let err = match h.classify("print(0755)", &aliases()) {
            Err(e) => e,
            Ok(_) => panic!("print(0755) should abort the source, not classify cleanly"),
        };
        assert!(
            err.contains("leading-zero"),
            "abort should name the real lex error: {err}"
        );
        assert!(
            err.contains("not valid Mix"),
            "abort should keep the framing: {err}"
        );
    }

    #[test]
    fn classify_path_program_is_shell() {
        let h = ReplShellHandler::new();
        // /usr/bin/true exists on Linux test boxes.
        if Path::new("/usr/bin/true").exists() {
            assert!(matches!(
                h.classify("/usr/bin/true", &aliases()).unwrap(),
                ShellLineKind::Shell
            ));
        }
    }
}
