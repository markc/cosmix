//! Local blobd adapter. No reference origin is ever used as a routing target.
//! Foundations only until the P5 cluster checkpoint: consumers follow in slices 4–6.
#![allow(dead_code)]

use std::{future::Future, sync::Arc, time::Duration};

use axum::{
    body::{Body, Bytes},
    http::{HeaderMap, HeaderValue, Method, StatusCode, header},
};
use cosmix_bus::PortReply;
use cosmix_client::NodedClient;
use reqwest::{Client, Response};
use serde_json::{Value, json};
use tokio::{
    sync::{OwnedSemaphorePermit, Semaphore, watch},
    time::{Instant, sleep_until, timeout},
};

use crate::{
    blob_reference::{Reference, blob_hex},
    bus::subscribe_granter::SharedBrokerHandle,
};

const BUS_TIMEOUT: Duration = Duration::from_secs(10);
const IO_TIMEOUT: Duration = Duration::from_secs(30);
const REFERENCE_LIMIT: usize = 64 * 1024;
const TRANSFERS: usize = 8;

/// Load once per operation, never retain a client across broker reconnects.
pub fn local(handle: &SharedBrokerHandle) -> Result<Arc<NodedClient>, String> {
    handle
        .load_full()
        .ok_or_else(|| "lane_unavailable: broker is disconnected".into())
}

pub trait Discovery: Send + Sync {
    fn bind(&self) -> impl Future<Output = Result<String, String>> + Send;
    fn stat(&self, blob: &str) -> impl Future<Output = Result<Value, String>> + Send;
    fn quota(&self, owner: &str) -> impl Future<Output = Result<Value, String>> + Send;
    fn pin(&self, blob: &str, owner: &str) -> impl Future<Output = Result<(), String>> + Send;
}

impl Discovery for NodedClient {
    async fn bind(&self) -> Result<String, String> {
        let value = call(self, "blob.props.get", json!({"path": "lane"})).await?;
        let bind = value["bind"]
            .as_str()
            .ok_or("lane_unavailable: no lane bind")?;
        checked_bind(bind).map(str::to_owned)
    }
    async fn stat(&self, blob: &str) -> Result<Value, String> {
        call(self, "blob.stat", json!({"blob": blob})).await
    }
    async fn quota(&self, owner: &str) -> Result<Value, String> {
        call(self, "blob.quota", json!({"owner": owner})).await
    }
    async fn pin(&self, blob: &str, owner: &str) -> Result<(), String> {
        call(self, "blob.pin", json!({"blob": blob, "owner": owner}))
            .await
            .map(|_| ())
    }
}

async fn call(client: &NodedClient, verb: &str, args: Value) -> Result<Value, String> {
    match timeout(BUS_TIMEOUT, client.call_typed("blobd", verb, args))
        .await
        .map_err(|_| format!("lane_unavailable: {verb} timed out"))?
        .map_err(|e| format!("lane_unavailable: {verb}: {e}"))?
    {
        PortReply::Ok { value, .. } => Ok(value),
        PortReply::AppError { message, .. } => Err(app_error(&message)),
    }
}

fn app_error(message: &str) -> String {
    let value: Value = serde_json::from_str(message).unwrap_or(Value::Null);
    let reason = value["error"].as_str().unwrap_or(message);
    if [
        "quota:",
        "not_present:",
        "verify_failed:",
        "lane:",
        "lane_unavailable:",
    ]
    .iter()
    .any(|p| reason.starts_with(p))
    {
        reason.to_owned()
    } else {
        format!("lane: {reason}")
    }
}

fn checked_bind(bind: &str) -> Result<&str, String> {
    bind.parse::<std::net::SocketAddr>()
        .map_err(|_| "lane_unavailable: invalid lane bind")?;
    Ok(bind)
}

fn valid_owner(owner: &str) -> Result<(), String> {
    if owner.is_empty() || owner.len() > 128 || !owner.bytes().all(|b| (0x21..=0x7e).contains(&b)) {
        return Err("invalid_arguments: invalid blob owner".into());
    }
    Ok(())
}

pub struct Lane {
    http: Client,
    permits: Arc<Semaphore>,
    io_timeout: Duration,
}

