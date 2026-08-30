//! The `BusCallHandler` seam (`bus_call`). Mirrors the `JmapHandler` /
//! `DbHandler` host-seam pattern: a stub handler is injected, the `bus_call`
//! builtin dispatches to it with the verb + args, and the
//! `CapabilityClass::Bus` gate denies access when the policy withholds it.
//! The mix layer's job is exactly: gate on the capability, validate the
//! args, dispatch to the host handler, and propagate the result/error. The
//! per-route verb allowlist + delegation envelope are the EMBEDDER's job
//! (webd), modelled here only as a stub that refuses an unlisted verb.

use std::cell::RefCell;
use std::rc::Rc;

use cosmix_mix::evaluator::{Evaluator, SharedBuf};
use cosmix_mix::lexer::Lexer;
use cosmix_mix::parser::Parser;
use cosmix_mix::value::Value;
use cosmix_mix::{
    BusCallFuture, BusCallHandler, CapabilityClass, CategoryAllowList, IndexMap, MixError,
    MixResult,
};

/// Records every (verb, args) it is handed. Models an embedder that allows
/// only `maild.vtoken.list` (a stand-in for a route's exact-verb allowlist):
/// any other verb is an `Err` (the refusal a real handler raises before
/// sending). The allowed verb echoes `{verb, got_args, ok}`.
#[derive(Default)]
struct StubBus {
    calls: RefCell<Vec<(String, Value)>>,
}

impl BusCallHandler for StubBus {
    fn call<'a>(&'a self, verb: &'a str, args: &'a Value) -> BusCallFuture<'a, MixResult<Value>> {
        self.calls
            .borrow_mut()
            .push((verb.to_string(), args.clone()));
        let verb = verb.to_string();
        let args = args.clone();
        Box::pin(async move {
            if verb != "maild.vtoken.list" {
                return Err(MixError::RuntimeError {
                    span: None,
                    msg: format!("verb {verb:?} not in this route's allowlist"),
                });
            }
            let mut m = IndexMap::new();
            m.insert("verb".to_string(), Value::String(verb));
            m.insert("got_args".to_string(), args);
            m.insert("ok".to_string(), Value::Bool(true));
            Ok(Value::map(m))
        })
    }
}

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

fn allow_bus(eval: &mut Evaluator, a: Rc<StubBus>) {
    eval.set_bus_call_handler(a);
    eval.set_capability_policy(Rc::new(CategoryAllowList::new(&[CapabilityClass::Bus])));
}

#[tokio::test]
async fn bus_call_dispatches_verb_and_args_and_returns_reply() {
    let a = Rc::new(StubBus::default());
    let a2 = a.clone();
    let out = run_with(
        "bus_call(\"maild.vtoken.list\", { domain: \"example.org\" })\n",
        move |e| allow_bus(e, a2),
    )
    .await
    .expect("bus_call allowed under the Bus capability");
    // Reply map echoed by the stub.
    let Value::Map(m) = &out else {
        panic!("expected a reply map, got {out:?}")
    };
    assert_eq!(
        m.get("verb"),
        Some(&Value::String("maild.vtoken.list".into()))
    );
    assert_eq!(m.get("ok"), Some(&Value::Bool(true)));
    // The verb + args actually reached the handler.
    let calls = a.calls.borrow();
    assert_eq!(calls.len(), 1);
    assert_eq!(calls[0].0, "maild.vtoken.list");
    let Value::Map(got) = &calls[0].1 else {
        panic!("args should be a map")
    };
    assert_eq!(
        got.get("domain"),
        Some(&Value::String("example.org".into()))
    );
}

#[tokio::test]
async fn bus_call_with_no_args_sends_an_empty_map() {
    let a = Rc::new(StubBus::default());
    let a2 = a.clone();
    run_with("bus_call(\"maild.vtoken.list\")\n", move |e| {
        allow_bus(e, a2)
    })
    .await
    .expect("no-args form is allowed");
    let calls = a.calls.borrow();
    // (Value::Map has the same PartialEq gap as Value::List — compare shape.)
    let Value::Map(got) = &calls[0].1 else {
        panic!("args should be a map")
    };
    assert!(got.is_empty(), "no-args form should send an empty args map");
}

#[tokio::test]
async fn bus_call_denied_without_the_bus_capability() {
    // Handler present, but the policy grants only Jmap — the Bus gate fires
    // BEFORE the handler is consulted.
    let a = Rc::new(StubBus::default());
    let a2 = a.clone();
    let err = run_with("bus_call(\"maild.vtoken.list\", {})\n", move |e| {
        e.set_bus_call_handler(a2);
        e.set_capability_policy(Rc::new(CategoryAllowList::new(&[CapabilityClass::Jmap])));
    })
    .await
    .expect_err("bus_call must be denied without the Bus capability");
    assert!(err.contains("capability denied"), "got: {err}");
    assert!(
        a.calls.borrow().is_empty(),
        "handler must not be consulted when denied"
    );
}

#[tokio::test]
async fn bus_call_raises_without_a_handler() {
    // Bus allowed but no handler injected (plain library use).
    let err = run_with("bus_call(\"maild.vtoken.list\", {})\n", |e| {
        e.set_capability_policy(Rc::new(CategoryAllowList::new(&[CapabilityClass::Bus])));
    })
    .await
    .expect_err("bus_call must raise when no handler is registered");
    assert!(err.contains("bus_call not available"), "got: {err}");
}

#[tokio::test]
async fn bus_call_propagates_a_handler_refusal() {
    // The stub refuses any verb other than maild.vtoken.list — the Err
    // propagates as a raise (the script can try/catch it).
    let a = Rc::new(StubBus::default());
    let a2 = a.clone();
    let err = run_with("bus_call(\"maild.vtoken.add\", {})\n", move |e| {
        allow_bus(e, a2)
    })
    .await
    .expect_err("a refused verb must raise");
    assert!(err.contains("not in this route's allowlist"), "got: {err}");
}

#[tokio::test]
async fn bus_call_rejects_bad_arguments() {
    // Non-string verb.
    let a = Rc::new(StubBus::default());
    let a2 = a.clone();
    let err = run_with("bus_call(123, {})\n", move |e| allow_bus(e, a2))
        .await
        .expect_err("non-string verb rejected");
    assert!(err.contains("verb"), "got: {err}");

    // Empty verb.
    let a = Rc::new(StubBus::default());
    let a2 = a.clone();
    let err = run_with("bus_call(\"\", {})\n", move |e| allow_bus(e, a2))
        .await
        .expect_err("empty verb rejected");
    assert!(err.contains("non-empty"), "got: {err}");

    // Non-map args.
    let a = Rc::new(StubBus::default());
    let a2 = a.clone();
    let err = run_with("bus_call(\"maild.vtoken.list\", \"nope\")\n", move |e| {
        allow_bus(e, a2)
    })
    .await
    .expect_err("non-map args rejected");
    assert!(err.contains("args map"), "got: {err}");

    // Too many args.
    let a = Rc::new(StubBus::default());
    let a2 = a.clone();
    let err = run_with("bus_call(\"maild.vtoken.list\", {}, {})\n", move |e| {
        allow_bus(e, a2)
    })
    .await
    .expect_err("extra args rejected");
    assert!(err.contains("verb, args"), "got: {err}");
}
