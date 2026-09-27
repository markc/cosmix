//! Filesystem-backed image storage for the CMS `media` library.
//!
//! CMS handler `.mix` scripts run in a Pure+FsRead Mix sandbox
//! (`FsWrite` denied — see `mix_handler::HandlerCapabilityPolicy`), so a
//! handler cannot write an uploaded image to disk. The byte-write is done
//! here in webd (Rust). This is also the substrate-first split: per-request
//! file writes with a path jail, magic-byte MIME validation, and a size cap
//! are per-request security machinery → Rust, not operator-authored Mix
//! policy. See `_decisions/substrate-first-service-pattern.md`.
//!
//! Two native routes, matched ahead of the `serve_static` fallback (so they
//! win over the `/admin/media` Mix gallery handler, which they don't collide
//! with anyway):
//!   * `POST /admin/media/upload` — auth-gated. Reads the client's base64
//!     image (the existing urlencoded `data` field — no axum `multipart`
//!     feature needed), validates by magic bytes (NOT the client mime),
//!     writes `<www_dir>/img/<YYYY>/<MM>/<blake3>.<ext>`, and records a
//!     `media` row (`storage='disk'`, `url_path`). The bytes are then served
//!     by `ServeDir` — fast, cached, range-capable, zero decode.
//!   * `POST /admin/media/delete` — auth-gated. Deletes the row and, for a
//!     disk row with no other row referencing the same `url_path`, unlinks
//!     the file (refcount by physical path, so an identical image uploaded in
//!     a different month — a different file — is never orphaned).
//!
//! Auth is the **unified maild session**: unseal the `cosmix_session` cookie
//! for the email, then require an `author`+ role in the per-vhost cms.db
//! `users` table (`cms_author`) — the same identity+role model the Mix
//! handlers use via `$SESSION`. (The old `sid`/`sessions` CMS login is
//! retired.)

use std::path::{Path, PathBuf};
use std::sync::Arc;

