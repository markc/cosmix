//! `mix lint` — the CLI over the semantic analyzer (0.29.0, decision
//! record D3).
//!
//! ```text
//! mix lint [--json | --data] [--deny-warnings]
//!          [--allow-global NAME]... [--allow-function NAME]...
//!          FILE...
//! ```
//!
//! Exit codes: 0 = no errors (warnings allowed unless --deny-warnings);
//! 1 = error diagnostics, or warnings under --deny-warnings; 2 =
//! invalid usage, unreadable input, or internal analyzer failure.
//! Diagnostics go to stdout; CLI/internal errors go to stderr.
//! `-` reads stdin (at most once).

use std::io::Read;

use cosmix_mix::analyzer::{self, Analysis, AnalyzerConfig, Diagnostic, Severity};
use cosmix_mix::error::MixError;
use cosmix_mix::token::Token;

const USAGE: &str = "Usage: mix lint [--json | --data] [--deny-warnings] \
[--allow-global NAME]... [--allow-function NAME]... FILE...";

/// Line/column pair lifted from a lexer/parser span.
struct SpanLite {
    line: usize,
    column: usize,
}

enum Format {
    Human,
    Json,
    Data,
}

enum LintOutcome {
    Script(Analysis),
    StrictData,
}

struct LintSummary {
    errors: usize,
    warnings: usize,
    /// `Severity::Note` findings (0.63.0, schema_version 2) — advisory
    /// only: NEVER counted as warnings, never denied.
    notes: usize,
    denied_warnings: bool,
}

pub fn run_lint(args: &[String], version: &str) -> i32 {
    let mut format = Format::Human;
    let mut format_set = false;
    let mut deny_warnings = false;
    let mut cfg = AnalyzerConfig::default();
    let mut files: Vec<String> = Vec::new();
    let mut stdin_used = false;

    let mut i = 0;
    while i < args.len() {
        match args[i].as_str() {
            "--json" | "--data" => {
                if format_set {
                    eprintln!("mix lint: --json and --data are mutually exclusive");
                    return 2;
                }
                format = if args[i] == "--json" {
                    Format::Json
                } else {
                    Format::Data
                };
                format_set = true;
            }
            "--deny-warnings" => deny_warnings = true,
            "--allow-global" | "--allow-function" => {
                let Some(name) = args.get(i + 1) else {
                    eprintln!("mix lint: {} requires a NAME", args[i]);
                    return 2;
                };
                if args[i] == "--allow-global" {
                    cfg.allow_globals.push(name.clone());
                } else {
                    cfg.allow_functions.push(name.clone());
                }
                i += 1;
            }
            "-" => {
                if stdin_used {
                    eprintln!("mix lint: '-' (stdin) may appear only once");
                    return 2;
                }
                stdin_used = true;
                files.push("-".to_string());
            }
            flag if flag.starts_with('-') => {
                eprintln!("mix lint: unknown flag '{flag}'");
                eprintln!("{USAGE}");
                return 2;
            }
            file => files.push(file.to_string()),
        }
        i += 1;
    }
    if files.is_empty() {
        eprintln!("{USAGE}");
        return 2;
    }

    let mut all_diags: Vec<Diagnostic> = Vec::new();
    let mut capabilities: Vec<&'static str> = Vec::new();
    let mut strict_data_files: Vec<String> = Vec::new();
    for file in &files {
        let source = if file == "-" {
            let mut buf = String::new();
            if let Err(e) = std::io::stdin().read_to_string(&mut buf) {
                eprintln!("mix lint: reading stdin: {e}");
                return 2;
            }
            buf
        } else {
            match std::fs::read_to_string(file) {
                Ok(s) => s,
                Err(e) => {
                    eprintln!("mix lint: {file}: {e}");
                    return 2;
                }
            }
        };
        // Stdin lints against the current directory for relative
        // require() resolution (D3): give it a synthetic ./- name.
        let file_label: Option<&str> = if file == "-" { Some("-") } else { Some(file) };
        match lint_one(&source, file_label, &cfg) {
            Ok(LintOutcome::Script(analysis)) => {
                all_diags.extend(analysis.diagnostics);
                for cap in analysis.capabilities {
                    if !capabilities.contains(&cap) {
                        capabilities.push(cap);
                    }
                }
            }
            Ok(LintOutcome::StrictData) => strict_data_files.push(file.clone()),
            Err(diag) => all_diags.push(*diag),
        }
    }
    capabilities.sort_unstable();

    // Errors first, then warnings, then notes; within a class, by
    // file+line.
    all_diags.sort_by(|a, b| {
        let sev = |d: &Diagnostic| match d.severity {
            Severity::Error => 0u8,
            Severity::Warning => 1,
            Severity::Note => 2,
        };
        sev(a)
            .cmp(&sev(b))
            .then_with(|| a.file.cmp(&b.file))
            .then_with(|| a.line.unwrap_or(0).cmp(&b.line.unwrap_or(0)))
    });

    let errors = all_diags
        .iter()
        .filter(|d| d.severity == Severity::Error)
        .count();
    // Explicit match, NOT "everything that isn't an error": notes must
    // never count as warnings, or `--deny-warnings` (a live fleet deploy
    // gate — deploy_shared.mix) would deny on advisories.
    let warnings = all_diags
        .iter()
        .filter(|d| d.severity == Severity::Warning)
        .count();
    let notes = all_diags
        .iter()
        .filter(|d| d.severity == Severity::Note)
        .count();
    let denied = deny_warnings && warnings > 0;
    let summary = LintSummary {
        errors,
        warnings,
        notes,
        denied_warnings: denied,
    };

    match format {
        Format::Human => {
            for file in &strict_data_files {
                println!("{file}: validated as strict data (not as a script)");
            }
            for d in &all_diags {
                let loc = match (&d.file, d.line) {
                    (Some(f), Some(l)) => format!("{f}:{l}"),
                    (Some(f), None) => f.clone(),
                    (None, Some(l)) => format!("line {l}"),
                    (None, None) => "<input>".to_string(),
                };
                println!(
                    "{loc}: {} {}: {}",
                    d.code,
                    d.severity.wire_name(),
                    d.message
                );
                if let Some(hint) = &d.hint {
                    println!("  hint: {hint}");
                }
            }
            println!(
                "{} error(s), {} warning(s), {} note(s){}",
                errors,
                warnings,
                notes,
                if denied { " [denied]" } else { "" }
            );
            // One trailer when anything was reported: an agent that hit a code
            // it has never seen gets the whole rationale offline in one call.
            if !all_diags.is_empty() {
                println!("explain any code with: mix explain MIX-XXXX");
            }
        }
        Format::Json | Format::Data => {
            let report = report_json(
                version,
                &files,
                &strict_data_files,
                &all_diags,
                &capabilities,
                &summary,
            );
            match format {
                Format::Json => match serde_json::to_string_pretty(&report) {
                    Ok(s) => println!("{s}"),
                    Err(e) => {
                        eprintln!("mix lint: encoding report: {e}");
                        return 2;
                    }
                },
                Format::Data => {
                    let value = crate::meta::json_to_mix_value(&report);
                    match value.to_mix_data_string_pretty() {
                        Ok(s) => println!("{s}"),
                        Err(e) => {
                            eprintln!("mix lint: encoding report: {e}");
                            return 2;
                        }
                    }
                }
                Format::Human => unreachable!(),
            }
        }
    }

    if errors > 0 || denied { 1 } else { 0 }
}

