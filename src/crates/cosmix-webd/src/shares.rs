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
    management: Arc<Semaphore>,
    attempts: Mutex<Attempts>,
    lane: OnceLock<Result<Arc<blob_lane::Lane>, String>>,
}
impl Default for Runtime {
    fn default() -> Self {
        Self {
            crypto: Arc::new(Semaphore::new(4)),
            management: Arc::new(Semaphore::new(8)),
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
            .crypto
            .clone()
            .try_acquire_owned()
            .map_err(|_| "busy: password workers full (4)")?;
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
        let key = (token.to_owned(), ip);
        if !self.0.contains_key(&key) && self.0.len() >= 4096 {
            return false;
        }
        let row = self.0.entry(key).or_insert((now, 0));
        if row.1 >= 5 {
            return false;
        }
        row.1 += 1;
        true
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
                .get(&request.account)
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
    let rows = file_share::list(&db, account, after, limit).map_err(catalogue_error)?;
    Ok(json!({"shares": rows, "next": rows.last().map(|r| &r.token)}))
}
pub async fn revoke(vhost: &VhostState, account: &str, token: &str) -> Result<Value, String> {
    if !file_share::valid_account(account) || !file_share::valid_token(token) {
        return Err("invalid_arguments: invalid account or token".into());
    }
    let db = vhost.db.as_ref().ok_or("not_found")?.lock().await;
    let revoked = file_share::revoke(&db, account, token).map_err(catalogue_error)?;
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

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    pub(crate) async fn fixture() -> (tempfile::TempDir, Arc<NodeState>, Arc<VhostState>) {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::write(tmp.path().join("file"), b"hello world").unwrap();
        let mut node = crate::session_login_tests::synth_jmap_node(&tmp, "http://127.0.0.1:1");
        let cfg = cosmix_config::node::WebdSharesConfig {
            roots: [("user@example.test".into(), tmp.path().to_owned())].into(),
        };
        Arc::get_mut(&mut node).unwrap().share_roots = file_share::Roots::from_config(&cfg);
        let vhost = node.vhost_for_host("pim.example").unwrap();
        file_share::init_schema(&vhost.db.as_ref().unwrap().lock().await).unwrap();
        (tmp, node, vhost)
    }
    fn cookie(node: &NodeState, kind: &str, epoch: i64) -> String {
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
    fn attempts_are_pair_scoped_expire_and_fail_closed_at_capacity() {
        let mut attempts = Attempts::default();
        let now = Instant::now();
        let ip = "192.0.2.1".parse().unwrap();
        for _ in 0..5 {
            assert!(attempts.allow("a", ip, now));
        }
        assert!(!attempts.allow("a", ip, now));
        assert!(attempts.allow("a", "192.0.2.2".parse().unwrap(), now));
        for n in 0..4094 {
            assert!(attempts.allow(&format!("t{n}"), ip, now));
        }
        assert!(!attempts.allow("new", ip, now));
        assert!(attempts.allow("a", ip, now + Duration::from_secs(60)));
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