use axum::extract::{Extension, Form, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use base64::Engine;
use rusqlite::{Connection, OptionalExtension};
use serde::Deserialize;

use crate::{NodeState, VhostState, session};

/// Max decoded image size: 10 MiB. The urlencoded base64 body is ~4/3 of
/// this, so the route layers a matching [`UPLOAD_BODY_LIMIT`].
const MAX_IMAGE_BYTES: usize = 10 * 1024 * 1024;

/// Per-route request-body cap for the upload (16 MiB ≈ 10 MiB image once
/// base64-inflated, with headroom). axum's default 2 MiB limit would
/// otherwise reject any non-trivial image.
pub(crate) const UPLOAD_BODY_LIMIT: usize = 16 * 1024 * 1024;

#[derive(Deserialize)]
pub(crate) struct UploadForm {
    /// Standard-alphabet base64 of the raw image bytes (the client
    /// FileReader data-URL payload after the comma). The client also sends
    /// `action`/`mime`; serde ignores unknown fields, and we sniff the mime
    /// from the bytes rather than trusting the declared one.
    data: String,
    /// Client-declared filename — kept only as a display label, never used
    /// in the on-disk path.
    #[serde(default)]
    filename: String,
}

/// Authorise a media mutation via the UNIFIED maild session + CMS role
/// (the login-unification slice — NOT the retired `sid`/`sessions` auth).
/// Unseals the `cosmix_session` cookie for the email, reads the role from the
/// per-vhost `users` table (`conn`), and returns true for `author`+ (the role
/// allowed to manage media). Sync — the caller already holds the db lock.
fn cms_author(
    sealer: &session::SessionSealer,
    vhost_fqdn: &str,
    conn: &Connection,
    headers: &HeaderMap,
) -> bool {
    let payload = match session::cookie_value(headers, session::SESSION_COOKIE)
        .and_then(|c| sealer.unseal(&c, vhost_fqdn, session::now_secs()))
    {
        Some(p) => p,
        None => return false,
    };
    // A billing-portal (kind="customer") session has NO CMS authority even on an
    // email that collides with an author/admin users row — admin authority is
    // maild-account only (Codex BLOCKER; mirrors cms_session_role).
    if payload.kind != "maild" {
        return false;
    }
    // Cookie-path revocation: a payload sealed under an older epoch is DEAD
    // regardless of TTL — same gate cms_session_role() applies, so a revoked
    // admin cookie can't keep mutating media (Codex MAJOR).
    if payload.epoch != crate::query_session_epoch(conn, &payload.email) {
        return false;
    }
    let role: String = conn
        .query_row(
            "SELECT role FROM users WHERE username = ?1",
            rusqlite::params![payload.email],
            |r| r.get(0),
        )
        .ok()
        .unwrap_or_else(|| "user".to_string());
    matches!(role.as_str(), "admin" | "author")
}

/// Split an `Origin`/`Referer` value `scheme://host[:port][/path]` into
/// `(scheme, host, port?)`. Port is carried (NOT stripped) because cookies are
/// host-scoped, not port-scoped — a hostile HTTPS service on another port of the
/// same host must be rejected, matching `main.rs::bus_parse_origin`.
fn origin_scheme_host(v: &str) -> Option<(&str, &str, Option<&str>)> {
    let (scheme, rest) = v.split_once("://")?;
    let authority = rest.split('/').next()?; // host[:port]
    let (host, port) = match authority.split_once(':') {
        Some((h, p)) => (h, Some(p)),
        None => (authority, None),
    };
    if scheme.is_empty() || host.is_empty() {
        None
    } else {
        Some((scheme, host, port))
    }
}

/// CSRF guard for the state-changing media routes. The CMS `sid` cookie is
/// `SameSite=Lax` (lib.mix `session_cookie`), which does NOT block a same-SITE
/// cross-ORIGIN POST (a sibling subdomain under the same registrable domain),
/// so this same-origin check is the real CSRF defence, not mere belt-and-braces.
/// A present `Origin`/`Referer` must be `https://<vhost fqdn>` on the default
/// port (absent or `443`) — scheme + host + port-aware, so a same-host different
/// -port HTTPS service is rejected; a present-but-mismatched or unparseable one
/// is rejected. BOTH absent is **allowed**: under Lax the sole CSRF vector is a
/// same-site cross-origin browser POST, which ALWAYS emits `Origin`, so a
/// header-less POST is a non-browser client, not an attack (full rationale on
/// `main.rs::handler_origin_is_cross_site`; corrected 2026-07-13 after the
/// fail-closed rule regressed cookieless workers fleet-wide). This mirrors the
/// general handler gate; the media routes run their own admin-session auth after
/// this check, so a header-less request still faces real authorization.
pub(crate) fn same_origin(headers: &HeaderMap, fqdn: &str) -> bool {
    for h in [axum::http::header::ORIGIN, axum::http::header::REFERER] {
        // A PRESENT header (even non-UTF-8) is authoritative: garbage bytes can't
        // be a legitimate same-origin request, so present-but-unparseable →
        // reject (`is_some_and` is false), never treated as "absent".
        if let Some(val) = headers.get(h) {
            return val.to_str().ok().and_then(origin_scheme_host).is_some_and(
                |(scheme, host, port)| {
                    scheme.eq_ignore_ascii_case("https")
                        && host.eq_ignore_ascii_case(fqdn)
                        && matches!(port, None | Some("443"))
                },
            );
        }
    }
    // Both headers absent → allow (not a browser cross-origin POST).
    true
}

/// 303 redirect with an empty body.
fn redirect(loc: &str) -> Response {
    Response::builder()
        .status(StatusCode::SEE_OTHER)
        .header(axum::http::header::LOCATION, loc)
        .body(axum::body::Body::empty())
        .map(IntoResponse::into_response)
        .unwrap_or_else(|_| StatusCode::INTERNAL_SERVER_ERROR.into_response())
}

/// Sniff a raster image by its magic bytes and return `(canonical mime,
/// extension)`. We trust the bytes, not the client-declared type. SVG is
/// deliberately unsupported (it can carry script).
fn sniff_image(b: &[u8]) -> Option<(&'static str, &'static str)> {
    if b.len() >= 8 && b[..8] == [0x89, 0x50, 0x4E, 0x47, 0x0D, 0x0A, 0x1A, 0x0A] {
        return Some(("image/png", "png"));
    }
    if b.len() >= 3 && b[..3] == [0xFF, 0xD8, 0xFF] {
        return Some(("image/jpeg", "jpg"));
    }
    if b.len() >= 6 && (&b[..6] == b"GIF87a" || &b[..6] == b"GIF89a") {
        return Some(("image/gif", "gif"));
    }
    // WebP container: "RIFF" <u32 size> "WEBP".
    if b.len() >= 12 && &b[..4] == b"RIFF" && &b[8..12] == b"WEBP" {
        return Some(("image/webp", "webp"));
    }
    None
}

/// On-disk path + public URL for a content-hashed image. The filename is
/// `<hash>.<ext>` (hex + a fixed extension — no client input), so traversal
/// is impossible by construction; date-sharding keeps any one directory
/// from holding years of a busy blog's images.
fn image_paths(www_dir: &Path, hash: &str, ext: &str) -> (PathBuf, String) {
    let now = time::OffsetDateTime::now_utc();
    let rel = format!(
        "img/{:04}/{:02}/{hash}.{ext}",
        now.year(),
        now.month() as u8
    );
    (www_dir.join(&rel), format!("/{rel}"))
}

/// Reconstruct (and jail) the on-disk path of a stored `url_path`. The value
/// is webd-authored, but we still require the `img/` prefix and reject `..`
/// so a delete can never reach outside the image tree.
fn safe_disk_path(www_dir: &Path, url_path: &str) -> Option<PathBuf> {
    let rel = url_path.strip_prefix('/').unwrap_or(url_path);
    if !rel.starts_with("img/") || rel.contains("..") {
        return None;
    }
    Some(www_dir.join(rel))
}

/// Atomic write: temp file in the same directory, then rename, so a reader
/// never sees a partially-written image.
fn write_atomic(path: &Path, bytes: &[u8]) -> std::io::Result<()> {
    use std::io::Write;
    let parent = path
        .parent()
        .ok_or_else(|| std::io::Error::other("image path has no parent"))?;
    std::fs::create_dir_all(parent)?;
    let mut tmp = tempfile::NamedTempFile::new_in(parent)?;
    tmp.write_all(bytes)?;
    tmp.flush()?;
    tmp.persist(path).map(|_| ()).map_err(|e| e.error)
}

/// Display-label hygiene for the original filename (Mix `esc()`s it on
/// render, so this is just length + control-char trimming).
fn sanitize_label(s: &str) -> String {
    let cleaned: String = s
        .trim()
        .chars()
        .filter(|c| !c.is_control())
        .take(200)
        .collect();
    if cleaned.is_empty() {
        "upload".to_string()
    } else {
        cleaned
    }
}

/// Both this and cms_init preserve legacy rows and distinguish existing columns
/// from genuine schema failures. The per-vhost DB mutex serialises each Rust call.
pub(crate) fn ensure_media_schema(conn: &Connection) -> rusqlite::Result<()> {
    conn.execute_batch(
        "CREATE TABLE IF NOT EXISTS media (id INTEGER PRIMARY KEY AUTOINCREMENT,
        filename TEXT NOT NULL, mime TEXT NOT NULL, data TEXT NOT NULL DEFAULT '',
        bytes INTEGER NOT NULL DEFAULT 0, created TEXT NOT NULL DEFAULT (datetime('now')))",
    )?;
    for (name, definition) in [
        ("storage", "storage TEXT NOT NULL DEFAULT 'inline'"),
        ("url_path", "url_path TEXT NOT NULL DEFAULT ''"),
        ("hash", "hash TEXT NOT NULL DEFAULT ''"),
        ("blob", "blob TEXT NULL"),
    ] {
        let exists = |conn: &Connection| {
            conn.query_row(
                "SELECT EXISTS(SELECT 1 FROM pragma_table_info('media') WHERE name=?1)",
                [name],
                |r| r.get::<_, bool>(0),
            )
        };
        if !exists(conn)?
            && let Err(error) =
                conn.execute_batch(&format!("ALTER TABLE media ADD COLUMN {definition}"))
            && !exists(conn)?
        {
            return Err(error);
        }
    }
    conn.prepare(
        "SELECT id, filename, mime, bytes, storage, url_path, hash, blob FROM media LIMIT 0",
    )?;
    Ok(())
}

