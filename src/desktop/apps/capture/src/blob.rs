//! Dual-write into the node blob store: after `publish()` lands the
//! finished PNG/MP4, the same bytes go to the local node's `blobd`
//! over its byte lane (`POST /blob`, server-hashed) and the returned
//! reference is surfaced additively in `capture.status`.
//!
//! The file under `~/Videos/Cosmix` stays the truth — scripts gate on
//! `is_file($path)` — so the store copy is a second write, never a
//! gate: an upload failure records `blob_error` and the capture keeps
//! its terminal phase. Not done yet: the `captures` collection and
//! dropping the file write.

use cosmix_client::{PortReply, SupervisedClient};
use serde_json::{Value, json};
use std::{
    fs::File,
    io::Read,
    path::Path,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    time::{Duration, Instant},
};

/// Pin owner for lane uploads. The bare service name; a
/// session-qualified owner (`capture:session-12`) comes with the
/// captures collection.
pub const OWNER: &str = "capture";

/// Connect/read/write bounds — the lane's own 30 s idle bound.
const IO_TIMEOUT: Duration = Duration::from_secs(30);
/// Whole-upload deadline: a five-minute MP4 needs headroom, a
/// drip-feed lane does not get one.
const UPLOAD_DEADLINE: Duration = Duration::from_secs(10 * 60);

/// Per-socket bounds under test: far above the upload deadline, so
/// the body guard's own error is the only one that can fire inside
/// a test window. A bound close to the deadline lets a slow box's
/// response-read timeout win the race instead of the guard.
fn io_timeout() -> Duration {
    if cfg!(test) {
        Duration::from_secs(10)
    } else {
        IO_TIMEOUT
    }
}

/// Whole-upload deadline under test: strictly tighter than the test
/// io bounds, short enough that a lane which never reads the body is
/// ended between chunks where the buffers run out — long before the
/// kernel surfaces a blocked write (around 3× the io bound) — and far
/// past anything a well-behaved lane needs.
fn upload_deadline() -> Duration {
    if cfg!(test) {
        Duration::from_secs(1)
    } else {
        UPLOAD_DEADLINE
    }
}
/// Bound on the 201 body: the reference is a few hundred bytes, so a
/// lane answering with a stream is cut short, not slurped.
const REFERENCE_BODY_LIMIT: u64 = 64 * 1024;
/// Bound on an error reply body folded into `blob_error` — enough for
/// blobd's `{"error":"quota: capture"}`, not enough to echo a page.
const ERROR_BODY_LIMIT: u64 = 512;
/// Timeout around the lane-resolution props call: above the 30 s mesh
/// response timeout, below the client's 60 s safety net, so a hung
/// hop becomes `blob_error`, not a parked worker.
const PROPS_TIMEOUT: Duration = Duration::from_secs(35);

/// Resolves the local node's lane bind with one mesh-open
/// `blob.props.get {"path":"lane"}` on the `blobd` service. The call
/// parks the worker thread through the runtime handle — never the
/// Bus loop, which keeps answering `capture.status` while the upload
/// runs.
#[derive(Clone)]
pub struct Lane {
    client: Arc<SupervisedClient>,
    handle: tokio::runtime::Handle,
}

impl Lane {
    pub fn new(client: Arc<SupervisedClient>, handle: tokio::runtime::Handle) -> Self {
        Self { client, handle }
    }

    /// The lane bind (`<ip>:<port>`) blobd proves is its WireGuard
    /// address; capture only ever combines it into `http://<bind>/blob`.
    ///
    /// The timed future is constructed inside `block_on`'s async block:
    /// `tokio::time::timeout` creates its `Sleep` when the future is
    /// built, which needs the runtime's time-driver handle from the
    /// thread-local context — present only once `block_on` has entered
    /// the runtime. This runs on a plain worker thread with no context
    /// of its own (building it outside, as 0.2.3 did, panicked the
    /// first live upload).
    pub fn bind(&self) -> Result<String, String> {
        let reply = self
            .handle
            .block_on(async {
                tokio::time::timeout(
                    PROPS_TIMEOUT,
                    self.client
                        .call_typed("blobd", "blob.props.get", json!({"path": "lane"})),
                )
                .await
            })
            .map_err(|_| "blob.props.get on blobd timed out".to_string())?
            .map_err(|e| format!("blob.props.get on blobd: {e}"))?;
        bind_from_reply(&reply)
    }
}

