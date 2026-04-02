#[global_allocator]
static GLOBAL: mimalloc::MiMalloc = mimalloc::MiMalloc;

use std::path::{Path as StdPath, PathBuf};
use std::sync::Arc;

use anyhow::Result;
use axum::Router;
use axum::extract::{Path, State};
use axum::extract::ws::{Message as AxumMessage, WebSocket, WebSocketUpgrade};
use axum::http::StatusCode;
use axum::response::{Html, IntoResponse, Json, Response};
use clap::{Parser, Subcommand};
use futures_util::{SinkExt, StreamExt};
use pulldown_cmark::{Options, Parser as MdParser};
use rusqlite::Connection;
use serde::{Deserialize, Serialize};
use tokio::sync::Mutex;
use tokio_tungstenite::tungstenite::Message as TungMessage;
use tower_http::services::ServeDir;
use tracing::info;

// ---------------------------------------------------------------------------
// CLI
// ---------------------------------------------------------------------------

#[derive(Parser)]
#[command(name = "cosmix-web", about = "Lightweight web server for cosmix WASM apps + CMS API")]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Start the web server
    Serve {
        /// Listen address
        #[arg(long, default_value = "0.0.0.0:8080")]
        listen: String,

        /// Directory containing pre-built Dioxus WASM apps
        #[arg(long, default_value = "/var/lib/cosmix/www")]
        www_dir: PathBuf,

        /// Path to the SQLite database
        #[arg(long, default_value = "/var/lib/cosmix/web.db")]
        db_path: PathBuf,

        /// Upstream JMAP server to reverse-proxy to
        #[arg(long, default_value = "http://127.0.0.1:8080")]
        jmap_upstream: String,

        /// Upstream hub WebSocket URL (proxied at /ws for WASM apps)
        #[arg(long, default_value = "ws://localhost:4200/ws")]
        hub_ws: String,

        /// Directory of markdown files to serve at /docs/
        #[arg(long)]
        docs_dir: Option<PathBuf>,

        /// TLS certificate file (PEM). Enables HTTPS when set.
        #[arg(long)]
        tls_cert: Option<PathBuf>,

        /// TLS private key file (PEM)
        #[arg(long)]
        tls_key: Option<PathBuf>,
    },
    /// Initialise the SQLite database
    Init {
        /// Path to the SQLite database
        #[arg(long, default_value = "/var/lib/cosmix/web.db")]
        db_path: PathBuf,
    },
}

// ---------------------------------------------------------------------------
// Application state
// ---------------------------------------------------------------------------

struct AppState {
    db: Mutex<Connection>,
    jmap_upstream: String,
    hub_ws: String,
    http_client: reqwest::Client,
    docs_dir: Option<PathBuf>,
}

// ---------------------------------------------------------------------------
// Post model
// ---------------------------------------------------------------------------

#[derive(Debug, Serialize, Deserialize)]
struct Post {
    id: i64,
    slug: String,
    title: String,
    content: String,
    published: bool,
    created: String,
    updated: String,
}

#[derive(Debug, Deserialize)]
struct CreatePost {
    slug: String,
    title: String,
    content: String,
    #[serde(default)]
    published: bool,
}

#[derive(Debug, Deserialize)]
struct UpdatePost {
    slug: Option<String>,
    title: Option<String>,
    content: Option<String>,
    published: Option<bool>,
}

// ---------------------------------------------------------------------------
// Error handling
// ---------------------------------------------------------------------------

struct AppError(anyhow::Error);

impl IntoResponse for AppError {
    fn into_response(self) -> Response {
        let body = serde_json::json!({ "error": self.0.to_string() });
        (StatusCode::INTERNAL_SERVER_ERROR, Json(body)).into_response()
    }
}

impl<E: Into<anyhow::Error>> From<E> for AppError {
    fn from(err: E) -> Self {
        Self(err.into())
    }
}

