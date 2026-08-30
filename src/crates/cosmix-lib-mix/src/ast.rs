use crate::token::StringPart;

#[derive(Debug, Clone)]
pub enum Expr {
    NumberLiteral(f64),
    StringLiteral(String),
    /// A double-quoted string whose source spelling contained `\"`.
    /// Evaluates exactly like `StringLiteral`; retained solely so static
    /// analysis can recognise nested-quoting hazards after lexing.
    EscapedQuoteStringLiteral(String),
    BoolLiteral(bool),
    NilLiteral,
    InterpolatedString(Vec<StringPart>),
    /// Heredoc body. Evaluates exactly like `InterpolatedString`; kept
    /// distinct so static analysis can apply heredoc-only checks.
    Heredoc(Vec<StringPart>),
    Variable(String),
    BinaryOp {
        left: Box<Expr>,
        op: BinOp,
        right: Box<Expr>,
    },
    UnaryOp {
        op: UnaryOp,
        operand: Box<Expr>,
    },
    /// C-style ternary conditional expression: `cond ? a : b`.
    ///
    /// Lowest-precedence, right-associative (`a ? b : c ? d : e` groups
    /// as `a ? b : (c ? d : e)`). Short-circuits: only the taken branch
    /// is evaluated. Unlike the Lua-style `cond and a or b` idiom it is
    /// immune to a falsy middle operand — `$ok ? false : x` yields
    /// `false`, not `x`. `cond` truthiness follows `Value::is_truthy`.
    Ternary {
        cond: Box<Expr>,
        then_branch: Box<Expr>,
        else_branch: Box<Expr>,
    },
    /// `if … then … else … end` used in EXPRESSION position (e.g.
    /// `$x = if c then 1 else 2 end`). Statement-position `if` still
    /// parses to `StmtKind::If`; this variant is produced only when an
    /// `if` is reached during expression parsing. The payload is boxed
    /// so the (large) if-shape doesn't inflate every `Expr`. Each branch
    /// is a statement block whose value is its last statement's value
    /// (or nil if empty), mirroring `StmtKind::If`'s execution.
    If(Box<IfExpr>),
    FunctionCall {
        name: String,
        args: Vec<Expr>,
    },
    /// Anonymous function expression: `function ($x) = expr` or
    /// `function ($x) ... end`. Constructed only in expression position;
    /// statement-position `function name(...)` still produces `FunctionDef`.
    FunctionLiteral {
        params: Vec<Param>,
        body: Box<FunctionBody>,
    },
    /// Call a function-valued expression: `$f(5)`, `get_fn()(arg)`, etc.
    /// Distinct from `FunctionCall` (which dispatches by bareword name
    /// through builtins / user functions / extensions).
    ValueCall {
        callee: Box<Expr>,
        args: Vec<Expr>,
    },
    /// Method-syntax call `expr.name(args)` where `name` is NOT a
    /// builtin / HOF / inline special form (those keep the legacy UFCS
    /// desugar to `FunctionCall` at parse time — see
    /// `Parser::parse_postfix`). Dispatch is name-first for
    /// compatibility (extensions, then user functions, both UFCS-style
    /// with the object as first argument; inside an `address` block an
    /// unknown name is still a send). Only the former error path gains
    /// meaning: a map member holding a `Value::Function` is called with
    /// `args` (no implicit object argument) — the `require()` exports
    /// calling convention.
    MethodCall {
        object: Box<Expr>,
        field: String,
        args: Vec<Expr>,
    },
    Index {
        object: Box<Expr>,
        index: Box<Expr>,
    },
    FieldAccess {
        object: Box<Expr>,
        field: String,
    },
    ListLiteral(Vec<Expr>),
    MapLiteral(Vec<(String, Expr)>),
    /// Send expression: `$result = send "target" command key=value`
    ///
    /// `command` is an Expr evaluated at runtime so the verb can be a
    /// variable, string literal, concat, or any expression that coerces
    /// to a string. Bare-identifier and dotted-identifier forms parse to
    /// `Expr::StringLiteral("verb")` / `Expr::StringLiteral("noded.ping")`
    /// — the common literal path stays allocation-light at runtime.
    Send {
        target: Box<Expr>,
        command: Box<Expr>,
        args: Vec<(String, Expr)>,
    },
    /// Sh expression: `$output = sh "command"` (captures stdout)
    Sh(Box<Expr>),
    /// Command substitution: `$(command)`
    CommandSub(String),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BinOp {
    Add,
    Sub,
    Mul,
    Div,
    Mod,
    Power,
    Eq,
    NotEq,
    Gt,
    Lt,
    GtEq,
    LtEq,
    StrEq,
    StrNe,
    And,
    Or,
    Concat,
    NilCoalesce,
    Pipe,
}

#[derive(Debug, Clone)]
pub enum UnaryOp {
    Neg,
    Not,
}

#[derive(Debug, Clone)]
pub struct Param {
    pub name: String,
    pub default: Option<Expr>,
}

/// The `catch` clause of a `try` statement.
#[derive(Debug, Clone)]
pub struct CatchClause {
    /// First binding: the message string (pre-0.29 contract).
    pub var: String,
    /// Optional second binding (0.29.0): the structured error map.
    pub err_var: Option<String>,
    pub body: Vec<Stmt>,
}

#[derive(Debug, Clone)]
pub enum FunctionBody {
    Block(Vec<Stmt>),
    Expression(Expr),
}

/// Payload of an expression-position `if` (`Expr::If`). Shape mirrors
/// `StmtKind::If` exactly so the evaluator can run it through the same
/// block-execution path; kept as a separate boxed struct purely so
/// `Expr` stays small (every `Box<Expr>` allocation pays `size_of::<Expr>`).
#[derive(Debug, Clone)]
pub struct IfExpr {
    pub condition: Expr,
    pub then_body: Vec<Stmt>,
    pub else_ifs: Vec<(Expr, Vec<Stmt>)>,
    pub else_body: Option<Vec<Stmt>>,
}

/// Operator for statement chaining (`&&` / `||`).
#[derive(Debug, Clone)]
pub enum ChainOp {
    /// `&&` — execute right only if `$rc == 0` after left
    And,
    /// `||` — execute right only if `$rc != 0` after left
    Or,
}

/// A component of a PARSE template: either a variable to capture into
/// or a literal delimiter to match and skip.
#[derive(Debug, Clone)]
pub enum ParsePart {
    Variable(String),
    Delimiter(String),
}

/// A statement with source line number for error reporting.
#[derive(Debug, Clone)]
pub struct Stmt {
    pub kind: StmtKind,
    pub line: usize,
}

impl Stmt {
    pub fn new(kind: StmtKind, line: usize) -> Self {
        Stmt { kind, line }
    }
}

/// One accessor in an assignment path (`.field` or `[expr]`).
///
/// `.k` and `["k"]` are deliberately NOT interchangeable here: they are
/// distinct segment kinds so structural comparisons stay honest.
#[derive(Debug, Clone)]
pub enum PathSeg {
    Field(String),
    Index(Expr),
}

#[derive(Debug, Clone)]
pub enum StmtKind {
    Expression(Expr),
    Assignment {
        name: String,
        value: Expr,
    },
    FieldAssignment {
        object: String,
        field: String,
        value: Expr,
    },
    IndexAssignment {
        object: String,
        index: Expr,
        value: Expr,
    },
    /// Nested lvalue: `$m[$u]["k"] = v`, `$m.a.b = v`, `$l[0][1] = v`.
    ///
    /// Only emitted for **two or more** accessors — a single accessor
    /// stays on `FieldAssignment` / `IndexAssignment`, whose evaluator
    /// arms carry hand-tuned for-loop fast paths that must not be
    /// perturbed. All three arms share one path-walk primitive so the
    /// semantics can't drift.
    PathAssignment {
        root: String,
        /// Invariant: `path.len() >= 2`.
        path: Vec<PathSeg>,
        value: Expr,
    },
    If {
        condition: Expr,
        then_body: Vec<Stmt>,
        else_ifs: Vec<(Expr, Vec<Stmt>)>,
        else_body: Option<Vec<Stmt>>,
    },
    For {
        var: String,
        start: Expr,
        end: Expr,
        step: Option<Expr>,
        body: Vec<Stmt>,
        label: Option<String>,
    },
    ForEach {
        var: String,
        index_var: Option<String>,
        iterable: Expr,
        body: Vec<Stmt>,
        label: Option<String>,
    },
    While {
        condition: Expr,
        body: Vec<Stmt>,
        label: Option<String>,
    },
    Loop {
        body: Vec<Stmt>,
        label: Option<String>,
    },
    Break(Option<String>),
    Continue(Option<String>),
    BreakIf(Expr, Option<String>),
    ContinueIf(Expr, Option<String>),
    FunctionDef {
        name: String,
        params: Vec<Param>,
        body: FunctionBody,
    },
    Return(Option<Expr>),
    Select {
        value: Expr,
        cases: Vec<(Expr, Vec<Stmt>)>,
        otherwise: Option<Vec<Stmt>>,
    },
    Print {
        args: Vec<Expr>,
        stderr: bool,
    },
    Parse {
        source: Expr,
        parts: Vec<ParsePart>,
    },
    Die(Expr),
    TryCatch {
        try_body: Vec<Stmt>,
        /// `None` = a `try … finally … end` with no catch (0.30.0): the
        /// error propagates after `finally` runs. `Some` = a catch
        /// clause is present.
        catch: Option<CatchClause>,
        /// Optional `finally` body (0.30.0) — runs on every exit path
        /// (normal, caught error, return/break/continue) except
        /// `exit`/`panic`. At least one of `catch`/`finally` is present.
        finally_body: Option<Vec<Stmt>>,
    },
    Export {
        name: String,
        value: Expr,
    },
    /// Alias define / query / list.
    ///
    /// `name` and `command` are Exprs evaluated at runtime; both bare
    /// identifiers (`alias gw = "ssh gw"`) and dynamic forms
    /// (`alias $h = "ssh " .. $h`) share this shape. Literal identifiers
    /// parse to `Expr::StringLiteral(_)` so the common path is
    /// allocation-light. `name == None` is the list-all form;
    /// `command == None` is the query-one form.
    Alias {
        name: Option<Expr>,
        command: Option<Expr>,
    },
    /// Send statement: `send "target" command key=value`
    ///
    /// `command` is an Expr (see `Expr::Send` doc for rationale).
    Send {
        target: Expr,
        command: Expr,
        args: Vec<(String, Expr)>,
    },
    /// Address block: `address "target" ... end`
    Address {
        target: Expr,
        body: Vec<Stmt>,
    },
    /// Emit (fire-and-forget send): `emit "target" command key=value`
    ///
    /// `command` is an Expr (see `Expr::Send` doc for rationale).
    Emit {
        target: Expr,
        command: Expr,
        args: Vec<(String, Expr)>,
    },
    /// Event handler registration: `on command.name ... done`
    ///
    /// At runtime, executing this statement pushes the handler body into the
    /// evaluator's handler registry keyed by `command`. The body runs later,
    /// once per matching incoming Bus message, with a `$event` local injected
    /// into the handler scope. Handlers are serialized (one at a time, queued
    /// in arrival order) to avoid reentrancy across nested `send` awaits.
    /// Multiple handlers may register for the same command; they fire in
    /// registration order.
    On {
        command: String,
        /// SPEC 18 Phase 2: `on <cmd> async` declares a Class C handler whose
        /// invocations may interleave across `send.await`. Plain handlers
        /// (`is_async = false`) stay Class S — full single-handler atomicity
        /// preserved per SPEC 05 §7.3.
        is_async: bool,
        body: Vec<Stmt>,
    },
    /// Source: `source "file.mix"` — execute file in current scope
    Source {
        path: Expr,
    },
    /// Include: `include "lib.mix"` — like `source`, but resolves the
    /// path relative to the INCLUDING file's directory (CWD fallback)
    /// and loads each file at most once per program run (dedup). Module
    /// semantics, vs `source`'s shell-style CWD-relative run-every-time.
    Include {
        path: Expr,
    },
    /// Sh statement: `sh "command"` — run via /bin/sh, inherit stdio
    Sh {
        command: Expr,
    },
    /// Pipe statement output to an external command: `send noded noded.ping | jq .`
    PipeToExternal {
        stmt: Box<Stmt>,
        command: String,
    },
    /// Chain statements with `&&` or `||`: `send noded noded.ping && print $result`
    Chain {
        left: Box<Stmt>,
        op: ChainOp,
        right: Box<Stmt>,
    },
}

impl StmtKind {
    /// Canonical authored keywords represented by this AST node.
    /// Empty slots avoid allocating on the evaluator hot path while allowing
    /// `try` nodes to expose their optional `catch` and `finally` clauses.
    pub fn stats_keywords(&self) -> [Option<&'static str>; 3] {
        let mut keys = [None, None, None];
        keys[0] = match self {
            StmtKind::If { .. } => Some("if"),
            StmtKind::For { .. } | StmtKind::ForEach { .. } => Some("for"),
            StmtKind::While { .. } => Some("while"),
            StmtKind::Loop { .. } => Some("loop"),
            StmtKind::Break(_) | StmtKind::BreakIf(..) => Some("break"),
            StmtKind::Continue(_) | StmtKind::ContinueIf(..) => Some("continue"),
            StmtKind::FunctionDef { .. } => Some("function"),
            StmtKind::Return(_) => Some("return"),
            StmtKind::Select { .. } => Some("select"),
            StmtKind::Print { stderr: true, .. } => Some("eprint"),
            StmtKind::Print { stderr: false, .. } => Some("print"),
            StmtKind::Parse { .. } => Some("parse"),
            StmtKind::Die(_) => Some("die"),
            StmtKind::TryCatch { .. } => Some("try"),
            StmtKind::Export { .. } => Some("export"),
            StmtKind::Alias { .. } => Some("alias"),
            StmtKind::Send { .. } => Some("send"),
            StmtKind::Address { .. } => Some("address"),
            StmtKind::Emit { .. } => Some("emit"),
            StmtKind::On { .. } => Some("on"),
            StmtKind::Source { .. } => Some("source"),
            StmtKind::Include { .. } => Some("include"),
            StmtKind::Sh { .. } => Some("sh"),
            _ => None,
        };
        if let StmtKind::TryCatch {
            catch,
            finally_body,
            ..
        } = self
        {
            keys[1] = catch.as_ref().map(|_| "catch");
            keys[2] = finally_body.as_ref().map(|_| "finally");
        }
        keys
    }