type KeyLocks<K> =
    std::sync::Mutex<std::collections::HashMap<K, std::sync::Weak<tokio::sync::Mutex<()>>>>;
pub struct Runtime {
    paths: KeyLocks<PathBuf>,
    rows: KeyLocks<(String, i64)>,
    writes: Arc<tokio::sync::Semaphore>,
    refs: Arc<tokio::sync::Semaphore>,
}
impl Default for Runtime {
    fn default() -> Self {
        Self {
            paths: Default::default(),
            rows: Default::default(),
            writes: Arc::new(tokio::sync::Semaphore::new(8)),
            refs: Arc::new(tokio::sync::Semaphore::new(8)),
        }
    }
}
fn slot<K: Eq + std::hash::Hash>(map: &KeyLocks<K>, key: K) -> Arc<tokio::sync::Mutex<()>> {
    let mut map = map.lock().unwrap_or_else(|e| e.into_inner());
    map.retain(|_, value| value.strong_count() > 0);
    if let Some(lock) = map.get(&key).and_then(std::sync::Weak::upgrade) {
        return lock;
    }
    let lock = Arc::new(tokio::sync::Mutex::new(()));
    map.insert(key, Arc::downgrade(&lock));
    lock
}
/// Canonicalise the nearest existing parent, preserving missing date directories.
/// Different vhost roots/aliases for the same physical image share one lock.
fn physical_key(path: &Path) -> PathBuf {
    let mut parent = path.parent().unwrap_or(path);
    let mut missing = Vec::new();
    let mut root = loop {
        if let Ok(real) = parent.canonicalize() {
            break real;
        }
        let Some(name) = parent.file_name() else {
            return path.to_owned();
        };
        missing.push(name.to_owned());
        let Some(next) = parent.parent() else {
            return path.to_owned();
        };
        parent = next;
    };
    for name in missing.into_iter().rev() {
        root.push(name);
    }
    if let Some(name) = path.file_name() {
        root.push(name);
    }
    root
}
impl Runtime {
    fn path(&self, path: &Path) -> Arc<tokio::sync::Mutex<()>> {
        slot(&self.paths, physical_key(path))
    }
    fn write_admit(&self) -> Result<tokio::sync::OwnedSemaphorePermit, String> {
        self.writes
            .clone()
            .try_acquire_owned()
            .map_err(|_| "busy: media write pool full (8)".into())
    }
}

