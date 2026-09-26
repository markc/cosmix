//! The byte lane: `GET/HEAD/PUT/POST /blob` on the WireGuard address.
//!
//! Bytes never ride a Bus frame — this listener is how they move between
//! nodes and in from user-side producers (capture, webd, Thunderbird).
//! Every fetch is verified by hash on arrival, so the lane needs no
//! integrity of its own: the BLAKE3 hash is the identity.
//!
//! **Bind proof, fail closed:** a lane is only ever constructed by
//! `main`, after [`bind_is_wg`] has proved the bind IP equals this
//! node's `wg_ip` (from `node.conf.mix`, the same source noded uses) —
//! never unspecified, never loopback, never another interface. A
//! mismatch is exit 2 before any socket is opened.
//!
//! **Bounded, restart-only:** uploads stream into mds staging under a
//! byte counter that aborts at the lane owner's remaining quota or the
//! total cap (413, staging deleted); an idle body (30 s) and the
//! concurrent-upload bound (`lane_max_uploads`, 503 beyond it, no
//! queue) bound the lane's resources. A dropped upload starts again —
//! v1 has no resume.

use std::io::{self, Read};
use std::net::SocketAddr;
use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use axum::Router;
use axum::body::Body;
use axum::extract::{ConnectInfo, Path as AxumPath, State};
use axum::http::{HeaderMap, Method, StatusCode, header};
use axum::response::Response;
use axum::routing::{get, post};
use cosmix_mds::blob;
use cosmix_mds::types::BlobHash;
use http_body_util::BodyExt;
use tokio::net::TcpListener;
use tokio::sync::{Semaphore, mpsc};
use tokio_util::io::ReaderStream;

use crate::core::mime;
use crate::core::reference::Reference;
use crate::core::store::{PutOutcome, Store, StoreError};

/// Idle request-body timeout: no data frame for this long aborts the
/// upload (staging deleted) and answers 408.
const IDLE_TIMEOUT: Duration = Duration::from_secs(30);
/// Frames buffered between the async body pump and the blocking CAS
/// writer. Small on purpose: the bound is the backpressure law, not
/// throughput.
const PUMP_FRAMES: usize = 4;
/// Longest `X-Cosmix-Owner` accepted; longer (or empty) values fall
/// back to the peer-derived owner.
const MAX_OWNER_HEADER: usize = 128;

/// Why a streaming upload was aborted mid-body. Travels through the
/// pump channel inside an `io::Error` so the CAS writer's error path
/// (which deletes the staging file) runs before the handler maps it
/// back to a status code.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum LaneAbort {
    /// The byte counter passed the owner/total cap.
    Cap,
    /// No body data for [`IDLE_TIMEOUT`].
    Idle,
}

impl std::fmt::Display for LaneAbort {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Cap => write!(f, "quota: upload exceeded the remaining cap mid-stream"),
            Self::Idle => write!(f, "request body idle for over 30s"),
        }
    }
}

impl std::error::Error for LaneAbort {}

fn abort_error(abort: LaneAbort) -> io::Error {
    io::Error::other(abort)
}

/// Fail-closed WG bind proof, copied from noded (`bind_is_wg`): the
/// bind IP must be this node's **own WG/mesh IP** (`wg_ip`), never
/// `0.0.0.0`/any (which would expose the lane off-mesh) and never some
/// other concrete public/LAN/loopback address (which is not the WG
/// interface). An unparseable bind or `wg_ip` fails closed — blobd
/// proves it is on the WG address, it does not trust its own config.
pub fn bind_is_wg(bind: &str, wg_ip: &str) -> bool {
    let bind_ip = match bind.parse::<SocketAddr>() {
        Ok(addr) => addr.ip(),
        Err(_) => return false,
    };
    if bind_ip.is_unspecified() {
        return false;
    }
    match wg_ip.parse::<std::net::IpAddr>() {
        Ok(wg) => bind_ip == wg,
        Err(_) => false,
    }
}

/// Shared lane state: the store and the upload-admission semaphore.
/// Reads are unbounded (they cost one open file each); uploads hold a
/// permit for their whole body. `gets` counts served `GET`s (the
/// single-flight test asserts exactly one download per hash).
pub struct Lane {
    store: Arc<Store>,
    uploads: Semaphore,
    gets: AtomicU64,
}

impl Lane {
    fn new(store: Arc<Store>, max_uploads: usize) -> Self {
        Self {
            store,
            uploads: Semaphore::new(max_uploads),
            gets: AtomicU64::new(0),
        }
    }

    #[cfg(test)]
    pub(crate) fn gets(&self) -> u64 {
        self.gets.load(Ordering::Relaxed)
    }
}

/// The lane's route table, shared by [`serve_lane`] and the test
/// constructor.
fn lane_router(lane: Arc<Lane>) -> Router {
    Router::new()
        .route(
            "/blob/{hex}",
            get(get_blob).put(put_upload).post(post_upload),
        )
        .route("/blob", post(post_upload))
        .with_state(lane)
}

/// Serve the byte lane on an already-bound `listener`. The WG bind
/// proof is the caller's (main's) job — see [`bind_is_wg`].
pub async fn serve_lane(
    listener: TcpListener,
    store: Arc<Store>,
    max_uploads: usize,
) -> std::io::Result<()> {
    let lane = Arc::new(Lane::new(store, max_uploads));
    axum::serve(
        listener,
        lane_router(lane).into_make_service_with_connect_info::<SocketAddr>(),
    )
    .await
}

// ---- GET/HEAD ----

