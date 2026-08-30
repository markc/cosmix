//! Phase B audit — enumerate every `[:= ]~/` occurrence inside a
//! double-quoted Mix string body where the planned Phase B lexer rule
//! ("if the *last buffered char* in `current` is `[:= ]` and the next
//! source char is `/`, emit `EnvVar(HOME)` and skip the `~`") would
//! fire at runtime.
//!
//! Run explicitly — never by `cargo test` default:
//!
//! ```sh
//! cargo test -p cosmix-lib-mix --test audit_tilde_corpus \
//!   -- --ignored --nocapture
//! ```
//!
//! Roots are configured by `COSMIX_AUDIT_ROOTS` (colon-separated);
//! defaults to `$HOME/.cosmix/src:$HOME/.rc:$HOME/.gh:$HOME/.ns`.
//!
//! ## Source-text scan, not token walk
//!
//! Earlier audit revisions walked the token stream
//! (`Token::InterpString` parts). Codex review of the Phase-B-deferral
//! commit flagged two bug classes that make that approach wrong:
//!
//! 1. **Single-literal collapse.** `lex_double_string` returns
//!    `Token::String(s)` when the whole body is one Literal segment
//!    (`lexer.rs:499-509`). Walking only `Token::InterpString` misses
//!    every plain `"…~/…"` double-string that has no `${X}` or `$(…)`.
//!    Real corpus examples Codex flagged:
//!    `apps/settings.mix:112` (`body="Config: ~/.config/..."`),
//!    `~/.rc/_lib/70-functions.mix:26` (`sh "nano -t -x -c ~/.myrc.mix"`),
//!    `~/.rc/_lib/70-functions.mix:28` (`print "Reloaded ~/.mixrc"`).
//!
//! 2. **`\~` decode collapse.** The lexer turns `\~` into a literal
//!    `~` in the decoded `StringPart::Literal` (`lexer.rs:393, 403`).
//!    Scanning the decoded literal therefore can't tell `\~/foo` (the
//!    plan's planned migration escape — must NOT fire) from `~/foo`
//!    (the rule WOULD fire). Source-text scan distinguishes by
//!    looking at the raw bytes preceding the `~`.
//!
//! This revision walks the raw source with a small state machine
//! (`Code | DoubleString | SingleString | HeredocBody{tag} |
//! LineComment`). The state machine mirrors the lexer's tokenization
//! decisions just deeply enough to identify which `~` characters are
//! inside a double-quoted body and what the immediately-preceding
//! source byte was.
//!
//! ## Position-0 tilde is out of scope
//!
//! `lex_double_string` lines 370-375 already handle `"~/foo"` and
//! `"~"` at position 0 of a double-string — emits `EnvVar("HOME")`
//! at lex time. Phase B is about *mid-string* `~` only. The audit's
//! `prev_char` is `None` at string start, so the separator check
//! cannot fire on position-0. Position-0 hits are correctly invisible.
//!
//! ## What counts as "preceding char in current"
//!
//! Per the rev 4 plan §233-242, the planned lexer rule fires on the
//! *last source character that was appended to the in-progress
//! `current` Literal buffer*. Across a `${X}` or `$(…)` interpolation
//! the buffer is flushed, so the next `~` starts a fresh `current`
//! with no preceding char — the rule does NOT fire on `"${X}~/foo"`.
//! The audit's `prev_char` tracking resets to `None` on every `${…}`
//! / `$(…)` flush to match.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

#[derive(Debug)]
struct Finding {
    file: PathBuf,
    line: usize,
    column: usize,
    snippet: String,
    sep: char,
}

#[test]
#[ignore]
fn audit_tilde_corpus() {
    let roots = audit_roots();
    let mut findings: Vec<Finding> = Vec::new();
    let mut files_walked: usize = 0;

    for root in &roots {
        walk_mix_files(root, &mut |path| {
            let source = match std::fs::read_to_string(path) {
                Ok(s) => s,
                Err(_) => return,
            };
            files_walked += 1;
            let file_findings = scan_source(&source);
            findings.extend(file_findings.into_iter().map(|f| Finding {
                file: path.to_path_buf(),
                ..f
            }));
        });
    }

    print_report(&roots, files_walked, &findings);
}