#[derive(Debug, PartialEq, Eq)]
struct MediaRow {
    filename: String,
    mime: String,
    bytes: i64,
    storage: String,
    url_path: String,
    hash: String,
    blob: Option<String>,
}
fn row(conn: &Connection, id: i64) -> Result<MediaRow, String> {
    conn.query_row(
        "SELECT filename,mime,bytes,storage,url_path,hash,blob FROM media WHERE id=?1",
        [id],
        |r| {
            Ok(MediaRow {
                filename: r.get(0)?,
                mime: r.get(1)?,
                bytes: r.get(2)?,
                storage: r.get(3)?,
                url_path: r.get(4)?,
                hash: r.get(5)?,
                blob: r.get(6)?,
            })
        },
    )
    .optional()
    .map_err(|_| "internal: media catalogue read failed")?
    .ok_or_else(|| "not_found".into())
}

/// Retry from the served file, coalesced per primary-vhost/media-id. No lane call
/// holds the DB lock. A concurrent delete/replacement prevents reference attachment.
pub(crate) async fn media_ref(
    node: &NodeState,
    vhost: &VhostState,
    id: i64,
) -> Result<crate::blob_reference::Reference, String> {
    if id <= 0 {
        return Err("invalid_arguments: positive media id required".into());
    }
    let admission = Arc::new(
        node.media_runtime
            .refs
            .clone()
            .try_acquire_owned()
            .map_err(|_| "busy: media reference pool full (8)")?,
    );
    let lock = slot(&node.media_runtime.rows, (vhost.fqdn.clone(), id));
    let _row_guard = lock.lock().await;
    let db = vhost.db.as_ref().ok_or("not_found")?;
    let source = {
        let db = db.lock().await;
        ensure_media_schema(&db).map_err(|_| "internal: media schema migration failed")?;
        row(&db, id)?
    };
    if source.storage != "disk" {
        return Err("unsupported_storage: only disk media can produce a blob reference".into());
    }
    if let Some(blob) = &source.blob {
        let value = serde_json::from_str(blob)
            .map_err(|_| "verify_failed: invalid stored media reference JSON")?;
        return crate::blob_reference::Reference::from_json(&value)
            .map_err(|_| "verify_failed: invalid stored media reference".into());
    }
    let path = safe_disk_path(&vhost.www_dir, &source.url_path)
        .ok_or("verify_failed: unsafe media path")?;
    let file_lock = node.media_runtime.path(&path);
    let file_guard = Arc::new(file_lock.lock_owned().await);
    let root = vhost.www_dir.clone();
    let relative = source.url_path.trim_start_matches('/').to_owned();
    let read_guard = file_guard.clone();
    let read_admission = admission.clone();
    let bytes = tokio::task::spawn_blocking(move || {
        use std::io::Read;
        let (_guard, _admission) = (read_guard, read_admission);
        let root = cosmix_files::rooted_read::ReadRoot::open(
            &root.canonicalize().map_err(|_| "not_found")?,
        )
        .map_err(|_| "not_found")?;
        let file = root.open_regular(&relative).map_err(|_| "not_found")?;
        if file.metadata().map_err(|_| "not_found")?.len() > MAX_IMAGE_BYTES as u64 {
            return Err("too_large: media exceeds 10 MiB".to_string());
        }
        let mut bytes = Vec::new();
        file.take((MAX_IMAGE_BYTES + 1) as u64)
            .read_to_end(&mut bytes)
            .map_err(|_| "internal: media read failed")?;
        if bytes.len() > MAX_IMAGE_BYTES {
            return Err("too_large: media exceeds 10 MiB".to_string());
        }
        Ok(bytes)
    })
    .await
    .map_err(|_| "internal: media read worker failed")??;
    drop(file_guard);
    let hash = blake3::hash(&bytes).to_hex();
    if source.bytes != bytes.len() as i64 || source.hash != hash[..32] {
        return Err("verify_failed: served file differs from media row".into());
    }
    let client = crate::blob_lane::local(&node.broker_handle)?;
    let reference = node
        .share_runtime
        .lane()?
        .reference(
            &*client,
            &crate::blob_reference::owner("media", &vhost.fqdn),
            bytes,
            &source.mime,
            Some(&source.filename),
        )
        .await?;
    let _guard = node.media_runtime.path(&path).lock_owned().await;
    let db = db.lock().await;
    attach_reference(&db, id, &source, &reference)?;
    Ok(reference)
}
fn attach_reference(
    db: &Connection,
    id: i64,
    source: &MediaRow,
    reference: &crate::blob_reference::Reference,
) -> Result<(), String> {
    reference.validate()?;
    if row(db, id)? != *source {
        return Err("conflict: media row changed during blob write".into());
    }
    let blob =
        serde_json::to_string(reference).map_err(|_| "internal: reference serialisation failed")?;
    db.execute(
        "UPDATE media SET blob=?1 WHERE id=?2 AND blob IS NULL",
        rusqlite::params![blob, id],
    )
    .map_err(|_| "internal: media reference attach failed")?;
    Ok(())
}

