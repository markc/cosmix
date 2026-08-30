use crate::continuation::{ContinuationSite, continuation_sites};
use crate::error::{MixError, MixResult, Span};
use crate::token::{SpannedToken, StringPart, Token};

pub struct Lexer {
    source: Vec<char>,
    pos: usize,
    line: usize,
    column: usize,
    token_start: usize,
    paren_depth: usize,
    bracket_depth: usize,
    brace_depth: usize,
    continuation_sites: Vec<ContinuationSite>,
    continuation_error: Option<MixError>,
}

impl Lexer {
    pub fn new(source: &str) -> Self {
        let (continuation_sites, continuation_error) = match continuation_sites(source) {
            Ok(sites) => (sites, None),
            Err(error) => (Vec::new(), Some(error)),
        };
        Lexer {
            source: source.chars().collect(),
            pos: 0,
            line: 1,
            column: 1,
            token_start: 0,
            paren_depth: 0,
            bracket_depth: 0,
            brace_depth: 0,
            continuation_sites,
            continuation_error,
        }
    }

    pub fn tokenize(&mut self) -> MixResult<Vec<SpannedToken>> {
        if let Some(error) = self.continuation_error.take() {
            return Err(error);
        }
        let mut tokens = Vec::new();
        loop {
            let tok = self.next_token()?;
            let is_eof = tok.token == Token::Eof;
            tokens.push(tok);
            if is_eof {
                break;
            }
        }
        Ok(tokens)
    }

    fn peek(&self) -> Option<char> {
        self.source.get(self.pos).copied()
    }

    fn peek_ahead(&self, offset: usize) -> Option<char> {
        self.source.get(self.pos + offset).copied()
    }

    fn advance(&mut self) -> Option<char> {
        let ch = self.source.get(self.pos).copied()?;
        self.pos += 1;
        if ch == '\n' {
            self.line += 1;
            self.column = 1;
        } else {
            self.column += 1;
        }
        Some(ch)
    }

    fn spanned(&self, token: Token, line: usize, column: usize) -> SpannedToken {
        SpannedToken {
            token,
            line,
            column,
            offset: self.token_start,
        }
    }

    fn skip_whitespace_no_newline(&mut self) {
        while let Some(ch) = self.peek() {
            if ch == ' ' || ch == '\t' || ch == '\r' {
                self.advance();
            } else if let Ok(site_idx) = self
                .continuation_sites
                .binary_search_by_key(&self.pos, |site| site.backslash)
            {
                let newline_chars = self.continuation_sites[site_idx].newline_chars;
                self.advance(); // skip continuation backslash
                for _ in 0..newline_chars {
                    self.advance(); // skip LF, or CRLF
                }
            } else {
                break;
            }
        }
    }

    fn skip_comment(&mut self) {
        while let Some(ch) = self.peek() {
            if ch == '\n' {
                break;
            }
            self.advance();
        }
    }

    fn next_token(&mut self) -> MixResult<SpannedToken> {
        // Loop instead of tail self-recursion for the skip cases
        // (comments, suppressed newlines): a long run of either must
        // not depend on LLVM TCO to keep the stack flat.
        loop {
            self.skip_whitespace_no_newline();

            self.token_start = self.pos;
            let line = self.line;
            let col = self.column;

            let ch = match self.peek() {
                None => return Ok(self.spanned(Token::Eof, line, col)),
                Some(c) => c,
            };

            // Comments
            if ch == '#' {
                self.skip_comment();
                continue;
            }
            if ch == '-' && self.peek_ahead(1) == Some('-') {
                self.skip_comment();
                continue;
            }

            // Newline
            if ch == '\n' {
                self.advance();
                // Skip consecutive newlines
                while self.peek() == Some('\n') {
                    self.advance();
                }
                // Suppress newlines inside grouping
                if self.paren_depth > 0 || self.bracket_depth > 0 || self.brace_depth > 0 {
                    continue;
                }
                return Ok(self.spanned(Token::Newline, line, col));
            }

            return self.next_token_at(ch, line, col);
        }
    }