// ---------------------------------------------------------------------------
// Database
// ---------------------------------------------------------------------------

const SCHEMA: &str = r#"
CREATE TABLE IF NOT EXISTS posts (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    slug TEXT UNIQUE NOT NULL,
    title TEXT NOT NULL,
    content TEXT NOT NULL,
    published INTEGER NOT NULL DEFAULT 0,
    created TEXT NOT NULL DEFAULT (datetime('now')),
    updated TEXT NOT NULL DEFAULT (datetime('now'))
);
"#;

fn open_db(path: &std::path::Path) -> Result<Connection> {
    let conn = Connection::open(path)?;
    conn.execute_batch("PRAGMA journal_mode=WAL; PRAGMA foreign_keys=ON;")?;
    Ok(conn)
}

fn init_db(path: &std::path::Path) -> Result<()> {
    let conn = open_db(path)?;
    conn.execute_batch(SCHEMA)?;
    info!("database initialised at {}", path.display());
    Ok(())
}

// ---------------------------------------------------------------------------
// API handlers
// ---------------------------------------------------------------------------

async fn list_posts(State(state): State<Arc<AppState>>) -> Result<Json<Vec<Post>>, AppError> {
    let db = state.db.lock().await;
    let posts = tokio::task::block_in_place(|| {
        let mut stmt = db.prepare(
            "SELECT id, slug, title, content, published, created, updated FROM posts ORDER BY created DESC",
        )?;
        let rows = stmt.query_map([], |row| {
            Ok(Post {
                id: row.get(0)?,
                slug: row.get(1)?,
                title: row.get(2)?,
                content: row.get(3)?,
                published: row.get::<_, i64>(4)? != 0,
                created: row.get(5)?,
                updated: row.get(6)?,
            })
        })?;
        rows.collect::<Result<Vec<_>, _>>()
    })?;
    Ok(Json(posts))
}

async fn get_post(
    State(state): State<Arc<AppState>>,
    Path(id): Path<i64>,
) -> Result<Response, AppError> {
    let db = state.db.lock().await;
    let result = tokio::task::block_in_place(|| {
        db.query_row(
            "SELECT id, slug, title, content, published, created, updated FROM posts WHERE id = ?1",
            [id],
            |row| {
                Ok(Post {
                    id: row.get(0)?,
                    slug: row.get(1)?,
                    title: row.get(2)?,
                    content: row.get(3)?,
                    published: row.get::<_, i64>(4)? != 0,
                    created: row.get(5)?,
                    updated: row.get(6)?,
                })
            },
        )
    });
    match result {
        Ok(post) => Ok(Json(post).into_response()),
        Err(rusqlite::Error::QueryReturnedNoRows) => {
            let body = serde_json::json!({ "error": "not found" });
            Ok((StatusCode::NOT_FOUND, Json(body)).into_response())
        }
        Err(e) => Err(AppError(e.into())),
    }
}

async fn create_post(
    State(state): State<Arc<AppState>>,
    Json(input): Json<CreatePost>,
) -> Result<Response, AppError> {
    let db = state.db.lock().await;
    let post = tokio::task::block_in_place(|| -> Result<Post> {
        db.execute(
            "INSERT INTO posts (slug, title, content, published) VALUES (?1, ?2, ?3, ?4)",
            rusqlite::params![input.slug, input.title, input.content, input.published as i64],
        )?;
        let id = db.last_insert_rowid();
        db.query_row(
            "SELECT id, slug, title, content, published, created, updated FROM posts WHERE id = ?1",
            [id],
            |row| {
                Ok(Post {
                    id: row.get(0)?,
                    slug: row.get(1)?,
                    title: row.get(2)?,
                    content: row.get(3)?,
                    published: row.get::<_, i64>(4)? != 0,
                    created: row.get(5)?,
                    updated: row.get(6)?,
                })
            },
        )
        .map_err(Into::into)
    })?;
    Ok((StatusCode::CREATED, Json(post)).into_response())
}