/// Only validated, allowlisted lane headers. Public security and attachment
/// headers belong to the share handler; no upstream cache/cookie/location leaks.
pub struct Download {
    pub status: StatusCode,
    pub headers: HeaderMap,
    pub body: Body,
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
            permits: Arc::new(Semaphore::new(TRANSFERS)),
            io_timeout: IO_TIMEOUT,
        })
    }

    fn admit(&self) -> Result<OwnedSemaphorePermit, String> {
        self.permits
            .clone()
            .try_acquire_owned()
            .map_err(|_| "lane: transfer pool full (8)".into())
    }

    /// Verify locality/size, then pin before a share token can be published.
    /// No cross-node fetch, no unpin, and no bytes in Bus frames.
    pub async fn pin_reference(
        &self,
        discovery: &impl Discovery,
        reference: &Reference,
        owner: &str,
    ) -> Result<(), String> {
        reference.validate()?;
        valid_owner(owner)?;
        let _permit = self.admit()?;
        let stat = discovery.stat(&reference.blob).await?;
        if stat["present"] != true {
            return Err("not_present: blob is not on this node".into());
        }
        if stat["size"].as_u64() != Some(reference.size) {
            return Err("verify_failed: stat size differs from reference".into());
        }
        discovery.pin(&reference.blob, owner).await
    }

    pub async fn download(
        &self,
        discovery: &impl Discovery,
        reference: &Reference,
        method: Method,
        range: Option<HeaderValue>,
    ) -> Result<Download, String> {
        reference.validate()?;
        if method != Method::GET && method != Method::HEAD {
            return Err("invalid_arguments: lane download requires GET or HEAD".into());
        }
        if range.as_ref().is_some_and(|r| r.as_bytes().len() > 1024) {
            return Err("invalid_arguments: Range too long".into());
        }
        let permit = self.admit()?;
        let bind = discovery.bind().await?;
        let url = format!(
            "http://{}/blob/{}",
            checked_bind(&bind)?,
            blob_hex(&reference.blob)?
        );
        let expected = expected_range(range.as_ref().and_then(|v| v.to_str().ok()), reference.size);
        let mut request = self.http.request(method.clone(), url);
        if let Some(range) = range {
            request = request.header(header::RANGE, range);
        }
        let response = timeout(self.io_timeout, request.send())
            .await
            .map_err(|_| "lane: response headers timed out")?
            .map_err(|e| format!("lane: request failed: {e}"))?;
        if ![200, 206, 416].contains(&response.status().as_u16()) {
            return Err(self.status_error(response, false).await);
        }
        let (headers, length) =
            validate_headers(response.status(), response.headers(), expected, reference)?;
        let status = response.status();
        let body = if method == Method::HEAD || status == StatusCode::RANGE_NOT_SATISFIABLE {
            // 416's upstream JSON diagnostics never reach the public client.
            drop(permit);
            Body::empty()
        } else {
            let state = StreamState {
                response,
                remaining: length,
                idle: self.io_timeout,
                _permit: permit,
            };
            Body::from_stream(futures_util::stream::try_unfold(
                state,
                |mut state| async move {
                    match timeout(state.idle, state.response.chunk())
                        .await
                        .map_err(|_| stream_error("lane: body idle timeout"))?
                        .map_err(|_| stream_error("verify_failed: incomplete lane body"))?
                    {
                        Some(chunk) => {
                            if chunk.len() as u64 > state.remaining {
                                return Err(stream_error(
                                    "verify_failed: body exceeds declared length",
                                ));
                            }
                            state.remaining -= chunk.len() as u64;
                            Ok(Some((chunk, state)))
                        }
                        None if state.remaining == 0 => Ok(None),
                        None => Err(stream_error(
                            "verify_failed: body shorter than declared length",
                        )),
                    }
                },
            ))
        };
        Ok(Download {
            status,
            headers,
            body,
        })
    }

    /// Media callers admit bounded work before allocating/reading bytes. This
    /// adapter bounds actual lane operations too. A retry hashes the served file.
    pub async fn reference(
        &self,
        discovery: &impl Discovery,
        owner: &str,
        bytes: Vec<u8>,
        mime: &str,
        name: Option<&str>,
    ) -> Result<Reference, String> {
        valid_owner(owner)?;
        let _permit = self.admit()?;
        let blob = format!("b3:{}", blake3::hash(&bytes).to_hex());
        let candidate = Reference {
            blob: blob.clone(),
            size: bytes.len() as u64,
            mime: mime.into(),
            name: name.map(str::to_owned),
            origin: "local".into(),
        };
        candidate.validate()?;
        match discovery.stat(&blob).await {
            Ok(stat) if stat["present"] == true => {
                let reference = stat_reference(&blob, &stat)?;
                if reference.size != bytes.len() as u64 {
                    return Err("verify_failed: stat size differs from media bytes".into());
                }
                if !stat["pins"]
                    .as_array()
                    .is_some_and(|pins| pins.iter().any(|p| p.as_str() == Some(owner)))
                {
                    discovery.pin(&blob, owner).await?;
                }
                return Ok(reference);
            }
            Ok(_) => (),
            Err(reason) => tracing::warn!(%reason, "blob stat unavailable; trying lane admission"),
        }
        match discovery
            .quota(owner)
            .await
            .and_then(|v| check_quota(&v, owner, bytes.len() as u64))
        {
            Ok(()) => (),
            Err(reason) if reason.starts_with("quota:") => return Err(reason),
            Err(reason) => tracing::warn!(%reason, "skipping advisory blob quota preflight"),
        }
        let bind = discovery.bind().await?;
        self.put(&bind, owner, bytes, mime, name).await
    }

    async fn put(
        &self,
        bind: &str,
        owner: &str,
        bytes: Vec<u8>,
        mime: &str,
        name: Option<&str>,
    ) -> Result<Reference, String> {
        let size = bytes.len();
        let hash = blake3::hash(&bytes).to_hex().to_string();
        let bytes = Bytes::from(bytes);
        let (progress, receiver) = watch::channel(Instant::now());
        let body = futures_util::stream::unfold(
            (bytes, 0, progress),
            |(bytes, offset, progress)| async move {
                progress.send_replace(Instant::now());
                if offset == bytes.len() {
                    return None;
                }
                let end = (offset + 64 * 1024).min(bytes.len());
                let chunk = bytes.slice(offset..end);
                Some((Ok::<_, std::io::Error>(chunk), (bytes, end, progress)))
            },
        );
        let mut request = self
            .http
            .put(format!("http://{}/blob/{hash}", checked_bind(bind)?))
            .header(header::CONTENT_LENGTH, size)
            .header("X-Cosmix-Owner", owner)
            .header("X-Cosmix-Mime", mime)
            .body(reqwest::Body::wrap_stream(body));
        if let Some(name) = name.and_then(encoded_name) {
            request = request.header("X-Cosmix-Name", name);
        }
        let response = upload_response(request.send(), receiver, self.io_timeout).await?;
        if ![200, 201].contains(&response.status().as_u16()) {
            return Err(self.status_error(response, true).await);
        }
        if response.headers().contains_key(header::CONTENT_ENCODING) {
            return Err("lane: encoded reference is not supported".into());
        }
        let bytes = self.read_small(response, REFERENCE_LIMIT).await?;
        let reference: Reference =
            serde_json::from_slice(&bytes).map_err(|_| "lane: invalid reference JSON")?;
        reference
            .validate()
            .map_err(|_| "lane: invalid reference metadata")?;
        if reference.size != size as u64 || reference.blob != format!("b3:{hash}") {
            return Err("verify_failed: reference hash or size differs from media bytes".into());
        }
        Ok(reference)
    }

    async fn read_small(&self, mut response: Response, cap: usize) -> Result<Vec<u8>, String> {
        // Metadata/errors have a total deadline as well as a hard byte cap.
        timeout(self.io_timeout, async move {
            let mut out = Vec::new();
            while let Some(chunk) = response
                .chunk()
                .await
                .map_err(|e| format!("lane: response body: {e}"))?
            {
                if chunk.len() > cap.saturating_sub(out.len()) {
                    return Err("lane: response exceeds size limit".into());
                }
                out.extend_from_slice(&chunk);
            }
            Ok(out)
        })
        .await
        .map_err(|_| "lane: metadata read timed out".to_string())?
    }

    async fn status_error(&self, response: Response, upload: bool) -> String {
        let status = response.status().as_u16();
        if status == 404 && !upload {
            return "not_present: blob is not on this node; fetch it first".into();
        }
        let token = match status {
            413 => "quota",
            422 => "verify_failed",
            _ => "lane",
        };
        let body = self.read_small(response, 512).await.unwrap_or_default();
        format!("{token}: HTTP {status}: {}", String::from_utf8_lossy(&body))
    }
}

