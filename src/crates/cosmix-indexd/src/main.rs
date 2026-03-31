// Use system allocator — it returns memory to OS on free, unlike mimalloc
// which holds freed pages in its pool. Critical for model unload to actually
// reduce RSS.

use anyhow::{Context, Result};
use candle_core::{DType, Device, Tensor};
use candle_nn::VarBuilder;
use candle_transformers::models::nomic_bert::{self, NomicBertModel};
use hf_hub::{api::sync::Api, Repo, RepoType};
use rusqlite::Connection;
use serde::{Deserialize, Serialize};
use std::sync::Arc;
use std::time::Duration;
use tokenizers::Tokenizer;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::UnixListener;
use tokio::sync::Mutex;
use tracing::{error, info};

const MODEL_ID: &str = "nomic-ai/nomic-embed-text-v1.5";
const EMBEDDING_DIM: usize = 768;
const SOCKET_DIR: &str = "/run/cosmix";
const SOCKET_PATH: &str = "/run/cosmix/embed.sock";
const MODEL_IDLE_SECS: u64 = 60;
const DEFAULT_DB_PATH: &str = "/var/lib/cosmix/vectors.db";

// --- Request/Response types ---

#[derive(Deserialize)]
#[serde(tag = "action")]
#[serde(rename_all = "snake_case")]
enum Request {
    Embed(EmbedRequest),
    Store(StoreRequest),
    Search(SearchRequest),
    Delete(DeleteRequest),
    Stats,
}

#[derive(Deserialize)]
struct EmbedRequest {
    texts: Vec<String>,
    #[serde(default = "default_doc_prefix")]
    prefix: String,
}

#[derive(Deserialize)]
struct StoreRequest {
    texts: Vec<String>,
    #[serde(default)]
    source: String,
    #[serde(default)]
    metadata: Vec<String>,
}

#[derive(Deserialize)]
struct SearchRequest {
    query: String,
    #[serde(default = "default_limit")]
    limit: usize,
    #[serde(default)]
    source: String,
}

#[derive(Deserialize)]
struct DeleteRequest {
    ids: Vec<i64>,
}

fn default_doc_prefix() -> String {
    "search_document: ".into()
}

fn default_limit() -> usize {
    10
}

#[derive(Serialize)]
struct EmbedResponse {
    embeddings: Vec<Vec<f32>>,
}

#[derive(Serialize)]
struct StoreResponse {
    stored: usize,
    ids: Vec<i64>,
}

#[derive(Serialize)]
struct SearchResult {
    id: i64,
    content: String,
    source: String,
    metadata: String,
    distance: f64,
}

#[derive(Serialize)]
struct SearchResponse {
    results: Vec<SearchResult>,
}

#[derive(Serialize)]
struct DeleteResponse {
    deleted: usize,
}

#[derive(Serialize)]
struct StatsResponse {
    total_vectors: usize,
    db_size_bytes: u64,
    model_loaded: bool,
}

#[derive(Serialize)]
struct ErrorResponse {
    error: String,
}

// --- Embedding model ---

struct EmbedModel {
    model: NomicBertModel,
    tokenizer: Tokenizer,
    device: Device,
}

impl EmbedModel {
    fn load(dtype: DType) -> Result<Self> {
        let device = Device::Cpu;

        info!("downloading model files from {MODEL_ID}...");
        let api = Api::new()?;
        let repo = api.repo(Repo::new(MODEL_ID.into(), RepoType::Model));

        let config_path = repo.get("config.json").context("downloading config.json")?;
        let tokenizer_path = repo
            .get("tokenizer.json")
            .context("downloading tokenizer.json")?;
        let weights_path = repo
            .get("model.safetensors")
            .context("downloading model.safetensors")?;

        info!("loading model with {dtype:?} precision...");
        let config: nomic_bert::Config = serde_json::from_str(
            &std::fs::read_to_string(&config_path).context("reading config.json")?,
        )?;
        let tokenizer =
            Tokenizer::from_file(&tokenizer_path).map_err(|e| anyhow::anyhow!("{e}"))?;

        let vb =
            unsafe { VarBuilder::from_mmaped_safetensors(&[weights_path], dtype, &device)? };
        let model = NomicBertModel::load(vb, &config)?;

        info!("model loaded successfully");
        Ok(Self {
            model,
            tokenizer,
            device,
        })
    }