    /// The non-skip remainder of `next_token`: `ch` is the first
    /// character of a real token starting at the current position.
    fn next_token_at(&mut self, ch: char, line: usize, col: usize) -> MixResult<SpannedToken> {
        // Numbers
        if ch.is_ascii_digit()
            || (ch == '.' && self.peek_ahead(1).is_some_and(|c| c.is_ascii_digit()))
        {
            return self.lex_number(line, col);
        }

        // Strings
        if ch == '"' {
            return self.lex_double_string(line, col);
        }
        if ch == '\'' {
            return self.lex_single_string(line, col);
        }

        // Variable $
        if ch == '$' {
            return self.lex_variable(line, col);
        }

        // Identifiers and keywords
        if ch.is_ascii_alphabetic() || ch == '_' {
            return self.lex_identifier(line, col);
        }

        // Operators and delimiters
        self.advance();
        match ch {
            '+' => Ok(self.spanned(Token::Plus, line, col)),
            '-' => Ok(self.spanned(Token::Minus, line, col)),
            '*' => {
                if self.peek() == Some('*') {
                    self.advance();
                    Ok(self.spanned(Token::Power, line, col))
                } else {
                    Ok(self.spanned(Token::Star, line, col))
                }
            }
            '/' => Ok(self.spanned(Token::Slash, line, col)),
            '%' => Ok(self.spanned(Token::Percent, line, col)),
            '=' => {
                if self.peek() == Some('=') {
                    self.advance();
                    Ok(self.spanned(Token::Eq, line, col))
                } else {
                    Ok(self.spanned(Token::Assign, line, col))
                }
            }
            '!' => {
                if self.peek() == Some('=') {
                    self.advance();
                    Ok(self.spanned(Token::NotEq, line, col))
                } else {
                    Ok(self.spanned(Token::Bang, line, col))
                }
            }
            '>' => {
                if self.peek() == Some('=') {
                    self.advance();
                    Ok(self.spanned(Token::GtEq, line, col))
                } else {
                    Ok(self.spanned(Token::Gt, line, col))
                }
            }
            '<' => {
                if self.peek() == Some('=') {
                    self.advance();
                    Ok(self.spanned(Token::LtEq, line, col))
                } else if self.peek() == Some('<') {
                    self.advance(); // skip second <
                    self.lex_heredoc(line, col)
                } else {
                    Ok(self.spanned(Token::Lt, line, col))
                }
            }
            '.' => {
                if self.peek() == Some('.') {
                    self.advance();
                    Ok(self.spanned(Token::DotDot, line, col))
                } else {
                    Ok(self.spanned(Token::Dot, line, col))
                }
            }
            '?' => {
                if self.peek() == Some('?') {
                    self.advance();
                    Ok(self.spanned(Token::NilCoalesce, line, col))
                } else {
                    // Lone `?` is the ternary operator (`cond ? a : b`).
                    // It was a lex error before ternary existed; making it
                    // a token is purely additive — no prior valid Mix
                    // source contained a bare `?`. One intended REPL
                    // consequence: a non-command line like `foo ? a : b`
                    // now lexes+parses as a Mix ternary (was a tie-break
                    // "command not found"); a real command head still
                    // routes to the shell before the Mix lexer runs.
                    Ok(self.spanned(Token::Question, line, col))
                }
            }
            '|' => {
                if self.peek() == Some('|') {
                    self.advance();
                    Ok(self.spanned(Token::OrOr, line, col))
                } else {
                    Ok(self.spanned(Token::Pipe, line, col))
                }
            }
            '&' => {
                if self.peek() == Some('&') {
                    self.advance();
                    Ok(self.spanned(Token::AndAnd, line, col))
                } else {
                    Err(MixError::LexerError {
                        msg: "unexpected '&', did you mean '&&'?".to_string(),
                        span: Span {
                            line,
                            column: col,
                            file: None,
                        },
                    })
                }
            }
            '(' => {
                self.paren_depth += 1;
                Ok(self.spanned(Token::LParen, line, col))
            }
            ')' => {
                self.paren_depth = self.paren_depth.saturating_sub(1);
                Ok(self.spanned(Token::RParen, line, col))
            }
            '[' => {
                self.bracket_depth += 1;
                Ok(self.spanned(Token::LBracket, line, col))
            }
            ']' => {
                self.bracket_depth = self.bracket_depth.saturating_sub(1);
                Ok(self.spanned(Token::RBracket, line, col))
            }
            '{' => {
                self.brace_depth += 1;
                Ok(self.spanned(Token::LBrace, line, col))
            }
            '}' => {
                self.brace_depth = self.brace_depth.saturating_sub(1);
                Ok(self.spanned(Token::RBrace, line, col))
            }
            ':' => Ok(self.spanned(Token::Colon, line, col)),
            ',' => Ok(self.spanned(Token::Comma, line, col)),
            // Executable-Mix statement separator. Unlike a physical newline,
            // this token is deliberately emitted inside grouping too; the
            // parser accepts it only where a statement boundary is legal and
            // rejects it in ordinary expressions / strict-data.
            ';' => Ok(self.spanned(Token::Semicolon, line, col)),
            // Bare `~` outside double-quoted strings. Emitted so the parser
            // can recognise it as the leading char of a bareword `source`
            // path (e.g. `source ~/.mixrc`). Inside `"..."` the leading-`~`
            // expansion in `lex_double_string` handles it before we get
            // here; mid-string `~` stays literal. Bare `~` has no
            // standalone expression meaning — the parser only consumes it
            // in bareword-path contexts; reaching it elsewhere falls
            // through to the usual "unexpected token" error.
            '~' => Ok(self.spanned(Token::Tilde, line, col)),
            _ => Err(MixError::LexerError {
                msg: format!("unexpected character '{}'", ch),
                span: Span {
                    line,
                    column: col,
                    file: None,
                },
            }),
        }
    }