async fn get_blob(
    State(lane): State<Arc<Lane>>,
    method: Method,
    AxumPath(hex): AxumPath<String>,
    headers: HeaderMap,
) -> Response {
    if method == Method::GET {
        lane.gets.fetch_add(1, Ordering::Relaxed);
    }
    let Some(hash) = blob::from_hex(&hex) else {
        return lane_error(StatusCode::BAD_REQUEST, "invalid blob id");
    };
    let root = lane.store.blobs_root();
    if !matches!(blob::exists(&root, &hash), Ok(true)) {
        return lane_error(StatusCode::NOT_FOUND, "not_present");
    }
    let size = match blob::size(&root, &hash) {
        Ok(size) => size,
        Err(e) => return lane_error(StatusCode::INTERNAL_SERVER_ERROR, &e.to_string()),
    };
    let mime = lane
        .store
        .stat(&hash)
        .ok()
        .and_then(|s| s.mime)
        .unwrap_or_else(|| "application/octet-stream".to_string());

    let mut status = StatusCode::OK;
    let (start, len) = match headers.get(header::RANGE).and_then(|v| v.to_str().ok()) {
        None => (0, size),
        Some(spec) => match parse_range(spec, size) {
            RangeParse::Ignore => (0, size),
            RangeParse::Unsatisfiable => {
                let mut response = json_response(
                    StatusCode::RANGE_NOT_SATISFIABLE,
                    serde_json::json!({"error": format!("range not satisfiable: blob is {size} bytes")}),
                );
                response.headers_mut().insert(
                    header::CONTENT_RANGE,
                    format!("bytes */{size}")
                        .parse()
                        .expect("content-range header value"),
                );
                return response;
            }
            RangeParse::Satisfiable(start, len) => {
                status = StatusCode::PARTIAL_CONTENT;
                (start, len)
            }
        },
    };

    let mut builder = Response::builder()
        .status(status)
        .header(header::CONTENT_TYPE, mime)
        .header(header::CONTENT_LENGTH, len.to_string())
        .header(header::ACCEPT_RANGES, "bytes")
        .header(header::ETAG, format!("\"{}\"", blob::hex(&hash)))
        .header(header::CACHE_CONTROL, "immutable");
    if status == StatusCode::PARTIAL_CONTENT {
        builder = builder.header(
            header::CONTENT_RANGE,
            format!("bytes {start}-{}/{size}", start + len - 1),
        );
    }
    // HEAD mirrors GET's headers without paying for the stream: the
    // body is the only difference by construction.
    let body = if method == Method::HEAD {
        Body::empty()
    } else {
        match stream_range(&root, &hash, start, len).await {
            Ok(body) => body,
            Err(e) => return lane_error(StatusCode::INTERNAL_SERVER_ERROR, &e.to_string()),
        }
    };
    builder
        .body(body)
        .unwrap_or_else(|e| lane_error(StatusCode::INTERNAL_SERVER_ERROR, &e.to_string()))
}

/// Stream `len` bytes from the CAS file at offset `start` — the blob is
/// never read into memory whole.
async fn stream_range(
    root: &Path,
    hash: &BlobHash,
    start: u64,
    len: u64,
) -> Result<Body, cosmix_mds::Error> {
    use tokio::io::{AsyncReadExt, AsyncSeekExt};
    let file = tokio::fs::File::from(blob::open(root, hash)?);
    let mut reader = tokio::io::BufReader::with_capacity(128 * 1024, file);
    reader.seek(io::SeekFrom::Start(start)).await?;
    Ok(Body::from_stream(ReaderStream::new(reader.take(len))))
}

/// What a `Range` header asked for.
enum RangeParse {
    /// Syntactically not a single satisfiable spec — per RFC the header
    /// is ignored and the whole blob is served as 200.
    Ignore,
    /// Parsed, but names no byte of this blob → 416.
    Unsatisfiable,
    /// `start` and a non-zero `len` within the blob.
    Satisfiable(u64, u64),
}

/// Parse a single `bytes=a-b` / `bytes=a-` / `bytes=-n` range against
/// a blob of `size` bytes. Multiple ranges and malformed specs are
/// `Ignore` (RFC 9110: an unsatisfiable-or-invalid list is ignored
/// unless *all* valid specs are unsatisfiable — a lone spec that parses
/// but starts at/after EOF, or a zero suffix, is `Unsatisfiable`).
fn parse_range(spec: &str, size: u64) -> RangeParse {
    let Some(spec) = spec.trim().strip_prefix("bytes=") else {
        return RangeParse::Ignore;
    };
    if spec.contains(',') {
        return RangeParse::Ignore;
    }
    let is_digits = |s: &str| !s.is_empty() && s.bytes().all(|b| b.is_ascii_digit());
    if let Some(suffix) = spec.strip_prefix('-') {
        // Last n bytes; n = 0 names nothing (RFC 9110 §14.1.2).
        if !is_digits(suffix) {
            return RangeParse::Ignore;
        }
        let n: u64 = suffix.parse().expect("digits");
        if n == 0 || size == 0 {
            return RangeParse::Unsatisfiable;
        }
        let len = n.min(size);
        return RangeParse::Satisfiable(size - len, len);
    }
    let Some((first, last)) = spec.split_once('-') else {
        return RangeParse::Ignore;
    };
    if !is_digits(first) {
        return RangeParse::Ignore;
    }
    let start: u64 = first.parse().expect("digits");
    // An absent last-byte-pos runs to EOF; a present one is clamped to
    // it (RFC 9110 §14.1.1) — saturating, because size 0 is decided by
    // the start check below.
    let end = if last.is_empty() {
        size.saturating_sub(1)
    } else {
        if !is_digits(last) {
            return RangeParse::Ignore;
        }
        let end: u64 = last.parse().expect("digits");
        if end < start {
            return RangeParse::Ignore;
        }
        end.min(size.saturating_sub(1))
    };
    if start >= size {
        return RangeParse::Unsatisfiable;
    }
    RangeParse::Satisfiable(start, end - start + 1)
}

// ---- PUT/POST ----

/// `PUT /blob/<hex>`: the client already knows the hash. If the CAS
/// already holds verified bytes for it the server answers 200 without
/// reading the body — the hash is the identity, the body cannot change
/// it — and pins it to the lane owner. Otherwise the body streams into
/// staging and the landed hash must equal `<hex>` or the answer is 422.
async fn put_upload(
    State(lane): State<Arc<Lane>>,
    ConnectInfo(peer): ConnectInfo<SocketAddr>,
    AxumPath(hex): AxumPath<String>,
    headers: HeaderMap,
    body: Body,
) -> Response {
    let Some(expected) = blob::from_hex(&hex) else {
        return lane_error(StatusCode::BAD_REQUEST, "invalid blob id");
    };
    lane.upload(Some(expected), peer, &headers, body).await
}