async fn update_post(
    State(state): State<Arc<AppState>>,
    Path(id): Path<i64>,
    Json(input): Json<UpdatePost>,
) -> Result<Response, AppError> {
    let db = state.db.lock().await;
    let result = tokio::task::block_in_place(|| -> Result<Option<Post>> {
        let mut sets = Vec::new();
        let mut params: Vec<Box<dyn rusqlite::types::ToSql>> = Vec::new();

        if let Some(ref slug) = input.slug {
            sets.push("slug = ?");
            params.push(Box::new(slug.clone()));
        }
        if let Some(ref title) = input.title {
            sets.push("title = ?");
            params.push(Box::new(title.clone()));
        }
        if let Some(ref content) = input.content {
            sets.push("content = ?");
            params.push(Box::new(content.clone()));
        }
        if let Some(published) = input.published {
            sets.push("published = ?");
            params.push(Box::new(published as i64));
        }

        if sets.is_empty() {
            // Nothing to update — just return the existing post
            let post = db
                .query_row(
                    "SELECT id, slug, title, content, published, created, updated FROM posts WHERE id = ?1",
                    [id],
                    |row| {
                        Ok(Post {
                            id: row.get(0)?,
                            slug: row.get(1)?,
                            title: row.get(2)?,
                            content: row.get(3)?,
                            published: row.get::<_, i64>(4)? != 0,
                            created: row.get(5)?,
                            updated: row.get(6)?,
                        })
                    },
                )
                .optional()?;
            return Ok(post);
        }

        sets.push("updated = datetime('now')");
        params.push(Box::new(id));

        let sql = format!(
            "UPDATE posts SET {} WHERE id = ?",
            sets.join(", ")
        );
        let param_refs: Vec<&dyn rusqlite::types::ToSql> = params.iter().map(|p| p.as_ref()).collect();
        let changed = db.execute(&sql, param_refs.as_slice())?;

        if changed == 0 {
            return Ok(None);
        }

        let post = db.query_row(
            "SELECT id, slug, title, content, published, created, updated FROM posts WHERE id = ?1",
            [id],
            |row| {
                Ok(Post {
                    id: row.get(0)?,
                    slug: row.get(1)?,
                    title: row.get(2)?,
                    content: row.get(3)?,
                    published: row.get::<_, i64>(4)? != 0,
                    created: row.get(5)?,
                    updated: row.get(6)?,
                })
            },
        )?;
        Ok(Some(post))
    })?;

    match result {
        Some(post) => Ok(Json(post).into_response()),
        None => {
            let body = serde_json::json!({ "error": "not found" });
            Ok((StatusCode::NOT_FOUND, Json(body)).into_response())
        }
    }
}

async fn delete_post(
    State(state): State<Arc<AppState>>,
    Path(id): Path<i64>,
) -> Result<Response, AppError> {
    let db = state.db.lock().await;
    let changed = tokio::task::block_in_place(|| {
        db.execute("DELETE FROM posts WHERE id = ?1", [id])
    })?;
    if changed == 0 {
        let body = serde_json::json!({ "error": "not found" });
        Ok((StatusCode::NOT_FOUND, Json(body)).into_response())
    } else {
        Ok(StatusCode::NO_CONTENT.into_response())
    }
}

// ---------------------------------------------------------------------------
// JMAP reverse proxy
// ---------------------------------------------------------------------------