    fn lex_number(&mut self, line: usize, col: usize) -> MixResult<SpannedToken> {
        // Radix integer literals: 0x.. (hex), 0o.. (octal), 0b.. (binary).
        // Mix has a single f64 numeric type, so these are sugar yielding the
        // f64 value — convenient for file modes / bitmasks (`0o755` == 493).
        if self.peek() == Some('0') {
            let radix = match self.peek_ahead(1) {
                Some('x' | 'X') => Some(16u32),
                Some('o' | 'O') => Some(8),
                Some('b' | 'B') => Some(2),
                _ => None,
            };
            if let Some(radix) = radix {
                return self.lex_radix_number(line, col, radix);
            }
        }

        let mut s = String::new();
        while let Some(ch) = self.peek() {
            if ch.is_ascii_digit() || ch == '.' || ch == '_' {
                if ch != '_' {
                    s.push(ch);
                }
                self.advance();
            } else {
                break;
            }
        }

        // Reject an ambiguous leading zero on the INTEGER PART (`0755`,
        // `007`, and also `0755.5` — the same hazard for floats). Rust's
        // f64 parser silently drops a leading zero (`0755` -> `755.0`,
        // `0755.5` -> `755.5`) — a footgun for a substrate whose daily work
        // is file modes. Force an explicit choice: a 0o/0x/0b radix prefix,
        // or no leading zero for decimal. (Plain `0`, and any genuine
        // fraction like `0.5` / `0.0`, have a single-char integer part and
        // are unaffected.)
        let int_part = s.split('.').next().unwrap_or(s.as_str());
        if int_part.len() > 1 && int_part.starts_with('0') {
            return Err(MixError::LexerError {
                msg: format!(
                    "ambiguous leading-zero number '{s}' — use a 0o (octal) / 0x (hex) / \
                     0b (binary) prefix, or drop the leading zero(s) for decimal"
                ),
                span: Span {
                    line,
                    column: col,
                    file: None,
                },
            });
        }

        // Optional scientific-notation exponent: `[eE][+-]?[0-9_]+`. Only
        // consume the `e`/`E` when a valid exponent actually follows, so a
        // number immediately followed by a bare `e` token (e.g. the `e()`
        // euler-constant builtin, `2 * e()`) is unaffected — `2e` with no
        // following digit stays Number(2) + `e`. Done AFTER the leading-zero
        // check so the check still sees only the mantissa (`0e5` is a valid
        // 0.0, `07e2` is still a rejected ambiguous leading zero).
        if matches!(self.peek(), Some('e' | 'E')) {
            let sign_len = usize::from(matches!(self.peek_ahead(1), Some('+' | '-')));
            if matches!(self.peek_ahead(1 + sign_len), Some(c) if c.is_ascii_digit()) {
                s.push(self.advance().expect("peeked e/E")); // e / E
                if sign_len == 1 {
                    s.push(self.advance().expect("peeked sign")); // + / -
                }
                while let Some(ch) = self.peek() {
                    if ch.is_ascii_digit() || ch == '_' {
                        if ch != '_' {
                            s.push(ch);
                        }
                        self.advance();
                    } else {
                        break;
                    }
                }
            }
        }

        let n: f64 = s.parse().map_err(|_| MixError::LexerError {
            msg: format!("invalid number '{}'", s),
            span: Span {
                line,
                column: col,
                file: None,
            },
        })?;
        Ok(self.spanned(Token::Number(n), line, col))
    }

