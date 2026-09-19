//! The expression evaluation mode (`eval_expr_string`) and the
//! `send`/`emit` capability gate. V1a lands both lib-mix prerequisites
//! for Mix Scenes: the bare Bus forms are class-gated
//! (`CapabilityClass::Bus`, mirroring the `sh`/`$()` `Process` gates),
//! and a one-expression entry point evaluates a single pure expression
//! under an optional policy with eval limits — rejecting every
//! non-expression construct BEFORE execution, untaken branches included.

use std::cell::RefCell;
use std::rc::Rc;

use cosmix_mix::evaluator::{BusFuture, BusHandler, Evaluator, IncomingEvent, SharedBuf};
use cosmix_mix::lexer::Lexer;
use cosmix_mix::parser::Parser;
use cosmix_mix::value::Value;
use cosmix_mix::{
    CategoryAllowList, EvalLimits, IndexMap, MAX_EXPR_DEPTH, MixResult, eval_expr_string,
};

/// Parse + run `source`, applying `configure` to the evaluator first.
/// Returns Ok(value) or Err(error message).
async fn run_with(source: &str, configure: impl FnOnce(&mut Evaluator)) -> Result<Value, String> {
    let mut lexer = Lexer::new(source);
    let tokens = lexer.tokenize().map_err(|e| e.to_string())?;
    let mut parser = Parser::new(tokens, source);
    let stmts = parser.parse_program().map_err(|e| e.to_string())?;
    let stdout = SharedBuf::new();
    let stderr = SharedBuf::new();
    let mut eval = Evaluator::with_output(Box::new(stdout), Box::new(stderr));
    configure(&mut eval);
    eval.execute(&stmts).await.map_err(|e| e.to_string())
}

/// Records every (target, command) handed to it; replies `rc=0`. The
/// no-policy halves of the gate tests use this to prove the bare forms
/// still DISPATCH when the host allows them.
#[derive(Default)]
struct RecordingBus {
    sent: RefCell<Vec<(String, String)>>,
    emitted: RefCell<Vec<(String, String)>>,
}

impl BusHandler for RecordingBus {
    fn send<'a>(
        &'a self,
        target: &'a str,
        command: &'a str,
        _args: &'a Value,
    ) -> BusFuture<'a, MixResult<(i32, Value)>> {
        let t = target.to_string();
        let c = command.to_string();
        Box::pin(async move {
            self.sent.borrow_mut().push((t, c));
            Ok((0, Value::Bool(true)))
        })
    }

    fn emit<'a>(
        &'a self,
        target: &'a str,
        command: &'a str,
        _args: &'a Value,
    ) -> BusFuture<'a, MixResult<()>> {
        let t = target.to_string();
        let c = command.to_string();
        Box::pin(async move {
            self.emitted.borrow_mut().push((t, c));
            Ok(())
        })
    }

    fn port_exists<'a>(&'a self, _target: &'a str) -> BusFuture<'a, MixResult<bool>> {
        Box::pin(async { Ok(true) })
    }

    fn next_incoming<'a>(&'a self) -> BusFuture<'a, Option<IncomingEvent>> {
        Box::pin(async { None })
    }
}

/// THE FALSIFIABLE GATE — the bare `send`/`emit` broker forms reach Bus
/// authority without a builtin name, so a deny-all `CategoryAllowList`
/// must deny them by CLASS (`Bus`) before any arg evaluates; with a Bus
/// handler registered and no policy, both still dispatch.
#[tokio::test]
async fn pure_policy_denies_send_and_emit() {
    // Deny-all policy, no handler: both forms must raise CAPABILITY_DENIED
    // (the gate fires before the handler is even consulted).
    for src in ["send \"x\" \"y\"\n", "emit \"x\" \"y\"\n"] {
        let err = run_with(src, |e| {
            e.set_capability_policy(Rc::new(CategoryAllowList::new(&[])));
        })
        .await
        .expect_err("deny-all policy must deny the bare Bus forms");
        assert!(err.contains("capability denied"), "got: {err}");
        assert!(err.contains("Bus"), "got: {err}");
    }

    // Handler registered, NO policy: both forms succeed and reach the handler.
    let bus = Rc::new(RecordingBus::default());
    let b2 = Rc::clone(&bus);
    run_with("send \"x\" \"y\"\n", move |e| e.set_bus_handler(b2))
        .await
        .expect("send dispatches with no policy installed");
    let b3 = Rc::clone(&bus);
    run_with("emit \"x\" \"y\"\n", move |e| e.set_bus_handler(b3))
        .await
        .expect("emit dispatches with no policy installed");
    assert_eq!(bus.sent.borrow().len(), 1, "send reached the handler");
    assert_eq!(bus.emitted.borrow().len(), 1, "emit reached the handler");
}