async fn jmap_proxy(
    State(state): State<Arc<AppState>>,
    req: axum::extract::Request,
) -> Result<Response, AppError> {
    let path = req.uri().path().to_string();
    let query = req.uri().query().map(|q| format!("?{q}")).unwrap_or_default();
    let method = req.method().clone();
    let headers = req.headers().clone();
    let body = axum::body::to_bytes(req.into_body(), 10 * 1024 * 1024).await?;

    let upstream_url = format!("{}{}{}", state.jmap_upstream, path, query);

    let mut upstream_req = state.http_client.request(method, &upstream_url);
    for (name, value) in &headers {
        if name == axum::http::header::HOST {
            continue;
        }
        upstream_req = upstream_req.header(name, value);
    }
    upstream_req = upstream_req.body(body);

    let resp = upstream_req.send().await?;
    let status = StatusCode::from_u16(resp.status().as_u16())?;
    let resp_headers = resp.headers().clone();
    let resp_body = resp.bytes().await?;

    let mut response = (status, resp_body).into_response();
    for (name, value) in &resp_headers {
        response.headers_mut().insert(name, value.clone());
    }
    Ok(response)
}

// ---------------------------------------------------------------------------
// WebSocket proxy to hub (for WASM apps)
// ---------------------------------------------------------------------------

async fn ws_proxy_handler(
    ws: WebSocketUpgrade,
    State(state): State<Arc<AppState>>,
) -> impl IntoResponse {
    let hub_url = state.hub_ws.clone();
    ws.on_upgrade(move |browser_ws| ws_proxy(browser_ws, hub_url))
}

async fn ws_proxy(browser_ws: WebSocket, hub_url: String) {
    // Connect to the upstream hub
    let hub_conn = match tokio_tungstenite::connect_async(&hub_url).await {
        Ok((stream, _)) => stream,
        Err(e) => {
            tracing::warn!(error = %e, "Failed to connect to hub for WS proxy");
            return;
        }
    };

    let (mut browser_sink, mut browser_stream) = browser_ws.split();
    let (mut hub_sink, mut hub_stream) = hub_conn.split();

    // Browser → Hub
    let browser_to_hub = async {
        while let Some(Ok(msg)) = browser_stream.next().await {
            let tung_msg = match msg {
                AxumMessage::Text(t) => TungMessage::Text(t.to_string().into()),
                AxumMessage::Binary(b) => TungMessage::Binary(b.into()),
                AxumMessage::Close(_) => break,
                AxumMessage::Ping(p) => TungMessage::Ping(p.into()),
                AxumMessage::Pong(p) => TungMessage::Pong(p.into()),
            };
            if hub_sink.send(tung_msg).await.is_err() {
                break;
            }
        }
    };

    // Hub → Browser
    let hub_to_browser = async {
        while let Some(Ok(msg)) = hub_stream.next().await {
            let axum_msg = match msg {
                TungMessage::Text(t) => AxumMessage::Text(t.to_string().into()),
                TungMessage::Binary(b) => AxumMessage::Binary(b.into()),
                TungMessage::Close(_) => break,
                TungMessage::Ping(p) => AxumMessage::Ping(p.into()),
                TungMessage::Pong(p) => AxumMessage::Pong(p.into()),
                _ => continue,
            };
            if browser_sink.send(axum_msg).await.is_err() {
                break;
            }
        }
    };

    tokio::select! {
        _ = browser_to_hub => {}
        _ = hub_to_browser => {}
    }
}

// ---------------------------------------------------------------------------
// Markdown docs handler
// ---------------------------------------------------------------------------