/// `POST /blob`: server-hashed streaming upload for clients that
/// cannot compute BLAKE3 (`curl`, browsers, Thunderbird FileLink).
/// Whatever hash the body lands under is the truth.
async fn post_upload(
    State(lane): State<Arc<Lane>>,
    ConnectInfo(peer): ConnectInfo<SocketAddr>,
    headers: HeaderMap,
    body: Body,
) -> Response {
    lane.upload(None, peer, &headers, body).await
}

/// One frame crossing the pump channel. `Eof` is the explicit
/// clean-end marker: a channel that closes without it (a cancelled or
/// panicked pump) is a hard read error, never a silent truncation.
/// Shared with `blob.fetch`, which pumps a lane response the same way
/// the lane pumps a request body.
pub(crate) enum Frame {
    Data(axum::body::Bytes),
    Eof,
    Abort(io::Error),
}

/// The blocking half of the pump: an `io::Read` view of the async
/// body, consumed by `blob::put_reader`/`put_reader_expect` on the
/// blocking pool.
pub(crate) struct ChannelReader {
    rx: mpsc::Receiver<Frame>,
    chunk: axum::body::Bytes,
    pos: usize,
    eof: bool,
}

impl ChannelReader {
    pub(crate) fn new(rx: mpsc::Receiver<Frame>) -> Self {
        Self {
            rx,
            chunk: axum::body::Bytes::new(),
            pos: 0,
            eof: false,
        }
    }
}

impl Read for ChannelReader {
    fn read(&mut self, out: &mut [u8]) -> io::Result<usize> {
        if out.is_empty() {
            return Ok(0);
        }
        loop {
            if self.pos < self.chunk.len() {
                let n = out.len().min(self.chunk.len() - self.pos);
                out[..n].copy_from_slice(&self.chunk[self.pos..self.pos + n]);
                self.pos += n;
                return Ok(n);
            }
            if self.eof {
                return Ok(0);
            }
            match self.rx.blocking_recv() {
                Some(Frame::Data(chunk)) => {
                    self.chunk = chunk;
                    self.pos = 0;
                }
                Some(Frame::Eof) => self.eof = true,
                Some(Frame::Abort(e)) => return Err(e),
                None => {
                    return Err(io::Error::new(
                        io::ErrorKind::BrokenPipe,
                        "upload pump ended without EOF",
                    ));
                }
            }
        }
    }
}

impl Lane {
    /// The shared upload pipeline for PUT (expected hash known) and
    /// POST (server-hashed).
    async fn upload(
        &self,
        expected: Option<BlobHash>,
        peer: SocketAddr,
        headers: &HeaderMap,
        body: Body,
    ) -> Response {
        let owner = lane_owner(headers, peer);
        let name = string_header(headers, "x-cosmix-name");
        let mime = string_header(headers, "x-cosmix-mime")
            .or_else(|| name.as_deref().map(mime::sniff).map(str::to_string))
            .unwrap_or_else(|| "application/octet-stream".to_string());

        // Before the first byte: the hash is the identity. If the CAS
        // already holds verified bytes, pin them and answer 200 without
        // reading the body at all.
        if let Some(expected) = expected
            && matches!(blob::exists(&self.store.blobs_root(), &expected), Ok(true))
        {
            return match self.finish_present(&expected, &mime, name.as_deref(), &owner) {
                Ok(outcome) => reference_response(StatusCode::OK, &outcome.reference),
                Err(e) => lane_error(StatusCode::INTERNAL_SERVER_ERROR, &e.to_string()),
            };
        }

        // Admission: a bounded number of concurrent uploads, no queue —
        // beyond the bound the lane refuses immediately.
        let _permit = match self.uploads.try_acquire() {
            Ok(permit) => permit,
            Err(_) => {
                return lane_error(
                    StatusCode::SERVICE_UNAVAILABLE,
                    "lane busy: too many concurrent uploads",
                );
            }
        };

        // Caps: quota is reserved at admission (M3) — the declared
        // Content-Length, or the owner's whole remaining room when
        // absent — so concurrent uploads cannot each spend the same
        // headroom. The reservation's bound is the mid-stream counter;
        // it releases when the pin lands (or the upload aborts).
        let reservation = match self.store.reserve_upload(&owner, content_length(headers)) {
            Ok(reservation) => reservation,
            Err(e @ (StoreError::QuotaOwner { .. } | StoreError::QuotaTotal { .. })) => {
                return lane_error(StatusCode::PAYLOAD_TOO_LARGE, &e.to_string());
            }
            Err(e) => return lane_error(StatusCode::INTERNAL_SERVER_ERROR, &e.to_string()),
        };
        let cap = reservation.cap();

        let (tx, rx) = mpsc::channel::<Frame>(PUMP_FRAMES);
        let pump = tokio::spawn(pump_body(body, tx, cap));
        let store = Arc::clone(&self.store);
        let landed = tokio::task::spawn_blocking(move || {
            stream_into_store(&store, rx, expected, mime, name, owner, reservation)
        })
        .await;

        match landed {
            Ok(Ok(UploadOutcome::Committed(outcome))) => {
                let _ = pump.await;
                reference_response(StatusCode::CREATED, &outcome.reference)
            }
            Ok(Ok(UploadOutcome::Mismatch { message })) => {
                let _ = pump.await;
                json_response(
                    StatusCode::UNPROCESSABLE_ENTITY,
                    serde_json::json!({ "error": message }),
                )
            }
            Ok(Err(UploadError::Io(e))) => {
                let _ = pump.await;
                // The abort reason rode inside the io::Error that
                // stopped the stream; the staging writer has already
                // deleted the staged file by the time it surfaces
                // here.
                let abort = match &e {
                    cosmix_mds::Error::Io(io) => io
                        .get_ref()
                        .and_then(|r| r.downcast_ref::<LaneAbort>().copied()),
                    _ => None,
                };
                match abort {
                    Some(LaneAbort::Cap) => {
                        lane_error(StatusCode::PAYLOAD_TOO_LARGE, &e.to_string())
                    }
                    Some(LaneAbort::Idle) => {
                        lane_error(StatusCode::REQUEST_TIMEOUT, &e.to_string())
                    }
                    None => lane_error(StatusCode::INTERNAL_SERVER_ERROR, &e.to_string()),
                }
            }
            Ok(Err(UploadError::Store(e))) => {
                lane_error(StatusCode::INTERNAL_SERVER_ERROR, &e.to_string())
            }
            Err(join) => lane_error(
                StatusCode::INTERNAL_SERVER_ERROR,
                &format!("upload task failed: {join}"),
            ),
        }
    }