/// The lane bind out of a `blob.props.get {"path":"lane"}` reply.
/// blobd publishes an empty `bind` until its lane listens, so an
/// empty string is refused here — otherwise it would combine into
/// `http:///blob` and surface as a baffling transport error instead
/// of "the lane is not up (yet)".
fn bind_from_reply(reply: &PortReply) -> Result<String, String> {
    match reply {
        PortReply::Ok { value, .. } => match value.get("bind").and_then(Value::as_str) {
            Some(bind) if !bind.is_empty() => Ok(bind.to_string()),
            Some(_) => Err("blobd lane not listening".into()),
            None => Err("props lane carries no bind".into()),
        },
        PortReply::AppError { message, .. } => Err(message.clone()),
    }
}

/// Mime for a finished capture. Capture writes exactly these two
/// kinds of file; anything else is a bug worth seeing, not sniffing.
fn mime(path: &Path) -> &'static str {
    match path.extension().and_then(|extension| extension.to_str()) {
        Some("mp4") => "video/mp4",
        _ => "image/png",
    }
}

/// The upload body: a reader over the file, gated on shutdown and the
/// whole-upload deadline. ureq's copy loop calls `read` between socket
/// writes — the checks never run inside a write — so an abandoned or
/// over-deadline upload errors at the next chunk instead of parking the
/// worker until a kernel timeout. ureq 2's plain-Content-Length path
/// copies the body through std's 8 KiB `io::copy` buffer, so a chunk is
/// 8 KiB: the chunk plus one socket write (bounded by the agent's write
/// bound) is what bounds abandon latency and deadline overshoot
/// together. The deadline bounds the body write alone: once the last
/// chunk is handed to the kernel the guard has nothing left to gate,
/// and the wait for the lane's reply is bounded by the agent's 30 s
/// socket read bound, not the deadline — a host whose socket buffers
/// swallow the whole body never sees the deadline mid-upload. The
/// clock is injected so the guard's tests advance time per chunk
/// instead of sleeping: whether a drip-paced write blocks at all is a
/// property of the host's buffers, not of the guard.
struct GuardedBody<R: Read> {
    reader: R,
    shutdown: Arc<AtomicBool>,
    deadline: Instant,
    now: Box<dyn Fn() -> Instant>,
}

impl<R: Read> Read for GuardedBody<R> {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        if self.shutdown.load(Ordering::Relaxed) {
            return Err(std::io::Error::other(
                "upload abandoned: capture is shutting down",
            ));
        }
        if (self.now)() >= self.deadline {
            return Err(std::io::Error::other("upload deadline passed"));
        }
        self.reader.read(buf)
    }
}

/// Whether the lane died under the upload — the socket reset, broken
/// or aborted mid-body (blobd quota enforcement closes instead of
/// answering) — as distinct from a timeout or the guard's own
/// abandon/deadline errors.
fn lane_closed(error: &ureq::Error) -> bool {
    let transport = match error {
        ureq::Error::Status(..) => return false,
        ureq::Error::Transport(transport) => transport,
    };
    transport.kind() == ureq::ErrorKind::Io
        && std::error::Error::source(transport)
            .and_then(|source| source.downcast_ref::<std::io::Error>())
            .is_some_and(|io| {
                matches!(
                    io.kind(),
                    std::io::ErrorKind::BrokenPipe
                        | std::io::ErrorKind::ConnectionReset
                        | std::io::ErrorKind::ConnectionAborted
                        | std::io::ErrorKind::UnexpectedEof
                )
            })
}