    /// Lex a `0x` / `0o` / `0b` radix integer literal into a `Number`. The
    /// `0` and prefix char have been peeked but not consumed. `_` digit
    /// separators are allowed. An empty body, a non-64-bit value, or a value
    /// beyond f64's exact-integer range (2^53) is a lex error rather than a
    /// silently-rounded number — Mix's numeric type is f64.
    fn lex_radix_number(&mut self, line: usize, col: usize, radix: u32) -> MixResult<SpannedToken> {
        let span = || Span {
            line,
            column: col,
            file: None,
        };
        let zero = self.advance().unwrap_or('0');
        let prefix_ch = self.advance().unwrap_or('?');
        let prefix = format!("{zero}{prefix_ch}");

        let radix_name = match radix {
            16 => "hex",
            8 => "octal",
            2 => "binary",
            _ => "radix",
        };
        let mut digits = String::new();
        while let Some(ch) = self.peek() {
            if ch == '_' {
                self.advance();
            } else if ch.is_digit(radix) {
                digits.push(ch);
                self.advance();
            } else {
                break;
            }
        }
        // A plausible-but-invalid digit immediately after the body — OR with
        // an empty body — is a malformed literal. Reject it at the lexer
        // boundary rather than splitting it into value + stray token: without
        // this, `0o78` would lex as `0o7` (7) then a stray `8`, and `0x1G` as
        // `0x1` then `G`. Only an ALPHANUMERIC follower is a bad digit; an
        // operator / `.` / space / EOF is a legitimate token boundary
        // (`0xFF+1`, `0o7 .. "x"`, `0b101)` all stay valid).
        if let Some(c) = self.peek()
            && c.is_alphanumeric()
        {
            return Err(MixError::LexerError {
                msg: format!("'{c}' is not a valid {radix_name} digit in a {prefix} literal"),
                span: span(),
            });
        }
        if digits.is_empty() {
            return Err(MixError::LexerError {
                msg: format!("{prefix} literal has no digits"),
                span: span(),
            });
        }
        let v = u64::from_str_radix(&digits, radix).map_err(|_| MixError::LexerError {
            msg: format!("{prefix}{digits} is out of range for a 64-bit integer"),
            span: span(),
        })?;
        if v > (1u64 << 53) {
            return Err(MixError::LexerError {
                msg: format!(
                    "{prefix}{digits} exceeds the exact-integer range (2^53); Mix numbers are f64"
                ),
                span: span(),
            });
        }
        Ok(self.spanned(Token::Number(v as f64), line, col))
    }

