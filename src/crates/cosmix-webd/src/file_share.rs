//! Public file shares — the Nextcloud public-link replacement (WS4 scaffold).
//!
//! **Status: compiling SCAFFOLD, not a routed production surface.** This
//! module owns the share *catalogue* + the security-critical primitives
//! (token minting, path jail, token → jailed-path resolution, expiry /
//! revocation / password gates). What is deliberately NOT wired this run:
//! the axum routes (`GET/HEAD /s/<token>`, the authenticated
//! `share.create|list|revoke` handler), file-drop upload, and Thunderbird
//! FileLink. Those integrate against webd's vhost/router/DB state in a
//! follow-up; the contracts they must honour are in
//! `_decisions/2026-07-13-files-sync-contracts.md` (C6).
//!
//! Why this shape (Codex D7): the 54 live NC public links are the real
//! Nextcloud dependency, not a sync daemon. Bytes live once under a
//! filesd-jailed per-account root; a share is a catalogue row mapping an
//! opaque token to a *relative* path under that root — the token never
//! carries a filesystem path, so a leaked/guessed token cannot escape the
//! jail even if the catalogue is intact.
//!
//! `dead_code` is allowed module-wide **because** the module is not yet
//! wired into the router — the public surface is exercised only by this
//! module's tests. Remove the allow in the same change that adds routes.
#![allow(dead_code)]

use std::path::{Component, Path, PathBuf};

use base64::Engine;
use rusqlite::{Connection, OptionalExtension, params};

/// Share kinds. `File`/`Dir` are read-only download links; `Drop` is a
/// write-only inbox (outsiders upload) — the upload route is a documented
/// follow-up, but the kind is modelled now so the schema is stable.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ShareKind {
    File,
    Dir,
    Drop,
}

impl ShareKind {
    pub fn as_str(self) -> &'static str {
        match self {
            ShareKind::File => "file",
            ShareKind::Dir => "dir",
            ShareKind::Drop => "drop",
        }
    }
    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "file" => Some(ShareKind::File),
            "dir" => Some(ShareKind::Dir),
            "drop" => Some(ShareKind::Drop),
            _ => None,
        }
    }
}

/// A catalogue row (what `share.list` returns; `password_hash` is never
/// surfaced).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Share {
    pub token: String,
    pub account_id: i64,
    pub rel_path: String,
    pub kind: ShareKind,
    pub has_password: bool,
    pub expires_at: Option<i64>,
    pub created_at: i64,
    pub revoked: bool,
    pub download_count: i64,
}

/// Why a token failed to resolve to a servable path. All map to a
/// deliberately indistinguishable client response (404 for
/// absent/revoked/expired, 401 for a password-required/mismatch) so a
/// probe learns nothing about which tokens exist.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ShareDenied {
    NotFound,
    Revoked,
    Expired,
    PasswordRequired,
    PasswordMismatch,
    /// The stored `rel_path` failed the jail (corrupt/hostile catalogue
    /// row) — never serve it.
    Unsafe,
}

/// Initialise the share catalogue. Idempotent.
pub fn init_schema(conn: &Connection) -> rusqlite::Result<()> {
    conn.execute_batch(
        "CREATE TABLE IF NOT EXISTS file_shares (
            token          TEXT PRIMARY KEY,
            account_id     INTEGER NOT NULL,
            rel_path       TEXT NOT NULL,
            kind           TEXT NOT NULL,
            password_hash  TEXT,
            expires_at     INTEGER,
            created_at     INTEGER NOT NULL,
            revoked        INTEGER NOT NULL DEFAULT 0,
            download_count INTEGER NOT NULL DEFAULT 0
        );
        CREATE INDEX IF NOT EXISTS idx_file_shares_account
            ON file_shares(account_id) WHERE revoked = 0;",
    )?;
    Ok(())
}

