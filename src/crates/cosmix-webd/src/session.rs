//! Stateless AEAD-sealed session cookies for the SSR PIM (Phase 2).
//!
//! A `/auth/login` POST exchanges the user's password for a maild bearer
//! token (Slice 1's `POST /auth/tokens/issue`) and seals
//! `{vhost, maild_token, email, iat, exp, csrf}` into an HttpOnly/Secure/
//! SameSite=Lax cookie with XChaCha20-Poly1305 under a per-node key. The
//! `email` is the verified user-principal id (the login-unification slice):
//! webd injects it as `$SESSION.email` so handlers map it to a CMS role.
//!
//! There is **no server-side session store**: every request's authority
//! is reconstructed by unsealing the cookie. The browser can never read
//! the maild token (HttpOnly + AEAD seal). The payload is
//! **vhost-bound**: unseal rejects a cookie whose `vhost` ≠ the serving
//! host, so a cookie minted for one tenant cannot authorise another even
//! if the nodes share a sealing key.
//!
//! **Revocation is two-authority** (2026-07 audit — the earlier claim
//! here that "maild stays the revocation authority" was true only for
//! the bearer path):
//! - **maild** owns the *bearer/JMAP* path: a 401 on a forwarded
//!   `Bearer` is treated as an expired session.
//! - **webd's per-account session epoch** (`session_epochs` table in the
//!   per-vhost CMS DB) owns the *cookie* path: the epoch current at
//!   login is sealed into the payload, and every cookie-authorized
//!   surface (CMS RBAC, the Mix `$SESSION` identity, the delegated
//!   `bus_call` gate) rejects a payload whose `epoch` no longer matches.
//!   Bumping the epoch (the `webd.session.revoke` Bus verb) instantly
//!   kills every outstanding cookie for that account, node-key rotation
//!   no longer being the only (all-sessions) kill switch.
//!
//! A surgical "revoke account X" must hit BOTH authorities — use the
//! unified revocation script (`_bin/revoke_account.mix` in the cosmix
//! hub), which bumps the webd epoch and revokes the maild tokens in one
//! operation, rather than either verb alone.
//!
//! CSRF uses double-submit: the same random `csrf` is both sealed inside
//! the (unreadable) session cookie and set in a readable companion
//! cookie; a state-changing POST must echo it in a form field, and the
//! handler compares the echo against the sealed value.

use std::io;
use std::path::Path;

use base64::Engine;
use chacha20poly1305::aead::{Aead, KeyInit};
use chacha20poly1305::{Key, XChaCha20Poly1305, XNonce};
use serde::{Deserialize, Serialize};

/// XChaCha20-Poly1305 nonce width (192-bit). Wide enough that fresh
/// per-seal random nonces have no practical collision bound.
const NONCE_BYTES: usize = 24;
/// Symmetric key width.
const KEY_BYTES: usize = 32;

/// Sealed session cookie name (HttpOnly — JS cannot read it).
pub const SESSION_COOKIE: &str = "cosmix_session";
/// Readable CSRF companion cookie (NOT HttpOnly — the double-submit
/// value the form/header must echo).
pub const CSRF_COOKIE: &str = "cosmix_csrf";

/// Email-2FA pending-login cookie (HttpOnly — JS cannot read it). Carries
/// ONLY an opaque random id keyed into webd's server-side pending-login map
/// (P3). The map row holds the live maild bearer + the emailed code's hash;
/// the bearer never reaches the browser until the second factor passes.
pub const LOGIN_PENDING_COOKIE: &str = "cosmix_login_pending";

/// Session lifetime — mirrors maild's 30-day token TTL. maild is the
/// real expiry authority; this bounds the cookie so it never long
/// outlives the token it carries.
pub const SESSION_TTL_SECS: i64 = 30 * 24 * 60 * 60;