    fn embed(&self, texts: &[String], prefix: &str) -> Result<Vec<Vec<f32>>> {
        if texts.is_empty() {
            return Ok(vec![]);
        }

        let prefixed: Vec<String> = texts.iter().map(|t| format!("{prefix}{t}")).collect();

        let tokens = self
            .tokenizer
            .encode_batch(
                prefixed.iter().map(|s| s.as_str()).collect::<Vec<_>>(),
                true,
            )
            .map_err(|e| anyhow::anyhow!("tokenization: {e}"))?;

        let max_len = tokens.iter().map(|t| t.get_ids().len()).max().unwrap_or(0);

        let mut all_ids = Vec::new();
        let mut all_mask = Vec::new();
        let mut all_type_ids = Vec::new();

        for encoding in &tokens {
            let ids = encoding.get_ids();
            let mask = encoding.get_attention_mask();
            let type_ids = encoding.get_type_ids();
            let pad_len = max_len - ids.len();

            let mut padded_ids = ids.to_vec();
            padded_ids.extend(vec![0u32; pad_len]);
            all_ids.extend(padded_ids);

            let mut padded_mask = mask.to_vec();
            padded_mask.extend(vec![0u32; pad_len]);
            all_mask.extend(padded_mask);

            let mut padded_type_ids = type_ids.to_vec();
            padded_type_ids.extend(vec![0u32; pad_len]);
            all_type_ids.extend(padded_type_ids);
        }

        let batch_size = tokens.len();
        let input_ids = Tensor::from_vec(all_ids, (batch_size, max_len), &self.device)?;
        let attention_mask = Tensor::from_vec(all_mask, (batch_size, max_len), &self.device)?;
        let token_type_ids =
            Tensor::from_vec(all_type_ids, (batch_size, max_len), &self.device)?;

        let hidden = self
            .model
            .forward(&input_ids, Some(&token_type_ids), Some(&attention_mask))?;

        let hidden = hidden.to_dtype(DType::F32)?;

        let pooled = nomic_bert::mean_pooling(&hidden, &attention_mask)?;
        let normalized = nomic_bert::l2_normalize(&pooled)?;

        let mut results = Vec::with_capacity(batch_size);
        for i in 0..batch_size {
            let emb = normalized.get(i)?.to_vec1::<f32>()?;
            results.push(emb);
        }

        Ok(results)
    }
}

// --- Vector database ---

struct VectorDb {
    conn: Connection,
}

impl VectorDb {
    fn open(path: &str) -> Result<Self> {
        unsafe {
            rusqlite::ffi::sqlite3_auto_extension(Some(std::mem::transmute(
                sqlite_vec::sqlite3_vec_init as *const (),
            )));
        }

        if let Some(parent) = std::path::Path::new(path).parent() {
            std::fs::create_dir_all(parent)?;
        }

        let conn = Connection::open(path)?;
        conn.execute_batch("PRAGMA journal_mode=WAL; PRAGMA synchronous=NORMAL;")?;

        conn.execute_batch(&format!(
            "CREATE TABLE IF NOT EXISTS chunks (
                id       INTEGER PRIMARY KEY AUTOINCREMENT,
                content  TEXT NOT NULL,
                source   TEXT NOT NULL DEFAULT '',
                metadata TEXT NOT NULL DEFAULT '',
                created  TEXT NOT NULL DEFAULT (datetime('now'))
            );
            CREATE VIRTUAL TABLE IF NOT EXISTS vec_chunks USING vec0(
                embedding float[{EMBEDDING_DIM}]
            );"
        ))?;

