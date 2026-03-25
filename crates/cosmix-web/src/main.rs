#[global_allocator]
static GLOBAL: mimalloc::MiMalloc = mimalloc::MiMalloc;

use std::path::PathBuf;
use std::sync::Arc;

use anyhow::Result;
use axum::Router;
use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Json, Response};
use clap::{Parser, Subcommand};
use rusqlite::Connection;
use serde::{Deserialize, Serialize};
use tokio::sync::Mutex;
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
        #[arg(long, default_value = "0.0.0.0:8443")]
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
    http_client: reqwest::Client,
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

    Router::new()
        .merge(api)
        .merge(jmap)
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
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
        )
        .init();

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
        } => {
            // Ensure DB exists and schema is applied
            if let Some(parent) = db_path.parent() {
                std::fs::create_dir_all(parent)?;
            }
            let conn = open_db(&db_path)?;
            conn.execute_batch(SCHEMA)?;

            let state = Arc::new(AppState {
                db: Mutex::new(conn),
                jmap_upstream,
                http_client: reqwest::Client::new(),
            });

            let router = build_router(state, www_dir);

            let listener = tokio::net::TcpListener::bind(&listen).await?;
            info!("cosmix-web listening on {listen}");
            axum::serve(listener, router).await?;
        }
    }

    Ok(())
}
