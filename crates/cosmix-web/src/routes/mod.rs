pub mod auth;
mod api;
mod chat;
mod mail;
mod pages;
mod panels;
mod settings;

use anyhow::Result;
use axum::{
    Router, response::Html,
    middleware::{self, Next},
    http::{Request, StatusCode, header},
    response::{IntoResponse, Response, Redirect},
    extract::Path,
};
use sea_orm::DatabaseConnection;
use tower_sessions::SessionManagerLayer;
use tower_sessions_sqlx_store::PostgresStore;

use crate::config::WebConfig;

/// Shared application state.
#[derive(Clone)]
pub struct AppState {
    pub db: DatabaseConnection,
}

/// Build the Axum router with all routes.
pub async fn router(config: &WebConfig, db: DatabaseConnection) -> Result<Router> {
    let state = AppState { db };

    // PostgreSQL session store
    let pool = sqlx::PgPool::connect(&config.database_url).await?;
    let session_store = PostgresStore::new(pool.clone())
        .with_table_name("cosmix_sessions")
        .map_err(|e| anyhow::anyhow!("session store: {e}"))?;
    session_store.migrate().await?;

    let secure_cookies = config.tls_cert.is_some();
    let session_layer = SessionManagerLayer::new(session_store)
        .with_secure(secure_cookies)
        .with_same_site(tower_sessions::cookie::SameSite::Lax);

    // Protected API routes (require auth)
    let api_routes = Router::new()
        .nest("/panel", panels::router())
        .nest("/page", pages::router())
        .merge(api::router())
        .merge(chat::router())
        .merge(mail::router())
        .merge(settings::router())
        .layer(middleware::from_fn(require_auth));

    let app = Router::new()
        .route("/", axum::routing::get(index))
        .route("/favicon.ico", axum::routing::get(favicon))
        .route("/ws", axum::routing::get(ws_handler))
        .nest("/api", api_routes)
        .nest("/auth", auth::router())
        .route("/static/{*path}", axum::routing::get(static_file))
        .layer(session_layer)
        .with_state(state);

    Ok(app)
}

/// Auth middleware — redirects to login if no session.
async fn require_auth(
    session: tower_sessions::Session,
    request: Request<axum::body::Body>,
    next: Next,
) -> Response {
    let user_id: Option<i64> = session.get("user_id").await.unwrap_or(None);
    if user_id.is_none() {
        // For API requests, return 401
        return StatusCode::UNAUTHORIZED.into_response();
    }
    next.run(request).await
}

/// Serve the DCS shell index page with user name injected.
async fn index(session: tower_sessions::Session) -> Response {
    let user_id: Option<i64> = session.get("user_id").await.unwrap_or(None);
    if user_id.is_none() {
        return Redirect::to("/auth/login").into_response();
    }

    let user_name: String = session
        .get("user_name")
        .await
        .unwrap_or(None)
        .unwrap_or_else(|| "User".into());

    // Inject user name into the template
    let html = include_str!("../templates/index.html")
        .replace("{{user_name}}", &html_escape(&user_name));

    Html(html).into_response()
}

/// Basic HTML escaping for user-provided strings.
pub(crate) fn html_escape(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
}

/// Serve favicon.
async fn favicon() -> Response {
    let svg = include_bytes!("../../static/favicon.svg");
    ([(header::CONTENT_TYPE, "image/svg+xml"), (header::CACHE_CONTROL, "public, max-age=86400")], svg.as_slice()).into_response()
}

/// Stub WebSocket handler — keeps connection alive for HTMX ws extension.
async fn ws_handler(
    ws: axum::extract::WebSocketUpgrade,
) -> Response {
    ws.on_upgrade(|mut socket| async move {
        // Keep alive until client disconnects
        while let Some(Ok(_)) = socket.recv().await {}
    })
}

/// Serve embedded static files, falling back to public/ on disk.
async fn static_file(Path(path): Path<String>) -> Response {
    // Embedded app assets (compiled into binary)
    let embedded: Option<(&[u8], &str)> = match path.as_str() {
        "base.css" => Some((include_bytes!("../../static/base.css").as_slice(), "text/css")),
        "app.css" => Some((include_bytes!("../../static/app.css").as_slice(), "text/css")),
        "base.js" => Some((include_bytes!("../../static/base.js").as_slice(), "text/javascript")),
        "app.js" => Some((include_bytes!("../../static/app.js").as_slice(), "text/javascript")),
        "htmx.min.js" => Some((include_bytes!("../../static/htmx.min.js").as_slice(), "text/javascript")),
        "ext/ws.js" => Some((include_bytes!("../../static/ext/ws.js").as_slice(), "text/javascript")),
        "ext/sse.js" => Some((include_bytes!("../../static/ext/sse.js").as_slice(), "text/javascript")),
        _ => None,
    };

    if let Some((content, mime)) = embedded {
        return ([(header::CONTENT_TYPE, mime), (header::CACHE_CONTROL, "public, max-age=3600")], content).into_response();
    }

    // Filesystem fallback: serve from public/ directory
    serve_from_public(&path).await
}

/// Serve a file from the public/ directory with path traversal protection.
async fn serve_from_public(path: &str) -> Response {
    use std::path::PathBuf;

    let public_dir = PathBuf::from("public");
    let file_path = public_dir.join(path);

    // Prevent path traversal
    let canonical = match file_path.canonicalize() {
        Ok(p) => p,
        Err(_) => return StatusCode::NOT_FOUND.into_response(),
    };
    let public_canonical = match public_dir.canonicalize() {
        Ok(p) => p,
        Err(_) => return StatusCode::NOT_FOUND.into_response(),
    };
    if !canonical.starts_with(&public_canonical) {
        return StatusCode::FORBIDDEN.into_response();
    }

    let content = match tokio::fs::read(&canonical).await {
        Ok(c) => c,
        Err(_) => return StatusCode::NOT_FOUND.into_response(),
    };

    let mime = mime_from_ext(canonical.extension().and_then(|e| e.to_str()).unwrap_or(""));
    ([(header::CONTENT_TYPE, mime), (header::CACHE_CONTROL, "public, max-age=3600")], content).into_response()
}

fn mime_from_ext(ext: &str) -> &'static str {
    match ext {
        "html" | "htm" => "text/html",
        "css" => "text/css",
        "js" | "mjs" => "text/javascript",
        "json" => "application/json",
        "png" => "image/png",
        "jpg" | "jpeg" => "image/jpeg",
        "gif" => "image/gif",
        "svg" => "image/svg+xml",
        "ico" => "image/x-icon",
        "webp" => "image/webp",
        "woff" => "font/woff",
        "woff2" => "font/woff2",
        "ttf" => "font/ttf",
        "pdf" => "application/pdf",
        "txt" => "text/plain",
        "xml" => "application/xml",
        "wasm" => "application/wasm",
        _ => "application/octet-stream",
    }
}