        let version: String = conn.query_row("SELECT vec_version()", [], |r| r.get(0))?;
        info!("vector db opened at {path} (sqlite-vec {version})");
        Ok(Self { conn })
    }

    fn store(
        &self,
        embeddings: &[Vec<f32>],
        texts: &[String],
        source: &str,
        metadata: &[String],
    ) -> Result<Vec<i64>> {
        let mut ids = Vec::with_capacity(embeddings.len());

        for (i, (emb, text)) in embeddings.iter().zip(texts.iter()).enumerate() {
            let meta = metadata.get(i).map(|s| s.as_str()).unwrap_or("");

            self.conn.execute(
                "INSERT INTO chunks (content, source, metadata) VALUES (?1, ?2, ?3)",
                rusqlite::params![text, source, meta],
            )?;
            let rowid = self.conn.last_insert_rowid();

            let blob = vec_to_blob(emb);
            self.conn.execute(
                "INSERT INTO vec_chunks (rowid, embedding) VALUES (?1, ?2)",
                rusqlite::params![rowid, blob],
            )?;

            ids.push(rowid);
        }

        Ok(ids)
    }

    fn search(
        &self,
        query_embedding: &[f32],
        limit: usize,
        source_filter: &str,
    ) -> Result<Vec<SearchResult>> {
        let blob = vec_to_blob(query_embedding);
        let mut results = Vec::new();

        if source_filter.is_empty() {
            let mut stmt = self.conn.prepare(
                "SELECT v.rowid, v.distance, c.content, c.source, c.metadata
                 FROM vec_chunks v
                 JOIN chunks c ON c.id = v.rowid
                 WHERE v.embedding MATCH ?1
                 AND k = ?2
                 ORDER BY v.distance",
            )?;
            let rows = stmt.query_map(rusqlite::params![blob, limit as i64], |row| {
                Ok(SearchResult {
                    id: row.get(0)?,
                    distance: row.get(1)?,
                    content: row.get(2)?,
                    source: row.get(3)?,
                    metadata: row.get(4)?,
                })
            })?;
            for row in rows {
                results.push(row?);
            }
        } else {
            let mut stmt = self.conn.prepare(
                "SELECT v.rowid, v.distance, c.content, c.source, c.metadata
                 FROM vec_chunks v
                 JOIN chunks c ON c.id = v.rowid
                 WHERE v.embedding MATCH ?1
                 AND k = ?2
                 AND c.source = ?3
                 ORDER BY v.distance",
            )?;
            let rows = stmt.query_map(
                rusqlite::params![blob, limit as i64, source_filter],
                |row| {
                    Ok(SearchResult {
                        id: row.get(0)?,
                        distance: row.get(1)?,
                        content: row.get(2)?,
                        source: row.get(3)?,
                        metadata: row.get(4)?,
                    })
                },
            )?;
            for row in rows {
                results.push(row?);
            }
        }

        Ok(results)
    }

    fn delete(&self, ids: &[i64]) -> Result<usize> {
        let mut deleted = 0usize;
        for id in ids {
            deleted += self
                .conn
                .execute("DELETE FROM chunks WHERE id = ?1", [id])?;
            self.conn
                .execute("DELETE FROM vec_chunks WHERE rowid = ?1", [id])?;
        }
        Ok(deleted)
    }

    fn stats(&self, db_path: &str) -> Result<StatsResponse> {
        let total: usize = self
            .conn
            .query_row("SELECT COUNT(*) FROM chunks", [], |r| r.get::<_, i64>(0).map(|v| v as usize))?;
        let db_size = std::fs::metadata(db_path).map(|m| m.len()).unwrap_or(0);
        Ok(StatsResponse {
            total_vectors: total,
            db_size_bytes: db_size,
            model_loaded: false, // caller fills this in
        })
    }
}

fn vec_to_blob(v: &[f32]) -> Vec<u8> {
    v.iter().flat_map(|f| f.to_le_bytes()).collect()
}

// --- Shared state ---

struct AppState {
    model: Option<EmbedModel>,
    dtype: DType,
    db: VectorDb,
    db_path: String,
}

