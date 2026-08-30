use cosmix_mix::evaluator::{Evaluator, SharedBuf};
use cosmix_mix::lexer::Lexer;
use cosmix_mix::parser::Parser;

async fn run_mix_capturing(source: &str) -> Result<String, String> {
    let mut lexer = Lexer::new(source);
    let tokens = lexer.tokenize().map_err(|e| e.to_string())?;
    let mut parser = Parser::new(tokens, source);
    let stmts = parser.parse_program().map_err(|e| e.to_string())?;

    let stdout = SharedBuf::new();
    let stderr = SharedBuf::new();

    let mut eval = Evaluator::with_output(Box::new(stdout.clone()), Box::new(stderr.clone()));
    eval.execute(&stmts).await.map_err(|e| e.to_string())?;

    let output = stdout.to_string_lossy();
    Ok(output)
}

fn extract_expected(source: &str) -> Vec<String> {
    let mut expected = Vec::new();
    let mut in_expected = false;
    for line in source.lines() {
        let trimmed = line.trim();
        if trimmed.starts_with("-- Expected output:") {
            in_expected = true;
            continue;
        }
        if in_expected {
            if let Some(rest) = trimmed.strip_prefix("-- ") {
                expected.push(rest.to_string());
            } else {
                break;
            }
        }
    }
    expected
}

async fn run_test_script(name: &str) {
    let path = format!("tests/scripts/{}", name);
    let source =
        std::fs::read_to_string(&path).unwrap_or_else(|e| panic!("Failed to read {}: {}", path, e));

    let expected = extract_expected(&source);
    assert!(!expected.is_empty(), "No expected output found in {}", name);

    let output = run_mix_capturing(&source)
        .await
        .unwrap_or_else(|e| panic!("{} failed: {}", name, e));

    let actual_lines: Vec<&str> = output.trim().lines().collect();

    assert_eq!(
        actual_lines.len(),
        expected.len(),
        "{}: expected {} lines, got {}.\nExpected:\n{}\nActual:\n{}",
        name,
        expected.len(),
        actual_lines.len(),
        expected.join("\n"),
        actual_lines.join("\n"),
    );

    for (i, (actual, exp)) in actual_lines.iter().zip(expected.iter()).enumerate() {
        assert_eq!(
            actual,
            exp,
            "{} line {}: expected '{}', got '{}'",
            name,
            i + 1,
            exp,
            actual
        );
    }
}

#[tokio::test]
async fn test_variables() {
    run_test_script("variables.mix").await;
}

// A runtime error raised on the sync fast path (inside a user function
// body that `is_stmt_sync` accepts) must report the offending
// statement's own line, not a stale line left by the last async
// statement (e.g. the call site). Regression guard for the
// `current_line` bookkeeping added alongside the classifier-drift fix.
#[tokio::test]
async fn test_sync_path_error_reports_correct_line() {
    let source = "function f()\n  $a = 1\n  $b = $undefined_zz\n  return $b\nend\nprint(f())\n";
    let err = run_mix_capturing(source)
        .await
        .expect_err("undefined variable must error");
    assert!(
        err.contains("undefined variable"),
        "expected undefined-variable error, got: {err}"
    );
    assert!(
        err.contains("line 3"),
        "error should point at the offending statement (line 3), got: {err}"
    );
}

#[tokio::test]
async fn test_strings() {
    run_test_script("strings.mix").await;
}

#[tokio::test]
async fn test_command_sub_removed() {
    run_test_script("command_sub_removed.mix").await;
}

#[tokio::test]
async fn test_ssh_quoting() {
    run_test_script("ssh_quoting.mix").await;
}

#[tokio::test]
async fn test_control() {
    run_test_script("control.mix").await;
}

#[tokio::test]
async fn test_ternary() {
    run_test_script("ternary.mix").await;
}

#[tokio::test]
async fn test_if_expr() {
    run_test_script("if_expr.mix").await;
}

#[tokio::test]
async fn test_elif_chain() {
    // `elif` is a one-word synonym for `else if`: it parses into the same
    // flat `else_ifs` chain closed by a single `end`. Exercise the statement
    // form, the expression position, and a chain that mixes both spellings.
    let src = r#"
$grade = function($n)
  if $n >= 90 then
    return "A"
  elif $n >= 80 then
    return "B"
  elif $n >= 70 then
    return "C"
  else
    return "F"
  end
end
print($grade(95) .. $grade(85) .. $grade(72) .. $grade(50))

$n = 2
print(if $n == 1 then "one" elif $n == 2 then "two" else "many" end)

$x = 4
if $x == 1 then
  print("a")
elif $x == 2 then
  print("b")
else if $x == 4 then
  print("mixed")
else
  print("z")
end
"#;
    let out = run_mix_capturing(src)
        .await
        .expect("elif chain must evaluate");
    let lines: Vec<&str> = out.trim().lines().collect();
    assert_eq!(lines, vec!["ABCF", "two", "mixed"], "got: {out:?}");
}

#[tokio::test]
async fn test_is_empty() {
    run_test_script("is_empty.mix").await;
}

#[tokio::test]
async fn test_loops() {
    run_test_script("loops.mix").await;
}

#[tokio::test]
async fn test_functions() {
    run_test_script("functions.mix").await;
}

#[tokio::test]
async fn test_lambdas() {
    run_test_script("lambdas.mix").await;
}

#[tokio::test]
async fn test_list_hofs() {
    run_test_script("list_hofs.mix").await;
}

#[tokio::test]
async fn test_slicing() {
    run_test_script("slicing.mix").await;
}

#[tokio::test]
async fn test_printf() {
    run_test_script("printf.mix").await;
}

#[tokio::test]
async fn test_fs_ergo() {
    run_test_script("fs_ergo.mix").await;
}

#[tokio::test]
async fn test_terminators() {
    run_test_script("terminators.mix").await;
}

#[tokio::test]
async fn test_jsonl() {
    run_test_script("jsonl.mix").await;
}

#[tokio::test]
async fn test_continue_for_each() {
    run_test_script("continue_for_each.mix").await;
}

#[tokio::test]
async fn test_lists() {
    run_test_script("lists.mix").await;
}

#[tokio::test]
async fn test_bytes() {
    run_test_script("bytes.mix").await;
}

#[tokio::test]
async fn test_tables() {
    run_test_script("tables.mix").await;
}

#[tokio::test]
async fn test_extensions() {
    use cosmix_mix::value::Value;

    let path = "tests/scripts/extensions.mix";
    let source =
        std::fs::read_to_string(path).unwrap_or_else(|e| panic!("Failed to read {}: {}", path, e));

    let expected = extract_expected(&source);
    assert!(!expected.is_empty());

    let mut lexer = Lexer::new(&source);
    let tokens = lexer.tokenize().unwrap();
    let mut parser = Parser::new(tokens, &source);
    let stmts = parser.parse_program().unwrap();

    let stdout = SharedBuf::new();
    let stderr = SharedBuf::new();
    let mut eval = Evaluator::with_output(Box::new(stdout.clone()), Box::new(stderr.clone()));

    // Register extension functions (using sync_ext helper for sync closures)
    eval.register(
        "add_numbers",
        cosmix_mix::sync_ext(|args| {
            let a = args.first().and_then(|v| v.to_number()).unwrap_or(0.0);
            let b = args.get(1).and_then(|v| v.to_number()).unwrap_or(0.0);
            Ok(Value::Number(a + b))
        }),
    );

    eval.register(
        "greet",
        cosmix_mix::sync_ext(|args| {
            let name = args.first().map(|v| v.to_mix_string()).unwrap_or_default();
            Ok(Value::String(format!("hello from {}", name)))
        }),
    );

    eval.register(
        "get_list",
        cosmix_mix::sync_ext(|_args| {
            Ok(Value::list(vec![
                Value::String("alpha".to_string()),
                Value::String("bravo".to_string()),
                Value::String("charlie".to_string()),
            ]))
        }),
    );

    eval.execute(&stmts).await.unwrap();

    let output = stdout.to_string_lossy();
    let actual_lines: Vec<&str> = output.trim().lines().collect();

    assert_eq!(
        actual_lines.len(),
        expected.len(),
        "extensions: expected {} lines, got {}.\nExpected:\n{}\nActual:\n{}",
        expected.len(),
        actual_lines.len(),
        expected.join("\n"),
        actual_lines.join("\n")
    );

    for (i, (actual, exp)) in actual_lines.iter().zip(expected.iter()).enumerate() {
        assert_eq!(
            actual,
            exp,
            "extensions line {}: expected '{}', got '{}'",
            i + 1,
            exp,
            actual
        );
    }
}

#[tokio::test]
async fn test_parse() {
    run_test_script("parse.mix").await;
}

#[tokio::test]
async fn test_system() {
    run_test_script("system.mix").await;
}

#[tokio::test]
async fn test_fileio() {
    run_test_script("fileio.mix").await;
}

#[tokio::test]
async fn test_json() {
    run_test_script("json.mix").await;
}

#[tokio::test]
async fn test_export() {
    run_test_script("export.mix").await;
}

#[tokio::test]
async fn test_labels() {
    run_test_script("labels.mix").await;
}

#[tokio::test]
async fn test_alias_dynamic() {
    run_test_script("alias_dynamic.mix").await;
}

#[tokio::test]
async fn test_tilde_expansion() {
    run_test_script("tilde_expansion.mix").await;
}

#[tokio::test]
async fn test_interp_brace_env_fallback() {
    // Phase A: scope → env → nil for ${X} in double-strings and
    // heredocs. Script self-verifies against env("PATH")/env("HOME")
    // so it doesn't depend on any value the test harness sets — no
    // env mutation required, safe to run in-process under tokio.
    //
    // The "all heads, not just uppercase" half of the contract is
    // pinned out-of-process by
    // `crates/cosmix-mix/tests/env_fallback_lc.rs` (subprocesses the
    // mix binary with the probe env var set on the child, avoiding
    // the parallel-test `set_var` UB).
    run_test_script("interp_brace_env_fallback.mix").await;
}

#[tokio::test]
async fn test_interp_coalesce() {
    // `${x ?? default}` (nil-only) and `${x ?: default}` (any falsy)
    // interpolation defaults: unbound name, nil value, present value,
    // empty-string handling, and expression/variable defaults.
    run_test_script("interp_coalesce.mix").await;
}

#[tokio::test]
async fn test_send() {
    use cosmix_mix::error::MixResult;
    use cosmix_mix::evaluator::BusHandler;
    use cosmix_mix::value::Value;
    use std::future::Future;
    use std::pin::Pin;

    /// Mock Bus handler that echoes command and args back.
    /// For "greet" command: returns "{name} from mock"
    /// For dotted/other commands: returns "{command}:{first_arg_value} from mock"
    struct MockBusHandler;

    impl BusHandler for MockBusHandler {
        fn send<'a>(
            &'a self,
            _target: &'a str,
            command: &'a str,
            args: &'a Value,
        ) -> Pin<Box<dyn Future<Output = MixResult<(i32, Value)>> + 'a>> {
            Box::pin(async move {
                let result = match args {
                    Value::Map(m) => {
                        if command == "greet" {
                            // Legacy: return name arg value
                            let name = m
                                .get("name")
                                .map(|v| v.to_mix_string())
                                .unwrap_or_else(|| "unknown".to_string());
                            format!("{} from mock", name)
                        } else {
                            // Return command:first_arg_value for testing
                            let first_val = m
                                .values()
                                .next()
                                .map(|v| v.to_mix_string())
                                .unwrap_or_default();
                            format!("{}:{} from mock", command, first_val)
                        }
                    }
                    _ => format!("{}: from mock", command),
                };
                Ok((0, Value::String(result)))
            })
        }

        fn emit<'a>(
            &'a self,
            _target: &'a str,
            _command: &'a str,
            _args: &'a Value,
        ) -> Pin<Box<dyn Future<Output = MixResult<()>> + 'a>> {
            Box::pin(async move { Ok(()) })
        }

        fn port_exists<'a>(
            &'a self,
            _target: &'a str,
        ) -> Pin<Box<dyn Future<Output = MixResult<bool>> + 'a>> {
            Box::pin(async move { Ok(true) })
        }

        fn next_incoming<'a>(
            &'a self,
        ) -> Pin<Box<dyn Future<Output = Option<cosmix_mix::evaluator::IncomingEvent>> + 'a>>
        {
            Box::pin(async move { None })
        }
    }

    let path = "tests/scripts/send.mix";
    let source =
        std::fs::read_to_string(path).unwrap_or_else(|e| panic!("Failed to read {}: {}", path, e));

    let expected = extract_expected(&source);
    assert!(!expected.is_empty());

    let mut lexer = Lexer::new(&source);
    let tokens = lexer.tokenize().unwrap();
    let mut parser = Parser::new(tokens, &source);
    let stmts = parser.parse_program().unwrap();

    let stdout = SharedBuf::new();
    let stderr = SharedBuf::new();
    let mut eval = Evaluator::with_output(Box::new(stdout.clone()), Box::new(stderr.clone()));

    eval.set_bus_handler(Rc::new(MockBusHandler));

    eval.execute(&stmts).await.unwrap();

    let output = stdout.to_string_lossy();
    let actual_lines: Vec<&str> = output.trim().lines().collect();

    assert_eq!(
        actual_lines.len(),
        expected.len(),
        "send: expected {} lines, got {}.\nExpected:\n{}\nActual:\n{}",
        expected.len(),
        actual_lines.len(),
        expected.join("\n"),
        actual_lines.join("\n")
    );

    for (i, (actual, exp)) in actual_lines.iter().zip(expected.iter()).enumerate() {
        assert_eq!(
            actual,
            exp,
            "send line {}: expected '{}', got '{}'",
            i + 1,
            exp,
            actual
        );
    }
}

// ── `on` handler registration (Phase 1: registry only, no dispatch) ──

#[tokio::test]
async fn test_on_registers_single_handler() {
    let source = r#"
on topic.delivery
    print "unreachable in stub phase"
done
print "registered"
"#;
    let mut lexer = Lexer::new(source);
    let tokens = lexer.tokenize().unwrap();
    let mut parser = Parser::new(tokens, source);
    let stmts = parser.parse_program().unwrap();

    let stdout = SharedBuf::new();
    let stderr = SharedBuf::new();
    let mut eval = Evaluator::with_output(Box::new(stdout.clone()), Box::new(stderr.clone()));
    eval.execute(&stmts).await.unwrap();

    assert_eq!(
        eval.handler_count(),
        1,
        "expected 1 handler after `on topic.delivery`"
    );
    assert_eq!(eval.handler_command_count(), 1);
    assert_eq!(stdout.to_string_lossy().trim(), "registered");
}

#[tokio::test]
async fn test_on_multiple_handlers_same_command() {
    // Decision (4): multiple handlers per command, fire in registration
    // order. Registry should preserve all three.
    let source = r#"
on topic.delivery
    print "h1"
done
on topic.delivery
    print "h2"
done
on topic.delivery
    print "h3"
done
"#;
    let mut lexer = Lexer::new(source);
    let tokens = lexer.tokenize().unwrap();
    let mut parser = Parser::new(tokens, source);
    let stmts = parser.parse_program().unwrap();

    let mut eval = Evaluator::new();
    eval.execute(&stmts).await.unwrap();

    assert_eq!(eval.handler_count(), 3);
    assert_eq!(eval.handler_command_count(), 1);
}

#[tokio::test]
async fn test_on_handlers_different_commands() {
    let source = r#"
on topic.delivery
    print "delivery"
done
on topic.idle
    print "idle"
done
on topic.active
    print "active"
done
"#;
    let mut lexer = Lexer::new(source);
    let tokens = lexer.tokenize().unwrap();
    let mut parser = Parser::new(tokens, source);
    let stmts = parser.parse_program().unwrap();

    let mut eval = Evaluator::new();
    eval.execute(&stmts).await.unwrap();

    assert_eq!(eval.handler_count(), 3);
    assert_eq!(eval.handler_command_count(), 3);
}

#[tokio::test]
async fn test_on_registration_inside_loop() {
    // Parser permissiveness: `on` inside a loop registers each iteration.
    // This is the design decision to enforce parse-time only what's invalid
    // and let runtime observability handle semantically-surprising patterns.
    let source = r#"
for $i = 1 to 3
    on topic.delivery
        print "handler"
    done
next
"#;
    let mut lexer = Lexer::new(source);
    let tokens = lexer.tokenize().unwrap();
    let mut parser = Parser::new(tokens, source);
    let stmts = parser.parse_program().unwrap();

    let mut eval = Evaluator::new();
    eval.execute(&stmts).await.unwrap();

    assert_eq!(eval.handler_count(), 3, "3 iterations register 3 handlers");
}

#[tokio::test]
async fn test_on_no_handlers_count_zero() {
    let source = r#"print "no handlers here""#;
    let mut lexer = Lexer::new(source);
    let tokens = lexer.tokenize().unwrap();
    let mut parser = Parser::new(tokens, source);
    let stmts = parser.parse_program().unwrap();

    let mut eval = Evaluator::new();
    eval.execute(&stmts).await.unwrap();

    assert_eq!(eval.handler_count(), 0);
    assert_eq!(eval.handler_command_count(), 0);
}

// ── `on` dispatch (phase 2): $event injection, serialization, errors ──

use cosmix_mix::evaluator::IncomingEvent;
use std::collections::BTreeMap;

fn mk_event(command: &str, body: &str, headers: &[(&str, &str)]) -> IncomingEvent {
    let mut h = BTreeMap::new();
    for (k, v) in headers {
        h.insert(k.to_string(), v.to_string());
    }
    IncomingEvent {
        command: command.to_string(),
        headers: h,
        body: body.to_string(),
    }
}

#[tokio::test]
async fn test_dispatch_injects_event_scope() {
    // Handler reads $event.command, $event.body, $event.headers["topic"]
    // and appends each to an outer-scope list. Confirms the $event shape
    // is correct and all three access patterns work from within a handler.
    let source = r#"
$seen_command = ""
$seen_body = ""
$seen_topic = ""

on test.msg
    $seen_command = $event.command
    $seen_body = $event.body
    $seen_topic = $event.headers["topic"]
done
"#;
    let stdout = SharedBuf::new();
    let stderr = SharedBuf::new();
    let mut eval = Evaluator::with_output(Box::new(stdout.clone()), Box::new(stderr.clone()));

    let mut lexer = Lexer::new(source);
    let stmts = Parser::new(lexer.tokenize().unwrap(), source)
        .parse_program()
        .unwrap();
    eval.execute(&stmts).await.unwrap();

    assert_eq!(eval.handler_count(), 1);

    eval.dispatch_event(mk_event(
        "test.msg",
        "hello world",
        &[("topic", "test.topic")],
    ))
    .await
    .unwrap();

    // Verify outer-scope variables were updated via walk-up assignment
    assert_eq!(
        eval.get_global("seen_command").unwrap().to_mix_string(),
        "test.msg"
    );
    assert_eq!(
        eval.get_global("seen_body").unwrap().to_mix_string(),
        "hello world"
    );
    assert_eq!(
        eval.get_global("seen_topic").unwrap().to_mix_string(),
        "test.topic"
    );
}

#[tokio::test]
async fn test_dispatch_no_handler_is_noop() {
    let mut eval = Evaluator::new();
    // No handlers registered — dispatching should be a silent no-op
    let result = eval
        .dispatch_event(mk_event("unknown.command", "", &[]))
        .await;
    assert!(result.is_ok());
    assert!(!eval.is_dispatching());
}

#[tokio::test]
async fn test_dispatch_multiple_handlers_registration_order() {
    // Decision (4): multiple handlers fire in registration order
    let source = r#"
$trace = ""
on test.msg
    $trace = $trace .. "A"
done
on test.msg
    $trace = $trace .. "B"
done
on test.msg
    $trace = $trace .. "C"
done
"#;
    let mut eval = Evaluator::new();
    let mut lexer = Lexer::new(source);
    let stmts = Parser::new(lexer.tokenize().unwrap(), source)
        .parse_program()
        .unwrap();
    eval.execute(&stmts).await.unwrap();

    eval.dispatch_event(mk_event("test.msg", "", &[]))
        .await
        .unwrap();
    assert_eq!(eval.get_global("trace").unwrap().to_mix_string(), "ABC");
}

#[tokio::test]
async fn test_dispatch_event_readonly_direct_assignment() {
    // Decision (5): $event is read-only inside handlers.
    // Direct `$event = ...` should error loudly, not silently clobber.
    let source = r#"
on test.msg
    $event = "replaced"
done
"#;
    let mut eval = Evaluator::new();
    let mut lexer = Lexer::new(source);
    let stmts = Parser::new(lexer.tokenize().unwrap(), source)
        .parse_program()
        .unwrap();
    eval.execute(&stmts).await.unwrap();

    // Handler body errors, but dispatch swallows the error (log-and-continue)
    // and returns Ok. We verify behavior via the stderr tracing log or by
    // observing that nothing crashed — the error doesn't propagate.
    let result = eval.dispatch_event(mk_event("test.msg", "body", &[])).await;
    assert!(result.is_ok(), "handler errors should not propagate");
    // Handler is still registered (log-and-continue preserves registration)
    assert_eq!(eval.handler_count(), 1);
}