/// POST the finished file to the lane and return the blob reference.
///
/// The body streams from disk under an explicit `Content-Length` —
/// never chunked: blobd's quota admission reads the declared length
/// before the first byte. The three `X-Cosmix-*` headers are the pin
/// owner and the attributes the lane records. `shutdown` abandons the
/// upload mid-body (the reader above), so process exit never waits
/// out a stalled lane. The request carries no `.timeout()` of its
/// own: in ureq 2 that overrides the agent's per-socket read/write
/// bounds with "time left until the deadline", which is how a
/// lane that accepts and stalls could hold the worker for the whole
/// 10 minutes. The agent's 30 s connect/read/write bound every
/// socket phase (including the 201 reply read); GuardedBody enforces
/// the whole-upload deadline between chunks. A lane that dies under
/// the upload — reset or broken pipe, the true early-413 close — is
/// named as `lane closed during upload` with the quota hint.
pub fn upload(lane_bind: &str, path: &Path, shutdown: Arc<AtomicBool>) -> Result<Value, String> {
    let url = format!("http://{lane_bind}/blob");
    let name = path
        .file_name()
        .and_then(|name| name.to_str())
        .ok_or_else(|| format!("{} has no file name", path.display()))?;
    let file = File::open(path).map_err(|e| format!("open {}: {e}", path.display()))?;
    let length = file
        .metadata()
        .map_err(|e| format!("stat {}: {e}", path.display()))?
        .len();
    let deadline = Instant::now() + upload_deadline();
    let agent = ureq::AgentBuilder::new()
        .timeout_connect(io_timeout())
        .timeout_read(io_timeout())
        .timeout_write(io_timeout())
        .build();
    let response = agent
        .post(&url)
        .set("Content-Length", &length.to_string())
        .set("X-Cosmix-Owner", OWNER)
        .set("X-Cosmix-Name", name)
        .set("X-Cosmix-Mime", mime(path))
        .send(GuardedBody {
            reader: file,
            shutdown,
            deadline,
            now: Box::new(Instant::now),
        })
        .map_err(|e| match e {
            ureq::Error::Status(status, response) => {
                // The refusal body says why (blobd's quota reason); a
                // bounded prefix of it belongs in blob_error. Best
                // effort: an unreadable or non-UTF-8 body just omits.
                let mut body = String::new();
                let _ = response
                    .into_reader()
                    .take(ERROR_BODY_LIMIT)
                    .read_to_string(&mut body);
                if body.is_empty() {
                    format!("lane answered {status} for {url}")
                } else {
                    format!("lane answered {status} for {url}: {body}")
                }
            }
            other if lane_closed(&other) => format!(
                "lane closed during upload (refused? check blobd quota): POST {url}: {other}"
            ),
            other => format!("POST {url}: {other}"),
        })?;
    if Instant::now() >= deadline {
        return Err(format!("upload deadline passed before the {url} reply"));
    }
    if response.status() != 201 {
        return Err(format!("lane answered {} for {url}", response.status()));
    }
    let mut body = String::new();
    response
        .into_reader()
        .take(REFERENCE_BODY_LIMIT)
        .read_to_string(&mut body)
        .map_err(|e| format!("read {url} reply: {e}"))?;
    parse_reference(&body)
}

