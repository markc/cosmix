//! Socket acceptance through the real evaluator, on loopback: a Class C
//! handler parks ~2 s in a NUMERIC tcp_recv while a concurrent handler
//! must still run (the read permit is released — the pull yields), and a
//! subscribed numeric handle still answers tcp_send by routing through
//! the owner thread while its frames keep arriving as events. These
//! execute the builtins end-to-end; they do not inspect queue internals.
#![cfg(target_os = "linux")]
use cosmix_mix::{
    MixResult,
    evaluator::{BusFuture, BusHandler, Evaluator, IncomingEvent, ReservedOutcome, ServeRuntime},
    lexer::Lexer,
    parser::Parser,
    value::Value,
};
use std::{
    collections::BTreeMap,
    rc::Rc,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    time::Duration,
};

struct Runtime;
impl ServeRuntime for Runtime {
    fn handle_reserved(
        &self,
        command: &str,
        _: Option<&str>,
        _: &str,
        _: &[(&str, Option<&str>)],
        _: bool,
    ) -> Option<ReservedOutcome> {
        (command == "RELOAD").then(|| ReservedOutcome {
            rc: 0,
            body: "{}".into(),
            quit: false,
            reload: true,
        })
    }
}

struct Bus {
    rx: tokio::sync::Mutex<tokio::sync::mpsc::UnboundedReceiver<IncomingEvent>>,
}
impl BusHandler for Bus {
    fn send<'a>(
        &'a self,
        _: &'a str,
        _: &'a str,
        _: &'a Value,
    ) -> BusFuture<'a, MixResult<(i32, Value)>> {
        Box::pin(async { Ok((0, Value::Nil)) })
    }
    fn emit<'a>(&'a self, _: &'a str, _: &'a str, _: &'a Value) -> BusFuture<'a, MixResult<()>> {
        Box::pin(async { Ok(()) })
    }
    fn port_exists<'a>(&'a self, _: &'a str) -> BusFuture<'a, MixResult<bool>> {
        Box::pin(async { Ok(true) })
    }
    fn next_incoming<'a>(&'a self) -> BusFuture<'a, Option<IncomingEvent>> {
        Box::pin(async { self.rx.lock().await.recv().await })
    }
}

fn event(command: &str) -> IncomingEvent {
    IncomingEvent {
        command: command.into(),
        body: "{}".into(),
        headers: BTreeMap::new(),
    }
}

async fn exec(eval: &mut Evaluator, source: &str) -> MixResult<Value> {
    let tokens = Lexer::new(source).tokenize()?;
    let stmts = Parser::new(tokens, source).parse_program()?;
    eval.execute(&stmts).await
}

fn evaluator() -> (Evaluator, tokio::sync::mpsc::UnboundedSender<IncomingEvent>) {
    let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
    let mut e = Evaluator::new();
    e.set_serve_runtime(Rc::new(Runtime));
    e.set_bus_handler(Rc::new(Bus {
        rx: tokio::sync::Mutex::new(rx),
    }));
    (e, tx)
}

