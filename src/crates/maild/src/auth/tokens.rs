//! Opaque bearer tokens for JMAP auth (SSR PIM Phase 2).
//!
//! Three HTTP endpoints on the maild router, beside the `/jmap` service
//! that consumes the tokens (deliberately HTTP, not Bus verbs — this is
//! per-browser login traffic, not operator/admin RPC):
//!
//! * `POST /auth/tokens/issue`  — **Basic**-authenticated; mints a token,
//!   returns the plaintext **once**.
//! * `POST /auth/tokens/verify` — **Bearer**-authenticated; echoes the
//!   bound account (debug/health; webd never calls this — it forwards the
//!   `Bearer` to `/jmap` directly so maild stays the one auth truth).
//! * `POST /auth/tokens/revoke` — **Bearer**-authenticated; self-revokes
//!   the presented token (webd `/logout`).
//!
//! The plaintext token is 32 cryptographically-random bytes, base64url
//! (no pad). Only its SHA-256 (hex) is stored; see [`crate::db::token`].

use std::sync::Arc;

use axum::Json;
use axum::extract::State;
use axum::http::{HeaderMap, StatusCode};
use axum::response::IntoResponse;
use base64::Engine;
use sha2::{Digest, Sha256};

use crate::db::{self, Db};
use crate::jmap::AppState;

/// Default token lifetime (D2.3). A login cookie outliving this forces a
/// re-login; maild remains the revocation authority in between.
const TOKEN_TTL_DAYS: i64 = 30;

/// Default `/auth/tokens/issue` lifetime in seconds (the 30-day default above)
/// — the fallback when a caller supplies no `ttl_secs`.
const TOKEN_TTL_SECS_DEFAULT: i64 = TOKEN_TTL_DAYS * 86_400;
/// Bounds for a caller-supplied `ttl_secs` override: at least a minute (shorter
/// is just re-mint thrash) and at most the 30-day default — a caller can shorten
/// a token's life (webd's service seams want ~1h) but never extend it past
/// policy.
const TOKEN_TTL_SECS_MIN: i64 = 60;
const TOKEN_TTL_SECS_MAX: i64 = TOKEN_TTL_DAYS * 86_400;

/// Plaintext token entropy in bytes (256 bits — within D2.3's 32–48).
const TOKEN_BYTES: usize = 32;

/// Generate a fresh opaque token: `TOKEN_BYTES` from the OS CSPRNG,
/// base64url-encoded without padding (URL-safe so it survives headers,
/// cookies, and query strings unescaped).
fn generate_token() -> String {
    let mut buf = [0u8; TOKEN_BYTES];
    rand::RngCore::fill_bytes(&mut rand::rngs::OsRng, &mut buf);
    base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(buf)
}

/// Hex SHA-256 of a token's bytes — the value stored and looked up. SHA-256
/// (not bcrypt) because the input is a full-entropy 256-bit token, not a
/// low-entropy password, so a fast hash is appropriate; lookup is an
/// indexed equality on this digest.
fn hash_token(token: &str) -> String {
    let digest = Sha256::digest(token.as_bytes());
    hex::encode(digest)
}

/// Verify a Bearer token → account id. Hashes the presented token and
/// resolves it against live (`account_tokens`) rows. The entry point used
/// by [`crate::auth::authenticate`] for the `Bearer` scheme.
pub async fn authenticate_bearer(db: &Db, token: &str) -> Option<i32> {
    if token.is_empty() {
        return None;
    }
    let hash = hash_token(token);
    db::token::lookup_valid(&db.conn, &hash)
        .await
        .ok()
        .flatten()
        .map(|v| v.account_id)
}

