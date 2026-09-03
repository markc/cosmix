//! `publish(topic, body[, opts])` (0.63.0) — the one-call topic publish
//! that replaces the two recorded traps: the hand-built `---\n…\n---\n`
//! wire frame, and `send noded topic.publish` needing `body=` before
//! `name=`/`retain=` would header-route. Asserts the EXACT frame and
//! args map handed to the bus, not merely that a send happened.

use std::cell::RefCell;
use std::future::Future;
use std::pin::Pin;
use std::rc::Rc;

use cosmix_mix::error::MixResult;
use cosmix_mix::evaluator::{BusHandler, Evaluator, SharedBuf};
use cosmix_mix::lexer::Lexer;
use cosmix_mix::parser::Parser;
use cosmix_mix::value::Value;

/// Records every send; replies (7, "accepted") so rc propagation is
/// distinguishable from a default 0.
struct RecordingBus {
    sends: RefCell<Vec<(String, String, Value)>>,
}

impl BusHandler for RecordingBus {
    fn send<'a>(
        &'a self,
        target: &'a str,
        command: &'a str,
        args: &'a Value,
    ) -> Pin<Box<dyn Future<Output = MixResult<(i32, Value)>> + 'a>> {
        Box::pin(async move {
            self.sends
                .borrow_mut()
                .push((target.to_string(), command.to_string(), args.clone()));
            Ok((7, Value::String("accepted".to_string())))
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
    ) -> Pin<Box<dyn Future<Output = Option<cosmix_mix::evaluator::IncomingEvent>> + 'a>> {
        Box::pin(async move { None })
    }
}

async fn run_with_bus(source: &str) -> (Evaluator, Rc<RecordingBus>, String) {
    let stdout = SharedBuf::new();
    let stderr = SharedBuf::new();
    let mut eval = Evaluator::with_output(Box::new(stdout.clone()), Box::new(stderr.clone()));
    let bus = Rc::new(RecordingBus {
        sends: RefCell::new(Vec::new()),
    });
    eval.set_bus_handler(bus.clone());
    let mut lexer = Lexer::new(source);
    let stmts = Parser::new(lexer.tokenize().unwrap(), source)
        .parse_program()
        .unwrap();
    eval.execute(&stmts).await.expect("source should run");
    (eval, bus, stdout.to_string_lossy())
}

fn map_str(args: &Value, key: &str) -> String {
    match args {
        Value::Map(m) => m.get(key).map(|v| v.to_mix_string()).unwrap_or_default(),
        _ => panic!("args not a map: {args:?}"),
    }
}

#[tokio::test]
async fn publish_builds_the_wire_frame_and_routes_via_noded() {
    let src = "$rc_out = publish(\"comp.corner.entered\", \"{\\\"corner\\\":\\\"tl\\\"}\")\nprint($rc_out .. \"/\" .. $rc .. \"/\" .. $result)\n";
    let (_eval, bus, out) = run_with_bus(src).await;
    // rc returned AND $rc/$result set, like `send`.
    assert_eq!(out, "7/7/accepted\n");
    let sends = bus.sends.borrow();
    assert_eq!(sends.len(), 1);
    let (target, command, args) = &sends[0];
    assert_eq!(target, "noded");
    assert_eq!(command, "topic.publish");
    assert_eq!(map_str(args, "name"), "comp.corner.entered");
    assert_eq!(map_str(args, "retain"), "false");
    assert_eq!(
        map_str(args, "body"),
        "---\ncommand: comp.corner.entered\n---\n{\"corner\":\"tl\"}"
    );
}

#[tokio::test]
async fn publish_opts_command_override_headers_and_retain() {
    // The fleet's real shape: topic `svc.corner.entered`, inner frame
    // command `corner.entered`, an extra event_seq header, retained.
    let src = "publish(\"comp.corner.entered\", \"payload\", {retain: true, command: \"corner.entered\", headers: {event_seq: 1042}})\n";
    let (_eval, bus, _) = run_with_bus(src).await;
    let sends = bus.sends.borrow();
    let (_, _, args) = &sends[0];
    assert_eq!(map_str(args, "retain"), "true");
    assert_eq!(
        map_str(args, "body"),
        "---\ncommand: corner.entered\nevent_seq: 1042\n---\npayload"
    );
}

#[tokio::test]
async fn publish_refuses_the_silent_wrong_shapes() {
    // Map body: no hidden encoding — the caller chooses the wire format.
    let (_eval, bus, out) = run_with_bus(
        "try\n  publish(\"t\", {a: 1})\ncatch $e\n  print(\"map:\" .. contains(\"\" .. $e, \"json_encode\"))\nend\ntry\n  publish(\"bad\\ntopic\", \"x\")\ncatch $e\n  print(\"nl:\" .. contains(\"\" .. $e, \"newline\"))\nend\ntry\n  publish(\"t\", \"x\", {bogus: 1})\ncatch $e\n  print(\"opt:\" .. contains(\"\" .. $e, \"unknown option\"))\nend\ntry\n  publish(\"t\", \"x\", {headers: {\"k:ey\": \"v\"}})\ncatch $e\n  print(\"hdr:\" .. contains(\"\" .. $e, \"frame-injection\"))\nend\n",
    )
    .await;
    assert_eq!(out, "map:true\nnl:true\nopt:true\nhdr:true\n");
    assert!(bus.sends.borrow().is_empty(), "nothing reached the bus");
}

#[tokio::test]
async fn publish_without_bus_degrades_like_send() {
    // No handler installed: RC_UNAVAILABLE (-3), non-fatal, $result says why.
    let stdout = SharedBuf::new();
    let stderr = SharedBuf::new();
    let mut eval = Evaluator::with_output(Box::new(stdout.clone()), Box::new(stderr.clone()));
    let src = "print(publish(\"t\", \"x\") .. \"/\" .. $rc)\n";
    let mut lexer = Lexer::new(src);
    let stmts = Parser::new(lexer.tokenize().unwrap(), src)
        .parse_program()
        .unwrap();
    eval.execute(&stmts).await.unwrap();
    assert_eq!(stdout.to_string_lossy(), "-3/-3\n");
}