#[tokio::test]
async fn test_dispatch_event_readonly_field_assignment() {
    let source = r#"
on test.msg
    $event.body = "replaced"
done
"#;
    let mut eval = Evaluator::new();
    let mut lexer = Lexer::new(source);
    let stmts = Parser::new(lexer.tokenize().unwrap(), source)
        .parse_program()
        .unwrap();
    eval.execute(&stmts).await.unwrap();

    let result = eval.dispatch_event(mk_event("test.msg", "body", &[])).await;
    assert!(result.is_ok());
    assert_eq!(eval.handler_count(), 1);
}

#[tokio::test]
async fn test_dispatch_event_readonly_index_assignment() {
    let source = r#"
on test.msg
    $event["body"] = "replaced"
done
"#;
    let mut eval = Evaluator::new();
    let mut lexer = Lexer::new(source);
    let stmts = Parser::new(lexer.tokenize().unwrap(), source)
        .parse_program()
        .unwrap();
    eval.execute(&stmts).await.unwrap();

    let result = eval.dispatch_event(mk_event("test.msg", "body", &[])).await;
    assert!(result.is_ok());
    assert_eq!(eval.handler_count(), 1);
}

#[tokio::test]
async fn test_dispatch_alias_copy_is_mutable() {
    // $event is read-only, but copying into a local should be freely mutable.
    // Verifies the "copy-by-value, no aliasing attack" property — the
    // $alias map is independent from $event after the assignment.
    let source = r#"
$result = ""
on test.msg
    $copy = $event
    $copy.body = "mutated"
    $result = $copy.body
done
"#;
    let mut eval = Evaluator::new();
    let mut lexer = Lexer::new(source);
    let stmts = Parser::new(lexer.tokenize().unwrap(), source)
        .parse_program()
        .unwrap();
    eval.execute(&stmts).await.unwrap();

    eval.dispatch_event(mk_event("test.msg", "original", &[]))
        .await
        .unwrap();
    assert_eq!(
        eval.get_global("result").unwrap().to_mix_string(),
        "mutated"
    );
}

#[tokio::test]
async fn test_dispatch_serialization_sequential() {
    // Property test: dispatch N events sequentially (each `dispatch_event`
    // awaits the scheduler's write permit before running the handler).
    // For Class S chains (no `async` handlers), the await acquires the
    // `tokio::sync::RwLock` writer permit; with no concurrent dispatches
    // there is no contention, and the handlers run in call order.
    //
    // C.7d retired the `pending_events` FIFO and `queue_event` /
    // `drain_pending` methods — the per-citizen event pump (run_source's
    // tokio::select! against next_incoming) is now the sole arrival site,
    // and Class C dispatches are spawned on the LocalSet instead of
    // queued for inline drain. Sequential `dispatch_event` is the direct
    // analogue at the test harness layer.
    //
    // Without real async yield-point integration (sleep drains incoming),
    // we can't cause reentrant dispatch from within a handler body here.
    // What we CAN verify is that sequential dispatches each acquire the
    // scheduler admission guard exclusively, run their handler to
    // completion, release the guard, and the next dispatch then admits.
    // The stronger reentrancy test lands with sleep integration in
    // phase 3.
    let source = r#"
$trace = ""
$active = 0
$max_active = 0

on test.msg
    $active = $active + 1
    if $active > $max_active then $max_active = $active end
    $trace = $trace .. $event.body
    $active = $active - 1
done
"#;
    let mut eval = Evaluator::new();
    let mut lexer = Lexer::new(source);
    let stmts = Parser::new(lexer.tokenize().unwrap(), source)
        .parse_program()
        .unwrap();
    eval.execute(&stmts).await.unwrap();

    // Dispatch events 1..5 sequentially — each await releases the
    // scheduler write guard before the next acquires it.
    for n in ["1", "2", "3", "4", "5"] {
        eval.dispatch_event(mk_event("test.msg", n, &[]))
            .await
            .unwrap();
    }

    // Verify: in-order trace, counter never exceeded 1, ended at 0
    assert_eq!(eval.get_global("trace").unwrap().to_mix_string(), "12345");
    assert_eq!(
        eval.get_global("max_active").unwrap().to_number().unwrap() as i64,
        1,
        "max concurrent handlers should be 1 (serialized execution)"
    );
    assert_eq!(
        eval.get_global("active").unwrap().to_number().unwrap() as i64,
        0,
        "counter should return to 0 after all handlers complete"
    );
    assert!(
        !eval.is_dispatching(),
        "scheduler admission guard should be released after dispatch"
    );
}

// ── `on` dispatch (phase 3): sleep as yield point, real-channel property test ──
//
// All three tests in this section require the `tokio-sleep` feature — the
// sleep builtin's yield-point loop only exists under `#[cfg(feature =
// "tokio-sleep")]`. Without the feature, sleep falls through to
// `std::thread::sleep` which blocks the whole thread with no yielding,
// and these tests would fail with cryptic assertions that look like
// logic bugs. The `cfg_attr(ignore)` gating makes the skip visible
// instead, so a future developer running `cargo test --no-default-features`
// sees "3 tests skipped" with a clear reason.
//
// Process lesson (recorded here because this is where the scars live):
// when a test fails in a way that implies the code under test isn't
// being reached, verify which `#[cfg]` branch was actually compiled
// before debugging logic. The feature-gate detour on phase 3a wasted
// time that a single `cfg_attr(ignore)` would have saved.

#[tokio::test(flavor = "current_thread")]
#[cfg_attr(not(feature = "tokio-sleep"), ignore = "requires tokio-sleep feature")]
async fn test_sleep_yields_to_handler_dispatch() {
    // Simple sanity: a single event in the channel gets dispatched
    // during a sleep, and the main body observes the side effect.
    use cosmix_mix::error::MixResult;
    use cosmix_mix::evaluator::BusHandler;
    use cosmix_mix::value::Value;
    use std::cell::RefCell;
    use std::future::Future;
    use std::pin::Pin;
    use tokio::sync::mpsc;

    struct ChannelHandler {
        rx: RefCell<Option<mpsc::UnboundedReceiver<IncomingEvent>>>,
        closed: RefCell<bool>,
    }

    impl ChannelHandler {
        fn new(rx: mpsc::UnboundedReceiver<IncomingEvent>) -> Self {
            ChannelHandler {
                rx: RefCell::new(Some(rx)),
                closed: RefCell::new(false),
            }
        }
    }

    impl BusHandler for ChannelHandler {
        fn send<'a>(
            &'a self,
            _t: &'a str,
            _c: &'a str,
            _a: &'a Value,
        ) -> Pin<Box<dyn Future<Output = MixResult<(i32, Value)>> + 'a>> {
            Box::pin(async move { Ok((0, Value::Nil)) })
        }

        fn emit<'a>(
            &'a self,
            _t: &'a str,
            _c: &'a str,
            _a: &'a Value,
        ) -> Pin<Box<dyn Future<Output = MixResult<()>> + 'a>> {
            Box::pin(async move { Ok(()) })
        }

        fn port_exists<'a>(
            &'a self,
            _t: &'a str,
        ) -> Pin<Box<dyn Future<Output = MixResult<bool>> + 'a>> {
            Box::pin(async move { Ok(false) })
        }

        fn next_incoming<'a>(
            &'a self,
        ) -> Pin<Box<dyn Future<Output = Option<IncomingEvent>> + 'a>> {
            Box::pin(async move {
                if *self.closed.borrow() {
                    return None;
                }
                // Take-then-restore: do not hold RefCell borrow across await.
                let mut rx = match self.rx.borrow_mut().take() {
                    Some(r) => r,
                    None => return None,
                };
                let result = rx.recv().await;
                *self.rx.borrow_mut() = Some(rx);
                if result.is_none() {
                    *self.closed.borrow_mut() = true;
                }
                result
            })
        }
    }

    let (tx, rx) = mpsc::unbounded_channel::<IncomingEvent>();

    let source = r#"
$fired = 0
on test.msg
    $fired = $fired + 1
done
sleep(0.2)
"#;
    let mut eval = Evaluator::new();
    eval.set_bus_handler(Rc::new(ChannelHandler::new(rx)));

    // Push the event BEFORE execute — it's buffered in the channel, so
    // when the main body's sleep(0.2) hits the select! yield point,
    // next_incoming returns immediately with the buffered event.
    tx.send(mk_event("test.msg", "1", &[])).unwrap();
    drop(tx); // close channel so transport_closed path fires after drain

    let mut lexer = Lexer::new(source);
    let stmts = Parser::new(lexer.tokenize().unwrap(), source)
        .parse_program()
        .unwrap();
    eval.execute(&stmts).await.unwrap();

    assert_eq!(
        eval.get_global("fired").unwrap().to_number().unwrap() as i64,
        1,
        "handler should have fired exactly once during sleep"
    );
}

#[tokio::test(flavor = "current_thread")]
#[cfg_attr(not(feature = "tokio-sleep"), ignore = "requires tokio-sleep feature")]
async fn test_class_s_sleep_serializes_dispatches() {
    // PROPERTY TEST — Class S serialization under the C.7d concurrency
    // model.
    //
    // **Invariant.** A sync `on` handler (Class S) holds the dispatch
    // scheduler's writer permit for the full duration of its body. Two
    // events arriving back-to-back at the per-citizen event pump are
    // dispatched sequentially — the second dispatch_event awaits the
    // first's writer release before running. At most ONE Class S
    // handler body is active at a time, regardless of yield points
    // (sleep, future async builtins) inside the handler.
    //
    // **What this used to test, and what changed.** Pre-C.7d the
    // serialization story was a `pending_events` FIFO + reentrant
    // queue: `sleep` inside a handler body fired an inner select! →
    // reentrant dispatch_event → `try_enter_dispatch::Reentrant` →
    // push onto pending_events → drain after the outer body returned.
    // C.7d retired both the queue and the reentrant arm; the
    // sole admission gate is now `acquire_writer_dispatch().await`
    // (Class S) / `spawn_local + acquire_reader().await` (Class C).
    // Per Codex C.7d R1 decision, `sleep` inside a held dispatch
    // admission is a plain timer (evaluator.rs `is_dispatching()`
    // gate); the inner select!→dispatch arm is only active when
    // sleep is called from the main body (no admission held). The
    // serialization property survives, but its mechanism is now
    // "writer permit blocks the second dispatch_event" rather than
    // "reentrant arm queues onto pending_events".
    //
    // **Sequence under C.7d.**
    //   1. Push event 1 + event 2 into the channel (buffered).
    //   2. Main body sleep(0.3) enters yield-point path (no admission
    //      held → select! is active).
    //   3. select! fires with event 1 → dispatch_event acquires
    //      writer → handler 1 body runs.
    //   4. Handler 1's `sleep(0.05)` sees `is_dispatching()` true →
    //      plain timer sleep (no inner select!).
    //   5. Handler 1 body exits → writer permit releases → dispatch_event
    //      returns to main-body sleep loop.
    //   6. Main-body sleep loop re-enters select! → fires with event 2 →
    //      dispatch_event acquires writer → handler 2 body runs to
    //      completion the same way.
    //   7. Main-body sleep deadline elapses → returns.
    //
    // Assertions stay identical: `$trace == "12"`, `$max_active == 1`,
    // `$active == 0`. The mechanism producing them changed; the
    // invariant did not.
    //
    // **Why this still catches regressions.** If `dispatch_event` ever
    // admits a second dispatch before the first's writer releases
    // (e.g. a broken `acquire_writer_dispatch` that returns without
    // actually holding the permit, or a refactor that drops the guard
    // before run_handlers_for returns), event 2 would race event 1
    // inside its handler-1 scope frame and `$max_active` would reach
    // 2 with the assertion message naming the broken invariant.
    use cosmix_mix::error::MixResult;
    use cosmix_mix::evaluator::BusHandler;
    use cosmix_mix::value::Value;
    use std::cell::RefCell;
    use std::future::Future;
    use std::pin::Pin;
    use tokio::sync::mpsc;

    struct ChannelHandler {
        rx: RefCell<Option<mpsc::UnboundedReceiver<IncomingEvent>>>,
        closed: RefCell<bool>,
    }

    impl ChannelHandler {
        fn new(rx: mpsc::UnboundedReceiver<IncomingEvent>) -> Self {
            ChannelHandler {
                rx: RefCell::new(Some(rx)),
                closed: RefCell::new(false),
            }
        }
    }

    impl BusHandler for ChannelHandler {
        fn send<'a>(
            &'a self,
            _t: &'a str,
            _c: &'a str,
            _a: &'a Value,
        ) -> Pin<Box<dyn Future<Output = MixResult<(i32, Value)>> + 'a>> {
            Box::pin(async move { Ok((0, Value::Nil)) })
        }

        fn emit<'a>(
            &'a self,
            _t: &'a str,
            _c: &'a str,
            _a: &'a Value,
        ) -> Pin<Box<dyn Future<Output = MixResult<()>> + 'a>> {
            Box::pin(async move { Ok(()) })
        }

        fn port_exists<'a>(
            &'a self,
            _t: &'a str,
        ) -> Pin<Box<dyn Future<Output = MixResult<bool>> + 'a>> {
            Box::pin(async move { Ok(false) })
        }

        fn next_incoming<'a>(
            &'a self,
        ) -> Pin<Box<dyn Future<Output = Option<IncomingEvent>> + 'a>> {
            Box::pin(async move {
                if *self.closed.borrow() {
                    return None;
                }
                let mut rx = match self.rx.borrow_mut().take() {
                    Some(r) => r,
                    None => return None,
                };
                let result = rx.recv().await;
                *self.rx.borrow_mut() = Some(rx);
                if result.is_none() {
                    *self.closed.borrow_mut() = true;
                }
                result
            })
        }
    }

    let (tx, rx) = mpsc::unbounded_channel::<IncomingEvent>();

    let source = r#"
$trace = ""
$active = 0
$max_active = 0

on test.msg
    $active = $active + 1
    if $active > $max_active then $max_active = $active end
    $trace = $trace .. $event.body
    sleep(0.05)
    $active = $active - 1
done

sleep(0.3)
"#;
    let mut eval = Evaluator::new();
    eval.set_bus_handler(Rc::new(ChannelHandler::new(rx)));

    // Both events buffered before execute. The C.7d serialization
    // sequence (see the long block comment above; NOT a reentrant
    // select! drain — that arm was retired by C.7d): main body
    // sleep(0.3) enters the select! yield path with no admission
    // held → drains event 1 → dispatch_event acquires writer →
    // handler 1 runs → handler 1's sleep(0.05) is gated by
    // is_dispatching() → plain timer (no inner select!) →
    // handler 1 exits → writer releases → main-body sleep loop
    // re-enters select! → drains event 2 → handler 2 runs the same
    // way. Two sequential dispatches under one writer each — not
    // one reentrant drain.
    tx.send(mk_event("test.msg", "1", &[])).unwrap();
    tx.send(mk_event("test.msg", "2", &[])).unwrap();
    drop(tx);

    let mut lexer = Lexer::new(source);
    let stmts = Parser::new(lexer.tokenize().unwrap(), source)
        .parse_program()
        .unwrap();
    eval.execute(&stmts).await.unwrap();

    assert_eq!(
        eval.get_global("trace").unwrap().to_mix_string(),
        "12",
        "events should process in FIFO order"
    );
    assert_eq!(
        eval.get_global("max_active").unwrap().to_number().unwrap() as i64,
        1,
        "SERIALIZATION INVARIANT: at most one Class S handler body \
         active at a time. If this assertion fails, dispatch_event's \
         writer-permit admission (acquire_writer_dispatch) is broken \
         and a second dispatch ran inside the first's writer-held \
         window — the exact hazard the C.7d writer permit prevents."
    );
    assert_eq!(
        eval.get_global("active").unwrap().to_number().unwrap() as i64,
        0,
        "counter should balance to 0 after both handlers complete"
    );
    assert!(!eval.is_dispatching());
}

#[tokio::test(flavor = "current_thread")]
async fn test_event_pump_drains_then_exits_on_transport_close() {
    // Auto-pump lifecycle: pump enters, drains all buffered events,
    // exits when next_incoming returns None (transport closed). This
    // is the end-to-end path for handler-only scripts that register
    // handlers and fall off the end of their main body.
    //
    // Invariants verified:
    //   - all N buffered events are dispatched (none lost to early exit)
    //   - pump exits cleanly when transport closes (no infinite spin)
    //   - handlers fire in FIFO order (same guarantee as the property test
    //     but through the pump entry point rather than sleep's select!)
    use cosmix_mix::error::MixResult;
    use cosmix_mix::evaluator::BusHandler;
    use cosmix_mix::value::Value;
    use std::cell::RefCell;
    use std::future::Future;
    use std::pin::Pin;
    use tokio::sync::mpsc;

    struct ChannelHandler {
        rx: RefCell<Option<mpsc::UnboundedReceiver<IncomingEvent>>>,
        closed: RefCell<bool>,
    }
    impl ChannelHandler {
        fn new(rx: mpsc::UnboundedReceiver<IncomingEvent>) -> Self {
            ChannelHandler {
                rx: RefCell::new(Some(rx)),
                closed: RefCell::new(false),
            }
        }
    }
    impl BusHandler for ChannelHandler {
        fn send<'a>(
            &'a self,
            _: &'a str,
            _: &'a str,
            _: &'a Value,
        ) -> Pin<Box<dyn Future<Output = MixResult<(i32, Value)>> + 'a>> {
            Box::pin(async move { Ok((0, Value::Nil)) })
        }
        fn emit<'a>(
            &'a self,
            _: &'a str,
            _: &'a str,
            _: &'a Value,
        ) -> Pin<Box<dyn Future<Output = MixResult<()>> + 'a>> {
            Box::pin(async move { Ok(()) })
        }
        fn port_exists<'a>(
            &'a self,
            _: &'a str,
        ) -> Pin<Box<dyn Future<Output = MixResult<bool>> + 'a>> {
            Box::pin(async move { Ok(false) })
        }
        fn next_incoming<'a>(
            &'a self,
        ) -> Pin<Box<dyn Future<Output = Option<IncomingEvent>> + 'a>> {
            Box::pin(async move {
                if *self.closed.borrow() {
                    return None;
                }
                let mut rx = match self.rx.borrow_mut().take() {
                    Some(r) => r,
                    None => return None,
                };
                let result = rx.recv().await;
                *self.rx.borrow_mut() = Some(rx);
                if result.is_none() {
                    *self.closed.borrow_mut() = true;
                }
                result
            })
        }
    }

    let (tx, rx) = mpsc::unbounded_channel::<IncomingEvent>();

    let source = r#"
$trace = ""
on test.msg
    $trace = $trace .. $event.body
done
"#;
    let mut eval = Evaluator::new();
    eval.set_bus_handler(Rc::new(ChannelHandler::new(rx)));

    // Register the handler, then push 3 events and close the channel.
    let mut lexer = Lexer::new(source);
    let stmts = Parser::new(lexer.tokenize().unwrap(), source)
        .parse_program()
        .unwrap();
    eval.execute(&stmts).await.unwrap();
    assert_eq!(eval.handler_count(), 1);

    tx.send(mk_event("test.msg", "A", &[])).unwrap();
    tx.send(mk_event("test.msg", "B", &[])).unwrap();
    tx.send(mk_event("test.msg", "C", &[])).unwrap();
    drop(tx); // transport_closed will fire after drain

    // Enter the pump. Drains all 3 events, then next_incoming returns
    // None (channel empty + sender dropped), pump exits cleanly.
    eval.run_event_pump().await.unwrap();

    assert_eq!(
        eval.get_global("trace").unwrap().to_mix_string(),
        "ABC",
        "all 3 events should dispatch in FIFO order through the pump"
    );
    assert!(!eval.is_dispatching());
}

#[tokio::test(flavor = "current_thread")]
async fn test_event_pump_respects_interrupt_flag() {
    // Ctrl-C path: pump checks `self.globals.interrupted` at the top
    // of each iteration and exits cleanly. Verifies the pump composes
    // with the existing interrupt machinery instead of needing its
    // own signal handler.
    use cosmix_mix::error::MixResult;
    use cosmix_mix::evaluator::BusHandler;
    use cosmix_mix::value::Value;
    use std::cell::RefCell;
    use std::future::Future;
    use std::pin::Pin;
    use std::sync::atomic::Ordering;
    use tokio::sync::mpsc;

    struct ChannelHandler {
        rx: RefCell<Option<mpsc::UnboundedReceiver<IncomingEvent>>>,
    }
    impl BusHandler for ChannelHandler {
        fn send<'a>(
            &'a self,
            _: &'a str,
            _: &'a str,
            _: &'a Value,
        ) -> Pin<Box<dyn Future<Output = MixResult<(i32, Value)>> + 'a>> {
            Box::pin(async move { Ok((0, Value::Nil)) })
        }
        fn emit<'a>(
            &'a self,
            _: &'a str,
            _: &'a str,
            _: &'a Value,
        ) -> Pin<Box<dyn Future<Output = MixResult<()>> + 'a>> {
            Box::pin(async move { Ok(()) })
        }
        fn port_exists<'a>(
            &'a self,
            _: &'a str,
        ) -> Pin<Box<dyn Future<Output = MixResult<bool>> + 'a>> {
            Box::pin(async move { Ok(false) })
        }
        fn next_incoming<'a>(
            &'a self,
        ) -> Pin<Box<dyn Future<Output = Option<IncomingEvent>> + 'a>> {
            Box::pin(async move {
                let mut rx = self.rx.borrow_mut().take()?;
                let result = rx.recv().await;
                *self.rx.borrow_mut() = Some(rx);
                result
            })
        }
    }

    let (tx, rx) = mpsc::unbounded_channel::<IncomingEvent>();

    let source = r#"
