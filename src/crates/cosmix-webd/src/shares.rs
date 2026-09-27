//! Public-share management and bounded password work. Bus callers are mesh-trusted;
//! HTTP callers can create only paths beneath their session account's configured root.
use crate::{NodeState, VhostState, blob_lane, blob_reference, file_share, session};
use axum::{
    extract::{Extension, Json, Path, Query, State},
    http::{HeaderMap, StatusCode},
    response::{IntoResponse, Response},
};
use serde::Deserialize;
use serde_json::{Value, json};
use std::{
    collections::HashMap,
    net::IpAddr,
    sync::{Arc, Mutex, OnceLock},
    time::{Duration, Instant},
};
use tokio::sync::{OwnedSemaphorePermit, Semaphore};

/// Stamped from the accepted transport, never from forwarding headers.
#[derive(Clone, Copy)]
pub struct Peer {
    pub ip: IpAddr,
    pub tls: bool,
}

pub struct Runtime {
    crypto: Arc<Semaphore>,
    creation_crypto: Arc<Semaphore>,
    management: Arc<Semaphore>,
    downloads: Arc<Semaphore>,
    attempts: Mutex<Attempts>,
    lane: OnceLock<Result<Arc<blob_lane::Lane>, String>>,
}
impl Default for Runtime {
    fn default() -> Self {
        Self {
            crypto: Arc::new(Semaphore::new(4)),
            creation_crypto: Arc::new(Semaphore::new(1)),
            management: Arc::new(Semaphore::new(8)),
            downloads: Arc::new(Semaphore::new(8)),
            attempts: Mutex::new(Attempts::default()),
            lane: OnceLock::new(),
        }
    }
}
impl Runtime {
    pub fn lane(&self) -> Result<Arc<blob_lane::Lane>, String> {
        self.lane
            .get_or_init(|| blob_lane::Lane::new().map(Arc::new))
            .clone()
    }
    pub fn admit(&self) -> Result<OwnedSemaphorePermit, String> {
        self.management
            .clone()
            .try_acquire_owned()
            .map_err(|_| "busy: share management pool full (8)".into())
    }
    pub fn attempt(&self, token: &str, ip: IpAddr) -> bool {
        self.attempts
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .allow(token, ip, Instant::now())
    }
    pub async fn hash(&self, password: Option<String>) -> Result<Option<String>, String> {
        let Some(password) = password else {
            return Ok(None);
        };
        valid_password(&password)?;
        let permit = self
            .creation_crypto
            .clone()
            .try_acquire_owned()
            .map_err(|_| "busy: password creation worker full (1)")?;
        tokio::task::spawn_blocking(move || {
            let _permit = permit; // cancellation cannot free admission while bcrypt still runs
            bcrypt::hash(password, 12)
                .map(Some)
                .map_err(|_| "internal: password hashing failed".into())
        })
        .await
        .map_err(|_| "internal: password worker failed")?
    }
    pub async fn verify(&self, password: String, hash: String) -> Result<bool, String> {
        if valid_password(&password).is_err() || !bounded_hash(&hash) {
            return Ok(false);
        }
        let permit = self
            .crypto
            .clone()
            .try_acquire_owned()
            .map_err(|_| "busy: password workers full (4)")?;
        tokio::task::spawn_blocking(move || {
            let _permit = permit;
            bcrypt::verify(password, &hash).unwrap_or(false)
        })
        .await
        .map_err(|_| "internal: password worker failed".into())
    }
}

