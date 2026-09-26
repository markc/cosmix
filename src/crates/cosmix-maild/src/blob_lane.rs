//! Local blobd byte lane. Bus carries discovery and references, never new byte
//! payloads. Origin in an input reference is provenance, not a routing target.

use cosmix_bus::PortReply;
use cosmix_client::NodedClient;
use reqwest::{Client, Response};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::{future::Future, time::Duration};
use tokio::{
    sync::watch,
    time::{Instant, sleep_until, timeout},
};

const BUS_TIMEOUT: Duration = Duration::from_secs(10);
const IO_TIMEOUT: Duration = Duration::from_secs(30);
const REFERENCE_LIMIT: usize = 64 * 1024;

pub trait Discovery: Send + Sync {
    fn bind(&self) -> impl Future<Output = Result<String, String>> + Send;
    fn quota(&self, owner: &str) -> impl Future<Output = Result<Value, String>> + Send;
}

impl Discovery for NodedClient {
    async fn bind(&self) -> Result<String, String> {
        let value = call(self, "blob.props.get", json!({"path": "lane"})).await?;
        let bind = value["bind"]
            .as_str()
            .ok_or("lane_unavailable: props lane carries no bind")?;
        checked_bind(bind).map(str::to_owned)
    }

    async fn quota(&self, owner: &str) -> Result<Value, String> {
        call(self, "blob.quota", json!({"owner": owner})).await
    }
}

async fn call(client: &NodedClient, verb: &str, args: Value) -> Result<Value, String> {
    match timeout(BUS_TIMEOUT, client.call_typed("blobd", verb, args))
        .await
        .map_err(|_| format!("lane_unavailable: {verb} on blobd timed out"))?
        .map_err(|e| format!("lane_unavailable: {verb} on blobd: {e}"))?
    {
        PortReply::Ok { value, .. } => Ok(value),
        PortReply::AppError { message, .. } => Err(format!("lane_unavailable: {verb}: {message}")),
    }
}

fn checked_bind(bind: &str) -> Result<&str, String> {
    if bind.is_empty() {
        return Err("lane_unavailable: blobd lane not listening".into());
    }
    bind.parse::<std::net::SocketAddr>()
        .map_err(|_| "lane_unavailable: invalid lane bind")?;
    Ok(bind)
}

/// Only canonical b3 IDs (or an object's blob member). ASCII validation must
/// precede any MDS hex parser; that parser slices at byte offsets.
pub fn blob_hex(value: &Value) -> Result<&str, String> {
    let id = value
        .as_str()
        .or_else(|| value.get("blob").and_then(Value::as_str))
        .ok_or("invalid blob id")?;
    let hex = id.strip_prefix("b3:").ok_or("invalid blob id")?;
    if hex.len() != 64 || !hex.bytes().all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f')) {
        return Err("invalid blob id".into());
    }
    Ok(hex)
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct Reference {
    pub blob: String,
    pub size: u64,
    pub mime: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    pub origin: String,
}

impl Reference {
    pub fn validate(&self) -> Result<(), String> {
        blob_hex(&json!(self.blob)).map_err(|_| "lane: invalid reference blob id")?;
        if self.mime.is_empty() || self.origin.is_empty() {
            return Err("lane: reference carries empty mime or origin".into());
        }
        Ok(())
    }
}

pub fn check_quota(value: &Value, owner: &str, length: u64) -> Result<Option<String>, String> {
    let remaining = |row: &Value| -> Result<u64, String> {
        let field = |name| {
            row.get(name)
                .and_then(Value::as_u64)
                .ok_or_else(|| format!("lane_unavailable: invalid blob.quota {name}"))
        };
        Ok(field("limit")?
            .saturating_sub(field("used")?)
            .saturating_sub(field("reserved")?))
    };
    let room = remaining(&value["owners"][owner])?.min(remaining(&value["total"])?);
    Ok(
        (length > room)
            .then(|| format!("quota: {length} B exceeds remaining {room} B for {owner}")),
    )
}

fn encoded_name(name: &str) -> Option<String> {
    let mut encoded = String::new();
    for byte in name.bytes() {
        if (0x21..=0x7e).contains(&byte) && byte != b'%' {
            encoded.push(char::from(byte));
        } else {
            encoded.push_str(&format!("%{byte:02X}"));
        }
        if encoded.len() > 128 {
            return None;
        }
    }
    Some(encoded)
}

pub struct Lane {
    http: Client,
    io_timeout: Duration,
}