$count = 0
on test.msg
    $count = $count + 1
done
"#;
    let mut eval = Evaluator::new();
    eval.set_bus_handler(Rc::new(ChannelHandler {
        rx: RefCell::new(Some(rx)),
    }));

    let mut lexer = Lexer::new(source);
    let stmts = Parser::new(lexer.tokenize().unwrap(), source)
        .parse_program()
        .unwrap();
    eval.execute(&stmts).await.unwrap();

    // Trip the interrupt flag BEFORE entering the pump. The pump should
    // observe it on the first iteration and exit before blocking on
    // next_incoming — so the channel staying open indefinitely is fine.
    eval.interrupt_flag().store(true, Ordering::Relaxed);
    tx.send(mk_event("test.msg", "1", &[])).unwrap();
    // Do NOT drop(tx) — if the pump ignored the interrupt, it would
    // block here forever waiting for more events.

    eval.run_event_pump().await.unwrap();

    // The interrupt fires BEFORE next_incoming is awaited, so no events
    // are dispatched. $count stays at 0.
    assert_eq!(
        eval.get_global("count").unwrap().to_number().unwrap() as i64,
        0,
        "pump should exit on interrupt before dispatching any events"
    );
}

#[tokio::test(flavor = "current_thread")]
#[cfg_attr(not(feature = "tokio-sleep"), ignore = "requires tokio-sleep feature")]
async fn test_sleep_without_handlers_uses_fast_path() {
    // Regression guard: a script with no registered handlers must take
    // the fast path (plain tokio::time::sleep, no yield-point machinery).
    // This test doesn't observe the fast path directly — it just confirms
    // that a no-handler sleep completes without needing an bus_handler
    // installed at all, which is only true on the fast path.
    let source = r#"sleep(0.02)"#;
    let mut eval = Evaluator::new();
    // NO bus_handler set — if the yield-point loop were hit, the
    // `self.globals.bus_handler.is_none()` check in the fast path
    // would still route us to plain sleep, but if that check were
    // removed in a refactor, this test would panic via the expect()
    // in the select! borrow scope. Regression guard against "unify
    // the paths" refactors.
    let mut lexer = Lexer::new(source);
    let stmts = Parser::new(lexer.tokenize().unwrap(), source)
        .parse_program()
        .unwrap();
    eval.execute(&stmts).await.unwrap();
    // If we got here, fast path worked.
}

#[tokio::test]
async fn run_failure_throws_die_caught_by_try_catch() {
    let src = r#"
try
    $x = run("false")
    print "should not reach"
catch $e
    print "caught: " .. $e
end
"#;
    let out = run_mix_capturing(src)
        .await
        .expect("die must not propagate");
    assert!(out.contains("caught:"), "expected caught in {out:?}");
    assert!(out.contains("rc=1"), "expected rc=1 in {out:?}");
    assert!(
        !out.contains("should not reach"),
        "die should skip post-call code: {out:?}"
    );
}

// ── SPEC 18 WS2: subscribe()/unsubscribe() builtins ──

use cosmix_mix::error::MixResult;
// `IncomingEvent` is already imported at module scope (used by `mk_event`);
// the `inert_bus_stubs!` macro below picks it up from there.
use cosmix_mix::evaluator::BusHandler;
use cosmix_mix::value::Value;
use std::cell::RefCell;
use std::future::Future;
use std::pin::Pin;
use std::rc::Rc;

/// Shared `(verb, topic)` log. The evaluator is single-threaded
/// (`!Send`, Rc/RefCell), so an `Rc<RefCell<…>>` is the idiomatic way
/// to keep an assertion handle while the box is owned by the evaluator.
type CallLog = Rc<RefCell<Vec<(String, String)>>>;

/// Records every (un)subscribe; all other Bus ops are inert stubs.
struct RecordingHandler(CallLog);
/// Overrides nothing topic-related — exercises the trait defaults.
struct BareHandler;

macro_rules! inert_bus_stubs {
    () => {
        fn send<'a>(
            &'a self,
            _t: &'a str,
            _c: &'a str,
            _a: &'a Value,
        ) -> Pin<Box<dyn Future<Output = MixResult<(i32, Value)>> + 'a>> {
            Box::pin(async { Ok((0, Value::Nil)) })
        }
        fn emit<'a>(
            &'a self,
            _t: &'a str,
            _c: &'a str,
            _a: &'a Value,
        ) -> Pin<Box<dyn Future<Output = MixResult<()>> + 'a>> {
            Box::pin(async { Ok(()) })
        }
        fn port_exists<'a>(
            &'a self,
            _t: &'a str,
        ) -> Pin<Box<dyn Future<Output = MixResult<bool>> + 'a>> {
            Box::pin(async { Ok(false) })
        }
        fn next_incoming<'a>(
            &'a self,
        ) -> Pin<Box<dyn Future<Output = Option<IncomingEvent>> + 'a>> {
            Box::pin(async { None })
        }
    };
}

impl BusHandler for RecordingHandler {
    inert_bus_stubs!();
    fn subscribe_topic<'a>(
        &'a self,
        name: &'a str,
    ) -> Pin<Box<dyn Future<Output = MixResult<()>> + 'a>> {
        self.0
            .borrow_mut()
            .push(("sub".to_string(), name.to_string()));
        Box::pin(async { Ok(()) })
    }
    fn unsubscribe_topic<'a>(
        &'a self,
        name: &'a str,
    ) -> Pin<Box<dyn Future<Output = MixResult<()>> + 'a>> {
        self.0
            .borrow_mut()
            .push(("unsub".to_string(), name.to_string()));
        Box::pin(async { Ok(()) })
    }
}

impl BusHandler for BareHandler {
    inert_bus_stubs!();
}

async fn run_with_handler(source: &str, handler: Rc<dyn BusHandler>) -> Result<(), String> {
    let mut lexer = Lexer::new(source);
    let tokens = lexer.tokenize().map_err(|e| e.to_string())?;
    let mut parser = Parser::new(tokens, source);
    let stmts = parser.parse_program().map_err(|e| e.to_string())?;
    let mut eval = Evaluator::with_output(Box::new(SharedBuf::new()), Box::new(SharedBuf::new()));
    eval.set_bus_handler(handler);
    eval.execute(&stmts)
        .await
        .map(|_| ())
        .map_err(|e| e.to_string())
}

/// The `subscribe(name)` / `unsubscribe(name)` builtins dispatch through
/// the one `BusHandler` chokepoint, in call order, with the topic name.
#[tokio::test]
async fn subscribe_unsubscribe_builtins_dispatch_through_handler() {
    let log: CallLog = Rc::new(RefCell::new(Vec::new()));
    run_with_handler(
        "subscribe(\"world.a\")\nsubscribe(\"world.b\")\nunsubscribe(\"world.a\")\n",
        Rc::new(RecordingHandler(log.clone())),
    )
    .await
    .expect("script runs");

    assert_eq!(
        *log.borrow(),
        vec![
            ("sub".to_string(), "world.a".to_string()),
            ("sub".to_string(), "world.b".to_string()),
            ("unsub".to_string(), "world.a".to_string()),
        ],
        "builtins must reach the handler in call order with the topic name"
    );
}

/// No handler → hard error. A silently-dropped subscribe is the
/// partial-truth bug WS2 exists to prevent.
#[tokio::test]
async fn subscribe_without_handler_errors() {
    let err = run_mix_capturing("subscribe(\"x\")")
        .await
        .expect_err("subscribe without a Bus handler must error");
    assert!(
        err.contains("Bus not available"),
        "expected no-handler error, got {err:?}"
    );
}

/// An empty topic name is a hard error, not a no-op subscribe.
#[tokio::test]
async fn subscribe_empty_topic_errors() {
    let err = run_mix_capturing("unsubscribe(\"\")")
        .await
        .expect_err("empty topic must error");
    assert!(
        err.contains("non-empty topic"),
        "expected empty-name error, got {err:?}"
    );
}

/// The `BusHandler` trait default is a hard error, never a silent `Ok`
/// — a handler that forgets to wire topics fails loudly (the
/// `feedback_refactor_silent_noop_audit` guard at the trait layer).
#[tokio::test]
async fn default_subscribe_topic_impl_errors_not_silent_ok() {
    let err = run_with_handler("subscribe(\"x\")", Rc::new(BareHandler))
        .await
        .expect_err("default subscribe_topic must error, not silently Ok");
    assert!(
        err.contains("requires a Bus connection"),
        "default impl must be a hard error, got {err:?}"
    );
}

// ── SPEC 18 WS-R: reply() builtin ──

/// One recorded `reply` call: (to, command, id, rc, body).
type ReplyCall = (String, String, Option<String>, u8, String);
type ReplyLog = Rc<RefCell<Vec<ReplyCall>>>;

/// Records every `reply`; all other Bus ops are inert stubs. Proves the
/// evaluator forwards the exact correlation parts it captured off the
/// in-flight event.
struct ReplyRecorder(ReplyLog);

impl BusHandler for ReplyRecorder {
    inert_bus_stubs!();
    fn reply<'a>(
        &'a self,
        to: &'a str,
        command: &'a str,
        id: Option<&'a str>,
        rc: u8,
        body: &'a str,
    ) -> Pin<Box<dyn Future<Output = MixResult<()>> + 'a>> {
        self.0.borrow_mut().push((
            to.to_string(),
            command.to_string(),
            id.map(|s| s.to_string()),
            rc,
            body.to_string(),
        ));
        Box::pin(async { Ok(()) })
    }
}

/// Parse + execute `source` (registering its `on` handlers), then
/// dispatch one event, returning captured stdout. `handler` is `None`
/// to exercise the "Bus not available" path.
async fn run_then_dispatch(
    source: &str,
    handler: Option<Rc<dyn BusHandler>>,
    event: IncomingEvent,
) -> String {
    let mut lexer = Lexer::new(source);
    let stmts = Parser::new(lexer.tokenize().unwrap(), source)
        .parse_program()
        .unwrap();
    let stdout = SharedBuf::new();
    let mut eval = Evaluator::with_output(Box::new(stdout.clone()), Box::new(SharedBuf::new()));
    if let Some(h) = handler {
        eval.set_bus_handler(h);
    }
    eval.execute(&stmts).await.expect("main body runs");
    eval.dispatch_event(event).await.expect("dispatch is soft");
    stdout.to_string_lossy()
}

/// The happy path: `reply(body)` inside an `on` handler forwards the
/// in-flight request's `from`/`command`/`id` plus rc=0 and the body —
/// no correlation invented, exactly the parts captured off the event.
#[tokio::test]
async fn reply_inside_handler_routes_correlation_parts() {
    let log: ReplyLog = Rc::new(RefCell::new(Vec::new()));
    run_then_dispatch(
        "on statecache.get\n    reply(\"the-value\")\ndone\n",
        Some(Rc::new(ReplyRecorder(log.clone()))),
        mk_event(
            "statecache.get",
            "",
            &[("from", "caller"), ("id", "42"), ("type", "request")],
        ),
    )
    .await;
    assert_eq!(
        *log.borrow(),
        vec![(
            "caller".to_string(),
            "statecache.get".to_string(),
            Some("42".to_string()),
            0u8,
            "the-value".to_string(),
        )],
        "reply() must forward the captured correlation parts verbatim"
    );
}

/// SPEC 18 WS-R live BLOCKER regression (2026-05-16): a correlated
/// request whose `from` was stripped by noded's SPEC 12 §15.5
/// anonymization (the normal CLI / MCP-bridge caller — every Rust daemon
/// answers these fine, only Mix serve-mode citizens hung) must STILL be
/// answered. The transport correlates the response by `id`
/// (pending_responses), not by `from`; `reply()` therefore gates on
/// `type=request` alone and forwards an empty `to`, exactly as the Rust
/// `NodedClient::respond_parts` path does. Before the fix this raised
/// "reply() has no requester to answer" and the requester blocked 60s.
/// Every other WS-R test injects a `from` header, which is precisely
/// what masked this in review.
#[tokio::test]
async fn reply_answers_request_even_when_noded_stripped_from() {
    let log: ReplyLog = Rc::new(RefCell::new(Vec::new()));
    run_then_dispatch(
        "on statecache.get\n    reply(\"the-value\")\ndone\n",
        Some(Rc::new(ReplyRecorder(log.clone()))),
        // No `from` — exactly what reaches a local service after
        // canonicalize_routed_from drops it for an anonymous caller.
        mk_event("statecache.get", "", &[("id", "42"), ("type", "request")]),
    )
    .await;
    assert_eq!(
        *log.borrow(),
        vec![(
            String::new(),
            "statecache.get".to_string(),
            Some("42".to_string()),
            0u8,
            "the-value".to_string(),
        )],
        "reply() must answer an anonymized request with an empty `to` \
         and the `id` intact (transport correlates the reply by `id`)"
    );
}

/// `reply(rc, body)` passes a non-zero rc through (the §3.4 error-reply
/// shape WS6 builds on).
#[tokio::test]
async fn reply_two_arg_form_passes_rc() {
    let log: ReplyLog = Rc::new(RefCell::new(Vec::new()));
    run_then_dispatch(
        "on q\n    reply(7, \"boom\")\ndone\n",
        Some(Rc::new(ReplyRecorder(log.clone()))),
        mk_event("q", "", &[("from", "c"), ("id", "9"), ("type", "request")]),
    )
    .await;
    let calls = log.borrow();
    assert_eq!(calls.len(), 1);
    assert_eq!(calls[0].3, 7u8, "rc must pass through the 2-arg form");
    assert_eq!(calls[0].4, "boom");
}

/// An id-less request still replies (id omitted, not fabricated).
#[tokio::test]
async fn reply_without_inbound_id_omits_id() {
    let log: ReplyLog = Rc::new(RefCell::new(Vec::new()));
    run_then_dispatch(
        "on q\n    reply(\"v\")\ndone\n",
        Some(Rc::new(ReplyRecorder(log.clone()))),
        mk_event("q", "", &[("from", "c"), ("type", "request")]),
    )
    .await;
    assert_eq!(log.borrow()[0].2, None, "missing inbound id stays None");
}

/// Wrong arity is a deterministic hard error — and because argument
/// validation runs before the in-handler / Bus-present checks, it fires
/// even from the main body with no handler and no Bus.
#[tokio::test]
async fn reply_arity_error_is_deterministic() {
    let err = run_mix_capturing("reply(\"a\", \"b\", \"c\")")
        .await
        .expect_err("3-arg reply must error");
    assert!(
        err.contains("takes (body) or (rc, body)"),
        "expected arity error, got {err:?}"
    );
}

/// A non-integer / out-of-range rc in the 2-arg form is a hard error.
#[tokio::test]
async fn reply_bad_rc_error_is_deterministic() {
    let err = run_mix_capturing("reply(1.5, \"b\")")
        .await
        .expect_err("fractional rc must error");
    assert!(
        err.contains("rc must be an integer"),
        "expected rc-type error, got {err:?}"
    );
}

/// `reply()` outside any `on` handler is a hard error. The meta check
/// precedes the Bus-present check, so this is deterministic regardless
/// of whether Bus is wired (here it is not).
#[tokio::test]
async fn reply_outside_handler_errors() {
    let err = run_mix_capturing("reply(\"x\")")
        .await
        .expect_err("reply() in the main body must error");
    assert!(
        err.contains("can only be called from within an `on` handler"),
        "expected not-in-handler error, got {err:?}"
    );
}

/// `reply()` on a non-request delivery (a topic broadcast: no
/// `type=request`) is a hard error, and the handler's reply is NOT
/// forwarded — replying to a broadcast would fire a spurious response
/// at the publisher. Surfaced into output via try/catch since handler
/// errors are otherwise soft.
#[tokio::test]
async fn reply_on_non_request_event_errors() {
    let log: ReplyLog = Rc::new(RefCell::new(Vec::new()));
    let out = run_then_dispatch(
        "on world.tick\n    try\n        reply(\"nope\")\n        print \"REACHED\"\n    catch $e\n        print \"caught: \" .. $e\n    end\ndone\n",
        Some(Rc::new(ReplyRecorder(log.clone()))),
        mk_event("world.tick", "", &[("from", "pub"), ("type", "event")]),
    )
    .await;
    assert!(out.contains("caught:"), "error must be raised, got {out:?}");
    assert!(
        out.contains("not type=request"),
        "expected non-request guard message, got {out:?}"
    );
    assert!(!out.contains("REACHED"), "reply() must abort the body");
    assert!(
        log.borrow().is_empty(),
        "a non-request reply must not reach the handler"
    );
}

/// The `BusHandler::reply` trait default is a hard error, never a
/// silent `Ok` — a dropped reply is the acute partial-truth bug (the
/// requester blocks forever). `BareHandler` overrides nothing, so a
/// valid in-handler request still surfaces the loud default.
#[tokio::test]
async fn reply_default_trait_impl_errors_not_silent_ok() {
    let out = run_then_dispatch(
        "on q\n    try\n        reply(\"v\")\n        print \"REACHED\"\n    catch $e\n        print \"caught: \" .. $e\n    end\ndone\n",
        Some(Rc::new(BareHandler)),
        mk_event("q", "", &[("from", "c"), ("id", "1"), ("type", "request")]),
    )
    .await;
    assert!(
        out.contains("caught:") && out.contains("requires a Bus connection"),
        "default reply must be a hard error, got {out:?}"
    );
    assert!(
        !out.contains("REACHED"),
        "the failed reply must abort the body"
    );
}

/// In a handler with valid correlation but no Bus handler at all, the
/// distinct "Bus not available" error fires (the meta check passed; the
/// bus-present check is what trips). Distinct branch from
/// `reply_outside_handler_errors`.
#[tokio::test]
async fn reply_in_handler_without_bus_errors() {
    let out = run_then_dispatch(
        "on q\n    try\n        reply(\"v\")\n    catch $e\n        print \"caught: \" .. $e\n    end\ndone\n",
        None,
        mk_event("q", "", &[("from", "c"), ("id", "1"), ("type", "request")]),
    )
    .await;
    assert!(
        out.contains("caught:") && out.contains("Bus not available"),
        "expected Bus-not-available error, got {out:?}"
    );
}

// ── SPEC 18 §3.4: handler fault isolation (WS6) ──

/// Build an evaluator wired to a `ReplyRecorder`, register a `boom()`
/// extension that triggers a real Rust panic (the deterministic stand-in
/// for "an `unwrap` on a `nil` field / a builtin that panics"), run the
/// script body to register its `on` handlers, then dispatch each event
/// in order. Returns the recorded reply log. `dispatch_event` is
/// `.expect`ed to stay `Ok` — a panicking handler must NOT unwind
/// through the pump (that is the whole point of §3.4).
async fn dispatch_with_boom(source: &str, events: &[IncomingEvent]) -> Vec<ReplyCall> {
    use cosmix_mix::evaluator::sync_ext;
    let log: ReplyLog = Rc::new(RefCell::new(Vec::new()));
    let mut lexer = Lexer::new(source);
    let stmts = Parser::new(lexer.tokenize().unwrap(), source)
        .parse_program()
        .unwrap();
    let mut eval = Evaluator::with_output(Box::new(SharedBuf::new()), Box::new(SharedBuf::new()));
    eval.set_bus_handler(Rc::new(ReplyRecorder(log.clone())));
    eval.register(
        "boom",
        sync_ext(|_| panic!("handler boom (test-induced Rust panic)")),
    );
    eval.execute(&stmts).await.expect("main body runs");
    for ev in events {
        eval.dispatch_event(ev.clone())
            .await
            .expect("dispatch must stay Ok — a handler panic is isolated");
    }
    log.borrow().clone()
}

/// A Rust panic inside a request handler is caught, the daemon survives,
/// and the requester gets a synthetic non-zero error reply (rc=1, fixed
/// non-sensitive body) so it is not left blocked forever. The panic
/// detail never crosses the wire.
#[tokio::test]
async fn panic_in_request_handler_is_isolated_and_synthesizes_error_reply() {
    let replies = dispatch_with_boom(
        "on q\n    boom()\ndone\n",
        &[mk_event(
            "q",
            "",
            &[("from", "caller"), ("id", "7"), ("type", "request")],
        )],
    )
    .await;
    assert_eq!(
        replies,
        vec![(
            "caller".to_string(),
            "q".to_string(),
            Some("7".to_string()),
            1u8,
            "internal handler error".to_string(),
        )],
        "a panicking request handler must yield exactly one rc=1 \
         synthetic reply with a non-sensitive body"
    );
}

