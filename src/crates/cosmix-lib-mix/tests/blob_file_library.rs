//! Exercise the shipped module with the evaluator's Bus seam and a loopback
//! byte lane. No daemon dependency or substitute production transport.
#![cfg(all(
    feature = "http",
    feature = "crypto",
    feature = "json",
    feature = "regex",
    feature = "url"
))]

use cosmix_mix::evaluator::{BusFuture, BusHandler, Evaluator, IncomingEvent, SharedBuf};
use cosmix_mix::lexer::Lexer;
use cosmix_mix::parser::Parser;
use cosmix_mix::value::Value;
use cosmix_mix::{IndexMap, MixResult};
use std::cell::Cell;
use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::time::{Duration, Instant};

const LIBRARY: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/../cosmix-blobd/mix/blob.mix");
const ID: &str = "00000000-0000-0000-0000-000000000001";

struct Temp(PathBuf);
impl Temp {
    fn new() -> Self {
        let path =
            std::env::temp_dir().join(format!("mix-blob-library-{:032x}", rand::random::<u128>()));
        std::fs::create_dir(&path).unwrap();
        Self(path)
    }
}
impl Drop for Temp {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

struct Bus {
    bind: String,
    pinned: bool,
    discovery: Cell<usize>,
}
impl BusHandler for Bus {
    fn send<'a>(
        &'a self,
        target: &'a str,
        command: &'a str,
        args: &'a Value,
    ) -> BusFuture<'a, MixResult<(i32, Value)>> {
        assert_eq!(target, "blobd-test");
        Box::pin(async move {
            let value = match command {
                "blob.stat" => Value::map(IndexMap::from([
                    ("present".into(), Value::Bool(self.pinned)),
                    (
                        "pins".into(),
                        Value::list(if self.pinned {
                            vec![Value::String("tester".into())]
                        } else {
                            vec![]
                        }),
                    ),
                    ("size".into(), Value::Number(4.0)),
                    (
                        "mime".into(),
                        Value::String("application/octet-stream".into()),
                    ),
                    ("origin".into(), Value::String("test-node".into())),
                ])),
                "blob.props.get" => {
                    let Value::Map(args) = args else {
                        panic!("expected map")
                    };
                    assert_eq!(args["path"], Value::String("lane.bind".into()));
                    self.discovery.set(self.discovery.get() + 1);
                    Value::String(self.bind.clone())
                }
                other => panic!("unexpected Bus verb {other}"),
            };
            Ok((0, value))
        })
    }
    fn emit<'a>(&'a self, _: &'a str, _: &'a str, _: &'a Value) -> BusFuture<'a, MixResult<()>> {
        Box::pin(async { Ok(()) })
    }
    fn port_exists<'a>(&'a self, _: &'a str) -> BusFuture<'a, MixResult<bool>> {
        Box::pin(async { Ok(true) })
    }
    fn next_incoming<'a>(&'a self) -> BusFuture<'a, Option<IncomingEvent>> {
        Box::pin(async { None })
    }
}

fn quoted(path: &Path) -> String {
    serde_json::to_string(&path.to_string_lossy()).unwrap()
}

async fn execute(bus: Rc<Bus>, script: &str) -> serde_json::Value {
    let source = format!(
        "$b = require({})\n{script}\nprint(json_encode($out))\n",
        quoted(Path::new(LIBRARY))
    );
    let tokens = Lexer::new(&source).tokenize().unwrap();
    let stmts = Parser::new(tokens, &source).parse_program().unwrap();
    let stdout = SharedBuf::new();
    let mut eval = Evaluator::with_output(Box::new(stdout.clone()), Box::new(SharedBuf::new()));
    eval.set_bus_handler(bus);
    eval.execute(&stmts).await.unwrap();
    serde_json::from_str(&stdout.to_string_lossy()).unwrap()
}

async fn upload(bus: Rc<Bus>, dir: &Path) -> serde_json::Value {
    execute(bus,&format!("$out = $b.blob_upload_file({}, {{service:\"blobd-test\", owner:\"tester\", resume_file:{}, chunk:2}})",quoted(&dir.join("source")),quoted(&dir.join("resume.json")))).await
}