    /// Bookkeeping for the already-present PUT: attrs (kept if the
    /// blob already has them) plus the lane-owner pin. The bytes are on
    /// disk already, so — like `blob.pin` — no quota check applies: the
    /// pin only accounts for what exists.
    fn finish_present(
        &self,
        hash: &BlobHash,
        mime: &str,
        name: Option<&str>,
        owner: &str,
    ) -> Result<PutOutcome, StoreError> {
        let size = blob::size(&self.store.blobs_root(), hash)?;
        self.store.record_upload(hash, size, mime, name, owner)
    }
}

/// Pump the async request body into the channel the blocking CAS
/// writer reads, enforcing the mid-stream cap and the idle timeout.
/// Every exit path except a closed channel signals the reader
/// explicitly (`Eof` or `Abort`); on a cap or idle abort the CAS
/// writer's error path deletes the staging file before the handler
/// answers.
async fn pump_body(mut body: Body, tx: mpsc::Sender<Frame>, cap: u64) {
    let mut count: u64 = 0;
    loop {
        let frame = match tokio::time::timeout(IDLE_TIMEOUT, body.frame()).await {
            Err(_) => {
                let _ = tx.send(Frame::Abort(abort_error(LaneAbort::Idle))).await;
                return;
            }
            Ok(None) => {
                let _ = tx.send(Frame::Eof).await;
                return;
            }
            Ok(Some(Err(e))) => {
                let _ = tx
                    .send(Frame::Abort(io::Error::other(format!("request body: {e}"))))
                    .await;
                return;
            }
            Ok(Some(Ok(frame))) => frame,
        };
        let Ok(bytes) = frame.into_data() else {
            continue; // trailers and other non-data frames carry no bytes
        };
        if bytes.is_empty() {
            continue;
        }
        count += bytes.len() as u64;
        if count > cap {
            let _ = tx.send(Frame::Abort(abort_error(LaneAbort::Cap))).await;
            return;
        }
        if tx.send(Frame::Data(bytes)).await.is_err() {
            return; // reader gone; it owns the error surface now
        }
    }
}

enum UploadOutcome {
    Committed(PutOutcome),
    /// The PUT's body did not hash to the claimed id. mds's
    /// `put_reader_expect` rejected it before anything committed, so
    /// the CAS holds no entry for either hash and staging is empty;
    /// the message names both hashes.
    Mismatch { message: String },
}

enum UploadError {
    Io(cosmix_mds::Error),
    Store(StoreError),
}

/// The blocking half of an upload: stream the body into mds staging
/// and land it. A PUT (expected hash known) goes through
/// `blob::put_reader_expect`, which compares **before** committing —
/// a body that does not hash to the claimed id never enters the CAS
/// under either hash and comes back as [`UploadOutcome::Mismatch`]. A
/// POST is server-hashed: `blob::put_reader`, the landed hash is the
/// truth. The reservation is held to the end of this frame — after
/// `record_upload` settles the real size — so the admission headroom
/// never double-counts and never leaks (M3).
fn stream_into_store(
    store: &Arc<Store>,
    rx: mpsc::Receiver<Frame>,
    expected: Option<BlobHash>,
    mime: String,
    name: Option<String>,
    owner: String,
    reservation: crate::core::store::Reservation,
) -> Result<UploadOutcome, UploadError> {
    let reader = ChannelReader::new(rx);
    let (landed, size) = match expected {
        Some(expected) => match blob::put_reader_expect(&store.blobs_root(), reader, &expected) {
            Ok(landed) => landed,
            Err(cosmix_mds::Error::BlobCorrupt(message)) => {
                return Ok(UploadOutcome::Mismatch { message })
            }
            Err(other) => return Err(UploadError::Io(other)),
        },
        None => blob::put_reader(&store.blobs_root(), reader).map_err(UploadError::Io)?,
    };

    let outcome = store
        .record_upload(&landed, size, &mime, name.as_deref(), &owner)
        .map_err(UploadError::Store)?;
    // Settle: the pin now accounts the real size; the admission's
    // headroom releases (on the error paths above, the `?` dropped it).
    drop(reservation);
    Ok(UploadOutcome::Committed(outcome))
}

// ---- Headers and responses ----

/// The lane carries no Bus identity, so an upload is pinned to
/// `lane:<peer ip>` unless `X-Cosmix-Owner` names a more specific
/// owner (non-empty, bounded length — it is a pin row key).
fn lane_owner(headers: &HeaderMap, peer: SocketAddr) -> String {
    headers
        .get("x-cosmix-owner")
        .and_then(|v| v.to_str().ok())
        .filter(|s| !s.is_empty() && s.len() <= MAX_OWNER_HEADER)
        .map(str::to_string)
        .unwrap_or_else(|| format!("lane:{}", peer.ip()))
}

fn string_header(headers: &HeaderMap, name: &str) -> Option<String> {
    headers
        .get(name)
        .and_then(|v| v.to_str().ok())
        .filter(|s| !s.is_empty() && s.len() <= MAX_OWNER_HEADER)
        .map(str::to_string)
}

fn content_length(headers: &HeaderMap) -> Option<u64> {
    headers
        .get(header::CONTENT_LENGTH)?
        .to_str()
        .ok()?
        .parse()
        .ok()
}

