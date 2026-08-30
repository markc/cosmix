//! DAV auth — reuses `auth::authenticate` (Basic or Bearer) and adds the
//! `WWW-Authenticate` challenge a DAV client needs (the JMAP path returns
//! 401 JSON with no challenge header, so a DAV client never prompts for
//! credentials).

use std::sync::Arc;

use axum::body::Body;
use axum::http::{HeaderMap, StatusCode, header};
use axum::response::Response;

use crate::jmap::AppState;

/// Authenticate a DAV request, returning the numeric account id.
/// Identical credential check to JMAP — same `auth::authenticate`
/// dispatch (DAV clients send Basic; Bearer is accepted too for parity).
pub async fn authenticate(state: &Arc<AppState>, headers: &HeaderMap) -> Option<i32> {
    crate::auth::authenticate(&state.db, headers).await
}

/// `401 Unauthorized` with a Basic challenge so the client prompts.
pub fn challenge() -> Response {
    Response::builder()
        .status(StatusCode::UNAUTHORIZED)
        .header(header::WWW_AUTHENTICATE, "Basic realm=\"maild\"")
        .body(Body::empty())
        .expect("static challenge response is always valid")
}