/// §3.4 companion to `reply_answers_request_even_when_noded_stripped_from`:
/// the synthetic error-reply for a panicking handler must also fire when
/// noded anonymized the caller (no `from`). Before the fix the synthetic
/// reply was gated on `!from.is_empty()`, so an anonymous requester whose
/// handler panicked blocked forever instead of getting rc=1.
#[tokio::test]
async fn panic_synthesizes_error_reply_even_when_noded_stripped_from() {
    let replies = dispatch_with_boom(
        "on q\n    boom()\ndone\n",
        &[mk_event("q", "", &[("id", "7"), ("type", "request")])],
    )
    .await;
    assert_eq!(
        replies,
        vec![(
            String::new(),
            "q".to_string(),
            Some("7".to_string()),
            1u8,
            "internal handler error".to_string(),
        )],
        "an anonymized (no-`from`) request whose handler panics must \
         still get the rc=1 synthetic reply, correlated by `id`"
    );
}

/// One malformed request must not deny service to all others: after a
/// handler panics on the first event, a subsequent unrelated request is
/// still dispatched and answered normally (proves the evaluator's
/// transient state — scope frames, function depth, var-slot caches — was
/// rewound to a clean baseline, not left corrupt).
#[tokio::test]
async fn panic_does_not_deny_service_to_subsequent_events() {
    let replies = dispatch_with_boom(
        "on q\n    boom()\ndone\non ok\n    reply(\"fine\")\ndone\n",
        &[
            mk_event("q", "", &[("from", "c1"), ("id", "1"), ("type", "request")]),
            mk_event(
                "ok",
                "",
                &[("from", "c2"), ("id", "2"), ("type", "request")],
            ),
        ],
    )
    .await;
    assert_eq!(
        replies,
        vec![
            (
                "c1".to_string(),
                "q".to_string(),
                Some("1".to_string()),
                1u8,
                "internal handler error".to_string(),
            ),
            (
                "c2".to_string(),
                "ok".to_string(),
                Some("2".to_string()),
                0u8,
                "fine".to_string(),
            ),
        ],
        "the post-panic event must be served normally"
    );
}

/// A handler that successfully `reply()`d and *then* panicked must not
/// double-reply: the recorded answer is the handler's own reply, and the
/// §3.4 synthetic error-reply is suppressed because the event is already
/// answered.
#[tokio::test]
async fn handler_that_replied_then_panics_does_not_double_reply() {
    let replies = dispatch_with_boom(
        "on q\n    reply(\"the-real-answer\")\n    boom()\ndone\n",
        &[mk_event(
            "q",
            "",
            &[("from", "caller"), ("id", "9"), ("type", "request")],
        )],
    )
    .await;
    assert_eq!(
        replies,
        vec![(
            "caller".to_string(),
            "q".to_string(),
            Some("9".to_string()),
            0u8,
            "the-real-answer".to_string(),
        )],
        "an already-answered event must not get a second synthetic reply"
    );
}

/// A panic while servicing a non-request delivery (a topic broadcast:
/// no `from`, `type` != request) is still isolated, but there is no
/// caller to answer — no synthetic reply is fired at the publisher.
#[tokio::test]
async fn panic_on_non_request_delivery_synthesizes_no_reply() {
    let replies = dispatch_with_boom(
        "on world.tick\n    boom()\ndone\n",
        &[mk_event("world.tick", "", &[("topic", "world.tick")])],
    )
    .await;
    assert!(
        replies.is_empty(),
        "a non-request delivery has no caller to answer, got {replies:?}"
    );
}

/// A panic *inside an `address` block* must not leak the address target:
/// the `address_stack` push has no matching pop on the unwind path, and
/// a stale entry would silently turn a later bare call into an
/// address-send. An `address` block desugars every line into a `Send`,
/// so the panic is induced from the handler's `send`. The §3.4 rewind
/// truncates `address_stack` to its pre-body floor, so the stack is
/// empty again and a subsequent request is still served normally.
#[tokio::test]
async fn panic_inside_address_block_does_not_leak_address_target() {
    /// Panics on `send` (the address-block primitive); records `reply`
    /// so the synthetic error-reply and continuity are still observable.
    struct PanicOnSend(ReplyLog);
    impl BusHandler for PanicOnSend {
        fn send<'a>(
            &'a self,
            _t: &'a str,
            _c: &'a str,
            _a: &'a Value,
        ) -> Pin<Box<dyn Future<Output = MixResult<(i32, Value)>> + 'a>> {
            Box::pin(async { panic!("send boom inside address block") })
        }
        fn emit<'a>(
            &'a self,
            _t: &'a str,
            _c: &'a str,
            _a: &'a Value,
        ) -> Pin<Box<dyn Future<Output = MixResult<()>> + 'a>> {
            Box::pin(async { Ok(()) })
        }
        fn port_exists<'a>(
            &'a self,
            _t: &'a str,
        ) -> Pin<Box<dyn Future<Output = MixResult<bool>> + 'a>> {
            Box::pin(async { Ok(false) })
        }
        fn next_incoming<'a>(
            &'a self,
        ) -> Pin<Box<dyn Future<Output = Option<IncomingEvent>> + 'a>> {
            Box::pin(async { None })
        }
        fn reply<'a>(
            &'a self,
            to: &'a str,
            command: &'a str,
            id: Option<&'a str>,
            rc: u8,
            body: &'a str,
        ) -> Pin<Box<dyn Future<Output = MixResult<()>> + 'a>> {
            self.0.borrow_mut().push((
                to.to_string(),
                command.to_string(),
                id.map(|s| s.to_string()),
                rc,
                body.to_string(),
            ));
            Box::pin(async { Ok(()) })
        }
    }

    let log: ReplyLog = Rc::new(RefCell::new(Vec::new()));
    let source = "on q\n    address \"ghost\"\n        trigger\n    end\ndone\n\
                  on ok\n    reply(\"clean\")\ndone\n";
    let mut lexer = Lexer::new(source);
    let stmts = Parser::new(lexer.tokenize().unwrap(), source)
        .parse_program()
        .unwrap();
    let mut eval = Evaluator::with_output(Box::new(SharedBuf::new()), Box::new(SharedBuf::new()));
    eval.set_bus_handler(Rc::new(PanicOnSend(log.clone())));
    eval.execute(&stmts).await.expect("main body runs");
    eval.dispatch_event(mk_event(
        "q",
        "",
        &[("from", "c1"), ("id", "1"), ("type", "request")],
    ))
    .await
    .expect("dispatch stays Ok");
    assert!(
        eval.address_stack().is_empty(),
        "the address target must not survive a panic inside the block, \
         got {:?}",
        eval.address_stack()
    );
    eval.dispatch_event(mk_event(
        "ok",
        "",
        &[("from", "c2"), ("id", "2"), ("type", "request")],
    ))
    .await
    .expect("dispatch stays Ok");
    assert_eq!(
        *log.borrow(),
        vec![
            (
                "c1".to_string(),
                "q".to_string(),
                Some("1".to_string()),
                1u8,
                "internal handler error".to_string(),
            ),
            (
                "c2".to_string(),
                "ok".to_string(),
                Some("2".to_string()),
                0u8,
                "clean".to_string(),
            ),
        ],
        "post-address-panic service must continue normally"
    );
}

/// `die` (and any uncaught `MixError`) on a request now also synthesizes
/// the §3.4 error-reply — previously such a fault was logged but the
/// requester was left blocked. The clean-error path and the panic path
/// converge on the same caller-visible contract.
#[tokio::test]
async fn die_in_request_handler_synthesizes_error_reply() {
    let replies = dispatch_with_boom(
        "on q\n    die \"intentional\"\ndone\n",
        &[mk_event(
            "q",
            "",
            &[("from", "caller"), ("id", "3"), ("type", "request")],
        )],
    )
    .await;
    assert_eq!(
        replies,
        vec![(
            "caller".to_string(),
            "q".to_string(),
            Some("3".to_string()),
            1u8,
            "internal handler error".to_string(),
        )],
        "an uncaught die on a request must surface an error reply"
    );
}

/// A handler that catches its own `die` with `try/catch` and completes
/// normally has NOT faulted — no synthetic error-reply is fired (the
/// fault boundary must not fire on a body that recovered).
#[tokio::test]
async fn caught_die_in_request_handler_does_not_synthesize_reply() {
    let replies = dispatch_with_boom(
        "on q\n    try\n        die \"x\"\n    catch $e\n    end\ndone\n",
        &[mk_event(
            "q",
            "",
            &[("from", "caller"), ("id", "4"), ("type", "request")],
        )],
    )
    .await;
    assert!(
        replies.is_empty(),
        "a handler that recovered from its own die has not faulted, \
         got {replies:?}"
    );
}

/// Regression: reading an undefined sigil-var inside an `if` condition
/// from a function body used to panic with `is_stmt_sync verified` (a
/// disagreement between the `is_expr_sync` predicate, which treated
/// `Expr::Variable(_)` as always-sync, and `try_eval_expr_sync`, which
/// returned `None` for unbound names). The async `eval_expr` arm
/// raises `RuntimeError("undefined variable '$X'")`; the sync arm
/// must do the same so the contract holds.
#[tokio::test]
async fn undefined_sigil_in_function_if_does_not_panic() {
    let err = run_mix_capturing(
        "function f()\n    if $X == nil then $X = 1 end\n    return $X\nend\n$y = f()\n",
    )
    .await
    .expect_err("reading $X inside f() must raise a runtime error, not panic");
    assert!(
        err.contains("undefined variable") && err.contains("$X"),
        "expected 'undefined variable $X' error, got {err:?}"
    );
}

/// Companion to the above: piped REPL input that triggers the same
/// sync-prefix path used to panic with `is_stmt_sync verified`. Here
/// we exercise the simpler `$X == nil` at top-level (the
/// `cargo run -- < script.mix` case from Mark's symptom report).
#[tokio::test]
async fn undefined_sigil_at_top_level_does_not_panic() {
    let err = run_mix_capturing("if $UNSET == nil then print \"hi\" end\n")
        .await
        .expect_err("reading $UNSET must raise a runtime error, not panic");
    assert!(
        err.contains("undefined variable") && err.contains("$UNSET"),
        "expected 'undefined variable $UNSET' error, got {err:?}"
    );
}

/// Companion positive case: once the variable has been assigned (even
/// to nil), the defensive `if $X == nil then $X = ... end` idiom works
/// as users expect — both at top level and inside a function body.
#[tokio::test]
async fn declared_nil_sigil_defensive_idiom_works() {
    let out = run_mix_capturing(
        "$COLOR = nil\nfunction p()\n    if $COLOR == nil then $COLOR = \"32\" end\n    return $COLOR\nend\nprint p()\n",
    )
    .await
    .expect("declared-nil sigil with defensive idiom must succeed");
    assert!(
        out.trim() == "32",
        "expected '32' from defensive default, got {out:?}"
    );
}

/// Loop fast-path prebinding must not silently bind unbound RHS
/// variables as `Nil`. Prior to the `try_global_slot_ptr` switch the
/// For-loop sync fast path called `Scope::global_slot_ptr` for every
/// RHS reference collected by `collect_variable_refs`, which inserts
/// a fresh `Value::Nil` for missing names. That masked the canonical
/// "undefined variable" RuntimeError introduced for `Expr::Variable`
/// in the sync fast path itself — the loop body would read the
/// synthetic Nil instead of erroring.
#[tokio::test]
async fn loop_body_unbound_rhs_var_errors() {
    let err = run_mix_capturing("for $i = 1 to 1\n    $x = $UNSET\nend\n")
        .await
        .expect_err("reading $UNSET in a loop body must raise a runtime error");
    assert!(
        err.contains("undefined variable") && err.contains("$UNSET"),
        "expected 'undefined variable $UNSET' error, got {err:?}"
    );
}

/// Companion regression for the `for each` all-numeric `take_fast`
/// branch — a fifth loop prebinding site that called the inserting
/// `global_slot_ptr` for RHS reads. Without the matching fix in
/// `try_eval_expr_num` the inner closure would also silently exit as
/// `Ok(())` on an unbound name (because the cache miss fell through
/// to `scope.get(name)?` returning `None`), making the outer loop
/// return `Ok(Value::Nil)` without ever raising the error.
#[tokio::test]
async fn for_each_take_fast_unbound_rhs_var_errors() {
    let err = run_mix_capturing(
        "$total = 0\nfor each $x in [1, 2, 3]\n    $total = $total + $UNSET\nend\n",
    )
    .await
    .expect_err("reading $UNSET in for-each body must raise a runtime error");
    assert!(
        err.contains("undefined variable") && err.contains("$UNSET"),
        "expected 'undefined variable $UNSET' error, got {err:?}"
    );
}

/// A `for each` loop variable inside a function must bind LOCALLY
/// (shadow), never clobber a same-named variable in the caller's
/// scope. Before the fix the generic ForEach loop used
/// `update_or_set`, whose globals-fallback overwrote a same-named
/// global — so a lib function's `for each $p in …` would trash the
/// caller's `$p` (e.g. a CMS handler's post map became a loop item).
/// The fast paths are `!in_function` guarded, so the generic loop
/// owns the in-function case and must mirror `StmtKind::Assignment`'s
/// `function_depth > 0` → `set_in_current` rule.
#[tokio::test]
async fn for_each_loop_var_in_function_does_not_clobber_caller() {
    let out = run_mix_capturing(
        "function f()\n  for each $p in [1, 2, 3]\n  end\nend\n\
         $p = { a: 7 }\nf()\nprint(\"\" .. $p[\"a\"])\n",
    )
    .await
    .expect("caller $p must survive a function's for-each loop var");
    assert_eq!(out.trim(), "7", "caller $p was clobbered by the loop var");
}

/// Nested function calls (each with its own `for each` accumulator)
/// must not clobber each other's or the caller's same-named vars —
/// the render_body→mdbold shape that surfaced the bug.
#[tokio::test]
async fn for_each_nested_function_loop_vars_are_isolated() {
    let out = run_mix_capturing(
        "function inner($s)\n  $out = \"\"\n  for each $w in [\"x\", \"y\"]\n    $out = $out .. $w\n  end\n  return $out .. \":\" .. $s\nend\n\
         function outer($parts)\n  $out = \"\"\n  for each $p in $parts\n    $out = $out .. inner($p) .. \"|\"\n  end\n  return $out\nend\n\
         $p = { title: \"T\" }\nprint(outer([\"a\", \"b\"]))\nprint($p[\"title\"])\n",
    )
    .await
    .expect("nested function loop vars must be isolated");
    assert_eq!(out.trim(), "xy:a|xy:b|\nT");
}

// ── self-concat assignment fast path (`$s = $s .. rhs` in-place append) ──

#[tokio::test]
async fn self_concat_in_function_does_not_mutate_outer_binding() {
    // The fast path must NOT mutate an outer `$s` in place: function
    // assignment shadows into the current frame (set_in_current).
    let out = run_mix_capturing(
        "function append_local()\n  $s = $s .. \"x\"\n  return $s\nend\n\
         $s = \"outer\"\nprint(append_local())\nprint($s)\n",
    )
    .await
    .expect("function self-concat must shadow, not mutate outer");
    assert_eq!(out.trim(), "outerx\nouter");
}

#[tokio::test]
async fn self_concat_for_each_function_accumulator() {
    // The datatable pattern: a function-local for-each accumulator with the
    // exact `$out = $out .. $part` shape hits the fast path each iteration. The
    // final `$out = $out .. upper($out)` stays on the GENERIC path (is_expr_sync
    // rejects the function call) — included to prove the two paths agree and
    // that a target-reading RHS reads the pre-append value.
    let out = run_mix_capturing(
        "function render($parts)\n  $out = \"\"\n  for each $part in $parts\n    $out = $out .. $part\n  end\n  $out = $out .. upper($out)\n  return $out\nend\n\
         print(render([\"a\", \"b\", \"c\"]))\n",
    )
    .await
    .expect("function-local for-each accumulator must work");
    assert_eq!(out.trim(), "abcABC");
}

#[tokio::test]
async fn self_concat_non_string_first_assignment_uses_generic_coercion() {
    // Target not yet a String → generic path (coerces 1 → "1").
    let out = run_mix_capturing("$s = 1\n$s = $s .. \"a\"\n$s = $s .. \"b\"\nprint($s)\n")
        .await
        .expect("non-string first assignment must retain generic concat semantics");
    assert_eq!(out.trim(), "1ab");
}

/// Same class as the for-each fix, for the numeric `for $i = a to b`
/// loop var: inside a function the counter must shadow locally, not
/// clobber a same-named caller/global var.
#[tokio::test]
async fn numeric_for_loop_var_in_function_does_not_clobber_caller() {
    let out = run_mix_capturing(
        "function g()\n  for $i = 1 to 3\n  end\nend\n\
         $i = { tag: \"keep\" }\ng()\nprint($i[\"tag\"])\n",
    )
    .await
    .expect("caller $i must survive a function's numeric for loop var");
    assert_eq!(
        out.trim(),
        "keep",
        "caller $i was clobbered by the loop counter"
    );
}

/// Ordering operators (`<` `>` `<=` `>=`) compare two strings
/// lexicographically (by Unicode codepoint) instead of erroring, while
/// preserving numeric comparison for numbers and numeric strings. Added
/// 0.15.6 — slugify's `$ch >= "a" and $ch <= "z"` char-range test used
/// to raise "cannot compare 't' as number".
#[tokio::test]
async fn string_ordering_comparison() {
    let cases = [
        ("\"t\" >= \"a\"", "true"),
        ("\"t\" <= \"z\"", "true"),
        ("\"apple\" < \"banana\"", "true"),
        ("\"banana\" < \"apple\"", "false"),
        ("\"abc\" <= \"abc\"", "true"),
        // numeric comparison preserved (NOT lexicographic):
        ("5 < 10", "true"),
        ("\"5\" < \"10\"", "true"),
    ];
    for (expr, want) in cases {
        let out = run_mix_capturing(&format!("print(\"\" .. ({}))\n", expr))
            .await
            .unwrap_or_else(|e| panic!("{expr} errored: {e}"));
        assert_eq!(out.trim(), want, "{expr}");
    }
}

/// The slugify char-range idiom (the live CMS regression) must work.
#[tokio::test]
async fn string_char_range_in_condition() {
    let out = run_mix_capturing(
        "$ch = \"t\"\nif ($ch >= \"a\" and $ch <= \"z\") or ($ch >= \"0\" and $ch <= \"9\") then\n  print(\"alnum\")\nelse\n  print(\"other\")\nend\n",
    )
    .await
    .expect("char-range comparison works");
    assert_eq!(out.trim(), "alnum");
}

/// A `return` inside a TOP-LEVEL control-flow block must terminate the
/// program (propagate to the invocation root), not be swallowed. The
/// nested block runs via `execute_block` so its `Return` propagates;
/// only the root unwraps it. Before the fix, the nested block ran via
/// the public `execute`, which unwraps `Return` at `function_depth==0`,
/// so the `return` produced `Ok(value)` and execution wrongly continued
/// past the block (the webd `if $cond then return {..} end`
/// fall-through). The map literal forces the async exec_stmt path (the
/// one that had the bug; the sync path always propagated correctly).
#[tokio::test]
async fn return_inside_top_level_if_terminates_program() {
    let out = run_mix_capturing(
        "print(\"start\")\nif true then\n  $m = { x: 1 }\n  print(\"in-if\")\n  return $m\nend\nprint(\"AFTER-IF-must-not-print\")\n",
    )
    .await
    .expect("runs");
    assert_eq!(
        out.trim(),
        "start\nin-if",
        "return inside a top-level if was swallowed: {out:?}"
    );
}

/// Same for a `return` inside a top-level loop body.
#[tokio::test]
async fn return_inside_top_level_loop_terminates_program() {
    let out = run_mix_capturing(
        "for each $x in [1, 2, 3]\n  print(\"iter \" .. (\"\" .. $x))\n  $m = { v: $x }\n  return $m\nend\nprint(\"AFTER-LOOP-must-not-print\")\n",
    )
    .await
    .expect("runs");
    assert_eq!(
        out.trim(),
        "iter 1",
        "return inside a top-level loop was swallowed: {out:?}"
    );
}

/// Same binder class: `catch $e` inside a function must bind locally,
/// not clobber a same-named caller/global var. Codex flagged this as
/// the third site of the loop-var fix's class.
#[tokio::test]
async fn catch_var_in_function_does_not_clobber_caller() {
    let out = run_mix_capturing(
        "function h()\n  try\n    die(\"boom\")\n  catch $e\n  end\nend\n\
         $e = { kind: \"keep\" }\nh()\nprint($e[\"kind\"])\n",
    )
    .await
    .expect("caller $e must survive a function's catch binder");
    assert_eq!(out.trim(), "keep", "caller $e was clobbered by catch-var");
}