    /// Return a short label for trace output.
    pub fn trace_name(&self) -> &'static str {
        match self {
            StmtKind::Expression(_) => "Expression",
            StmtKind::Assignment { .. } => "Assignment",
            StmtKind::FieldAssignment { .. } => "FieldAssignment",
            StmtKind::IndexAssignment { .. } => "IndexAssignment",
            StmtKind::PathAssignment { .. } => "PathAssignment",
            StmtKind::If { .. } => "If",
            StmtKind::For { .. } => "For",
            StmtKind::ForEach { .. } => "ForEach",
            StmtKind::While { .. } => "While",
            StmtKind::Loop { .. } => "Loop",
            StmtKind::Break(_) => "Break",
            StmtKind::Continue(_) => "Continue",
            StmtKind::BreakIf(..) => "BreakIf",
            StmtKind::ContinueIf(..) => "ContinueIf",
            StmtKind::FunctionDef { .. } => "FunctionDef",
            StmtKind::Return(_) => "Return",
            StmtKind::Select { .. } => "Select",
            StmtKind::Print { .. } => "Print",
            StmtKind::Parse { .. } => "Parse",
            StmtKind::Die(_) => "Die",
            StmtKind::TryCatch { .. } => "TryCatch",
            StmtKind::Export { .. } => "Export",
            StmtKind::Alias { .. } => "Alias",
            StmtKind::Send { .. } => "Send",
            StmtKind::Address { .. } => "Address",
            StmtKind::Emit { .. } => "Emit",
            StmtKind::On { .. } => "On",
            StmtKind::Source { .. } => "Source",
            StmtKind::Include { .. } => "Include",
            StmtKind::Sh { .. } => "Sh",
            StmtKind::PipeToExternal { .. } => "PipeToExternal",
            StmtKind::Chain { .. } => "Chain",
        }
    }
}
