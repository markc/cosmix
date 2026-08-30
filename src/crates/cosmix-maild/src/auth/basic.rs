//! HTTP Basic auth helper — shared between JMAP, SMTP, and the token
//! issue endpoint. The `Authorization`-header dispatch (Basic vs Bearer)
//! lives in [`super::authenticate`]; this module owns the Basic half.

use anyhow::Result;
use base64::Engine;

use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant};

use sha2::{Digest, Sha256};

use crate::db::{self, Db};

// PERF: a short-TTL, POSITIVE-only cache of bcrypt-verified Basic credentials, so
// repeat HTTP Basic auth (native DAV / SMTP AUTH / direct-JMAP Basic clients, and
// webd's /auth/tokens/issue mint) skips the cost-12 bcrypt (~480ms) for a few
// seconds. Key = SHA-256 over a versioned (email, password) tuple — a wrong
// password is a DIFFERENT key, so a bad credential can NEVER authorise, and no
// plaintext password is retained. Only SUCCESSES are cached (a failed verify
// never creates negative-auth state an attacker could probe/poison). A password
// change / account removal is honoured after at most the TTL on Basic surfaces.
// Bearer auth (the SSR web path) never touches this. Size-capped to bound memory
// against a distinct-credential flood.
const BASIC_VERIFY_CACHE_TTL: Duration = Duration::from_secs(30);
const BASIC_VERIFY_CACHE_MAX: usize = 4096;

/// Monotonic account-auth generation, bumped on every COMMITTED account mutation
/// (`AccountsHooks::after_set`/`after_delete` → [`clear_verify_cache`]). A cache
/// entry is served only while its stamped epoch still equals this, AND a verify
/// caches a success only if no bump raced it (the epoch is unchanged between the
/// pre-DB-read observation and the insert). Together that closes the residual a
/// bare post-commit clear leaves: an in-flight bcrypt that read the OLD row just
/// before a password-change commit can no longer re-poison the cache after the
/// clear — its entry is born epoch-stale (or never inserted) and never served.
static ACCOUNT_AUTH_EPOCH: AtomicU64 = AtomicU64::new(0);

#[derive(Clone, Copy)]
struct BasicCacheEntry {
    account_id: i32,
    inserted_at: Instant,
    epoch: u64,
}

static BASIC_VERIFY_CACHE: OnceLock<Mutex<HashMap<[u8; 32], BasicCacheEntry>>> = OnceLock::new();

fn basic_verify_cache() -> &'static Mutex<HashMap<[u8; 32], BasicCacheEntry>> {
    BASIC_VERIFY_CACHE.get_or_init(|| Mutex::new(HashMap::new()))
}

/// SHA-256 over a versioned, length-delimited (email, password) tuple. The
/// version tag + the email length prevent any cross-credential confusion (e.g.
/// "a","bc" vs "ab","c" hashing the same), and the digest is non-reversible.
fn basic_cache_key(email: &str, password: &str) -> [u8; 32] {
    let mut h = Sha256::new();
    h.update(b"cosmix-maild-basic-v1\0");
    h.update((email.len() as u64).to_be_bytes());
    h.update(email.as_bytes());
    h.update(password.as_bytes());
    h.finalize().into()
}

/// Authenticate a `Basic <base64>` header value (the part after the
/// `Basic ` prefix). Returns the account id on success, `None` on any
/// malformed/incorrect credential. Used by [`super::authenticate`] and by
/// the token `issue` endpoint (which is Basic-only — you exchange a
/// password for a token, never a token for a token).
pub async fn authenticate_b64(db: &Db, b64: &str) -> Option<i32> {
    let decoded = String::from_utf8(
        base64::engine::general_purpose::STANDARD
            .decode(b64.trim())
            .ok()?,
    )
    .ok()?;
    let (email, password) = decoded.split_once(':')?;
    verify(db, email, password).await.ok()?
}