/// `POST /auth/tokens/issue` — exchange Basic credentials for a token.
/// Body is optional `{"label": "...", "ttl_secs": N}` — `label` a human tag
/// (device/service), `ttl_secs` an optional shorter lifetime (clamped to
/// `[TOKEN_TTL_SECS_MIN, TOKEN_TTL_SECS_MAX]`; webd's dev_session/public_read
/// seams pass ~1h to bound their cached-Bearer revocation tail). A
/// missing/empty/invalid body or absent field falls back to the defaults (no
/// label, 30-day TTL), so a bare POST still works.
pub async fn issue(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    body: axum::body::Bytes,
) -> impl IntoResponse {
    let Some(b64) = headers
        .get("authorization")
        .and_then(|h| h.to_str().ok())
        .and_then(|h| h.strip_prefix("Basic "))
    else {
        return unauthorized();
    };
    let Some(account_id) = crate::auth::basic::authenticate_b64(&state.db, b64).await else {
        return unauthorized();
    };

    // Parse the optional JSON body ONCE, tolerant of an absent/empty/non-JSON
    // body — both fields fall back to defaults.
    let parsed: Option<serde_json::Value> = if body.is_empty() {
        None
    } else {
        serde_json::from_slice::<serde_json::Value>(&body).ok()
    };
    let label: Option<String> = parsed.as_ref().and_then(|v| {
        v.get("label")
            .and_then(|l| l.as_str())
            .map(|s| s.to_string())
    });
    // A caller-supplied ttl_secs is clamped into policy bounds; absent/invalid
    // keeps the 30-day default (normal user-login behaviour).
    let ttl_secs = parsed
        .as_ref()
        .and_then(|v| v.get("ttl_secs").and_then(|t| t.as_i64()))
        .map(|t| t.clamp(TOKEN_TTL_SECS_MIN, TOKEN_TTL_SECS_MAX))
        .unwrap_or(TOKEN_TTL_SECS_DEFAULT);

    let token = generate_token();
    let hash = hash_token(&token);
    match db::token::insert(
        &state.db.conn,
        account_id,
        &hash,
        label.as_deref(),
        ttl_secs,
    )
    .await
    {
        Ok(issued) => (
            StatusCode::OK,
            Json(serde_json::json!({
                "token": token,
                "account_id": issued.account_id,
                "issued_at": issued.issued_at,
                "expires_at": issued.expires_at,
            })),
        )
            .into_response(),
        Err(e) => {
            tracing::error!(error = %e, "token issue: insert failed");
            internal_error()
        }
    }
}

/// `POST /auth/tokens/verify` — Bearer-authenticated; reports the bound
/// account. Returns 401 for an absent/unknown/expired/revoked token.
pub async fn verify(State(state): State<Arc<AppState>>, headers: HeaderMap) -> impl IntoResponse {
    let Some(token) = bearer(&headers) else {
        return unauthorized();
    };
    let hash = hash_token(token);
    match db::token::lookup_valid(&state.db.conn, &hash).await {
        Ok(Some(v)) => (
            StatusCode::OK,
            Json(serde_json::json!({
                "account_id": v.account_id,
                "email": v.email,
                "expires_at": v.expires_at,
            })),
        )
            .into_response(),
        Ok(None) => unauthorized(),
        Err(e) => {
            tracing::error!(error = %e, "token verify: lookup failed");
            internal_error()
        }
    }
}

/// `POST /auth/tokens/revoke` — Bearer-authenticated; self-revokes the
/// presented token. Idempotent (a re-revoke is still 204). A never-issued
/// token is 401 (it isn't "yours" to revoke); a known-but-expired token
/// still revokes (and returns 204) so logout always clears server state.
pub async fn revoke(State(state): State<Arc<AppState>>, headers: HeaderMap) -> impl IntoResponse {
    let Some(token) = bearer(&headers) else {
        return unauthorized();
    };
    let hash = hash_token(token);
    match db::token::revoke(&state.db.conn, &hash).await {
        Ok(true) => StatusCode::NO_CONTENT.into_response(),
        Ok(false) => unauthorized(),
        Err(e) => {
            tracing::error!(error = %e, "token revoke: update failed");
            internal_error()
        }
    }
}

/// Extract a non-empty `Bearer <token>` from the header map.
fn bearer(headers: &HeaderMap) -> Option<&str> {
    let token = headers
        .get("authorization")?
        .to_str()
        .ok()?
        .strip_prefix("Bearer ")?
        .trim();
    if token.is_empty() { None } else { Some(token) }
}

fn unauthorized() -> axum::response::Response {
    (
        StatusCode::UNAUTHORIZED,
        Json(serde_json::json!({"error": "unauthorized"})),
    )
        .into_response()
}

fn internal_error() -> axum::response::Response {
    (
        StatusCode::INTERNAL_SERVER_ERROR,
        Json(serde_json::json!({"error": "internal error"})),
    )
        .into_response()
}