/// Build a sidebar navigation from the docs directory structure.
fn build_sidebar(docs_dir: &StdPath, current_path: &str) -> String {
    let mut sections: Vec<(String, Vec<(String, String)>)> = Vec::new();

    if let Ok(entries) = std::fs::read_dir(docs_dir) {
        let mut dirs: Vec<_> = entries.filter_map(|e| e.ok()).collect();
        dirs.sort_by_key(|e| e.file_name());

        for entry in &dirs {
            let name = entry.file_name().to_string_lossy().to_string();
            let path = entry.path();
            if path.is_dir() && !name.starts_with('.') {
                // Section title: strip numeric prefix "00-getting-started" -> "Getting Started"
                let title = name
                    .trim_start_matches(|c: char| c.is_ascii_digit() || c == '-')
                    .replace(['-', '_'], " ");
                let title = title
                    .split_whitespace()
                    .map(|w| {
                        let mut c = w.chars();
                        match c.next() {
                            None => String::new(),
                            Some(f) => f.to_uppercase().to_string() + c.as_str(),
                        }
                    })
                    .collect::<Vec<_>>()
                    .join(" ");

                let mut pages = Vec::new();
                if let Ok(files) = std::fs::read_dir(&path) {
                    let mut files: Vec<_> = files.filter_map(|e| e.ok()).collect();
                    files.sort_by_key(|e| e.file_name());
                    for file in &files {
                        let fname = file.file_name().to_string_lossy().to_string();
                        if fname.ends_with(".md") {
                            let slug = fname.trim_end_matches(".md");
                            let href = format!("/docs/{name}/{slug}");
                            let page_title = if slug == "index" {
                                "Overview".to_string()
                            } else {
                                slug.replace(['-', '_'], " ")
                            };
                            pages.push((href, page_title));
                        }
                    }
                }
                if !pages.is_empty() {
                    sections.push((title, pages));
                }
            }
        }
    }

    let mut html = String::from("<nav class=\"sidebar\">\n<h2><a href=\"/docs\">Docs</a></h2>\n");
    for (title, pages) in &sections {
        html.push_str(&format!("<details{}>\n<summary>{title}</summary>\n<ul>\n",
            if pages.iter().any(|(href, _)| href.trim_end_matches("/index") == format!("/docs/{}", current_path.split('/').next().unwrap_or(""))) { " open" } else { "" }
        ));
        for (href, page_title) in pages {
            let active = if current_path == href.trim_start_matches("/docs/") { " class=\"active\"" } else { "" };
            html.push_str(&format!("<li{active}><a href=\"{href}\">{page_title}</a></li>\n"));
        }
        html.push_str("</ul>\n</details>\n");
    }
    html.push_str("</nav>\n");
    html
}

/// Resolve a docs path to its raw markdown content.
fn resolve_markdown_path(docs_dir: &StdPath, rel_path: &str) -> Option<String> {
    let candidates = [
        docs_dir.join(format!("{rel_path}.md")),
        docs_dir.join(rel_path).join("index.md"),
        docs_dir.join(rel_path),
    ];

    let file_path = candidates.iter().find(|p| p.is_file())?;

    // Security: ensure resolved path is under docs_dir
    let canonical = file_path.canonicalize().ok()?;
    let docs_canonical = docs_dir.canonicalize().ok()?;
    if !canonical.starts_with(&docs_canonical) {
        return None;
    }

    std::fs::read_to_string(&canonical).ok()
}