impl Lane {
    pub fn new() -> Result<Self, String> {
        let http = Client::builder()
            .connect_timeout(BUS_TIMEOUT)
            .redirect(reqwest::redirect::Policy::none())
            .no_proxy()
            .no_gzip()
            .no_brotli()
            .no_deflate()
            .no_zstd()
            .http1_only()
            .pool_max_idle_per_host(0)
            .build()
            .map_err(|e| format!("lane: HTTP client: {e}"))?;
        Ok(Self {
            http,
            io_timeout: IO_TIMEOUT,
        })
    }

    pub async fn reference(
        &self,
        discovery: &impl Discovery,
        owner: &str,
        bytes: Vec<u8>,
        mime: &str,
        name: Option<&str>,
    ) -> Result<Reference, String> {
        // Advisory failure is fail-open; authoritative HTTP admission is not.
        match discovery
            .quota(owner)
            .await
            .and_then(|v| check_quota(&v, owner, bytes.len() as u64))
        {
            Ok(Some(reason)) => return Err(reason),
            Ok(None) => (),
            Err(reason) => tracing::warn!(%reason, "skipping advisory blob quota preflight"),
        }
        let bind = discovery.bind().await?;
        self.post(&bind, owner, bytes, mime, name).await
    }

    pub async fn post(
        &self,
        bind: &str,
        owner: &str,
        bytes: Vec<u8>,
        mime: &str,
        name: Option<&str>,
    ) -> Result<Reference, String> {
        let bind = checked_bind(bind)?;
        let size = bytes.len();
        let hash = blake3::hash(&bytes).to_hex().to_string();
        let bytes = axum::body::Bytes::from(bytes);
        let (progress, receiver) = watch::channel(Instant::now());
        let body = async_stream::stream! {
            for offset in (0..bytes.len()).step_by(64 * 1024) {
                progress.send_replace(Instant::now());
                yield Ok::<_, std::io::Error>(bytes.slice(offset..(offset + 64 * 1024).min(bytes.len())));
            }
            progress.send_replace(Instant::now());
        };
        let mut request = self
            .http
            .post(format!("http://{bind}/blob"))
            .header("Content-Length", size)
            .header("X-Cosmix-Owner", owner)
            .header("X-Cosmix-Mime", mime)
            .body(reqwest::Body::wrap_stream(body));
        if let Some(name) = name.and_then(encoded_name) {
            request = request.header("X-Cosmix-Name", name);
        }
        // read_timeout starts before request upload in reqwest. A progress
        // watchdog instead bounds body backpressure and the wait for headers,
        // resetting as bounded chunks are consumed. There is no total deadline.
        let response = upload_response(request.send(), receiver, self.io_timeout).await?;
        let response = self.status(response, 201, true).await?;
        let bytes = self
            .read(
                response,
                REFERENCE_LIMIT,
                "lane: reference exceeds size limit",
            )
            .await?;
        let reference: Reference =
            serde_json::from_slice(&bytes).map_err(|e| format!("lane: invalid reference: {e}"))?;
        reference.validate()?;
        if reference.size != size as u64 || reference.blob != format!("b3:{hash}") {
            return Err("verify_failed: reference hash or size differs from uploaded bytes".into());
        }
        Ok(reference)
    }

    pub async fn fetch(
        &self,
        discovery: &impl Discovery,
        value: &Value,
        cap: usize,
    ) -> Result<Vec<u8>, String> {
        blob_hex(value)?; // refuse before any discovery or HTTP
        self.get(&discovery.bind().await?, value, cap).await
    }

    pub async fn get(&self, bind: &str, value: &Value, cap: usize) -> Result<Vec<u8>, String> {
        let hex = blob_hex(value)?;
        let bind = checked_bind(bind)?;
        let response = timeout(
            self.io_timeout,
            self.http.get(format!("http://{bind}/blob/{hex}")).send(),
        )
        .await
        .map_err(|_| "lane: response headers timed out")?
        .map_err(|e| format!("lane: {e}"))?;
        let response = self.status(response, 200, false).await?;
        if response.headers().contains_key("content-encoding")
            || response.headers().contains_key("transfer-encoding")
        {
            return Err("lane: encoded blob response is not supported".into());
        }
        let length = response
            .headers()
            .get("content-length")
            .and_then(|v| v.to_str().ok())
            .and_then(|v| v.parse::<u64>().ok())
            .ok_or("lane: missing or invalid Content-Length")?;
        if length > cap as u64 {
            return Err("too_large: message exceeds max_message_size".into());
        }
        let mut response = response;
        let mut bytes = Vec::new();
        let mut hash = blake3::Hasher::new();
        while let Some(chunk) = self.chunk(&mut response).await? {
            if chunk.len() > cap.saturating_sub(bytes.len()) {
                return Err("too_large: message exceeds max_message_size".into());
            }
            hash.update(&chunk);
            bytes.extend_from_slice(&chunk);
        }
        if bytes.len() as u64 != length
            || hash.finalize().to_hex().as_str() != hex
            || value
                .get("size")
                .is_some_and(|s| s.as_u64() != Some(length))
        {
            return Err("verify_failed: blob hash or size mismatch".into());
        }
        Ok(bytes)
    }

