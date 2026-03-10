//! HTTP Basic auth helper — shared between JMAP and SMTP.

use anyhow::Result;
use base64::Engine;

use crate::db::{self, Db};

/// Authenticate via HTTP Basic auth header. Returns account_id on success.
pub async fn authenticate(db: &Db, headers: &axum::http::HeaderMap) -> Option<i32> {
    let auth = headers.get("authorization")?.to_str().ok()?;
    if !auth.starts_with("Basic ") {
        return None;
    }

    let decoded = String::from_utf8(
        base64::engine::general_purpose::STANDARD
            .decode(&auth[6..])
            .ok()?,
    )
    .ok()?;

    let (email, password) = decoded.split_once(':')?;
    verify(db, email, password).await.ok()?
}

/// Verify email + password against the accounts table.
pub async fn verify(db: &Db, email: &str, password: &str) -> Result<Option<i32>> {
    let account = db::account::get_by_email(&db.pool, email).await?;
    match account {
        // Simple plaintext comparison for now — will add bcrypt/argon2 in hardening phase
        Some(a) if a.password == password => Ok(Some(a.id)),
        _ => Ok(None),
    }
}