/// Same binder class: `parse … with $var` inside a function must bind
/// locally, not clobber a same-named caller/global. Codex flagged this
/// as the fourth site of the loop-var fix's class.
#[tokio::test]
async fn parse_with_var_in_function_does_not_clobber_caller() {
    let out = run_mix_capturing(
        "function pf()\n  parse \"a-b\" with $x \"-\" $y\n  return $x .. $y\nend\n\
         $x = { keep: 1 }\nprint(pf())\nprint(\"\" .. $x[\"keep\"])\n",
    )
    .await
    .expect("caller $x must survive a function's parse binder");
    assert_eq!(out.trim(), "ab\n1", "caller $x was clobbered by parse-with");
}

// NOTE: the pipe `BinOp::Pipe` `$_` scratch register (evaluator.rs) was
// also routed through `bind_scoped` for class-completeness, but it is
// parser-unreachable dead code — the statement-level `|` always parses
// as the shell pipe (`StmtKind::PipeToExternal`) and no surface syntax
// emits `BinOp::Pipe`, so there is no Mix program that exercises it. The
// change is a harmless defensive guard; no regression test is possible.

/// And the top-level catch-var still binds the error message (unchanged).
#[tokio::test]
async fn catch_var_top_level_binds_message() {
    let out =
        run_mix_capturing("try\n  die(\"boom\")\ncatch $e\n  print(\"caught: \" .. $e)\nend\n")
            .await
            .expect("top-level catch binds the message");
    assert!(
        out.contains("caught:") && out.contains("boom"),
        "got {out:?}"
    );
}

/// Regression guard: a top-level `for each` loop var still persists
/// after the loop (it lives in the single global frame). The fix only
/// changes the in-function case, so this must be unaffected.
#[tokio::test]
async fn for_each_top_level_loop_var_persists() {
    let out = run_mix_capturing("for each $x in [10, 20, 30]\nend\nprint(\"\" .. $x)\n")
        .await
        .expect("top-level loop var persists");
    assert_eq!(out.trim(), "30");
}

/// Sync `index_value_clone` must mirror the async Index arm's
/// "cannot index <T> with <U>" error for non-indexable objects.
/// Before the helper was switched to `MixResult<Value>` the sync
/// path silently returned `Nil` for `$x[0]` when `$x` was a number,
/// diverging from the async path which errored.
#[tokio::test]
async fn indexing_non_indexable_bound_value_errors() {
    let err = run_mix_capturing("$x = 1\n$y = $x[0]\nprint $y\n")
        .await
        .expect_err("indexing a number must raise a runtime error");
    assert!(
        err.contains("cannot index") && err.contains("number"),
        "expected 'cannot index number with number' error, got {err:?}"
    );
}

#[tokio::test]
async fn test_source_bareword() {
    run_test_script("source_bareword.mix").await;
}

/// `source ~/foo` and `source ~` must build an `InterpolatedString`
/// with a leading `EnvVar("HOME")` part — same primitive
/// `lex_double_string` emits for `"~/foo"`, so runtime semantics
/// match the quoted form exactly. No filesystem touched.
#[test]
fn source_bareword_tilde_expansion_ast() {
    use cosmix_mix::ast::{Expr, Stmt, StmtKind};
    use cosmix_mix::token::StringPart;

    fn parse_one(src: &str) -> Stmt {
        let tokens = Lexer::new(src).tokenize().expect("lex");
        let mut p = Parser::new(tokens, src);
        let stmts = p.parse_program().expect("parse");
        assert_eq!(stmts.len(), 1, "expected single stmt");
        stmts.into_iter().next().unwrap()
    }

    // `source ~/.mixrc` -> InterpolatedString [EnvVar("HOME"), Literal("/.mixrc")]
    let stmt = parse_one("source ~/.mixrc\n");
    let Stmt {
        kind: StmtKind::Source { path },
        ..
    } = stmt
    else {
        panic!("expected Source stmt");
    };
    let Expr::InterpolatedString(parts) = path else {
        panic!("expected InterpolatedString, got {:?}", path);
    };
    assert_eq!(
        parts,
        vec![
            StringPart::EnvVar("HOME".to_string()),
            StringPart::Literal("/.mixrc".to_string()),
        ]
    );

    // `source ~` alone -> InterpolatedString [EnvVar("HOME")]
    let stmt = parse_one("source ~\n");
    let Stmt {
        kind: StmtKind::Source { path },
        ..
    } = stmt
    else {
        panic!("expected Source stmt");
    };
    let Expr::InterpolatedString(parts) = path else {
        panic!("expected InterpolatedString, got {:?}", path);
    };
    assert_eq!(parts, vec![StringPart::EnvVar("HOME".to_string())]);

    // `source ./.mixrc` -> plain StringLiteral, no tilde expansion
    let stmt = parse_one("source ./.mixrc\n");
    let Stmt {
        kind: StmtKind::Source { path },
        ..
    } = stmt
    else {
        panic!("expected Source stmt");
    };
    let Expr::StringLiteral(s) = path else {
        panic!("expected StringLiteral, got {:?}", path);
    };
    assert_eq!(s, "./.mixrc");

    // `source /etc/foo.mix` -> plain StringLiteral
    let stmt = parse_one("source /etc/foo.mix\n");
    let Stmt {
        kind: StmtKind::Source { path },
        ..
    } = stmt
    else {
        panic!("expected Source stmt");
    };
    let Expr::StringLiteral(s) = path else {
        panic!("expected StringLiteral, got {:?}", path);
    };
    assert_eq!(s, "/etc/foo.mix");

    // `source ../foo.mix` -> plain StringLiteral (DotDot start)
    let stmt = parse_one("source ../foo.mix\n");
    let Stmt {
        kind: StmtKind::Source { path },
        ..
    } = stmt
    else {
        panic!("expected Source stmt");
    };
    let Expr::StringLiteral(s) = path else {
        panic!("expected StringLiteral, got {:?}", path);
    };
    assert_eq!(s, "../foo.mix");

    // Bareword stops at trailing whitespace + comment.
    let stmt = parse_one("source ./foo.mix # comment\n");
    let Stmt {
        kind: StmtKind::Source { path },
        ..
    } = stmt
    else {
        panic!("expected Source stmt");
    };
    let Expr::StringLiteral(s) = path else {
        panic!("expected StringLiteral, got {:?}", path);
    };
    assert_eq!(s, "./foo.mix");

    // Mid-path `#` is a legal filename char, not a comment. Caught by
    // Codex round 1 — original scanner truncated `./foo#bar.mix` to
    // `./foo`. Shell rule: `#` is comment only at word-start, and the
    // bareword scanner only enters this loop after the keyword + space,
    // so we never see `#` at word-start here.
    let stmt = parse_one("source ./foo#bar.mix\n");
    let Stmt {
        kind: StmtKind::Source { path },
        ..
    } = stmt
    else {
        panic!("expected Source stmt");
    };
    let Expr::StringLiteral(s) = path else {
        panic!("expected StringLiteral, got {:?}", path);
    };
    assert_eq!(s, "./foo#bar.mix");

    // Same for mid-path `--`.
    let stmt = parse_one("source ./foo--bar.mix\n");
    let Stmt {
        kind: StmtKind::Source { path },
        ..
    } = stmt
    else {
        panic!("expected Source stmt");
    };
    let Expr::StringLiteral(s) = path else {
        panic!("expected StringLiteral, got {:?}", path);
    };
    assert_eq!(s, "./foo--bar.mix");
}

/// SPEC 18 Phase 2 WS1: `on <cmd> async` parses as Class C
/// (is_async=true) and `on <cmd>` parses as Class S (is_async=false).
/// `async` is contextual — used as a variable name elsewhere it
/// must still tokenise/parse fine, since making it a global
/// reserved word would break existing scripts.
#[test]
fn parse_on_async_modifier() {
    use cosmix_mix::ast::{Stmt, StmtKind};

    fn parse_one(src: &str) -> Stmt {
        let tokens = Lexer::new(src).tokenize().expect("lex");
        let mut p = Parser::new(tokens, src);
        let stmts = p.parse_program().expect("parse");
        assert_eq!(stmts.len(), 1, "expected single stmt, got {}", stmts.len());
        stmts.into_iter().next().unwrap()
    }

    // Plain handler: is_async = false (today's behaviour, unchanged).
    let stmt = parse_one("on foo\n  print \"hi\"\ndone\n");
    let Stmt {
        kind: StmtKind::On {
            command,
            is_async,
            body,
        },
        ..
    } = stmt
    else {
        panic!("expected On stmt");
    };
    assert_eq!(command, "foo");
    assert!(!is_async, "plain `on foo` must be Class S");
    assert_eq!(body.len(), 1);

    // `async` modifier: is_async = true. Body identical to the plain form.
    let stmt = parse_one("on foo async\n  print \"hi\"\ndone\n");
    let Stmt {
        kind: StmtKind::On {
            command,
            is_async,
            body,
        },
        ..
    } = stmt
    else {
        panic!("expected On stmt");
    };
    assert_eq!(command, "foo");
    assert!(is_async, "`on foo async` must be Class C");
    assert_eq!(body.len(), 1);

    // Dotted command names + `async` together (the common shape).
    let stmt = parse_one("on topic.delivery async\n  print \"hi\"\ndone\n");
    let Stmt {
        kind: StmtKind::On {
            command, is_async, ..
        },
        ..
    } = stmt
    else {
        panic!("expected On stmt");
    };
    assert_eq!(command, "topic.delivery");
    assert!(is_async);

    // `async` is a contextual identifier, not a reserved word: a
    // variable named `$async` (and a function arg named `async`)
    // must still tokenise + parse cleanly anywhere else.
    let tokens = Lexer::new("$async = 1\nprint $async\n")
        .tokenize()
        .expect("lex `$async = 1`");
    let mut p = Parser::new(tokens, "$async = 1\nprint $async\n");
    let _ = p.parse_program().expect("parse `$async = 1`");

    // Lockdown: a dotted command suffix `.async` is consumed by the
    // dotted-name loop (parser.rs:1015-1025) BEFORE the modifier check
    // at parser.rs:1035, so `on foo.async` is a Class S handler for
    // command `foo.async` — NOT a Class C handler for `foo`. (Codex
    // WS1 round-1 NIT — locks the exact dotted/modifier ambiguity.)
    let stmt = parse_one("on foo.async\n  print \"hi\"\ndone\n");
    let Stmt {
        kind: StmtKind::On {
            command, is_async, ..
        },
        ..
    } = stmt
    else {
        panic!("expected On stmt");
    };
    assert_eq!(command, "foo.async");
    assert!(
        !is_async,
        "`on foo.async` must consume `.async` as the dotted suffix, NOT the modifier"
    );

    // Lockdown: `on foo async` on a single header line is *always* the
    // modifier interpretation; the body starts on the next line. A
    // script previously relying on a body statement beginning with the
    // bareword `async` on the *next* line keeps working — the modifier
    // check happens before `skip_newlines`, so a body-position `async`
    // (separated by a newline) is left intact. (Codex WS1 round-1
    // MINOR — locks header-position precedence so a future refactor
    // can't drift it.)
    let stmt = parse_one("on foo\n  async\n  print \"hi\"\ndone\n");
    let Stmt {
        kind: StmtKind::On {
            command,
            is_async,
            body,
        },
        ..
    } = stmt
    else {
        panic!("expected On stmt");
    };
    assert_eq!(command, "foo");
    assert!(
        !is_async,
        "body-position `async` (after newline) must NOT be consumed as the modifier"
    );
    assert_eq!(
        body.len(),
        2,
        "body should be two statements: bareword `async` + `print`"
    );
}

/// SPEC 18 Phase 2 WS2: the WS1 parse-time `is_async` flag is
/// preserved at registration in the handler registry. Each
/// `HandlerEntry` records the class so WS3-C can compute chain
/// class (Class C if ANY matching handler is async, else Class S).
///
/// Test runs Mix source through the full lexer → parser → evaluator
/// pipeline (matching `test_on_handlers_multiple_per_command`'s
/// shape) and reads back the stored flag via the
/// `handler_is_async` accessor.
#[tokio::test]
async fn ws2_registry_preserves_is_async() {
    let source = r#"
on topic.s_only
    print "h1"
done
on topic.c_only async
    print "h2"
done
on topic.mixed
    print "h3a"
done
on topic.mixed async
    print "h3b"
done
"#;
    let mut lexer = Lexer::new(source);
    let tokens = lexer.tokenize().unwrap();
    let mut parser = Parser::new(tokens, source);
    let stmts = parser.parse_program().unwrap();

    let mut eval = Evaluator::new();
    eval.execute(&stmts).await.unwrap();

    // Three commands, four handlers total — control: count math matches
    // the pre-WS2 registry shape (commit f60cb2c reference).
    assert_eq!(eval.handler_command_count(), 3);
    assert_eq!(eval.handler_count(), 4);

    // Plain `on <cmd>` registers as Class S.
    assert_eq!(eval.handler_is_async("topic.s_only", 0), Some(false));
    // `on <cmd> async` registers as Class C.
    assert_eq!(eval.handler_is_async("topic.c_only", 0), Some(true));
    // Mixed: registration order preserved, per-entry class preserved.
    // This is the prerequisite for WS3-C's mixed-class-per-event rule
    // (chain runs Class C if ANY entry is async; plain bodies inside
    // still get the writer permit for their own duration).
    assert_eq!(eval.handler_is_async("topic.mixed", 0), Some(false));
    assert_eq!(eval.handler_is_async("topic.mixed", 1), Some(true));

    // Unregistered command / out-of-bounds index return None.
    assert_eq!(eval.handler_is_async("topic.missing", 0), None);
    assert_eq!(eval.handler_is_async("topic.s_only", 99), None);
}

/// SPEC 18 Phase 2 WS3-C.7d — single async handler dispatches via
/// `tokio::task::spawn_local`, the spawned task drives the body to
/// completion when the LocalSet is advanced, and the per-citizen
/// task registry self-removes on normal completion (count returns to
/// zero after enough yields).
///
/// Why this matters: `dispatch_event` for a Class C chain MUST return
/// without awaiting the handler body — that's the whole point of the
/// LocalSet spawn (the parent pump's `next_incoming` must keep polling).
/// The task registry's self-removal step happens INSIDE the spawned
/// closure, after `run_handlers_for` returns. If a refactor accidentally
/// drops the closure's final `task_registry_for_body.remove(task_id)`
/// line, the registry would grow without bound, and this test would
/// catch it (the post-yield `class_c_task_count() == 0` assertion).
///
/// MUST run inside a `LocalSet` context — `tokio::task::spawn_local`
/// panics otherwise. We wrap in `LocalSet::new().run_until(...)`.
#[tokio::test(flavor = "current_thread")]
async fn class_c_dispatch_spawns_and_drains_task_registry() {
    let local = tokio::task::LocalSet::new();
    local
        .run_until(async {
            let source = r#"
$fired = 0
on test.async_msg async
    $fired = $fired + 1
end
"#;
            let mut eval = Evaluator::new();
            // BareHandler provides inert stubs — the handler body doesn't
            // make Bus calls so we don't need a recording handler.
            eval.set_bus_handler(Rc::new(BareHandler));

            let mut lexer = Lexer::new(source);
            let stmts = Parser::new(lexer.tokenize().unwrap(), source)
                .parse_program()
                .unwrap();
            eval.execute(&stmts).await.unwrap();
            assert_eq!(eval.handler_count(), 1);
            assert_eq!(
                eval.handler_is_async("test.async_msg", 0),
                Some(true),
                "handler must register as Class C (async) for this test \
                 to exercise the spawn_local path"
            );

            // dispatch_event returns immediately after spawning. The body
            // has not run yet; the task is queued on the LocalSet but
            // hasn't been polled.
            eval.dispatch_event(mk_event("test.async_msg", "", &[]))
                .await
                .unwrap();

            // Yield repeatedly to let the LocalSet poll the spawned task
            // through completion AND through the self-removal step. A few
            // yields are enough — the body is a single assignment with no
            // await points beyond the per-entry reader permit acquisition.
            for _ in 0..16 {
                tokio::task::yield_now().await;
            }

            assert_eq!(
                eval.get_global("fired").unwrap().to_number().unwrap() as i64,
                1,
                "spawned Class C body must have run exactly once after \
                 the LocalSet advanced"
            );
            assert_eq!(
                eval.class_c_task_count(),
                0,
                "task registry must drain to zero after the spawned task \
                 completes its self-removal step — if this assertion fails, \
                 the closure-tail `task_registry_for_body.remove(task_id)` \
                 line in dispatch_event's Class C arm has regressed"
            );
            assert!(!eval.is_dispatching());
        })
        .await;
}

#[tokio::test]
async fn class_s_handler_exit_reaches_event_pump_boundary() {
    let source = "on test.exit\n  try\n    exit(12)\n  finally\n    print(\"handler cleanup\")\n  end\nend\n";
    let stdout = SharedBuf::new();
    let mut eval = Evaluator::with_output(Box::new(stdout.clone()), Box::new(SharedBuf::new()));
    let mut lexer = Lexer::new(source);
    let stmts = Parser::new(lexer.tokenize().unwrap(), source)
        .parse_program()
        .unwrap();
    eval.execute(&stmts).await.unwrap();
    eval.dispatch_event(mk_event("test.exit", "", &[]))
        .await
        .unwrap();

    let err = eval
        .run_event_pump()
        .await
        .expect_err("handler exit must reach pump");
    assert!(matches!(
        err,
        cosmix_mix::error::MixError::ExitRequest { code: 12 }
    ));
    assert_eq!(stdout.to_string_lossy(), "handler cleanup\n");
}

#[tokio::test(flavor = "current_thread")]
async fn class_c_handler_exit_wakes_event_pump_boundary() {
    let local = tokio::task::LocalSet::new();
    local
        .run_until(async {
            let source = "on test.exit async\n  try\n    exit(13)\n  finally\n    print(\"async cleanup\")\n  end\nend\n";
            let stdout = SharedBuf::new();
            let mut eval =
                Evaluator::with_output(Box::new(stdout.clone()), Box::new(SharedBuf::new()));
            let mut lexer = Lexer::new(source);
            let stmts = Parser::new(lexer.tokenize().unwrap(), source)
                .parse_program()
                .unwrap();
            eval.execute(&stmts).await.unwrap();
            eval.dispatch_event(mk_event("test.exit", "", &[]))
                .await
                .unwrap();
            for _ in 0..16 {
                tokio::task::yield_now().await;
            }

            let err = eval
                .run_event_pump()
                .await
                .expect_err("spawned handler exit must reach pump");
            assert!(matches!(
                err,
                cosmix_mix::error::MixError::ExitRequest { code: 13 }
            ));
            assert_eq!(stdout.to_string_lossy(), "async cleanup\n");
        })
        .await;
}

/// SPEC 18 Phase 2 WS3-C.7d R2 MAJOR-3 — a chain with at least one
/// async entry is classified as Class C and spawned on the LocalSet,
/// but each entry still acquires its OWN per-entry permit inside
/// `run_handlers_for`: async entries take the reader, plain entries
/// embedded in the Class C chain take the writer (with `CleanExitGuard`).
///
/// **Why this exists.** Before MAJOR-3, the Class C spawn arm grabbed
/// a chain-level reader once and held it for the whole chain — so a
/// plain entry mixed into a Class C chain ran under the *reader*,
/// losing the exclusion guarantee a standalone Class S dispatch would
/// have given it. The fix wires `Option<Rc<DispatchScheduler>>` into
/// `run_handlers_for` and transitions permits per entry. If the
/// permit-transition logic ever regresses (e.g. the loop forgets to
/// release the reader before grabbing the writer, or drops the
/// CleanExitGuard before the body runs), this test will still pass
/// the registration-order assertion BUT a deeper concurrency test
/// would catch the lost exclusion — this test is the
/// "smoke-level" companion that proves both entries fire.
///
/// **What this test verifies directly.** Both entries fire in
/// registration order (`$trace == "AB"`); the chain class is computed
/// as Class C (per `ws2_registry_preserves_is_async`, ANY async entry
/// → Class C); the task registry drains to zero after completion.
#[tokio::test(flavor = "current_thread")]
async fn mixed_class_c_chain_runs_all_entries_in_registration_order() {
    let local = tokio::task::LocalSet::new();
    local
        .run_until(async {
            // Plain entry registered FIRST, async SECOND. Chain class
            // is still Class C (ANY async → Class C), so the chain is
            // spawned. Per MAJOR-3, the plain entry acquires the
            // writer permit per-entry; the async entry acquires the
            // reader permit per-entry; both fire in registration order.
            let source = r#"
$trace = ""
on test.mixed
    $trace = $trace .. "A"
end
on test.mixed async
    $trace = $trace .. "B"
end
"#;
            let mut eval = Evaluator::new();
            eval.set_bus_handler(Rc::new(BareHandler));

            let mut lexer = Lexer::new(source);
            let stmts = Parser::new(lexer.tokenize().unwrap(), source)
                .parse_program()
                .unwrap();
            eval.execute(&stmts).await.unwrap();
            assert_eq!(eval.handler_count(), 2);
            assert_eq!(eval.handler_is_async("test.mixed", 0), Some(false));
            assert_eq!(eval.handler_is_async("test.mixed", 1), Some(true));

            eval.dispatch_event(mk_event("test.mixed", "", &[]))
                .await
                .unwrap();

            // Drive the LocalSet through the spawned chain task and
            // both per-entry permit acquisitions. The plain entry's
            // writer acquisition is an `.await` site (so is the async
            // entry's reader), so multiple yields are needed to clear
            // both permits and the closure's self-removal step.
            for _ in 0..32 {
                tokio::task::yield_now().await;
            }

            assert_eq!(
                eval.get_global("trace").unwrap().to_mix_string(),
                "AB",
                "mixed Class C chain must run all entries in \
                 registration order: plain (A) then async (B). If this \
                 fails, the per-entry permit transition in \
                 run_handlers_for has regressed — either the writer \
                 permit for the plain entry deadlocked (still holding \
                 the chain-level reader, pre-MAJOR-3 bug), or the \
                 entry loop short-circuited on the first entry."
            );
            assert_eq!(
                eval.class_c_task_count(),
                0,
                "task registry must drain after the mixed chain \
                 completes"
            );
            assert!(!eval.is_dispatching());
        })
        .await;
}