/// The deny-all policy denies the impure builtin classes — pins the
/// existing table classification the expression mode leans on.
#[tokio::test]
async fn policy_denies_impure_builtins() {
    // FsRead / Network / Process, one representative each. Denial fires
    // before the builtin runs, so the paths/URLs are never touched.
    for src in [
        "read_file(\"/nonexistent-gate-probe\")\n",
        "http_get(\"http://127.0.0.1:1/\")\n",
        "run(\"true\")\n",
    ] {
        let err = run_with(src, |e| {
            e.set_capability_policy(Rc::new(CategoryAllowList::new(&[])));
        })
        .await
        .expect_err("deny-all policy must deny impure builtins");
        assert!(err.contains("capability denied"), "got: {err}");
    }
}

// ---------------------------------------------------------------------------
// eval_expr_string — the expression evaluation mode (sync entry point)
// ---------------------------------------------------------------------------

/// eval_expr_string under the deny-all policy with default limits —
/// the shape an embedding host runs.
fn eval_pure(source: &str, globals: &[(&str, Value)]) -> Result<Value, String> {
    eval_expr_string(
        source,
        globals,
        Some(Rc::new(CategoryAllowList::deny_all())),
        EvalLimits::default(),
    )
    .map_err(|e| e.to_string())
}

/// The single-expression rule: anything that is not EXACTLY ONE
/// expression statement is rejected, naming the construct.
#[test]
fn eval_expr_string_rejects_non_expression() {
    let err = eval_pure("$x = 1", &[]).expect_err("assignment is not an expression");
    assert!(err.contains("assignment"), "got: {err}");

    let err = eval_pure("1\n2", &[]).expect_err("two statements are not one expression");
    assert!(err.contains("statements"), "got: {err}");

    let err = eval_pure("if true then 1 end", &[])
        .expect_err("a bare if block is a statement, not an expression");
    assert!(err.contains("if"), "got: {err}");
}

/// The static deny walk: each denied construct errors BEFORE execution
/// — including when it sits in an untaken branch.
#[test]
fn eval_expr_string_static_denies() {
    let cases: &[(&str, &str, &str)] = &[
        // (source, expected construct name, what shape puts it in the tree)
        ("sh \"id\"", "sh statement", "bare statement form"),
        ("false ? 1 : sh \"id\"", "sh expression", "untaken ternary arm"),
        ("$(echo hi)", "command substitution", "bare $() expression"),
        ("false ? 1 : function ($x) = $x", "function literal", "untaken ternary arm"),
        ("$f(1)", "function-value call", "call on a function-valued expr"),
        (
            // Nested position: parse_postfix sees `.unknown(` on a
            // non-builtin name → a real MethodCall node.
            "false ? 1 : $m.unknown(1)",
            "method call",
            "non-builtin method name",
        ),
        (
            // Variable-led statement quirk: the FIRST `.name(` folds to
            // FieldAccess and the `(` becomes a ValueCall (pre-0.33.0
            // map-member-call semantics) — denied as first-class call.
            "$m.unknown(1)",
            "function-value call",
            "bare map-member call statement",
        ),
        ("\"~/root\"", "environment-variable interpolation", "leading ~ expansion"),
        (
            "false ? 1 : (if false then 1 else sh \"id\" end)",
            "sh statement",
            "untaken if-expression branch",
        ),
    ];
    for (src, construct, shape) in cases {
        let err = match eval_pure(src, &[]) {
            Err(e) => e,
            Ok(_) => panic!("{shape} ({src}) must be denied before execution"),
        };
        assert!(err.contains(construct), "{shape} ({src}): got: {err}");
    }

    // A heredoc body carrying `$(...)` — the command-sub STRING part.
    let err = eval_pure("<<EOF\nx$(echo hi)y\nEOF\n", &[])
        .expect_err("command substitution inside a heredoc string must be denied");
    assert!(err.contains("command substitution in string"), "got: {err}");
}