/// The sealed cookie payload. Serialized to JSON, then AEAD-sealed.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct SessionPayload {
    /// The serving vhost FQDN this session is bound to. Unseal rejects a
    /// host mismatch.
    pub vhost: String,
    /// The maild bearer token (Slice 1). Forwarded as `Bearer <token>`
    /// to maild by the SSR `jmap()` seam.
    pub maild_token: String,
    /// The authenticated maild account email — the user-principal id. Sealed
    /// at login so a handler learns *who* is signed in via the injected
    /// `$SESSION` global without a maild round-trip; the per-vhost grants map
    /// this email to a role (the app-level RBAC slice).
    pub email: String,
    /// Issued-at (unix seconds).
    pub iat: i64,
    /// Expiry (unix seconds). Unseal rejects when `exp <= now`.
    pub exp: i64,
    /// Double-submit CSRF token (also mirrored in the readable
    /// `CSRF_COOKIE`).
    pub csrf: String,
    /// The account's webd session epoch at seal time (cookie-path
    /// revocation — see the module doc). Compared against the live
    /// `session_epochs` row on every cookie-authorized request; a bump
    /// invalidates every earlier cookie. `serde(default)` = 0 so
    /// cookies sealed before this field existed stay valid until the
    /// account's first revocation.
    #[serde(default)]
    pub epoch: i64,
    /// Identity kind. `"maild"` (default; every cookie sealed before this
    /// field existed) = a maild-account session with a `maild_token` and full
    /// role resolution via the users table. `"customer"` = a billing-portal
    /// customer session: `maild_token` is EMPTY (no JMAP authority) and role
    /// resolution is CAPPED at rank-1 "user" regardless of the users table, so
    /// a customer session can never reach an admin surface even if a customer
    /// email collides with an admin grant. Unseal rejects any other value.
    #[serde(default = "default_kind")]
    pub kind: String,
    /// The billing `customers.id` for a `kind="customer"` session (0/None for
    /// maild sessions). A convenience for the portal; Mix still re-checks by
    /// email — the security boundary is `kind`, not this field.
    #[serde(default)]
    pub customer_id: i64,
}

/// serde default for `SessionPayload.kind` — pre-field cookies are maild.
fn default_kind() -> String {
    "maild".to_string()
}

/// Holds the per-node sealing key as an initialized cipher. Cheap to
/// share behind an `Arc`.
pub struct SessionSealer {
    cipher: XChaCha20Poly1305,
}

impl std::fmt::Debug for SessionSealer {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // Never print the key material.
        f.debug_struct("SessionSealer").finish_non_exhaustive()
    }
}

impl SessionSealer {
    /// Build a sealer from a raw 32-byte key.
    pub fn from_key(key: [u8; KEY_BYTES]) -> Self {
        let cipher = XChaCha20Poly1305::new(Key::from_slice(&key));
        Self { cipher }
    }

    /// An in-memory sealer with a random key, for tests and the pre-ACME
    /// bootstrap node (which never serves login traffic). Cookies sealed
    /// by an ephemeral key die with the process.
    pub fn ephemeral() -> Self {
        let mut key = [0u8; KEY_BYTES];
        rand::RngCore::fill_bytes(&mut rand::rngs::OsRng, &mut key);
        Self::from_key(key)
    }