fn request(listener: &TcpListener) -> (TcpStream, String, Vec<u8>) {
    let until = Instant::now() + Duration::from_secs(10);
    let mut socket = loop {
        match listener.accept() {
            Ok((s, _)) => break s,
            Err(e) if e.kind() == std::io::ErrorKind::WouldBlock && Instant::now() < until => {
                std::thread::sleep(Duration::from_millis(5))
            }
            Err(e) => panic!("lane accept: {e}"),
        }
    };
    socket
        .set_read_timeout(Some(Duration::from_secs(5)))
        .unwrap();
    let mut header = Vec::new();
    while !header.ends_with(b"\r\n\r\n") {
        let mut byte = [0];
        socket.read_exact(&mut byte).unwrap();
        header.push(byte[0]);
        assert!(header.len() < 65536);
    }
    let header = String::from_utf8(header).unwrap();
    let length = header
        .lines()
        .find_map(|s| {
            s.to_ascii_lowercase()
                .strip_prefix("content-length:")
                .map(|n| n.trim().parse::<usize>().unwrap())
        })
        .unwrap_or(0);
    let mut body = vec![0; length];
    socket.read_exact(&mut body).unwrap();
    (socket, header, body)
}

fn reply(socket: &mut TcpStream, status: u16, headers: &str, body: &str) {
    socket.write_all(format!("HTTP/1.1 {status} Test\r\nConnection: close\r\nContent-Length: {}\r\n{headers}\r\n{body}",body.len()).as_bytes()).unwrap();
}

#[tokio::test]
async fn blob_upload_owner_pin_short_circuits_before_lane_and_record() {
    let dir = Temp::new();
    std::fs::write(dir.0.join("source"), b"abcd").unwrap();
    let bus = Rc::new(Bus {
        bind: "unused".into(),
        pinned: true,
        discovery: Cell::new(0),
    });
    let result = upload(Rc::clone(&bus), &dir.0).await;
    assert_eq!(result["ok"], true);
    assert_eq!(result["result"]["size"], 4);
    assert_eq!(bus.discovery.get(), 0);
    assert!(!dir.0.join("resume.json").exists());
}

#[tokio::test]
async fn blob_upload_lost_create_reply_persisted_key_409_head_and_commit_replay() {
    let dir = Temp::new();
    std::fs::write(dir.0.join("source"), b"abcd").unwrap();
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    listener.set_nonblocking(true).unwrap();
    let bus = Rc::new(Bus {
        bind: listener.local_addr().unwrap().to_string(),
        pinned: false,
        discovery: Cell::new(0),
    });
    let resume = dir.0.join("resume.json");
    let server = std::thread::spawn(move || {
        let mut key = String::new();
        let hash = blake3::hash(b"abcd").to_hex().to_string();
        for step in 0..9 {
            let (mut socket, head, body) = request(&listener);
            let saved: serde_json::Value =
                serde_json::from_slice(&std::fs::read(&resume).unwrap()).unwrap();
            assert_eq!(saved["blake3"], hash);
            assert_eq!(saved["size"], 4);
            if step == 0 {
                key = saved["key"].as_str().unwrap().to_owned();
            }
            assert_eq!(saved["key"], key);
            let metadata = |offset| {
                format!(
                    "X-Cosmix-Offset: {offset}\r\nX-Cosmix-Size: 4\r\nX-Cosmix-Owner: tester\r\nX-Cosmix-Mime: application/octet-stream\r\nX-Cosmix-Expect: b3:{hash}\r\nX-Cosmix-Upload-Key: {key}\r\nX-Cosmix-State: active\r\n"
                )
            };
            match step {
                0 => {
                    assert!(head.starts_with("POST /blob/uploads "));
                    assert!(saved["upload"].is_null());
                } // lost response
                1 => {
                    assert!(head.starts_with("POST /blob/uploads "));
                    assert!(head.contains(&key));
                    reply(&mut socket, 200, "", &format!("{{\"upload\":\"{ID}\"}}"));
                }
                2 | 4 | 6 => {
                    assert!(head.starts_with(&format!("HEAD /blob/uploads/{ID} ")));
                    assert_eq!(saved["upload"], ID);
                    reply(&mut socket, 200, &metadata(step - 2), "");
                }
                3 => {
                    assert!(head.starts_with("PATCH "));
                    assert_eq!(body, b"ab");
                    reply(&mut socket, 409, "X-Cosmix-Offset: 2\r\n", "{}");
                }
                5 => {
                    assert!(head.starts_with("PATCH "));
                    assert_eq!(body, b"cd");
                    assert!(
                        head.to_ascii_lowercase()
                            .contains("content-range: bytes 2-3/4")
                    );
                    reply(&mut socket, 200, &metadata(4), "{}");
                }
                7 => {
                    assert!(head.starts_with(&format!("POST /blob/uploads/{ID}/commit ")));
                } // lost commit response
                8 => {
                    assert!(head.starts_with(&format!("POST /blob/uploads/{ID}/commit ")));
                    reply(
                        &mut socket,
                        200,
                        "",
                        &format!("{{\"blob\":\"b3:{hash}\",\"size\":4}}"),
                    );
                }
                _ => unreachable!(),
            }
        }
    });
    let first = upload(Rc::clone(&bus), &dir.0).await;
    assert_eq!(first["ok"], false, "{first}");
    let second = upload(bus, &dir.0).await;
    server.join().unwrap();
    assert_eq!(second["ok"], true, "{second}");
    assert_eq!(second["result"]["size"], 4);
}

