//! Authentication module.
//!
//! [`authenticate`] is the single entry point for the JMAP/DAV surfaces:
//! it dispatches on the `Authorization` scheme, accepting **both** HTTP
//! Basic (email:password, bcrypt) and Bearer (an opaque `account_tokens`
//! token). `/jmap` and the DAV routes are structurally unchanged — they
//! call this and get back an account id either way, so a browser session
//! forwarding `Bearer <token>` (SSR PIM Phase 2) and a native client
//! sending Basic both authorise through one truth.

use axum::http::HeaderMap;

use crate::db::Db;

pub mod basic;
pub mod tokens;

/// Resolve an `Authorization` header to an account id, accepting Basic or
/// Bearer. Returns `None` when the header is absent, malformed, an
/// unknown scheme, or the credential does not validate.
pub async fn authenticate(db: &Db, headers: &HeaderMap) -> Option<i32> {
    let auth = headers.get("authorization")?.to_str().ok()?;
    if let Some(b64) = auth.strip_prefix("Basic ") {
        return basic::authenticate_b64(db, b64).await;
    }
    if let Some(token) = auth.strip_prefix("Bearer ") {
        return tokens::authenticate_bearer(db, token.trim()).await;
    }
    None
}