/// Lex+parse+analyze one file. If script parsing fails but the same
/// source is valid strict data, report that validation mode instead.
fn lint_one(
    source: &str,
    file: Option<&str>,
    cfg: &AnalyzerConfig,
) -> Result<LintOutcome, Box<Diagnostic>> {
    let to_diag = |code: &'static str, msg: String, span: Option<SpanLite>| {
        Box::new(Diagnostic {
            code,
            severity: Severity::Error,
            file: file.map(str::to_string),
            line: span.as_ref().map(|s| s.line),
            column: span
                .as_ref()
                .and_then(|s| (s.column > 0).then_some(s.column)),
            message: msg,
            hint: None,
        })
    };
    let tokens = match cosmix_mix::lexer::Lexer::new(source).tokenize() {
        Ok(t) => t,
        Err(MixError::LexerError { msg, span }) => {
            let diag = to_diag(
                "MIX-E1001",
                msg,
                Some(SpanLite {
                    line: span.line,
                    column: span.column,
                }),
            );
            return strict_data_fallback(source, file, diag);
        }
        Err(e) => {
            return strict_data_fallback(source, file, to_diag("MIX-E1001", e.to_string(), None));
        }
    };
    let stmts = match cosmix_mix::parser::Parser::new(tokens, source).parse_program() {
        Ok(s) => s,
        Err(MixError::ParseError { msg, span }) | Err(MixError::IncompleteInput { msg, span }) => {
            let diag = to_diag(
                "MIX-E1002",
                msg,
                Some(SpanLite {
                    line: span.line,
                    column: span.column,
                }),
            );
            return strict_data_fallback(source, file, diag);
        }
        Err(MixError::AssignmentChainParseError { operator, span }) => {
            let diag = to_diag(
                "MIX-E1002",
                cosmix_mix::error::assignment_chain_parse_message(&operator),
                Some(SpanLite {
                    line: span.line,
                    column: span.column,
                }),
            );
            return strict_data_fallback(source, file, diag);
        }
        Err(e) => {
            return strict_data_fallback(source, file, to_diag("MIX-E1002", e.to_string(), None));
        }
    };
    Ok(LintOutcome::Script(analyzer::analyze(&stmts, file, cfg)))
}