    fn lex_single_string(&mut self, line: usize, col: usize) -> MixResult<SpannedToken> {
        self.advance(); // skip opening '
        let mut s = String::new();
        loop {
            match self.advance() {
                None => {
                    return Err(MixError::LexerError {
                        msg: "unterminated string".to_string(),
                        span: Span {
                            line,
                            column: col,
                            file: None,
                        },
                    });
                }
                Some('\'') => break,
                Some('\\') => match self.advance() {
                    Some('\'') => s.push('\''),
                    Some('\\') => s.push('\\'),
                    Some(c) => {
                        s.push('\\');
                        s.push(c);
                    }
                    None => {
                        return Err(MixError::LexerError {
                            msg: "unterminated string".to_string(),
                            span: Span {
                                line,
                                column: col,
                                file: None,
                            },
                        });
                    }
                },
                Some(c) => s.push(c),
            }
        }
        Ok(self.spanned(Token::String(s), line, col))
    }

    fn lex_double_string(&mut self, line: usize, col: usize) -> MixResult<SpannedToken> {
        self.advance(); // skip opening "
        let mut parts: Vec<StringPart> = Vec::new();
        let mut current = String::new();

        // Leading `~` expansion: a bare `~` or `~/...` at the very start of
        // a double-quoted string expands to `$HOME` at runtime. Mid-string
        // `~` is always literal (preserves DNS zone tokens like "~bus",
        // "~example.com", "~." in existing scripts). `~user` is not
        // supported — only the running user's HOME.
        if self.peek() == Some('~') && matches!(self.peek_ahead(1), Some('/') | Some('"')) {
            self.advance(); // consume the `~`
            parts.push(StringPart::EnvVar("HOME".to_string()));
        }

        loop {
            match self.peek() {
                None => {
                    return Err(MixError::LexerError {
                        msg: "unterminated string".to_string(),
                        span: Span {
                            line,
                            column: col,
                            file: None,
                        },
                    });
                }
                Some('"') => {
                    self.advance();
                    break;
                }
                Some('\\') => {
                    self.advance();
                    match self.advance() {
                        Some('n') => current.push('\n'),
                        Some('t') => current.push('\t'),
                        Some('r') => current.push('\r'),
                        Some('e') => current.push('\x1b'),
                        Some('"') => current.push('"'),
                        Some('\\') => current.push('\\'),
                        Some('$') => current.push('$'),
                        Some('~') => current.push('~'),
                        // `\u{XXXX}` unicode escape (1–6 hex digits in
                        // braces, Rust convention) — decodes to the
                        // codepoint so a script can match/strip a real
                        // char (e.g. `\u{FEFF}` zero-width no-break space).
                        // ONLY the braced form is an escape: a bare `\u`
                        // (no following `{`) stays literal `\u`, so existing
                        // strings with a Windows path (`\users`) or an
                        // embedded JSON `\uXXXX` are unchanged — a pure
                        // addition, not a break.
                        Some('u') if self.peek() == Some('{') => {
                            self.lex_unicode_escape(line, col, &mut current)?
                        }
                        Some(c) => {
                            current.push('\\');
                            current.push(c);
                        }
                        None => {
                            return Err(MixError::LexerError {
                                msg: "unterminated string".to_string(),
                                span: Span {
                                    line,
                                    column: col,
                                    file: None,
                                },
                            });
                        }
                    }
                }
                Some('$') => {
                    if self.peek_ahead(1) == Some('{') {
                        // ${varname} interpolation
                        if !current.is_empty() {
                            parts.push(StringPart::Literal(std::mem::take(&mut current)));
                        }
                        self.advance(); // skip $
                        self.advance(); // skip {
                        let mut var_name = String::new();
                        loop {
                            match self.advance() {
                                Some('}') => break,
                                Some(c) => var_name.push(c),
                                None => {
                                    return Err(MixError::LexerError {
                                        msg: "unterminated interpolation".to_string(),
                                        span: Span {
                                            line,
                                            column: col,
                                            file: None,
                                        },
                                    });
                                }
                            }
                        }
                        parts.push(StringPart::Variable(var_name));
                    } else {
                        // `$(` is NOT command substitution in a double-quoted
                        // string — removed footgun: it used to shell out and
                        // execute at lex/build time on the ORCHESTRATOR, not on
                        // the target shell. `$` followed by anything but `{` is
                        // now literal text. For a shell `$(...)` destined for a
                        // child/remote shell, use a single-quoted 'raw' string;
                        // to splice a command's output, use `run()`/`run_rc()`
                        // with `..` concat. (Standalone `$(cmd)` as an EXPRESSION
                        // and heredoc `$(...)` are unaffected — separate forms.)
                        current.push('$');
                        self.advance();
                    }
                }
                Some(c) => {
                    current.push(c);
                    self.advance();
                }
            }
        }

        if !current.is_empty() {
            parts.push(StringPart::Literal(current));
        }

        // Optimize: if no interpolation, return plain string
        if parts.is_empty() {
            Ok(self.spanned(Token::String(String::new()), line, col))
        } else if parts.len() == 1 {
            if let StringPart::Literal(s) = &parts[0] {
                return Ok(self.spanned(Token::String(s.clone()), line, col));
            }
            Ok(self.spanned(Token::InterpString(parts), line, col))
        } else {
            Ok(self.spanned(Token::InterpString(parts), line, col))
        }
    }