/// Validate and normalise a lane 201 body into a blob reference,
/// `{"blob":"b3:<64 hex>","size":N,"mime":…,"name":…,"origin":…}`.
/// The checked fields are the ones every consumer keys on; `name`
/// and `origin` ride along when the lane recorded one.
pub fn parse_reference(body: &str) -> Result<Value, String> {
    let value: Value =
        serde_json::from_str(body).map_err(|e| format!("reference is not JSON: {e}"))?;
    let blob = value
        .get("blob")
        .and_then(Value::as_str)
        .ok_or("reference carries no blob id")?;
    let hex = blob
        .strip_prefix("b3:")
        .ok_or("blob id is not b3:-prefixed")?;
    if hex.len() != 64 || !hex.bytes().all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f')) {
        return Err("blob id is not 64 lowercase hex digits".into());
    }
    let size = value
        .get("size")
        .and_then(Value::as_u64)
        .ok_or("reference carries no size")?;
    let mime = value
        .get("mime")
        .and_then(Value::as_str)
        .ok_or("reference carries no mime")?;
    Ok(json!({
        "blob": blob,
        "size": size,
        "mime": mime,
        "name": value.get("name").and_then(Value::as_str),
        "origin": value.get("origin").and_then(Value::as_str),
    }))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{
        cell::Cell,
        fs,
        io::{BufRead, BufReader, Cursor, Write},
        net::TcpListener,
        rc::Rc,
        sync::mpsc,
    };

    /// A `type: response` reply to a wire-format request, carrying the
    /// request's `id` and `command` back — the one correlation the
    /// client's reader matches on. Built by hand (not `BusMessage`) so
    /// the test needs no bus dev-dependency.
    fn bus_response(request: &str, rc: &str) -> String {
        let header = |name: &str| {
            request
                .lines()
                .find_map(|line| line.strip_prefix(&format!("{name}: ")))
                .unwrap_or_default()
                .to_string()
        };
        format!(
            "---\ntype: response\ncommand: {}\nfrom: noded\nid: {}\nrc: {rc}\n---\n",
            header("command"),
            header("id")
        )
    }

    /// A one-shot stub broker: accepts one WebSocket, answers its
    /// `noded.register` rc=0 (the least a `SupervisedClient` needs to
    /// come up), then closes and drops the listener — the client's
    /// outbound lane dies with it, so a later `call_typed` errors
    /// quickly instead of parking.
    async fn register_then_close(listener: tokio::net::TcpListener) {
        let (tcp, _) = listener.accept().await.unwrap();
        drop(listener);
        let mut ws = tokio_tungstenite::accept_async(tcp).await.unwrap();
        let request = match futures_util::StreamExt::next(&mut ws).await {
            Some(Ok(tokio_tungstenite::tungstenite::Message::Text(text))) => text.to_string(),
            _ => return,
        };
        let _ = futures_util::SinkExt::send(
            &mut ws,
            tokio_tungstenite::tungstenite::Message::Text(bus_response(&request, "0").into()),
        )
        .await;
        let _ = ws.close(None).await;
    }

    /// `Lane::bind` from a bare worker thread, in the production shape:
    /// the runtime lives on the thread running the Bus loop; the upload
    /// tail runs on a plain `std::thread` whose only tokio touch is
    /// `bind`'s `handle.block_on`. 0.2.3 built `tokio::time::timeout`'s
    /// `Sleep` when the future was constructed — outside the runtime
    /// context, before `block_on` entered it — so the first live
    /// screenshot panicked the worker and stranded `blob_pending`.
    /// The runtime is driven from THIS thread while the worker runs:
    /// `handle.block_on` parks the worker, and on a current_thread
    /// runtime only a driving thread fires the timers and IO it parks
    /// on. Against a broker that has already died, `bind` must return
    /// `Err` — never panic, never park.
    #[test]
    fn lane_bind_on_a_bare_worker_thread_is_an_error_not_a_panic() {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        let listener = runtime
            .block_on(tokio::net::TcpListener::bind("127.0.0.1:0"))
            .unwrap();
        let url = format!("ws://{}", listener.local_addr().unwrap());
        runtime.spawn(register_then_close(listener));
        let client = Arc::new(
            runtime
                .block_on(
                    cosmix_client::SupervisedClient::connect_options("capture", &url).connect(),
                )
                .unwrap(),
        );
        let lane = Lane::new(client, runtime.handle().clone());
        let worker = std::thread::spawn(move || lane.bind());
        runtime.block_on(async {
            let deadline = tokio::time::Instant::now() + Duration::from_secs(30);
            while !worker.is_finished() && tokio::time::Instant::now() < deadline {
                tokio::time::sleep(Duration::from_millis(2)).await;
            }
        });
        assert!(
            worker.is_finished(),
            "bind did not return within the drive window"
        );
        let error = worker
            .join()
            .expect("Lane::bind must not panic")
            .unwrap_err();
        assert!(error.contains("blob.props.get"), "{error}");
    }

    /// A reference body from a well-behaved lane, for tests to tweak.
    fn reference_body(blob: &str, extra: &str) -> String {
        format!("{{\"blob\":\"{blob}\",\"size\":9,\"mime\":\"image/png\"{extra}}}")
    }

    /// A shutdown flag nobody sets: uploads in ordinary tests.
    fn quiet() -> Arc<AtomicBool> {
        Arc::new(AtomicBool::new(false))
    }

    /// The body behind a drip-paced lane, without the socket: each
    /// read serves the next chunk and advances the shared fake clock
    /// by 50 ms — the time one chunk takes to drain. The guard
    /// samples that clock between chunks exactly as it samples the
    /// real clock between socket writes.
    struct DrippingCursor {
        bytes: Cursor<Vec<u8>>,
        millis: Rc<Cell<u64>>,
    }

    impl Read for DrippingCursor {
        fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
            let n = self.bytes.read(buf)?;
            self.millis.set(self.millis.get() + 50);
            Ok(n)
        }
    }

    /// One-shot loopback lane: accepts a single request, reads it
    /// exactly (head to the blank line, then the Content-Length
    /// body) and answers `status_line` with `reply_body`. The
    /// received head and body reach the test over a channel; the
    /// returned bind is `127.0.0.1:<port>`.
    fn serve_once(
        status_line: &str,
        reply_body: &str,
    ) -> (String, mpsc::Receiver<(String, Vec<u8>)>) {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let bind = listener.local_addr().unwrap().to_string();
        let (tx, rx) = mpsc::channel();
        let status_line = status_line.to_string();
        let reply_body = reply_body.to_string();
        std::thread::spawn(move || {
            let (stream, _) = listener.accept().unwrap();
            let mut reader = BufReader::new(stream);
            let mut head = String::new();
            loop {
                let mut line = String::new();
                reader.read_line(&mut line).unwrap();
                let blank = line.trim_end().is_empty();
                head.push_str(&line);
                if blank {
                    break;
                }
            }
            let length: usize = head
                .lines()
                .filter_map(|line| line.split_once(':'))
                .find(|(name, _)| name.trim().eq_ignore_ascii_case("content-length"))
                .and_then(|(_, value)| value.trim().parse().ok())
                .unwrap_or(0);
            let mut body = vec![0u8; length];
            reader.read_exact(&mut body).unwrap();
            tx.send((head, body)).unwrap();
            let reply = format!(
                "HTTP/1.1 {status_line}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{reply_body}",
                reply_body.len()
            );
            reader.get_mut().write_all(reply.as_bytes()).unwrap();
        });
        (bind, rx)
    }

    #[test]
    fn finished_captures_map_to_exactly_two_mimes() {
        assert_eq!(mime(Path::new("/videos/a.png")), "image/png");
        assert_eq!(mime(Path::new("/videos/a.mp4")), "video/mp4");
    }

    #[test]
    fn an_empty_lane_bind_means_the_lane_is_not_listening() {
        // blobd publishes bind:"" until its lane listens; that must
        // refuse here instead of combining into http:///blob.
        let empty = PortReply::Ok {
            rc: 0,
            value: json!({"bind": ""}),
        };
        let error = bind_from_reply(&empty).unwrap_err();
        assert!(error.contains("blobd lane not listening"), "{error}");
    }

    #[test]
    fn lane_binds_parse_from_the_props_reply_shapes() {
        let listening = PortReply::Ok {
            rc: 0,
            value: json!({"bind": "10.42.0.5:4210", "port": 4210}),
        };
        assert_eq!(bind_from_reply(&listening).unwrap(), "10.42.0.5:4210");
        let without_bind = PortReply::Ok {
            rc: 0,
            value: json!({"port": 4210}),
        };
        assert_eq!(
            bind_from_reply(&without_bind).unwrap_err(),
            "props lane carries no bind"
        );
        let non_string = PortReply::Ok {
            rc: 0,
            value: json!({"bind": 4210}),
        };
        assert_eq!(
            bind_from_reply(&non_string).unwrap_err(),
            "props lane carries no bind"
        );
        let refused = PortReply::AppError {
            rc: 10,
            message: "blobd has no lane".into(),
        };
        assert_eq!(bind_from_reply(&refused).unwrap_err(), "blobd has no lane");
    }

    #[test]
    fn parses_a_lane_reference_and_rejects_malformed_ones() {
        let id = format!("b3:{}", "0".repeat(64));
        let full = parse_reference(&reference_body(
            &id,
            ",\"name\":\"a.png\",\"origin\":\"alpha\"",
        ))
        .unwrap();
        assert_eq!(full["blob"], id.as_str());
        assert_eq!(full["size"], 9);
        assert_eq!(full["mime"], "image/png");
        assert_eq!(full["name"], "a.png");
        assert_eq!(full["origin"], "alpha");
        // name/origin are optional on the wire; the shape carries nulls.
        let bare = parse_reference(&reference_body(&id, "")).unwrap();
        assert!(bare["name"].is_null());
        assert!(bare["origin"].is_null());
        for bad in [
            "not json".to_string(),
            reference_body(&format!("sha:{}", "0".repeat(64)), ""),
            reference_body("b3:short", ""),
            reference_body(&format!("b3:{}", "A".repeat(64)), ""),
            reference_body(&format!("b3:{}", "0".repeat(63)), ""),
            "{\"size\":9,\"mime\":\"image/png\"}".to_string(),
            format!("{{\"blob\":\"{id}\",\"mime\":\"image/png\"}}"),
            format!("{{\"blob\":\"{id}\",\"size\":9}}"),
        ] {
            assert!(parse_reference(&bad).is_err(), "{bad}");
        }
    }

    #[test]
    fn upload_streams_the_file_with_lane_headers() {
        let id = format!("b3:{}", "5".repeat(64));
        let reply = format!(
            "{{\"blob\":\"{id}\",\"size\":9,\"mime\":\"image/png\",\"name\":\"cosmix-1.png\",\"origin\":\"alpha\"}}"
        );
        let (bind, rx) = serve_once("201 Created", &reply);
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("cosmix-1.png");
        let bytes = b"png bytes";
        fs::write(&path, bytes).unwrap();
        let reference = upload(&bind, &path, quiet()).unwrap();
        assert_eq!(reference["blob"], id.as_str());
        assert_eq!(reference["size"], 9);
        assert_eq!(reference["origin"], "alpha");
        let (head, body) = rx.recv_timeout(Duration::from_secs(10)).unwrap();
        assert!(head.starts_with("POST /blob HTTP/1.1\r\n"), "{head}");
        let lower = head.to_ascii_lowercase();
        assert!(lower.contains("content-length: 9\r\n"), "{head}");
        assert!(lower.contains("x-cosmix-owner: capture\r\n"), "{head}");
        assert!(lower.contains("x-cosmix-name: cosmix-1.png\r\n"), "{head}");
        assert!(lower.contains("x-cosmix-mime: image/png\r\n"), "{head}");
        assert!(!lower.contains("transfer-encoding"), "{head}");
        assert_eq!(body, bytes);
    }

    #[test]
    fn an_mp4_uploads_as_video() {
        let id = format!("b3:{}", "6".repeat(64));
        let reply = format!(
            "{{\"blob\":\"{id}\",\"size\":4,\"mime\":\"video/mp4\",\"name\":\"cosmix-1.mp4\",\"origin\":\"alpha\"}}"
        );
        let (bind, rx) = serve_once("201 Created", &reply);
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("cosmix-1.mp4");
        fs::write(&path, b"mp4!").unwrap();
        let reference = upload(&bind, &path, quiet()).unwrap();
        assert_eq!(reference["mime"], "video/mp4");
        let (head, _) = rx.recv_timeout(Duration::from_secs(10)).unwrap();
        assert!(
            head.to_ascii_lowercase()
                .contains("x-cosmix-mime: video/mp4"),
            "{head}"
        );
    }

    #[test]
    fn a_refused_upload_is_an_error_not_a_panic() {
        let (bind, rx) = serve_once("413 Payload Too Large", "{\"error\":\"quota: capture\"}");
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("cosmix-1.png");
        fs::write(&path, b"png bytes").unwrap();
        let error = upload(&bind, &path, quiet()).unwrap_err();
        assert!(error.contains("413"), "{error}");
        assert!(error.contains("quota: capture"), "{error}");
        rx.recv_timeout(Duration::from_secs(10)).unwrap();
    }

    #[test]
    fn an_early_413_carries_the_refusal_body_over_a_multi_mib_upload() {
        // blobd refuses a 413 after the head, before the body. This
        // lane answers the same way and keeps draining so the client
        // finishes its write and reads the refusal; blob_error must
        // name the status or the quota reason. (A lane that refuses
        // AND stops reading instead breaks the client's write — ureq
        // cannot read a response after that, so a true early-413
        // surfaces as a transport error; see the manual.)
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let bind = listener.local_addr().unwrap().to_string();
        std::thread::spawn(move || {
            let (stream, _) = listener.accept().unwrap();
            let mut reader = BufReader::new(stream);
            let mut head = String::new();
            loop {
                let mut line = String::new();
                let blank = {
                    reader.read_line(&mut line).unwrap();
                    line.trim_end().is_empty()
                };
                head.push_str(&line);
                if blank {
                    break;
                }
            }
            let reply = "HTTP/1.1 413 Payload Too Large\r\nContent-Type: application/json\r\nContent-Length: 26\r\nConnection: close\r\n\r\n{\"error\":\"quota: capture\"}";
            use std::io::Write;
            reader.get_mut().write_all(reply.as_bytes()).unwrap();
            let mut scratch = [0u8; 16384];
            loop {
                match reader.read(&mut scratch) {
                    Ok(0) | Err(_) => return,
                    Ok(_) => {}
                }
            }
        });
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("cosmix-8.png");
        fs::write(&path, vec![0u8; 3 * 1024 * 1024]).unwrap();
        let error = upload(&bind, &path, quiet()).unwrap_err();
        assert!(error.contains("413") || error.contains("quota"), "{error}");
    }

    #[test]
    fn an_unreachable_lane_is_an_error_not_a_panic() {
        // Bind then drop: the port answers refused, as an absent
        // blobd lane does.
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let bind = listener.local_addr().unwrap().to_string();
        drop(listener);
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("cosmix-2.png");
        fs::write(&path, b"x").unwrap();
        assert!(upload(&bind, &path, quiet()).is_err());
    }

    #[test]
    fn the_body_guard_errors_once_shutdown_is_set() {
        // Shutdown is checked before every chunk: chunks stream off the
        // reader, the flag lands, and the very next read errors with
        // the guard's own text — the daemon's exit unwinds the upload
        // at that error, within one socket write of the flag.
        let shutdown = quiet();
        let mut body = GuardedBody {
            reader: Cursor::new(vec![0u8; 1024 * 1024]),
            shutdown: shutdown.clone(),
            deadline: Instant::now() + UPLOAD_DEADLINE,
            now: Box::new(Instant::now),
        };
        let mut buffer = [0u8; 8192];
        for _ in 0..3 {
            assert_eq!(body.read(&mut buffer).unwrap(), 8192);
        }
        shutdown.store(true, Ordering::Relaxed);
        let error = body.read(&mut buffer).unwrap_err();
        assert_eq!(
            error.to_string(),
            "upload abandoned: capture is shutting down"
        );
    }

    #[test]
    fn an_in_flight_upload_is_abandoned_promptly_at_shutdown() {
        // The lane drains at a fixed crawl and never finishes inside
        // the test: the writer paces on that drain, so GuardedBody runs
        // between chunks and shutdown set at 200 ms errors the upload
        // at the next chunk with the guard's own text. How soon is
        // bounded by one socket write: the guard never runs inside a
        // write, and cbc2 (twice) had the writer sitting seconds inside
        // a single write while the kernel drained its buffers — the
        // guard fired with the right text at 4.4 s, past a 2 s bound.
        // The daemon's real contract is abandon latency ≤ one socket
        // write ≤ the io bound (10 s under test, 30 s in production),
        // so that is the assertion; the daemon is not tightened to
        // make a 2 s number true. The 16 MiB body is larger than any
        // loopback socket buffering (Linux defaults cap wmem+rmem near
        // 10 MiB), so the pass cannot hinge on the box's buffer sizes
        // — the first cbc2 flake was a 4 MiB body swallowed whole
        // before the flag was set, parking the client on the reply
        // read where the guard never runs.
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let bind = listener.local_addr().unwrap().to_string();
        std::thread::spawn(move || {
            let (stream, _) = listener.accept().unwrap();
            let mut reader = BufReader::new(stream);
            let mut head = String::new();
            loop {
                let mut line = String::new();
                reader.read_line(&mut line).unwrap();
                let blank = line.trim_end().is_empty();
                head.push_str(&line);
                if blank {
                    break;
                }
            }
            let length: usize = head
                .lines()
                .filter_map(|line| line.split_once(':'))
                .find(|(name, _)| name.trim().eq_ignore_ascii_case("content-length"))
                .and_then(|(_, value)| value.trim().parse().ok())
                .unwrap_or(0);
            let mut scratch = [0u8; 8192];
            let mut read = 0;
            while read < length {
                let n = reader.read(&mut scratch).unwrap_or(0);
                if n == 0 {
                    break;
                }
                read += n;
                std::thread::sleep(Duration::from_millis(25));
            }
        });
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("cosmix-4.png");
        fs::write(&path, vec![0u8; 16 * 1024 * 1024]).unwrap();
        let shutdown = quiet();
        let flag = shutdown.clone();
        let worker = std::thread::spawn(move || upload(&bind, &path, flag));
        std::thread::sleep(Duration::from_millis(200));
        let abandoned = Instant::now();
        shutdown.store(true, Ordering::Relaxed);
        let error = worker.join().unwrap().unwrap_err();
        assert!(error.contains("shutting down"), "{error}");
        // One socket write can hold the writer for seconds on a loaded
        // box, so the io bound — not a wish about kernel drain speed —
        // is the honest ceiling.
        assert!(
            abandoned.elapsed() < io_timeout(),
            "{:?}: {error}",
            abandoned.elapsed()
        );
    }

    #[test]
    fn a_lane_that_closes_mid_body_is_named_not_a_bare_transport_error() {
        // blobd quota enforcement closes the connection instead of
        // answering; the manual documents that this surfaces as a
        // transport error. Name it: broken pipe or reset, either on the
        // body write or on the reply read once the RST lands, the
        // operator must read the quota hint, not a bare io error.
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let bind = listener.local_addr().unwrap().to_string();
        std::thread::spawn(move || {
            let (stream, _) = listener.accept().unwrap();
            let mut reader = BufReader::new(stream);
            loop {
                let mut line = String::new();
                if reader.read_line(&mut line).is_err() || line.is_empty() {
                    return;
                }
                let blank = line.trim_end().is_empty();
                if blank {
                    // Head read, body never read: the socket dies with
                    // unread data, so the peer sees a reset.
                    return;
                }
            }
        });
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("cosmix-9.png");
        fs::write(&path, vec![0u8; 2 * 1024 * 1024]).unwrap();
        let error = upload(&bind, &path, quiet()).unwrap_err();
        assert!(
            error.contains("lane closed during upload") && error.contains("check blobd quota"),
            "{error}"
        );
    }

    #[test]
    fn the_body_guard_errors_at_the_first_chunk_past_the_deadline() {
        // Socket-free on purpose: whether a drip-paced write actually
        // blocks — and so whether the deadline gets to run between
        // chunks — is a property of the host's socket buffers, not of
        // the guard. cbc2 and cbc3 swallowed the whole 1 MiB body
        // into kernel buffers before the 1 s deadline could fire
        // once, the client finished writing and sat in the reply read
        // until the io bound. Here the drain pace is injected time
        // instead: each 8 KiB chunk handed over costs 50 ms on the
        // fake clock the guard samples, so the 200 ms deadline lands
        // exactly on the fifth chunk's check — an exact chunk count,
        // where a sleeping test could only bound it from below.
        let epoch = Instant::now(); // arbitrary; only differences matter
        let millis = Rc::new(Cell::new(0u64));
        let mut body = GuardedBody {
            reader: DrippingCursor {
                bytes: Cursor::new(vec![0u8; 1024 * 1024]),
                millis: millis.clone(),
            },
            shutdown: quiet(),
            deadline: epoch + Duration::from_millis(200),
            now: Box::new(move || epoch + Duration::from_millis(millis.get())),
        };
        let mut buffer = [0u8; 8192];
        let mut streamed = 0;
        for _ in 0..4 {
            streamed += body.read(&mut buffer).unwrap();
        }
        assert_eq!(streamed, 4 * 8192);
        let error = body.read(&mut buffer).unwrap_err();
        assert_eq!(error.to_string(), "upload deadline passed");
    }

    #[test]
    fn a_lane_that_accepts_and_stalls_errors_at_the_socket_bound() {
        // A lane that accepts, swallows the body, but never answers:
        // the reply read must error at the agent's read bound (10 s
        // under test), not park the worker until the whole-upload
        // deadline. (A lane that stops reading mid-body errors at the
        // write bound instead, but the kernel surfaces that blocked
        // write only after ~3x the io bound, so it stays a production
        // 30 s guarantee rather than a test.)
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let bind = listener.local_addr().unwrap().to_string();
        std::thread::spawn(move || {
            let (stream, _) = listener.accept().unwrap();
            let mut reader = BufReader::new(stream);
            loop {
                let mut line = String::new();
                if reader.read_line(&mut line).is_err() || line.is_empty() {
                    return;
                }
            }
        });
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("cosmix-6.png");
        fs::write(&path, vec![0u8; 4 * 1024 * 1024]).unwrap();
        let started = Instant::now();
        assert!(upload(&bind, &path, quiet()).is_err());
        assert!(
            started.elapsed() < io_timeout() + Duration::from_secs(2),
            "{:?}",
            started.elapsed()
        );
    }
}