    async fn chunk(&self, response: &mut Response) -> Result<Option<axum::body::Bytes>, String> {
        timeout(self.io_timeout, response.chunk())
            .await
            .map_err(|_| "lane: read timed out")?
            .map_err(|e| format!("lane: read body: {e}"))
    }

    async fn read(
        &self,
        mut response: Response,
        cap: usize,
        too_large: &str,
    ) -> Result<Vec<u8>, String> {
        let mut out = Vec::new();
        while let Some(chunk) = self.chunk(&mut response).await? {
            if chunk.len() > cap.saturating_sub(out.len()) {
                return Err(too_large.into());
            }
            out.extend_from_slice(&chunk);
        }
        Ok(out)
    }

    async fn status(
        &self,
        mut response: Response,
        expected: u16,
        upload: bool,
    ) -> Result<Response, String> {
        let status = response.status().as_u16();
        if status == expected {
            return Ok(response);
        }
        if status == 404 && !upload {
            return Err("not_present: blob is not on this node — blob.fetch it first".into());
        }
        let mut body = Vec::new();
        while body.len() < 512 {
            match self.chunk(&mut response).await {
                Ok(Some(chunk)) => {
                    body.extend_from_slice(&chunk[..chunk.len().min(512 - body.len())])
                }
                _ => break,
            }
        }
        let token = if status == 413 { "quota" } else { "lane" };
        Err(format!(
            "{token}: HTTP {status}: {}",
            String::from_utf8_lossy(&body)
        ))
    }
}

async fn upload_response(
    request: impl Future<Output = Result<Response, reqwest::Error>>,
    mut progress: watch::Receiver<Instant>,
    idle: Duration,
) -> Result<Response, String> {
    tokio::pin!(request);
    let mut deadline = Instant::now() + idle;
    let mut open = true;
    loop {
        tokio::select! {
            result = &mut request => return result.map_err(|e|
                format!("lane: upload failed (refused? check blobd quota): {e}")),
            changed = progress.changed(), if open => {
                open = changed.is_ok();
                if open { deadline = *progress.borrow_and_update() + idle; }
            }
            _ = sleep_until(deadline) => return Err("lane: upload I/O timed out".into()),
        }
    }
}

/// Session-owned task set: no detached transfers, no unbounded waiting queue.
/// Drop (parent cancellation) aborts all members; reconnect drains explicitly.
#[derive(Default)]
pub struct Transfers {
    tasks: tokio::task::JoinSet<()>,
}