struct StreamState {
    response: Response,
    remaining: u64,
    idle: Duration,
    _permit: OwnedSemaphorePermit,
}

fn stream_error(reason: &str) -> std::io::Error {
    std::io::Error::other(reason)
}

fn stat_reference(blob: &str, stat: &Value) -> Result<Reference, String> {
    let reference = Reference {
        blob: blob.into(),
        size: stat["size"].as_u64().ok_or("lane: invalid stat size")?,
        mime: stat["mime"]
            .as_str()
            .ok_or("lane: invalid stat mime")?
            .into(),
        // blob.stat currently has no name field; do not invent first-writer metadata.
        name: None,
        origin: stat["origin"]
            .as_str()
            .ok_or("lane: invalid stat origin")?
            .into(),
    };
    reference
        .validate()
        .map_err(|_| "lane: invalid stat reference")?;
    Ok(reference)
}

fn check_quota(value: &Value, owner: &str, length: u64) -> Result<(), String> {
    let remaining = |row: &Value| -> Result<u64, String> {
        let field = |key| {
            row.get(key)
                .and_then(Value::as_u64)
                .ok_or_else(|| format!("lane_unavailable: invalid quota {key}"))
        };
        Ok(field("limit")?
            .saturating_sub(field("used")?)
            .saturating_sub(field("reserved")?))
    };
    let room = remaining(&value["owners"][owner])?.min(remaining(&value["total"])?);
    if length > room {
        Err(format!("quota: {length} B exceeds remaining {room} B"))
    } else {
        Ok(())
    }
}

