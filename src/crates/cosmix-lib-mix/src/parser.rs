use crate::ast::*;
use crate::error::{MixError, MixResult, Span};
use crate::token::{SpannedToken, StringPart, Token};
use crate::value::Value;
use indexmap::IndexMap;

/// Maximum nesting depth for recursive parse cycles (statements,
/// expressions, unary chains, strict-data values). Deep enough for any
/// real script, shallow enough that the recursive descent stays within
/// a 2 MB worker stack even in debug builds — past it the parser
/// returns a normal `ParseError` instead of overflowing the native
/// stack (SIGABRT, uncatchable).
const MAX_NESTING_DEPTH: usize = 200;

/// Budget units one nested STATEMENT consumes. The statement cycle
/// (parse_statement → parse_pipeline → parse_single_statement → block
/// construct → parse_block) stacks several large debug frames per
/// level — measured several times an expression level — so it burns
/// the shared budget faster (max ~50 nested blocks).
const STMT_NESTING_COST: usize = 4;

/// The source lexeme of a keyword token, for positions where a keyword is
/// unambiguously a NAME (bare map keys, `.field` access, strict-data keys).
/// `Token::Function` is deliberately absent: both `fn` and `function` lex to
/// it, so the original spelling can't be recovered — returning "function"
/// for a source `fn` would silently store the wrong key.
fn keyword_lexeme(tok: &Token) -> Option<&'static str> {
    Some(match tok {
        Token::If => "if",
        Token::Then => "then",
        Token::Else => "else",
        Token::Elif => "elif",
        Token::End => "end",
        Token::For => "for",
        Token::Each => "each",
        Token::In => "in",
        Token::To => "to",
        Token::Step => "step",
        Token::Next => "next",
        Token::While => "while",
        Token::Done => "done",
        Token::Loop => "loop",
        Token::Break => "break",
        Token::Continue => "continue",
        Token::Return => "return",
        Token::Select => "select",
        Token::When => "when",
        Token::Otherwise => "otherwise",
        Token::And => "and",
        Token::Or => "or",
        Token::Not => "not",
        Token::True => "true",
        Token::False => "false",
        Token::Nil => "nil",
        Token::Parse => "parse",
        Token::With => "with",
        Token::Send => "send",
        Token::Address => "address",
        Token::Emit => "emit",
        Token::On => "on",
        Token::Try => "try",
        Token::Catch => "catch",
        Token::Finally => "finally",
        Token::Die => "die",
        Token::Export => "export",
        Token::Alias => "alias",
        Token::Print => "print",
        Token::Eprint => "eprint",
        Token::Source => "source",
        Token::Include => "include",
        Token::Label => "label",
        Token::Sh => "sh",
        Token::StrEq => "eq",
        Token::StrNe => "ne",
        _ => return None,
    })
}

pub struct Parser {
    tokens: Vec<SpannedToken>,
    source: Vec<char>,
    pos: usize,
    depth: usize,
    /// A speculative parse asks "would this parse?" and throws the answer
    /// away, so its diagnostics must never reach the user. The shell
    /// classifier probes every `&&`/`||` line before it can know whether the
    /// line is Mix at all: without this flag a deprecation warning fires on
    /// lines that go on to run as *shell*, and fires twice on the ones that
    /// really are Mix and get parsed again to produce the executed statements.
    speculative: bool,
    /// Deprecations a speculative parse withheld, in emission order. A probe
    /// whose result is discarded drops them; a *provisional* parse — one that
    /// becomes authoritative if it succeeds, like `source`'s whole-file
    /// attempt — flushes them with `take_deprecations` once it knows the
    /// statements will actually run.
    deprecations: Vec<String>,
}

impl Parser {
    pub fn new(tokens: Vec<SpannedToken>, source: &str) -> Self {
        Parser {
            tokens,
            source: source.chars().collect(),
            pos: 0,
            depth: 0,
            speculative: false,
            deprecations: Vec::new(),
        }
    }

    /// Parser for a speculative probe: identical grammar, no stderr
    /// diagnostics. Use for any parse whose `Ok` result is discarded. The
    /// authoritative parse — the one whose statements are actually executed —
    /// must use `new`, so a real deprecation still reaches the user once.
    pub fn new_speculative(tokens: Vec<SpannedToken>, source: &str) -> Self {
        Parser {
            speculative: true,
            ..Parser::new(tokens, source)
        }
    }

    /// Take the deprecations a speculative parse withheld. For a provisional
    /// parse that turned out to be authoritative: flush these so the warning
    /// reaches the user exactly once. Dropping them is the correct handling
    /// for a parse whose result is discarded.
    pub fn take_deprecations(&mut self) -> Vec<String> {
        std::mem::take(&mut self.deprecations)
    }

    /// Guarded entry to a recursive parse cycle. Callers that get
    /// `Ok(())` MUST decrement `self.depth` by the same `cost` once the
    /// nested call returns (on both the Ok and Err paths); on `Err` the
    /// counter was not incremented.
    fn enter_nested_cost(&mut self, cost: usize) -> MixResult<()> {
        if self.depth + cost > MAX_NESTING_DEPTH {
            let span = self.peek_span();
            return Err(MixError::ParseError {
                msg: format!("nesting too deep (limit {})", MAX_NESTING_DEPTH),
                span,
            });
        }
        self.depth += cost;
        Ok(())
    }

    fn enter_nested(&mut self) -> MixResult<()> {
        self.enter_nested_cost(1)
    }

    pub fn parse_program(&mut self) -> MixResult<Vec<Stmt>> {
        let mut stmts = Vec::new();
        self.skip_statement_separators();
        while !self.is_at_end() {
            let stmt = self.parse_statement()?;
            stmts.push(stmt);
            self.skip_statement_separators();
        }
        Ok(stmts)
    }

    // --- Token helpers ---

    fn peek(&self) -> &Token {
        self.tokens
            .get(self.pos)
            .map(|t| &t.token)
            .unwrap_or(&Token::Eof)
    }

    /// Token `n` positions ahead (`peek_ahead(0) == peek()`). Used by the
    /// assignment-path collector to tell `.field =` (an lvalue segment)
    /// from `.method(` (an expression), without backtracking.
    fn peek_ahead(&self, n: usize) -> &Token {
        self.tokens
            .get(self.pos + n)
            .map(|t| &t.token)
            .unwrap_or(&Token::Eof)
    }

    fn peek_span(&self) -> Span {
        self.tokens
            .get(self.pos)
            .map(|t| Span {
                line: t.line,
                column: t.column,
                file: None,
            })
            .unwrap_or(Span {
                line: 0,
                column: 0,
                file: None,
            })
    }

    fn current_line(&self) -> usize {
        self.tokens.get(self.pos).map(|t| t.line).unwrap_or(0)
    }

    /// Line of the most recently consumed token — the anchor for "something
    /// is missing AFTER this" diagnostics (a missing separator is a defect
    /// of the entry that lacks it, not of whatever happens to follow).
    fn previous_line(&self) -> usize {
        self.pos
            .checked_sub(1)
            .and_then(|i| self.tokens.get(i))
            .map(|t| t.line)
            .unwrap_or(0)
    }

    /// Whether the current token is a double-quoted source literal containing
    /// an escaped quote. The lexer necessarily decodes `\"` to `"`; retain
    /// this one source fact for the ssh nested-quoting lint.
    fn current_string_has_escaped_quote(&self) -> bool {
        let Some(token) = self.tokens.get(self.pos) else {
            return false;
        };
        let mut pos = token.offset;
        if self.source.get(pos) != Some(&'"') {
            return false;
        }
        pos += 1;
        while let Some(ch) = self.source.get(pos) {
            match ch {
                '"' => return false,
                '\\' => {
                    if self.source.get(pos + 1) == Some(&'"') {
                        return true;
                    }
                    pos += 2;
                }
                _ => pos += 1,
            }
        }
        false
    }

    /// v0.2.2 block-terminator acceptor: accept `end` universally
    /// for any block, with the construct's legacy terminator
    /// (`done` / `next`) still parsing but emitting a deprecation
    /// warning to stderr. This is the Phase 5 Option B landing:
    /// one style (`end`) for everything, old styles supported for
    /// one release cycle so existing scripts don't break on upgrade.
    ///
    /// Returns `Ok(())` if either form was consumed; otherwise the
    /// usual "expected X, got Y" parse error. Keeps the warning
    /// side-channel in one place — callers shouldn't try to
    /// eprintln! their own deprecations.
    fn expect_end_or_legacy(&mut self, construct: &str, legacy: &Token) -> MixResult<()> {
        if self.check(&Token::End) {
            self.advance();
            return Ok(());
        }
        if self.check(legacy) {
            let line = self.current_line();
            self.advance();
            let legacy_name = match legacy {
                Token::Done => "done",
                Token::Next => "next",
                _ => "<legacy>",
            };
            let message = format!(
                "warning: {} line {}: '{}' is deprecated for '{}' blocks, use 'end' instead",
                self.source_hint(),
                line,
                legacy_name,
                construct
            );
            if self.speculative {
                self.deprecations.push(message);
            } else {
                eprintln!("{}", message);
            }
            return Ok(());
        }
        let span = self.peek_span();
        let tok = self.peek().clone();
        Err(MixError::ParseError {
            msg: format!(
                "expected 'end' or '{:?}' to close {} block, got {:?}",
                legacy, construct, tok
            ),
            span,
        })
    }