/// SPEC 18 Phase 2 WS3-C.7e.5 — the yield helper is load-bearing.
///
/// **Invariant.** A Class C (async) handler body that reaches an
/// `.await` site (`sleep`, `send`, `emit`, `reply`, `subscribe`,
/// `unsubscribe`, `port_exists`, `bus_reconnect`, `noded_register`,
/// or any registered extension future) MUST release its
/// `DispatchScheduler` reader permit across the await and reacquire
/// it afterward. While the reader is released, a queued Class S
/// dispatch's `acquire_writer_dispatch` admits — the writer permit
/// becomes obtainable the instant the last reader drops. The
/// invariant collapses into one observable: a Class S handler queued
/// AFTER a Class C handler's yield point must run BEFORE the Class C
/// handler resumes past that yield point.
///
/// **Sequence under C.7e.**
///   1. `dispatch_event(slow.msg)` → Class C → `spawn_local` → returns
///      immediately. Spawned task is queued on the LocalSet but not yet
///      polled (current_thread executor only polls at await suspension
///      points).
///   2. `yield_now()` × N — drives the spawned slow task: acquires
///      reader (uncontended → immediate), runs body up to `$trace=
///      ..="S"`, hits `sleep(0.1)`. The `await_with_class_c_yield`
///      helper takes the owned reader guard off `ctx.class_c_read_permit`,
///      drops it, then awaits the timer. Reader is NOW released.
///   3. `dispatch_event(fast.msg)` → Class S → `acquire_writer_dispatch`.
///      No readers held → writer admits immediately. Body runs
///      `$trace=..="F"`. Writer drops.
///   4. Caller awaits a wall-clock 150ms so slow's 100ms timer fires.
///      Spawned task's sleep completes; helper reacquires reader (uncontended
///      → immediate); body resumes with `$trace=..="E"`. Task finishes,
///      self-removes from the registry.
///
/// **Expected trace under C.7e: "SFE"**. The discriminating ordering
/// — F sits BETWEEN S and E — is only possible if slow's reader was
/// dropped during the sleep.
///
/// **Regression trace without yield: "SEF"**. If `await_with_class_c_yield`
/// were a transparent no-op (or `ctx.class_c_read_permit` were never
/// installed by `run_handlers_for`), slow would hold the reader through
/// sleep. The writer permit cannot admit while a reader is held, so
/// `dispatch_event(fast.msg)`'s `acquire_writer_dispatch.await` would
/// block. The current_thread executor would still poll the spawned slow
/// task; its sleep would complete, slow would run "E", drop the reader,
/// and ONLY THEN would the writer admit and run "F". The discriminating
/// "F-between-S-and-E" ordering would collapse into "S then E then F".
///
/// **Why not two Class C handlers as the discriminator.** Two Class C
/// handlers naturally interleave through the `tokio::sync::RwLock`
/// reader-shared semantics regardless of the yield helper — both can
/// hold the reader simultaneously, so the helper would be a no-op for
/// that observation. Only the writer-admits-on-yield case requires
/// the per-await release, and only that case discriminates a regression.
///
/// MUST run inside a `LocalSet` context — Class C dispatch uses
/// `tokio::task::spawn_local`, which panics outside a LocalSet.
#[tokio::test(flavor = "current_thread")]
#[cfg_attr(not(feature = "tokio-sleep"), ignore = "requires tokio-sleep feature")]
async fn class_c_yield_admits_pending_class_s_writer() {
    let local = tokio::task::LocalSet::new();
    local
        .run_until(async {
            let source = r#"
$trace = ""
on slow.msg async
    $trace = $trace .. "S"
    sleep(0.1)
    $trace = $trace .. "E"
end
on fast.msg
    $trace = $trace .. "F"
end
"#;
            let mut eval = Evaluator::new();
            eval.set_bus_handler(Rc::new(BareHandler));

            let mut lexer = Lexer::new(source);
            let stmts = Parser::new(lexer.tokenize().unwrap(), source)
                .parse_program()
                .unwrap();
            eval.execute(&stmts).await.unwrap();
            assert_eq!(eval.handler_is_async("slow.msg", 0), Some(true));
            assert_eq!(eval.handler_is_async("fast.msg", 0), Some(false));

            // Step 1: dispatch the Class C event — spawn_local, returns
            // immediately. The spawned task is queued but unpolled.
            eval.dispatch_event(mk_event("slow.msg", "", &[]))
                .await
                .unwrap();

            // Step 2: drive the LocalSet so the spawned slow task progresses
            // to its `sleep(0.1)` yield point. Two awaits stand between
            // spawn and sleep: per-entry reader acquisition (uncontended →
            // resolves on the next poll) and the sleep itself. 32 yields
            // is comfortably enough; the loop guards against pathological
            // scheduler reorderings.
            for _ in 0..32 {
                tokio::task::yield_now().await;
            }
            assert_eq!(
                eval.get_global("trace")
                    .map(|v| v.to_mix_string())
                    .unwrap_or_default(),
                "S",
                "slow's body must have reached the `sleep(0.1)` yield \
                 point after the yield-now drive — saw {:?}. If this \
                 fails, the spawn / per-entry reader acquisition path \
                 has regressed and slow never got to its body.",
                eval.get_global("trace").map(|v| v.to_mix_string())
            );
            assert_eq!(
                eval.class_c_task_count(),
                1,
                "spawned slow task must still be in flight (parked on \
                 its sleep timer) when fast.msg is dispatched"
            );

            // Step 3: dispatch the Class S event. With C.7e yield, slow's
            // reader was released at the sleep above, so the writer admits
            // immediately and F runs synchronously inside dispatch_event.
            // Without C.7e yield, this acquire_writer_dispatch.await would
            // block until slow's sleep fully completes (~100ms).
            eval.dispatch_event(mk_event("fast.msg", "", &[]))
                .await
                .unwrap();

            // Step 4: wait for slow's spawned task to complete its
            // sleep (~100ms) and run the trailing "E". 50 iterations × 10ms
            // = up to 500ms ceiling; the loop exits as soon as the registry
            // drains. Using `tokio::time::sleep` rather than `yield_now`
            // because the timer is real-time, not virtual.
            for _ in 0..50 {
                if eval.class_c_task_count() == 0 {
                    break;
                }
                tokio::time::sleep(std::time::Duration::from_millis(10)).await;
            }

            assert_eq!(
                eval.get_global("trace").unwrap().to_mix_string(),
                "SFE",
                "C.7e YIELD INVARIANT VIOLATED. Expected \"SFE\" (slow \
                 starts S → yields at sleep, releasing reader → fast \
                 admits writer and writes F → slow's sleep completes, \
                 reacquires reader, writes E). Got {:?}. If this is \
                 \"SEF\", `await_with_class_c_yield` is no longer \
                 releasing the Class C reader permit across the sleep \
                 — the writer was blocked behind a still-held reader \
                 for the full duration of slow's body, defeating the \
                 entire point of the C.7e yield discipline. Audit \
                 `ctx.class_c_read_permit` installation in \
                 `run_handlers_for` and the take/drop/reacquire sequence \
                 in `await_with_class_c_yield`.",
                eval.get_global("trace").map(|v| v.to_mix_string())
            );
            assert_eq!(
                eval.class_c_task_count(),
                0,
                "task registry must drain to zero after slow's spawned \
                 task self-removes — if non-zero, slow's body never \
                 resumed past its sleep, meaning the reacquire half of \
                 `await_with_class_c_yield` is broken."
            );
            assert!(!eval.is_dispatching());
        })
        .await;
}

// ── SPEC 18 Phase 2 WS3-C.7f.1 — Class C shutdown drain library API ──

use cosmix_mix::evaluator::{ClassCDrainOutcome, SHUTDOWN_SYNTH_BODY, SHUTDOWN_SYNTH_RC};

/// Drain branch 1: every spawned Class C task completes inside the
/// grace window and the handler answered its request via `reply()`.
///
/// **Invariant.** `drain_class_c_for_shutdown` MUST drive in-flight
/// task handles to natural completion when they finish before the
/// deadline — it must NOT pre-emptively abort tasks that would have
/// completed cleanly inside grace. A regression where Phase 1's
/// `FuturesUnordered` loop fell through to the `Phase 2 abort` arm
/// regardless of pending-state (e.g. inverted `if pending.is_empty()`
/// break, dropped `Some(_)` arm) would still wire a reply (via the
/// handler-body `reply()`) but would over-abort and inflate `aborted`.
///
/// **What this proves.** The single in-flight task is drained cleanly
/// (`drained_clean == 1`, `aborted == 0`), and the synth path is
/// inert (`synth_sent == synth_failed == synth_skipped_no_socket == 0`)
/// because `registration.complete()` has already removed the handle
/// from the reply registry by the time Phase 3 snapshots it. The
/// `ReplyRecorder` log must contain exactly the one in-body reply
/// (rc=0, body="ok") — never a follow-up synth.
#[tokio::test(flavor = "current_thread")]
#[cfg_attr(not(feature = "tokio-sleep"), ignore = "requires tokio-sleep feature")]
async fn c7f_drain_clean_completes_inside_grace_with_no_synth() {
    let local = tokio::task::LocalSet::new();
    local
        .run_until(async {
            let source = r#"
on test.req async
    reply(0, "ok")
end
"#;
            let log: ReplyLog = Rc::new(RefCell::new(Vec::new()));
            let mut eval = Evaluator::new();
            eval.set_bus_handler(Rc::new(ReplyRecorder(log.clone())));

            let mut lexer = Lexer::new(source);
            let stmts = Parser::new(lexer.tokenize().unwrap(), source)
                .parse_program()
                .unwrap();
            eval.execute(&stmts).await.unwrap();
            assert_eq!(eval.handler_is_async("test.req", 0), Some(true));

            eval.dispatch_event(mk_event(
                "test.req",
                "",
                &[("type", "request"), ("from", "caller"), ("id", "corr-1")],
            ))
            .await
            .unwrap();

            // Do NOT pre-yield — let drain itself drive the spawned task
            // through its body and self-removal. This exercises the
            // Phase 1 `FuturesUnordered`-polls-handles-to-completion
            // contract, which is the load-bearing observation.
            let outcome: ClassCDrainOutcome = eval
                .drain_class_c_for_shutdown(std::time::Duration::from_secs(5), true)
                .await;

            assert_eq!(
                outcome.initial_tasks, 1,
                "task registry snapshot at drain entry must see the \
                 just-spawned-but-unpolled handle"
            );
            assert_eq!(
                outcome.initial_pending, 1,
                "reply registry snapshot at drain entry must see the \
                 unanswered request (body has not run yet so neither \
                 reply() nor registration.complete() has fired)"
            );
            assert_eq!(
                outcome.drained_clean, 1,
                "the single in-flight task must drain cleanly inside \
                 the 5s grace — if this is 0, Phase 1's pending.next() \
                 arm did not count completions; if this is >1, the \
                 counter is double-incrementing"
            );
            assert_eq!(
                outcome.aborted, 0,
                "no survivor — Phase 2 must NOT abort tasks that \
                 completed within grace"
            );
            assert_eq!(outcome.synth_sent, 0);
            assert_eq!(outcome.synth_failed, 0);
            assert_eq!(outcome.synth_skipped_no_socket, 0);

            let entries = log.borrow();
            assert_eq!(
                entries.len(),
                1,
                "ReplyRecorder must have exactly the in-body reply — \
                 a follow-up synth would mean the drain did not \
                 observe registration.complete()'s deregistration"
            );
            assert_eq!(entries[0].0, "caller");
            assert_eq!(entries[0].1, "test.req");
            assert_eq!(entries[0].2.as_deref(), Some("corr-1"));
            assert_eq!(entries[0].3, 0);
            assert_eq!(entries[0].4, "ok");

            assert!(!eval.is_dispatching());
            assert_eq!(eval.class_c_task_count(), 0);
        })
        .await;
}

/// Drain branch 2: an in-flight Class C task exceeds grace, gets
/// aborted, and the still-pending request gets a synthesized §3.4
/// shutdown reply (rc=2, fixed body).
///
/// **Invariant.** Phase 2's abort path drops survivors from the
/// LocalSet AND Phase 3's synth loop force-paths a wire reply through
/// `InvocationReplyHandle::synthesize_unanswered`, bypassing the
/// `reply_once` gate (because state may still be Unanswered, or in a
/// race window it could be transient Replying — both must be answered
/// to prevent a caller-side hang). The rc and body MUST be the
/// canonical `SHUTDOWN_SYNTH_RC` / `SHUTDOWN_SYNTH_BODY` constants —
/// these are wire contracts, not local strings.
///
/// **Regression catch.** If `synth_sent` is 0 but `aborted` is 1, the
/// synth loop is filtering on the wrong predicate (e.g. the §3.4
/// `is_unanswered_request` rather than C.7f's wider
/// `is_pending_request`) or short-circuiting on `allow_synth_replies`
/// inverted. If `synth_failed` is 1, the `BusHandler::reply` returned
/// `Err` — possible if the recorder were swapped for an inert handler,
/// which is exactly what `c7f_drain_synth_fails_when_handler_errors`
/// would cover in a future expansion.
#[tokio::test(flavor = "current_thread")]
#[cfg_attr(not(feature = "tokio-sleep"), ignore = "requires tokio-sleep feature")]
async fn c7f_drain_aborts_survivors_past_grace_and_synthesizes() {
    let local = tokio::task::LocalSet::new();
    local
        .run_until(async {
            let source = r#"
on test.req async
    sleep(2)
    reply(0, "this body must never reach reply()")
end
"#;
            let log: ReplyLog = Rc::new(RefCell::new(Vec::new()));
            let mut eval = Evaluator::new();
            eval.set_bus_handler(Rc::new(ReplyRecorder(log.clone())));

            let mut lexer = Lexer::new(source);
            let stmts = Parser::new(lexer.tokenize().unwrap(), source)
                .parse_program()
                .unwrap();
            eval.execute(&stmts).await.unwrap();

            eval.dispatch_event(mk_event(
                "test.req",
                "",
                &[("type", "request"), ("from", "caller"), ("id", "corr-1")],
            ))
            .await
            .unwrap();

            // Short grace (50ms) << handler's `sleep(2)` (2s) so the
            // sleep timer can't possibly fire inside grace. Abort path
            // is forced.
            let outcome = eval
                .drain_class_c_for_shutdown(std::time::Duration::from_millis(50), true)
                .await;

            assert_eq!(outcome.initial_tasks, 1);
            assert_eq!(outcome.initial_pending, 1);
            assert_eq!(
                outcome.drained_clean, 0,
                "the sleep(2) body cannot complete inside a 50ms grace \
                 — drained_clean must be 0, else the timer-vs-grace \
                 inequality has inverted"
            );
            assert_eq!(
                outcome.aborted, 1,
                "Phase 2 must abort the lone survivor on grace expiry"
            );
            assert_eq!(
                outcome.synth_sent, 1,
                "Phase 3 must synthesize a shutdown reply for the \
                 stranded pending request — a 0 here means the synth \
                 loop is filtering wrong (likely is_unanswered_request \
                 instead of is_pending_request) or short-circuited on \
                 allow_synth_replies"
            );
            assert_eq!(outcome.synth_failed, 0);
            assert_eq!(outcome.synth_skipped_no_socket, 0);

            let entries = log.borrow();
            assert_eq!(
                entries.len(),
                1,
                "exactly one synth wire reply — the in-body reply() \
                 would have come from the post-sleep path which never \
                 ran"
            );
            assert_eq!(entries[0].0, "caller");
            assert_eq!(entries[0].1, "test.req");
            assert_eq!(entries[0].2.as_deref(), Some("corr-1"));
            assert_eq!(
                entries[0].3, SHUTDOWN_SYNTH_RC,
                "synth rc MUST be the module constant SHUTDOWN_SYNTH_RC \
                 (=2, the §3.4 fault-domain code) — a different rc \
                 means a fork in the synth path or a stale local"
            );
            assert_eq!(
                entries[0].4, SHUTDOWN_SYNTH_BODY,
                "synth body MUST be the module constant \
                 SHUTDOWN_SYNTH_BODY — this string is a wire contract \
                 to callers parsing the body for cause identification"
            );

            assert!(!eval.is_dispatching());
        })
        .await;
}

/// Drain branch 3 (transport-drop): `allow_synth_replies = false`.
/// The socket is already gone — there is no caller channel to reply
/// on, so the drain MUST abort survivors WITHOUT firing a synth
/// wire send. `synth_skipped_no_socket` counts the suppressed
/// pendings so the outer logger can record "n requests stranded by
/// transport drop" structured.
///
/// **Invariant.** `allow_synth_replies = false` ⇒ `synth_sent == 0
/// && synth_failed == 0`, regardless of `initial_pending`. The synth
/// loop's short-circuit MUST happen *before* the wire `handler.reply
/// (...).await` call — wiring a reply to a dropped transport would
/// either panic the Bus handler or log a transport error, neither of
/// which is the intended path.
#[tokio::test(flavor = "current_thread")]
#[cfg_attr(not(feature = "tokio-sleep"), ignore = "requires tokio-sleep feature")]
async fn c7f_drain_skips_synth_when_synth_disallowed() {
    let local = tokio::task::LocalSet::new();
    local
        .run_until(async {
            let source = r#"
on test.req async
    sleep(2)
end
"#;
            let log: ReplyLog = Rc::new(RefCell::new(Vec::new()));
            let mut eval = Evaluator::new();
            eval.set_bus_handler(Rc::new(ReplyRecorder(log.clone())));

            let mut lexer = Lexer::new(source);
            let stmts = Parser::new(lexer.tokenize().unwrap(), source)
                .parse_program()
                .unwrap();
            eval.execute(&stmts).await.unwrap();

            eval.dispatch_event(mk_event(
                "test.req",
                "",
                &[("type", "request"), ("from", "caller"), ("id", "corr-1")],
            ))
            .await
            .unwrap();

            let outcome = eval
                .drain_class_c_for_shutdown(std::time::Duration::from_millis(50), false)
                .await;

            assert_eq!(outcome.initial_tasks, 1);
            assert_eq!(outcome.initial_pending, 1);
            assert_eq!(outcome.aborted, 1);
            assert_eq!(
                outcome.synth_sent, 0,
                "synth wire send MUST NOT fire when allow_synth_replies \
                 = false — transport is gone, the wire write would \
                 reach a dead socket"
            );
            assert_eq!(outcome.synth_failed, 0);
            assert_eq!(
                outcome.synth_skipped_no_socket, 1,
                "the suppressed pending must be counted under \
                 synth_skipped_no_socket so the outer logger can \
                 report 'n requests stranded by transport drop'"
            );

            assert_eq!(
                log.borrow().len(),
                0,
                "ReplyRecorder MUST be empty — any entry would mean \
                 the synth wire send fired despite allow_synth_replies \
                 = false (transport-drop guard regression)"
            );
        })
        .await;
}