#[tokio::main]
async fn main() -> Result<()> {
    let _log = cosmix_daemon::init_tracing("cosmix_indexd");

    let listener = if let Ok(listener) = try_systemd_socket() {
        info!("using systemd socket activation");
        listener
    } else {
        std::fs::create_dir_all(SOCKET_DIR)
            .with_context(|| format!("creating {SOCKET_DIR}"))?;
        let _ = std::fs::remove_file(SOCKET_PATH);
        let listener = UnixListener::bind(SOCKET_PATH)
            .with_context(|| format!("binding {SOCKET_PATH}"))?;
        std::fs::set_permissions(
            SOCKET_PATH,
            std::os::unix::fs::PermissionsExt::from_mode(0o666),
        )?;
        info!("listening on {SOCKET_PATH}");
        listener
    };

    let dtype = if std::env::args().any(|a| a == "--f32") {
        DType::F32
    } else {
        DType::F16
    };

    let db_path = std::env::var("COSMIX_VECTORS_DB").unwrap_or_else(|_| DEFAULT_DB_PATH.into());
    let db = VectorDb::open(&db_path)?;

    let state = Arc::new(Mutex::new(AppState {
        model: None,
        dtype,
        db,
        db_path,
    }));

    let (activity_tx, mut activity_rx) = tokio::sync::mpsc::channel::<()>(16);

    info!("ready — model loads on first request, unloads after {MODEL_IDLE_SECS}s idle");

    // Spawn idle watchdog
    let watchdog_state = state.clone();
    tokio::spawn(async move {
        loop {
            if activity_rx.recv().await.is_none() {
                break;
            }
            while activity_rx.try_recv().is_ok() {}

            let mut idle_remaining = MODEL_IDLE_SECS;
            loop {
                tokio::select! {
                    _ = tokio::time::sleep(Duration::from_secs(1)) => {
                        idle_remaining = idle_remaining.saturating_sub(1);
                        if idle_remaining == 0 {
                            let mut guard = watchdog_state.lock().await;
                            if guard.model.is_some() {
                                info!("model idle for {MODEL_IDLE_SECS}s, unloading to free memory");
                                guard.model = None;
                                drop(guard);
                                unsafe { libc::malloc_trim(0); }
                            }
                            break;
                        }
                    }
                    result = activity_rx.recv() => {
                        if result.is_none() {
                            return;
                        }
                        while activity_rx.try_recv().is_ok() {}
                        idle_remaining = MODEL_IDLE_SECS;
                    }
                }
            }
        }
    });

    loop {
        let (stream, _) = listener.accept().await?;
        let app_state = state.clone();
        let tx = activity_tx.clone();

        tokio::spawn(async move {
            if let Err(e) = handle_connection(stream, &app_state, &tx).await {
                error!("connection error: {e}");
            }
        });
    }
}

async fn handle_connection(
    stream: tokio::net::UnixStream,
    state: &Arc<Mutex<AppState>>,
    activity_tx: &tokio::sync::mpsc::Sender<()>,
) -> Result<()> {
    let (reader, mut writer) = stream.into_split();
    let mut reader = BufReader::new(reader);
    let mut line = String::new();

    loop {
        line.clear();
        let n = reader.read_line(&mut line).await?;
        if n == 0 {
            break;
        }

        let response = process_request(line.trim(), state, activity_tx).await;

        writer.write_all(response.as_bytes()).await?;
        writer.write_all(b"\n").await?;
        writer.flush().await?;
    }

    Ok(())
}

async fn process_request(
    input: &str,
    state: &Arc<Mutex<AppState>>,
    activity_tx: &tokio::sync::mpsc::Sender<()>,
) -> String {
    let req: Request = match serde_json::from_str(input) {
        Ok(r) => r,
        Err(e) => return json_error(&format!("invalid request: {e}")),
    };

    match req {
        Request::Embed(req) => handle_embed(req, state, activity_tx).await,
        Request::Store(req) => handle_store(req, state, activity_tx).await,
        Request::Search(req) => handle_search(req, state, activity_tx).await,
        Request::Delete(req) => handle_delete(req, state).await,
        Request::Stats => handle_stats(state).await,
    }
}

async fn ensure_model(state: &mut AppState) -> Result<()> {
    if state.model.is_none() {
        info!("loading model on demand...");
        state.model = Some(EmbedModel::load(state.dtype)?);
    }
    Ok(())
}

async fn handle_embed(
    req: EmbedRequest,
    state: &Arc<Mutex<AppState>>,
    activity_tx: &tokio::sync::mpsc::Sender<()>,
) -> String {
    let mut guard = state.lock().await;
    if let Err(e) = ensure_model(&mut guard).await {
        return json_error(&format!("model load failed: {e}"));
    }
    let model = guard.model.as_ref().unwrap();
    match model.embed(&req.texts, &req.prefix) {
        Ok(embeddings) => {
            let _ = activity_tx.send(()).await;
            serde_json::to_string(&EmbedResponse { embeddings }).unwrap()
        }
        Err(e) => json_error(&e.to_string()),
    }
}