/// Loops and Bus-runtime constructs nested in if-expression branch bodies
/// are denied before execution too. The fuel premise of the mode ("a
/// binding expression cannot loop") holds ONLY if the loop statements a
/// branch body can carry are denied statically — a `for`/`while` in an
/// untaken branch must reject exactly like one that would run. `select`
/// pends on Bus/watch events and `address` targets a Bus service; both
/// are runtime-mode constructs that would hang a host's synchronous eval.
#[test]
fn eval_expr_string_denies_loops_and_bus_constructs_in_if_bodies() {
    let cases: &[(&str, &str)] = &[
        (
            "(if $x then for $i = 1 to 9\n$i\nend else 0 end)",
            "for loop",
        ),
        (
            "(if $x then for $e in [1, 2]\n$e\nend else 0 end)",
            "for-each loop",
        ),
        (
            "(if $x then while false\n1\nend else 0 end)",
            "while loop",
        ),
        (
            "(if $x then loop\n1\nend else 0 end)",
            "loop statement",
        ),
        (
            "(if $x then select 1\nwhen 1 then 1\notherwise 0\nend else 0 end)",
            "select statement",
        ),
        ("(if $x then address \"sh\"\nend else 0 end)", "address block"),
    ];
    for (src, construct) in cases {
        // $x is false — every branch is untaken; denial is static, so the
        // constructs reject anyway. That is the property under test.
        let err = match eval_pure(src, &[("x", Value::Bool(false))]) {
            Err(e) => e,
            Ok(_) => panic!("{src} must be denied before execution (untaken branch)"),
        };
        assert!(err.contains(construct), "{src}: got: {err}");
    }
}

/// The pure shapes an embedding host actually evaluates — all allowed,
/// under the deny-all policy.
#[test]
fn eval_expr_string_allows_pure_shapes() {
    let ok = Value::Bool(true);
    let n = Value::Number(5.0);

    assert_eq!(eval_pure("1 + 2 * 3", &[]).unwrap(), Value::Number(7.0));
    assert_eq!(eval_pure("'a' .. 1", &[]).unwrap(), Value::String("a1".into()));
    assert_eq!(
        eval_pure("$ok ? \"yes\" : \"no\"", &[("ok", ok.clone())]).unwrap(),
        Value::String("yes".into())
    );
    // if-as-expression (nested in parens — a bare `if` is a statement).
    assert_eq!(
        eval_pure("(if $n > 2 then \"big\" else \"small\" end)", &[("n", n.clone())])
            .unwrap(),
        Value::String("big".into())
    );
    assert_eq!(eval_pure("[10, 20, 30][1]", &[]).unwrap(), Value::Number(20.0));
    assert_eq!(eval_pure("length([1, 2, 3])", &[]).unwrap(), Value::Number(3.0));
    assert_eq!(eval_pure("length({a: 1, b: 2})", &[]).unwrap(), Value::Number(2.0));
    assert_eq!(eval_pure("upper(\"abc\")", &[]).unwrap(), Value::String("ABC".into()));
    // Method syntax on a builtin desugars to a bareword FunctionCall at
    // parse time (Parser::parse_postfix), so it stays allowed — in a
    // NESTED position. (Bare `$s.upper()` as the whole statement is the
    // variable-led first-accessor form: a ValueCall on the map member,
    // denied like every first-class call.)
    assert_eq!(
        eval_pure("'X-' .. $s.upper()", &[("s", Value::String("abc".into()))]).unwrap(),
        Value::String("X-ABC".into())
    );
}

/// Fuel and depth: a result over `max_string_len` errors cleanly, and a
/// tree deeper than `MAX_EXPR_DEPTH` errors cleanly (no panic).
#[test]
fn eval_expr_string_fuel_and_depth() {
    // Fuel: 24-byte global concatenated with itself exceeds a 32-byte cap.
    // (Checked on the generic `..` path before the value is stored.)
    let pad = Value::String("x".repeat(24));
    let err = eval_expr_string(
        "$pad .. $pad",
        &[("pad", pad)],
        Some(Rc::new(CategoryAllowList::deny_all())),
        EvalLimits {
            max_string_len: Some(32),
            ..Default::default()
        },
    )
    .expect_err("an over-cap concat must error cleanly");
    assert!(err.to_string().contains("string length"), "got: {err}");

    // Depth: a 300-term `..` chain parses iteratively (left-associative)
    // but builds a left-deep tree — deeper than MAX_EXPR_DEPTH (256).
    let deep = std::iter::repeat_n("'a'", MAX_EXPR_DEPTH + 50)
        .collect::<Vec<_>>()
        .join(" .. ");
    let err = eval_pure(&deep, &[])
        .expect_err("an over-depth expression must error cleanly, not overflow");
    assert!(err.contains("MAX_EXPR_DEPTH"), "got: {err}");
}

/// Preset globals are visible to the expression, exactly as handler
/// dispatch sees `$event`.
#[test]
fn globals_visible_in_expr() {
    let mut model = IndexMap::new();
    model.insert("a".to_string(), Value::Number(21.0));
    let v = eval_pure("$model.a * 2", &[("model", Value::map(model))])
        .expect("field access on a preset global");
    assert_eq!(v, Value::Number(42.0));
}
