use axum::{
    Router, routing::{get, post},
    response::{Html, Json},
};

use super::AppState;

pub fn router() -> Router<AppState> {
    Router::new()
        .route("/mail/inbox", get(inbox))
        .route("/mail/send", post(send_mail))
}

/// Render inbox as HTML fragment (placeholder -- needs JMAP auth).
async fn inbox() -> Html<String> {
    Html(r#"<div class="mail-inbox">
        <h3>Inbox</h3>
        <p class="text-sm text-muted">JMAP integration pending -- Stalwart at localhost:8443</p>
        <p class="text-sm">To connect: configure JMAP credentials in Settings.</p>
    </div>"#.into())
}

/// Send mail via JMAP (placeholder).
async fn send_mail() -> Json<serde_json::Value> {
    Json(serde_json::json!({
        "error": "JMAP mail sending not yet implemented"
    }))
}