fn audit_roots() -> Vec<PathBuf> {
    let raw = std::env::var("COSMIX_AUDIT_ROOTS").unwrap_or_else(|_| {
        let home = std::env::var("HOME").unwrap_or_default();
        format!(
            "{home}/.cosmix/src:{home}/.rc:{home}/.gh:{home}/.ns",
            home = home
        )
    });
    raw.split(':')
        .filter(|s| !s.is_empty())
        .map(PathBuf::from)
        .filter(|p| p.exists())
        .collect()
}

fn walk_mix_files(root: &Path, on_file: &mut dyn FnMut(&Path)) {
    let entries = match std::fs::read_dir(root) {
        Ok(e) => e,
        Err(_) => return,
    };
    for entry in entries.flatten() {
        let path = entry.path();
        let ftype = match entry.file_type() {
            Ok(t) => t,
            Err(_) => continue,
        };
        if ftype.is_symlink() {
            continue;
        }
        if ftype.is_dir() {
            if let Some(name) = path.file_name().and_then(|n| n.to_str())
                && matches!(name, "target" | "node_modules" | ".git" | ".direnv")
            {
                continue;
            }
            walk_mix_files(&path, on_file);
            continue;
        }
        if !ftype.is_file() {
            continue;
        }
        if is_mix_file(&path) {
            on_file(&path);
        }
    }
}

fn is_mix_file(path: &Path) -> bool {
    if path.extension().and_then(|e| e.to_str()) == Some("mix") {
        return true;
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let meta = match std::fs::metadata(path) {
            Ok(m) => m,
            Err(_) => return false,
        };
        if meta.permissions().mode() & 0o111 == 0 {
            return false;
        }
        let mut buf = [0u8; 128];
        let n = match std::fs::File::open(path)
            .and_then(|mut f| std::io::Read::read(&mut f, &mut buf))
        {
            Ok(n) => n,
            Err(_) => return false,
        };
        let head = std::str::from_utf8(&buf[..n]).unwrap_or("");
        let first = head.lines().next().unwrap_or("");
        if !first.starts_with("#!") {
            return false;
        }
        is_mix_shebang(first)
    }
    #[cfg(not(unix))]
    {
        false
    }
}

fn is_mix_shebang(line: &str) -> bool {
    let rest = line.trim_start_matches("#!").trim_start();
    let mut tokens = rest.split_whitespace();
    let prog = match tokens.next() {
        Some(p) => p,
        None => return false,
    };
    let last_seg = |s: &str| s.rsplit('/').next().unwrap_or(s).to_string();
    if last_seg(prog) == "env" {
        if let Some(next) = tokens.next() {
            return last_seg(next) == "mix";
        }
        return false;
    }
    last_seg(prog) == "mix"
}