/// The D6 reference JSON, the exact shape every verb returns.
fn reference_response(status: StatusCode, reference: &Reference) -> Response {
    json_response(status, reference.to_json())
}

fn lane_error(status: StatusCode, message: &str) -> Response {
    json_response(status, serde_json::json!({ "error": message }))
}

fn json_response(status: StatusCode, body: serde_json::Value) -> Response {
    Response::builder()
        .status(status)
        .header(header::CONTENT_TYPE, "application/json")
        .body(Body::from(body.to_string()))
        .expect("static response parts")
}

/// TESTS ONLY, shared with `blob.fetch`'s tests: a lane bound to
/// loopback, its store options, and deterministic pseudo-random bytes.
/// In production a lane is constructed only by main, and only after
/// `bind_is_wg` has proved the bind is this node's own WG address —
/// fail closed, exit 2 before any socket; loopback is never a legal
/// production bind. The proof itself is a pure function with its own
/// unit test below (the arm-6 shape).
#[cfg(test)]
pub(crate) mod test_support {
    use super::*;
    use crate::core::store::StoreOptions;
    use std::collections::BTreeMap;
    use tempfile::TempDir;

    pub(crate) fn options() -> StoreOptions {
        StoreOptions {
            origin: "testnode".into(),
            quota_total_bytes: crate::core::DEFAULT_QUOTA_TOTAL_BYTES,
            quota_owner_default_bytes: crate::core::DEFAULT_QUOTA_OWNER_BYTES,
            owner_limits: BTreeMap::new(),
            cas_group: "cosmix-blob".into(),
        }
    }

    /// Store options naming this lane's node `origin` (references and
    /// fetch bookkeeping report it).
    pub(crate) fn options_for(origin: &str) -> StoreOptions {
        StoreOptions {
            origin: origin.into(),
            ..options()
        }
    }

    /// Deterministic pseudo-random bytes (xorshift64*): incompressible
    /// enough that no accidental dedup hides a wrong-range read, and
    /// reproducible across runs.
    pub(crate) fn pseudo_random(len: usize, seed: u64) -> Vec<u8> {
        let mut out = Vec::with_capacity(len);
        let mut state = seed | 1;
        while out.len() < len {
            state ^= state >> 12;
            state ^= state << 25;
            state ^= state >> 27;
            out.extend(state.wrapping_mul(0x2545_F491_4F6C_DD1D).to_le_bytes());
        }
        out.truncate(len);
        out
    }

    /// A loopback lane with its GET counter exposed (the fetch tests
    /// assert exactly one download per hash).
    pub(crate) async fn counted_lane(
        options: StoreOptions,
    ) -> (TempDir, Arc<Store>, SocketAddr, Arc<Lane>) {
        let dir = TempDir::new().unwrap();
        let store = Arc::new(Store::open(dir.path(), options).unwrap());
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let lane = Arc::new(Lane::new(Arc::clone(&store), 4));
        let serve_lane = Arc::clone(&lane);
        tokio::spawn(async move {
            if let Err(error) = axum::serve(
                listener,
                lane_router(serve_lane).into_make_service_with_connect_info::<SocketAddr>(),
            )
            .await
            {
                eprintln!("test lane stopped: {error}");
            }
        });
        (dir, store, addr, lane)
    }

    /// A loopback lane without the counter handle.
    pub(crate) async fn test_lane(options: StoreOptions) -> (TempDir, Arc<Store>, SocketAddr) {
        let (dir, store, addr, _lane) = counted_lane(options).await;
        (dir, store, addr)
    }
}

#[cfg(test)]
mod tests {
    use super::test_support::{options, pseudo_random, test_lane};
    use super::*;
    use crate::core::store::StoreOptions;
    use std::collections::BTreeMap;
    // `Read` arrives via `use super::*`; `BufRead` and `Write` are
    // test-local.
    use std::io::{BufRead, BufReader, Write as _};
    use std::net::TcpStream;

    /// A whole HTTP/1.1 exchange over a raw socket: the lane only ever
    /// answers with Content-Length bodies, so the response is read
    /// exactly (HEAD excepted — headers only, by definition). Returns
    /// (status, lowercase-header map, body).
    fn request(
        addr: SocketAddr,
        method: &str,
        path: &str,
        headers: &[(&str, &str)],
        body: &[u8],
    ) -> (u16, Vec<(String, String)>, Vec<u8>) {
        let mut stream = TcpStream::connect(addr).unwrap();
        stream
            .set_read_timeout(Some(Duration::from_secs(60)))
            .unwrap();
        let mut head = format!("{method} {path} HTTP/1.1\r\nHost: blobd.test\r\n");
        for (name, value) in headers {
            head.push_str(&format!("{name}: {value}\r\n"));
        }
        head.push_str(&format!("Content-Length: {}\r\n\r\n", body.len()));
        stream.write_all(head.as_bytes()).unwrap();
        stream.write_all(body).unwrap();
        read_response(&mut stream, method == "HEAD")
    }

    /// Read a status line, headers and a Content-Length body (absent
    /// for `is_head`).
    fn read_response(
        stream: &mut TcpStream,
        is_head: bool,
    ) -> (u16, Vec<(String, String)>, Vec<u8>) {
        let mut reader = BufReader::new(stream);
        let mut line = String::new();
        reader.read_line(&mut line).unwrap();
        let status: u16 = line
            .split_whitespace()
            .nth(1)
            .and_then(|code| code.parse().ok())
            .unwrap_or_else(|| panic!("bad status line {line:?}"));
        let mut headers = Vec::new();
        loop {
            let mut line = String::new();
            reader.read_line(&mut line).unwrap();
            let line = line.trim_end();
            if line.is_empty() {
                break;
            }
            let (name, value) = line.split_once(':').expect("header colon");
            headers.push((name.trim().to_ascii_lowercase(), value.trim().to_string()));
        }
        let len: usize = if is_head {
            0
        } else {
            headers
                .iter()
                .find(|(name, _)| name == "content-length")
                .and_then(|(_, value)| value.parse().ok())
                .unwrap_or(0)
        };
        let mut body = vec![0u8; len];
        reader.read_exact(&mut body).unwrap();
        (status, headers, body)
    }