impl Transfers {
    pub fn spawn(
        &mut self,
        work: impl Future<Output = ()> + Send + 'static,
    ) -> Result<(), &'static str> {
        while let Some(result) = self.tasks.try_join_next() {
            if let Err(e) = result {
                tracing::warn!(error = %e, "blob transfer worker failed");
            }
        }
        if self.tasks.len() >= 8 {
            return Err("busy: maild blob transfer pool is full (8)");
        }
        self.tasks.spawn(work);
        Ok(())
    }

    pub async fn shutdown(&mut self) {
        self.tasks.shutdown().await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{
        io::{BufRead, BufReader, Read, Write},
        net::TcpListener,
        sync::{
            Arc,
            atomic::{AtomicUsize, Ordering},
        },
    };

    fn serve(
        status: &str,
        body: &[u8],
        extra: &str,
        length: usize,
    ) -> (String, std::thread::JoinHandle<(String, Vec<u8>)>) {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let bind = listener.local_addr().unwrap().to_string();
        let (status, body, extra) = (status.to_owned(), body.to_vec(), extra.to_owned());
        let worker = std::thread::spawn(move || {
            let (socket, _) = listener.accept().unwrap();
            socket
                .set_read_timeout(Some(Duration::from_secs(5)))
                .unwrap();
            socket
                .set_write_timeout(Some(Duration::from_secs(5)))
                .unwrap();
            let mut reader = BufReader::new(socket);
            let mut head = String::new();
            loop {
                let mut line = String::new();
                assert_ne!(reader.read_line(&mut line).unwrap(), 0);
                head.push_str(&line);
                if line == "\r\n" {
                    break;
                }
            }
            let size = head
                .lines()
                .filter_map(|l| l.split_once(':'))
                .find(|(k, _)| k.eq_ignore_ascii_case("content-length"))
                .map(|(_, v)| v.trim().parse::<usize>().unwrap())
                .unwrap_or(0);
            let mut incoming = vec![0; size];
            reader.read_exact(&mut incoming).unwrap();
            let _ = write!(
                reader.get_mut(),
                "HTTP/1.1 {status}\r\nContent-Length: {length}\r\nConnection: close\r\n{extra}\r\n"
            );
            let _ = reader.get_mut().write_all(&body);
            (head, incoming)
        });
        (bind, worker)
    }

    fn reference(bytes: &[u8]) -> Value {
        json!({"blob": format!("b3:{}", blake3::hash(bytes).to_hex()), "size": bytes.len(),
            "mime": "application/octet-stream", "name": "first-writer", "origin": "alpha"})
    }

    struct Fake {
        bind: String,
        quota: Result<Value, String>,
        discoveries: AtomicUsize,
    }
    impl Discovery for Fake {
        async fn bind(&self) -> Result<String, String> {
            self.discoveries.fetch_add(1, Ordering::SeqCst);
            Ok(self.bind.clone())
        }
        async fn quota(&self, owner: &str) -> Result<Value, String> {
            assert_eq!(owner, "maild:7");
            self.quota.clone()
        }
    }

    #[tokio::test]
    async fn post_hash_checks_and_preserves_canonical_metadata_and_headers() {
        for name in ["café% file.bin".to_owned(), "a".repeat(129)] {
            let reply = reference(b"hello").to_string();
            let (bind, worker) = serve("201 Created", reply.as_bytes(), "", reply.len());
            let result = Lane::new()
                .unwrap()
                .post(
                    &bind,
                    "maild:7",
                    b"hello".to_vec(),
                    "text/plain",
                    Some(&name),
                )
                .await
                .unwrap();
            assert_eq!(result.name.as_deref(), Some("first-writer"));
            assert_eq!(result.origin, "alpha");
            let (head, bytes) = worker.join().unwrap();
            assert_eq!(bytes, b"hello");
            let head = head.to_ascii_lowercase();
            assert!(head.contains("x-cosmix-owner: maild:7"));
            assert!(head.contains("x-cosmix-mime: text/plain"));
            assert!(!head.contains("transfer-encoding"));
            if name.len() > 128 {
                assert!(!head.contains("x-cosmix-name"));
            } else {
                assert!(head.contains("x-cosmix-name: caf%c3%a9%25%20file.bin"));
            }
        }
    }

    #[tokio::test]
    async fn advisory_quota_failure_proceeds_but_known_exhaustion_refuses_before_discovery() {
        for quota in [Err("offline".into()), Ok(json!({}))] {
            let reply = reference(b"x").to_string();
            let (bind, worker) = serve("201 Created", reply.as_bytes(), "", reply.len());
            let fake = Fake {
                bind,
                quota,
                discoveries: AtomicUsize::new(0),
            };
            Lane::new()
                .unwrap()
                .reference(&fake, "maild:7", b"x".to_vec(), "text/plain", None)
                .await
                .unwrap();
            worker.join().unwrap();
        }
        let fake = Fake {
            bind: "127.0.0.1:1".into(),
            discoveries: AtomicUsize::new(0),
            quota: Ok(
                json!({"owners": {"maild:7": {"limit": 10, "used": 5, "reserved": 5}},
                "total": {"limit": 100, "used": 0, "reserved": 0}}),
            ),
        };
        assert!(
            Lane::new()
                .unwrap()
                .reference(&fake, "maild:7", vec![1], "text/plain", None)
                .await
                .unwrap_err()
                .starts_with("quota:")
        );
        assert_eq!(fake.discoveries.load(Ordering::SeqCst), 0);
    }

    #[tokio::test]
    async fn get_checks_hash_size_framing_caps_status_and_does_not_follow_redirects() {
        let lane = Lane::new().unwrap();
        let value = reference(b"right");
        let (bind, worker) = serve("200 OK", b"right", "", 5);
        assert_eq!(lane.get(&bind, &value, 5).await.unwrap(), b"right");
        worker.join().unwrap();
        for (status, bytes, extra, length, cap, token) in [
            ("200 OK", &b"wrong"[..], "", 5, 5, "verify_failed:"),
            ("200 OK", &b"rig"[..], "", 5, 5, "lane:"),
            ("200 OK", &b"right"[..], "", 5, 4, "too_large:"),
            (
                "200 OK",
                &b"right"[..],
                "Content-Encoding: gzip\r\n",
                5,
                5,
                "lane:",
            ),
            ("404 Not Found", &b"{}"[..], "", 2, 5, "not_present:"),
            ("413 Payload Too Large", &b"{}"[..], "", 2, 5, "quota:"),
            (
                "302 Found",
                &b""[..],
                "Location: http://127.0.0.1:1/\r\n",
                0,
                5,
                "lane: HTTP 302",
            ),
        ] {
            let (bind, worker) = serve(status, bytes, extra, length);
            let error = lane.get(&bind, &value, cap).await.unwrap_err();
            assert!(error.starts_with(token), "{error}");
            worker.join().unwrap();
        }
    }

    #[tokio::test]
    async fn invalid_input_and_reference_responses_fail_closed() {
        let lane = Lane::new().unwrap();
        let fake = Fake {
            bind: "".into(),
            quota: Err("unused".into()),
            discoveries: AtomicUsize::new(0),
        };
        for value in [
            json!("é".repeat(32)),
            json!("b3:ABC"),
            json!({}),
            Value::Null,
        ] {
            assert_eq!(
                lane.fetch(&fake, &value, 5).await.unwrap_err(),
                "invalid blob id"
            );
        }
        assert_eq!(fake.discoveries.load(Ordering::SeqCst), 0);
        for reply in [
            json!({}),
            json!({"blob": "b3:bad", "size": 1, "origin": "alpha", "mime": "x"}),
            reference(b"other"),
        ] {
            let reply = reply.to_string();
            let (bind, worker) = serve("201 Created", reply.as_bytes(), "", reply.len());
            assert!(
                lane.post(&bind, "maild:7", vec![1], "text/plain", None)
                    .await
                    .is_err()
            );
            worker.join().unwrap();
        }
        assert!(
            lane.get("", &reference(b"x"), 5)
                .await
                .unwrap_err()
                .starts_with("lane_unavailable:")
        );
        assert!(checked_bind("http://127.0.0.1:1/path").is_err());
        assert!(checked_bind("[::1]:8000").is_ok());
    }

    #[tokio::test]
    async fn response_idle_timeout_and_progress_reset_have_no_total_deadline() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let bind = listener.local_addr().unwrap().to_string();
        let server = tokio::spawn(async move {
            let (socket, _) = listener.accept().await.unwrap();
            tokio::time::sleep(Duration::from_millis(200)).await;
            drop(socket);
        });
        let mut lane = Lane::new().unwrap();
        lane.io_timeout = Duration::from_millis(20);
        assert!(
            lane.get(&bind, &reference(b"x"), 5)
                .await
                .unwrap_err()
                .contains("timed out")
        );
        server.abort();
        let _ = server.await;
        let (bind, server) = serve("200 OK", b"x", "", 1);
        let (tx, rx) = watch::channel(Instant::now());
        let pulse = tokio::spawn(async move {
            for _ in 0..10 {
                tokio::time::sleep(Duration::from_millis(10)).await;
                tx.send_replace(Instant::now());
            }
        });
        let request = async {
            tokio::time::sleep(Duration::from_millis(80)).await;
            lane.http.get(format!("http://{bind}/blob")).send().await
        };
        upload_response(request, rx, Duration::from_millis(40))
            .await
            .unwrap();
        pulse.await.unwrap();
        server.join().unwrap();
    }

    #[tokio::test]
    async fn eight_slots_refuse_immediately_and_cancel_on_session_end_or_drop() {
        struct Guard(Arc<AtomicUsize>);
        impl Drop for Guard {
            fn drop(&mut self) {
                self.0.fetch_add(1, Ordering::SeqCst);
            }
        }
        for explicit in [true, false] {
            let dropped = Arc::new(AtomicUsize::new(0));
            let started = Arc::new(tokio::sync::Semaphore::new(0));
            let mut transfers = Transfers::default();
            for _ in 0..8 {
                let guard = Guard(dropped.clone());
                let started = started.clone();
                transfers
                    .spawn(async move {
                        let _guard = guard;
                        started.add_permits(1);
                        std::future::pending::<()>().await;
                    })
                    .unwrap();
            }
            started.acquire_many(8).await.unwrap().forget();
            assert_eq!(
                transfers.spawn(async {}),
                Err("busy: maild blob transfer pool is full (8)")
            );
            if explicit {
                transfers.shutdown().await;
            }
            drop(transfers);
            for _ in 0..10 {
                tokio::task::yield_now().await;
            }
            assert_eq!(dropped.load(Ordering::SeqCst), 8);
        }
    }
}
