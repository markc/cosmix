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
fn same_origin(headers: &HeaderMap, fqdn: &str) -> bool {
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
    let parent = path
        .parent()
        .ok_or_else(|| std::io::Error::other("image path has no parent"))?;
    std::fs::create_dir_all(parent)?;
    let fname = path
        .file_name()
        .and_then(|n| n.to_str())
        .ok_or_else(|| std::io::Error::other("image path has no file name"))?;
    let tmp = parent.join(format!(".tmp-{fname}"));
    std::fs::write(&tmp, bytes)?;
    std::fs::rename(&tmp, path)
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

/// Idempotently ensure the `media` table and its disk-storage columns
/// exist, so a native upload doesn't depend on a Mix handler (`cms_init`)
/// having run first. ALTER on an existing column errors — ignored.
fn ensure_media_schema(conn: &Connection) {
    let _ = conn.execute(
        "CREATE TABLE IF NOT EXISTS media (id INTEGER PRIMARY KEY AUTOINCREMENT, \
         filename TEXT NOT NULL, mime TEXT NOT NULL, data TEXT NOT NULL DEFAULT '', \
         bytes INTEGER NOT NULL DEFAULT 0, created TEXT NOT NULL DEFAULT (datetime('now')))",
        [],
    );
    for col in [
        "storage TEXT NOT NULL DEFAULT 'inline'",
        "url_path TEXT NOT NULL DEFAULT ''",
        "hash TEXT NOT NULL DEFAULT ''",
    ] {
        let _ = conn.execute(&format!("ALTER TABLE media ADD COLUMN {col}"), []);
    }
}

/// `POST /admin/media/upload` — store an uploaded image as a file on disk
/// and record its metadata row.
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

    // Auth FIRST — an unauthenticated request does no decode or filesystem
    // work (avoids the base64-decode/IO DoS surface). Brief lock just for
    // the session check + schema ensure.
    {
        let db = db_mtx.lock().await;
        let authed = tokio::task::block_in_place(|| {
            ensure_media_schema(&db);
            cms_author(&node.session, &vhost.fqdn, &db, &headers)
        });
        drop(db);
        if !authed {
            return redirect("/auth/login");
        }
    }

    // Decode + validate (no lock held).
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
    let hash = &hash_full[..32]; // 128-bit prefix — ample for a filename
    let (disk_path, url_path) = image_paths(&vhost.www_dir, hash, ext);
    let label = sanitize_label(&form.filename);
    let bytes_len = raw.len() as i64;

    // Write the file WITHOUT holding the DB lock (filesystem latency must
    // not serialize unrelated CMS DB work). There is a narrow window between
    // this write and the insert below where a process kill would leave a
    // row-less file under public/img — an accepted residual: it's a valid,
    // admin-uploaded image at an unguessable hashed URL (no security impact,
    // just disk cruft a future reconcile sweep can GC). A normal insert
    // failure is rolled back just below.
    if tokio::task::block_in_place(|| write_atomic(&disk_path, &raw)).is_err() {
        return redirect("/admin/media?err=writeerr");
    }

    // Record the row. If the insert fails, roll back the file we just
    // wrote — but ONLY if no other row still references that exact path
    // (a prior identical upload in the same month shares the file).
    let db = db_mtx.lock().await;
    let inserted = tokio::task::block_in_place(|| {
        db.execute(
            "INSERT INTO media (filename, mime, data, bytes, storage, url_path, hash) \
             VALUES (?1, ?2, '', ?3, 'disk', ?4, ?5)",
            rusqlite::params![label, mime, bytes_len, url_path, hash],
        )
    });
    if inserted.is_err() {
        let still: i64 = tokio::task::block_in_place(|| {
            db.query_row(
                "SELECT count(*) FROM media WHERE url_path = ?1",
                rusqlite::params![url_path],
                |r| r.get(0),
            )
            .unwrap_or(1)
        });
        if still == 0 {
            let _ = std::fs::remove_file(&disk_path);
        }
    }
    drop(db);

    if inserted.is_ok() {
        redirect("/admin/media")
    } else {
        redirect("/admin/media?err=dberr")
    }
}

/// `POST /admin/media/delete` — delete one OR many media rows; for each disk row
/// whose content no other row still references, unlink the file too. Accepts a
/// single `id` (the per-row right-click delete) AND/OR the shared datatable
/// multi-select convention `sel_<id>=1` (bulk delete) — both POST here so the file
/// unlink stays server-side (the Mix handler is sandboxed from unlinking files).
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

    // Collect every id to delete: a lone `id` (single delete) + each checked
    // `sel_<id>` (datatable bulk). Dedup so an id sent both ways deletes once.
    let mut ids: Vec<i64> = Vec::new();
    if let Some(v) = form.get("id")
        && let Ok(n) = v.parse::<i64>()
    {
        ids.push(n);
    }
    for (k, v) in &form {
        if v == "1"
            && let Some(rest) = k.strip_prefix("sel_")
            && let Ok(n) = rest.parse::<i64>()
        {
            ids.push(n);
        }
    }
    ids.sort_unstable();
    ids.dedup();
    // Bound the per-request work under the DB lock: a malformed (but authed + same-origin)
    // POST could otherwise send an unbounded sel_* set. The datatable's max page size is the
    // realistic ceiling for a real bulk action; ignore any excess.
    const MAX_BULK_DELETE: usize = 1000;
    ids.truncate(MAX_BULK_DELETE);

    let db = db_mtx.lock().await;
    let authed = tokio::task::block_in_place(|| {
        ensure_media_schema(&db);
        if !cms_author(&node.session, &vhost.fqdn, &db, &headers) {
            return false;
        }
        for id in &ids {
            let row: Option<(String, String)> = db
                .query_row(
                    "SELECT storage, url_path FROM media WHERE id = ?1",
                    rusqlite::params![id],
                    |r| Ok((r.get(0)?, r.get(1)?)),
                )
                .optional()
                .ok()
                .flatten();
            let _ = db.execute("DELETE FROM media WHERE id = ?1", rusqlite::params![id]);
            // Refcount by the PHYSICAL path (url_path), not the content hash:
            // identical bytes uploaded in different months are separate files,
            // so a hash-based refcount would orphan one of them.
            if let Some((storage, url_path)) = row
                && storage == "disk"
            {
                let still: i64 = db
                    .query_row(
                        "SELECT count(*) FROM media WHERE url_path = ?1 AND storage = 'disk'",
                        rusqlite::params![url_path],
                        |r| r.get(0),
                    )
                    .unwrap_or(0);
                if still == 0
                    && let Some(p) = safe_disk_path(&vhost.www_dir, &url_path)
                {
                    let _ = std::fs::remove_file(p);
                }
            }
        }
        true
    });
    drop(db);

    if authed {
        redirect("/admin/media")
    } else {
        redirect("/auth/login")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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