pub(crate) async fn media_upload(
    State(node): State<Arc<NodeState>>,
    Extension(vhost): Extension<Arc<VhostState>>,
    headers: HeaderMap,
    Form(form): Form<UploadForm>,
) -> Response {
    let Some(db_mtx) = &vhost.db else {
        return redirect("/admin/media?err=noconfig");
    };
    if !same_origin(&headers, &vhost.fqdn) {
        return redirect("/admin/media?err=csrf");
    }
    let Ok(admission) = node.media_runtime.write_admit() else {
        return redirect("/admin/media?err=busy");
    };
    let admission = Arc::new(admission);
    {
        let db = db_mtx.lock().await;
        if !cms_author(&node.session, &vhost.fqdn, &db, &headers) {
            return redirect("/auth/login");
        }
        if ensure_media_schema(&db).is_err() {
            return redirect("/admin/media?err=dberr");
        }
    }
    let raw = match base64::engine::general_purpose::STANDARD.decode(form.data.trim()) {
        Ok(b) => b,
        Err(_) => return redirect("/admin/media?err=baddata"),
    };
    if raw.is_empty() {
        return redirect("/admin/media?err=baddata");
    }
    if raw.len() > MAX_IMAGE_BYTES {
        return redirect("/admin/media?err=toobig");
    }
    let Some((mime, ext)) = sniff_image(&raw) else {
        return redirect("/admin/media?err=badtype");
    };
    let hash_full = blake3::hash(&raw).to_hex();
    let hash = &hash_full[..32];
    let (disk_path, url_path) = image_paths(&vhost.www_dir, hash, ext);
    let label = sanitize_label(&form.filename);
    let bytes_len = raw.len() as i64;
    let guard = Arc::new(node.media_runtime.path(&disk_path).lock_owned().await);
    let (write_guard, write_admission, write_path) =
        (guard.clone(), admission.clone(), disk_path.clone());
    let written = tokio::task::spawn_blocking(move || {
        let (_guard, _admission) = (write_guard, write_admission);
        write_atomic(&write_path, &raw)
    })
    .await;
    if !matches!(written, Ok(Ok(()))) {
        return redirect("/admin/media?err=writeerr");
    }
    let inserted = {
        let db = db_mtx.lock().await;
        db.execute("INSERT INTO media (filename,mime,data,bytes,storage,url_path,hash) VALUES (?1,?2,'',?3,'disk',?4,?5)", rusqlite::params![label,mime,bytes_len,url_path,hash])
            .map(|_| db.last_insert_rowid())
    };
    let id = match inserted {
        Ok(id) => id,
        Err(_) => {
            if !path_in_use(&node, &disk_path).await {
                let _ = std::fs::remove_file(&disk_path);
            }
            return redirect("/admin/media?err=dberr");
        }
    };
    drop(guard);
    drop(admission);
    // Durable row/file first. Optional blob failure is explicitly non-fatal.
    if let Err(reason) = media_ref(&node, &vhost, id).await {
        tracing::warn!(%reason, id, vhost=%vhost.fqdn, "media saved without blob reference; retry webd.media.ref");
    }
    redirect("/admin/media")
}