/// Render a markdown file to a full HTML page.
fn render_markdown(docs_dir: &StdPath, rel_path: &str) -> Option<String> {
    let content = resolve_markdown_path(docs_dir, rel_path)?;

    // Parse markdown
    let opts = Options::ENABLE_TABLES
        | Options::ENABLE_STRIKETHROUGH
        | Options::ENABLE_TASKLISTS
        | Options::ENABLE_HEADING_ATTRIBUTES;
    let parser = MdParser::new_ext(&content, opts);
    let mut body_html = String::new();
    pulldown_cmark::html::push_html(&mut body_html, parser);

    // Convert <img> tags pointing to .mp4/.webm to <video> tags
    let video_re = regex_lite::Regex::new(
        r#"<img src="([^"]+\.(?:mp4|webm|mov))" alt="([^"]*)"(?: /)?>"#
    ).unwrap();
    body_html = video_re.replace_all(&body_html, |caps: &regex_lite::Captures| {
        let src = &caps[1];
        let alt = &caps[2];
        format!(r#"<video src="{src}" alt="{alt}" controls muted autoplay loop style="max-width:100%;border-radius:0.5rem;margin:1rem 0"></video>"#)
    }).to_string();

    // Extract title from first <h1>
    let title = content
        .lines()
        .find(|l| l.starts_with("# "))
        .map(|l| l.trim_start_matches("# "))
        .unwrap_or("Docs");

    let sidebar = build_sidebar(docs_dir, rel_path);

    Some(format!(
        r#"<!DOCTYPE html>
<html lang="en">
<head>
<meta charset="utf-8">
<meta name="viewport" content="width=device-width, initial-scale=1">
<title>{title}</title>
<style>
  :root {{
    --bg: #1a1a2e; --fg: #e0e0e0; --sidebar-bg: #16213e; --accent: #0f3460;
    --link: #6cb4ee; --code-bg: #0d1117; --border: #2a2a4a;
    --active-bg: #0f3460; --hover-bg: #1a1a3e;
  }}
  @media (prefers-color-scheme: light) {{
    :root {{
      --bg: #fff; --fg: #1a1a1a; --sidebar-bg: #f5f5f5; --accent: #e8e8e8;
      --link: #0366d6; --code-bg: #f6f8fa; --border: #d0d0d0;
      --active-bg: #e2e8f0; --hover-bg: #edf2f7;
    }}
  }}
  * {{ margin: 0; padding: 0; box-sizing: border-box; }}
  body {{
    font-family: -apple-system, BlinkMacSystemFont, "Segoe UI", Roboto, sans-serif;
    background: var(--bg); color: var(--fg); line-height: 1.6;
    display: flex; min-height: 100vh;
  }}
  .sidebar {{
    width: 16rem; min-height: 100vh; padding: 1.5rem 1rem;
    background: var(--sidebar-bg); border-right: 1px solid var(--border);
    overflow-y: auto; flex-shrink: 0; position: sticky; top: 0;
    max-height: 100vh;
  }}
  .sidebar h2 {{ margin-bottom: 1rem; font-size: 1.25rem; }}
  .sidebar h2 a {{ color: var(--fg); text-decoration: none; }}
  .sidebar details {{ margin-bottom: 0.25rem; }}
  .sidebar summary {{
    cursor: pointer; padding: 0.3rem 0.5rem; font-weight: 600;
    font-size: 0.85rem; text-transform: uppercase; letter-spacing: 0.05em;
    color: var(--fg); opacity: 0.7;
  }}
  .sidebar ul {{ list-style: none; padding-left: 0.5rem; }}
  .sidebar li {{ margin: 0.1rem 0; }}
  .sidebar li a {{
    display: block; padding: 0.2rem 0.5rem; color: var(--link);
    text-decoration: none; font-size: 0.85rem; border-radius: 0.25rem;
  }}
  .sidebar li a:hover {{ background: var(--hover-bg); }}
  .sidebar li.active a {{ background: var(--active-bg); font-weight: 600; }}
  .content {{
    flex: 1; max-width: 52rem; padding: 2rem 3rem; min-width: 0;
  }}
  .content h1 {{ font-size: 2rem; margin-bottom: 1rem; border-bottom: 1px solid var(--border); padding-bottom: 0.5rem; }}
  .content h2 {{ font-size: 1.5rem; margin: 2rem 0 0.75rem; }}
  .content h3 {{ font-size: 1.2rem; margin: 1.5rem 0 0.5rem; }}
  .content p {{ margin: 0.75rem 0; }}
  .content a {{ color: var(--link); }}
  .content img {{ max-width: 100%; border-radius: 0.5rem; margin: 1rem 0; }}
  .content ul, .content ol {{ margin: 0.75rem 0; padding-left: 1.5rem; }}
  .content li {{ margin: 0.25rem 0; }}
  .content table {{ border-collapse: collapse; width: 100%; margin: 1rem 0; }}
  .content th, .content td {{ border: 1px solid var(--border); padding: 0.5rem 0.75rem; text-align: left; }}
  .content th {{ background: var(--sidebar-bg); }}
  .content blockquote {{
    border-left: 3px solid var(--link); padding: 0.5rem 1rem;
    margin: 1rem 0; background: var(--code-bg); border-radius: 0 0.25rem 0.25rem 0;
  }}
  .content pre {{
    background: var(--code-bg); padding: 1rem; border-radius: 0.5rem;
    overflow-x: auto; margin: 1rem 0; border: 1px solid var(--border);
    font-size: 0.875rem; line-height: 1.5;
  }}
  .content code {{
    font-family: "JetBrains Mono", "Fira Code", "Cascadia Code", monospace;
    font-size: 0.875em;
  }}
  .content :not(pre) > code {{
    background: var(--code-bg); padding: 0.15rem 0.35rem; border-radius: 0.25rem;
  }}
  @media (max-width: 768px) {{
    body {{ flex-direction: column; }}
    .sidebar {{ width: 100%; max-height: none; position: static; border-right: none; border-bottom: 1px solid var(--border); }}
    .content {{ padding: 1.5rem; }}
  }}
</style>
</head>
<body>
{sidebar}
<main class="content">
{body_html}
</main>
</body>
</html>"#
    ))
}

async fn serve_docs(
    State(state): State<Arc<AppState>>,
    Path(path): Path<String>,
    query: axum::extract::Query<std::collections::HashMap<String, String>>,
) -> Response {
    let docs_dir = match &state.docs_dir {
        Some(d) => d,
        None => return (StatusCode::NOT_FOUND, "docs not configured").into_response(),
    };

    let path = path.trim_end_matches('/');

    // ?format=md returns raw markdown
    if query.get("format").map(|v| v.as_str()) == Some("md") {
        return match resolve_markdown_path(docs_dir, path) {
            Some(content) => (
                [(axum::http::header::CONTENT_TYPE, "text/markdown; charset=utf-8")],
                content,
            ).into_response(),
            None => (StatusCode::NOT_FOUND, "page not found").into_response(),
        };
    }

    match render_markdown(docs_dir, path) {
        Some(html) => Html(html).into_response(),
        None => (StatusCode::NOT_FOUND, "page not found").into_response(),
    }
}

async fn serve_docs_index(State(state): State<Arc<AppState>>) -> Response {
    let docs_dir = match &state.docs_dir {
        Some(d) => d,
        None => return (StatusCode::NOT_FOUND, "docs not configured").into_response(),
    };

    match render_markdown(docs_dir, "index") {
        Some(html) => Html(html).into_response(),
        None => {
            // No index.md — generate a directory listing
            let sidebar = build_sidebar(docs_dir, "");
            let html = format!(
                r#"<!DOCTYPE html><html><head><meta charset="utf-8"><title>Docs</title></head>
<body style="display:flex">{sidebar}<main style="padding:2rem"><h1>Documentation</h1>
<p>Select a section from the sidebar.</p></main></body></html>"#
            );
            Html(html).into_response()
        }
    }
}

// ---------------------------------------------------------------------------
// Router
// ---------------------------------------------------------------------------

fn build_router(state: Arc<AppState>, www_dir: PathBuf) -> Router {
    let api = Router::new()
        .route("/api/posts", axum::routing::get(list_posts).post(create_post))
        .route(
            "/api/posts/{id}",
            axum::routing::get(get_post)
                .put(update_post)
                .delete(delete_post),
        );

    let jmap = Router::new()
        .route("/jmap", axum::routing::any(jmap_proxy))
        .route("/jmap/{*rest}", axum::routing::any(jmap_proxy));

    let ws_proxy = Router::new()
        .route("/ws", axum::routing::get(ws_proxy_handler));

    let docs = Router::new()
        .route("/docs", axum::routing::get(serve_docs_index))
        .route("/docs/", axum::routing::get(serve_docs_index))
        .route("/docs/{*path}", axum::routing::get(serve_docs));

    // Serve docs assets (images, videos) at /assets/ if docs_dir has an assets/ subdir
    let docs_assets = if let Some(ref docs_dir) = state.docs_dir {
        let assets_path = docs_dir.join("assets");
        if assets_path.is_dir() {
            Some(Router::new().nest_service("/assets", ServeDir::new(assets_path)))
        } else {
            None
        }
    } else {
        None
    };

    let mut router = Router::new()
        .merge(api)
        .merge(jmap)
        .merge(ws_proxy)
        .merge(docs);

    if let Some(assets_router) = docs_assets {
        router = router.merge(assets_router);
    }

    router
        .fallback_service(ServeDir::new(www_dir))
        .with_state(state)
}

// ---------------------------------------------------------------------------
// Rusqlite optional helper (query_row that returns Option)
// ---------------------------------------------------------------------------

trait QueryRowOptional {
    fn optional(self) -> Result<Option<Post>, rusqlite::Error>;
}

impl QueryRowOptional for Result<Post, rusqlite::Error> {
    fn optional(self) -> Result<Option<Post>, rusqlite::Error> {
        match self {
            Ok(post) => Ok(Some(post)),
            Err(rusqlite::Error::QueryReturnedNoRows) => Ok(None),
            Err(e) => Err(e),
        }
    }
}

// ---------------------------------------------------------------------------
// Main
// ---------------------------------------------------------------------------

#[tokio::main]
async fn main() -> Result<()> {
    let _log = cosmix_daemon::init_tracing("cosmix_webd");

    let cli = Cli::parse();

    match cli.command {
        Command::Init { db_path } => {
            if let Some(parent) = db_path.parent() {
                std::fs::create_dir_all(parent)?;
            }
            init_db(&db_path)?;
        }
        Command::Serve {
            listen,
            www_dir,
            db_path,
            jmap_upstream,
            hub_ws,
            docs_dir,
            tls_cert,
            tls_key,
        } => {
            rustls::crypto::ring::default_provider()
                .install_default()
                .expect("Failed to install rustls crypto provider");

            // Ensure DB exists and schema is applied
            if let Some(parent) = db_path.parent() {
                std::fs::create_dir_all(parent)?;
            }
            let conn = open_db(&db_path)?;
            conn.execute_batch(SCHEMA)?;

            if let Some(ref d) = docs_dir {
                info!("serving markdown docs from {}", d.display());
            }

            let state = Arc::new(AppState {
                db: Mutex::new(conn),
                jmap_upstream,
                hub_ws,
                http_client: reqwest::Client::builder()
                    .danger_accept_invalid_certs(true)
                    .build()?,
                docs_dir,
            });

            let app = build_router(state, www_dir);
            let listener = tokio::net::TcpListener::bind(&listen).await?;

            if let (Some(cert_path), Some(key_path)) = (&tls_cert, &tls_key) {
                let tls_acceptor = cosmix_daemon::load_tls_config(
                    &cert_path.to_string_lossy(),
                    &key_path.to_string_lossy(),
                )?;

                info!("cosmix-web listening on {listen} (HTTPS)");
                loop {
                    let (stream, _) = listener.accept().await?;
                    let acceptor = tls_acceptor.clone();
                    let app = app.clone();
                    tokio::spawn(async move {
                        match acceptor.accept(stream).await {
                            Ok(tls_stream) => {
                                let io = hyper_util::rt::TokioIo::new(tls_stream);
                                let svc = hyper_util::service::TowerToHyperService::new(app);
                                let _ = hyper_util::server::conn::auto::Builder::new(
                                    hyper_util::rt::TokioExecutor::new(),
                                )
                                .serve_connection(io, svc)
                                .await;
                            }
                            Err(e) => tracing::debug!(error = %e, "TLS handshake failed"),
                        }
                    });
                }
            } else {
                info!("cosmix-web listening on {listen}");
                axum::serve(listener, app).await?;
            }
        }
    }

    Ok(())
}