/// Scan source text, return all `[:= ]~/` hits inside double-quoted
/// strings. State machine details in the file-level doc comment.
fn scan_source(source: &str) -> Vec<Finding> {
    let chars: Vec<char> = source.chars().collect();
    let mut out: Vec<Finding> = Vec::new();
    let mut i = 0usize;
    let mut line = 1usize;
    let mut col = 1usize;

    while i < chars.len() {
        let c = chars[i];

        // Line comment: `--` or `#` to end of line. Matches the
        // lexer at `lexer.rs:107-114` — both forms skip to EOL at
        // top-level code. (We don't reach this branch from inside
        // any string state — those are handled below.) Codex round-2
        // catch: the original cut only handled `--`, which would
        // false-positive on a comment like `# example "PATH=~/bin"`.
        if (c == '-' && i + 1 < chars.len() && chars[i + 1] == '-') || c == '#' {
            while i < chars.len() && chars[i] != '\n' {
                i += 1;
                col += 1;
            }
            continue;
        }

        // Single-quoted string: no tilde expansion of any kind. Just
        // walk to the closing `'`, honoring `\'` and `\\` escapes.
        if c == '\'' {
            i += 1;
            col += 1;
            while i < chars.len() {
                let c2 = chars[i];
                if c2 == '\\' && i + 1 < chars.len() {
                    let nxt = chars[i + 1];
                    if nxt == '\n' {
                        line += 1;
                        col = 1;
                    } else {
                        col += 2;
                    }
                    i += 2;
                    continue;
                }
                if c2 == '\'' {
                    i += 1;
                    col += 1;
                    break;
                }
                if c2 == '\n' {
                    line += 1;
                    col = 1;
                } else {
                    col += 1;
                }
                i += 1;
            }
            continue;
        }

        // Heredoc: `<<TAG` followed by ident chars then a newline.
        // The body is out of scope for Phase B (no tilde expansion in
        // heredocs at any position), so we skip from the opener
        // through the closing tag line.
        if c == '<' && i + 1 < chars.len() && chars[i + 1] == '<' {
            let mut j = i + 2;
            let tag_start = j;
            while j < chars.len() && (chars[j].is_ascii_alphanumeric() || chars[j] == '_') {
                j += 1;
            }
            let tag: String = chars[tag_start..j].iter().collect();
            if tag.is_empty() {
                // Not a heredoc opener (no ident after `<<`). Treat
                // the two `<` chars as ordinary code and move on. Mix
                // has no shift operators today; this is conservative.
                i += 2;
                col += 2;
                continue;
            }
            // Advance to end of opener line.
            while j < chars.len() && chars[j] != '\n' {
                j += 1;
            }
            // Skip the trailing newline so body starts on next line.
            if j < chars.len() {
                j += 1;
                line += 1;
                col = 1;
            }
            // Scan body lines for the closing tag.
            loop {
                let line_start = j;
                while j < chars.len() && chars[j] != '\n' {
                    j += 1;
                }
                let body_line: String = chars[line_start..j].iter().collect();
                if body_line.trim() == tag {
                    // Position past the closing tag line (lexer
                    // rewinds the trailing newline for statement
                    // boundary; we don't care for audit purposes).
                    i = j;
                    if i < chars.len() && chars[i] == '\n' {
                        line += 1;
                        col = 1;
                        i += 1;
                    }
                    break;
                }
                if j >= chars.len() {
                    // Unterminated heredoc; lexer would error. Bail.
                    i = j;
                    break;
                }
                j += 1; // consume newline
                line += 1;
                col = 1;
            }
            continue;
        }

        // Double-quoted string — the only state where the audit fires.
        if c == '"' {
            // Skip opening `"`.
            i += 1;
            col += 1;
            // `prev_char` mirrors the lexer's planned "last buffered
            // char in current" for the rule check. None at string
            // start (so position-0 ~ never fires), reset to None on
            // every ${...} / $(...) flush.
            let mut prev_char: Option<char> = None;
            loop {
                if i >= chars.len() {
                    // Unterminated string; lexer would error. Bail.
                    break;
                }
                let c2 = chars[i];
                // Escape sequences: \" \\ \$ \~ \n \t \r \e — the
                // RESULT of the escape becomes the next prev_char.
                // For `\~`, the result is literal `~`, but critically
                // the planned lexer rule checks raw-source `~` and
                // never fires on `\~` (the `\` is the escape lead,
                // not a separator). We model this by treating the
                // escape-result as the next char buffered AND skipping
                // the tilde-rule check for this position.
                if c2 == '\\' && i + 1 < chars.len() {
                    let nxt = chars[i + 1];
                    let resulting = match nxt {
                        'n' => '\n',
                        't' => '\t',
                        'r' => '\r',
                        'e' => '\x1b',
                        '"' => '"',
                        '\\' => '\\',
                        '$' => '$',
                        '~' => '~',
                        other => other,
                    };
                    prev_char = Some(resulting);
                    if nxt == '\n' {
                        line += 1;
                        col = 1;
                    } else {
                        col += 2;
                    }
                    i += 2;
                    continue;
                }
                if c2 == '"' {
                    i += 1;
                    col += 1;
                    break;
                }
                // ${VAR} interpolation: flush current → prev_char = None.
                if c2 == '$' && i + 1 < chars.len() && chars[i + 1] == '{' {
                    prev_char = None;
                    i += 2;
                    col += 2;
                    while i < chars.len() && chars[i] != '}' {
                        if chars[i] == '\n' {
                            line += 1;
                            col = 1;
                        } else {
                            col += 1;
                        }
                        i += 1;
                    }
                    if i < chars.len() {
                        i += 1;
                        col += 1;
                    }
                    continue;
                }
                // $(cmd) substitution: flush current → prev_char = None.
                if c2 == '$' && i + 1 < chars.len() && chars[i + 1] == '(' {
                    prev_char = None;
                    i += 2;
                    col += 2;
                    let mut depth: i32 = 1;
                    while i < chars.len() && depth > 0 {
                        match chars[i] {
                            '(' => depth += 1,
                            ')' => depth -= 1,
                            '\n' => {
                                line += 1;
                                col = 0; // will be +1'd below
                            }
                            _ => {}
                        }
                        i += 1;
                        col += 1;
                    }
                    continue;
                }
                // Mid-string `~/` check.
                if c2 == '~' && i + 1 < chars.len() && chars[i + 1] == '/' {
                    if let Some(sep) = prev_char
                        && matches!(sep, ':' | '=' | ' ')
                    {
                        let snippet_start = i.saturating_sub(1);
                        let snippet_end = (i + 12).min(chars.len());
                        let snippet: String = chars[snippet_start..snippet_end].iter().collect();
                        out.push(Finding {
                            file: PathBuf::new(),
                            line,
                            column: col,
                            snippet,
                            sep,
                        });
                    }
                    prev_char = Some('~');
                    i += 1;
                    col += 1;
                    continue;
                }
                if c2 == '\n' {
                    line += 1;
                    col = 1;
                } else {
                    col += 1;
                }
                prev_char = Some(c2);
                i += 1;
            }
            continue;
        }

        // Newline tracking in code.
        if c == '\n' {
            line += 1;
            col = 1;
            i += 1;
            continue;
        }

        // Anything else in code: advance one char.
        i += 1;
        col += 1;
    }

    out
}