    /// Parse a `\u{XXXX}` unicode escape (the `\u` is already consumed) and
    /// push the decoded codepoint onto `out`. 1–6 hex digits in braces,
    /// matching Rust/JS. Errors loudly on a missing brace, empty/over-long
    /// hex, a non-hex digit, or a codepoint that isn't a valid `char`
    /// (e.g. a surrogate) — never silently passes through.
    fn lex_unicode_escape(&mut self, line: usize, col: usize, out: &mut String) -> MixResult<()> {
        let err = |msg: String| MixError::LexerError {
            msg,
            span: Span {
                line,
                column: col,
                file: None,
            },
        };
        if self.advance() != Some('{') {
            return Err(err(
                "\\u must be followed by '{' — write \\u{FEFF} (braced hex)".to_string(),
            ));
        }
        let mut hex = String::new();
        loop {
            match self.advance() {
                Some('}') => break,
                Some(c) if c.is_ascii_hexdigit() => {
                    hex.push(c);
                    if hex.len() > 6 {
                        return Err(err("\\u{...} takes at most 6 hex digits".to_string()));
                    }
                }
                Some(c) => {
                    return Err(err(format!("invalid hex digit '{c}' in \\u{{...}}")));
                }
                None => return Err(err("unterminated \\u{...} escape".to_string())),
            }
        }
        if hex.is_empty() {
            return Err(err("empty \\u{} escape — write \\u{FEFF}".to_string()));
        }
        let cp =
            u32::from_str_radix(&hex, 16).map_err(|_| err(format!("invalid \\u{{{hex}}} hex")))?;
        let ch = char::from_u32(cp)
            .ok_or_else(|| err(format!("\\u{{{hex}}} is not a valid unicode codepoint")))?;
        out.push(ch);
        Ok(())
    }