    fn header_value<'a>(headers: &'a [(String, String)], name: &str) -> &'a str {
        headers
            .iter()
            .find(|(n, _)| n == name)
            .map(|(_, v)| v.as_str())
            .unwrap_or_default()
    }

    /// `blobs/.tmp` holds nothing — the abort paths' no-residue claim.
    fn tmp_is_empty(store: &Store) -> bool {
        match std::fs::read_dir(store.blobs_root().join(".tmp")) {
            Ok(mut entries) => entries.next().is_none(),
            Err(e) if e.kind() == io::ErrorKind::NotFound => true,
            Err(_) => false,
        }
    }

    // ---- Gate arm 1: size-forced lane ----

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn arm1_put_32mib_get_back_whole_and_via_two_ranges() {
        let (_dir, store, addr) = test_lane(options()).await;
        // Above MAX_MESSAGE_BYTES (16 MiB), so this body can never ride
        // a Bus frame — the lane is the only path.
        let bytes = pseudo_random(32 * 1024 * 1024 + 123, 0x5EED);
        let hash = blob::hash_bytes(&bytes);
        let path = format!("/blob/{}", blob::hex(&hash));

        let (status, _, body) = request(addr, "PUT", &path, &[], &bytes);
        assert_eq!(status, 201, "{}", String::from_utf8_lossy(&body));
        let reference: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(reference["blob"], format!("b3:{}", blob::hex(&hash)));
        assert_eq!(reference["size"], bytes.len() as u64);
        assert_eq!(reference["origin"], "testnode");

        // The upload is pinned to the lane owner and accounted.
        let stat = store.stat(&hash).unwrap();
        assert!(stat.present);
        assert_eq!(stat.pins, vec!["lane:127.0.0.1".to_string()]);
        assert_eq!(
            store.quota_report(None).unwrap().total.used,
            bytes.len() as u64
        );

        // Whole read.
        let (status, headers, body) = request(addr, "GET", &path, &[], b"");
        assert_eq!(status, 200);
        assert_eq!(
            header_value(&headers, "content-length"),
            bytes.len().to_string()
        );
        assert_eq!(
            header_value(&headers, "etag"),
            format!("\"{}\"", blob::hex(&hash))
        );
        assert_eq!(header_value(&headers, "accept-ranges"), "bytes");
        assert_eq!(header_value(&headers, "cache-control"), "immutable");
        assert_eq!(
            header_value(&headers, "content-type"),
            "application/octet-stream"
        );
        assert_eq!(body, bytes);

        // Range one: an interior window.
        let (status, headers, body) = request(
            addr,
            "GET",
            &path,
            &[("Range", "bytes=1048576-2097151")],
            b"",
        );
        let size = bytes.len() as u64;
        assert_eq!(status, 206);
        assert_eq!(
            header_value(&headers, "content-range"),
            format!("bytes 1048576-2097151/{size}")
        );
        assert_eq!(body, bytes[1_048_576..2_097_152]);

        // Range two: a suffix.
        let (status, headers, body) = request(addr, "GET", &path, &[("Range", "bytes=-4096")], b"");
        assert_eq!(status, 206);
        assert_eq!(
            header_value(&headers, "content-range"),
            format!("bytes {}-{}/{size}", size - 4096, size - 1)
        );
        assert_eq!(body, bytes[bytes.len() - 4096..]);
    }

    // ---- Gate arm 4: wrong bytes ----

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn arm4_put_of_wrong_hash_is_422_with_no_residue() {
        let (_dir, store, addr) = test_lane(options()).await;
        let bytes = pseudo_random(1024 * 1024, 0xBEEF);
        let claimed = blob::hash_bytes(b"entirely different bytes");
        let path = format!("/blob/{}", blob::hex(&claimed));

        let (status, headers, body) = request(addr, "PUT", &path, &[], &bytes);
        assert_eq!(status, 422, "{}", String::from_utf8_lossy(&body));
        assert_eq!(header_value(&headers, "content-type"), "application/json");
        let error: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert!(error["error"].as_str().unwrap().contains("hash mismatch"));

        // No entry for the claimed hash, none for what the body hashed
        // to (put_reader_expect rejected it before any commit), and
        // nothing left staging.
        assert!(!blob::exists(&store.blobs_root(), &claimed).unwrap());
        assert!(!blob::exists(&store.blobs_root(), &blob::hash_bytes(&bytes)).unwrap());
        assert!(tmp_is_empty(&store));
        assert_eq!(store.quota_report(None).unwrap().total.used, 0);
    }

    // ---- Gate arm 5: quota, refused cleanly mid-stream ----

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn arm5_owner_cap_refuses_413_with_no_residue() {
        let capped = options();
        let StoreOptions {
            quota_total_bytes,
            quota_owner_default_bytes,
            ..
        } = capped.clone();
        let capped = StoreOptions {
            owner_limits: BTreeMap::from([("capped".to_string(), 1024 * 1024)]),
            quota_total_bytes,
            quota_owner_default_bytes,
            ..capped
        };
        let (_dir, store, addr) = test_lane(capped).await;

        // A declared Content-Length over the cap is refused before the
        // body is read: headers claim 2 MiB, only a prefix is sent.
        let mut stream = TcpStream::connect(addr).unwrap();
        stream
            .set_read_timeout(Some(Duration::from_secs(10)))
            .unwrap();
        let head = "PUT /blob/aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa \
                    HTTP/1.1\r\nHost: blobd.test\r\nX-Cosmix-Owner: capped\r\n\
                    Content-Length: 2097152\r\n\r\n";
        stream.write_all(head.as_bytes()).unwrap();
        stream.write_all(b"pref").unwrap();
        let (status, _, _) = read_response(&mut stream, false);
        assert_eq!(status, 413);

        // A lying length meets the same limit mid-stream: chunked (no
        // Content-Length to pre-check), 2 MiB total, abort at the cap.
        // The reader runs on its own thread because the server stops
        // reading at the cap and the remaining writes can fail — the
        // 413 is what matters, and it is consumed as it arrives.
        let mut stream = TcpStream::connect(addr).unwrap();
        stream
            .set_read_timeout(Some(Duration::from_secs(10)))
            .unwrap();
        let mut response_stream = stream.try_clone().unwrap();
        let reader = std::thread::spawn(move || read_response(&mut response_stream, false));
        let head = "POST /blob HTTP/1.1\r\nHost: blobd.test\r\nX-Cosmix-Owner: capped\r\n\
                    Transfer-Encoding: chunked\r\n\r\n";
        stream.write_all(head.as_bytes()).unwrap();
        let chunk = pseudo_random(128 * 1024, 0xCA11);
        for _ in 0..16 {
            // 2 MiB total: past the 1 MiB cap.
            let piece = format!("{:x}\r\n", chunk.len()).into_bytes();
            if stream.write_all(&piece).is_err()
                || stream.write_all(&chunk).is_err()
                || stream.write_all(b"\r\n").is_err()
            {
                break; // the abort closed its end; the 413 is already out
            }
        }
        let (status, _, body) = reader.join().expect("reader thread");
        assert_eq!(status, 413, "{}", String::from_utf8_lossy(&body));
        assert!(String::from_utf8_lossy(&body).contains("quota"));

        // No residue, quota untouched.
        assert!(tmp_is_empty(&store));
        let report = store.quota_report(None).unwrap();
        assert_eq!(report.owners["capped"].used, 0);
        assert_eq!(report.total.used, 0);
        assert!(store.list(None, 10, None).unwrap().is_empty());
    }

    // ---- M3: concurrent uploads cannot overshoot the owner cap ----

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn two_concurrent_uploads_get_one_201_and_one_413() {
        // Two 700 KiB uploads against a 1 MiB owner cap, both in
        // flight at once: the admission reservation must refuse one
        // (413) before any byte — the old check-then-act read the cap
        // once and granted both.
        let capped = StoreOptions {
            owner_limits: BTreeMap::from([(("race").to_string(), 1024 * 1024)]),
            ..options()
        };
        let (_dir, store, addr) = test_lane(capped).await;

        // Slow distinct uploads (11 × 64 KiB chunks with a pause), so
        // both are admitted while the other is mid-body. The response
        // is read on its own thread: the refused upload's writes fail
        // with EPIPE once the lane answers 413 and stops reading —
        // the status is what matters, and it is consumed as it
        // arrives.
        let upload = |seed: u64| {
            std::thread::spawn(move || {
                let bytes = pseudo_random(700 * 1024, seed);
                let mut stream = TcpStream::connect(addr).unwrap();
                stream
                    .set_read_timeout(Some(Duration::from_secs(30)))
                    .unwrap();
                let mut response_stream = stream.try_clone().unwrap();
                let reader =
                    std::thread::spawn(move || read_response(&mut response_stream, false).0);
                let head = format!(
                    "POST /blob HTTP/1.1\r\nHost: blobd.test\r\nX-Cosmix-Owner: race\r\n\
                     Content-Length: {}\r\n\r\n",
                    bytes.len()
                );
                stream.write_all(head.as_bytes()).unwrap();
                for chunk in bytes.chunks(64 * 1024) {
                    if stream.write_all(chunk).is_err() {
                        break; // the refusal closed its end; the 413 is out
                    }
                    std::thread::sleep(Duration::from_millis(15));
                }
                reader.join().unwrap()
            })
        };
        let a = upload(0xA11CE);
        let b = upload(0xB0B);
        let (sa, sb) = (a.join().unwrap(), b.join().unwrap());
        assert!(
            [(sa, sb)].iter().any(|(x, y)| {
                (*x == 201 && *y == 413) || (*x == 413 && *y == 201)
            }),
            "exactly one 201 and one 413, got {sa}/{sb}"
        );

        // The cap held: exactly one 700 KiB blob accounted, nothing
        // staging, and the refused one left no bytes.
        let report = store.quota_report(None).unwrap();
        assert_eq!(report.owners["race"].used, 700 * 1024);
        assert_eq!(report.total.used, 700 * 1024);
        assert_eq!(report.total.reserved, 0, "the reservation settled");
        assert!(tmp_is_empty(&store));
    }

    // ---- Lane idempotence: PUT of a hash the CAS already holds ----

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn put_of_present_hash_answers_200_without_reading_the_body() {
        let (dir, store, addr) = test_lane(options()).await;
        let bytes = pseudo_random(64 * 1024, 0x01D0);
        let src = dir.path().join("present.bin");
        std::fs::write(&src, &bytes).unwrap();
        let outcome = store
            .put(&src, &crate::core::store::PutOptions::new("filesd"))
            .unwrap();
        let hash = outcome.reference.hash;
        let path = format!("/blob/{}", blob::hex(&hash));

        // Declare 10 MiB, send 3 bytes, and expect the 200 anyway: the
        // hash is the identity, so the body is never consumed (a server
        // that tried to read it would block on the missing bytes and
        // the 5-second read timeout below would fire instead).
        let mut stream = TcpStream::connect(addr).unwrap();
        stream
            .set_read_timeout(Some(Duration::from_secs(5)))
            .unwrap();
        let head =
            format!("PUT {path} HTTP/1.1\r\nHost: blobd.test\r\nContent-Length: 10485760\r\n\r\n");
        stream.write_all(head.as_bytes()).unwrap();
        stream.write_all(b"abc").unwrap();
        let (status, headers, body) = read_response(&mut stream, false);
        assert_eq!(status, 200, "{}", String::from_utf8_lossy(&body));
        assert_eq!(header_value(&headers, "content-type"), "application/json");
        let reference: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(reference["blob"], format!("b3:{}", blob::hex(&hash)));
        assert_eq!(reference["size"], bytes.len() as u64);

        // Pinned to the lane owner beside the original pin.
        let stat = store.stat(&hash).unwrap();
        assert!(stat.pins.contains(&"filesd".to_string()));
        assert!(stat.pins.contains(&"lane:127.0.0.1".to_string()));
    }

    // ---- POST: origin and stat ----

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn post_201_reference_origin_is_the_node_name_and_stat_sees_it() {
        let (_dir, store, addr) = test_lane(options()).await;
        let bytes = pseudo_random(4096, 0xB057);
        let (status, _, body) = request(
            addr,
            "POST",
            "/blob",
            &[
                ("X-Cosmix-Mime", "image/png"),
                ("X-Cosmix-Name", "shot.png"),
                ("X-Cosmix-Owner", "capture"),
            ],
            &bytes,
        );
        assert_eq!(status, 201, "{}", String::from_utf8_lossy(&body));
        let reference: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(reference["origin"], "testnode");
        assert_eq!(reference["mime"], "image/png");
        assert_eq!(reference["name"], "shot.png");
        assert_eq!(reference["size"], bytes.len() as u64);

        let hash = blob::from_hex(
            reference["blob"]
                .as_str()
                .unwrap()
                .strip_prefix("b3:")
                .unwrap(),
        )
        .unwrap();
        assert_eq!(hash, blob::hash_bytes(&bytes));
        let stat = store.stat(&hash).unwrap();
        assert!(stat.present);
        assert_eq!(stat.mime.as_deref(), Some("image/png"));
        assert_eq!(stat.pins, vec!["capture".to_string()]);
        assert_eq!(stat.origin.as_deref(), Some("testnode"));

        // And the lane serves what the POST landed.
        let (status, headers, body) = request(
            addr,
            "GET",
            &format!("/blob/{}", blob::hex(&hash)),
            &[("Range", "bytes=0-99")],
            b"",
        );
        assert_eq!(status, 206);
        assert_eq!(header_value(&headers, "content-type"), "image/png");
        assert_eq!(body, bytes[..100]);
    }

    // ---- Gate arm 6: WG bind proof (unit test on the pure function) ----

    #[test]
    fn arm6_bind_is_wg_fails_closed() {
        let wg = "10.42.0.5";
        // Only the node's own WG address passes.
        assert!(bind_is_wg("10.42.0.5:4210", wg));
        assert!(bind_is_wg("[fd00::5]:4210", "fd00::5"));
        // Unspecified, loopback, another interface: all refuse.
        assert!(!bind_is_wg("0.0.0.0:4210", wg));
        assert!(!bind_is_wg("[::]:4210", wg));
        assert!(!bind_is_wg("127.0.0.1:4210", wg));
        assert!(!bind_is_wg("[::1]:4210", wg));
        assert!(!bind_is_wg("10.42.0.6:4210", wg));
        assert!(!bind_is_wg("192.168.1.10:4210", wg));
        // Unparseable bind, absent or unparseable wg_ip: fail closed.
        assert!(!bind_is_wg("10.42.0.5", wg));
        assert!(!bind_is_wg("not-an-addr:4210", wg));
        assert!(!bind_is_wg("10.42.0.5:4210", ""));
        assert!(!bind_is_wg("10.42.0.5:4210", "wg.invalid"));
    }

    // ---- HEAD mirrors GET; 404 and 416 shapes ----

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn head_mirrors_get_and_error_shapes() {
        let (_dir, _store, addr) = test_lane(options()).await;
        let bytes = pseudo_random(8192, 0x4EA2);
        let hash = blob::hash_bytes(&bytes);
        let (status, _, _) = request(
            addr,
            "PUT",
            &format!("/blob/{}", blob::hex(&hash)),
            &[],
            &bytes,
        );
        assert_eq!(status, 201);
        let path = format!("/blob/{}", blob::hex(&hash));

        let (.., get_headers, _) = request(addr, "GET", &path, &[], b"");
        let (status, head_headers, body) = request(addr, "HEAD", &path, &[], b"");
        assert_eq!(status, 200);
        assert!(body.is_empty(), "HEAD carries no body");
        for name in [
            "content-length",
            "content-type",
            "etag",
            "accept-ranges",
            "cache-control",
        ] {
            assert_eq!(
                header_value(&get_headers, name),
                header_value(&head_headers, name),
                "HEAD must mirror GET's {name}"
            );
        }
        assert_eq!(header_value(&head_headers, "content-length"), "8192");

        // Unknown hash → 404 with the verb-style error token.
        let (status, _, body) = request(
            addr,
            "GET",
            "/blob/bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb",
            &[],
            b"",
        );
        assert_eq!(status, 404);
        assert!(String::from_utf8_lossy(&body).contains("not_present"));

        // A non-hash id → 400.
        let (status, _, body) = request(addr, "GET", "/blob/b3:not-hex-at-all", &[], b"");
        assert_eq!(status, 400);
        assert!(String::from_utf8_lossy(&body).contains("invalid blob id"));

        // Start at EOF → 416 with bytes */size.
        let (status, headers, _) = request(addr, "GET", &path, &[("Range", "bytes=8192-")], b"");
        assert_eq!(status, 416);
        assert_eq!(header_value(&headers, "content-range"), "bytes */8192");

        // A zero suffix → 416.
        let (status, _, _) = request(addr, "GET", &path, &[("Range", "bytes=-0")], b"");
        assert_eq!(status, 416);

        // Malformed and multi-range specs are ignored: whole blob, 200.
        let (status, _, body) = request(addr, "GET", &path, &[("Range", "bytes=9000-1")], b"");
        assert_eq!(status, 200);
        assert_eq!(body.len(), 8192);
        let (status, _, body) = request(addr, "GET", &path, &[("Range", "bytes=0-1,3-4")], b"");
        assert_eq!(status, 200);
        assert_eq!(body.len(), 8192);

        // A past-EOF last byte is clamped: whole-file 206.
        let (status, headers, body) =
            request(addr, "GET", &path, &[("Range", "bytes=0-99999999")], b"");
        assert_eq!(status, 206);
        assert_eq!(header_value(&headers, "content-range"), "bytes 0-8191/8192");
        assert_eq!(body.len(), 8192);
    }
}