fn print_report(roots: &[PathBuf], files_walked: usize, findings: &[Finding]) {
    println!("== Mix tilde audit (Phase B — `[:= ]~/` in double-quoted strings) ==");
    println!("Approach: source-text state machine (handles Token::String");
    println!("single-literal collapse + `\\~` escape correctly; Codex");
    println!("review of token-walk approach flagged both as bugs).");
    println!("Roots:");
    for r in roots {
        println!("  {}", r.display());
    }
    println!("Files walked: {}", files_walked);
    println!("Tilde hits: {}", findings.len());
    println!();

    let mut by_sep: BTreeMap<char, usize> = BTreeMap::new();
    for f in findings {
        *by_sep.entry(f.sep).or_insert(0) += 1;
    }
    println!("== Hits by separator ==");
    for sep in [':', '=', ' '] {
        println!("  '{sep}~/' →  {} hits", by_sep.get(&sep).unwrap_or(&0));
    }
    println!();

    println!("== Sites needing manual review (literal-intent vs expansion-target) ==");
    let mut by_file: BTreeMap<&PathBuf, Vec<&Finding>> = BTreeMap::new();
    for f in findings {
        by_file.entry(&f.file).or_default().push(f);
    }
    for (file, group) in &by_file {
        println!("--- {} ({} hits) ---", file.display(), group.len());
        for f in group {
            println!(
                "  {}:{}: '{}~/...' (snippet: {:?})",
                f.line, f.column, f.sep, f.snippet
            );
        }
    }
}