    /// Short filename-or-<script> hint for deprecation warnings.
    /// The parser doesn't currently track a source path; return a
    /// static placeholder so warnings have a uniform shape. Callers
    /// that want filename reporting can switch this to read from a
    /// field later without changing every call site.
    fn source_hint(&self) -> &'static str {
        "<mix>"
    }

    fn advance(&mut self) -> &Token {
        let tok = self
            .tokens
            .get(self.pos)
            .map(|t| &t.token)
            .unwrap_or(&Token::Eof);
        if self.pos < self.tokens.len() {
            self.pos += 1;
        }
        tok
    }

    fn expect(&mut self, expected: &Token) -> MixResult<()> {
        let span = self.peek_span();
        let tok = self.advance().clone();
        if std::mem::discriminant(&tok) == std::mem::discriminant(expected) {
            Ok(())
        } else {
            Err(MixError::ParseError {
                msg: if matches!(tok, Token::Semicolon) {
                    "unexpected ';' inside expression; ';' separates Mix statements only"
                        .to_string()
                } else {
                    format!("expected {:?}, got {:?}", expected, tok)
                },
                span,
            })
        }
    }

    fn expect_identifier(&mut self) -> MixResult<String> {
        let span = self.peek_span();
        let tok = self.advance().clone();
        match tok {
            Token::String(s) => Ok(s),
            _ => Err(MixError::ParseError {
                msg: format!("expected identifier, got {:?}", tok),
                span,
            }),
        }
    }

    /// A NAME in a position where a keyword is unambiguous — bare map keys
    /// (`{label: 1}`), `.field` access (`$m.to`), and strict-data keys (a
    /// `.conf.mix` key named `on`/`in`/`to`). Accepts a bareword or any
    /// keyword's source lexeme, retiring the reserved-word footgun. The one
    /// exception is `fn`: it lexes to the same token as `function`, so the
    /// original spelling is unrecoverable and `"fn"` still needs quoting.
    fn expect_name_or_keyword(&mut self) -> MixResult<String> {
        if let Some(kw) = keyword_lexeme(self.peek()) {
            self.advance();
            return Ok(kw.to_string());
        }
        self.expect_identifier()
    }

    /// Like [`expect_name_or_keyword`](Self::expect_name_or_keyword)
    /// but additionally accepts `function`/`fn` — used ONLY in field
    /// positions (`$m.function`, `$m.function = v`). Both spellings lex
    /// to the same token, so both read the canonical "function" name;
    /// in field position that ambiguity is harmless. Needed since
    /// 0.29.0 because error-traceback frames carry a `function` key
    /// (`$err.frames[0].function`). Map-literal and strict-data keys
    /// deliberately keep rejecting the bareword (write `"function"` —
    /// `data_encode` quotes it automatically).
    fn expect_field_name(&mut self) -> MixResult<String> {
        if matches!(self.peek(), Token::Function) {
            self.advance();
            return Ok("function".to_string());
        }
        self.expect_name_or_keyword()
    }

    fn expect_variable(&mut self) -> MixResult<String> {
        let span = self.peek_span();
        let tok = self.advance().clone();
        match tok {
            Token::Variable(name) => Ok(name),
            _ => Err(MixError::ParseError {
                msg: format!("expected variable, got {:?}", tok),
                span,
            }),
        }
    }

    fn check(&self, token: &Token) -> bool {
        std::mem::discriminant(self.peek()) == std::mem::discriminant(token)
    }

    fn match_token(&mut self, token: &Token) -> bool {
        if self.check(token) {
            self.advance();
            true
        } else {
            false
        }
    }

    fn is_at_end(&self) -> bool {
        matches!(self.peek(), Token::Eof)
    }

    fn skip_newlines(&mut self) {
        while matches!(self.peek(), Token::Newline) {
            self.advance();
        }
    }

    /// Skip executable-Mix statement separators. Keep this separate from
    /// `skip_newlines`: strict-data deliberately accepts physical newlines and
    /// commas only, never semicolons.
    fn skip_statement_separators(&mut self) {
        while matches!(self.peek(), Token::Newline | Token::Semicolon) {
            self.advance();
        }
    }

    fn is_statement_terminator(token: &Token) -> bool {
        matches!(
            token,
            Token::Newline
                | Token::Semicolon
                | Token::Eof
                | Token::End
                | Token::Next
                | Token::Done
                | Token::Else
                | Token::Elif
                | Token::When
                | Token::Otherwise
                | Token::Catch
                | Token::Finally
                | Token::Pipe
                | Token::AndAnd
                | Token::OrOr
        )
    }

    fn at_end_of_statement(&self) -> bool {
        Self::is_statement_terminator(self.peek())
    }

    fn expect_end_of_statement(&mut self) -> MixResult<()> {
        match self.peek() {
            Token::Newline => {
                self.advance();
                // Preserve newline-continuation for `&&`/`||`, but do not
                // swallow a following semicolon: `a\n; && b` is still a hard
                // sequence boundary.
                self.skip_newlines();
                Ok(())
            }
            // Leave `;` for parse_program / parse_block to consume. Unlike a
            // newline (which Mix permits around `&&`/`||`), semicolon is the
            // looser sequence boundary: `a; && b` must not be reattached as a
            // Chain after the atomic statement parser returns.
            Token::Semicolon => Ok(()),
            Token::Eof => Ok(()),
            // Allow implicit end-of-statement before certain closing keywords
            Token::End
            | Token::Next
            | Token::Done
            | Token::Else
            | Token::Elif
            | Token::When
            | Token::Otherwise
            | Token::Catch
            | Token::Finally => Ok(()),
            // Allow pipe and chain operators (consumed by parse_statement's post-processing)
            Token::Pipe | Token::AndAnd | Token::OrOr => Ok(()),
            other => Err(MixError::ParseError {
                msg: format!("expected end of statement, got {:?}", other),
                span: self.peek_span(),
            }),
        }
    }

    // --- Statement parsing ---

    /// Parse a complete statement including any `|`, `&&`, `||` suffixes.
    /// Depth-guarded: nested blocks (`if`/`while`/`for`/`function`/`on`)
    /// recurse back here via `parse_block`.
    fn parse_statement(&mut self) -> MixResult<Stmt> {
        self.enter_nested_cost(STMT_NESTING_COST)?;
        let result = self.parse_statement_inner();
        self.depth -= STMT_NESTING_COST;
        result
    }

    fn parse_statement_inner(&mut self) -> MixResult<Stmt> {
        let mut stmt = self.parse_pipeline()?;
        // Left-associative chaining: `a && b || c` → `(a && b) || c`
        loop {
            let line = stmt.line;
            let (op, operator) = match self.peek() {
                Token::AndAnd => (ChainOp::And, "&&"),
                Token::OrOr => (ChainOp::Or, "||"),
                _ => return Ok(stmt),
            };
            let operator_span = self.peek_span();
            self.reject_assignment_chain_operand(&stmt, operator, operator_span.clone())?;
            self.advance();
            self.skip_newlines();
            let right = self.parse_pipeline()?;
            self.reject_assignment_chain_operand(&right, operator, operator_span)?;
            stmt = Stmt::new(
                StmtKind::Chain {
                    left: Box::new(stmt),
                    op,
                    right: Box::new(right),
                },
                line,
            );
        }
    }

    fn reject_assignment_chain_operand(
        &self,
        stmt: &Stmt,
        operator: &str,
        operator_span: Span,
    ) -> MixResult<()> {
        if Self::is_assignment_chain_operand(stmt) {
            return Err(MixError::assignment_chain_parse(operator, operator_span));
        }
        Ok(())
    }

    fn is_assignment_chain_operand(stmt: &Stmt) -> bool {
        match &stmt.kind {
            StmtKind::Assignment { .. }
            | StmtKind::FieldAssignment { .. }
            | StmtKind::IndexAssignment { .. }
            | StmtKind::PathAssignment { .. } => true,
            // `export x = v` always carries a value, and `alias n = c` in
            // its DEFINE form (`command: Some`) binds one — both are
            // assignments wearing a keyword, and both hid the same falsy
            // operand behind a green chain. The alias QUERY and LIST forms
            // (`command: None`) bind nothing, so they stay legal operands.
            StmtKind::Export { .. } => true,
            StmtKind::Alias {
                command: Some(_), ..
            } => true,
            // The terse `function f() = expr` form is the same shape one
            // more keyword out: it binds a value with `=`, so `function f()
            // = false || fallback()` bound `false` and dropped the fallback
            // silently. Its BLOCK form (`function f() … end`) binds no `=`
            // expression and is not assignment-shaped, so it stays legal —
            // rejecting every binder would be a materially broader policy.
            StmtKind::FunctionDef {
                body: FunctionBody::Expression(_),
                ..
            } => true,
            StmtKind::PipeToExternal { stmt, .. } => Self::is_assignment_chain_operand(stmt),
            _ => false,
        }
    }

    /// Parse a single statement optionally followed by `| external_command`.
    fn parse_pipeline(&mut self) -> MixResult<Stmt> {
        let stmt = self.parse_single_statement()?;
        let line = stmt.line;
        if matches!(self.peek(), Token::Pipe) {
            self.advance(); // consume `|`
            let command = self.collect_external_command();
            Ok(Stmt::new(
                StmtKind::PipeToExternal {
                    stmt: Box::new(stmt),
                    command,
                },
                line,
            ))
        } else {
            Ok(stmt)
        }
    }

    /// Parse one atomic statement (no pipe/chain post-processing).
    fn parse_single_statement(&mut self) -> MixResult<Stmt> {
        let line = self.current_line();
        let kind = match self.peek().clone() {
            Token::If => self.parse_if(),
            Token::For => self.parse_for(),
            Token::While => self.parse_while(),
            Token::Loop => self.parse_loop(),
            Token::Function => self.parse_function_def(),
            Token::Return => self.parse_return(),
            Token::Select => self.parse_select(),
            Token::Print => self.parse_print(false),
            Token::Eprint => self.parse_print(true),
            Token::Die => self.parse_die(),
            Token::Break => self.parse_break(),
            Token::Continue => self.parse_continue(),
            Token::Try => self.parse_try_catch(),
            Token::Parse => self.parse_parse(),
            Token::Export => self.parse_export(),
            Token::Alias => self.parse_alias(),
            Token::Send => self.parse_send(),
            Token::Address => self.parse_address(),
            Token::Emit => self.parse_emit(),
            Token::On => self.parse_on(),
            Token::Source => self.parse_source(),
            Token::Include => self.parse_include(),
            Token::Sh => self.parse_sh(),
            Token::Variable(_) => self.parse_variable_stmt(),
            _ => {
                let expr = self.parse_expression()?;
                self.expect_end_of_statement()?;
                Ok(StmtKind::Expression(expr))
            }
        }?;
        Ok(Stmt::new(kind, line))
    }

    /// Collect raw source text for the external command after `|`.
    /// Consumes tokens until Newline/Semicolon/Eof (does not consume the
    /// terminator). A semicolon is the looser Mix statement boundary, not part
    /// of the raw external pipeline tail.
    fn collect_external_command(&mut self) -> String {
        let start = self
            .tokens
            .get(self.pos)
            .map(|t| t.offset)
            .unwrap_or(self.source.len());

        while !matches!(self.peek(), Token::Newline | Token::Semicolon | Token::Eof) {
            self.advance();
        }

        let end = self
            .tokens
            .get(self.pos)
            .map(|t| t.offset)
            .unwrap_or(self.source.len());

        if start >= self.source.len() || start >= end {
            return String::new();
        }
        self.source[start..end]
            .iter()
            .collect::<String>()
            .trim()
            .to_string()
    }

    fn parse_variable_stmt(&mut self) -> MixResult<StmtKind> {
        let span = self.peek_span();
        let name = self.expect_variable()?;

        match self.peek().clone() {
            Token::Assign => {
                self.advance();
                let value = self.parse_expression()?;
                self.expect_end_of_statement()?;
                Ok(StmtKind::Assignment { name, value })
            }
            // `$config.* = default` — the wildcard-default form. Only
            // meaningful as the sole accessor, so it is matched before
            // the general accessor-chain collector below.
            Token::Dot if self.peek_ahead(1) == &Token::Star => {
                self.advance(); // .
                self.advance(); // *
                self.expect(&Token::Assign)?;
                let value = self.parse_expression()?;
                self.expect_end_of_statement()?;
                Ok(StmtKind::FieldAssignment {
                    object: name,
                    field: "*".to_string(),
                    value,
                })
            }
            // A chain of accessors after `$var`. Collect the whole run,
            // THEN classify: `=` makes it an lvalue (1 segment keeps the
            // existing Field/IndexAssignment nodes, whose evaluator arms
            // carry the for-loop fast paths; 2+ becomes PathAssignment),
            // and anything else folds the segments back into an Expr and
            // continues through the ordinary postfix/binary/ternary path.
            Token::Dot | Token::LBracket => {
                let mut segs: Vec<PathSeg> = Vec::new();
                loop {
                    match self.peek() {
                        // `.name(` is a method call — an expression, not
                        // an lvalue segment. Leave the Dot unconsumed so
                        // parse_postfix sees the call intact.
                        //
                        // EXCEPT as the very first accessor, where the
                        // pre-0.33.0 parser always folded `.name` into a
                        // FieldAccess and let the following `(` become a
                        // ValueCall — i.e. `$m.f(1)` as a STATEMENT calls
                        // the map member `$m.f`, while parse_postfix would
                        // build a MethodCall and dispatch name-first (UFCS)
                        // to a global `f` instead. Breaking here on an
                        // empty seg list would silently retarget every such
                        // call. Keep collecting; the no-`=` branch below
                        // folds it back to FieldAccess and parse_postfix
                        // sees the `(` exactly as it used to.
                        Token::Dot if !segs.is_empty() && self.peek_ahead(2) == &Token::LParen => {
                            break;
                        }
                        Token::Dot => {
                            self.advance();
                            // Keywords allowed as field names here too
                            // ($cfg.to = 1), matching parse_postfix.
                            segs.push(PathSeg::Field(self.expect_field_name()?));
                        }
                        Token::LBracket => {
                            self.advance();
                            let index = self.parse_expression()?;
                            self.expect(&Token::RBracket)?;
                            segs.push(PathSeg::Index(index));
                        }
                        _ => break,
                    }
                }

                if self.match_token(&Token::Assign) {
                    let value = self.parse_expression()?;
                    self.expect_end_of_statement()?;
                    // segs is never empty: this arm is only entered on a
                    // Dot or LBracket, and both push a segment or error.
                    if segs.len() == 1 {
                        return Ok(match segs.pop().expect("one segment") {
                            PathSeg::Field(field) => StmtKind::FieldAssignment {
                                object: name,
                                field,
                                value,
                            },
                            PathSeg::Index(index) => StmtKind::IndexAssignment {
                                object: name,
                                index,
                                value,
                            },
                        });
                    }
                    Ok(StmtKind::PathAssignment {
                        root: name,
                        path: segs,
                        value,
                    })
                } else {
                    let mut expr = Expr::Variable(name);
                    for seg in segs {
                        expr = match seg {
                            PathSeg::Field(field) => Expr::FieldAccess {
                                object: Box::new(expr),
                                field,
                            },
                            PathSeg::Index(index) => Expr::Index {
                                object: Box::new(expr),
                                index: Box::new(index),
                            },
                        };
                    }
                    let expr = self.parse_postfix(expr)?;
                    let expr = self.parse_binary_rhs(expr, 0)?;
                    let expr = self.parse_ternary_suffix(expr)?;
                    self.expect_end_of_statement()?;
                    Ok(StmtKind::Expression(expr))
                }
            }
            _ => {
                // Expression starting with a variable
                let expr = self.parse_postfix(Expr::Variable(name))?;
                let expr = self.parse_binary_rhs(expr, 0)?;
                let expr = self.parse_ternary_suffix(expr)?;

                // Check if this is used as a statement
                self.expect_end_of_statement()
                    .map_err(|_| MixError::ParseError {
                        msg: "unexpected token after expression".to_string(),
                        span,
                    })?;
                Ok(StmtKind::Expression(expr))
            }
        }
    }

    /// Parse the shared shape of an `if … then … [else if …] [else …] end`
    /// — consuming the opening `if` through the closing `end`, but NOT the
    /// trailing end-of-statement. Used by both `parse_if` (statement form,
    /// which adds `expect_end_of_statement`) and the expression-position
    /// `if` arm in `parse_primary` (which must not, since it returns a value
    /// into a larger expression).
    #[allow(clippy::type_complexity)]
    fn parse_if_parts(
        &mut self,
    ) -> MixResult<(Expr, Vec<Stmt>, Vec<(Expr, Vec<Stmt>)>, Option<Vec<Stmt>>)> {
        self.advance(); // skip 'if'
        let condition = self.parse_expression()?;
        self.expect(&Token::Then)?;
        self.skip_statement_separators();

        // Bodies in an if-chain end at `else`, the one-word `elif`, or `end`.
        let stops = [Token::Else, Token::Elif, Token::End];
        let then_body = self.parse_block(&stops)?;

        let mut else_ifs = Vec::new();
        let mut else_body = None;

        loop {
            if self.match_token(&Token::Elif) {
                // `elif <cond> then …` — a one-word synonym for `else if`,
                // parsed into the same flat `else_ifs` chain (single `end`).
                let cond = self.parse_expression()?;
                self.expect(&Token::Then)?;
                self.skip_statement_separators();
                let body = self.parse_block(&stops)?;
                else_ifs.push((cond, body));
            } else if self.match_token(&Token::Else) {
                if self.match_token(&Token::If) {
                    // `else if <cond> then …` — the equivalent two-word form.
                    let cond = self.parse_expression()?;
                    self.expect(&Token::Then)?;
                    self.skip_statement_separators();
                    let body = self.parse_block(&stops)?;
                    else_ifs.push((cond, body));
                } else {
                    self.skip_statement_separators();
                    else_body = Some(self.parse_block(&[Token::End])?);
                    break;
                }
            } else {
                break;
            }
        }

        self.expect(&Token::End)?;
        Ok((condition, then_body, else_ifs, else_body))
    }

    fn parse_if(&mut self) -> MixResult<StmtKind> {
        let (condition, then_body, else_ifs, else_body) = self.parse_if_parts()?;
        self.expect_end_of_statement()?;

        Ok(StmtKind::If {
            condition,
            then_body,
            else_ifs,
            else_body,
        })
    }

    fn parse_for(&mut self) -> MixResult<StmtKind> {
        self.advance(); // skip 'for'

        if self.match_token(&Token::Each) {
            return self.parse_for_each(None);
        }

        let var = self.expect_variable()?;
        // `each` is optional (0.63.0): `for $x in $xs` and
        // `for $i, $x in $xs` are the same iteration forms as
        // `for each …`, which stays accepted indefinitely (~1,600 fleet
        // call sites — no deprecation). One token of lookahead after the
        // variable disambiguates: `in`/`,` is iteration, `=` keeps the
        // counted loop.
        if matches!(self.peek(), Token::In | Token::Comma) {
            return self.parse_for_each(Some(var));
        }
        self.expect(&Token::Assign)?;
        let start = self.parse_expression()?;
        self.expect(&Token::To)?;
        let end = self.parse_expression()?;

        let step = if self.match_token(&Token::Step) {
            Some(self.parse_expression()?)
        } else {
            None
        };

        let label = self.parse_optional_label()?;

        self.skip_statement_separators();
        let body = self.parse_block(&[Token::Next, Token::End])?;
        self.expect_end_or_legacy("for", &Token::Next)?;
        self.expect_end_of_statement()?;

        Ok(StmtKind::For {
            var,
            start,
            end,
            step,
            body,
            label,
        })
    }

    /// Body of both iteration spellings. `pre_var` carries the first
    /// variable when `parse_for` already consumed it deciding between the
    /// bare `for $x in …` form and the counted loop.
    fn parse_for_each(&mut self, pre_var: Option<String>) -> MixResult<StmtKind> {
        // Check for index variable: for each $i, $item in $list
        let first_var = match pre_var {
            Some(v) => v,
            None => self.expect_variable()?,
        };
        let (index_var, var) = if self.match_token(&Token::Comma) {
            let second = self.expect_variable()?;
            (Some(first_var), second)
        } else {
            (None, first_var)
        };

        self.expect(&Token::In)?;
        let iterable = self.parse_expression()?;

        let label = self.parse_optional_label()?;

        self.skip_statement_separators();
        let body = self.parse_block(&[Token::Next, Token::End])?;
        self.expect_end_or_legacy("for each", &Token::Next)?;
        self.expect_end_of_statement()?;

        Ok(StmtKind::ForEach {
            var,
            index_var,
            iterable,
            body,
            label,
        })
    }

    fn parse_while(&mut self) -> MixResult<StmtKind> {
        self.advance(); // skip 'while'
        let condition = self.parse_expression()?;

        let label = self.parse_optional_label()?;

        self.skip_statement_separators();
        let body = self.parse_block(&[Token::Done, Token::End])?;
        self.expect_end_or_legacy("while", &Token::Done)?;
        self.expect_end_of_statement()?;

        Ok(StmtKind::While {
            condition,
            body,
            label,
        })
    }

    fn parse_loop(&mut self) -> MixResult<StmtKind> {
        self.advance(); // skip 'loop'

        let label = self.parse_optional_label()?;

        self.skip_statement_separators();
        let body = self.parse_block(&[Token::Done, Token::End])?;
        self.expect_end_or_legacy("loop", &Token::Done)?;
        self.expect_end_of_statement()?;

        Ok(StmtKind::Loop { body, label })
    }

    fn parse_break(&mut self) -> MixResult<StmtKind> {
        self.advance(); // skip 'break'
        // Parse optional label: `break label_name` or `break label_name if cond`
        let label = self.parse_break_continue_label();
        if self.match_token(&Token::If) {
            let cond = self.parse_expression()?;
            self.expect_end_of_statement()?;
            Ok(StmtKind::BreakIf(cond, label))
        } else {
            self.expect_end_of_statement()?;
            Ok(StmtKind::Break(label))
        }
    }

    fn parse_continue(&mut self) -> MixResult<StmtKind> {
        self.advance(); // skip 'continue'
        // Parse optional label: `continue label_name` or `continue label_name if cond`
        let label = self.parse_break_continue_label();
        if self.match_token(&Token::If) {
            let cond = self.parse_expression()?;
            self.expect_end_of_statement()?;
            Ok(StmtKind::ContinueIf(cond, label))
        } else {
            self.expect_end_of_statement()?;
            Ok(StmtKind::Continue(label))
        }
    }

    /// Parse optional `label IDENT` after a loop header.
    fn parse_optional_label(&mut self) -> MixResult<Option<String>> {
        if self.match_token(&Token::Label) {
            let name = self.expect_identifier()?;
            Ok(Some(name))
        } else {
            Ok(None)
        }
    }

    /// Parse optional label name after break/continue.
    /// Bare identifiers that aren't `if` are consumed as label names.
    fn parse_break_continue_label(&mut self) -> Option<String> {
        // If next token is a bare identifier (Token::String) and it's not "if",
        // treat it as a label name.
        if let Token::String(ref s) = self.peek().clone()
            && s != "if"
        {
            let label = s.clone();
            self.advance();
            return Some(label);
        }
        None
    }

    fn parse_function_def(&mut self) -> MixResult<StmtKind> {
        self.advance(); // skip 'function'
        let name = self.expect_identifier()?;
        let (params, body) = self.parse_function_tail(/*expression_position=*/ false)?;
        Ok(StmtKind::FunctionDef { name, params, body })
    }

    /// Parse `($params) = expr` | `($params) ... end` after the
    /// `function` keyword (and optional name). Shared between
    /// statement-position `function name(...)` (FunctionDef) and
    /// expression-position `function(...)` (FunctionLiteral).
    ///
    /// When `expression_position` is true, the trailing
    /// `expect_end_of_statement` is skipped so the literal can
    /// participate in larger expressions (e.g.
    /// `$f = function($x) = $x * 2`).
    fn parse_function_tail(
        &mut self,
        expression_position: bool,
    ) -> MixResult<(Vec<Param>, FunctionBody)> {
        self.expect(&Token::LParen)?;

        let mut params = Vec::new();
        while !self.check(&Token::RParen) {
            let pname = self.expect_variable()?;
            let default = if self.match_token(&Token::Assign) {
                Some(self.parse_expression()?)
            } else {
                None
            };
            params.push(Param {
                name: pname,
                default,
            });
            if !self.check(&Token::RParen) {
                self.expect(&Token::Comma)?;
            }
        }
        self.expect(&Token::RParen)?;

        // Single expression form: `= expr`
        if self.match_token(&Token::Assign) {
            let expr = self.parse_expression()?;
            if !expression_position {
                self.expect_end_of_statement()?;
            }
            return Ok((params, FunctionBody::Expression(expr)));
        }

        self.skip_statement_separators();
        let body = self.parse_block(&[Token::End])?;
        self.expect(&Token::End)?;
        if !expression_position {
            self.expect_end_of_statement()?;
        }

        Ok((params, FunctionBody::Block(body)))
    }

    fn parse_return(&mut self) -> MixResult<StmtKind> {
        self.advance(); // skip 'return'
        match self.peek() {
            Token::Newline | Token::Semicolon | Token::Eof | Token::End => {
                self.expect_end_of_statement()?;
                Ok(StmtKind::Return(None))
            }
            _ => {
                let expr = self.parse_expression()?;
                self.expect_end_of_statement()?;
                Ok(StmtKind::Return(Some(expr)))
            }
        }
    }

    fn parse_select(&mut self) -> MixResult<StmtKind> {
        self.advance(); // skip 'select'
        let value = self.parse_expression()?;
        self.skip_statement_separators();

        let mut cases = Vec::new();
        let mut otherwise = None;

        while !self.check(&Token::End) && !self.is_at_end() {
            if self.match_token(&Token::When) {
                let case_val = self.parse_expression()?;
                self.expect(&Token::Then)?;
                self.skip_statement_separators();
                let body = self.parse_block(&[Token::When, Token::Otherwise, Token::End])?;
                cases.push((case_val, body));
            } else if self.match_token(&Token::Otherwise) {
                self.skip_statement_separators();
                otherwise = Some(self.parse_block(&[Token::End])?);
            } else {
                break;
            }
            self.skip_statement_separators();
        }

        self.expect(&Token::End)?;
        self.expect_end_of_statement()?;

        Ok(StmtKind::Select {
            value,
            cases,
            otherwise,
        })
    }

    fn parse_print(&mut self, stderr: bool) -> MixResult<StmtKind> {
        self.advance(); // skip 'print' / 'eprint'
        let mut args = Vec::new();

        // Parse arguments until end of statement
        match self.peek() {
            Token::Newline | Token::Semicolon | Token::Eof => {}
            _ => {
                args.push(self.parse_expression()?);
                while self.match_token(&Token::Comma) {
                    args.push(self.parse_expression()?);
                }
            }
        }

        self.expect_end_of_statement()?;
        Ok(StmtKind::Print { args, stderr })
    }

    fn parse_die(&mut self) -> MixResult<StmtKind> {
        self.advance(); // skip 'die'
        let msg = self.parse_expression()?;
        self.expect_end_of_statement()?;
        Ok(StmtKind::Die(msg))
    }

    fn parse_try_catch(&mut self) -> MixResult<StmtKind> {
        self.advance(); // skip 'try'
        self.skip_statement_separators();
        // The try body ends at `catch`, `finally`, or `end` (0.30.0:
        // finally is optional and catch may be absent when finally is
        // present; `end` is allowed here so the missing-clause case
        // returns to the explicit check below instead of parse_block's
        // generic "unexpected End").
        let try_body = self.parse_block(&[Token::Catch, Token::Finally, Token::End])?;

        let catch = if matches!(self.peek(), Token::Catch) {
            self.advance();
            let var = self.expect_variable()?;
            // Optional second binding (0.29.0): `catch $msg, $err` — the
            // structured error map. Comma-gated so `catch $msg` parses
            // exactly as before.
            let err_var = if matches!(self.peek(), Token::Comma) {
                self.advance();
                Some(self.expect_variable()?)
            } else {
                None
            };
            self.skip_statement_separators();
            let body = self.parse_block(&[Token::End, Token::Finally])?;
            Some(crate::ast::CatchClause { var, err_var, body })
        } else {
            None
        };

        let finally_body = if matches!(self.peek(), Token::Finally) {
            self.advance();
            self.skip_statement_separators();
            Some(self.parse_block(&[Token::End])?)
        } else {
            None
        };

        // At least one of catch/finally must be present (a bare
        // `try … end` is meaningless).
        if catch.is_none() && finally_body.is_none() {
            return Err(MixError::ParseError {
                msg: "try requires a catch and/or finally clause".to_string(),
                span: self.peek_span(),
            });
        }

        self.expect(&Token::End)?;
        self.expect_end_of_statement()?;

        Ok(StmtKind::TryCatch {
            try_body,
            catch,
            finally_body,
        })
    }

    fn parse_parse(&mut self) -> MixResult<StmtKind> {
        self.advance(); // skip 'parse'
        let source = self.parse_expression()?;
        self.expect(&Token::With)?;

        let mut parts = Vec::new();
        loop {
            match self.peek() {
                Token::Newline | Token::Semicolon | Token::Eof | Token::End => break,
                Token::Variable(_) => {
                    let name = self.expect_variable()?;
                    parts.push(ParsePart::Variable(name));
                }
                Token::String(_) => {
                    let tok = self.advance().clone();
                    if let Token::String(s) = tok {
                        parts.push(ParsePart::Delimiter(s));
                    }
                }
                // A keyword lexeme is a literal delimiter word
                // (`parse $s with $a to $b`) — EXCEPT the block/branch
                // terminators, which must keep ending an inline parse
                // statement exactly as before (`… parse $s with $a else …`).
                t if keyword_lexeme(t).is_some()
                    && !matches!(
                        t,
                        Token::End
                            | Token::Else
                            | Token::Elif
                            | Token::Catch
                            | Token::Finally
                            | Token::When
                            | Token::Otherwise
                            | Token::Done
                            | Token::Next
                    ) =>
                {
                    let kw = keyword_lexeme(t).unwrap().to_string();
                    self.advance();
                    parts.push(ParsePart::Delimiter(kw));
                }
                _ => break,
            }
        }

        self.expect_end_of_statement()?;
        Ok(StmtKind::Parse { source, parts })
    }

    fn parse_export(&mut self) -> MixResult<StmtKind> {
        self.advance(); // skip 'export'
        let name = self.expect_identifier()?;
        self.expect(&Token::Assign)?;
        let value = self.parse_expression()?;
        self.expect_end_of_statement()?;
        Ok(StmtKind::Export { name, value })
    }

    fn parse_alias(&mut self) -> MixResult<StmtKind> {
        self.advance(); // skip 'alias'

        // `alias` alone — list all aliases
        if self.at_end_of_statement() {
            return Ok(StmtKind::Alias {
                name: None,
                command: None,
            });
        }

        // Name side accepts a bare identifier (`alias gw = …`) or any
        // expression (`alias $h = …`, `alias ($prefix .. "x") = …`).
        // Evaluated at runtime to a string. See ast.rs StmtKind::Alias.
        let name = self.parse_command_expr()?;

        // `alias name` — query a single alias
        if self.at_end_of_statement() {
            return Ok(StmtKind::Alias {
                name: Some(name),
                command: None,
            });
        }

        // `alias name = <expr>` — define alias. The value is any
        // expression; coerced to string at runtime.
        self.expect(&Token::Assign)?;
        let command = self.parse_expression()?;
        self.expect_end_of_statement()?;
        Ok(StmtKind::Alias {
            name: Some(name),
            command: Some(command),
        })
    }

    /// Parse the verb/name position used by `send`, `emit`, `address`-body
    /// commands, and `alias` names. Accepts:
    /// - bare identifier `verb` and dotted identifier `service.verb`
    ///   (parsed into a single `Expr::StringLiteral`, no runtime overhead
    ///   beyond the eval dispatch);
    /// - any expression starting with `$var`, `(expr)`, interpolated
    ///   string, or command-substitution.
    ///
    /// Plain `Token::String` keeps the dotted-identifier shape so existing
    /// `send target a.b key=val` syntax is unchanged; the key=val lookahead
    /// in `parse_send_args` only fires after this returns.
    fn parse_command_expr(&mut self) -> MixResult<Expr> {
        match self.peek() {
            Token::Variable(_)
            | Token::LParen
            | Token::InterpString(_)
            | Token::HeredocString(_)
            | Token::CommandSub(_) => self.parse_expression(),
            Token::String(_) => {
                let mut command = self.expect_identifier()?;
                while self.peek() == &Token::Dot {
                    if self.pos + 1 < self.tokens.len()
                        && let Token::String(_) = &self.tokens[self.pos + 1].token
                    {
                        self.advance(); // skip '.'
                        let part = self.expect_identifier()?;
                        command = format!("{}.{}", command, part);
                        continue;
                    }
                    break;
                }
                Ok(Expr::StringLiteral(command))
            }
            _ => {
                let span = self.peek_span();
                let tok = self.peek().clone();
                Err(MixError::ParseError {
                    msg: format!("expected identifier or expression, got {:?}", tok),
                    span,
                })
            }
        }
    }

    /// Parse send args: tokens until end of line.
    /// Handles `key=value` pairs and positional expressions.
    /// Returns (command_expr, args_vec).
    /// Command supports dotted literal forms (`edit.open`, `ui.set`) and
    /// runtime expressions (`$cmd`, `$prefix .. ".verb"`) via
    /// `parse_command_expr`.
    fn parse_send_args(&mut self) -> MixResult<(Expr, Vec<(String, Expr)>)> {
        let command = self.parse_command_expr()?;
        let mut args = Vec::new();

        // Parse args until the historical send terminator set plus `;`.
        // `next`, `done`, `when`, and `otherwise` deliberately remain valid
        // keyword argument names here even though they terminate other blocks.
        loop {
            match self.peek() {
                Token::Newline
                | Token::Semicolon
                | Token::Eof
                | Token::End
                | Token::Else
                | Token::Elif
                | Token::Catch
                | Token::Finally
                | Token::Pipe
                | Token::AndAnd
                | Token::OrOr => break,
                _ => {}
            }

            // Check for key=value pattern: a bareword OR keyword lexeme
            // followed by Assign (`send maild mailbox.move to=$dest` must
            // not choke on the keyword `to`). Block terminators (end/else/
            // catch) break the loop above before this check, so they can
            // never be read as kwarg keys.
            let next_is_assign = self.pos + 1 < self.tokens.len()
                && self.tokens[self.pos + 1].token == Token::Assign;
            let is_named = next_is_assign
                && (matches!(self.peek(), Token::String(_))
                    || keyword_lexeme(self.peek()).is_some());

            if is_named {
                let key = self.expect_name_or_keyword()?;
                self.advance(); // skip '='
                let value = self.parse_expression()?;
                args.push((key, value));
            } else {
                // Positional argument
                let value = self.parse_expression()?;
                args.push((String::new(), value));
            }
        }

        Ok((command, args))
    }

    /// Parse: `send "target" command arg1 key=value ...`
    /// Parse a `send`/`address`/`emit` TARGET.
    ///
    /// A **dotted** bare identifier (`noded.delta.bus`, `maild.gamma.bus`) is
    /// a literal Bus address — so a mesh address is written directly,
    /// without building a `"noded." .. $n .. ".bus"` string. This is a
    /// pure addition: `bareword.bareword` was previously parsed as
    /// field-access and was a *runtime* error ("cannot access field … on
    /// string"), so nothing valid changes meaning.
    ///
    /// Everything else parses as an expression exactly as before — a
    /// single bareword (`noded`), `$var`, `(expr)`, a bareword **call**
    /// (`env("DEST")`), an index, or a concat. Only `String` immediately
    /// followed by `Dot` takes the literal-address path.
    fn parse_send_target(&mut self) -> MixResult<Expr> {
        if let Token::String(_) = self.peek()
            && matches!(
                self.tokens.get(self.pos + 1).map(|t| &t.token),
                Some(Token::Dot)
            )
        {
            return self.parse_command_expr();
        }
        self.parse_expression()
    }

    fn parse_send(&mut self) -> MixResult<StmtKind> {
        self.advance(); // skip 'send'
        let target = self.parse_send_target()?;
        let (command, args) = self.parse_send_args()?;
        self.expect_end_of_statement()?;
        Ok(StmtKind::Send {
            target,
            command,
            args,
        })
    }

    /// Parse: `address "target" ... end`
    fn parse_address(&mut self) -> MixResult<StmtKind> {
        self.advance(); // skip 'address'
        let target = self.parse_send_target()?;
        self.expect_end_of_statement()?;

        // Parse body: each line is `command arg1 key=value ...`
        // desugared into Stmt::Send with the address target
        let mut body = Vec::new();
        loop {
            self.skip_statement_separators();
            if self.is_at_end() || self.check(&Token::End) {
                break;
            }
            // Each line in address block is an implicit send. The
            // dynamic-command lift makes `$cmd …` parse here, which in
            // turn makes the older "I forgot the body is implicit" typo
            // (`send $cmd …`) a likelier confusion — catch it with a
            // targeted error rather than the generic "unexpected token".
            let line = self.current_line();
            if matches!(self.peek(), Token::Send) {
                let span = self.peek_span();
                return Err(MixError::ParseError {
                    msg: "address blocks use implicit send: drop the `send` keyword \
                          (e.g. `verb name=value` or `$cmd name=value`)"
                        .to_string(),
                    span,
                });
            }
            let (command, args) = self.parse_send_args()?;
            body.push(Stmt::new(
                StmtKind::Send {
                    target: target.clone(),
                    command,
                    args,
                },
                line,
            ));
            self.expect_end_of_statement()?;
        }
        self.expect(&Token::End)?;
        self.expect_end_of_statement()?;
        Ok(StmtKind::Address { target, body })
    }

    /// Parse: `emit "target" command arg1 key=value ...`
    fn parse_emit(&mut self) -> MixResult<StmtKind> {
        self.advance(); // skip 'emit'
        let target = self.parse_send_target()?;
        let (command, args) = self.parse_send_args()?;
        self.expect_end_of_statement()?;
        Ok(StmtKind::Emit {
            target,
            command,
            args,
        })
    }

    /// Parse: `on command.name [async] ... done`
    ///
    /// Registers a handler body to fire when a matching Bus message arrives.
    /// Command name parsing mirrors `parse_send_args` — a bare identifier
    /// optionally followed by dotted parts (e.g. `topic.delivery`). No args
    /// or filter expressions — filtering happens inside the handler body.
    ///
    /// SPEC 18 Phase 2: an optional `async` modifier after the command name
    /// marks the handler as Class C (the dispatch may yield to other
    /// invocations across `send.await`). `async` is parsed as a contextual
    /// identifier — it is NOT a global reserved word, so existing
    /// scripts/vars named `async` keep working everywhere else.
    fn parse_on(&mut self) -> MixResult<StmtKind> {
        self.advance(); // skip 'on'

        // Dotted command name, same shape as send/emit command parsing.
        let mut command = self.expect_identifier()?;
        while self.peek() == &Token::Dot {
            if self.pos + 1 < self.tokens.len()
                && let Token::String(_) = &self.tokens[self.pos + 1].token
            {
                self.advance(); // skip '.'
                let part = self.expect_identifier()?;
                command = format!("{}.{}", command, part);
                continue;
            }
            break;
        }

        // Optional `async` modifier (contextual identifier, not a keyword).
        // SPEC 18 §10.3 / Ch04: per-handler classification of yield behaviour.
        // Mix represents barewords as `Token::String(_)`; matching by string
        // value here keeps `async` a contextual modifier rather than a global
        // reserved word so existing scripts/vars named `async` are unaffected.
        let is_async = matches!(self.peek(), Token::String(name) if name == "async");
        if is_async {
            self.advance();
        }

        self.skip_statement_separators();
        let body = self.parse_block(&[Token::Done, Token::End])?;
        self.expect_end_or_legacy("on", &Token::Done)?;
        self.expect_end_of_statement()?;

        Ok(StmtKind::On {
            command,
            is_async,
            body,
        })
    }

    /// Parse send in expression position: `$result = send "target" command ...`
    fn parse_send_expr(&mut self) -> MixResult<Expr> {
        self.advance(); // skip 'send'
        let target = self.parse_send_target()?;
        let (command, args) = self.parse_send_args()?;
        Ok(Expr::Send {
            target: Box::new(target),
            command: Box::new(command),
            args,
        })
    }

    /// Parse: `source <expr>` or `source <bareword-path>`
    ///
    /// Bareword path mode triggers when the first token after `source` is
    /// `.`, `..`, `/`, or `~` — none of which can legitimately start a Mix
    /// expression. In that mode we read the raw source bytes from the
    /// token's offset to the next whitespace / newline / semicolon / comment
    /// marker,
    /// so `source ./foo.mix`, `source ../bar.mix`, `source /etc/x.mix`,
    /// and `source ~/.mixrc` all work without quoting. A leading `~` or
    /// `~/` expands to `$HOME` at runtime via `StringPart::EnvVar` — same
    /// primitive `lex_double_string` already uses for `"~/foo"`. The
    /// remaining cases (`source "..."`, `source $var`,
    /// `source env(...) .. "/foo"`) keep the existing expression path.
    fn parse_source(&mut self) -> MixResult<StmtKind> {
        self.advance(); // skip 'source'
        let path = match self.peek() {
            Token::Dot | Token::DotDot | Token::Slash | Token::Tilde => {
                self.parse_bareword_path()?
            }
            _ => self.parse_expression()?,
        };
        self.expect_end_of_statement()?;
        Ok(StmtKind::Source { path })
    }

    /// `include "lib.mix"` — same path-grammar as `source` (bareword or
    /// expression); semantics (script-relative resolution + load-once)
    /// live in the evaluator's `exec_include`.
    fn parse_include(&mut self) -> MixResult<StmtKind> {
        self.advance(); // skip 'include'
        let path = match self.peek() {
            Token::Dot | Token::DotDot | Token::Slash | Token::Tilde => {
                self.parse_bareword_path()?
            }
            _ => self.parse_expression()?,
        };
        self.expect_end_of_statement()?;
        Ok(StmtKind::Include { path })
    }

    /// Read a bareword path from the raw source. Stops at the first
    /// whitespace, newline, or `;` outside the path. Returns either
    /// a plain `StringLiteral` or an `InterpolatedString` with a leading
    /// `EnvVar("HOME")` part if the path starts with `~` or `~/`.
    fn parse_bareword_path(&mut self) -> MixResult<Expr> {
        let start = self
            .tokens
            .get(self.pos)
            .map(|t| t.offset)
            .unwrap_or(self.source.len());
        let mut end = start;
        while end < self.source.len() {
            let c = self.source[end];
            if c == '\n' || c == ';' || c.is_whitespace() {
                break;
            }
            // `#` and `--` are NOT comment markers inside a bareword
            // path — they're legal POSIX filename chars and shells only
            // treat `#` as a comment at word-start (which here means the
            // scanner would not have entered the loop at all). A
            // trailing `# comment` works because the preceding
            // whitespace breaks us out first; the lexer then eats the
            // `# ...\n` and emits Newline, so `expect_end_of_statement`
            // is satisfied. NOTE: non-ASCII path chars (e.g. `café.mix`)
            // still lex-fail at `lexer.rs:277` before reaching this
            // function — bareword paths are ASCII-only; quote
            // non-ASCII paths as `source "..."`.
            end += 1;
        }
        let path_text: String = self.source[start..end].iter().collect();
        // Advance the token cursor past every token whose offset falls
        // inside the consumed source range. The path was assembled from
        // the underlying chars, so individual tokens (Dot, Slash, Tilde,
        // bare-ident-as-String, etc.) are now subsumed.
        while self.pos < self.tokens.len() && self.tokens[self.pos].offset < end {
            self.pos += 1;
        }
        if path_text.is_empty() {
            return Err(MixError::ParseError {
                msg: "source: empty bareword path".to_string(),
                span: self.peek_span(),
            });
        }
        // `~` or `~/...` → leading-HOME expansion. Mid-string `~` stays
        // literal (matches `lex_double_string` semantics; preserves DNS
        // tokens like `~bus` that show up in path-adjacent code).
        if path_text == "~" {
            Ok(Expr::InterpolatedString(vec![
                crate::token::StringPart::EnvVar("HOME".to_string()),
            ]))
        } else if let Some(rest) = path_text.strip_prefix("~/") {
            Ok(Expr::InterpolatedString(vec![
                crate::token::StringPart::EnvVar("HOME".to_string()),
                crate::token::StringPart::Literal(format!("/{}", rest)),
            ]))
        } else {
            Ok(Expr::StringLiteral(path_text))
        }
    }

    /// Parse: `sh <expr>` (statement form — streams to stdout)
    fn parse_sh(&mut self) -> MixResult<StmtKind> {
        self.advance(); // skip 'sh'
        let command = self.parse_expression()?;
        self.expect_end_of_statement()?;
        Ok(StmtKind::Sh { command })
    }

    fn parse_block(&mut self, terminators: &[Token]) -> MixResult<Vec<Stmt>> {
        let mut stmts = Vec::new();
        loop {
            self.skip_statement_separators();
            if self.is_at_end() {
                break;
            }
            // Check if current token is a terminator
            let current = self.peek();
            for term in terminators {
                if std::mem::discriminant(current) == std::mem::discriminant(term) {
                    return Ok(stmts);
                }
            }
            stmts.push(self.parse_statement()?);
        }
        Ok(stmts)
    }

    // --- Expression parsing (Pratt) ---

    /// Depth-guarded: every nesting construct (`(`, `[`, `{`, call
    /// args, postfix index) recurses back here from `parse_primary` /
    /// `parse_postfix`, so this is the choke point that turns deeply
    /// nested input into a clean error instead of a stack overflow.
    fn parse_expression(&mut self) -> MixResult<Expr> {
        self.enter_nested()?;
        let result = self.parse_expression_inner();
        self.depth -= 1;
        result
    }

    fn parse_expression_inner(&mut self) -> MixResult<Expr> {
        let left = self.parse_unary()?;
        let cond = self.parse_binary_rhs(left, 0)?;
        self.parse_ternary_suffix(cond)
    }

    /// Consume a trailing ternary `? a : b` on an already-parsed condition
    /// expression, returning `Expr::Ternary`; otherwise return `cond`
    /// unchanged. Lowest precedence — the caller has already consumed every
    /// binary operator into `cond` — and right-associative: the branches
    /// recurse through `parse_expression`, so `a ? b : c ? d : e` groups as
    /// `a ? b : (c ? d : e)`. The `then` branch stops at the `:` because
    /// `:` is not a binary operator. Single-line at statement top level (a
    /// newline before `?` ends the statement first — use an `if` expression
    /// for a multi-line conditional value).
    ///
    /// Shared so the ternary suffix is applied uniformly: both the normal
    /// expression path (`parse_expression_inner`) AND the variable-led
    /// expression-statement paths in `parse_variable_stmt`, which build their
    /// head expression by hand and call `parse_binary_rhs` directly, must run
    /// it — otherwise `$c ? a : b` as a bare statement would not parse while
    /// `true ? a : b` and `$x = $c ? a : b` do.
    fn parse_ternary_suffix(&mut self, cond: Expr) -> MixResult<Expr> {
        if self.check(&Token::Question) {
            self.advance(); // consume '?'
            let then_branch = self.parse_expression()?;
            self.expect(&Token::Colon)?;
            let else_branch = self.parse_expression()?;
            return Ok(Expr::Ternary {
                cond: Box::new(cond),
                then_branch: Box::new(then_branch),
                else_branch: Box::new(else_branch),
            });
        }
        Ok(cond)
    }

    fn parse_binary_rhs(&mut self, mut left: Expr, min_prec: u8) -> MixResult<Expr> {
        loop {
            let (op, prec) = match self.peek() {
                Token::Or => (BinOp::Or, 1),
                Token::And => (BinOp::And, 2),
                Token::Eq => (BinOp::Eq, 3),
                Token::NotEq => (BinOp::NotEq, 3),
                Token::Gt => (BinOp::Gt, 3),
                Token::Lt => (BinOp::Lt, 3),
                Token::GtEq => (BinOp::GtEq, 3),
                Token::LtEq => (BinOp::LtEq, 3),
                Token::StrEq => (BinOp::StrEq, 3),
                Token::StrNe => (BinOp::StrNe, 3),
                Token::NilCoalesce => (BinOp::NilCoalesce, 4),
                Token::DotDot => (BinOp::Concat, 5),
                Token::Plus => (BinOp::Add, 6),
                Token::Minus => (BinOp::Sub, 6),
                Token::Star => (BinOp::Mul, 7),
                Token::Slash => (BinOp::Div, 7),
                Token::Percent => (BinOp::Mod, 7),
                Token::Power => (BinOp::Power, 8),
                _ => break,
            };

            if prec < min_prec {
                break;
            }

            let operator_span = self.peek_span();
            self.advance(); // consume operator
            if matches!(op, BinOp::Concat) {
                // `..` is the one expression operator that explicitly
                // continues across physical lines. Keep this parser-local:
                // strict-data shares the lexer but has its own grammar, and
                // source/include consume a leading DotDot as a bareword path.
                self.skip_newlines();
                if self.is_at_end() {
                    return Err(MixError::IncompleteInput {
                        msg: "expected expression after `..`".to_string(),
                        span: operator_span,
                    });
                }
            }
            let right = self.parse_unary()?;
            // `prec + 1` keeps same-precedence chains iterative
            // (left-assoc), so this recursion is bounded by the number
            // of precedence levels — the guard is belt-and-braces.
            self.enter_nested()?;
            let right = self.parse_binary_rhs(right, prec + 1);
            self.depth -= 1;
            let right = right?;

            left = Expr::BinaryOp {
                left: Box::new(left),
                op,
                right: Box::new(right),
            };
        }
        Ok(left)
    }

    fn parse_unary(&mut self) -> MixResult<Expr> {
        let op = match self.peek() {
            Token::Minus => UnaryOp::Neg,
            Token::Not | Token::Bang => UnaryOp::Not,
            _ => return self.parse_primary(),
        };
        self.advance();
        // Depth-guard the self-recursion: a long `!!!…` / `---…` run
        // never passes back through parse_expression.
        self.enter_nested()?;
        let operand = self.parse_unary();
        self.depth -= 1;
        let operand = operand?;
        Ok(Expr::UnaryOp {
            op,
            operand: Box::new(operand),
        })
    }

    fn parse_primary(&mut self) -> MixResult<Expr> {
        let expr = match self.peek().clone() {
            Token::Number(n) => {
                self.advance();
                Expr::NumberLiteral(n)
            }
            Token::String(s) => {
                let has_escaped_quote = self.current_string_has_escaped_quote();
                self.advance();
                // Check if this is a function call: identifier followed by (
                if self.check(&Token::LParen) {
                    return self.parse_function_call(s);
                }
                if has_escaped_quote {
                    Expr::EscapedQuoteStringLiteral(s)
                } else {
                    Expr::StringLiteral(s)
                }
            }
            Token::InterpString(parts) => {
                self.advance();
                Expr::InterpolatedString(parts)
            }
            Token::HeredocString(parts) => {
                self.advance();
                Expr::Heredoc(parts)
            }
            Token::CommandSub(cmd) => {
                self.advance();
                Expr::CommandSub(cmd)
            }
            Token::Variable(name) => {
                self.advance();
                Expr::Variable(name)
            }
            Token::True => {
                self.advance();
                Expr::BoolLiteral(true)
            }
            Token::False => {
                self.advance();
                Expr::BoolLiteral(false)
            }
            Token::Nil => {
                self.advance();
                Expr::NilLiteral
            }
            Token::LParen => {
                self.advance();
                let expr = self.parse_expression()?;
                self.expect(&Token::RParen)?;
                expr
            }
            Token::LBracket => self.parse_list_literal()?,
            Token::LBrace => self.parse_map_literal()?,
            Token::Send => {
                return self.parse_send_expr();
            }
            Token::Sh => {
                self.advance();
                let command = self.parse_expression()?;
                return Ok(Expr::Sh(Box::new(command)));
            }
            Token::Function => {
                // Anonymous function literal. Statement-position
                // `function name(...)` is dispatched earlier (parser.rs:206)
                // and never reaches expression parsing; reaching here means
                // the next token after `function` is `(` (no identifier).
                self.advance(); // skip 'function'
                let (params, body) =
                    self.parse_function_tail(/*expression_position=*/ true)?;
                Expr::FunctionLiteral {
                    params,
                    body: Box::new(body),
                }
            }
            // `if … then … else … end` in expression position. A leading
            // `if` at statement start is intercepted by parse_statement
            // (→ StmtKind::If), so reaching here means the `if` is nested
            // inside an expression (RHS of `=`, a call arg, a `..` concat,
            // inside parens, etc.). Shares the statement form's body parser.
            Token::If => {
                let (condition, then_body, else_ifs, else_body) = self.parse_if_parts()?;
                Expr::If(Box::new(IfExpr {
                    condition,
                    then_body,
                    else_ifs,
                    else_body,
                }))
            }
            Token::Semicolon => {
                return Err(MixError::ParseError {
                    msg: "unexpected ';' inside expression; ';' separates Mix statements only"
                        .to_string(),
                    span: self.peek_span(),
                });
            }
            _ => {
                let span = self.peek_span();
                return Err(MixError::ParseError {
                    msg: format!("unexpected token {:?}", self.peek()),
                    span,
                });
            }
        };

        self.parse_postfix(expr)
    }

    fn parse_postfix(&mut self, mut expr: Expr) -> MixResult<Expr> {
        loop {
            match self.peek() {
                Token::Dot => {
                    self.advance();
                    // Keywords are unambiguous after a field-access dot:
                    // $m.to, $r.label ('fn' excepted — see keyword_lexeme).
                    let field = self.expect_field_name()?;
                    // Check if it's a method call: expr.method(args)
                    if self.check(&Token::LParen) {
                        self.advance();
                        let mut args = Vec::new();
                        if !self.check(&Token::RParen) {
                            args.push(self.parse_expression()?);
                            while self.match_token(&Token::Comma) {
                                args.push(self.parse_expression()?);
                            }
                        }
                        self.expect(&Token::RParen)?;
                        if method_desugars_to_ufcs(&field) {
                            // Legacy UFCS desugar: builtins, HOFs, and the
                            // evaluator's inline special forms (incl. the
                            // in-place mutating push/pop/shift trio, which
                            // needs the RAW object expression to mutate the
                            // scope slot). Bit-identical dispatch and hot
                            // path to pre-MethodCall behaviour.
                            args.insert(0, expr);
                            expr = Expr::FunctionCall { name: field, args };
                        } else {
                            // Dynamic-name method call: extensions / user
                            // functions still dispatch UFCS-first at eval
                            // time; a map member holding a Function is the
                            // fallback (require() exports).
                            expr = Expr::MethodCall {
                                object: Box::new(expr),
                                field,
                                args,
                            };
                        }
                    } else {
                        expr = Expr::FieldAccess {
                            object: Box::new(expr),
                            field,
                        };
                    }
                }
                Token::LBracket => {
                    self.advance();
                    let index = self.parse_expression()?;
                    self.expect(&Token::RBracket)?;
                    expr = Expr::Index {
                        object: Box::new(expr),
                        index: Box::new(index),
                    };
                }
                Token::LParen => {
                    // Call a function-valued expression: `$f(5)`,
                    // `get_fn()(arg)`, `(function($x) = $x+1)(4)`.
                    // Bareword-name calls like `sort_by(...)` never
                    // reach here — they dispatch through the
                    // String+LParen branch in `parse_primary` and
                    // build `Expr::FunctionCall` directly.
                    self.advance();
                    let mut args = Vec::new();
                    if !self.check(&Token::RParen) {
                        args.push(self.parse_expression()?);
                        while self.match_token(&Token::Comma) {
                            args.push(self.parse_expression()?);
                        }
                    }
                    self.expect(&Token::RParen)?;
                    expr = Expr::ValueCall {
                        callee: Box::new(expr),
                        args,
                    };
                }
                _ => break,
            }
        }
        Ok(expr)
    }

    fn parse_function_call(&mut self, name: String) -> MixResult<Expr> {
        self.expect(&Token::LParen)?;
        let mut args = Vec::new();
        if !self.check(&Token::RParen) {
            args.push(self.parse_expression()?);
            while self.match_token(&Token::Comma) {
                args.push(self.parse_expression()?);
            }
        }
        self.expect(&Token::RParen)?;
        let expr = Expr::FunctionCall { name, args };
        self.parse_postfix(expr)
    }

    fn parse_list_literal(&mut self) -> MixResult<Expr> {
        self.advance(); // skip [
        let mut items = Vec::new();
        if !self.check(&Token::RBracket) {
            items.push(self.parse_expression()?);
            while self.match_token(&Token::Comma) {
                if self.check(&Token::RBracket) {
                    break; // trailing comma
                }
                items.push(self.parse_expression()?);
            }
        }
        self.expect(&Token::RBracket)?;
        Ok(Expr::ListLiteral(items))
    }

    fn parse_map_literal(&mut self) -> MixResult<Expr> {
        self.advance(); // skip {
        let mut entries = Vec::new();
        self.skip_newlines();
        if !self.check(&Token::RBrace) {
            loop {
                self.skip_newlines();
                let key = match self.peek().clone() {
                    Token::String(s) => {
                        self.advance();
                        s
                    }
                    // A keyword is unambiguous in key position: {label: 1},
                    // {to: "x"}. (`fn` excepted — see keyword_lexeme.)
                    _ => self.expect_name_or_keyword()?,
                };
                self.expect(&Token::Colon)?;
                let value = self.parse_expression()?;
                entries.push((key, value));
                self.skip_newlines();
                if !self.match_token(&Token::Comma) {
                    break;
                }
            }
        }
        self.skip_newlines();
        self.expect(&Token::RBrace)?;
        Ok(Expr::MapLiteral(entries))
    }

    // --- Strict-data parsing ---
    //
    // Parallel codepath to executable Mix that constructs a `Value`
    // tree from the literal-data subset only. Anything that could
    // run code (calls, sends, interpolation, variable references,
    // arithmetic, control flow) is rejected with
    // `MixError::StrictDataViolation` — see
    // `_doc/2026-05-04-substrate-mix-data-formats.md`.

    /// Parse a complete `.spec.mix` / `.conf.mix` / etc. source.
    ///
    /// Top-level shape is one of:
    /// * `{ ... }` — explicit map literal (matches `Value::write_mix`
    ///   round-trip output)
    /// * `[ ... ]` — top-level list
    /// * implicit map body — `key: value` pairs at column zero,
    ///   newline- or comma-separated
    /// * empty input → `Value::Map(empty)`
    pub fn parse_data(&mut self) -> MixResult<Value> {
        self.skip_newlines();
        if matches!(self.peek(), Token::Semicolon) {
            return Err(self.strict_data_semicolon_error());
        }
        let value = if self.is_at_end() {
            Value::map(IndexMap::new())
        } else {
            match self.peek() {
                Token::LBrace => self.parse_data_map()?,
                Token::LBracket => self.parse_data_list()?,
                _ => self.parse_top_level_map_body()?,
            }
        };
        self.skip_newlines();
        if matches!(self.peek(), Token::Semicolon) {
            return Err(self.strict_data_semicolon_error());
        }
        if !self.is_at_end() {
            let line = self.current_line();
            return Err(MixError::StrictDataViolation {
                construct: format!("trailing content (token {:?})", self.peek()),
                line,
                hint: "a data file holds a single top-level value (map body, [ list ], or { map })"
                    .into(),
            });
        }
        Ok(value)
    }

    fn strict_data_semicolon_error(&self) -> MixError {
        MixError::StrictDataViolation {
            construct: "semicolon statement separator `;`".into(),
            line: self.current_line(),
            hint: "strict-data uses newline or `,` separators; `;` is executable-Mix syntax".into(),
        }
    }

    /// Parse a single data value. Exhaustive over the literal-data
    /// production set; everything else returns
    /// `StrictDataViolation`. Adding a new `Token` variant must
    /// extend this match — that's the structural guarantee strict
    /// mode relies on.
    fn parse_data_value(&mut self) -> MixResult<Value> {
        // Depth-guarded: `[`/`{` recurse back here via parse_data_list /
        // parse_data_map, and daemons feed untrusted files through
        // from_conf_mix_str / parse_data_file.
        self.enter_nested()?;
        let result = self.parse_data_value_inner();
        self.depth -= 1;
        result
    }

    fn parse_data_value_inner(&mut self) -> MixResult<Value> {
        let line = self.current_line();
        // Allow a single leading `-` on a number literal so that
        // `-42` and `-3.14` round-trip. Anything else after `-` is
        // arithmetic and gets rejected.
        if matches!(self.peek(), Token::Minus) {
            self.advance();
            match self.peek().clone() {
                Token::Number(n) => {
                    self.advance();
                    return Ok(Value::Number(-n));
                }
                _ => {
                    return Err(MixError::StrictDataViolation {
                        construct: "unary minus on non-number".into(),
                        line,
                        hint: "negation is only valid on a number literal (e.g. -42)".into(),
                    });
                }
            }
        }
        match self.peek().clone() {
            Token::Number(n) => {
                self.advance();
                Ok(Value::Number(n))
            }
            Token::String(s) => {
                self.advance();
                if matches!(self.peek(), Token::LParen) {
                    return Err(MixError::StrictDataViolation {
                        construct: format!("function call `{}(...)`", s),
                        line,
                        hint: "data files cannot invoke functions; use a literal value".into(),
                    });
                }
                Ok(Value::String(s))
            }
            Token::True => {
                self.advance();
                Ok(Value::Bool(true))
            }
            Token::False => {
                self.advance();
                Ok(Value::Bool(false))
            }
            Token::Nil => {
                self.advance();
                Ok(Value::Nil)
            }
            Token::LBracket => self.parse_data_list(),
            Token::LBrace => self.parse_data_map(),
            Token::Semicolon => Err(self.strict_data_semicolon_error()),

            // Rejected productions — each gets a specific hint so the
            // author knows what to swap in.
            Token::HeredocString(parts) => {
                let mut literal = String::new();
                for part in parts {
                    let StringPart::Literal(s) = part else {
                        return Err(MixError::StrictDataViolation {
                            construct: "string interpolation".into(),
                            line,
                            hint: "data files use plain strings; precompute the value".into(),
                        });
                    };
                    literal.push_str(&s);
                }
                self.advance();
                Ok(Value::String(literal))
            }
            Token::InterpString(_) => Err(MixError::StrictDataViolation {
                construct: "string interpolation".into(),
                line,
                hint: "data files use plain strings; precompute the value".into(),
            }),
            Token::Variable(name) => Err(MixError::StrictDataViolation {
                construct: format!("variable reference `${}`", name),
                line,
                hint: "data files have no variable scope; inline the value".into(),
            }),
            Token::CommandSub(_) => Err(MixError::StrictDataViolation {
                construct: "command substitution `$(...)`".into(),
                line,
                hint: "data files cannot run shell commands".into(),
            }),
            Token::Send => Err(MixError::StrictDataViolation {
                construct: "send expression".into(),
                line,
                hint: "data files cannot perform Bus IPC".into(),
            }),
            Token::Sh => Err(MixError::StrictDataViolation {
                construct: "sh expression".into(),
                line,
                hint: "data files cannot run shell commands".into(),
            }),
            Token::Function => Err(MixError::StrictDataViolation {
                construct: "function literal".into(),
                line,
                hint: "data files contain values, not behaviour".into(),
            }),
            Token::LParen => Err(MixError::StrictDataViolation {
                construct: "parenthesised expression".into(),
                line,
                hint: "data files have no expression precedence to override".into(),
            }),
            other => Err(MixError::StrictDataViolation {
                construct: format!("unexpected token {:?}", other),
                line,
                hint: "data files only allow scalars (number, string, true, false, nil), [ list ], { map }".into(),
            }),
        }
    }

    /// `[ value, value, ... ]` → `Value::List`. Trailing comma and
    /// newlines between entries are accepted.
    fn parse_data_list(&mut self) -> MixResult<Value> {
        self.advance(); // [
        let mut items = Vec::new();
        self.skip_newlines();
        if !self.check(&Token::RBracket) {
            items.push(self.parse_data_value()?);
            loop {
                self.skip_newlines();
                if matches!(self.peek(), Token::Semicolon) {
                    return Err(self.strict_data_semicolon_error());
                }
                if !self.match_token(&Token::Comma) {
                    // Same missing-separator diagnostic as parse_data_map:
                    // anchor at the item that lacks its comma, not at
                    // whatever token follows it.
                    if !self.check(&Token::RBracket) && !self.is_at_end() {
                        return Err(MixError::StrictDataViolation {
                            construct: format!(
                                "missing `,` after this list item (next token: {:?})",
                                self.peek()
                            ),
                            line: self.previous_line(),
                            hint: "separate `[ ]` list items with commas; a trailing comma before `]` is fine".into(),
                        });
                    }
                    break;
                }
                self.skip_newlines();
                if self.check(&Token::RBracket) {
                    break; // trailing comma
                }
                items.push(self.parse_data_value()?);
            }
        }
        self.skip_newlines();
        self.expect(&Token::RBracket)?;
        Ok(Value::list(items))
    }

    /// `{ key: value, key: value }` → `Value::Map`. Keys are
    /// identifier-shaped barewords or quoted strings.
    ///
    /// The lexer suppresses `Newline` tokens inside `{ }`, so multi-
    /// line map literals reach the parser as adjacent key-value pairs.
    /// To avoid silently accepting `{ a: 1 b: 2 }` as two fields, an
    /// explicit `,` is required between entries (mirrors
    /// `parse_data_list`). A trailing comma before `}` is allowed.
    /// Top-level map body (no enclosing braces) keeps newline-as-
    /// separator since newline tokens survive there.
    fn parse_data_map(&mut self) -> MixResult<Value> {
        self.advance(); // {
        let mut entries: IndexMap<String, Value> = IndexMap::new();
        if !self.check(&Token::RBrace) {
            loop {
                let key_line = self.current_line();
                let key = self.parse_data_key()?;
                self.expect(&Token::Colon)?;
                let value = self.parse_data_value()?;
                Self::insert_unique(&mut entries, key, value, key_line)?;
                if matches!(self.peek(), Token::Semicolon) {
                    return Err(self.strict_data_semicolon_error());
                }
                if !self.match_token(&Token::Comma) {
                    // No comma and not the closing brace: the entry that
                    // just ended is missing its separator. Say so, anchored
                    // at that entry's END — the generic expect(RBrace) below
                    // would name the NEXT entry's position and describe the
                    // parser's state ("expected RBrace, got String") rather
                    // than the author's mistake.
                    if !self.check(&Token::RBrace) && !self.is_at_end() {
                        return Err(MixError::StrictDataViolation {
                            construct: format!(
                                "missing `,` after this map entry (next token: {:?})",
                                self.peek()
                            ),
                            line: self.previous_line(),
                            hint: "separate `{ }` map entries with commas; a trailing comma before `}` is fine".into(),
                        });
                    }
                    break;
                }
                if self.check(&Token::RBrace) {
                    break; // trailing comma
                }
            }
        }
        self.expect(&Token::RBrace)?;
        Ok(Value::map(entries))
    }

    /// Top-level map body: `key: value` pairs without enclosing braces,
    /// terminated by EOF.
    fn parse_top_level_map_body(&mut self) -> MixResult<Value> {
        let mut entries: IndexMap<String, Value> = IndexMap::new();
        loop {
            self.skip_newlines();
            if self.is_at_end() {
                break;
            }
            let key_line = self.current_line();
            let key = self.parse_data_key()?;
            self.expect(&Token::Colon)?;
            self.skip_newlines();
            let value = self.parse_data_value()?;
            Self::insert_unique(&mut entries, key, value, key_line)?;
            if matches!(self.peek(), Token::Semicolon) {
                return Err(self.strict_data_semicolon_error());
            }
            let had_comma = self.match_token(&Token::Comma);
            let had_newline = matches!(self.peek(), Token::Newline);
            self.skip_newlines();
            if self.is_at_end() {
                break;
            }
            if !had_comma && !had_newline {
                let line = self.current_line();
                return Err(MixError::StrictDataViolation {
                    construct: format!("missing separator before {:?}", self.peek()),
                    line,
                    hint: "top-level map entries are separated by newline or `,`".into(),
                });
            }
        }
        Ok(Value::map(entries))
    }

    /// Insert into a map, returning `StrictDataViolation` on a
    /// duplicate key. Strict-data files have one source of truth per
    /// key — silent last-wins would mask copy-paste bugs in `.spec.mix`
    /// / `.conf.mix` files.
    fn insert_unique(
        entries: &mut IndexMap<String, Value>,
        key: String,
        value: Value,
        line: usize,
    ) -> MixResult<()> {
        if entries.contains_key(&key) {
            return Err(MixError::StrictDataViolation {
                construct: format!("duplicate map key `{}`", key),
                line,
                hint: "data files have one source of truth per key; remove or rename one".into(),
            });
        }
        entries.insert(key, value);
        Ok(())
    }

    /// Map keys: bareword identifiers (lexed as `Token::String`) or
    /// quoted strings. Anything else is a strict-data violation.
    fn parse_data_key(&mut self) -> MixResult<String> {
        let line = self.current_line();
        match self.peek().clone() {
            Token::Semicolon => Err(self.strict_data_semicolon_error()),
            Token::String(s) => {
                self.advance();
                Ok(s)
            }
            // A keyword lexeme is a legitimate data key — a `.conf.mix`
            // key named `to`/`in`/`on`/`label` must not need quoting.
            // (`fn` excepted — see keyword_lexeme.)
            ref other if keyword_lexeme(other).is_some() => {
                let kw = keyword_lexeme(other).unwrap().to_string();
                self.advance();
                Ok(kw)
            }
            other => Err(MixError::StrictDataViolation {
                construct: format!("non-identifier map key {:?}", other),
                line,
                hint: "map keys must be bareword identifiers or quoted strings".into(),
            }),
        }
    }
}

/// Parse-time UFCS decision for `expr.name(args)` (see
/// `parse_postfix`'s Dot+LParen arm). `true` → keep the legacy desugar
/// to `Expr::FunctionCall { name, [expr, args…] }`; `false` → emit
/// `Expr::MethodCall` (name-first dispatch with a map-member-Function
/// fallback at eval time).
///
/// Builtin / HOF / inline-special-form names are a fixed set per
/// binary, and bareword dispatch already gives them precedence over
/// user functions and map members, so deciding at parse time is
/// behaviour-identical to deciding at dispatch time — and keeps the
/// hot UFCS paths (`$s.upper()`, `$list.map($f)`, `$l.push(x)`)
/// bit-for-bit unchanged, including `push`/`pop`/`shift`'s in-place
/// mutation which needs the RAW object expression.
fn method_desugars_to_ufcs(name: &str) -> bool {
    crate::builtins::is_builtin(name)
        || crate::builtins_hof::lookup(name).is_some()
        || crate::evaluator::INLINE_SPECIAL_FORMS.contains(&name)
}