/// Mint a 160-bit opaque, URL-safe token. Unguessable — the only
/// authorisation a share link carries.
pub fn mint_token() -> String {
    let mut buf = [0u8; 20];
    rand::RngCore::fill_bytes(&mut rand::rngs::OsRng, &mut buf);
    base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(buf)
}

/// Jail a caller-supplied relative path to a per-account root. Rejects
/// absolute paths, `..`, `.`, root/prefix/current-dir components, and
/// (belt over the component check) any residual `..` — a share path must
/// stay strictly inside `root`. Returns the absolute on-disk path.
///
/// This is the ONE place a token becomes a filesystem path; every serve
/// call goes through it, so a corrupt catalogue row can never escape.
pub fn jail_path(root: &Path, rel: &str) -> Result<PathBuf, ShareDenied> {
    if rel.is_empty() {
        return Err(ShareDenied::Unsafe);
    }
    let candidate = Path::new(rel);
    // Only plain, forward path segments are allowed.
    for comp in candidate.components() {
        match comp {
            Component::Normal(_) => {}
            _ => return Err(ShareDenied::Unsafe),
        }
    }
    // Defence in depth: even after the component check, refuse a literal
    // `..` anywhere in the raw string (covers exotic encodings the
    // component iterator might normalise).
    if rel.split(['/', '\\']).any(|seg| seg == "..") {
        return Err(ShareDenied::Unsafe);
    }
    Ok(root.join(candidate))
}

/// Create a share. `password_hash` is a pre-hashed bcrypt string (webd's
/// authenticated handler hashes the plaintext before calling — this
/// module never sees the plaintext). Caller is responsible for having
/// verified `account_id` owns `rel_path`; `jail_path` is re-checked at
/// serve time regardless.
#[allow(clippy::too_many_arguments)]
pub fn create(
    conn: &Connection,
    account_id: i64,
    rel_path: &str,
    kind: ShareKind,
    password_hash: Option<&str>,
    expires_at: Option<i64>,
    now: i64,
) -> rusqlite::Result<String> {
    let token = mint_token();
    conn.execute(
        "INSERT INTO file_shares
            (token, account_id, rel_path, kind, password_hash, expires_at, created_at)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
        params![
            token,
            account_id,
            rel_path,
            kind.as_str(),
            password_hash,
            expires_at,
            now,
        ],
    )?;
    Ok(token)
}

fn row_to_share(row: &rusqlite::Row<'_>) -> rusqlite::Result<(Share, Option<String>)> {
    let kind_s: String = row.get("kind")?;
    let pw: Option<String> = row.get("password_hash")?;
    let share = Share {
        token: row.get("token")?,
        account_id: row.get("account_id")?,
        rel_path: row.get("rel_path")?,
        kind: ShareKind::parse(&kind_s).unwrap_or(ShareKind::File),
        has_password: pw.is_some(),
        expires_at: row.get("expires_at")?,
        created_at: row.get("created_at")?,
        revoked: row.get::<_, i64>("revoked")? != 0,
        download_count: row.get("download_count")?,
    };
    Ok((share, pw))
}

/// List an account's non-revoked shares (newest first). Never returns
/// the password hash.
pub fn list(conn: &Connection, account_id: i64) -> rusqlite::Result<Vec<Share>> {
    let mut stmt = conn.prepare(
        "SELECT * FROM file_shares
         WHERE account_id = ?1 AND revoked = 0
         ORDER BY created_at DESC",
    )?;
    let rows = stmt.query_map(params![account_id], |r| Ok(row_to_share(r)?.0))?;
    rows.collect()
}

/// Revoke a share the account owns. Returns `true` if a row was revoked
/// (idempotent: re-revoking an already-revoked/absent token = `false`).
/// Scoped by `account_id` so one tenant can't revoke another's token.
pub fn revoke(conn: &Connection, account_id: i64, token: &str) -> rusqlite::Result<bool> {
    let n = conn.execute(
        "UPDATE file_shares SET revoked = 1
         WHERE token = ?1 AND account_id = ?2 AND revoked = 0",
        params![token, account_id],
    )?;
    Ok(n > 0)
}