    fn lex_heredoc(&mut self, line: usize, col: usize) -> MixResult<SpannedToken> {
        // Read the delimiter tag (e.g., END, HTML, EOF)
        let mut tag = String::new();
        while let Some(ch) = self.peek() {
            if ch.is_ascii_alphanumeric() || ch == '_' {
                tag.push(ch);
                self.advance();
            } else {
                break;
            }
        }
        if tag.is_empty() {
            return Err(MixError::LexerError {
                msg: "expected heredoc tag after '<<'".to_string(),
                span: Span {
                    line,
                    column: col,
                    file: None,
                },
            });
        }

        // Skip to end of current line (consume the newline after <<TAG)
        while let Some(ch) = self.peek() {
            if ch == '\n' {
                self.advance(); // advance() handles line counting
                break;
            }
            self.advance();
        }

        // Accumulate lines until we find the closing tag on its own line
        let mut body = String::new();
        loop {
            // Read a line
            let mut line_content = String::new();
            let mut hit_eof = true;
            while let Some(ch) = self.peek() {
                if ch == '\n' {
                    self.advance(); // advance() handles line counting
                    hit_eof = false;
                    break;
                }
                line_content.push(ch);
                self.advance();
            }

            // Check if this line is the closing tag
            if line_content.trim() == tag {
                // Put back the newline so the main lexer sees a statement boundary
                if !hit_eof {
                    self.pos -= 1;
                    self.line -= 1;
                    // Rewind column to end of previous line (approximate — column doesn't matter much)
                    self.column = 1;
                }
                break;
            }

            body.push_str(&line_content);
            body.push('\n');

            if hit_eof {
                return Err(MixError::LexerError {
                    msg: format!("unterminated heredoc (expected '{}')", tag),
                    span: Span {
                        line,
                        column: col,
                        file: None,
                    },
                });
            }
        }

        // Remove trailing newline
        if body.ends_with('\n') {
            body.pop();
        }

        // Process interpolation (same as double-quoted strings)
        let mut parts: Vec<StringPart> = Vec::new();
        let mut current = String::new();
        let chars: Vec<char> = body.chars().collect();
        let mut i = 0;

        while i < chars.len() {
            if chars[i] == '$' && i + 1 < chars.len() && chars[i + 1] == '{' {
                // ${var} interpolation
                if !current.is_empty() {
                    parts.push(StringPart::Literal(std::mem::take(&mut current)));
                }
                i += 2; // skip ${
                let mut var_name = String::new();
                while i < chars.len() && chars[i] != '}' {
                    var_name.push(chars[i]);
                    i += 1;
                }
                if i < chars.len() {
                    i += 1; // skip }
                }
                parts.push(StringPart::Variable(var_name));
            } else if chars[i] == '$' && i + 1 < chars.len() && chars[i + 1] == '(' {
                // $(command) substitution
                if !current.is_empty() {
                    parts.push(StringPart::Literal(std::mem::take(&mut current)));
                }
                i += 2; // skip $(
                let mut cmd = String::new();
                let mut depth = 1;
                while i < chars.len() {
                    match chars[i] {
                        '(' => {
                            depth += 1;
                            cmd.push('(');
                        }
                        ')' => {
                            depth -= 1;
                            if depth == 0 {
                                break;
                            }
                            cmd.push(')');
                        }
                        c => cmd.push(c),
                    }
                    i += 1;
                }
                if i < chars.len() {
                    i += 1;
                } // skip )
                parts.push(StringPart::CommandSub(cmd));
            } else if chars[i] == '\\' && i + 1 < chars.len() {
                // Escape sequences
                i += 1;
                match chars[i] {
                    'n' => current.push('\n'),
                    't' => current.push('\t'),
                    'r' => current.push('\r'),
                    'e' => current.push('\x1b'),
                    '\\' => current.push('\\'),
                    '$' => {
                        current.push('$');
                        // Preserve that this dollar was explicitly
                        // escaped. The analyzer scans literal parts for
                        // bare `$name`; ending the part at `$` prevents
                        // the following identifier from being mistaken
                        // for an unescaped spelling. Evaluation still
                        // concatenates the same bytes.
                        parts.push(StringPart::Literal(std::mem::take(&mut current)));
                    }
                    c => {
                        current.push('\\');
                        current.push(c);
                    }
                }
                i += 1;
            } else {
                current.push(chars[i]);
                i += 1;
            }
        }

        if !current.is_empty() {
            parts.push(StringPart::Literal(current));
        }

        // Keep heredocs distinct even when entirely literal so the AST
        // retains enough provenance for heredoc-only analyzer checks.
        if parts.is_empty() {
            parts.push(StringPart::Literal(String::new()));
        }
        Ok(self.spanned(Token::HeredocString(parts), line, col))
    }

