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
use std::time::{Duration, SystemTime};

use axum::body::Body;
use axum::extract::{ConnectInfo, Path as AxumPath, State};
use axum::http::{HeaderMap, Method, StatusCode, header};
use axum::response::Response;
use axum::routing::{get, post};
use axum::Router;
use cosmix_mds::blob;
use cosmix_mds::types::BlobHash;
use http_body_util::BodyExt;
use tokio::net::TcpListener;
use tokio::sync::{Semaphore, mpsc};
use tokio_util::io::ReaderStream;

use crate::core::mime;
use crate::core::reference::{Reference, blob_id};
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
/// pump channel inside an `io::Error` so `put_reader`'s error path
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
/// permit for their whole body.
pub struct Lane {
    store: Arc<Store>,
    uploads: Semaphore,
}

/// Serve the byte lane on an already-bound `listener`. The WG bind
/// proof is the caller's (main's) job — see [`bind_is_wg`].
pub async fn serve_lane(
    listener: TcpListener,
    store: Arc<Store>,
    max_uploads: usize,
) -> std::io::Result<()> {
    let lane = Arc::new(Lane {
        store,
        uploads: Semaphore::new(max_uploads),
    });
    let app = Router::new()
        .route(
            "/blob/{hex}",
            get(get_blob).put(put_upload).post(post_upload),
        )
        .route("/blob", post(post_upload))
        .with_state(lane);
    axum::serve(
        listener,
        app.into_make_service_with_connect_info::<SocketAddr>(),
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
    builder.body(body).unwrap_or_else(|e| {
        lane_error(StatusCode::INTERNAL_SERVER_ERROR, &e.to_string())
    })
}

/// Stream `len` bytes from the CAS file at offset `start` — the blob is
/// never read into memory whole.
async fn stream_range(root: &Path, hash: &BlobHash, start: u64, len: u64) -> Result<Body, cosmix_mds::Error> {
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
enum Frame {
    Data(axum::body::Bytes),
    Eof,
    Abort(io::Error),
}

/// The blocking half of the pump: an `io::Read` view of the async
/// request body, consumed by `blob::put_reader` on the blocking pool.
struct ChannelReader {
    rx: mpsc::Receiver<Frame>,
    chunk: axum::body::Bytes,
    pos: usize,
    eof: bool,
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

        // Caps: the tighter of the owner's remaining quota and the
        // total cap. A declared Content-Length is checked before any
        // byte is read; a lying (or absent) one meets the same limit
        // as a mid-stream byte counter.
        let cap = match self.store.upload_cap(&owner) {
            Ok(cap) => cap,
            Err(e) => return lane_error(StatusCode::INTERNAL_SERVER_ERROR, &e.to_string()),
        };
        if let Some(len) = content_length(headers)
            && len > cap
        {
            return lane_error(
                StatusCode::PAYLOAD_TOO_LARGE,
                &format!("quota: body of {len} bytes exceeds the remaining cap of {cap}"),
            );
        }

        let started = SystemTime::now();
        let (tx, rx) = mpsc::channel::<Frame>(PUMP_FRAMES);
        let pump = tokio::spawn(pump_body(body, tx, cap));
        let store = Arc::clone(&self.store);
        let landed = tokio::task::spawn_blocking(move || {
            stream_into_store(&store, rx, expected, started, mime, name, owner)
        })
        .await;

        match landed {
            Ok(Ok(UploadOutcome::Committed(outcome))) => {
                let _ = pump.await;
                reference_response(StatusCode::CREATED, &outcome.reference)
            }
            Ok(Ok(UploadOutcome::Mismatch {
                expected,
                landed,
                size,
            })) => {
                let _ = pump.await;
                json_response(
                    StatusCode::UNPROCESSABLE_ENTITY,
                    serde_json::json!({
                        "error": format!(
                            "hash mismatch: body hashes to {}, not {}; CAS unchanged",
                            blob_id(&landed),
                            blob_id(&expected),
                        ),
                        "blob": blob_id(&landed),
                        "size": size,
                    }),
                )
            }
            Ok(Err(UploadError::Io(e))) => {
                let _ = pump.await;
                // The abort reason rode inside the io::Error that
                // stopped the stream; put_reader has already deleted
                // the staging file by the time it surfaces here.
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
                    None => {
                        lane_error(StatusCode::INTERNAL_SERVER_ERROR, &e.to_string())
                    }
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
/// explicitly (`Eof` or `Abort`); on a cap or idle abort `put_reader`'s
/// error path deletes the staging file before the handler answers.
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
    Mismatch {
        expected: BlobHash,
        landed: BlobHash,
        size: u64,
    },
}

enum UploadError {
    Io(cosmix_mds::Error),
    Store(StoreError),
}

/// The blocking half of an upload: stream the body into mds staging
/// via `blob::put_reader`, then either record the upload (hash as
/// expected, or POST's server-hashed truth) or — for a PUT whose body
/// hashed to something else — undo what this request did.
fn stream_into_store(
    store: &Arc<Store>,
    rx: mpsc::Receiver<Frame>,
    expected: Option<BlobHash>,
    started: SystemTime,
    mime: String,
    name: Option<String>,
    owner: String,
) -> Result<UploadOutcome, UploadError> {
    let reader = ChannelReader {
        rx,
        chunk: axum::body::Bytes::new(),
        pos: 0,
        eof: false,
    };
    // put_reader hashes while staging and commits under the hash of the
    // bytes that actually arrived; on a read error it removes the
    // staged file and leaves no CAS entry.
    let (landed, size) =
        blob::put_reader(&store.blobs_root(), reader).map_err(UploadError::Io)?;

    if let Some(expected) = expected
        && expected != landed
    {
        // The landed bytes committed under their own (wrong) hash.
        // Remove that entry only when this request created it (the CAS
        // file's mtime falls inside the request — content-addressed
        // writes never refresh an existing file's mtime) and nothing
        // pins or describes it; a pre-existing entry is not ours to
        // delete.
        let path = blob::blob_path(&store.blobs_root(), &landed);
        let created_here = fs_modified(&path)
            .map(|modified| modified >= started)
            .unwrap_or(false);
        if created_here {
            let anonymous = store
                .stat(&landed)
                .map(|s| s.pins.is_empty() && s.first_put.is_none())
                .unwrap_or(true);
            if anonymous {
                let _ = std::fs::remove_file(&path);
            }
        }
        return Ok(UploadOutcome::Mismatch {
            expected,
            landed,
            size,
        });
    }

    let outcome = store
        .record_upload(&landed, size, &mime, name.as_deref(), &owner)
        .map_err(UploadError::Store)?;
    Ok(UploadOutcome::Committed(outcome))
}

fn fs_modified(path: &Path) -> io::Result<SystemTime> {
    std::fs::metadata(path).and_then(|md| md.modified())
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