/// Drain branch 4: a handle that the handler already answered MUST
/// NOT be re-answered by Phase 3's synth loop, even when its owning
/// task gets aborted in Phase 2.
///
/// **Setup.** Handler replies fast, THEN sleeps long. Test pre-drives
/// the LocalSet until the in-body `reply(0, "fast")` has fired
/// (proven by `log.len() == 1`), at which point the handle's state
/// has transitioned to `Answered`. The task is still alive inside
/// `sleep(2)`, so drain's Phase 2 will abort it. Phase 3 snapshots
/// the reply registry — the handle still appears because the body
/// has not reached `registration.complete()` — but
/// `is_pending_request()` returns false (state == Answered), so the
/// synth loop skips it.
///
/// **Why this matters.** Without the `state != Answered` guard in
/// `is_pending_request`, this scenario produces a duplicate wire
/// reply — first the handler's "fast", then the drain's synth "rc=2,
/// shutdown..." — a caller that already moved on from the first
/// reply would see a phantom second message correlated to the same
/// request id. That's the bug class C.7f's split between §3.4's
/// `is_unanswered_request` and C.7f's `is_pending_request` exists
/// to prevent.
#[tokio::test(flavor = "current_thread")]
#[cfg_attr(not(feature = "tokio-sleep"), ignore = "requires tokio-sleep feature")]
async fn c7f_drain_skips_resynth_for_already_answered_handle() {
    let local = tokio::task::LocalSet::new();
    local
        .run_until(async {
            let source = r#"
on test.req async
    reply(0, "fast")
    sleep(2)
    reply(99, "this body must never reach the post-sleep reply")
end
"#;
            let log: ReplyLog = Rc::new(RefCell::new(Vec::new()));
            let mut eval = Evaluator::new();
            eval.set_bus_handler(Rc::new(ReplyRecorder(log.clone())));

            let mut lexer = Lexer::new(source);
            let stmts = Parser::new(lexer.tokenize().unwrap(), source)
                .parse_program()
                .unwrap();
            eval.execute(&stmts).await.unwrap();

            eval.dispatch_event(mk_event(
                "test.req",
                "",
                &[("type", "request"), ("from", "caller"), ("id", "corr-1")],
            ))
            .await
            .unwrap();

            // Drive the spawned task through reply(0, "fast") and into
            // sleep(2). The body's reply() is the first await site
            // past the spawn — a handful of yields is sufficient; the
            // loop guards against unrelated scheduler reorderings.
            for _ in 0..64 {
                if !log.borrow().is_empty() {
                    break;
                }
                tokio::task::yield_now().await;
            }
            assert_eq!(
                log.borrow().len(),
                1,
                "handler must have called reply(0, \"fast\") before \
                 the drain runs — if this is 0, the yield-drive cap is \
                 too low or the handler body never reached reply()"
            );

            let outcome = eval
                .drain_class_c_for_shutdown(std::time::Duration::from_millis(50), true)
                .await;

            assert_eq!(outcome.initial_tasks, 1, "task still alive in sleep(2)");
            assert_eq!(
                outcome.initial_pending, 0,
                "handle is Answered post-reply — is_pending_request() \
                 must return false even though the registry entry has \
                 not been removed yet (body never reached \
                 registration.complete())"
            );
            assert_eq!(outcome.drained_clean, 0);
            assert_eq!(
                outcome.aborted, 1,
                "the still-sleeping task must be aborted on grace \
                 expiry"
            );
            assert_eq!(
                outcome.synth_sent, 0,
                "synth MUST NOT fire — the handle is Answered, a synth \
                 here would be a duplicate-reply bug on the wire"
            );
            assert_eq!(outcome.synth_failed, 0);
            assert_eq!(outcome.synth_skipped_no_socket, 0);

            let entries = log.borrow();
            assert_eq!(
                entries.len(),
                1,
                "ReplyRecorder must still hold only the one fast reply \
                 — a second entry would be the duplicate-reply \
                 regression this test exists to catch"
            );
            assert_eq!(entries[0].4, "fast");
        })
        .await;
}

/// Drain branch 5: a topic delivery (no `type=request` header)
/// produces a non-request reply handle. Phase 3's synth filter MUST
/// skip it — there is no caller to answer.
///
/// **Invariant.** `is_pending_request()` is false for any handle with
/// `is_request == false`. A topic-delivery Class C task that exceeds
/// grace gets aborted (just like any other Class C task), but the
/// synth loop MUST NOT call `synthesize_unanswered` on it — that
/// method itself returns `Err` for non-request handles as a defensive
/// belt-and-braces, but the outer filter should make that branch
/// unreachable. If this test ever shows `synth_failed >= 1`, the
/// outer filter has regressed and the defensive inner check is the
/// only thing saving us from a wire panic.
#[tokio::test(flavor = "current_thread")]
#[cfg_attr(not(feature = "tokio-sleep"), ignore = "requires tokio-sleep feature")]
async fn c7f_drain_skips_synth_for_topic_deliveries() {
    let local = tokio::task::LocalSet::new();
    local
        .run_until(async {
            let source = r#"
on test.topic async
    sleep(2)
end
"#;
            let log: ReplyLog = Rc::new(RefCell::new(Vec::new()));
            let mut eval = Evaluator::new();
            eval.set_bus_handler(Rc::new(ReplyRecorder(log.clone())));

            let mut lexer = Lexer::new(source);
            let stmts = Parser::new(lexer.tokenize().unwrap(), source)
                .parse_program()
                .unwrap();
            eval.execute(&stmts).await.unwrap();

            // No `type=request` header → topic delivery semantics.
            // Handle registers with is_request=false.
            eval.dispatch_event(mk_event("test.topic", "", &[("topic", "test.topic")]))
                .await
                .unwrap();

            let outcome = eval
                .drain_class_c_for_shutdown(std::time::Duration::from_millis(50), true)
                .await;

            assert_eq!(outcome.initial_tasks, 1);
            assert_eq!(
                outcome.initial_pending, 0,
                "topic delivery has is_request=false; pending-count \
                 (which gates on is_request) MUST be 0 at entry"
            );
            assert_eq!(outcome.aborted, 1);
            assert_eq!(
                outcome.synth_sent, 0,
                "topic deliveries have no caller — synth MUST NOT fire"
            );
            assert_eq!(
                outcome.synth_failed, 0,
                "synth_failed >= 1 would mean the outer \
                 is_pending_request filter let a non-request handle \
                 reach synthesize_unanswered(), and only the defensive \
                 inner non-request guard saved us — fix the outer \
                 filter"
            );
            assert_eq!(outcome.synth_skipped_no_socket, 0);

            assert_eq!(
                log.borrow().len(),
                0,
                "ReplyRecorder MUST be empty — a topic-delivery \
                 synth-reply is a contract violation regardless of \
                 the rc/body"
            );
        })
        .await;
}

/// Unbound positional `$1[0]` must error rather than silently
/// returning Nil. The sync fast path's positional shortcut routes
/// through `index_value_clone(&Value::Nil, &idx)` which now errors
/// with "cannot index nil with number", matching async.
#[tokio::test]
async fn indexing_unbound_positional_errors() {
    let err = run_mix_capturing("$y = $1[0]\nprint $y\n")
        .await
        .expect_err("indexing unbound positional arg must raise a runtime error");
    assert!(
        err.contains("cannot index") && err.contains("nil"),
        "expected 'cannot index nil with number' error, got {err:?}"
    );
}

/// SPEC 18 Phase 2 WS3-C.7g B1 — Class S handler reads an existing
/// top-level global, increments it, and writes it back across N
/// dispatches. Each Class S dispatch now runs on a per-invocation
/// activation built by `for_invocation()`; the shared-global frame
/// must be visible *and* the increment must be observable to the
/// parent evaluator after the dispatch returns.
///
/// Protects the activation's shared-global frame contract before
/// C.7g B2 removes the CleanExitGuard / poison fallback. If the
/// activation accidentally got its own non-shared frame, reads would
/// silently see `nil`, the `..` concatenation would coerce, and the
/// per-dispatch update would be lost — the assert chain catches that.
#[tokio::test]
async fn class_s_handler_reads_and_updates_shared_global_via_activation() {
    let source = r#"
$counter = 0
$trace = "start"
on bump
    $counter = $counter + 1
    $trace = $trace .. ":" .. $event.body
done
"#;
    let mut eval = Evaluator::new();
    let mut lexer = Lexer::new(source);
    let stmts = Parser::new(lexer.tokenize().unwrap(), source)
        .parse_program()
        .unwrap();
    eval.execute(&stmts).await.unwrap();

    assert_eq!(
        eval.get_global("counter").unwrap().to_mix_string(),
        "0",
        "parent must see initial value before any dispatch"
    );

    for n in 1..=3 {
        eval.dispatch_event(mk_event("bump", &n.to_string(), &[]))
            .await
            .unwrap();
        assert_eq!(
            eval.get_global("counter").unwrap().to_mix_string(),
            n.to_string(),
            "Class S handler's increment of $counter on the activation \
             must be visible on the parent evaluator after dispatch \
             returns (shared-global frame contract — C.7g B1)"
        );
    }
    assert_eq!(
        eval.get_global("trace").unwrap().to_mix_string(),
        "start:1:2:3",
        "Class S `..` concatenation across the activation boundary \
         must mutate the same parent-visible string slot (would be \
         'start' if reads hit a non-shared frame and writes hit a \
         different one)"
    );
}

/// SPEC 18 Phase 2 WS4 — per-`send` timeout shape.
///
/// A `SlowBusHandler` sleeps `delay_ms` before replying. The Mix
/// script issues two sends: one with `timeout=0.05` (50 ms) against a
/// 500 ms reply (should time out — `$rc="-1"`, `$result="timeout:
/// send to <target> exceeded 0.05s"`), and one with no timeout (should
/// succeed and write the slow handler's reply). Also covers:
/// - The downstream handler receives ONLY the user-authored args
///   (the `timeout=` control kwarg must NOT be forwarded as an arg
///   into the Bus args map — a recorded-arg-keys assertion would
///   catch a forwarding regression).
/// - Negative / zero / non-numeric `timeout=` values raise a
///   `RuntimeError` rather than silently meaning "no timeout".
#[tokio::test]
async fn send_timeout_kwarg_writes_typed_error_and_does_not_forward() {
    use cosmix_mix::error::MixResult;
    use cosmix_mix::evaluator::BusHandler;
    use cosmix_mix::value::Value;
    use std::cell::RefCell;
    use std::future::Future;
    use std::pin::Pin;
    use std::rc::Rc;

    struct SlowBusHandler {
        delay_ms: u64,
        recorded_arg_keys: RefCell<Vec<Vec<String>>>,
    }
    impl BusHandler for SlowBusHandler {
        fn send<'a>(
            &'a self,
            _target: &'a str,
            command: &'a str,
            args: &'a Value,
        ) -> Pin<Box<dyn Future<Output = MixResult<(i32, Value)>> + 'a>> {
            if let Value::Map(m) = args {
                self.recorded_arg_keys
                    .borrow_mut()
                    .push(m.keys().cloned().collect());
            }
            Box::pin(async move {
                tokio::time::sleep(std::time::Duration::from_millis(self.delay_ms)).await;
                Ok((0, Value::String(format!("reply:{}", command))))
            })
        }
        fn emit<'a>(
            &'a self,
            _t: &'a str,
            _c: &'a str,
            _a: &'a Value,
        ) -> Pin<Box<dyn Future<Output = MixResult<()>> + 'a>> {
            Box::pin(async move { Ok(()) })
        }
        fn port_exists<'a>(
            &'a self,
            _t: &'a str,
        ) -> Pin<Box<dyn Future<Output = MixResult<bool>> + 'a>> {
            Box::pin(async move { Ok(true) })
        }
        fn next_incoming<'a>(
            &'a self,
        ) -> Pin<Box<dyn Future<Output = Option<cosmix_mix::evaluator::IncomingEvent>> + 'a>>
        {
            Box::pin(async move { None })
        }
    }

    let handler = Rc::new(SlowBusHandler {
        delay_ms: 500,
        recorded_arg_keys: RefCell::new(Vec::new()),
    });

    let source = r#"
send "downstream" "echo" payload="ping" timeout=0.05
$rc_to = $rc
$result_to = $result
send "downstream" "echo" payload="pong"
$rc_ok = $rc
$result_ok = $result
"#;

    let mut eval = Evaluator::new();
    eval.set_bus_handler(handler.clone());
    let mut lexer = Lexer::new(source);
    let stmts = Parser::new(lexer.tokenize().unwrap(), source)
        .parse_program()
        .unwrap();
    eval.execute(&stmts).await.unwrap();

    assert_eq!(
        eval.get_global("rc_to").unwrap(),
        Value::Number(-2.0),
        "send timeout must write the NUMBER rc=-2 (RC_TIMEOUT), \
         distinct from -1 transport (2026-07 rc-band unification)"
    );
    assert_eq!(
        eval.get_global("result_to").unwrap().to_mix_string(),
        "timeout: send to downstream exceeded 0.05s",
        "send timeout must write the canonical message including \
         target and budget so handlers can discriminate it from \
         other rc bands"
    );

    assert_eq!(
        eval.get_global("rc_ok").unwrap().to_mix_string(),
        "0",
        "send without timeout must run the slow handler to completion"
    );
    assert_eq!(
        eval.get_global("result_ok").unwrap().to_mix_string(),
        "reply:echo",
        "send without timeout must observe the slow handler's reply"
    );

    let keys = handler.recorded_arg_keys.borrow();
    assert_eq!(keys.len(), 2, "downstream must have received both sends");
    for (i, k) in keys.iter().enumerate() {
        assert!(
            !k.iter().any(|s| s == "timeout"),
            "send #{}: the `timeout=` control kwarg must NOT be \
             forwarded to the downstream handler as an arg (got {:?})",
            i + 1,
            k
        );
        assert!(
            k.iter().any(|s| s == "payload"),
            "send #{}: real arg `payload=` must reach the downstream \
             handler (got {:?})",
            i + 1,
            k
        );
    }
}

/// SPEC 18 Phase 2 WS4 — invalid `timeout=` values reject at the
/// `send` site rather than silently meaning "no timeout".
///
/// "Absence means no timeout" is the spec; sentinels (zero, negative,
/// non-numeric) must surface as runtime errors so a handler bug doesn't
/// quietly mask a missing budget on a network call.
#[tokio::test]
async fn send_timeout_invalid_values_raise_runtime_error() {
    use cosmix_mix::error::MixResult;
    use cosmix_mix::evaluator::BusHandler;
    use cosmix_mix::value::Value;
    use std::future::Future;
    use std::pin::Pin;
    use std::rc::Rc;

    struct OkBusHandler;
    impl BusHandler for OkBusHandler {
        fn send<'a>(
            &'a self,
            _t: &'a str,
            _c: &'a str,
            _a: &'a Value,
        ) -> Pin<Box<dyn Future<Output = MixResult<(i32, Value)>> + 'a>> {
            Box::pin(async move { Ok((0, Value::String("ok".to_string()))) })
        }
        fn emit<'a>(
            &'a self,
            _t: &'a str,
            _c: &'a str,
            _a: &'a Value,
        ) -> Pin<Box<dyn Future<Output = MixResult<()>> + 'a>> {
            Box::pin(async move { Ok(()) })
        }
        fn port_exists<'a>(
            &'a self,
            _t: &'a str,
        ) -> Pin<Box<dyn Future<Output = MixResult<bool>> + 'a>> {
            Box::pin(async move { Ok(true) })
        }
        fn next_incoming<'a>(
            &'a self,
        ) -> Pin<Box<dyn Future<Output = Option<cosmix_mix::evaluator::IncomingEvent>> + 'a>>
        {
            Box::pin(async move { None })
        }
    }

    for (src, needle) in [
        (r#"send "t" "c" timeout=0"#, "must be positive"),
        (r#"send "t" "c" timeout=-1.5"#, "must be positive"),
        (r#"send "t" "c" timeout="abc""#, "must be a positive number"),
    ] {
        let mut eval = Evaluator::new();
        eval.set_bus_handler(Rc::new(OkBusHandler));
        let mut lexer = Lexer::new(src);
        let stmts = Parser::new(lexer.tokenize().unwrap(), src)
            .parse_program()
            .unwrap();
        let err = eval
            .execute(&stmts)
            .await
            .expect_err(&format!("expected runtime error for: {}", src));
        let msg = err.to_string();
        assert!(
            msg.contains(needle),
            "{}: error message should contain {:?}, got {:?}",
            src,
            needle,
            msg
        );
    }
}

/// SPEC 18 Phase 2 WS4 — the hanging downstream future is actually
/// dropped when `timeout=` elapses (not just abandoned with a typed
/// rc).
///
/// The Codex R1 BLOCKER on WS4 was that wrapping `handler.send` in
/// `tokio::time::timeout` is only as cancellation-safe as the inner
/// future: if dropping the inner future fails to release the Bus
/// client's pending-correlation entry (the real
/// `cosmix-lib-client::NodedClient::call` case), each elapsed timeout
/// would leak a pending slot. The fix landed
/// `cosmix-lib-client::PendingGuard` (RAII-removes the pending entry
/// on drop). This test pins the upstream evaluator side of that
/// contract: the inner future MUST be dropped on elapsed (so any RAII
/// guard inside it actually fires) rather than parked alive until a
/// late reply.
///
/// Construction: the handler builds an `Arc<AtomicBool>` "DropFlag"
/// owned by the returned send-future. The future then awaits
/// `std::future::pending()` forever. On elapsed, if the timeout branch
/// drops the future correctly, `DropFlag::drop` runs and flips the
/// flag; if instead the future is leaked (parked, leaked-pinned, kept
/// in a queue without abort), the flag stays `false` and the test
/// fails. The Mix script sets `timeout=0.05` and reads `$rc`/`$result`
/// to also verify the typed-error shape on the same elapsed event.
#[tokio::test]
async fn send_timeout_drops_hanging_future_on_elapsed() {
    use cosmix_mix::error::MixResult;
    use cosmix_mix::evaluator::BusHandler;
    use cosmix_mix::value::Value;
    use std::future::Future;
    use std::pin::Pin;
    use std::rc::Rc;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicBool, Ordering};

    /// Sentinel whose `Drop` flips a shared atomic. Embedding it in
    /// the Bus-send future under test lets us observe whether the
    /// future was actually discarded on elapsed.
    struct DropFlag {
        flag: Arc<AtomicBool>,
    }
    impl Drop for DropFlag {
        fn drop(&mut self) {
            self.flag.store(true, Ordering::SeqCst);
        }
    }

    struct HangingBusHandler {
        flag: Arc<AtomicBool>,
    }
    impl BusHandler for HangingBusHandler {
        fn send<'a>(
            &'a self,
            _target: &'a str,
            _command: &'a str,
            _args: &'a Value,
        ) -> Pin<Box<dyn Future<Output = MixResult<(i32, Value)>> + 'a>> {
            let drop_flag = DropFlag {
                flag: self.flag.clone(),
            };
            Box::pin(async move {
                // Bind so the DropFlag lives as long as this future.
                let _drop_observer = drop_flag;
                std::future::pending::<()>().await;
                unreachable!("hanging handler must never resolve");
            })
        }
        fn emit<'a>(
            &'a self,
            _t: &'a str,
            _c: &'a str,
            _a: &'a Value,
        ) -> Pin<Box<dyn Future<Output = MixResult<()>> + 'a>> {
            Box::pin(async move { Ok(()) })
        }
        fn port_exists<'a>(
            &'a self,
            _t: &'a str,
        ) -> Pin<Box<dyn Future<Output = MixResult<bool>> + 'a>> {
            Box::pin(async move { Ok(true) })
        }
        fn next_incoming<'a>(
            &'a self,
        ) -> Pin<Box<dyn Future<Output = Option<cosmix_mix::evaluator::IncomingEvent>> + 'a>>
        {
            Box::pin(async move { None })
        }
    }

    let flag = Arc::new(AtomicBool::new(false));
    let handler = Rc::new(HangingBusHandler { flag: flag.clone() });

    let source = r#"
send "downstream" "echo" timeout=0.05
$rc_to = $rc
$result_to = $result
"#;
    let mut eval = Evaluator::new();
    eval.set_bus_handler(handler.clone());
    let mut lexer = Lexer::new(source);
    let stmts = Parser::new(lexer.tokenize().unwrap(), source)
        .parse_program()
        .unwrap();
    eval.execute(&stmts).await.unwrap();

    assert_eq!(
        eval.get_global("rc_to").unwrap(),
        Value::Number(-2.0),
        "elapsed timeout must publish the NUMBER rc=-2 (RC_TIMEOUT) even \
         when the inner future never replies"
    );
    assert_eq!(
        eval.get_global("result_to").unwrap().to_mix_string(),
        "timeout: send to downstream exceeded 0.05s",
        "elapsed timeout must publish the canonical message regardless \
         of the inner future's reply path"
    );
    assert!(
        flag.load(Ordering::SeqCst),
        "the hanging Bus send future MUST be dropped on elapsed — the \
         embedded DropFlag's Drop should have run when the timeout \
         branch returned. A `false` here means the inner future is \
         parked alive (no RAII inside it can fire), which is the \
         pending-leak class the WS4 R1 BLOCKER flagged."
    );
}