    fn lex_variable(&mut self, line: usize, col: usize) -> MixResult<SpannedToken> {
        self.advance(); // skip $

        // Check for command substitution: $(command)
        if self.peek() == Some('(') {
            self.advance(); // skip (
            return self.lex_command_sub(line, col);
        }

        let mut name = String::new();
        while let Some(ch) = self.peek() {
            if ch.is_ascii_alphanumeric() || ch == '_' {
                name.push(ch);
                self.advance();
            } else {
                break;
            }
        }
        if name.is_empty() {
            return Err(MixError::LexerError {
                msg: "expected variable name after '$'".to_string(),
                span: Span {
                    line,
                    column: col,
                    file: None,
                },
            });
        }
        Ok(self.spanned(Token::Variable(name), line, col))
    }

    /// Read balanced parens for command substitution: $(...)
    fn lex_command_sub(&mut self, line: usize, col: usize) -> MixResult<SpannedToken> {
        let mut cmd = String::new();
        let mut depth = 1;
        loop {
            match self.advance() {
                Some('(') => {
                    depth += 1;
                    cmd.push('(');
                }
                Some(')') => {
                    depth -= 1;
                    if depth == 0 {
                        break;
                    }
                    cmd.push(')');
                }
                Some(c) => cmd.push(c),
                None => {
                    return Err(MixError::LexerError {
                        msg: "unterminated command substitution".to_string(),
                        span: Span {
                            line,
                            column: col,
                            file: None,
                        },
                    });
                }
            }
        }
        Ok(self.spanned(Token::CommandSub(cmd), line, col))
    }

    fn lex_identifier(&mut self, line: usize, col: usize) -> MixResult<SpannedToken> {
        let mut name = String::new();
        while let Some(ch) = self.peek() {
            if ch.is_ascii_alphanumeric() || ch == '_' {
                name.push(ch);
                self.advance();
            } else {
                break;
            }
        }
        let token = match name.as_str() {
            "if" => Token::If,
            "then" => Token::Then,
            "else" => Token::Else,
            "elif" => Token::Elif,
            "end" => Token::End,
            "for" => Token::For,
            "each" => Token::Each,
            "in" => Token::In,
            "to" => Token::To,
            "step" => Token::Step,
            "next" => Token::Next,
            "while" => Token::While,
            "done" => Token::Done,
            "loop" => Token::Loop,
            "break" => Token::Break,
            "continue" => Token::Continue,
            "function" => Token::Function,
            "fn" => Token::Function, // Rust-style short alias for `function`
            "return" => Token::Return,
            "select" => Token::Select,
            "when" => Token::When,
            "otherwise" => Token::Otherwise,
            "and" => Token::And,
            "or" => Token::Or,
            "not" => Token::Not,
            "true" => Token::True,
            "false" => Token::False,
            "nil" => Token::Nil,
            "parse" => Token::Parse,
            "with" => Token::With,
            "send" => Token::Send,
            "address" => Token::Address,
            "emit" => Token::Emit,
            "on" => Token::On,
            "try" => Token::Try,
            "catch" => Token::Catch,
            "finally" => Token::Finally,
            "die" => Token::Die,
            "export" => Token::Export,
            "alias" => Token::Alias,
            "print" => Token::Print,
            "eprint" => Token::Eprint,
            "source" => Token::Source,
            "include" => Token::Include,
            "label" => Token::Label,
            "sh" => Token::Sh,
            "eq" => Token::StrEq,
            "ne" => Token::StrNe,
            _ => Token::String(name), // bare identifier — used for function names, map keys, etc.
        };
        Ok(self.spanned(token, line, col))
    }
}
