//! `$event` dynamic extent + handler-fault observability (0.63.0).
//!
//! The historical failure shape (hub 2133f4b): a fn called from an `on`
//! handler raised `NAME_UNDEFINED` on `$event`, serve-mode fault
//! isolation swallowed the raise, and the citizen stayed registered and
//! subscribed with noded reporting delivered=1 while its side effects
//! silently never happened — a blind-but-healthy citizen, found after an
//! hour of live bisection. Two fixes, both pinned here: the event is
//! bound with dynamic extent so handler-called fns see it, and an
//! isolated fault now reaches the stderr sink + the serve runtime's
//! health hook. The load-bearing test is raise-then-survive-then-
//! OBSERVABLE — a survive-only test would have passed before the fix.

use std::cell::RefCell;
use std::collections::BTreeMap;
use std::rc::Rc;

use cosmix_mix::evaluator::{Evaluator, IncomingEvent, ReservedOutcome, ServeRuntime, SharedBuf};
use cosmix_mix::lexer::Lexer;
use cosmix_mix::parser::Parser;

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

struct Setup {
    eval: Evaluator,
    stdout: SharedBuf,
    stderr: SharedBuf,
}

async fn setup(source: &str) -> Setup {
    let stdout = SharedBuf::new();
    let stderr = SharedBuf::new();
    let mut eval = Evaluator::with_output(Box::new(stdout.clone()), Box::new(stderr.clone()));
    let mut lexer = Lexer::new(source);
    let stmts = Parser::new(lexer.tokenize().unwrap(), source)
        .parse_program()
        .unwrap();
    eval.execute(&stmts).await.unwrap();
    Setup {
        eval,
        stdout,
        stderr,
    }
}

// ---------- $event dynamic extent ----------

#[tokio::test]
async fn fn_called_from_handler_sees_event() {
    // The exact pre-fix failure: record() reads $event and used to raise
    // NAME_UNDEFINED (function frames fall through to globals only).
    let source = r#"
$recorded = ""
fn describe()
  return $event.command .. "|" .. $event.headers["topic"] .. "|" .. $event.body
end
on play.note
  $recorded = describe()
end
"#;
    let mut s = setup(source).await;
    s.eval
        .dispatch_event(mk_event("play.note", "C4", &[("topic", "music.notes")]))
        .await
        .unwrap();
    assert_eq!(
        s.eval.get_global("recorded").unwrap().to_mix_string(),
        "play.note|music.notes|C4"
    );
    assert!(
        s.stderr.to_string_lossy().is_empty(),
        "no fault: {}",
        s.stderr.to_string_lossy()
    );
}

#[tokio::test]
async fn event_seen_two_calls_deep() {
    let source = r#"
$got = ""
fn inner()
  return $event.body
end
fn outer()
  return inner()
end
on deep.msg
  $got = outer()
end
"#;
    let mut s = setup(source).await;
    s.eval
        .dispatch_event(mk_event("deep.msg", "payload", &[]))
        .await
        .unwrap();
    assert_eq!(s.eval.get_global("got").unwrap().to_mix_string(), "payload");
}

#[tokio::test]
async fn fn_local_event_parameter_still_shadows() {
    // Normal resolution order: a fn's own $event binding wins over the
    // dispatch-scoped one.
    let source = r#"
$got = ""
fn with_own($event)
  return $event
end
on shadow.msg
  $got = with_own("mine")
end
"#;
    let mut s = setup(source).await;
    s.eval
        .dispatch_event(mk_event("shadow.msg", "dispatch-body", &[]))
        .await
        .unwrap();
    assert_eq!(s.eval.get_global("got").unwrap().to_mix_string(), "mine");
}

#[tokio::test]
async fn preexisting_global_event_is_restored_after_dispatch() {
    // The dynamic binding is dispatch-scoped: a top-level $event global
    // (legal outside handlers) survives the dispatch unchanged.
    let source = r#"
$event = "preexisting"
on restore.msg
  $ignore = 1
end
"#;
    let mut s = setup(source).await;
    s.eval
        .dispatch_event(mk_event("restore.msg", "x", &[]))
        .await
        .unwrap();
    assert_eq!(
        s.eval.get_global("event").unwrap().to_mix_string(),
        "preexisting"
    );
}

// ---------- fault observability: raise, survive, OBSERVABLE ----------

/// Test double capturing the health hook.
struct RecordingRuntime {
    faults: RefCell<Vec<String>>,
}

impl ServeRuntime for RecordingRuntime {
    fn handle_reserved(
        &self,
        _command: &str,
        _args_header: Option<&str>,
        _req_body: &str,
        _handler_commands: &[&str],
    ) -> Option<ReservedOutcome> {
        None
    }
    fn record_handler_fault(&self, summary: &str) {
        self.faults.borrow_mut().push(summary.to_string());
    }
}

#[tokio::test]
async fn handler_fault_survives_and_is_observable() {
    let source = r#"
$after = ""
fn blow_up()
  die("deliberate fault")
end
on play.note
  blow_up()
end
on play.note
  $after = "second handler still ran"
end
"#;
    let mut s = setup(source).await;
    let runtime = Rc::new(RecordingRuntime {
        faults: RefCell::new(Vec::new()),
    });
    s.eval.set_serve_runtime(runtime.clone());

    s.eval
        .dispatch_event(mk_event("play.note", "C4", &[]))
        .await
        .unwrap();

    // Survive: the citizen keeps its handlers and later handlers ran.
    assert_eq!(s.eval.handler_count(), 2);
    assert_eq!(
        s.eval.get_global("after").unwrap().to_mix_string(),
        "second handler still ran"
    );
    // Observable half 1: the stderr sink (tracing may be routed nowhere).
    let err = s.stderr.to_string_lossy();
    assert!(
        err.contains("handler fault: play.note[0]"),
        "fault must reach stderr: {err}"
    );
    assert!(err.contains("deliberate fault"), "{err}");
    assert!(err.contains("handler remains registered"), "{err}");
    // Observable half 2: the health hook fired exactly once.
    let faults = runtime.faults.borrow();
    assert_eq!(faults.len(), 1, "{faults:?}");
    assert!(faults[0].contains("play.note[0]"), "{faults:?}");

    // And the citizen still dispatches afterwards.
    drop(faults);
    s.eval
        .dispatch_event(mk_event("play.note", "D4", &[]))
        .await
        .unwrap();
    assert_eq!(runtime.faults.borrow().len(), 2);
    assert!(s.stdout.to_string_lossy().is_empty());
}

#[tokio::test]
async fn healthy_dispatch_records_nothing() {
    let source = "on ok.msg\n  $x = 1\nend\n";
    let mut s = setup(source).await;
    let runtime = Rc::new(RecordingRuntime {
        faults: RefCell::new(Vec::new()),
    });
    s.eval.set_serve_runtime(runtime.clone());
    s.eval
        .dispatch_event(mk_event("ok.msg", "x", &[]))
        .await
        .unwrap();
    assert!(runtime.faults.borrow().is_empty());
    assert!(s.stderr.to_string_lossy().is_empty());
}