// `include` (cosmix-lib-mix 0.3.3): script-relative resolution + load-once
// dedup + fn/var propagation into the caller scope. Distinct from `source`
// (CWD-relative, runs every time), which is left unchanged.
#[tokio::test]
async fn include_dedups_and_resolves_script_relative() {
    let dir = std::env::temp_dir().join("mix_include_test_sr");
    let sub = dir.join("sub");
    std::fs::create_dir_all(&sub).unwrap();
    // library: a load-time side-effect print ('L'), a function, and a var.
    std::fs::write(
        sub.join("lib.mix"),
        "print(\"L\")\nfunction lib_fn($x)\n  return \"lib:\" .. $x\nend\n$LIB_VER = \"1.0\"\n",
    )
    .unwrap();
    let main_path = sub.join("main.mix");
    // include the sibling by BARE name, twice — second must be a no-op.
    std::fs::write(
        &main_path,
        "include \"lib.mix\"\ninclude \"lib.mix\"\nprint(lib_fn(\"ok\") .. $LIB_VER)\n",
    )
    .unwrap();

    let source = std::fs::read_to_string(&main_path).unwrap();
    let mut lexer = Lexer::new(&source);
    let tokens = lexer.tokenize().unwrap();
    let mut parser = Parser::new(tokens, &source);
    let stmts = parser.parse_program().unwrap();

    let stdout = SharedBuf::new();
    let stderr = SharedBuf::new();
    let mut eval = Evaluator::with_output(Box::new(stdout.clone()), Box::new(stderr.clone()));
    // Set the running file so `include` resolves relative to its directory
    // even though the process CWD is elsewhere (the script-relative path).
    eval.set_file(main_path.to_string_lossy().to_string());
    eval.execute(&stmts).await.unwrap();

    let out = stdout.to_string_lossy();
    let _ = std::fs::remove_dir_all(&dir);

    // Dedup: the library body ran exactly once despite two includes.
    assert_eq!(
        out.matches('L').count(),
        1,
        "include must load each file at most once; got: {out:?}"
    );
    // Propagation: the function and the var both crossed into the caller.
    assert!(
        out.contains("lib:ok1.0"),
        "include must propagate fn+var into caller scope; got: {out:?}"
    );
}

// Regression (Codex MAJOR, 0.4.0): a FAILED include must stay retryable —
// the dedup mark is recorded only after read+parse succeed, never on failure.
#[tokio::test]
async fn include_failed_load_is_retryable() {
    let dir = std::env::temp_dir().join("mix_include_test_retry");
    std::fs::create_dir_all(&dir).unwrap();
    let lib = dir.join("late.mix");
    let _ = std::fs::remove_file(&lib); // ensure absent at first include
    let lib_s = lib.to_string_lossy().to_string();

    // First include fails (file absent) and is caught; then the file is
    // written and included again — it MUST load, not silently no-op.
    let src = format!(
        "try\n  include \"{lib}\"\ncatch $e\n  print(\"caught\")\nend\n\
         write_file(\"{lib}\", \"function late_fn()\\n  return \\\"late-ok\\\"\\nend\\n\")\n\
         include \"{lib}\"\nprint(late_fn())\n",
        lib = lib_s
    );
    let mut lexer = Lexer::new(&src);
    let tokens = lexer.tokenize().unwrap();
    let mut parser = Parser::new(tokens, &src);
    let stmts = parser.parse_program().unwrap();

    let stdout = SharedBuf::new();
    let stderr = SharedBuf::new();
    let mut eval = Evaluator::with_output(Box::new(stdout.clone()), Box::new(stderr.clone()));
    eval.execute(&stmts).await.unwrap();

    let out = stdout.to_string_lossy();
    let _ = std::fs::remove_dir_all(&dir);

    assert!(
        out.contains("caught"),
        "first include must fail+be caught; got: {out:?}"
    );
    assert!(
        out.contains("late-ok"),
        "retry after the file appears must LOAD, not silently no-op; got: {out:?}"
    );
}

// ── Dotted-literal Bus address as a send/emit/address target ──
// `send noded.delta.bus <verb>` must parse the dotted address as a literal
// Bus address string (so a mesh address is written directly), NOT as
// field access on the bareword `noded`. `$var`/`(expr)` stay expressions.

fn parse_first(src: &str) -> cosmix_mix::ast::StmtKind {
    let mut lexer = Lexer::new(src);
    let tokens = lexer.tokenize().expect("lex");
    let mut parser = Parser::new(tokens, src);
    let stmts = parser.parse_program().expect("parse");
    stmts.into_iter().next().expect("one stmt").kind
}

#[test]
fn send_target_dotted_bus_address_is_literal() {
    use cosmix_mix::ast::{Expr, StmtKind};
    match parse_first("send noded.delta.bus noded.info\n") {
        StmtKind::Send {
            target, command, ..
        } => {
            match target {
                Expr::StringLiteral(s) => assert_eq!(s, "noded.delta.bus"),
                other => panic!("target must be a literal address, got {other:?}"),
            }
            match command {
                Expr::StringLiteral(s) => assert_eq!(s, "noded.info"),
                other => panic!("verb must be a literal, got {other:?}"),
            }
        }
        other => panic!("expected Send, got {other:?}"),
    }
}

#[test]
fn send_target_single_bareword_still_literal() {
    use cosmix_mix::ast::{Expr, StmtKind};
    match parse_first("send noded noded.ping\n") {
        StmtKind::Send { target, .. } => match target {
            Expr::StringLiteral(s) => assert_eq!(s, "noded"),
            other => panic!("single bareword target must be a literal, got {other:?}"),
        },
        other => panic!("expected Send, got {other:?}"),
    }
}

#[test]
fn send_target_variable_stays_expression() {
    use cosmix_mix::ast::{Expr, StmtKind};
    // A `$var` target must remain an expression (variable read), so
    // string-built addresses keep working.
    match parse_first("send $dest noded.info\n") {
        StmtKind::Send { target, .. } => {
            assert!(
                !matches!(target, Expr::StringLiteral(_)),
                "a $var target must stay an expression, not a literal: {target:?}"
            );
        }
        other => panic!("expected Send, got {other:?}"),
    }
}

#[test]
fn send_target_bareword_call_stays_expression() {
    use cosmix_mix::ast::{Expr, StmtKind};
    // A bareword CALL target (`env("X")`) must stay an expression — the
    // String-then-Dot rule must NOT capture `env` (no following dot) as a
    // literal address and strip the `(...)`.
    match parse_first("send env(\"X\") noded.info\n") {
        StmtKind::Send { target, .. } => assert!(
            !matches!(target, Expr::StringLiteral(_)),
            "a bareword call target must stay an expression: {target:?}"
        ),
        other => panic!("expected Send, got {other:?}"),
    }
}

#[test]
fn send_expr_position_dotted_target_is_literal() {
    use cosmix_mix::ast::{Expr, StmtKind};
    // Expression-position send (`$r = send noded.delta.bus ...`) must ALSO
    // take the literal-address path (the parse_send_expr fix), not field
    // access on `noded`.
    match parse_first("$r = send noded.delta.bus noded.info\n") {
        StmtKind::Assignment { value, .. } => match value {
            Expr::Send { target, .. } => match *target {
                Expr::StringLiteral(s) => assert_eq!(s, "noded.delta.bus"),
                other => panic!("expr-send target must be a literal address: {other:?}"),
            },
            other => panic!("expected Send expr as RHS, got {other:?}"),
        },
        other => panic!("expected Assignment, got {other:?}"),
    }
}

#[test]
fn emit_target_dotted_bus_address_is_literal() {
    use cosmix_mix::ast::{Expr, StmtKind};
    match parse_first("emit world.delta.bus records.changed\n") {
        StmtKind::Emit { target, .. } => match target {
            Expr::StringLiteral(s) => assert_eq!(s, "world.delta.bus"),
            other => panic!("emit target must be a literal address: {other:?}"),
        },
        other => panic!("expected Emit, got {other:?}"),
    }
}

#[test]
fn address_target_dotted_bus_address_is_literal() {
    use cosmix_mix::ast::{Expr, StmtKind};
    match parse_first("address maild.gamma.bus\n  account.list\nend\n") {
        StmtKind::Address { target, .. } => match target {
            Expr::StringLiteral(s) => assert_eq!(s, "maild.gamma.bus"),
            other => panic!("address target must be a literal address: {other:?}"),
        },
        other => panic!("expected Address, got {other:?}"),
    }
}

#[tokio::test]
async fn unicode_escape_decodes_codepoint() {
    // `\u{XXXX}` decodes to the real codepoint (was the literal 8 chars
    // before), so a script can match/strip a zero-width char natively —
    // no perl. Post-P1 `length` is CODEPOINTS (a + FEFF + b = 3); `byte_length`
    // is the byte measure that proves FEFF is a real 3-byte codepoint:
    // a(1) + FEFF(3 UTF-8 bytes) + b(1) = 5.
    let src = r#"$s = "a" .. "\u{FEFF}" .. "b"
print("len=" .. ("" .. length($s)))
print("bytes=" .. ("" .. byte_length($s)))
$c = replace($s, "\u{FEFF}", "")
print("stripped=" .. $c)
print("heart=" .. "\u{2764}")
"#;
    let out = run_mix_capturing(src).await.expect("run");
    assert!(
        out.contains("len=3"),
        "FEFF decodes to ONE codepoint: {out:?}"
    );
    assert!(
        out.contains("bytes=5"),
        "FEFF must be a real 3-byte codepoint: {out:?}"
    );
    assert!(
        out.contains("stripped=ab"),
        "FEFF must strip natively: {out:?}"
    );
    assert!(
        out.contains('\u{2764}'),
        "\\u{{2764}} must decode to ❤: {out:?}"
    );
}

#[tokio::test]
async fn prelude_chars_composes_after_codepoint_flip() {
    // The std prelude's chars() loops `0..length($s)-1` and substr's each index.
    // Before the P1 char-aware flip, length was BYTES and substr was CODEPOINTS,
    // so they did NOT compose: chars("café") yielded ["c","a","f","é",""] — a
    // spurious trailing empty from the out-of-range byte index 4 on a 4-codepoint
    // string. Post-P1 both are codepoints; this is the live-bug repair.
    // (run_mix_capturing doesn't load the prelude, so inline its exact body.)
    let src = r#"function chars($s)
    $result = []
    for $i = 0 to length($s) - 1
        push($result, substr($s, $i, 1))
end
    return $result
end
print("n=" .. ("" .. length(chars("café"))))
print("joined=" .. join(chars("café"), ","))
"#;
    let out = run_mix_capturing(src).await.expect("run");
    assert!(
        out.contains("n=4"),
        "chars(café) must be exactly 4 elements (no trailing empty): {out:?}"
    );
    assert!(
        out.contains("joined=c,a,f,é"),
        "chars(café) must compose to the 4 chars: {out:?}"
    );
}

#[tokio::test]
async fn unicode_escape_single_quoted_stays_literal() {
    // Single-quoted 'raw' strings are fully literal — `\u{FEFF}` there is
    // the 8 literal chars, unchanged (the raw-string contract).
    let out = run_mix_capturing("print('\\u{FEFF}')\n")
        .await
        .expect("run");
    assert!(
        out.contains("\\u{FEFF}"),
        "single-quoted \\u must stay literal: {out:?}"
    );
}

#[tokio::test]
async fn unicode_escape_rejects_bad_forms() {
    // Every malformed braced \u{...} is a loud lexer error, never silent.
    for src in [
        "print(\"\\u{}\")\n",        // empty
        "print(\"\\u{ZZ}\")\n",      // non-hex digit
        "print(\"\\u{D800}\")\n",    // surrogate (invalid scalar)
        "print(\"\\u{110000}\")\n",  // > U+10FFFF
        "print(\"\\u{1234567}\")\n", // > 6 hex digits
    ] {
        assert!(
            run_mix_capturing(src).await.is_err(),
            "malformed unicode escape must be rejected: {src:?}"
        );
    }
}

#[tokio::test]
async fn bare_backslash_u_stays_literal() {
    // `\u` NOT followed by `{` is literal (Windows paths, embedded JSON
    // `\uXXXX`) — the braced-only rule keeps this a pure addition.
    let out = run_mix_capturing("print(\"C:\\users\")\n")
        .await
        .expect("run");
    assert!(
        out.contains("C:\\users"),
        "bare \\u must stay literal: {out:?}"
    );
}

#[tokio::test]
async fn unicode_escape_unterminated_is_rejected() {
    // `\u{` with no closing `}` before the string ends must error loudly,
    // never silently swallow the rest of the string.
    assert!(run_mix_capturing("print(\"\\u{FEFF\")\n").await.is_err());
    assert!(run_mix_capturing("$x = \"\\u{\"\n").await.is_err());
}

#[tokio::test]
async fn radix_integer_literals() {
    // 0o / 0x / 0b yield the f64 value (Mix's single numeric type).
    assert_eq!(run_mix_capturing("print(0o755)\n").await.unwrap(), "493\n");
    assert_eq!(run_mix_capturing("print(0xFF)\n").await.unwrap(), "255\n");
    assert_eq!(run_mix_capturing("print(0b101)\n").await.unwrap(), "5\n");
    assert_eq!(run_mix_capturing("print(0o0)\n").await.unwrap(), "0\n");
    // case-insensitive prefix + `_` digit separators
    assert_eq!(run_mix_capturing("print(0XfF)\n").await.unwrap(), "255\n");
    assert_eq!(
        run_mix_capturing("print(0xFF_FF)\n").await.unwrap(),
        "65535\n"
    );
    // usable in arithmetic / comparison (the mode use case)
    assert_eq!(
        run_mix_capturing("print(0o644 == 420)\n").await.unwrap(),
        "true\n"
    );
}

#[tokio::test]
async fn leading_zero_integer_is_an_error_not_silent_decimal() {
    // The footgun this fix closes: 0755 must NOT silently become decimal 755.
    assert!(run_mix_capturing("print(0755)\n").await.is_err());
    assert!(run_mix_capturing("$m = 007\n").await.is_err());
    // Leading-zero FLOATS are the same hazard class (0755.5 -> 755.5) — also
    // rejected, since the guard checks the integer part only.
    assert!(run_mix_capturing("print(0755.5)\n").await.is_err());
    assert!(run_mix_capturing("print(007.0)\n").await.is_err());
    // Plain 0 and genuine fractions (single-char integer part) are unaffected.
    assert_eq!(run_mix_capturing("print(0)\n").await.unwrap(), "0\n");
    assert_eq!(run_mix_capturing("print(0.5)\n").await.unwrap(), "0.5\n");
    assert_eq!(run_mix_capturing("print(0.0)\n").await.unwrap(), "0\n");
    assert_eq!(run_mix_capturing("print(12.5)\n").await.unwrap(), "12.5\n");
    assert_eq!(run_mix_capturing("print(755)\n").await.unwrap(), "755\n");
}

#[tokio::test]
async fn malformed_radix_literals_are_errors() {
    assert!(run_mix_capturing("print(0x)\n").await.is_err()); // no digits
    assert!(run_mix_capturing("print(0o)\n").await.is_err());
    assert!(run_mix_capturing("print(0b)\n").await.is_err());
    assert!(run_mix_capturing("print(0o8)\n").await.is_err()); // 8 not an octal digit
    assert!(run_mix_capturing("print(0b2)\n").await.is_err()); // 2 not binary
    // A bad digit AFTER a valid one must also reject the whole literal — not
    // split into value + stray token (0o78 != 0o7 then 8). (Codex MAJOR.)
    assert!(run_mix_capturing("print(0o78)\n").await.is_err());
    assert!(run_mix_capturing("print(0b102)\n").await.is_err());
    assert!(run_mix_capturing("print(0x1G)\n").await.is_err());
    // Beyond f64's exact-integer range (2^53) — rejected, not silently rounded.
    assert!(
        run_mix_capturing("print(0x20000000000001)\n")
            .await
            .is_err()
    );
    // But a NON-alphanumeric follower is a legit token boundary.
    assert_eq!(
        run_mix_capturing("print(0xFF + 1)\n").await.unwrap(),
        "256\n"
    );
    assert_eq!(
        run_mix_capturing("print(0o7 .. \"x\")\n").await.unwrap(),
        "7x\n"
    );
}

/// 2026-07-02 audit — send/emit rc-band contract (Codex-ruled).
/// `$rc` is always a NUMBER: 0 ok, >=10 broker/app, -1 transport, -2 timeout,
/// -3 unavailable/no-broker/no-handler. emit is non-fatal on delivery failure.
#[cfg(test)]
mod rc_band_contract_tests {
    use cosmix_mix::error::{MixError, MixResult};
    use cosmix_mix::evaluator::{
        BusFuture, BusHandler, Evaluator, IncomingEvent, RC_TRANSPORT, RC_UNAVAILABLE, SharedBuf,
    };
    use cosmix_mix::lexer::Lexer;
    use cosmix_mix::parser::Parser;
    use cosmix_mix::value::Value;

    async fn run(source: &str, handler: Option<std::rc::Rc<dyn BusHandler>>) -> Evaluator {
        let mut lexer = Lexer::new(source);
        let stmts = Parser::new(lexer.tokenize().unwrap(), source)
            .parse_program()
            .unwrap();
        let mut eval =
            Evaluator::with_output(Box::new(SharedBuf::new()), Box::new(SharedBuf::new()));
        if let Some(h) = handler {
            eval.set_bus_handler(h);
        }
        eval.execute(&stmts).await.expect("script must not abort");
        eval
    }

    /// A handler whose send/emit both return a transport Err, and one whose
    /// send returns Ok((rc, _)) verbatim (to simulate the NeverPresent -3
    /// the real MixBusHandler now emits).
    struct RcHandler {
        send_rc: Option<i32>, // Some(rc) → Ok((rc, Nil)); None → Err
    }
    impl BusHandler for RcHandler {
        fn send<'a>(
            &'a self,
            _t: &'a str,
            _c: &'a str,
            _a: &'a Value,
        ) -> BusFuture<'a, MixResult<(i32, Value)>> {
            let rc = self.send_rc;
            Box::pin(async move {
                match rc {
                    Some(rc) => Ok((rc, Value::Nil)),
                    None => Err(MixError::RuntimeError {
                        span: None,
                        msg: "mesh unavailable: broken".into(),
                    }),
                }
            })
        }
        fn emit<'a>(
            &'a self,
            _t: &'a str,
            _c: &'a str,
            _a: &'a Value,
        ) -> BusFuture<'a, MixResult<()>> {
            // Always an Err — the delivery-failure case emit must NOT abort on.
            Box::pin(async {
                Err(MixError::RuntimeError {
                    span: None,
                    msg: "mesh unavailable: emit broken".into(),
                })
            })
        }
        fn port_exists<'a>(&'a self, _t: &'a str) -> BusFuture<'a, MixResult<bool>> {
            Box::pin(async { Ok(false) })
        }
        fn next_incoming<'a>(&'a self) -> BusFuture<'a, Option<IncomingEvent>> {
            Box::pin(async { None })
        }
    }

    #[tokio::test]
    async fn no_handler_send_is_rc_unavailable_numeric() {
        // No Bus handler registered → RC_UNAVAILABLE (-3), a NUMBER (was the
        // string "-1"), non-fatal (script continues to the print).
        let eval = run("send svc ping\n$after = 1\n", None).await;
        assert_eq!(
            eval.get_global("rc").unwrap(),
            Value::Number(RC_UNAVAILABLE as f64)
        );
        assert_eq!(eval.get_global("after").unwrap(), Value::Number(1.0));
        // The obvious failure check now fires (it silently never did with "-1").
        let e2 = run("send svc ping\n$bad = ($rc == -3)\n", None).await;
        assert_eq!(e2.get_global("bad").unwrap(), Value::Bool(true));
    }

    #[tokio::test]
    async fn send_transport_err_is_rc_transport_numeric_and_nonfatal() {
        let h = std::rc::Rc::new(RcHandler { send_rc: None });
        let eval = run("send svc ping\n$after = 2\n", Some(h)).await;
        assert_eq!(
            eval.get_global("rc").unwrap(),
            Value::Number(RC_TRANSPORT as f64)
        );
        assert_eq!(eval.get_global("after").unwrap(), Value::Number(2.0));
    }

    #[tokio::test]
    async fn send_handler_rc_passthrough_unavailable() {
        // The handler's own -3 (NeverPresent degrade) reaches $rc verbatim.
        let h = std::rc::Rc::new(RcHandler {
            send_rc: Some(RC_UNAVAILABLE),
        });
        let eval = run("send noded.bogus.bus ping\n", Some(h)).await;
        assert_eq!(
            eval.get_global("rc").unwrap(),
            Value::Number(RC_UNAVAILABLE as f64)
        );
    }

    #[tokio::test]
    async fn emit_delivery_failure_is_nonfatal() {
        // emit's handler returns Err — the script must NOT abort and must
        // reach the statement after emit. emit writes no $rc.
        let h = std::rc::Rc::new(RcHandler { send_rc: Some(0) });
        let eval = run("emit svc event\n$after = 3\n", Some(h)).await;
        assert_eq!(eval.get_global("after").unwrap(), Value::Number(3.0));
    }

    #[tokio::test]
    async fn no_handler_emit_is_nonfatal() {
        let eval = run("emit svc event\n$after = 4\n", None).await;
        assert_eq!(eval.get_global("after").unwrap(), Value::Number(4.0));
    }
}