async fn handle_store(
    req: StoreRequest,
    state: &Arc<Mutex<AppState>>,
    activity_tx: &tokio::sync::mpsc::Sender<()>,
) -> String {
    let mut guard = state.lock().await;
    if let Err(e) = ensure_model(&mut guard).await {
        return json_error(&format!("model load failed: {e}"));
    }
    let model = guard.model.as_ref().unwrap();

    let prefix = "search_document: ";
    let embeddings = match model.embed(&req.texts, prefix) {
        Ok(e) => e,
        Err(e) => return json_error(&format!("embedding failed: {e}")),
    };
    let _ = activity_tx.send(()).await;

    match guard.db.store(&embeddings, &req.texts, &req.source, &req.metadata) {
        Ok(ids) => serde_json::to_string(&StoreResponse {
            stored: ids.len(),
            ids,
        })
        .unwrap(),
        Err(e) => json_error(&format!("store failed: {e}")),
    }
}

async fn handle_search(
    req: SearchRequest,
    state: &Arc<Mutex<AppState>>,
    activity_tx: &tokio::sync::mpsc::Sender<()>,
) -> String {
    let mut guard = state.lock().await;
    if let Err(e) = ensure_model(&mut guard).await {
        return json_error(&format!("model load failed: {e}"));
    }
    let model = guard.model.as_ref().unwrap();

    let query_emb = match model.embed(&[req.query.clone()], "search_query: ") {
        Ok(mut e) => {
            if e.is_empty() {
                return json_error("empty query");
            }
            e.remove(0)
        }
        Err(e) => return json_error(&format!("embedding failed: {e}")),
    };
    let _ = activity_tx.send(()).await;

    match guard.db.search(&query_emb, req.limit, &req.source) {
        Ok(results) => serde_json::to_string(&SearchResponse { results }).unwrap(),
        Err(e) => json_error(&format!("search failed: {e}")),
    }
}

async fn handle_delete(req: DeleteRequest, state: &Arc<Mutex<AppState>>) -> String {
    let guard = state.lock().await;
    match guard.db.delete(&req.ids) {
        Ok(deleted) => serde_json::to_string(&DeleteResponse { deleted }).unwrap(),
        Err(e) => json_error(&format!("delete failed: {e}")),
    }
}

async fn handle_stats(state: &Arc<Mutex<AppState>>) -> String {
    let guard = state.lock().await;
    match guard.db.stats(&guard.db_path) {
        Ok(mut stats) => {
            stats.model_loaded = guard.model.is_some();
            serde_json::to_string(&stats).unwrap()
        }
        Err(e) => json_error(&format!("stats failed: {e}")),
    }
}

fn json_error(msg: &str) -> String {
    serde_json::to_string(&ErrorResponse {
        error: msg.to_string(),
    })
    .unwrap()
}

/// Try to get a socket from systemd socket activation (LISTEN_FDS).
fn try_systemd_socket() -> Result<UnixListener> {
    use std::os::unix::io::FromRawFd;

    let listen_pid: u32 = std::env::var("LISTEN_PID")
        .ok()
        .and_then(|s| s.parse().ok())
        .ok_or_else(|| anyhow::anyhow!("no LISTEN_PID"))?;

    if listen_pid != std::process::id() {
        anyhow::bail!("LISTEN_PID mismatch");
    }

    let listen_fds: u32 = std::env::var("LISTEN_FDS")
        .ok()
        .and_then(|s| s.parse().ok())
        .ok_or_else(|| anyhow::anyhow!("no LISTEN_FDS"))?;

    if listen_fds < 1 {
        anyhow::bail!("no fds");
    }

    let std_listener = unsafe { std::os::unix::net::UnixListener::from_raw_fd(3) };
    std_listener.set_nonblocking(true)?;
    let listener = UnixListener::from_std(std_listener)?;
    Ok(listener)
}