fn encoded_name(name: &str) -> Option<String> {
    let mut out = String::new();
    for b in name.bytes() {
        if (0x21..=0x7e).contains(&b) && b != b'%' {
            out.push(char::from(b));
        } else {
            out.push_str(&format!("%{b:02X}"));
        }
        if out.len() > 128 {
            return None;
        }
    }
    Some(out)
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
            result = &mut request => return result.map_err(|e| format!("lane: upload failed: {e}")),
            changed = progress.changed(), if open => {
                open = changed.is_ok();
                if open { deadline = *progress.borrow_and_update() + idle; }
            }
            _ = sleep_until(deadline) => return Err("lane: upload I/O timed out".into()),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Range {
    Full,
    Partial(u64, u64),
    Unsatisfiable,
}

/// Match blobd's single-range semantics, including ignoring malformed/multi ranges.
pub(crate) fn expected_range(spec: Option<&str>, size: u64) -> Range {
    let Some(spec) = spec.and_then(|s| s.trim().strip_prefix("bytes=")) else {
        return Range::Full;
    };
    if spec.contains(',') {
        return Range::Full;
    }
    let number = |s: &str| {
        if !s.is_empty() && s.bytes().all(|b| b.is_ascii_digit()) {
            s.parse::<u64>().ok()
        } else {
            None
        }
    };
    if let Some(suffix) = spec.strip_prefix('-') {
        let Some(n) = number(suffix) else {
            return Range::Full;
        };
        if n == 0 || size == 0 {
            return Range::Unsatisfiable;
        }
        let len = n.min(size);
        return Range::Partial(size - len, len);
    }
    let Some((first, last)) = spec.split_once('-') else {
        return Range::Full;
    };
    let Some(start) = number(first) else {
        return Range::Full;
    };
    let end = if last.is_empty() {
        size.saturating_sub(1)
    } else {
        let Some(end) = number(last) else {
            return Range::Full;
        };
        if end < start {
            return Range::Full;
        }
        end.min(size.saturating_sub(1))
    };
    if start >= size {
        Range::Unsatisfiable
    } else {
        Range::Partial(start, end - start + 1)
    }
}

fn one_header<'a>(headers: &'a HeaderMap, key: &str) -> Result<Option<&'a str>, String> {
    let mut values = headers.get_all(key).iter();
    let value = values
        .next()
        .map(|v| v.to_str().map_err(|_| format!("lane: invalid {key}")))
        .transpose()?;
    if values.next().is_some() {
        return Err(format!("lane: duplicate {key}"));
    }
    Ok(value)
}

fn validate_headers(
    status: StatusCode,
    upstream: &HeaderMap,
    expected: Range,
    reference: &Reference,
) -> Result<(HeaderMap, u64), String> {
    if upstream.contains_key(header::CONTENT_ENCODING)
        || upstream.contains_key(header::TRANSFER_ENCODING)
    {
        return Err("lane: encoded/chunked blob response is not supported".into());
    }
    let (code, length, content_range) = match expected {
        Range::Full => (StatusCode::OK, reference.size, None),
        Range::Partial(start, len) => (
            StatusCode::PARTIAL_CONTENT,
            len,
            Some(format!(
                "bytes {start}-{}/{}",
                start + len - 1,
                reference.size
            )),
        ),
        Range::Unsatisfiable => (
            StatusCode::RANGE_NOT_SATISFIABLE,
            0,
            Some(format!("bytes */{}", reference.size)),
        ),
    };
    if status != code || one_header(upstream, "content-range")? != content_range.as_deref() {
        return Err("verify_failed: lane status/range differs from requested extent".into());
    }
    if status != StatusCode::RANGE_NOT_SATISFIABLE {
        let declared = one_header(upstream, "content-length")?.and_then(|v| v.parse::<u64>().ok());
        if declared != Some(length) {
            return Err("verify_failed: lane length differs from reference extent".into());
        }
        if one_header(upstream, "accept-ranges")? != Some("bytes") {
            return Err("lane: missing/invalid Accept-Ranges".into());
        }
    } else if one_header(upstream, "accept-ranges")?.is_some_and(|v| v != "bytes") {
        return Err("lane: invalid Accept-Ranges".into());
    }
    let mut headers = HeaderMap::new();
    headers.insert(
        header::CONTENT_LENGTH,
        HeaderValue::from_str(&length.to_string()).map_err(|_| "lane: invalid length")?,
    );
    headers.insert(
        header::CONTENT_TYPE,
        HeaderValue::from_str(&reference.mime).map_err(|_| "lane: invalid reference mime")?,
    );
    headers.insert(header::ACCEPT_RANGES, HeaderValue::from_static("bytes"));
    if let Some(range) = content_range {
        headers.insert(
            header::CONTENT_RANGE,
            HeaderValue::from_str(&range).map_err(|_| "lane: invalid range")?,
        );
    }
    Ok((headers, length))
}