/// The outcome of resolving a public token to a servable path.
#[derive(Debug)]
pub struct Resolved {
    pub share: Share,
    /// Absolute, jailed on-disk path under the account root.
    pub path: PathBuf,
}

/// Resolve a public `GET /s/<token>` to a jailed on-disk path, enforcing
/// revocation, expiry, and (via `verify_password`) any password gate.
/// `account_root_for` maps the row's `account_id` to that account's
/// filesd root (the caller owns the root layout). `password` is the
/// visitor's submitted plaintext (None if none supplied); `verify_password`
/// checks it against the stored hash in constant time (bcrypt).
///
/// Does NOT increment the download counter — the caller does that only
/// after a byte actually ships (see `bump_download`).
pub fn resolve<F, V>(
    conn: &Connection,
    token: &str,
    now: i64,
    password: Option<&str>,
    account_root_for: F,
    verify_password: V,
) -> Result<Resolved, ShareDenied>
where
    F: FnOnce(i64) -> Option<PathBuf>,
    V: FnOnce(&str, &str) -> bool,
{
    let mut stmt = conn
        .prepare("SELECT * FROM file_shares WHERE token = ?1")
        .map_err(|_| ShareDenied::NotFound)?;
    let row = stmt
        .query_row(params![token], row_to_share)
        .optional()
        .map_err(|_| ShareDenied::NotFound)?;
    let (share, pw_hash) = row.ok_or(ShareDenied::NotFound)?;

    if share.revoked {
        return Err(ShareDenied::Revoked);
    }
    if share.expires_at.is_some_and(|e| now >= e) {
        return Err(ShareDenied::Expired);
    }
    if let Some(hash) = pw_hash.as_deref() {
        match password {
            None => return Err(ShareDenied::PasswordRequired),
            Some(p) if verify_password(p, hash) => {}
            Some(_) => return Err(ShareDenied::PasswordMismatch),
        }
    }

    let root = account_root_for(share.account_id).ok_or(ShareDenied::NotFound)?;
    let path = jail_path(&root, &share.rel_path)?;
    Ok(Resolved { share, path })
}