    /// Load the 32-byte key from `path`, or generate + persist one (mode
    /// 0600, atomic temp-file + rename) if absent. Parent dirs are
    /// created. A file present but not exactly 32 bytes is a hard error
    /// (refuse to silently overwrite an unexpected file — an operator
    /// deletes it to force regeneration; regenerating only invalidates
    /// live cookies, never maild tokens).
    pub fn load_or_generate(path: &Path) -> io::Result<Self> {
        match std::fs::read(path) {
            Ok(bytes) if bytes.len() == KEY_BYTES => {
                check_key_perms(path)?;
                let mut key = [0u8; KEY_BYTES];
                key.copy_from_slice(&bytes);
                Ok(Self::from_key(key))
            }
            Ok(bytes) => Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!(
                    "session key at {} is {} bytes, expected {KEY_BYTES}; delete it to regenerate",
                    path.display(),
                    bytes.len()
                ),
            )),
            Err(e) if e.kind() == io::ErrorKind::NotFound => {
                let mut key = [0u8; KEY_BYTES];
                rand::RngCore::fill_bytes(&mut rand::rngs::OsRng, &mut key);
                // Race-safe: a concurrent webd that won the create gets its
                // key persisted; the loser's no-clobber persist fails, and
                // it re-reads the winner's key rather than churning it.
                match Self::persist_key_noclobber(path, &key)? {
                    Some(()) => Ok(Self::from_key(key)),
                    None => {
                        let bytes = std::fs::read(path)?;
                        if bytes.len() != KEY_BYTES {
                            return Err(io::Error::new(
                                io::ErrorKind::InvalidData,
                                format!(
                                    "session key at {} reappeared at {} bytes (expected {KEY_BYTES})",
                                    path.display(),
                                    bytes.len()
                                ),
                            ));
                        }
                        check_key_perms(path)?;
                        let mut existing = [0u8; KEY_BYTES];
                        existing.copy_from_slice(&bytes);
                        Ok(Self::from_key(existing))
                    }
                }
            }
            Err(e) => Err(e),
        }
    }

    /// Atomically write `key` to `path` with mode 0600 WITHOUT clobbering
    /// an existing file. Uses a temp file in the same directory + a
    /// no-clobber rename. Returns `Ok(Some(()))` if this call created the
    /// key, `Ok(None)` if `path` already existed (lost a concurrent race),
    /// `Err` on real IO failure.
    fn persist_key_noclobber(path: &Path, key: &[u8; KEY_BYTES]) -> io::Result<Option<()>> {
        let dir = path.parent().ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::InvalidInput,
                "session key path has no parent directory",
            )
        })?;
        std::fs::create_dir_all(dir)?;
        // `NamedTempFile` creates with mode 0600 on Unix (O_EXCL); persist
        // preserves it across the rename.
        let mut tmp = tempfile::NamedTempFile::new_in(dir)?;
        {
            use std::io::Write;
            tmp.write_all(key)?;
            tmp.flush()?;
        }
        // Belt-and-braces: assert 0600 even if the platform default ever
        // changes.
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(tmp.path(), std::fs::Permissions::from_mode(0o600))?;
        }
        match tmp.persist_noclobber(path) {
            Ok(_) => Ok(Some(())),
            Err(e) if e.error.kind() == io::ErrorKind::AlreadyExists => Ok(None),
            Err(e) => Err(e.error),
        }
    }

    /// Seal a payload into a cookie value: `base64url(nonce || ciphertext)`.
    pub fn seal(&self, payload: &SessionPayload) -> Result<String, String> {
        let plaintext = serde_json::to_vec(payload).map_err(|e| format!("seal serialize: {e}"))?;
        let mut nonce_bytes = [0u8; NONCE_BYTES];
        rand::RngCore::fill_bytes(&mut rand::rngs::OsRng, &mut nonce_bytes);
        let nonce = XNonce::from_slice(&nonce_bytes);
        let ct = self
            .cipher
            .encrypt(nonce, plaintext.as_ref())
            .map_err(|_| "seal encrypt failed".to_string())?;
        let mut out = Vec::with_capacity(NONCE_BYTES + ct.len());
        out.extend_from_slice(&nonce_bytes);
        out.extend_from_slice(&ct);
        Ok(base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(out))
    }

    /// Unseal a cookie value. Returns `None` for any failure — bad
    /// base64, wrong key (AEAD tag mismatch), malformed JSON, a vhost
    /// mismatch, or an expired (`exp <= now`) payload. The caller cannot
    /// distinguish the causes (no oracle).
    pub fn unseal(&self, cookie: &str, expected_vhost: &str, now: i64) -> Option<SessionPayload> {
        let raw = base64::engine::general_purpose::URL_SAFE_NO_PAD
            .decode(cookie.trim())
            .ok()?;
        if raw.len() <= NONCE_BYTES {
            return None;
        }
        let (nonce_bytes, ct) = raw.split_at(NONCE_BYTES);
        let nonce = XNonce::from_slice(nonce_bytes);
        let pt = self.cipher.decrypt(nonce, ct).ok()?;
        let payload: SessionPayload = serde_json::from_slice(&pt).ok()?;
        if payload.vhost != expected_vhost {
            return None;
        }
        if payload.exp <= now {
            return None;
        }
        // Reject an unknown identity kind rather than fall through to a
        // default-maild interpretation — a forged/garbled kind must never be
        // treated as a privileged maild session. (Only our own seal writes
        // these, but unseal is the trust boundary.)
        if payload.kind != "maild" && payload.kind != "customer" {
            return None;
        }
        Some(payload)
    }
}