pub(crate) async fn media_delete(
    State(node): State<Arc<NodeState>>,
    Extension(vhost): Extension<Arc<VhostState>>,
    headers: HeaderMap,
    Form(form): Form<std::collections::HashMap<String, String>>,
) -> Response {
    let Some(db_mtx) = &vhost.db else {
        return redirect("/admin/media");
    };
    if !same_origin(&headers, &vhost.fqdn) {
        return redirect("/admin/media?err=csrf");
    }
    let Ok(_admission) = node.media_runtime.write_admit() else {
        return redirect("/admin/media?err=busy");
    };
    {
        let db = db_mtx.lock().await;
        if !cms_author(&node.session, &vhost.fqdn, &db, &headers) {
            return redirect("/auth/login");
        }
        if ensure_media_schema(&db).is_err() {
            return redirect("/admin/media?err=dberr");
        }
    }
    let mut ids = Vec::new();
    if let Some(id) = form.get("id").and_then(|s| s.parse::<i64>().ok()) {
        ids.push(id);
    }
    for (key, value) in form {
        if value == "1"
            && let Some(id) = key.strip_prefix("sel_").and_then(|s| s.parse::<i64>().ok())
        {
            ids.push(id);
        }
    }
    ids.sort_unstable();
    ids.dedup();
    ids.truncate(1000);
    for id in ids {
        let source = {
            let db = db_mtx.lock().await;
            row(&db, id)
        };
        let source = match source {
            Ok(source) => source,
            Err(e) if e == "not_found" => continue,
            Err(_) => return redirect("/admin/media?err=dberr"),
        };
        let path = (source.storage == "disk")
            .then(|| safe_disk_path(&vhost.www_dir, &source.url_path))
            .flatten();
        let _guard = match &path {
            Some(path) => Some(node.media_runtime.path(path).lock_owned().await),
            None => None,
        };
        let db = db_mtx.lock().await;
        let current = match row(&db, id) {
            Ok(row) => row,
            Err(e) if e == "not_found" => continue,
            Err(_) => return redirect("/admin/media?err=dberr"),
        };
        if current.storage != source.storage || current.url_path != source.url_path {
            return redirect("/admin/media?err=conflict");
        }
        if db.execute("DELETE FROM media WHERE id=?1", [id]).is_err() {
            return redirect("/admin/media?err=dberr");
        }
        drop(db);
        if let Some(path) = path
            && !path_in_use(&node, &path).await
        {
            let _ = std::fs::remove_file(path);
        }
        // Blob pins deliberately survive deletion; reconciliation owns release.
    }
    redirect("/admin/media")
}