/// Increment a share's download counter (best-effort telemetry; call
/// after a successful serve).
pub fn bump_download(conn: &Connection, token: &str) -> rusqlite::Result<()> {
    conn.execute(
        "UPDATE file_shares SET download_count = download_count + 1 WHERE token = ?1",
        params![token],
    )?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn mem() -> Connection {
        let c = Connection::open_in_memory().unwrap();
        init_schema(&c).unwrap();
        c
    }

    #[test]
    fn token_is_urlsafe_and_unique() {
        let a = mint_token();
        let b = mint_token();
        assert_ne!(a, b);
        assert!(!a.contains('/') && !a.contains('+') && !a.contains('='));
        assert!(a.len() >= 26); // 20 bytes base64url-nopad = 27 chars
    }

    #[test]
    fn jail_rejects_traversal_and_absolute() {
        let root = Path::new("/srv/acct/7");
        assert_eq!(jail_path(root, "../etc/passwd"), Err(ShareDenied::Unsafe));
        assert_eq!(jail_path(root, "a/../../b"), Err(ShareDenied::Unsafe));
        assert_eq!(jail_path(root, "/etc/passwd"), Err(ShareDenied::Unsafe));
        assert_eq!(jail_path(root, ""), Err(ShareDenied::Unsafe));
        assert_eq!(jail_path(root, "."), Err(ShareDenied::Unsafe));
        // A legitimate nested path stays inside the root.
        assert_eq!(
            jail_path(root, "Photos/2020/img.jpg").unwrap(),
            PathBuf::from("/srv/acct/7/Photos/2020/img.jpg")
        );
    }

    #[test]
    fn create_list_revoke_roundtrip() {
        let c = mem();
        let t = create(&c, 7, "Docs/report.pdf", ShareKind::File, None, None, 1000).unwrap();
        let shares = list(&c, 7).unwrap();
        assert_eq!(shares.len(), 1);
        assert_eq!(shares[0].token, t);
        assert_eq!(shares[0].rel_path, "Docs/report.pdf");
        assert!(!shares[0].has_password);
        // Another account can't see it.
        assert!(list(&c, 8).unwrap().is_empty());
        // Another account can't revoke it.
        assert!(!revoke(&c, 8, &t).unwrap());
        assert_eq!(list(&c, 7).unwrap().len(), 1);
        // Owner revokes; idempotent second revoke is false.
        assert!(revoke(&c, 7, &t).unwrap());
        assert!(!revoke(&c, 7, &t).unwrap());
        assert!(list(&c, 7).unwrap().is_empty());
    }

    fn root_for(id: i64) -> Option<PathBuf> {
        Some(PathBuf::from(format!("/srv/acct/{id}")))
    }
    fn pw_never(_p: &str, _h: &str) -> bool {
        false
    }
    fn pw_always(_p: &str, _h: &str) -> bool {
        true
    }

    #[test]
    fn resolve_happy_path() {
        let c = mem();
        let t = create(&c, 3, "a/b.txt", ShareKind::File, None, None, 1000).unwrap();
        let r = resolve(&c, &t, 2000, None, root_for, pw_never).unwrap();
        assert_eq!(r.path, PathBuf::from("/srv/acct/3/a/b.txt"));
        assert_eq!(r.share.account_id, 3);
    }

    #[test]
    fn resolve_rejects_revoked_expired_missing() {
        let c = mem();
        assert_eq!(
            resolve(&c, "nope", 2000, None, root_for, pw_never).unwrap_err(),
            ShareDenied::NotFound
        );
        let t = create(&c, 3, "a.txt", ShareKind::File, None, Some(1500), 1000).unwrap();
        assert_eq!(
            resolve(&c, &t, 2000, None, root_for, pw_never).unwrap_err(),
            ShareDenied::Expired
        );
        // not yet expired
        assert!(resolve(&c, &t, 1400, None, root_for, pw_never).is_ok());
        revoke(&c, 3, &t).unwrap();
        assert_eq!(
            resolve(&c, &t, 1400, None, root_for, pw_never).unwrap_err(),
            ShareDenied::Revoked
        );
    }

    #[test]
    fn resolve_password_gate() {
        let c = mem();
        let t = create(
            &c,
            3,
            "a.txt",
            ShareKind::File,
            Some("bcrypt$stored"),
            None,
            1000,
        )
        .unwrap();
        assert_eq!(
            resolve(&c, &t, 2000, None, root_for, pw_always).unwrap_err(),
            ShareDenied::PasswordRequired
        );
        assert_eq!(
            resolve(&c, &t, 2000, Some("wrong"), root_for, pw_never).unwrap_err(),
            ShareDenied::PasswordMismatch
        );
        assert!(resolve(&c, &t, 2000, Some("right"), root_for, pw_always).is_ok());
    }

    #[test]
    fn resolve_rejects_corrupt_relpath() {
        let c = mem();
        // Hostile catalogue row (simulating tampering) must not escape.
        c.execute(
            "INSERT INTO file_shares (token, account_id, rel_path, kind, created_at)
             VALUES ('x', 3, '../../etc/passwd', 'file', 1000)",
            [],
        )
        .unwrap();
        assert_eq!(
            resolve(&c, "x", 2000, None, root_for, pw_never).unwrap_err(),
            ShareDenied::Unsafe
        );
    }

    #[test]
    fn download_counter_bumps() {
        let c = mem();
        let t = create(&c, 3, "a.txt", ShareKind::File, None, None, 1000).unwrap();
        bump_download(&c, &t).unwrap();
        bump_download(&c, &t).unwrap();
        assert_eq!(list(&c, 3).unwrap()[0].download_count, 2);
    }
}