/// Refuse a session key file that is group/world-accessible (Unix). A
/// `0640`/`0644` key would expose the AEAD secret to other local users,
/// letting them forge or decrypt cookies — fail loud (operator runs
/// `chmod 600`) rather than silently accept it. No-op on non-Unix.
fn check_key_perms(path: &Path) -> io::Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mode = std::fs::metadata(path)?.permissions().mode();
        if mode & 0o077 != 0 {
            return Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                format!(
                    "session key at {} has insecure permissions {:#o} (group/other bits set); \
                     run `chmod 600` on it",
                    path.display(),
                    mode & 0o7777
                ),
            ));
        }
    }
    #[cfg(not(unix))]
    {
        let _ = path;
    }
    Ok(())
}

/// Mint a fresh random CSRF token (192-bit, base64url) for the
/// double-submit pattern.
pub fn new_csrf_token() -> String {
    let mut buf = [0u8; 24];
    rand::RngCore::fill_bytes(&mut rand::rngs::OsRng, &mut buf);
    base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(buf)
}

/// Mint a fresh opaque pending-login id (192-bit, base64url) — the
/// unguessable handle into webd's email-2FA pending map (P3). Same entropy
/// shape as a CSRF token; a distinct name documents the different role (a map
/// key, not a double-submit value).
pub fn new_pending_id() -> String {
    new_csrf_token()
}

/// Current unix time in seconds. A clock before the epoch yields 0
/// (only possible on a grossly-misconfigured host; treated as "very old"
/// so sessions simply fail closed).
pub fn now_secs() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

/// Extract a cookie value by name from a `Cookie` request header. Parses
/// the RFC 6265 `name=value; name2=value2` list; returns the first match.
pub fn cookie_value(headers: &axum::http::HeaderMap, name: &str) -> Option<String> {
    let header = headers.get(axum::http::header::COOKIE)?.to_str().ok()?;
    for pair in header.split(';') {
        let pair = pair.trim();
        if let Some((k, v)) = pair.split_once('=')
            && k.trim() == name
        {
            return Some(v.trim().to_string());
        }
    }
    None
}

/// Constant-time comparison of two CSRF tokens (avoid a timing oracle on
/// the double-submit check). Lengths differing is an immediate `false`.
pub fn csrf_eq(a: &str, b: &str) -> bool {
    let (a, b) = (a.as_bytes(), b.as_bytes());
    if a.len() != b.len() {
        return false;
    }
    let mut diff = 0u8;
    for (x, y) in a.iter().zip(b.iter()) {
        diff |= x ^ y;
    }
    diff == 0
}

#[cfg(test)]
mod tests {
    use super::*;

    fn payload(vhost: &str, exp: i64) -> SessionPayload {
        SessionPayload {
            vhost: vhost.to_string(),
            maild_token: "tok-abc".to_string(),
            email: "user@example.test".to_string(),
            iat: 1000,
            exp,
            csrf: "csrf-xyz".to_string(),
            epoch: 0,
            kind: "maild".to_string(),
            customer_id: 0,
        }
    }