#[cfg(test)]
mod tests {
    use super::*;
    use http_body_util::BodyExt;
    use std::sync::{
        Mutex,
        atomic::{AtomicUsize, Ordering},
    };
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    struct Fake {
        bind: String,
        stat: Result<Value, String>,
        quota: Result<Value, String>,
        pin_error: Option<String>,
        calls: Mutex<Vec<String>>,
        binds: AtomicUsize,
    }
    impl Fake {
        fn new(bind: &str) -> Self {
            Self {
                bind: bind.into(),
                stat: Ok(json!({"present": false})),
                quota: Err("lane_unavailable: offline".into()),
                pin_error: None,
                calls: Mutex::new(Vec::new()),
                binds: AtomicUsize::new(0),
            }
        }
    }
    impl Discovery for Fake {
        async fn bind(&self) -> Result<String, String> {
            self.binds.fetch_add(1, Ordering::SeqCst);
            self.calls.lock().unwrap().push("bind".into());
            Ok(self.bind.clone())
        }
        async fn stat(&self, blob: &str) -> Result<Value, String> {
            self.calls.lock().unwrap().push(format!("stat:{blob}"));
            self.stat.clone()
        }
        async fn quota(&self, _: &str) -> Result<Value, String> {
            self.calls.lock().unwrap().push("quota".into());
            self.quota.clone()
        }
        async fn pin(&self, blob: &str, owner: &str) -> Result<(), String> {
            self.calls
                .lock()
                .unwrap()
                .push(format!("pin:{blob}:{owner}"));
            self.pin_error.clone().map_or(Ok(()), Err)
        }
    }