fn strict_data_fallback(
    source: &str,
    file: Option<&str>,
    script_diag: Box<Diagnostic>,
) -> Result<LintOutcome, Box<Diagnostic>> {
    match cosmix_mix::parse_data(source) {
        Ok(_) => Ok(LintOutcome::StrictData),
        Err(data_error) if looks_like_strict_data(source, file) => {
            Err(Box::new(strict_data_diagnostic(data_error, file)))
        }
        Err(_) => Err(script_diag),
    }
}

/// A successful strict-data parse is decisive. When both grammars fail,
/// choose the data diagnostic only for an explicit top-level `key:` shape
/// or a conventional strict-data suffix; otherwise preserve the original
/// script diagnostic. This keeps malformed scripts from being excused by
/// the fallback while still giving damaged data files the useful error.
fn looks_like_strict_data(source: &str, file: Option<&str>) -> bool {
    let conventional_suffix = file.is_some_and(|name| {
        [
            ".conf.mix",
            ".spec.mix",
            ".state.mix",
            ".journal.mix",
            ".verdict.mix",
            ".call.mix",
        ]
        .iter()
        .any(|suffix| name.ends_with(suffix))
    });
    let tokenised_top_level_pair = cosmix_mix::lexer::Lexer::new(source)
        .tokenize()
        .ok()
        .map(|tokens| {
            let mut significant = tokens
                .iter()
                .map(|t| &t.token)
                .filter(|t| !matches!(t, Token::Newline | Token::Semicolon));
            matches!(significant.next(), Some(Token::String(_)))
                && matches!(significant.next(), Some(Token::Colon))
        })
        .unwrap_or(false);
    tokenised_top_level_pair || first_line_is_data_pair(source) || conventional_suffix
}

fn first_line_is_data_pair(source: &str) -> bool {
    let Some(line) = source
        .lines()
        .map(str::trim)
        .find(|line| !line.is_empty() && !line.starts_with('#') && !line.starts_with("--"))
    else {
        return false;
    };
    let Some(colon) = line.find(':') else {
        return false;
    };
    let key = line[..colon].trim();
    (!key.is_empty() && key.chars().all(|c| c.is_ascii_alphanumeric() || c == '_'))
        || (key.len() >= 2 && key.starts_with('"') && key.ends_with('"'))
}

fn strict_data_diagnostic(error: MixError, file: Option<&str>) -> Diagnostic {
    let (message, line, column, hint) = match error {
        MixError::LexerError { msg, span } | MixError::ParseError { msg, span } => (
            msg,
            Some(span.line),
            (span.column > 0).then_some(span.column),
            None,
        ),
        // No `AssignmentChainParseError` arm: this function only ever receives
        // the error from `parse_data()`, which parses strict data and never
        // reaches statement parsing, so that variant cannot occur here. An arm
        // for it would be unreachable code implying otherwise — and no test
        // could pin it. The script path's arm (above) is the live one.
        MixError::StrictDataViolation {
            construct,
            line,
            hint,
        } => (construct, Some(line), None, Some(hint)),
        other => (other.to_string(), None, None, None),
    };
    Diagnostic {
        code: "MIX-E1003",
        severity: Severity::Error,
        file: file.map(str::to_string),
        line,
        column,
        message: format!("strict-data parse error: {message}"),
        hint,
    }
}

fn report_json(
    version: &str,
    files: &[String],
    strict_data_files: &[String],
    diags: &[Diagnostic],
    capabilities: &[&'static str],
    summary: &LintSummary,
) -> serde_json::Value {
    serde_json::json!({
        // 2 (0.63.0): severity domain gains "note"; summary gains
        // `notes`. No in-tree consumer parses this report (inventoried
        // 2026-09-03, zero `lint --json` callers fleet-wide).
        "schema_version": 2,
        "tool": "mix lint",
        "mix_version": version,
        "files": files,
        "strict_data_files": strict_data_files,
        "diagnostics": diags.iter().map(|d| serde_json::json!({
            "code": d.code,
            "severity": d.severity.wire_name(),
            "file": d.file,
            "line": d.line,
            "column": d.column,
            "message": d.message,
            "hint": d.hint,
        })).collect::<Vec<_>>(),
        "capabilities": capabilities,
        "summary": {
            "errors": summary.errors,
            "warnings": summary.warnings,
            "notes": summary.notes,
            "denied_warnings": summary.denied_warnings,
        },
    })
}