/// `delayed2srecv`: a Class C handler parks ~2 s in a NUMERIC tcp_recv.
/// A concurrent handler must still run while it is parked — its side
/// effect reaches the peer well before the delayed bytes arrive, which a
/// blocking (non-yielding) recv would make impossible (the tick would
/// land after the probe reader's 1.5 s deadline).
#[tokio::test(flavor = "current_thread")]
async fn class_c_numeric_recv_yields_to_a_concurrent_handler() {
    let data_l = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let data_port = data_l.local_addr().unwrap().port();
    let probe_l = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let probe_port = probe_l.local_addr().unwrap().port();
    let ticked = Arc::new(AtomicBool::new(false));
    let server = std::thread::spawn({
        let ticked = ticked.clone();
        move || {
            use std::io::{Read, Write};
            let (mut data, _) = data_l.accept().unwrap();
            let (mut probe, _) = probe_l.accept().unwrap();
            let flag = ticked.clone();
            let probe_reader = std::thread::spawn(move || {
                probe
                    .set_read_timeout(Some(Duration::from_millis(1500)))
                    .unwrap();
                let mut buf = [0u8; 8];
                if let Ok(n) = probe.read(&mut buf) {
                    flag.store(&buf[..n] == b"tick", Ordering::Relaxed);
                }
            });
            // The delayed 2 s recv: the bytes arrive only after this
            // pause, so a blocking recv would hold the pump and the tick
            // would miss the probe deadline.
            std::thread::sleep(Duration::from_millis(2000));
            data.write_all(b"payload").unwrap();
            probe_reader.join().unwrap();
            std::thread::sleep(Duration::from_millis(200));
        }
    });
    let (mut e, tx) = evaluator();
    let received = Rc::new(std::cell::RefCell::new(None));
    let sink = received.clone();
    e.register(
        "observe_recv",
        cosmix_mix::sync_ext(move |args| {
            *sink.borrow_mut() = args.first().cloned();
            Ok(Value::Nil)
        }),
    );
    e.set_global("port", Value::Number(data_port as f64));
    e.set_global("pport", Value::Number(probe_port as f64));
    exec(
        &mut e,
        r#"
$h = tcp_connect("127.0.0.1", $port, {timeout: 5})
$probe = tcp_connect("127.0.0.1", $pport, {timeout: 5})
$result = nil
$ticks = 0
on cmd1 async
    $result = tcp_recv($h, {timeout: 6})
    observe_recv($result)
    quit()
end
on cmd2
    $ticks = $ticks + 1
    tcp_send($probe, "tick")
end
"#,
    )
    .await
    .unwrap();
    let local = tokio::task::LocalSet::new();
    local
        .run_until(async move {
            let driver = tokio::task::spawn_local({
                let tx = tx.clone();
                async move {
                    tokio::time::sleep(Duration::from_millis(100)).await;
                    tx.send(event("cmd1")).unwrap();
                    tokio::time::sleep(Duration::from_millis(300)).await;
                    tx.send(event("cmd2")).unwrap();
                }
            });
            tokio::time::timeout(Duration::from_secs(120), e.run_event_pump())
                .await
                .expect("pump deadline")
                .unwrap();
            driver.await.unwrap();
            assert!(
                ticked.load(Ordering::Relaxed),
                "the concurrent tick ran while the recv was parked"
            );
            let observed = received.borrow();
            let Some(Value::Bytes(b)) = observed.as_ref() else {
                panic!("receive must return bytes: {observed:?}");
            };
            assert_eq!(b.as_slice(), b"payload");
            assert_eq!(e.get_global("ticks").unwrap().to_number().unwrap(), 1.0);
        })
        .await;
    server.join().unwrap();
}

/// Same-connection send after subscribing, through the real evaluator:
/// frames arrive as events AND the subscribed numeric handle answers
/// tcp_send by routing through the owner thread, in FIFO order around
/// the send.
#[tokio::test(flavor = "current_thread")]
async fn subscribed_handle_sends_from_a_handler_and_fifo_keeps_flowing() {
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    let (sent_tx, sent_rx) = std::sync::mpsc::channel::<Vec<u8>>();
    let server = std::thread::spawn(move || {
        use std::io::{Read, Write};
        let (mut stream, _) = listener.accept().unwrap();
        stream.write_all(b"ping").unwrap();
        let mut buf = vec![0u8; 8];
        let n = stream.read(&mut buf).unwrap();
        sent_tx.send(buf[..n].to_vec()).unwrap();
        stream.write_all(b"reply").unwrap();
        std::thread::sleep(Duration::from_millis(300));
    });
    let (mut e, _tx) = evaluator();
    e.set_global("port", Value::Number(port as f64));
    exec(
        &mut e,
        r#"
$h = tcp_connect("127.0.0.1", $port, {timeout: 5})
$h2 = tcp_on($h, "rcv", {frame: "bytes"})
$sent = nil
$echo = nil
on rcv
    if $event.args.frame.data.hex == "70696e67" then
        $sent = tcp_send($h, "pong")
    else
        $echo = $event.args.frame.data.hex
        quit()
    end
end
"#,
    )
    .await
    .unwrap();
    tokio::time::timeout(Duration::from_secs(120), e.run_event_pump())
        .await
        .expect("pump deadline")
        .unwrap();
    assert_eq!(e.get_global("sent").unwrap().to_number().unwrap(), 4.0);
    assert_eq!(e.get_global("echo").unwrap().to_mix_string(), "7265706c79");
    assert_eq!(
        sent_rx.recv_timeout(Duration::from_secs(5)).unwrap(),
        b"pong"
    );
    server.join().unwrap();
}