#[derive(Default)]
struct Attempts(HashMap<(String, IpAddr), (Instant, u8)>);
impl Attempts {
    fn allow(&mut self, token: &str, ip: IpAddr, now: Instant) -> bool {
        self.0
            .retain(|_, (start, _)| now.duration_since(*start) < Duration::from_secs(60));
        self.0.get(&(token.to_owned(), attempt_ip(ip))).is_none_or(|row| row.1 < 5)
    }
    fn failed(&mut self, token: &str, ip: IpAddr, now: Instant) {
        self.allow(token, ip, now); // expire stale entries before admission
        let key = (token.to_owned(), attempt_ip(ip));
        if !self.0.contains_key(&key) && self.0.len() >= 4096 {
            let oldest = self.0.iter().filter(|((t, _), _)| t == token)
                .min_by_key(|(_, (start, _))| *start)
                .or_else(|| self.0.iter().min_by_key(|(_, (start, _))| *start))
                .map(|(key, _)| key.clone());
            if let Some(oldest) = oldest { self.0.remove(&oldest); }
        }
        let row = self.0.entry(key).or_insert((now, 0));
        row.1 = row.1.saturating_add(1);
    }
}
fn attempt_ip(ip: IpAddr) -> IpAddr {
    match ip {
        IpAddr::V6(ip) => match ip.to_ipv4_mapped() {
            Some(ip) => IpAddr::V4(ip),
            None => IpAddr::V6(std::net::Ipv6Addr::from(u128::from(ip) & (u128::MAX << 64))),
        },
        ip => ip,
    }
}
fn valid_password(password: &str) -> Result<(), String> {
    if password.is_empty() || password.len() > 72 {
        Err("invalid_arguments: password must contain 1..72 UTF-8 bytes".into())
    } else {
        Ok(())
    }
}
fn bounded_hash(hash: &str) -> bool {
    let parts: Vec<_> = hash.split('$').collect();
    hash.len() == 60
        && hash.is_ascii()
        && parts.len() == 4
        && parts[0].is_empty()
        && matches!(parts[1], "2a" | "2b" | "2y")
        && parts[2].len() == 2
        && parts[2].parse::<u32>().is_ok_and(|c| (4..=14).contains(&c))
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Create {
    pub account: String,
    #[serde(default = "file_kind")]
    pub kind: String,
    pub rel_path: Option<String>,
    pub blob: Option<blob_reference::Reference>,
    pub name: Option<String>,
    pub password: Option<String>,
    pub expires: Option<i64>,
}
fn file_kind() -> String {
    "file".into()
}

pub async fn create(
    node: &NodeState,
    vhost: &VhostState,
    request: Create,
) -> Result<Value, String> {
    if request.kind != "file" || !file_share::valid_account(&request.account) {
        return Err(
            "invalid_arguments: valid account and kind=file required; dir/drop unsupported".into(),
        );
    }
    if request.expires.is_some_and(|e| e <= session::now_secs()) {
        return Err("invalid_arguments: expiry must be in the future".into());
    }
    if let Some(password) = &request.password {
        valid_password(password)?;
    }
    let db = vhost.db.as_ref().ok_or("not_found")?;
    let target = match (request.rel_path, request.blob) {
        (Some(rel_path), None) => {
            if request.name.is_some() {
                return Err("invalid_arguments: name is only supported for blob targets".into());
            }
            let root = node
                .share_roots
                .checked_get(&vhost.fqdn, &request.account, &node.vhosts.load())
                .ok_or("unauthorized")?
                .clone();
            let path = rel_path.clone();
            tokio::task::spawn_blocking(move || root.open_regular(&path))
                .await
                .map_err(|_| "internal: path worker failed")?
                .map_err(
                    |_| "invalid_arguments: source must be a regular file beneath the account root",
                )?;
            file_share::Target::Path { rel_path }
        }
        (None, Some(mut reference)) => {
            if let Some(name) = request.name {
                reference.name = Some(name);
            }
            reference.validate()?;
            file_share::Target::Blob { reference }
        }
        _ => return Err("invalid_arguments: exactly one of rel_path or blob is required".into()),
    };
    let hash = node.share_runtime.hash(request.password).await?;
    if let file_share::Target::Blob { reference } = &target {
        let client = blob_lane::local(&node.broker_handle)?;
        node.share_runtime
            .lane()?
            .pin_reference(
                &*client,
                reference,
                &blob_reference::owner("share", &vhost.fqdn),
            )
            .await?;
    }
    let db = db.lock().await;
    let token = file_share::create(
        &db,
        &vhost.fqdn,
        &request.account,
        "file",
        &target,
        hash.as_deref(),
        request.expires,
        session::now_secs(),
    )
    .map_err(catalogue_error)?;
    Ok(json!({"token": token, "url": format!("/s/{token}")}))
}

pub fn catalogue_error(error: file_share::Error) -> String {
    match error {
        file_share::Error::Database(_) => "internal: share catalogue unavailable".into(),
        other => other.to_string(),
    }
}

/// HTTP JSON omits account and blob entirely: deny_unknown_fields prevents
/// requests from smuggling either across the session/path boundary.
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PathCreate {
    rel_path: String,
    #[serde(default = "file_kind")]
    kind: String,
    password: Option<String>,
    expires: Option<i64>,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct List {
    pub after: Option<String>,
    pub limit: Option<usize>,
}

async fn account(
    node: &NodeState,
    vhost: &VhostState,
    headers: &HeaderMap,
    mutate: bool,
) -> Result<String, String> {
    let payload = session::cookie_value(headers, session::SESSION_COOKIE)
        .and_then(|c| node.session.unseal(&c, &vhost.fqdn, session::now_secs()))
        .ok_or("unauthorized")?;
    if payload.kind != "maild" || !file_share::valid_account(&payload.email) {
        return Err("unauthorized".into());
    }
    let db = vhost.db.as_ref().ok_or("not_found")?.lock().await;
    if payload.epoch != crate::query_session_epoch(&db, &payload.email) {
        return Err("unauthorized".into());
    }
    if mutate
        && (!crate::media::same_origin(headers, &vhost.fqdn)
            || payload.csrf.is_empty()
            || !headers
                .get("x-csrf-token")
                .and_then(|h| h.to_str().ok())
                .is_some_and(|t| session::csrf_eq(t, &payload.csrf)))
    {
        return Err("csrf: invalid CSRF token or origin".into());
    }
    Ok(payload.email)
}

pub fn error(reason: String) -> Response {
    let status = if reason == "not_found" || reason == "revoked" || reason == "expired" {
        StatusCode::NOT_FOUND
    } else if reason == "unauthorized" {
        StatusCode::UNAUTHORIZED
    } else if reason.starts_with("csrf:") {
        StatusCode::FORBIDDEN
    } else if reason.starts_with("invalid_arguments:") {
        StatusCode::BAD_REQUEST
    } else if reason.starts_with("busy:") || reason.starts_with("lane_unavailable:") {
        StatusCode::SERVICE_UNAVAILABLE
    } else if reason.starts_with("internal:") {
        StatusCode::INTERNAL_SERVER_ERROR
    } else {
        StatusCode::BAD_GATEWAY
    };
    (status, Json(json!({"error": reason}))).into_response()
}
pub async fn private_response(
    request: axum::extract::Request,
    next: axum::middleware::Next,
) -> Response {
    let mut response = next.run(request).await;
    response
        .headers_mut()
        .insert("cache-control", "private, no-store".parse().unwrap());
    response
}
pub async fn http_create(
    State(node): State<Arc<NodeState>>,
    Extension(vhost): Extension<Arc<VhostState>>,
    headers: HeaderMap,
    Json(form): Json<PathCreate>,
) -> Response {
    let result = async {
        let _permit = node.share_runtime.admit()?;
        let account = account(&node, &vhost, &headers, true).await?;
        create(
            &node,
            &vhost,
            Create {
                account,
                kind: form.kind,
                rel_path: Some(form.rel_path),
                blob: None,
                name: None,
                password: form.password,
                expires: form.expires,
            },
        )
        .await
    }
    .await;
    match result {
        Ok(value) => (StatusCode::CREATED, Json(value)).into_response(),
        Err(e) => error(e),
    }
}
pub async fn http_list(
    State(node): State<Arc<NodeState>>,
    Extension(vhost): Extension<Arc<VhostState>>,
    headers: HeaderMap,
    Query(query): Query<List>,
) -> Response {
    let result = async {
        let _permit = node.share_runtime.admit()?;
        let account = account(&node, &vhost, &headers, false).await?;
        list(
            &vhost,
            &account,
            query.after.as_deref(),
            query.limit.unwrap_or(100),
        )
        .await
    }
    .await;
    match result {
        Ok(value) => Json(value).into_response(),
        Err(e) => error(e),
    }
}
pub async fn list(
    vhost: &VhostState,
    account: &str,
    after: Option<&str>,
    limit: usize,
) -> Result<Value, String> {
    let db = vhost.db.as_ref().ok_or("not_found")?.lock().await;
    let rows = file_share::list(&db, &vhost.fqdn, account, after, limit).map_err(catalogue_error)?;
    Ok(json!(rows))
}
pub async fn revoke(vhost: &VhostState, account: &str, token: &str) -> Result<Value, String> {
    if !file_share::valid_account(account) || !file_share::valid_token(token) {
        return Err("invalid_arguments: invalid account or token".into());
    }
    let db = vhost.db.as_ref().ok_or("not_found")?.lock().await;
    let revoked = file_share::revoke(&db, &vhost.fqdn, account, token).map_err(catalogue_error)?;
    Ok(json!({"revoked": revoked}))
}
pub async fn http_revoke(
    State(node): State<Arc<NodeState>>,
    Extension(vhost): Extension<Arc<VhostState>>,
    Path(token): Path<String>,
    headers: HeaderMap,
) -> Response {
    let result = async {
        let _permit = node.share_runtime.admit()?;
        let account = account(&node, &vhost, &headers, true).await?;
        revoke(&vhost, &account, &token).await
    }
    .await;
    match result {
        Ok(value) => Json(value).into_response(),
        Err(e) => error(e),
    }
}

/// Every response on the token route, including errors/method refusals, bypasses caches.
pub async fn public_headers(
    request: axum::extract::Request,
    next: axum::middleware::Next,
) -> Response {
    let head = *request.method() == axum::http::Method::HEAD;
    let mut response = next.run(request).await;
    let headers = response.headers_mut();
    headers.insert("cache-control", "private, no-store".parse().unwrap());
    headers.insert("x-content-type-options", "nosniff".parse().unwrap());
    headers.insert("content-security-policy", "sandbox".parse().unwrap());
    if head {
        *response.body_mut() = axum::body::Body::empty();
    }
    response
}

fn basic_password(headers: &HeaderMap) -> Option<String> {
    use base64::Engine;
    let value = headers.get("authorization")?.to_str().ok()?;
    if value.len() > 512 {
        return None;
    }
    let (scheme, credentials) = value.split_once(' ')?;
    if !scheme.eq_ignore_ascii_case("basic") {
        return None;
    }
    let decoded = base64::engine::general_purpose::STANDARD
        .decode(credentials)
        .ok()?;
    let decoded = String::from_utf8(decoded).ok()?;
    let (_, password) = decoded.split_once(':')?;
    valid_password(password).ok()?;
    Some(password.into())
}
fn challenge() -> Response {
    let mut response = error("unauthorized".into());
    response
        .headers_mut()
        .insert("www-authenticate", "Basic realm=\"share\"".parse().unwrap());
    response
}
fn public_error(reason: String) -> Response {
    let token = reason.split_once(':').map(|(prefix, _)| format!("{prefix}:"));
    if let Some(token) = token {
        tracing::warn!(%reason, "public share request failed");
        error(token)
    } else {
        error(reason)
    }
}
fn attachment(name: &str) -> String {
    let name = name.rsplit(['/', '\\']).next().unwrap_or("download");
    let name: String = name.chars().filter(|c| !c.is_control()).take(200).collect();
    let mut out = String::from("attachment; filename*=UTF-8''");
    for byte in if name.is_empty() { "download" } else { &name }.bytes() {
        if byte.is_ascii_alphanumeric() || b"!#$&+-.^_`|~".contains(&byte) {
            out.push(char::from(byte));
        } else {
            out.push_str(&format!("%{byte:02X}"));
        }
    }
    out
}

pub async fn serve(
    State(node): State<Arc<NodeState>>,
    Extension(vhost): Extension<Arc<VhostState>>,
    Path(token): Path<String>,
    peer: Option<Extension<Peer>>,
    method: axum::http::Method,
    headers: HeaderMap,
) -> Response {
    use axum::http::Method;
    if method != Method::GET && method != Method::HEAD {
        return (StatusCode::METHOD_NOT_ALLOWED, [("allow", "GET, HEAD")]).into_response();
    }
    let Some(db) = &vhost.db else {
        return public_error("not_found".into());
    };
    let gate = {
        let db = db.lock().await;
        match file_share::resolve(&db, &vhost.fqdn, &token, session::now_secs()) {
            Ok(g) => g,
            Err(e) => return public_error(catalogue_error(e)),
        }
    };
    let Some(Extension(peer)) = peer else {
        return public_error("busy: peer context unavailable".into());
    };
    let permit = match node.share_runtime.downloads.clone().try_acquire_owned() {
        Ok(permit) => permit,
        Err(_) => return public_error("busy: public download pool full (8)".into()),
    };
    let verified = if let Some(hash) = gate.password_hash {
        if !peer.tls {
            return (
                StatusCode::FORBIDDEN,
                "unauthorized: password shares require HTTPS",
            )
                .into_response();
        }
        let Some(password) = basic_password(&headers) else {
            return challenge();
        };
        if !node.share_runtime.attempt(&token, peer.ip) {
            return (
                StatusCode::TOO_MANY_REQUESTS,
                [("retry-after", "60")],
                "unauthorized",
            )
                .into_response();
        }
        match node.share_runtime.verify(password, hash.clone()).await {
            Ok(true) => Some(hash),
            Ok(false) => {
                node.share_runtime.attempts.lock().unwrap_or_else(|e| e.into_inner())
                    .failed(&token, peer.ip, Instant::now());
                return challenge();
            },
            Err(e) => return public_error(e),
        }
    } else {
        None
    };
    // Refresh after asynchronous password work; old snapshots confer no authority.
    let target = {
        let db = db.lock().await;
        let gate = match file_share::resolve(&db, &vhost.fqdn, &token, session::now_secs()) {
            Ok(g) => g,
            Err(e) => return public_error(catalogue_error(e)),
        };
        match gate.authorize(verified.as_deref()) {
            Ok(t) => (gate.share.account.clone(), t.clone()),
            Err(_) => return challenge(),
        }
    };
    // We expose no validator contract. An unverifiable If-Range requires the
    // complete representation, not a potentially stale partial response.
    let range = if headers.contains_key("if-range") { None } else { headers.get("range").cloned() };
    if range.as_ref().is_some_and(|r| r.as_bytes().len() > 1024) {
        return public_error("invalid_arguments: Range too long".into());
    }
    let result = match target.1 {
        file_share::Target::Blob { reference } => {
            let name = reference.name.clone().unwrap_or_else(|| "download".into());
            async {
                let client = blob_lane::local(&node.broker_handle)?;
                let download = node
                    .share_runtime
                    .lane()?
                    .download(&*client, &reference, method.clone(), range)
                    .await?;
                Ok::<_, String>((download, name))
            }
            .await
        }
        file_share::Target::Path { rel_path } => match node.share_roots.checked_get(&vhost.fqdn, &target.0, &node.vhosts.load()) {
            Some(root) => path_download(root.clone(), &rel_path, &method, range.as_ref())
                .await
                .map(|d| (d, rel_path)),
            None => Err("not_found".into()),
        },
    };
    let (download, name) = match result {
        Ok(value) => value,
        Err(e) => return public_error(e),
    };
    let mut response = Response::new(
        if method == Method::HEAD || download.status == StatusCode::RANGE_NOT_SATISFIABLE {
            drop(permit);
            axum::body::Body::empty()
        } else {
            counted_body(download.body, permit, vhost, token)
        },
    );
    *response.status_mut() = download.status;
    *response.headers_mut() = download.headers;
    response
        .headers_mut()
        .insert("content-disposition", attachment(&name).parse().unwrap());
    response
}

async fn path_download(
    root: Arc<cosmix_files::rooted_read::ReadRoot>,
    path: &str,
    method: &axum::http::Method,
    range: Option<&axum::http::HeaderValue>,
) -> Result<blob_lane::Download, String> {
    use tokio::io::{AsyncReadExt, AsyncSeekExt};
    let path = path.to_owned();
    let (file, size) = tokio::task::spawn_blocking(move || {
        let file = root.open_regular(&path)?;
        let size = file.metadata()?.len();
        Ok::<_, std::io::Error>((file, size))
    })
    .await
    .map_err(|_| "internal: file worker failed")?
    .map_err(|_| "not_found")?;
    let extent = blob_lane::expected_range(range.and_then(|r| r.to_str().ok()), size);
    let (status, start, length, content_range) = match extent {
        blob_lane::Range::Full => (StatusCode::OK, 0, size, None),
        blob_lane::Range::Partial(start, length) => (
            StatusCode::PARTIAL_CONTENT,
            start,
            length,
            Some(format!("bytes {start}-{}/{size}", start + length - 1)),
        ),
        blob_lane::Range::Unsatisfiable => (
            StatusCode::RANGE_NOT_SATISFIABLE,
            0,
            0,
            Some(format!("bytes */{size}")),
        ),
    };
    let mut headers = HeaderMap::new();
    headers.insert("content-length", length.into());
    headers.insert("content-type", "application/octet-stream".parse().unwrap());
    headers.insert("accept-ranges", "bytes".parse().unwrap());
    if let Some(range) = content_range {
        headers.insert("content-range", range.parse().unwrap());
    }
    let body = if *method == axum::http::Method::HEAD || length == 0 {
        axum::body::Body::empty()
    } else {
        let mut file = tokio::fs::File::from_std(file);
        file.seek(std::io::SeekFrom::Start(start))
            .await
            .map_err(|_| "internal: file seek failed")?;
        axum::body::Body::from_stream(futures_util::stream::try_unfold(
            (file, length),
            |(mut file, remaining)| async move {
                if remaining == 0 {
                    return Ok(None);
                }
                let mut bytes = vec![0u8; remaining.min(64 * 1024) as usize];
                let n = tokio::time::timeout(Duration::from_secs(30), file.read(&mut bytes))
                    .await
                    .map_err(|_| std::io::Error::other("file read idle timeout"))??;
                if n == 0 {
                    return Err(std::io::Error::other("file shorter than declared length"));
                }
                bytes.truncate(n);
                Ok(Some((
                    axum::body::Bytes::from(bytes),
                    (file, remaining - n as u64),
                )))
            },
        ))
    };
    Ok(blob_lane::Download {
        status,
        headers,
        body,
    })
}

fn counted_body(
    body: axum::body::Body,
    permit: OwnedSemaphorePermit,
    vhost: Arc<VhostState>,
    token: String,
) -> axum::body::Body {
    counted_body_with_deadlines(body, permit, vhost, token, Duration::from_secs(30), Duration::from_secs(3600))
}

fn counted_body_with_deadlines(
    body: axum::body::Body,
    permit: OwnedSemaphorePermit,
    vhost: Arc<VhostState>,
    token: String,
    idle: Duration,
    lifetime: Duration,
) -> axum::body::Body {
    use futures_util::StreamExt;
    let (tx, rx) = tokio::sync::mpsc::channel(1);
    let aborted = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let flag = aborted.clone();
    // This task owns BOTH the public permit and the upstream body (and its lane
    // permit). Its timers run even when hyper never polls the response again.
    tokio::spawn(async move {
        let _permit = permit;
        let mut stream = body.into_data_stream();
        let transfer = async {
            loop {
                let step = async {
                    let Some(chunk) = stream.next().await else { return false; };
                    tx.send(chunk.map_err(std::io::Error::other)).await.is_ok()
                };
                match tokio::time::timeout(idle, step).await {
                    Ok(true) => (),
                    Ok(false) => return,
                    Err(_) => { flag.store(true, std::sync::atomic::Ordering::Release); return; }
                }
            }
        };
        if tokio::time::timeout(lifetime, transfer).await.is_err() {
            flag.store(true, std::sync::atomic::Ordering::Release);
        }
    });
    let state = (rx, aborted, vhost, token, false);
    axum::body::Body::from_stream(futures_util::stream::unfold(
        state,
        |(mut rx, aborted, vhost, token, mut counted)| async move {
            let chunk = rx.recv().await?;
            if aborted.load(std::sync::atomic::Ordering::Acquire) {
                rx.close();
                while rx.try_recv().is_ok() {}
                return Some((Err(std::io::Error::other("download deadline exceeded")), (rx, aborted, vhost, token, counted)));
            }
            if chunk.as_ref().is_ok_and(|b| !b.is_empty()) && !counted {
                counted = true;
                if let Some(db) = &vhost.db {
                    let _ = tokio::time::timeout(Duration::from_secs(5), async {
                        let db = db.lock().await;
                        if let Err(error) = file_share::bump_download(&db, &token) {
                            tracing::warn!(%error, "share download counter failed");
                        }
                    })
                    .await;
                }
            }
            Some((chunk, (rx, aborted, vhost, token, counted)))
        },
    ))
}

#[cfg(test)]
pub(crate) mod tests {
    #[tokio::test]
    async fn unverifiable_if_range_returns_full_representation() {
        use tower::ServiceExt;
        let (_tmp, node, vhost) = fixture().await;
        let token = path_token(&node, &vhost, None).await;
        for method in ["GET", "HEAD"] {
            let mut request = download_request(&token, method, Some("bytes=6-"), None, true);
            request.headers_mut().insert("if-range", "\"old\"".parse().unwrap());
            let response = crate::build_per_vhost_router(node.clone()).oneshot(request).await.unwrap();
            assert_eq!(response.status(), StatusCode::OK);
            assert_eq!(response.headers()["content-length"], "11");
            assert!(!response.headers().contains_key("content-range"));
            let bytes = axum::body::to_bytes(response.into_body(), 100).await.unwrap();
            assert_eq!(bytes.as_ref(), if method == "GET" { b"hello world".as_slice() } else { b"" });
        }
    }
    #[tokio::test]
    async fn public_lane_errors_do_not_disclose_upstream_details() {
        let response = public_error("lane: http://127.0.0.1:9999/blob secret upstream body".into());
        assert_eq!(response.status(), StatusCode::BAD_GATEWAY);
        let bytes = axum::body::to_bytes(response.into_body(), 1024).await.unwrap();
        assert_eq!(serde_json::from_slice::<Value>(&bytes).unwrap(), json!({"error":"lane:"}));
    }
    #[tokio::test]
    async fn stalled_reader_releases_public_and_upstream_permits() {
        let (_tmp, _node, vhost) = fixture().await;
        for (idle, lifetime) in [(20, 1000), (1000, 20)] {
            let public = Arc::new(Semaphore::new(1));
            let lane = Arc::new(Semaphore::new(1));
            let upstream_permit = lane.clone().acquire_owned().await.unwrap();
            let upstream = axum::body::Body::from_stream(futures_util::stream::unfold(upstream_permit, |permit| async move {
                Some((Ok::<_, std::io::Error>(axum::body::Bytes::from_static(b"x")), permit))
            }));
            let _unpolled = counted_body_with_deadlines(upstream, public.clone().acquire_owned().await.unwrap(),
                vhost.clone(), "unused".into(), Duration::from_millis(idle), Duration::from_millis(lifetime));
            tokio::time::timeout(Duration::from_secs(1), async {
                let _public = public.acquire().await.unwrap();
                let _lane = lane.acquire().await.unwrap();
            }).await.unwrap();
        }
    }
    use super::*;
    async fn path_token(node: &NodeState, vhost: &VhostState, password: Option<String>) -> String {
        create(
            node,
            vhost,
            Create {
                account: "user@example.test".into(),
                kind: "file".into(),
                rel_path: Some("file".into()),
                blob: None,
                name: None,
                password,
                expires: None,
            },
        )
        .await
        .unwrap()["token"]
            .as_str()
            .unwrap()
            .to_owned()
    }
    fn download_request(
        token: &str,
        method: &str,
        range: Option<&str>,
        auth: Option<&str>,
        tls: bool,
    ) -> axum::http::Request<axum::body::Body> {
        let mut request = axum::http::Request::builder()
            .method(method)
            .uri(format!("/s/{token}"))
            .header("host", "pim.example")
            .extension(Peer {
                ip: "192.0.2.1".parse().unwrap(),
                tls,
            });
        if let Some(range) = range {
            request = request.header("range", range);
        }
        if let Some(auth) = auth {
            request = request.header("authorization", auth);
        }
        request.body(axum::body::Body::empty()).unwrap()
    }
    #[tokio::test]
    async fn public_path_ranges_headers_and_download_start_counting() {
        use tower::ServiceExt;
        let (_tmp, node, vhost) = fixture().await;
        let token = path_token(&node, &vhost, None).await;
        let app = crate::build_per_vhost_router(node.clone());
        for (method, range, status, expected, content_range) in [
            ("HEAD", None, 200, "", None),
            ("GET", Some("bytes=6-"), 206, "world", Some("bytes 6-10/11")),
            ("GET", Some("bytes=11-"), 416, "", Some("bytes */11")),
            ("GET", None, 200, "hello world", None),
        ] {
            let response = app
                .clone()
                .oneshot(download_request(&token, method, range, None, true))
                .await
                .unwrap();
            assert_eq!(response.status().as_u16(), status);
            for (key, value) in [
                ("cache-control", "private, no-store"),
                ("x-content-type-options", "nosniff"),
                ("content-security-policy", "sandbox"),
                ("accept-ranges", "bytes"),
                ("content-disposition", "attachment; filename*=UTF-8''file"),
            ] {
                assert_eq!(response.headers()[key], value);
            }
            assert_eq!(
                response
                    .headers()
                    .get("content-range")
                    .map(|h| h.to_str().unwrap()),
                content_range
            );
            let bytes = axum::body::to_bytes(response.into_body(), 100)
                .await
                .unwrap();
            assert_eq!(bytes.as_ref(), expected.as_bytes());
        }
        let db = vhost.db.as_ref().unwrap().lock().await;
        assert_eq!(
            file_share::resolve(&db, &vhost.fqdn, &token, session::now_secs())
                .unwrap()
                .share
                .download_count,
            2
        );
        assert_eq!(node.share_runtime.downloads.available_permits(), 8);
    }
    #[tokio::test]
    async fn public_password_revocation_and_unknown_token_do_not_fall_through() {
        use tower::ServiceExt;
        let (_tmp, node, vhost) = fixture().await;
        let token = path_token(&node, &vhost, Some("secret".into())).await;
        let app = crate::build_per_vhost_router(node.clone());
        let plain = app
            .clone()
            .oneshot(download_request(&token, "GET", None, None, false))
            .await
            .unwrap();
        assert_eq!(plain.status(), StatusCode::FORBIDDEN);
        assert!(!plain.headers().contains_key("www-authenticate"));
        for _ in 0..5 {
            let response = app
                .clone()
                .oneshot(download_request(&token, "HEAD", None, None, true))
                .await
                .unwrap();
            assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
            assert_eq!(
                response.headers()["www-authenticate"],
                "Basic realm=\"share\""
            );
            assert!(
                axum::body::to_bytes(response.into_body(), 1024)
                    .await
                    .unwrap()
                    .is_empty()
            );
        }
        assert_eq!(
            app.clone()
                .oneshot(download_request(&token, "GET", None, None, true))
                .await
                .unwrap()
                .status(),
            StatusCode::TOO_MANY_REQUESTS
        );
        // A separate peer gets its own budget; Basic's username is ignored.
        let mut request =
            download_request(&token, "GET", None, Some("Basic dXNlcjpzZWNyZXQ="), true);
        request.extensions_mut().insert(Peer {
            ip: "192.0.2.2".parse().unwrap(),
            tls: true,
        });
        assert_eq!(
            app.clone().oneshot(request).await.unwrap().status(),
            StatusCode::OK
        );
        revoke(&vhost, "user@example.test", &token).await.unwrap();
        assert_eq!(
            app.clone()
                .oneshot(download_request(&token, "GET", None, None, true))
                .await
                .unwrap()
                .status(),
            StatusCode::NOT_FOUND
        );
        std::fs::create_dir_all(vhost.www_dir.join("s")).unwrap();
        std::fs::write(vhost.www_dir.join("s/unknown"), b"must not serve").unwrap();
        assert_eq!(
            app.oneshot(download_request("unknown", "GET", None, None, true))
                .await
                .unwrap()
                .status(),
            StatusCode::NOT_FOUND
        );
    }
    #[test]
    fn attachment_encodes_untrusted_unicode_and_separators() {
        assert_eq!(
            attachment("dir/é\".txt"),
            "attachment; filename*=UTF-8''%C3%A9%22.txt"
        );
        assert_eq!(attachment("\r\n"), "attachment; filename*=UTF-8''download");
    }
    pub(crate) async fn fixture() -> (tempfile::TempDir, Arc<NodeState>, Arc<VhostState>) {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::write(tmp.path().join("file"), b"hello world").unwrap();
        let mut node = crate::session_login_tests::synth_jmap_node(&tmp, "http://127.0.0.1:1");
        let cfg = cosmix_config::node::WebdSharesConfig {
            roots: [("user@example.test".into(), tmp.path().to_owned())].into(),
        };
        Arc::get_mut(&mut node).unwrap().share_roots = file_share::Roots::from_config(&cfg);
        let vhost = node.vhost_for_host("pim.example").unwrap();
        file_share::init_schema(&*vhost.db.as_ref().unwrap().lock().await).unwrap();
        (tmp, node, vhost)
    }
    pub(crate) fn cookie(node: &NodeState, kind: &str, epoch: i64) -> String {
        let now = session::now_secs();
        node.session
            .seal(&session::SessionPayload {
                vhost: "pim.example".into(),
                maild_token: "test".into(),
                email: "user@example.test".into(),
                iat: now,
                exp: now + 300,
                csrf: "test-csrf".into(),
                epoch,
                kind: kind.into(),
                customer_id: 0,
            })
            .unwrap()
    }
    #[tokio::test]
    async fn http_management_enforces_session_epoch_csrf_and_path_only() {
        use tower::ServiceExt;
        let (_tmp, node, _vhost) = fixture().await;
        let app = crate::build_per_vhost_router(node.clone());
        for (kind, epoch, csrf, expected) in [
            ("maild", 0, "", 403),
            ("customer", 0, "test-csrf", 401),
            ("maild", 1, "test-csrf", 401),
            ("maild", 0, "test-csrf", 201),
        ] {
            let request = axum::http::Request::builder()
                .method("POST")
                .uri("/api/shares")
                .header("host", "pim.example")
                .header("content-type", "application/json")
                .header(
                    "cookie",
                    format!("cosmix_session={}", cookie(&node, kind, epoch)),
                )
                .header("x-csrf-token", csrf)
                .body(axum::body::Body::from(r#"{"rel_path":"file"}"#))
                .unwrap();
            let response = app.clone().oneshot(request).await.unwrap();
            assert_eq!(response.status().as_u16(), expected);
            assert_eq!(response.headers()["cache-control"], "private, no-store");
        }
    }
    #[tokio::test]
    async fn bus_is_mesh_open_and_revoke_is_account_scoped() {
        let (_tmp, node, vhost) = fixture().await;
        let mut cmd = cosmix_client::IncomingCommand {
            from: "any-mesh-peer".into(),
            command: "webd.share.create".into(),
            id: None,
            args: json!({"vhost":"pim.example", "account":"user@example.test", "rel_path":"file"}),
            body: String::new(),
            headers: Default::default(),
        };
        let (rc, body) = crate::bus::share_verbs::dispatch(&node, &cmd).await;
        assert_eq!(rc, 0, "{body}");
        let value: Value = serde_json::from_str(&body).unwrap();
        let token = value["token"].as_str().unwrap();
        assert_eq!(
            revoke(&vhost, "other@example.test", token).await.unwrap()["revoked"],
            false
        );
        assert_eq!(
            revoke(&vhost, "user@example.test", token).await.unwrap()["revoked"],
            true
        );
        cmd.args["kind"] = json!("dir");
        let (rc, body) = crate::bus::share_verbs::dispatch(&node, &cmd).await;
        assert_eq!(rc, 10);
        assert!(body.contains("invalid_arguments:"));
    }
    #[test]
    fn attempts_count_failures_group_ipv6_and_admit_at_capacity() {
        let mut attempts = Attempts::default();
        let now = Instant::now();
        let ip = "192.0.2.1".parse().unwrap();
        for _ in 0..5 {
            assert!(attempts.allow("a", ip, now));
            attempts.failed("a", ip, now);
        }
        assert!(!attempts.allow("a", ip, now));
        assert!(attempts.allow("a", "192.0.2.2".parse().unwrap(), now));
        for n in 0..4095 {
            assert!(attempts.allow(&format!("t{n}"), ip, now));
            attempts.failed(&format!("t{n}"), ip, now);
        }
        assert_eq!(attempts.0.len(), 4096);
        assert!(attempts.allow("a", "192.0.2.2".parse().unwrap(), now));
        attempts.failed("a", "192.0.2.2".parse().unwrap(), now);
        assert_eq!(attempts.0.len(), 4096);
        assert!(!attempts.0.contains_key(&("a".into(), ip)));
        assert!(attempts.allow("new", ip, now));
        assert!(attempts.allow("a", ip, now + Duration::from_secs(60)));
        let v6 = "2001:db8:1:2::1".parse().unwrap();
        for _ in 0..5 { attempts.failed("v6", v6, now); }
        assert!(!attempts.allow("v6", "2001:db8:1:2::abcd".parse().unwrap(), now));
        assert!(attempts.allow("v6", "2001:db8:1:3::1".parse().unwrap(), now));
    }
    #[test]
    fn http_rejects_blob_account_and_unknown_fields() {
        for field in ["account", "blob", "root", "vhost"] {
            let mut value = json!({"rel_path": "file"});
            value[field] = json!("forged");
            assert!(serde_json::from_value::<PathCreate>(value).is_err());
        }
        assert!(valid_password(&"x".repeat(73)).is_err());
        assert!(valid_password("").is_err());
        assert!(!bounded_hash(&format!("$2b$31${}", "a".repeat(53))));
    }
    #[tokio::test]
    async fn bcrypt_roundtrip_and_cost_bound() {
        let runtime = Runtime::default();
        let hash = runtime.hash(Some("secret".into())).await.unwrap().unwrap();
        assert!(runtime.verify("secret".into(), hash.clone()).await.unwrap());
        assert!(!runtime.verify("wrong".into(), hash).await.unwrap());
        assert!(
            !runtime
                .verify("secret".into(), format!("$2b$31${}", "a".repeat(53)))
                .await
                .unwrap()
        );
    }
}