/// Verify email + password against the accounts table using bcrypt, with the
/// short-TTL positive cache above so repeat Basic auth skips the bcrypt. The
/// cache mutex is held ONLY for the brief map op — never across the DB read or
/// the bcrypt `spawn_blocking` (no lock-across-await).
pub async fn verify(db: &Db, email: &str, password: &str) -> Result<Option<i32>> {
    let key = basic_cache_key(email, password);
    // Observe the account-auth epoch BEFORE the DB read; the insert below caches
    // only if it is still unchanged afterwards (no account mutation raced us).
    let observed_epoch = ACCOUNT_AUTH_EPOCH.load(Ordering::SeqCst);

    // Fast path: a still-fresh success for this exact credential whose stamped
    // epoch is STILL current (a committed account mutation bumps the epoch, which
    // instantly invalidates every prior entry without touching the map).
    {
        let mut cache = basic_verify_cache()
            .lock()
            .map_err(|e| anyhow::anyhow!("basic auth cache lock poisoned: {e}"))?;
        if let Some(entry) = cache.get(&key).copied() {
            if entry.epoch == ACCOUNT_AUTH_EPOCH.load(Ordering::SeqCst)
                && entry.inserted_at.elapsed() < BASIC_VERIFY_CACHE_TTL
            {
                return Ok(Some(entry.account_id));
            }
            cache.remove(&key);
        }
    }

    let account = db::account::get_by_email(&db.conn, email).await?;
    let Some(a) = account else {
        return Ok(None);
    };
    let hash = a.password.clone();
    let pwd = password.to_string();
    let valid = tokio::task::spawn_blocking(move || bcrypt::verify(&pwd, &hash).unwrap_or(false))
        .await
        .unwrap_or(false);
    if !valid {
        return Ok(None);
    }

    // Cache the SUCCESS — but ONLY if no account mutation committed while we were
    // reading the row + running bcrypt (epoch unchanged since `observed_epoch`).
    // If one did, this verify may have read a now-stale row, so skip the insert
    // rather than create a reusable stale positive entry (the in-flight-bcrypt
    // race a bare post-commit clear can't catch). Drop epoch-stale/expired entries
    // first; if still at the cap, evict the oldest by inserted_at.
    if ACCOUNT_AUTH_EPOCH.load(Ordering::SeqCst) == observed_epoch {
        let mut cache = basic_verify_cache()
            .lock()
            .map_err(|e| anyhow::anyhow!("basic auth cache lock poisoned: {e}"))?;
        cache.retain(|_, entry| {
            entry.epoch == observed_epoch && entry.inserted_at.elapsed() < BASIC_VERIFY_CACHE_TTL
        });
        if cache.len() >= BASIC_VERIFY_CACHE_MAX
            && let Some(oldest) = cache
                .iter()
                .min_by_key(|(_, entry)| entry.inserted_at)
                .map(|(k, _)| *k)
        {
            cache.remove(&oldest);
        }
        cache.insert(
            key,
            BasicCacheEntry {
                account_id: a.id,
                inserted_at: Instant::now(),
                epoch: observed_epoch,
            },
        );
    }

    Ok(Some(a.id))
}

/// Invalidate every cached Basic-credential verification. Called POST-commit from
/// `AccountsHooks::after_set`/`after_delete` (NOT inside the write txn — see the
/// note there) when an account's password changes or the account is removed, so a
/// rotated/deleted credential stops authenticating on Basic surfaces AT ONCE
/// rather than lingering for up to the cache TTL.
///
/// Bumping the epoch is the load-bearing step: it atomically invalidates every
/// prior entry (each is served only while its stamped epoch matches), and because
/// [`verify`] re-checks the epoch before inserting, an in-flight bcrypt that read
/// the now-stale row can't re-poison the cache after this runs. The map clear is
/// just memory hygiene (epoch-stale entries would otherwise sit until the next
/// `retain`); a poisoned lock is recovered and cleared. Cheap — account mutations
/// are rare — so a full clear beats the index a by-email eviction would need (the
/// key is a non-reversible SHA-256 of (email,password), not searchable by email).
pub fn clear_verify_cache() {
    ACCOUNT_AUTH_EPOCH.fetch_add(1, Ordering::SeqCst);
    let Some(cache) = BASIC_VERIFY_CACHE.get() else {
        return;
    };
    match cache.lock() {
        Ok(mut guard) => guard.clear(),
        Err(poisoned) => poisoned.into_inner().clear(),
    }
}