#[tokio::test]
async fn blob_download_discovers_target_lane_and_verifies_reference_hash() {
    let dir = Temp::new();
    let path = dir.0.join("target");
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    listener.set_nonblocking(true).unwrap();
    let bus = Rc::new(Bus {
        bind: listener.local_addr().unwrap().to_string(),
        pinned: false,
        discovery: Cell::new(0),
    });
    let hash = blake3::hash(b"abcd").to_hex().to_string();
    let expected = hash.clone();
    let server = std::thread::spawn(move || {
        let (mut socket, head, _) = request(&listener);
        assert!(head.starts_with(&format!("GET /blob/{expected} ")));
        reply(&mut socket, 200, "", "abcd");
    });
    let result = execute(
        bus,
        &format!(
            "$out = $b.blob_download_file(\"b3:{hash}\", {}, {{service:\"blobd-test\"}})",
            quoted(&path)
        ),
    )
    .await;
    server.join().unwrap();
    assert_eq!(result["ok"], true, "{result}");
    assert_eq!(std::fs::read(path).unwrap(), b"abcd");
}

#[tokio::test]
async fn blob_upload_empty_commits_without_patch() {
    let dir = Temp::new();
    std::fs::write(dir.0.join("source"), b"").unwrap();
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    listener.set_nonblocking(true).unwrap();
    let bus = Rc::new(Bus {
        bind: listener.local_addr().unwrap().to_string(),
        pinned: false,
        discovery: Cell::new(0),
    });
    let resume = dir.0.join("resume.json");
    let server = std::thread::spawn(move || {
        let hash = blake3::hash(b"").to_hex().to_string();
        for step in 0..3 {
            let (mut socket, head, body) = request(&listener);
            assert!(body.is_empty());
            let saved: serde_json::Value =
                serde_json::from_slice(&std::fs::read(&resume).unwrap()).unwrap();
            let key = saved["key"].as_str().unwrap();
            match step {
                0 => {
                    assert!(head.starts_with("POST /blob/uploads "));
                    reply(&mut socket, 201, "", &format!("{{\"upload\":\"{ID}\"}}"));
                }
                1 => {
                    assert!(head.starts_with("HEAD "));
                    reply(
                        &mut socket,
                        200,
                        &format!(
                            "X-Cosmix-Offset: 0\r\nX-Cosmix-Size: 0\r\nX-Cosmix-Owner: tester\r\nX-Cosmix-Mime: application/octet-stream\r\nX-Cosmix-Expect: b3:{hash}\r\nX-Cosmix-Upload-Key: {key}\r\nX-Cosmix-State: active\r\n"
                        ),
                        "",
                    );
                }
                2 => {
                    assert!(head.starts_with(&format!("POST /blob/uploads/{ID}/commit ")));
                    reply(
                        &mut socket,
                        201,
                        "",
                        &format!("{{\"blob\":\"b3:{hash}\",\"size\":0}}"),
                    );
                }
                _ => unreachable!(),
            }
        }
    });
    let result = upload(bus, &dir.0).await;
    server.join().unwrap();
    assert_eq!(result["ok"], true, "{result}");
}

#[tokio::test]
async fn blob_upload_mismatched_record_refuses_before_lane_discovery() {
    let dir = Temp::new();
    std::fs::write(dir.0.join("source"), b"abcd").unwrap();
    std::fs::write(dir.0.join("resume.json"), b"{\"owner\":\"somebody-else\"}").unwrap();
    let bus = Rc::new(Bus {
        bind: "unused".into(),
        pinned: false,
        discovery: Cell::new(0),
    });
    let result = upload(Rc::clone(&bus), &dir.0).await;
    assert_eq!(result["ok"], false);
    assert_eq!(bus.discovery.get(), 0);
    assert_eq!(
        std::fs::read(dir.0.join("resume.json")).unwrap(),
        b"{\"owner\":\"somebody-else\"}"
    );
}