    /// One raw HTTP response allows malformed/truncated bodies that axum repairs.
    /// Drop aborts the task even when an assertion fails; no detached test servers.
    struct Server {
        bind: String,
        request: tokio::sync::oneshot::Receiver<String>,
        task: tokio::task::JoinHandle<()>,
    }
    impl Drop for Server {
        fn drop(&mut self) {
            self.task.abort();
        }
    }
    async fn server(response: Vec<u8>, linger: Duration) -> Server {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let bind = listener.local_addr().unwrap().to_string();
        let (sender, request) = tokio::sync::oneshot::channel();
        let task = tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.unwrap();
            let mut request = Vec::new();
            loop {
                let mut byte = [0u8; 1];
                if socket.read_exact(&mut byte).await.is_err() {
                    return;
                }
                request.push(byte[0]);
                if request.ends_with(b"\r\n\r\n") {
                    break;
                }
                assert!(request.len() < 8192);
            }
            let _ = sender.send(String::from_utf8(request).unwrap());
            let _ = socket.write_all(&response).await;
            tokio::time::sleep(linger).await;
        });
        Server {
            bind,
            request,
            task,
        }
    }
    fn wire(code: u16, headers: &str, body: &[u8]) -> Vec<u8> {
        let mut bytes =
            format!("HTTP/1.1 {code} Test\r\nConnection: close\r\n{headers}\r\n").into_bytes();
        bytes.extend_from_slice(body);
        bytes
    }
    fn reference(bytes: &[u8]) -> Reference {
        Reference {
            blob: format!("b3:{}", blake3::hash(bytes).to_hex()),
            size: bytes.len() as u64,
            mime: "application/octet-stream".into(),
            name: None,
            origin: "provenance-only.example.test".into(),
        }
    }
    fn stat(reference: &Reference, pins: &[&str]) -> Value {
        json!({"present": true, "size": reference.size, "mime": reference.mime, "origin": "alpha", "pins": pins})
    }
    fn quota(owner: &str, limit: u64) -> Value {
        let row = json!({"used": 0, "reserved": 0, "limit": limit});
        json!({"owners": {owner: row.clone()}, "total": row})
    }

    #[test]
    fn range_contract_matches_blobd_including_extremes() {
        for (spec, expected) in [
            (None, Range::Full),
            (Some("bytes=1-3"), Range::Partial(1, 3)),
            (Some("bytes=8-99"), Range::Partial(8, 2)),
            (Some("bytes=8-"), Range::Partial(8, 2)),
            (Some("bytes=-3"), Range::Partial(7, 3)),
            (Some("bytes=-99"), Range::Partial(0, 10)),
            (Some("bytes=10-"), Range::Unsatisfiable),
            (Some("bytes=-0"), Range::Unsatisfiable),
            (Some("bytes=3-1"), Range::Full),
            (Some("bytes=0-1,4-5"), Range::Full),
            (Some("bytes=+1-2"), Range::Full),
            (Some("bytes=18446744073709551616-"), Range::Full),
        ] {
            assert_eq!(expected_range(spec, 10), expected, "{spec:?}");
        }
        assert_eq!(expected_range(Some("bytes=0-"), 0), Range::Unsatisfiable);
        assert_eq!(expected_range(Some("bytes=-1"), 0), Range::Unsatisfiable);
        assert_eq!(
            expected_range(Some("bytes=0-18446744073709551615"), u64::MAX),
            Range::Partial(0, u64::MAX)
        );
    }

    #[test]
    fn headers_are_checked_and_allowlisted() {
        let r = reference(b"abc");
        let mut h = HeaderMap::new();
        h.insert("content-length", HeaderValue::from_static("3"));
        h.insert("accept-ranges", HeaderValue::from_static("bytes"));
        h.insert("set-cookie", HeaderValue::from_static("private=oops"));
        h.insert("cache-control", HeaderValue::from_static("immutable"));
        let (safe, len) = validate_headers(StatusCode::OK, &h, Range::Full, &r).unwrap();
        assert_eq!(len, 3);
        assert_eq!(safe.len(), 3);
        assert!(!safe.contains_key("set-cookie"));
        assert!(!safe.contains_key("cache-control"));
        h.insert("content-length", HeaderValue::from_static("4"));
        assert!(
            validate_headers(StatusCode::OK, &h, Range::Full, &r)
                .unwrap_err()
                .starts_with("verify_failed:")
        );
        h.insert("content-length", HeaderValue::from_static("3"));
        h.append("accept-ranges", HeaderValue::from_static("bytes"));
        assert!(validate_headers(StatusCode::OK, &h, Range::Full, &r).is_err());
        h.remove("accept-ranges");
        h.insert("accept-ranges", HeaderValue::from_static("none"));
        assert!(validate_headers(StatusCode::OK, &h, Range::Full, &r).is_err());
        h.insert("accept-ranges", HeaderValue::from_static("bytes"));
        for key in ["content-encoding", "transfer-encoding"] {
            h.insert(key, HeaderValue::from_static("gzip"));
            assert!(validate_headers(StatusCode::OK, &h, Range::Full, &r).is_err());
            h.remove(key);
        }
        h.insert("content-range", HeaderValue::from_static("bytes 0-2/4"));
        assert!(
            validate_headers(StatusCode::PARTIAL_CONTENT, &h, Range::Partial(0, 3), &r).is_err()
        );
    }

    #[tokio::test]
    async fn binary_get_head_range_and_416() {
        let lane = Lane::new().unwrap();
        let r = reference(&[0, 255, 128, 7]);
        for (method, range, code, headers, body, expected) in [
            (
                Method::GET,
                None,
                200,
                "Content-Length: 4\r\nAccept-Ranges: bytes\r\n",
                vec![0, 255, 128, 7],
                vec![0, 255, 128, 7],
            ),
            (
                Method::HEAD,
                None,
                200,
                "Content-Length: 4\r\nAccept-Ranges: bytes\r\n",
                vec![],
                vec![],
            ),
            (
                Method::GET,
                Some("bytes=1-2"),
                206,
                "Content-Length: 2\r\nAccept-Ranges: bytes\r\nContent-Range: bytes 1-2/4\r\n",
                vec![255, 128],
                vec![255, 128],
            ),
            (
                Method::HEAD,
                Some("bytes=-1"),
                206,
                "Content-Length: 1\r\nAccept-Ranges: bytes\r\nContent-Range: bytes 3-3/4\r\n",
                vec![],
                vec![],
            ),
            (
                Method::GET,
                Some("bytes=4-"),
                416,
                "Content-Length: 6\r\nContent-Range: bytes */4\r\n",
                b"secret".to_vec(),
                vec![],
            ),
        ] {
            let mut server = server(wire(code, headers, &body), Duration::ZERO).await;
            let fake = Fake::new(&server.bind);
            let result = lane
                .download(
                    &fake,
                    &r,
                    method.clone(),
                    range.map(HeaderValue::from_static),
                )
                .await
                .unwrap();
            assert_eq!(result.status.as_u16(), code);
            assert_eq!(
                result.body.collect().await.unwrap().to_bytes().as_ref(),
                expected.as_slice()
            );
            let request = (&mut server.request).await.unwrap().to_ascii_lowercase();
            assert!(request.starts_with(&format!(
                "{} /blob/{} ",
                method.as_str().to_ascii_lowercase(),
                blob_hex(&r.blob).unwrap()
            )));
            if let Some(range) = range {
                assert!(request.contains(&format!("range: {range}\r\n")));
            }
            assert_eq!(lane.permits.available_permits(), TRANSFERS);
        }
    }

    #[tokio::test]
    async fn truncated_and_idle_bodies_fail_and_release_admission() {
        for linger in [Duration::ZERO, Duration::from_secs(5)] {
            let server = server(
                wire(200, "Content-Length: 4\r\nAccept-Ranges: bytes\r\n", b"a"),
                linger,
            )
            .await;
            let mut lane = Lane::new().unwrap();
            lane.io_timeout = Duration::from_millis(100);
            let result = lane
                .download(
                    &Fake::new(&server.bind),
                    &reference(b"abcd"),
                    Method::GET,
                    None,
                )
                .await
                .unwrap();
            assert!(
                timeout(Duration::from_secs(2), result.body.collect())
                    .await
                    .unwrap()
                    .is_err()
            );
            assert_eq!(lane.permits.available_permits(), TRANSFERS);
        }
    }

    #[tokio::test]
    async fn eight_streams_bound_admission_and_drop_releases() {
        let lane = Lane::new().unwrap();
        let mut servers = Vec::new();
        let mut downloads = Vec::new();
        for _ in 0..TRANSFERS {
            let server = server(
                wire(200, "Content-Length: 1\r\nAccept-Ranges: bytes\r\n", b"x"),
                Duration::ZERO,
            )
            .await;
            downloads.push(
                lane.download(
                    &Fake::new(&server.bind),
                    &reference(b"x"),
                    Method::GET,
                    None,
                )
                .await
                .unwrap(),
            );
            servers.push(server);
        }
        let fake = Fake::new("bad-bind");
        let err = lane
            .download(&fake, &reference(b"x"), Method::GET, None)
            .await
            .err()
            .unwrap();
        assert!(err.starts_with("lane: transfer pool full"));
        assert_eq!(fake.binds.load(Ordering::SeqCst), 0);
        downloads.pop();
        assert_eq!(lane.permits.available_permits(), 1);
        drop(downloads);
        assert_eq!(lane.permits.available_permits(), TRANSFERS);
    }

    #[tokio::test]
    async fn invalid_input_missing_local_broker_and_redirects_fail_closed() {
        assert!(
            local(&crate::bus::subscribe_granter::new_broker_handle())
                .err()
                .unwrap()
                .starts_with("lane_unavailable:")
        );
        let lane = Lane::new().unwrap();
        let fake = Fake::new("http://remote.example.test/blob");
        let mut r = reference(b"x");
        r.blob = format!("b3:{}", "é".repeat(32));
        assert!(lane.download(&fake, &r, Method::GET, None).await.is_err());
        assert_eq!(fake.binds.load(Ordering::SeqCst), 0);
        assert!(
            lane.download(&fake, &reference(b"x"), Method::GET, None)
                .await
                .err()
                .unwrap()
                .starts_with("lane_unavailable:")
        );
        let target = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let headers = format!(
            "Content-Length: 0\r\nLocation: http://{}/leak\r\n",
            target.local_addr().unwrap()
        );
        let server = server(wire(302, &headers, b""), Duration::ZERO).await;
        assert!(
            lane.download(
                &Fake::new(&server.bind),
                &reference(b"x"),
                Method::GET,
                None
            )
            .await
            .err()
            .unwrap()
            .starts_with("lane:")
        );
        assert!(
            timeout(Duration::from_millis(50), target.accept())
                .await
                .is_err()
        );
    }

    #[tokio::test]
    async fn stat_first_recovery_pins_without_upload_and_checks_size() {
        let lane = Lane::new().unwrap();
        let r = reference(b"abc");
        let mut fake = Fake::new("must-not-connect");
        fake.stat = Ok(stat(&r, &["owner"]));
        let found = lane
            .reference(
                &fake,
                "owner",
                b"abc".to_vec(),
                "image/png",
                Some("new-name"),
            )
            .await
            .unwrap();
        assert_eq!(found.mime, r.mime);
        assert_eq!(found.name, None);
        assert_eq!(fake.calls.lock().unwrap().len(), 1);
        fake.stat = Ok(stat(&r, &[]));
        lane.reference(&fake, "owner", b"abc".to_vec(), "image/png", None)
            .await
            .unwrap();
        assert!(
            fake.calls
                .lock()
                .unwrap()
                .last()
                .unwrap()
                .starts_with("pin:")
        );
        assert_eq!(fake.binds.load(Ordering::SeqCst), 0);
        fake.pin_error = Some("quota: owner full".into());
        assert!(
            lane.pin_reference(&fake, &r, "owner")
                .await
                .unwrap_err()
                .starts_with("quota:")
        );
        fake.stat = Ok(json!({"present": true, "size": 4, "mime": "image/png", "origin": "alpha"}));
        assert!(
            lane.reference(&fake, "owner", b"abc".to_vec(), "image/png", None)
                .await
                .unwrap_err()
                .starts_with("verify_failed:")
        );
        fake.stat = Ok(json!({"present": false}));
        assert!(
            lane.pin_reference(&fake, &r, "owner")
                .await
                .unwrap_err()
                .starts_with("not_present:")
        );
    }

    #[tokio::test]
    async fn quota_refusal_precedes_http_and_put_accepts_early_present_response() {
        let lane = Lane::new().unwrap();
        let mut fake = Fake::new("must-not-connect");
        fake.quota = Ok(quota("owner", 2));
        assert!(
            lane.reference(&fake, "owner", b"abc".to_vec(), "image/png", None)
                .await
                .unwrap_err()
                .starts_with("quota:")
        );
        assert_eq!(fake.binds.load(Ordering::SeqCst), 0);
        for code in [200, 201] {
            let r = reference(b"abc");
            let body = serde_json::to_vec(&r).unwrap();
            let mut server = server(
                wire(code, &format!("Content-Length: {}\r\n", body.len()), &body),
                Duration::from_millis(20),
            )
            .await;
            let fake = Fake::new(&server.bind); // quota advisory offline: PUT is authoritative
            assert_eq!(
                lane.reference(&fake, "owner", b"abc".to_vec(), "image/png", Some("a b%é"))
                    .await
                    .unwrap(),
                r
            );
            let request = (&mut server.request).await.unwrap().to_ascii_lowercase();
            assert!(request.starts_with(&format!("put /blob/{} ", blob_hex(&r.blob).unwrap())));
            assert!(request.contains("x-cosmix-owner: owner\r\n"));
            assert!(request.contains("x-cosmix-name: a%20b%25%c3%a9\r\n"));
            assert!(fake.calls.lock().unwrap()[0].starts_with("stat:b3:"));
        }
    }

    #[tokio::test]
    async fn put_rejects_wrong_reference_and_preserves_lane_tokens() {
        let lane = Lane::new().unwrap();
        let wrong = serde_json::to_vec(&reference(b"xyz")).unwrap();
        let bad_server = server(
            wire(201, &format!("Content-Length: {}\r\n", wrong.len()), &wrong),
            Duration::from_millis(20),
        )
        .await;
        assert!(
            lane.reference(
                &Fake::new(&bad_server.bind),
                "owner",
                b"abc".to_vec(),
                "image/png",
                None
            )
            .await
            .unwrap_err()
            .starts_with("verify_failed:")
        );
        for (status, token) in [
            (404, "not_present:"),
            (413, "quota:"),
            (422, "verify_failed:"),
            (500, "lane:"),
        ] {
            let server = server(wire(status, "Content-Length: 0\r\n", b""), Duration::ZERO).await;
            assert!(
                lane.download(
                    &Fake::new(&server.bind),
                    &reference(b"abc"),
                    Method::GET,
                    None
                )
                .await
                .err()
                .unwrap()
                .starts_with(token)
            );
        }
        assert_eq!(app_error(r#"{"error":"quota: full"}"#), "quota: full");
        assert!(app_error("unexpected").starts_with("lane:"));
    }

    #[tokio::test]
    async fn header_and_upload_idle_deadlines_release_permits() {
        let mut lane = Lane::new().unwrap();
        lane.io_timeout = Duration::from_millis(100);
        for upload in [false, true] {
            let server = server(Vec::new(), Duration::from_secs(5)).await;
            let fake = Fake::new(&server.bind);
            let err = timeout(Duration::from_secs(2), async {
                if upload {
                    lane.reference(&fake, "owner", b"abc".to_vec(), "image/png", None)
                        .await
                        .err()
                        .unwrap()
                } else {
                    lane.download(&fake, &reference(b"abc"), Method::GET, None)
                        .await
                        .err()
                        .unwrap()
                }
            })
            .await
            .unwrap();
            assert!(err.starts_with("lane:"), "{err}");
            assert!(err.contains("timed out"), "{err}");
            assert_eq!(lane.permits.available_permits(), TRANSFERS);
        }
    }

    #[tokio::test]
    async fn reference_reply_is_size_bounded_and_pin_has_no_http_dependency() {
        let lane = Lane::new().unwrap();
        let bytes = vec![b' '; REFERENCE_LIMIT + 1];
        let server = server(
            wire(201, &format!("Content-Length: {}\r\n", bytes.len()), &bytes),
            Duration::from_millis(20),
        )
        .await;
        assert!(
            lane.reference(
                &Fake::new(&server.bind),
                "owner",
                b"abc".to_vec(),
                "image/png",
                None
            )
            .await
            .unwrap_err()
            .contains("size limit")
        );
        let r = reference(b"abc");
        let mut fake = Fake::new("must-not-connect");
        fake.stat = Ok(stat(&r, &[]));
        lane.pin_reference(&fake, &r, "owner").await.unwrap();
        assert_eq!(
            *fake.calls.lock().unwrap(),
            vec![format!("stat:{}", r.blob), format!("pin:{}:owner", r.blob)]
        );
        assert_eq!(fake.binds.load(Ordering::SeqCst), 0);
    }
}