/// Called under the physical-path lock, with NO database lock held. Shared
/// document roots across active vhosts must not unlink one another's media.
async fn path_in_use(node: &NodeState, path: &Path) -> bool {
    let key = physical_key(path);
    let directory = node.vhosts.load_full();
    for primary in &directory.primaries {
        let vhost = &primary.state;
        let Some(db) = &vhost.db else {
            continue;
        };
        let Ok(root) = vhost.www_dir.canonicalize() else {
            return true;
        };
        let Ok(relative) = key.strip_prefix(&root) else {
            continue;
        };
        let url = format!("/{}", relative.to_string_lossy());
        let db = db.lock().await;
        // A vhost with no media table cannot reference the file. Other schema or
        // query failures retain it conservatively for later reconciliation.
        match db.query_row(
            "SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE type='table' AND name='media')",
            [],
            |r| r.get::<_, bool>(0),
        ) {
            Ok(false) => continue,
            Ok(true) => (),
            Err(_) => return true,
        }
        let used = db
            .query_row(
                "SELECT count(*) FROM media WHERE storage='disk' AND url_path=?1",
                [url],
                |r| r.get::<_, i64>(0),
            )
            .unwrap_or(1);
        if used != 0 {
            return true;
        }
    }
    false
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn schema_migration_preserves_inline_rows_and_propagates_real_failures() {
        let db = Connection::open_in_memory().unwrap();
        db.execute_batch("CREATE TABLE media(id INTEGER PRIMARY KEY, filename TEXT, mime TEXT, data TEXT, bytes INTEGER, created TEXT);
            INSERT INTO media VALUES(1,'old','image/png','base64',6,'old-date')").unwrap();
        ensure_media_schema(&db).unwrap();
        ensure_media_schema(&db).unwrap();
        let source = row(&db, 1).unwrap();
        assert_eq!(source.storage, "inline");
        assert_eq!(source.blob, None);
        assert_eq!(
            db.query_row("SELECT data FROM media WHERE id=1", [], |r| r
                .get::<_, String>(0))
                .unwrap(),
            "base64"
        );
        let readonly = Connection::open_in_memory().unwrap();
        readonly.execute_batch("PRAGMA query_only=ON").unwrap();
        assert!(ensure_media_schema(&readonly).is_err());
    }
    #[test]
    fn attach_refuses_deleted_or_changed_rows() {
        let db = Connection::open_in_memory().unwrap();
        ensure_media_schema(&db).unwrap();
        db.execute("INSERT INTO media(filename,mime,bytes,storage,url_path,hash) VALUES('x','image/png',3,'disk','/img/x','prefix')", []).unwrap();
        let source = row(&db, 1).unwrap();
        let reference = crate::blob_reference::Reference {
            blob: format!("b3:{}", blake3::hash(b"abc").to_hex()),
            size: 3,
            mime: "image/png".into(),
            name: None,
            origin: "alpha".into(),
        };
        db.execute("UPDATE media SET url_path='/img/changed' WHERE id=1", [])
            .unwrap();
        assert!(
            attach_reference(&db, 1, &source, &reference)
                .unwrap_err()
                .starts_with("conflict:")
        );
        db.execute("DELETE FROM media WHERE id=1", []).unwrap();
        assert_eq!(
            attach_reference(&db, 1, &source, &reference).unwrap_err(),
            "not_found"
        );
    }
    #[tokio::test]
    async fn row_first_upload_survives_lane_outage_and_ref_rejects_inline() {
        let (_tmp, node, vhost) = crate::shares::tests::fixture().await;
        {
            let db = vhost.db.as_ref().unwrap().lock().await;
            db.execute_batch("CREATE TABLE users(username TEXT,role TEXT); INSERT INTO users VALUES('user@example.test','author')").unwrap();
        }
        let mut headers = HeaderMap::new();
        headers.insert(
            "cookie",
            format!(
                "cosmix_session={}",
                crate::shares::tests::cookie(&node, "maild", 0)
            )
            .parse()
            .unwrap(),
        );
        let png = [0x89, 0x50, 0x4e, 0x47, 0x0d, 0x0a, 0x1a, 0x0a, 0, 1];
        let response = media_upload(
            State(node.clone()),
            Extension(vhost.clone()),
            headers,
            Form(UploadForm {
                data: base64::engine::general_purpose::STANDARD.encode(png),
                filename: "image.png".into(),
            }),
        )
        .await;
        assert_eq!(response.status(), StatusCode::SEE_OTHER);
        assert_eq!(response.headers()["location"], "/admin/media");
        let source = {
            let db = vhost.db.as_ref().unwrap().lock().await;
            row(&db, 1).unwrap()
        };
        assert_eq!(source.blob, None);
        assert_eq!(
            std::fs::read(safe_disk_path(&vhost.www_dir, &source.url_path).unwrap()).unwrap(),
            png
        );
        assert!(
            media_ref(&node, &vhost, 1)
                .await
                .unwrap_err()
                .starts_with("lane_unavailable:")
        );
        {
            let db = vhost.db.as_ref().unwrap().lock().await;
            db.execute(
                "INSERT INTO media(filename,mime,data) VALUES('old','image/png','legacy')",
                [],
            )
            .unwrap();
        }
        assert!(
            media_ref(&node, &vhost, 2)
                .await
                .unwrap_err()
                .starts_with("unsupported_storage:")
        );
    }
    #[tokio::test]
    async fn path_and_row_slots_coalesce_and_prune() {
        let runtime = Runtime::default();
        let tmp = tempfile::tempdir().unwrap();
        let first = runtime.path(&tmp.path().join("img/x"));
        assert!(Arc::ptr_eq(
            &first,
            &runtime.path(&tmp.path().join("img/x"))
        ));
        let key = ("pim.example".into(), 1);
        let row = slot(&runtime.rows, key.clone());
        assert!(Arc::ptr_eq(&row, &slot(&runtime.rows, key)));
        let held = row.clone().lock_owned().await;
        assert!(row.try_lock().is_err());
        drop(held);
        assert!(row.try_lock().is_ok());
        drop(first);
        runtime.path(&tmp.path().join("img/y"));
        assert_eq!(runtime.paths.lock().unwrap().len(), 1);
    }
    #[test]
    fn unique_atomic_temps_never_publish_partial_bytes() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("image");
        let mut workers = Vec::new();
        for byte in 0..8u8 {
            let path = path.clone();
            workers.push(std::thread::spawn(move || {
                write_atomic(&path, &vec![byte; 65536]).unwrap()
            }));
        }
        for worker in workers {
            worker.join().unwrap();
        }
        let bytes = std::fs::read(&path).unwrap();
        assert_eq!(bytes.len(), 65536);
        assert!(bytes.iter().all(|b| *b == bytes[0]));
        assert_eq!(std::fs::read_dir(tmp.path()).unwrap().count(), 1);
    }

    #[test]
    fn sniff_recognises_each_allowed_type_and_rejects_others() {
        let png = [0x89, 0x50, 0x4E, 0x47, 0x0D, 0x0A, 0x1A, 0x0A, 0, 0];
        assert_eq!(sniff_image(&png), Some(("image/png", "png")));
        let jpg = [0xFF, 0xD8, 0xFF, 0xE0, 0, 0];
        assert_eq!(sniff_image(&jpg), Some(("image/jpeg", "jpg")));
        assert_eq!(sniff_image(b"GIF89a..."), Some(("image/gif", "gif")));
        let mut webp = Vec::from(*b"RIFF");
        webp.extend_from_slice(&[0, 0, 0, 0]);
        webp.extend_from_slice(b"WEBP....");
        assert_eq!(sniff_image(&webp), Some(("image/webp", "webp")));
        // SVG / HTML / empty must NOT pass.
        assert_eq!(sniff_image(b"<svg xmlns="), None);
        assert_eq!(sniff_image(b"<!DOCTYPE html>"), None);
        assert_eq!(sniff_image(b""), None);
    }

    #[test]
    fn safe_disk_path_jails_to_img_tree() {
        let root = Path::new("/srv/x/web/app/public");
        assert_eq!(
            safe_disk_path(root, "/img/2026/06/abc.png"),
            Some(root.join("img/2026/06/abc.png"))
        );
        // Escapes / wrong prefix are rejected.
        assert_eq!(safe_disk_path(root, "/img/../../etc/passwd"), None);
        assert_eq!(safe_disk_path(root, "/etc/passwd"), None);
        assert_eq!(safe_disk_path(root, "/base.css"), None);
    }

    #[test]
    fn image_paths_are_date_sharded_under_img() {
        let (disk, url) = image_paths(Path::new("/srv/pub"), "deadbeef", "webp");
        assert!(url.starts_with("/img/"));
        assert!(url.ends_with("/deadbeef.webp"));
        assert!(disk.starts_with("/srv/pub/img/"));
    }

    #[test]
    fn same_origin_accepts_match_and_missing_rejects_cross_origin() {
        let mk = |h: &'static str, v: &str| {
            let mut m = HeaderMap::new();
            m.insert(h, v.parse().unwrap());
            m
        };
        // Same-origin https Origin / Referer → ok.
        assert!(same_origin(
            &mk("origin", "https://example.net"),
            "example.net"
        ));
        assert!(same_origin(
            &mk("origin", "https://example.net:443/x"),
            "example.net"
        ));
        assert!(same_origin(
            &mk("referer", "https://example.net/admin/media"),
            "example.net"
        ));
        // Foreign host, non-https scheme, or unparseable → rejected.
        assert!(!same_origin(
            &mk("origin", "https://evil.example"),
            "example.net"
        ));
        assert!(!same_origin(
            &mk("origin", "http://example.net"),
            "example.net"
        ));
        assert!(!same_origin(&mk("origin", "null"), "example.net"));
        // Same host, DIFFERENT https port → rejected (cookies are host-scoped,
        // not port-scoped; a hostile service on :8443 must not ride the cookie).
        assert!(!same_origin(
            &mk("origin", "https://example.net:8443"),
            "example.net"
        ));
        // Explicit default port 443 → accepted.
        assert!(same_origin(
            &mk("origin", "https://example.net:443"),
            "example.net"
        ));
        // No Origin/Referer → ALLOWED: a browser cross-origin POST always emits
        // Origin, so a header-less POST is a non-browser client, not an attack
        // (the media route's own admin-session auth still applies).
        assert!(same_origin(&HeaderMap::new(), "example.net"));
        // PRESENT but non-UTF-8 Origin → rejected (not treated as "absent").
        let mut ng = HeaderMap::new();
        ng.insert(
            "origin",
            axum::http::HeaderValue::from_bytes(&[0xff, 0xfe]).unwrap(),
        );
        assert!(!same_origin(&ng, "example.net"));
    }

    #[test]
    fn sanitize_label_trims_controls_and_defaults() {
        assert_eq!(sanitize_label("  photo.png\n"), "photo.png");
        assert_eq!(sanitize_label(""), "upload");
        assert_eq!(sanitize_label("\t\r\n"), "upload");
        assert_eq!(sanitize_label(&"x".repeat(500)).len(), 200);
    }
}