    #[test]
    fn payload_without_epoch_field_defaults_to_zero() {
        // A cookie sealed BEFORE the epoch field existed must keep
        // unsealing (as epoch 0) — the serde(default) migration contract.
        let old_json = r#"{"vhost":"a.example","maild_token":"t","email":"u@example.test",
                           "iat":1000,"exp":2000,"csrf":"c"}"#;
        let p: SessionPayload = serde_json::from_str(old_json).unwrap();
        assert_eq!(p.epoch, 0);
    }

    #[test]
    fn seal_roundtrip() {
        let s = SessionSealer::ephemeral();
        let p = payload("a.example", 2000);
        let sealed = s.seal(&p).unwrap();
        let back = s.unseal(&sealed, "a.example", 1500).unwrap();
        assert_eq!(back, p);
    }

    #[test]
    fn unseal_rejects_vhost_mismatch() {
        let s = SessionSealer::ephemeral();
        let sealed = s.seal(&payload("a.example", 2000)).unwrap();
        assert!(s.unseal(&sealed, "b.example", 1500).is_none());
    }

    #[test]
    fn unseal_rejects_expired() {
        let s = SessionSealer::ephemeral();
        let sealed = s.seal(&payload("a.example", 2000)).unwrap();
        assert!(
            s.unseal(&sealed, "a.example", 2000).is_none(),
            "exp boundary is exclusive"
        );
        assert!(s.unseal(&sealed, "a.example", 2001).is_none());
    }

    #[test]
    fn unseal_rejects_wrong_key() {
        let s1 = SessionSealer::ephemeral();
        let s2 = SessionSealer::ephemeral();
        let sealed = s1.seal(&payload("a.example", 2000)).unwrap();
        assert!(
            s2.unseal(&sealed, "a.example", 1500).is_none(),
            "tag mismatch under a different key"
        );
    }

    #[test]
    fn unseal_rejects_garbage() {
        let s = SessionSealer::ephemeral();
        assert!(s.unseal("not base64!!!", "a.example", 1500).is_none());
        assert!(s.unseal("", "a.example", 1500).is_none());
        assert!(s.unseal("AAAA", "a.example", 1500).is_none());
    }

    #[test]
    fn nonce_is_fresh_per_seal() {
        let s = SessionSealer::ephemeral();
        let p = payload("a.example", 2000);
        // Same payload, two seals → different ciphertext (random nonce).
        assert_ne!(s.seal(&p).unwrap(), s.seal(&p).unwrap());
    }

    #[test]
    fn load_or_generate_persists_and_reloads() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("nested").join("session.key");
        let s1 = SessionSealer::load_or_generate(&path).unwrap();
        assert!(path.exists(), "key file created");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(&path).unwrap().permissions().mode();
            assert_eq!(mode & 0o777, 0o600, "key file is 0600");
        }
        // A second load reads the SAME key → a cookie sealed by s1
        // unseals under s2.
        let s2 = SessionSealer::load_or_generate(&path).unwrap();
        let sealed = s1.seal(&payload("a.example", 2000)).unwrap();
        assert!(s2.unseal(&sealed, "a.example", 1500).is_some());
    }

    #[test]
    fn load_rejects_wrong_length_key() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("session.key");
        std::fs::write(&path, b"too short").unwrap();
        assert!(SessionSealer::load_or_generate(&path).is_err());
    }

    #[cfg(unix)]
    #[test]
    fn load_rejects_group_world_readable_key() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("session.key");
        std::fs::write(&path, [0u8; KEY_BYTES]).unwrap();
        // 0644 — group/other readable → must be refused.
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o644)).unwrap();
        assert!(
            SessionSealer::load_or_generate(&path).is_err(),
            "an insecure-perms key file must be rejected"
        );
        // Tighten to 0600 → now accepted.
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600)).unwrap();
        assert!(SessionSealer::load_or_generate(&path).is_ok());
    }

    #[test]
    fn cookie_value_parses_list() {
        let mut h = axum::http::HeaderMap::new();
        h.insert(
            axum::http::header::COOKIE,
            "foo=1; cosmix_session=abc.def; bar=2".parse().unwrap(),
        );
        assert_eq!(
            cookie_value(&h, "cosmix_session").as_deref(),
            Some("abc.def")
        );
        assert_eq!(cookie_value(&h, "missing"), None);
    }

    #[test]
    fn csrf_eq_basic() {
        assert!(csrf_eq("abc", "abc"));
        assert!(!csrf_eq("abc", "abd"));
        assert!(!csrf_eq("abc", "abcd"));
        assert!(!csrf_eq("", "x"));
    }
}
