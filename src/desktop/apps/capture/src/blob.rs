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

/// Per-socket bounds under test: a stalled lane must error in
/// seconds, not the production 30 s, or the bound itself would never
/// be exercised.
fn io_timeout() -> Duration {
    if cfg!(test) {
        Duration::from_secs(1)
    } else {
        IO_TIMEOUT
    }
}

/// Whole-upload deadline under test: short enough that a drip-feed
/// lane crosses it within the test, long enough for the well-behaved
/// lanes the other uploads use.
fn upload_deadline() -> Duration {
    if cfg!(test) {
        Duration::from_secs(2)
    } else {
        UPLOAD_DEADLINE
    }
}
/// Bound on the 201 body: the reference is a few hundred bytes, so a
/// lane answering with a stream is cut short, not slurped.
const REFERENCE_BODY_LIMIT: u64 = 64 * 1024;
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
    pub fn bind(&self) -> Result<String, String> {
        let reply = self
            .handle
            .block_on(tokio::time::timeout(
                PROPS_TIMEOUT,
                self.client
                    .call_typed("blobd", "blob.props.get", json!({"path": "lane"})),
            ))
            .map_err(|_| "blob.props.get on blobd timed out".to_string())?
            .map_err(|e| format!("blob.props.get on blobd: {e}"))?;
        match reply {
            PortReply::Ok { value, .. } => value
                .get("bind")
                .and_then(Value::as_str)
                .map(str::to_string)
                .ok_or_else(|| "props lane carries no bind".into()),
            PortReply::AppError { message, .. } => Err(message),
        }
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

/// The upload body: the file, gated on shutdown and the whole-upload
/// deadline. ureq's copy loop calls `read` between socket writes, so
/// an abandoned or over-deadline upload errors at the next chunk
/// instead of parking the worker until a kernel timeout.
struct GuardedBody {
    file: File,
    shutdown: Arc<AtomicBool>,
    deadline: Instant,
}

impl Read for GuardedBody {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        if self.shutdown.load(Ordering::Relaxed) {
            return Err(std::io::Error::other(
                "upload abandoned: capture is shutting down",
            ));
        }
        if Instant::now() >= self.deadline {
            return Err(std::io::Error::other("upload deadline passed"));
        }
        self.file.read(buf)
    }
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
/// the whole-upload deadline between chunks.
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
            file,
            shutdown,
            deadline,
        })
        .map_err(|e| match e {
            ureq::Error::Status(status, _) => format!("lane answered {status} for {url}"),
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
        fs,
        io::{BufRead, BufReader, Write},
        net::TcpListener,
        sync::mpsc,
    };

    /// A reference body from a well-behaved lane, for tests to tweak.
    fn reference_body(blob: &str, extra: &str) -> String {
        format!("{{\"blob\":\"{blob}\",\"size\":9,\"mime\":\"image/png\"{extra}}}")
    }

    /// A shutdown flag nobody sets: uploads in ordinary tests.
    fn quiet() -> Arc<AtomicBool> {
        Arc::new(AtomicBool::new(false))
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
        rx.recv_timeout(Duration::from_secs(10)).unwrap();
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
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("cosmix-3.png");
        fs::write(&path, b"png bytes").unwrap();
        let shutdown = quiet();
        let mut body = GuardedBody {
            file: File::open(&path).unwrap(),
            shutdown: shutdown.clone(),
            deadline: Instant::now() + UPLOAD_DEADLINE,
        };
        let mut buffer = [0u8; 4];
        assert_eq!(body.read(&mut buffer).unwrap(), 4);
        shutdown.store(true, Ordering::Relaxed);
        let error = body.read(&mut buffer).unwrap_err();
        assert!(error.to_string().contains("shutting down"), "{error}");
    }

    #[test]
    fn an_in_flight_upload_is_abandoned_promptly_at_shutdown() {
        // A lane that drains slowly keeps the guard's checks running
        // between chunks; shutdown set mid-body errors the upload at
        // the next one, not at any socket deadline.
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
                std::thread::sleep(Duration::from_millis(5));
            }
        });
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("cosmix-4.png");
        fs::write(&path, vec![0u8; 4 * 1024 * 1024]).unwrap();
        let shutdown = quiet();
        let flag = shutdown.clone();
        let worker = std::thread::spawn(move || upload(&bind, &path, flag));
        std::thread::sleep(Duration::from_millis(300));
        let abandoned = Instant::now();
        shutdown.store(true, Ordering::Relaxed);
        let error = worker.join().unwrap().unwrap_err();
        assert!(error.contains("shutting down"), "{error}");
        assert!(abandoned.elapsed() < Duration::from_secs(2), "{error}");
    }

    #[test]
    fn the_body_guard_errors_once_the_deadline_passes() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("cosmix-5.png");
        fs::write(&path, b"png bytes").unwrap();
        let mut body = GuardedBody {
            file: File::open(&path).unwrap(),
            shutdown: quiet(),
            deadline: Instant::now(),
        };
        let error = body.read(&mut [0u8; 4]).unwrap_err();
        assert!(error.to_string().contains("deadline passed"), "{error}");
    }

    #[test]
    fn a_lane_that_accepts_and_stalls_errors_at_the_socket_bound() {
        // Accepts and reads the head, then never reads the body: the
        // blocked write must error at the agent's write bound (1 s
        // under test), not sit out the whole-upload deadline.
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
        assert!(started.elapsed() < Duration::from_secs(5));
    }

    #[test]
    fn a_drip_feed_lane_hits_the_whole_upload_deadline() {
        // Drains slower than the deadline: every socket write succeeds
        // within its own bound, so only the between-chunks deadline
        // check ends it (2 s under test).
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
        let path = dir.path().join("cosmix-7.png");
        fs::write(&path, vec![0u8; 1024 * 1024]).unwrap();
        let started = Instant::now();
        let error = upload(&bind, &path, quiet()).unwrap_err();
        assert!(error.contains("deadline passed"), "{error}");
        assert!(started.elapsed() < Duration::from_secs(5), "{error}");
    }
}
