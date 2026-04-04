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
use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokenizers::Tokenizer;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::UnixListener;
use tokio::sync::Mutex;
use tracing::{error, info, warn};

const EMBEDDING_DIM: usize = 768;

// --- Circuit breaker for model loading ---

#[derive(Debug, Clone, PartialEq, Eq)]
enum CircuitState {
    Closed,
    Open { opened_at: Instant },
    HalfOpen,
}

struct CircuitBreaker {
    state: CircuitState,
    consecutive_failures: u32,
    failure_threshold: u32,
    cooldown: Duration,
}

impl CircuitBreaker {
    fn new(failure_threshold: u32, cooldown: Duration) -> Self {
        Self {
            state: CircuitState::Closed,
            consecutive_failures: 0,
            failure_threshold,
            cooldown,
        }
    }

    fn allow_request(&mut self) -> bool {
        match self.state {
            CircuitState::Closed | CircuitState::HalfOpen => true,
            CircuitState::Open { opened_at } => {
                if opened_at.elapsed() >= self.cooldown {
                    self.state = CircuitState::HalfOpen;
                    true
                } else {
                    false
                }
            }
        }
    }

    fn record_success(&mut self) {
        self.consecutive_failures = 0;
        self.state = CircuitState::Closed;
    }

    fn record_failure(&mut self) {
        match self.state {
            CircuitState::Closed => {
                self.consecutive_failures += 1;
                if self.consecutive_failures >= self.failure_threshold {
                    warn!("model circuit breaker OPEN after {} consecutive failures", self.consecutive_failures);
                    self.state = CircuitState::Open {
                        opened_at: Instant::now(),
                    };
                }
            }
            CircuitState::HalfOpen => {
                warn!("model circuit breaker re-OPEN (half-open probe failed)");
                self.state = CircuitState::Open {
                    opened_at: Instant::now(),
                };
            }
            CircuitState::Open { .. } => {}
        }
    }

    fn state_name(&self) -> &'static str {
        match self.state {
            CircuitState::Closed => "closed",
            CircuitState::Open { .. } => "open",
            CircuitState::HalfOpen => "half-open",
        }
    }
}

// --- Embedding cache (FNV-1a keyed by text+prefix, TTL eviction) ---

const FNV_OFFSET_BASIS: u64 = 0xcbf2_9ce4_8422_2325;
const FNV_PRIME: u64 = 0x0000_0100_0000_01b3;
const EMBED_CACHE_TTL_SECS: u64 = 300; // 5 minutes
const EMBED_CACHE_MAX_ENTRIES: usize = 512;

struct CachedEmbedding {
    embedding: Vec<f32>,
    created_at: Instant,
}

struct EmbeddingCache {
    entries: HashMap<u64, CachedEmbedding>,
    ttl: Duration,
    max_entries: usize,
    hits: u64,
    misses: u64,
}

impl EmbeddingCache {
    fn new() -> Self {
        Self {
            entries: HashMap::new(),
            ttl: Duration::from_secs(EMBED_CACHE_TTL_SECS),
            max_entries: EMBED_CACHE_MAX_ENTRIES,
            hits: 0,
            misses: 0,
        }
    }

    fn lookup(&mut self, text: &str, prefix: &str) -> Option<Vec<f32>> {
        let key = fnv1a_hash(text, prefix);
        if let Some(entry) = self.entries.get(&key) {
            if entry.created_at.elapsed() < self.ttl {
                self.hits += 1;
                return Some(entry.embedding.clone());
            }
            self.entries.remove(&key);
        }
        self.misses += 1;
        None
    }

    fn store(&mut self, text: &str, prefix: &str, embedding: Vec<f32>) {
        if self.entries.len() >= self.max_entries {
            // Evict oldest entry
            if let Some(&oldest_key) = self
                .entries
                .iter()
                .min_by_key(|(_, v)| v.created_at)
                .map(|(k, _)| k)
            {
                self.entries.remove(&oldest_key);
            }
        }
        let key = fnv1a_hash(text, prefix);
        self.entries.insert(
            key,
            CachedEmbedding {
                embedding,
                created_at: Instant::now(),
            },
        );
    }

    /// Look up a batch, returning cached embeddings and indices that need computing.
    fn lookup_batch(
        &mut self,
        texts: &[String],
        prefix: &str,
    ) -> (Vec<Option<Vec<f32>>>, Vec<usize>) {
        let mut results = Vec::with_capacity(texts.len());
        let mut needs_embed = Vec::new();
        for (i, text) in texts.iter().enumerate() {
            match self.lookup(text, prefix) {
                Some(emb) => results.push(Some(emb)),
                None => {
                    results.push(None);
                    needs_embed.push(i);
                }
            }
        }
        (results, needs_embed)
    }
}

fn fnv1a_hash(text: &str, prefix: &str) -> u64 {
    let mut hash = FNV_OFFSET_BASIS;
    for byte in prefix.bytes().chain(text.bytes()) {
        hash ^= u64::from(byte);
        hash = hash.wrapping_mul(FNV_PRIME);
    }
    hash
}

// --- Content hash for deduplication ---

fn content_hash(text: &str, source: &str) -> Vec<u8> {
    // FNV-1a 128-bit hash (two 64-bit passes with different seeds)
    let mut h1 = FNV_OFFSET_BASIS;
    let mut h2 = 0x6c62_272e_07bb_0142_u64; // second seed
    for byte in source.bytes().chain(b":".iter().copied()).chain(text.bytes()) {
        h1 ^= u64::from(byte);
        h1 = h1.wrapping_mul(FNV_PRIME);
        h2 ^= u64::from(byte);
        h2 = h2.wrapping_mul(0x0000_0100_0000_01c9); // different prime
    }
    let mut out = Vec::with_capacity(16);
    out.extend_from_slice(&h1.to_le_bytes());
    out.extend_from_slice(&h2.to_le_bytes());
    out
}

// --- Request/Response types ---

#[derive(Deserialize)]
#[serde(tag = "action")]
#[serde(rename_all = "snake_case")]
enum Request {
    Embed(EmbedRequest),
    Store(StoreRequest),
    Search(SearchRequest),
    Update(UpdateRequest),
    Delete(DeleteRequest),
    List(ListRequest),
    Feedback(FeedbackRequest),
    Stale(StaleRequest),
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
    #[serde(default)]
    metadata_filter: Vec<MetadataFilter>,
}

#[derive(Deserialize)]
struct MetadataFilter {
    field: String,
    op: FilterOp,
    value: serde_json::Value,
}

#[derive(Deserialize)]
#[serde(rename_all = "snake_case")]
enum FilterOp {
    Eq,
    Gt,
    Lt,
    Gte,
    Lte,
    Contains,
}

#[derive(Deserialize)]
struct UpdateRequest {
    id: i64,
    #[serde(default)]
    content: Option<String>,
    #[serde(default)]
    metadata: Option<String>,
    #[serde(default)]
    source: Option<String>,
}

#[derive(Deserialize)]
struct DeleteRequest {
    ids: Vec<i64>,
}

#[derive(Deserialize)]
struct ListRequest {
    #[serde(default)]
    source: String,
    #[serde(default = "default_limit")]
    limit: usize,
    #[serde(default)]
    offset: usize,
}

#[derive(Deserialize)]
struct FeedbackRequest {
    id: i64,
    useful: bool,
}

#[derive(Deserialize)]
struct StaleRequest {
    /// Only include chunks of this source (empty = all sources).
    #[serde(default)]
    source: String,
    /// "Never retrieved & old" bucket: age > this many days (default 90).
    #[serde(default = "default_stale_age_days")]
    never_retrieved_age_days: i64,
    /// "Low value" bucket: retrieval_count > this AND feedback_score <= 0 (default 3).
    #[serde(default = "default_low_value_retrievals")]
    low_value_min_retrievals: i64,
    /// "Long dormant" bucket: last_retrieved older than this many days (default 180).
    #[serde(default = "default_dormant_days")]
    long_dormant_days: i64,
    /// Max chunks per bucket (default 50).
    #[serde(default = "default_stale_limit")]
    per_bucket_limit: usize,
}

fn default_stale_age_days() -> i64 { 90 }
fn default_low_value_retrievals() -> i64 { 3 }
fn default_dormant_days() -> i64 { 180 }
fn default_stale_limit() -> usize { 50 }

#[derive(Serialize)]
struct StaleChunk {
    id: i64,
    source: String,
    preview: String,
    retrieval_count: i64,
    feedback_score: i64,
    last_retrieved: Option<String>,
    created: String,
    path: Option<String>,
    filename: Option<String>,
}

#[derive(Serialize)]
struct StaleResponse {
    never_retrieved_old: Vec<StaleChunk>,
    low_value: Vec<StaleChunk>,
    long_dormant: Vec<StaleChunk>,
    total_chunks: usize,
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
    duplicates: usize,
    ids: Vec<i64>,
}

#[derive(Serialize)]
struct SearchResult {
    id: i64,
    content: String,
    source: String,
    metadata: String,
    distance: f64,
    feedback_score: i64,
    retrieval_count: i64,
    #[serde(skip_serializing_if = "Option::is_none")]
    last_retrieved: Option<String>,
    created: String,
}

#[derive(Serialize)]
struct SearchResponse {
    results: Vec<SearchResult>,
}

#[derive(Serialize)]
struct UpdateResponse {
    updated: bool,
    re_embedded: bool,
}

#[derive(Serialize)]
struct DeleteResponse {
    deleted: usize,
}

#[derive(Serialize)]
struct ListItem {
    id: i64,
    content: String,
    source: String,
    metadata: String,
    created: String,
}

#[derive(Serialize)]
struct ListResponse {
    items: Vec<ListItem>,
    total: usize,
}

#[derive(Serialize)]
struct StatsResponse {
    total_vectors: usize,
    db_size_bytes: u64,
    model_loaded: bool,
    model_circuit: String,
    embed_cache_entries: usize,
    embed_cache_hits: u64,
    embed_cache_misses: u64,
    #[serde(default)]
    by_source: Vec<SourceCount>,
}

#[derive(Serialize)]
struct SourceCount {
    source: String,
    count: usize,
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
    fn load(dtype: DType, model_id: &str) -> Result<Self> {
        let device = Device::Cpu;

        info!("downloading model files from {model_id}...");
        let api = Api::new()?;
        let repo = api.repo(Repo::new(model_id.into(), RepoType::Model));

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

        // Create base tables (index on content_hash deferred — may not exist on upgraded DBs)
        conn.execute_batch(&format!(
            "CREATE TABLE IF NOT EXISTS chunks (
                id            INTEGER PRIMARY KEY AUTOINCREMENT,
                content       TEXT NOT NULL,
                source        TEXT NOT NULL DEFAULT '',
                metadata      TEXT NOT NULL DEFAULT '',
                content_hash  BLOB,
                created       TEXT NOT NULL DEFAULT (datetime('now'))
            );
            CREATE VIRTUAL TABLE IF NOT EXISTS vec_chunks USING vec0(
                embedding float[{EMBEDDING_DIM}]
            );"
        ))?;

        // Migration: add content_hash column if upgrading from older schema
        let has_hash_col: bool = conn
            .prepare("SELECT content_hash FROM chunks LIMIT 0")
            .is_ok();
        if !has_hash_col {
            info!("migrating: adding content_hash column");
            conn.execute_batch(
                "ALTER TABLE chunks ADD COLUMN content_hash BLOB;
                 CREATE UNIQUE INDEX IF NOT EXISTS idx_chunks_content_hash
                     ON chunks(content_hash) WHERE content_hash IS NOT NULL;",
            )?;
        }

        // Migration: add feedback_score column for relevance tracking
        let has_feedback_col: bool = conn
            .prepare("SELECT feedback_score FROM chunks LIMIT 0")
            .is_ok();
        if !has_feedback_col {
            info!("migrating: adding feedback_score column");
            conn.execute_batch(
                "ALTER TABLE chunks ADD COLUMN feedback_score INTEGER NOT NULL DEFAULT 0;",
            )?;
        }

        // Migration: add retrieval tracking columns (implicit negative signal + staleness)
        let has_retrieval_col: bool = conn
            .prepare("SELECT retrieval_count FROM chunks LIMIT 0")
            .is_ok();
        if !has_retrieval_col {
            info!("migrating: adding retrieval_count + last_retrieved columns");
            conn.execute_batch(
                "ALTER TABLE chunks ADD COLUMN retrieval_count INTEGER NOT NULL DEFAULT 0;
                 ALTER TABLE chunks ADD COLUMN last_retrieved TEXT;",
            )?;
        }

        // Ensure content_hash index exists (must come AFTER content_hash migration above).
        conn.execute_batch(
            "CREATE UNIQUE INDEX IF NOT EXISTS idx_chunks_content_hash
                 ON chunks(content_hash) WHERE content_hash IS NOT NULL;",
        )?;

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
    ) -> Result<(Vec<i64>, usize)> {
        let mut ids = Vec::with_capacity(embeddings.len());
        let mut duplicates = 0usize;

        for (i, (emb, text)) in embeddings.iter().zip(texts.iter()).enumerate() {
            let meta = metadata.get(i).map(|s| s.as_str()).unwrap_or("");
            let hash = content_hash(text, source);

            // INSERT OR IGNORE — unique index on content_hash rejects exact duplicates
            let inserted = self.conn.execute(
                "INSERT OR IGNORE INTO chunks (content, source, metadata, content_hash) VALUES (?1, ?2, ?3, ?4)",
                rusqlite::params![text, source, meta, hash],
            )?;

            if inserted == 0 {
                // Duplicate — return existing ID, optionally update metadata
                let existing_id: i64 = self.conn.query_row(
                    "SELECT id FROM chunks WHERE content_hash = ?1",
                    [&hash],
                    |r| r.get(0),
                )?;
                if !meta.is_empty() {
                    self.conn.execute(
                        "UPDATE chunks SET metadata = ?1 WHERE id = ?2",
                        rusqlite::params![meta, existing_id],
                    )?;
                }
                ids.push(existing_id);
                duplicates += 1;
                continue;
            }

            let rowid = self.conn.last_insert_rowid();

            let blob = vec_to_blob(emb);
            self.conn.execute(
                "INSERT INTO vec_chunks (rowid, embedding) VALUES (?1, ?2)",
                rusqlite::params![rowid, blob],
            )?;

            ids.push(rowid);
        }

        Ok((ids, duplicates))
    }

    fn search(
        &self,
        query_embedding: &[f32],
        limit: usize,
        source_filter: &str,
        metadata_filters: &[MetadataFilter],
    ) -> Result<Vec<SearchResult>> {
        let blob = vec_to_blob(query_embedding);

        // Build the base query — sqlite-vec requires MATCH + k in the WHERE clause
        let mut sql = String::from(
            "SELECT v.rowid, v.distance, c.content, c.source, c.metadata, c.feedback_score,
                    c.retrieval_count, c.last_retrieved, c.created
             FROM vec_chunks v
             JOIN chunks c ON c.id = v.rowid
             WHERE v.embedding MATCH ?1
             AND k = ?2",
        );

        let mut params: Vec<Box<dyn rusqlite::types::ToSql>> = vec![
            Box::new(blob),
            Box::new(limit as i64),
        ];
        let mut param_idx = 3;

        if !source_filter.is_empty() {
            sql.push_str(&format!(" AND c.source = ?{param_idx}"));
            params.push(Box::new(source_filter.to_string()));
            param_idx += 1;
        }

        for filter in metadata_filters {
            let json_path = format!("$.{}", filter.field);
            let op_str = match filter.op {
                FilterOp::Eq => "=",
                FilterOp::Gt => ">",
                FilterOp::Lt => "<",
                FilterOp::Gte => ">=",
                FilterOp::Lte => "<=",
                FilterOp::Contains => "LIKE",
            };

            if matches!(filter.op, FilterOp::Contains) {
                let pattern = format!("%{}%", filter.value.as_str().unwrap_or(""));
                sql.push_str(&format!(
                    " AND json_extract(c.metadata, ?{}) {} ?{}",
                    param_idx,
                    op_str,
                    param_idx + 1
                ));
                params.push(Box::new(json_path));
                params.push(Box::new(pattern));
            } else {
                sql.push_str(&format!(
                    " AND json_extract(c.metadata, ?{}) {} ?{}",
                    param_idx,
                    op_str,
                    param_idx + 1
                ));
                params.push(Box::new(json_path));
                match &filter.value {
                    serde_json::Value::Number(n) => {
                        if let Some(f) = n.as_f64() {
                            params.push(Box::new(f));
                        } else {
                            params.push(Box::new(n.as_i64().unwrap_or(0)));
                        }
                    }
                    serde_json::Value::String(s) => params.push(Box::new(s.clone())),
                    other => params.push(Box::new(other.to_string())),
                }
            }
            param_idx += 2;
        }

        sql.push_str(" ORDER BY v.distance");

        let param_refs: Vec<&dyn rusqlite::types::ToSql> = params.iter().map(|p| p.as_ref()).collect();
        let mut stmt = self.conn.prepare(&sql)?;
        let rows = stmt.query_map(&*param_refs, |row| {
            Ok(SearchResult {
                id: row.get(0)?,
                distance: row.get(1)?,
                content: row.get(2)?,
                source: row.get(3)?,
                metadata: row.get(4)?,
                feedback_score: row.get::<_, i64>(5).unwrap_or(0),
                retrieval_count: row.get::<_, i64>(6).unwrap_or(0),
                last_retrieved: row.get::<_, Option<String>>(7).ok().flatten(),
                created: row.get::<_, String>(8).unwrap_or_default(),
            })
        })?;

        let mut results: Vec<SearchResult> = Vec::new();
        for row in rows {
            results.push(row?);
        }
        // Re-sort by adjusted distance: feedback boost - implicit negative - staleness penalty.
        // Lower adjusted distance = better rank.
        let now_days = days_since_epoch_utc();
        results.sort_by(|a, b| {
            let adj_a = adjusted_distance(a, now_days);
            let adj_b = adjusted_distance(b, now_days);
            adj_a.partial_cmp(&adj_b).unwrap_or(std::cmp::Ordering::Equal)
        });
        Ok(results)
    }

    /// Fire-and-forget: mark these chunk IDs as retrieved (increment count, update timestamp).
    /// Silently ignores errors — retrieval tracking is best-effort.
    fn mark_retrieved(&self, ids: &[i64]) {
        if ids.is_empty() { return; }
        let placeholders = std::iter::repeat("?").take(ids.len()).collect::<Vec<_>>().join(",");
        let sql = format!(
            "UPDATE chunks SET retrieval_count = retrieval_count + 1,
                               last_retrieved = datetime('now')
             WHERE id IN ({placeholders})"
        );
        let params: Vec<Box<dyn rusqlite::types::ToSql>> =
            ids.iter().map(|id| Box::new(*id) as Box<dyn rusqlite::types::ToSql>).collect();
        let refs: Vec<&dyn rusqlite::types::ToSql> = params.iter().map(|p| p.as_ref()).collect();
        let _ = self.conn.execute(&sql, &*refs);
    }

    fn update(
        &self,
        id: i64,
        content: Option<&str>,
        metadata: Option<&str>,
        source: Option<&str>,
        new_embedding: Option<&[f32]>,
    ) -> Result<bool> {
        let mut set_clauses = Vec::new();
        let mut params: Vec<Box<dyn rusqlite::types::ToSql>> = Vec::new();
        let mut idx = 1;

        if let Some(c) = content {
            set_clauses.push(format!("content = ?{idx}"));
            params.push(Box::new(c.to_string()));
            idx += 1;
        }
        if let Some(m) = metadata {
            set_clauses.push(format!("metadata = ?{idx}"));
            params.push(Box::new(m.to_string()));
            idx += 1;
        }
        if let Some(s) = source {
            set_clauses.push(format!("source = ?{idx}"));
            params.push(Box::new(s.to_string()));
            idx += 1;
        }

        if set_clauses.is_empty() && new_embedding.is_none() {
            return Ok(false);
        }

        if !set_clauses.is_empty() {
            let sql = format!(
                "UPDATE chunks SET {} WHERE id = ?{}",
                set_clauses.join(", "),
                idx
            );
            params.push(Box::new(id));
            let param_refs: Vec<&dyn rusqlite::types::ToSql> =
                params.iter().map(|p| p.as_ref()).collect();
            self.conn.execute(&sql, &*param_refs)?;
        }

        if let Some(emb) = new_embedding {
            let blob = vec_to_blob(emb);
            // sqlite-vec: delete old + insert new for the same rowid
            self.conn
                .execute("DELETE FROM vec_chunks WHERE rowid = ?1", [id])?;
            self.conn.execute(
                "INSERT INTO vec_chunks (rowid, embedding) VALUES (?1, ?2)",
                rusqlite::params![id, blob],
            )?;
        }

        Ok(true)
    }

    fn list(&self, source_filter: &str, limit: usize, offset: usize) -> Result<(Vec<ListItem>, usize)> {
        let has_filter = !source_filter.is_empty();

        let total: usize = if has_filter {
            self.conn.query_row(
                "SELECT COUNT(*) FROM chunks WHERE source = ?1",
                [source_filter],
                |r| r.get::<_, i64>(0).map(|v| v as usize),
            )?
        } else {
            self.conn.query_row(
                "SELECT COUNT(*) FROM chunks",
                [],
                |r| r.get::<_, i64>(0).map(|v| v as usize),
            )?
        };

        let sql = if has_filter {
            "SELECT id, content, source, metadata, created FROM chunks WHERE source = ?1 ORDER BY created DESC LIMIT ?2 OFFSET ?3"
        } else {
            "SELECT id, content, source, metadata, created FROM chunks WHERE 1=1 ORDER BY created DESC LIMIT ?1 OFFSET ?2"
        };

        let mut stmt = self.conn.prepare(sql)?;
        let mut items = Vec::new();

        if has_filter {
            let rows = stmt.query_map(
                rusqlite::params![source_filter, limit as i64, offset as i64],
                |row| {
                    Ok(ListItem {
                        id: row.get(0)?,
                        content: row.get(1)?,
                        source: row.get(2)?,
                        metadata: row.get(3)?,
                        created: row.get(4)?,
                    })
                },
            )?;
            for row in rows {
                items.push(row?);
            }
        } else {
            let rows = stmt.query_map(
                rusqlite::params![limit as i64, offset as i64],
                |row| {
                    Ok(ListItem {
                        id: row.get(0)?,
                        content: row.get(1)?,
                        source: row.get(2)?,
                        metadata: row.get(3)?,
                        created: row.get(4)?,
                    })
                },
            )?;
            for row in rows {
                items.push(row?);
            }
        }

        Ok((items, total))
    }

    fn feedback(&self, id: i64, useful: bool) -> Result<i64> {
        let delta: i64 = if useful { 1 } else { -1 };
        self.conn.execute(
            "UPDATE chunks SET feedback_score = feedback_score + ?1 WHERE id = ?2",
            rusqlite::params![delta, id],
        )?;
        let new_score: i64 = self.conn.query_row(
            "SELECT feedback_score FROM chunks WHERE id = ?1",
            [id],
            |r| r.get(0),
        )?;
        Ok(new_score)
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

    /// Return staleness candidates split into 3 buckets:
    /// 1. Never retrieved AND older than `never_retrieved_age_days` days
    /// 2. Retrieved > `low_value_min_retrievals` times with feedback_score <= 0
    /// 3. Last retrieved older than `long_dormant_days` days ago
    fn stale_query(&self, req: &StaleRequest) -> Result<StaleResponse> {
        let total: usize = self.conn.query_row(
            "SELECT COUNT(*) FROM chunks", [],
            |r| r.get::<_, i64>(0).map(|v| v as usize),
        )?;

        // Build optional source clause — always put source param at position 3 if present.
        let (src_clause, src_param): (&str, Option<&str>) = if req.source.is_empty() {
            ("", None)
        } else {
            (" AND source = ?3", Some(req.source.as_str()))
        };
        let limit = req.per_bucket_limit as i64;

        let map_row = |row: &rusqlite::Row| -> rusqlite::Result<StaleChunk> {
            let metadata: String = row.get(7)?;
            let (path, filename) = extract_path_fields(&metadata);
            Ok(StaleChunk {
                id: row.get(0)?,
                source: row.get(1)?,
                preview: row.get(2)?,
                retrieval_count: row.get(3)?,
                feedback_score: row.get(4)?,
                last_retrieved: row.get(5)?,
                created: row.get(6)?,
                path,
                filename,
            })
        };

        let run_query = |sql: String, p1: i64| -> Result<Vec<StaleChunk>> {
            let mut stmt = self.conn.prepare(&sql)?;
            let rows = if let Some(s) = src_param {
                stmt.query_map(rusqlite::params![p1, limit, s], map_row)?
                    .collect::<Result<Vec<_>, _>>()?
            } else {
                stmt.query_map(rusqlite::params![p1, limit], map_row)?
                    .collect::<Result<Vec<_>, _>>()?
            };
            Ok(rows)
        };

        let never_retrieved_old = run_query(
            format!(
                "SELECT id, source, substr(content, 1, 200), retrieval_count, feedback_score,
                        last_retrieved, created, metadata
                 FROM chunks
                 WHERE last_retrieved IS NULL
                   AND julianday('now') - julianday(created) > ?1{src_clause}
                 ORDER BY created ASC LIMIT ?2"
            ),
            req.never_retrieved_age_days,
        )?;

        let low_value = run_query(
            format!(
                "SELECT id, source, substr(content, 1, 200), retrieval_count, feedback_score,
                        last_retrieved, created, metadata
                 FROM chunks
                 WHERE retrieval_count > ?1 AND feedback_score <= 0{src_clause}
                 ORDER BY retrieval_count DESC LIMIT ?2"
            ),
            req.low_value_min_retrievals,
        )?;

        let long_dormant = run_query(
            format!(
                "SELECT id, source, substr(content, 1, 200), retrieval_count, feedback_score,
                        last_retrieved, created, metadata
                 FROM chunks
                 WHERE last_retrieved IS NOT NULL
                   AND julianday('now') - julianday(last_retrieved) > ?1{src_clause}
                 ORDER BY last_retrieved ASC LIMIT ?2"
            ),
            req.long_dormant_days,
        )?;

        Ok(StaleResponse {
            never_retrieved_old,
            low_value,
            long_dormant,
            total_chunks: total,
        })
    }

    fn stats(&self, db_path: &str) -> Result<StatsResponse> {
        let total: usize = self
            .conn
            .query_row("SELECT COUNT(*) FROM chunks", [], |r| r.get::<_, i64>(0).map(|v| v as usize))?;
        let db_size = std::fs::metadata(db_path).map(|m| m.len()).unwrap_or(0);

        let mut by_source = Vec::new();
        let mut stmt = self.conn.prepare(
            "SELECT source, COUNT(*) FROM chunks GROUP BY source ORDER BY COUNT(*) DESC"
        )?;
        let rows = stmt.query_map([], |row| {
            Ok(SourceCount {
                source: row.get(0)?,
                count: row.get::<_, i64>(1).map(|v| v as usize)?,
            })
        })?;
        for row in rows {
            by_source.push(row?);
        }

        Ok(StatsResponse {
            total_vectors: total,
            db_size_bytes: db_size,
            model_loaded: false, // caller fills runtime fields
            model_circuit: String::new(),
            embed_cache_entries: 0,
            embed_cache_hits: 0,
            embed_cache_misses: 0,
            by_source,
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
    model_id: String,
    db: VectorDb,
    db_path: String,
    idle_timeout_secs: u64,
    model_breaker: CircuitBreaker,
    embed_cache: EmbeddingCache,
}

#[tokio::main]
async fn main() -> Result<()> {
    let _log = cosmix_daemon::init_tracing("cosmix_indexd");

    let cfg = cosmix_config::store::load().unwrap_or_default().embed;

    // CLI --f32 flag overrides config; env var COSMIX_VECTORS_DB overrides config db path
    let dtype = if std::env::args().any(|a| a == "--f32") || cfg.dtype == "f32" {
        DType::F32
    } else {
        DType::F16
    };
    let db_path = std::env::var("COSMIX_VECTORS_DB").unwrap_or(cfg.vectors_db);
    let socket_path = cfg.socket_path;
    let model_id = cfg.model_id;
    let idle_timeout_secs = cfg.idle_timeout_secs;

    let listener = if let Ok(listener) = try_systemd_socket() {
        info!("using systemd socket activation");
        listener
    } else {
        let socket_dir = std::path::Path::new(&socket_path)
            .parent()
            .map(|p| p.to_string_lossy().to_string())
            .unwrap_or_else(|| "/run/cosmix".into());
        std::fs::create_dir_all(&socket_dir)
            .with_context(|| format!("creating {socket_dir}"))?;
        let _ = std::fs::remove_file(&socket_path);
        let listener = UnixListener::bind(&socket_path)
            .with_context(|| format!("binding {socket_path}"))?;
        std::fs::set_permissions(
            &socket_path,
            std::os::unix::fs::PermissionsExt::from_mode(0o666),
        )?;
        info!("listening on {socket_path}");
        listener
    };

    let db = VectorDb::open(&db_path)?;

    let state = Arc::new(Mutex::new(AppState {
        model: None,
        dtype,
        model_id,
        db,
        db_path,
        idle_timeout_secs,
        model_breaker: CircuitBreaker::new(2, Duration::from_secs(60)),
        embed_cache: EmbeddingCache::new(),
    }));

    let (activity_tx, mut activity_rx) = tokio::sync::mpsc::channel::<()>(16);

    info!("ready — model loads on first request, unloads after {idle_timeout_secs}s idle");

    // Spawn AMP hub registration (non-blocking — if hub isn't running, indexd still works)
    let amp_state = state.clone();
    let amp_activity_tx = activity_tx.clone();
    tokio::spawn(async move {
        match cosmix_client::HubClient::connect_default("indexd").await {
            Ok(client) => {
                info!("registered as AMP service 'indexd' on hub");
                let client = std::sync::Arc::new(client);
                handle_amp_commands(client, amp_state, amp_activity_tx).await;
            }
            Err(e) => {
                info!("hub not available, running socket-only mode: {e}");
            }
        }
    });

    // Spawn idle watchdog
    let watchdog_state = state.clone();
    tokio::spawn(async move {
        loop {
            if activity_rx.recv().await.is_none() {
                break;
            }
            while activity_rx.try_recv().is_ok() {}

            let timeout = watchdog_state.lock().await.idle_timeout_secs;
            let mut idle_remaining = timeout;
            loop {
                tokio::select! {
                    _ = tokio::time::sleep(Duration::from_secs(1)) => {
                        idle_remaining = idle_remaining.saturating_sub(1);
                        if idle_remaining == 0 {
                            let mut guard = watchdog_state.lock().await;
                            if guard.model.is_some() {
                                info!("model idle for {timeout}s, unloading to free memory");
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
                        idle_remaining = timeout;
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

/// Handle incoming AMP commands from the hub mesh.
/// Maps AMP commands to the same JSON protocol used by the Unix socket.
async fn handle_amp_commands(
    client: std::sync::Arc<cosmix_client::HubClient>,
    state: Arc<Mutex<AppState>>,
    activity_tx: tokio::sync::mpsc::Sender<()>,
) {
    let mut rx = match client.incoming_async().await {
        Some(rx) => rx,
        None => return,
    };

    while let Some(cmd) = rx.recv().await {
        // The AMP command args ARE the JSON request, just add the "action" field
        // e.g. amp_call("indexd", "indexd.search", {"query": "...", "limit": 5})
        // becomes {"action": "search", "query": "...", "limit": 5}
        let action = cmd.command.strip_prefix("indexd.").unwrap_or(&cmd.command);

        let request_json = if cmd.args.is_object() {
            let mut args = cmd.args.clone();
            args.as_object_mut().unwrap().insert("action".into(), serde_json::Value::String(action.into()));
            args.to_string()
        } else {
            serde_json::json!({"action": action}).to_string()
        };

        let response = process_request(&request_json, &state, &activity_tx).await;

        // Check if response is an error
        let rc = if response.contains("\"error\"") { 10 } else { 0 };
        if let Err(e) = client.respond(&cmd, rc, &response).await {
            error!("failed to send AMP response: {e}");
        }
    }

    info!("hub connection closed");
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
        Request::Update(req) => handle_update(req, state, activity_tx).await,
        Request::Delete(req) => handle_delete(req, state).await,
        Request::List(req) => handle_list(req, state).await,
        Request::Feedback(req) => handle_feedback(req, state).await,
        Request::Stale(req) => handle_stale(req, state).await,
        Request::Stats => handle_stats(state).await,
    }
}

async fn ensure_model(state: &mut AppState) -> Result<()> {
    if state.model.is_some() {
        return Ok(());
    }
    if !state.model_breaker.allow_request() {
        anyhow::bail!(
            "model loading suspended (circuit {}, cooldown active)",
            state.model_breaker.state_name()
        );
    }
    info!("loading model on demand...");
    match EmbedModel::load(state.dtype, &state.model_id) {
        Ok(model) => {
            state.model = Some(model);
            state.model_breaker.record_success();
            Ok(())
        }
        Err(e) => {
            state.model_breaker.record_failure();
            Err(e)
        }
    }
}

async fn handle_embed(
    req: EmbedRequest,
    state: &Arc<Mutex<AppState>>,
    activity_tx: &tokio::sync::mpsc::Sender<()>,
) -> String {
    let mut guard = state.lock().await;

    // Check cache for each text, only embed uncached ones
    let (mut cached_results, needs_embed) =
        guard.embed_cache.lookup_batch(&req.texts, &req.prefix);

    if needs_embed.is_empty() {
        // All cache hits — no model needed
        let embeddings: Vec<Vec<f32>> = cached_results
            .into_iter()
            .map(|o| o.unwrap())
            .collect();
        return serde_json::to_string(&EmbedResponse { embeddings }).unwrap();
    }

    if let Err(e) = ensure_model(&mut guard).await {
        return json_error(&format!("model load failed: {e}"));
    }
    let model = guard.model.as_ref().unwrap();

    let texts_to_embed: Vec<String> = needs_embed.iter().map(|&i| req.texts[i].clone()).collect();
    match model.embed(&texts_to_embed, &req.prefix) {
        Ok(new_embeddings) => {
            let _ = activity_tx.send(()).await;
            // Fill cached_results with freshly computed embeddings and cache them
            for (embed_idx, &original_idx) in needs_embed.iter().enumerate() {
                let emb = new_embeddings[embed_idx].clone();
                guard
                    .embed_cache
                    .store(&req.texts[original_idx], &req.prefix, emb.clone());
                cached_results[original_idx] = Some(emb);
            }
            let embeddings: Vec<Vec<f32>> = cached_results
                .into_iter()
                .map(|o| o.unwrap())
                .collect();
            serde_json::to_string(&EmbedResponse { embeddings }).unwrap()
        }
        Err(e) => json_error(&e.to_string()),
    }
}

/// Days since Unix epoch, UTC. Used for staleness penalties without a date crate.
fn days_since_epoch_utc() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| (d.as_secs() / 86400) as i64)
        .unwrap_or(0)
}

/// Parse "YYYY-MM-DD HH:MM:SS" (SQLite datetime('now')) or "YYYY-MM-DDTHH:MM:SS" to days-since-epoch.
/// Returns None on any parse failure (no date crate — naive conversion using days-per-month).
fn sqlite_datetime_to_days(s: &str) -> Option<i64> {
    if s.len() < 10 { return None; }
    let b = s.as_bytes();
    if b[4] != b'-' || b[7] != b'-' { return None; }
    let y: i64 = s[0..4].parse().ok()?;
    let m: i64 = s[5..7].parse().ok()?;
    let d: i64 = s[8..10].parse().ok()?;
    // Days from epoch: count years+leap days, month days, day of month.
    // Using the common algorithm (Howard Hinnant's date algorithms, shifted-year form).
    let y = y - (if m <= 2 { 1 } else { 0 });
    let era = if y >= 0 { y } else { y - 399 } / 400;
    let yoe = (y - era * 400) as i64;
    let doy = (153 * (if m > 2 { m - 3 } else { m + 9 }) + 2) / 5 + d - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    Some(era * 146097 + doe - 719468)
}

/// Compute ranking-adjusted distance for a search result.
/// Lower is better. Combines: feedback_score boost, implicit negative signal, staleness penalty.
fn adjusted_distance(r: &SearchResult, now_days: i64) -> f64 {
    let feedback_boost = r.feedback_score as f64 * 0.05;

    // Implicit negative: retrieved >3 times with non-positive feedback = weak penalty.
    let implicit_penalty = if r.retrieval_count > 3 && r.feedback_score <= 0 {
        (r.retrieval_count as f64 - 3.0) * 0.01
    } else {
        0.0
    };

    // Staleness penalty: never-retrieved old chunks, or long-unretrieved chunks.
    let staleness_penalty = match r.last_retrieved.as_deref() {
        None => {
            // Never retrieved — penalize if chunk is old.
            if let Some(created_days) = sqlite_datetime_to_days(&r.created) {
                let age = now_days - created_days;
                if age > 90 { 0.03 } else { 0.0 }
            } else { 0.0 }
        }
        Some(lr) => {
            if let Some(lr_days) = sqlite_datetime_to_days(lr) {
                let since = now_days - lr_days;
                if since > 180 { 0.02 } else { 0.0 }
            } else { 0.0 }
        }
    };

    r.distance - feedback_boost + implicit_penalty + staleness_penalty
}

/// Extract `path` and `filename` from a JSON metadata string.
fn extract_path_fields(metadata: &str) -> (Option<String>, Option<String>) {
    let meta: serde_json::Value = match serde_json::from_str(metadata) {
        Ok(v) => v,
        Err(_) => return (None, None),
    };
    let path = meta.get("path").and_then(|v| v.as_str()).map(String::from);
    let filename = meta.get("filename").and_then(|v| v.as_str()).map(String::from);
    (path, filename)
}

/// Quick YYYY-MM-DD validator (no external date crate needed).
fn is_valid_ymd(s: &str) -> bool {
    if s.len() != 10 { return false; }
    let b = s.as_bytes();
    if b[4] != b'-' || b[7] != b'-' { return false; }
    let year: u32 = match s[0..4].parse() { Ok(n) => n, Err(_) => return false };
    let month: u32 = match s[5..7].parse() { Ok(n) => n, Err(_) => return false };
    let day: u32 = match s[8..10].parse() { Ok(n) => n, Err(_) => return false };
    year >= 1970 && year <= 9999 && (1..=12).contains(&month) && (1..=31).contains(&day)
}

/// Validate source type + required metadata fields. Enforces layer separation:
/// each source type has a distinct contract, not just a label.
fn validate_store_entry(source: &str, metadata_json: &str) -> Result<(), String> {
    // Skip validation for empty metadata (legacy/test path)
    if metadata_json.is_empty() {
        return Ok(());
    }
    let meta: serde_json::Value = serde_json::from_str(metadata_json)
        .map_err(|e| format!("metadata is not valid JSON: {e}"))?;
    let has = |f: &str| meta.get(f).is_some_and(|v| !v.is_null());
    match source {
        "skill" => {
            for f in ["name", "trigger", "approach"] {
                if !has(f) {
                    return Err(format!("skill metadata missing required field: {f}"));
                }
            }
        }
        "doc" => {
            for f in ["path", "domain"] {
                if !has(f) {
                    return Err(format!("doc metadata missing required field: {f}"));
                }
            }
        }
        "journal" => {
            for f in ["path", "domain", "date"] {
                if !has(f) {
                    return Err(format!("journal metadata missing required field: {f}"));
                }
            }
            let date_str = meta.get("date").and_then(|v| v.as_str()).unwrap_or("");
            if !is_valid_ymd(date_str) {
                return Err(format!("journal date '{date_str}' not valid YYYY-MM-DD"));
            }
        }
        "memory" => {
            // CMM-generated observations. Requires generator + date for provenance.
            for f in ["path", "domain", "date", "generator"] {
                if !has(f) {
                    return Err(format!("memory metadata missing required field: {f}"));
                }
            }
            let date_str = meta.get("date").and_then(|v| v.as_str()).unwrap_or("");
            if !is_valid_ymd(date_str) {
                return Err(format!("memory date '{date_str}' not valid YYYY-MM-DD"));
            }
        }
        "" => {} // allow unset source (some callers use empty)
        other => return Err(format!("unknown source type: {other}")),
    }
    Ok(())
}

async fn handle_store(
    req: StoreRequest,
    state: &Arc<Mutex<AppState>>,
    activity_tx: &tokio::sync::mpsc::Sender<()>,
) -> String {
    // Layer separation enforcement: validate each entry's metadata
    for meta_str in req.metadata.iter() {
        if let Err(e) = validate_store_entry(&req.source, meta_str) {
            return json_error(&format!("validation failed: {e}"));
        }
    }

    let mut guard = state.lock().await;

    let prefix = "search_document: ";

    // Use embedding cache for store too
    let (mut cached_results, needs_embed) =
        guard.embed_cache.lookup_batch(&req.texts, prefix);

    if !needs_embed.is_empty() {
        if let Err(e) = ensure_model(&mut guard).await {
            return json_error(&format!("model load failed: {e}"));
        }
        let model = guard.model.as_ref().unwrap();

        let texts_to_embed: Vec<String> =
            needs_embed.iter().map(|&i| req.texts[i].clone()).collect();
        match model.embed(&texts_to_embed, prefix) {
            Ok(new_embeddings) => {
                let _ = activity_tx.send(()).await;
                for (embed_idx, &original_idx) in needs_embed.iter().enumerate() {
                    let emb = new_embeddings[embed_idx].clone();
                    guard
                        .embed_cache
                        .store(&req.texts[original_idx], prefix, emb.clone());
                    cached_results[original_idx] = Some(emb);
                }
            }
            Err(e) => return json_error(&format!("embedding failed: {e}")),
        }
    }

    let embeddings: Vec<Vec<f32>> = cached_results.into_iter().map(|o| o.unwrap()).collect();

    match guard
        .db
        .store(&embeddings, &req.texts, &req.source, &req.metadata)
    {
        Ok((ids, duplicates)) => {
            let stored = ids.len() - duplicates;
            serde_json::to_string(&StoreResponse {
                stored,
                duplicates,
                ids,
            })
            .unwrap()
        }
        Err(e) => json_error(&format!("store failed: {e}")),
    }
}

async fn handle_search(
    req: SearchRequest,
    state: &Arc<Mutex<AppState>>,
    activity_tx: &tokio::sync::mpsc::Sender<()>,
) -> String {
    let mut guard = state.lock().await;

    let prefix = "search_query: ";

    // Check embedding cache — avoids model load for repeated queries
    let query_emb = if let Some(cached) = guard.embed_cache.lookup(&req.query, prefix) {
        cached
    } else {
        if let Err(e) = ensure_model(&mut guard).await {
            return json_error(&format!("model load failed: {e}"));
        }
        let model = guard.model.as_ref().unwrap();

        match model.embed(&[req.query.clone()], prefix) {
            Ok(mut e) => {
                if e.is_empty() {
                    return json_error("empty query");
                }
                let _ = activity_tx.send(()).await;
                let emb = e.remove(0);
                guard.embed_cache.store(&req.query, prefix, emb.clone());
                emb
            }
            Err(e) => return json_error(&format!("embedding failed: {e}")),
        }
    };

    match guard
        .db
        .search(&query_emb, req.limit, &req.source, &req.metadata_filter)
    {
        Ok(results) => {
            // Fire-and-forget: track that these chunks were retrieved (implicit feedback signal).
            let ids: Vec<i64> = results.iter().map(|r| r.id).collect();
            guard.db.mark_retrieved(&ids);
            serde_json::to_string(&SearchResponse { results }).unwrap()
        }
        Err(e) => json_error(&format!("search failed: {e}")),
    }
}

async fn handle_update(
    req: UpdateRequest,
    state: &Arc<Mutex<AppState>>,
    activity_tx: &tokio::sync::mpsc::Sender<()>,
) -> String {
    let mut guard = state.lock().await;

    let prefix = "search_document: ";

    // Re-embed if content changed — check cache first
    let new_embedding = if let Some(ref content) = req.content {
        if let Some(cached) = guard.embed_cache.lookup(content, prefix) {
            Some(cached)
        } else {
            if let Err(e) = ensure_model(&mut guard).await {
                return json_error(&format!("model load failed: {e}"));
            }
            let model = guard.model.as_ref().unwrap();
            match model.embed(&[content.clone()], prefix) {
                Ok(mut embs) => {
                    let _ = activity_tx.send(()).await;
                    let emb = embs.remove(0);
                    guard.embed_cache.store(content, prefix, emb.clone());
                    Some(emb)
                }
                Err(e) => return json_error(&format!("embedding failed: {e}")),
            }
        }
    } else {
        None
    };

    match guard.db.update(
        req.id,
        req.content.as_deref(),
        req.metadata.as_deref(),
        req.source.as_deref(),
        new_embedding.as_deref(),
    ) {
        Ok(updated) => serde_json::to_string(&UpdateResponse {
            updated,
            re_embedded: new_embedding.is_some(),
        })
        .unwrap(),
        Err(e) => json_error(&format!("update failed: {e}")),
    }
}

async fn handle_delete(req: DeleteRequest, state: &Arc<Mutex<AppState>>) -> String {
    let guard = state.lock().await;
    match guard.db.delete(&req.ids) {
        Ok(deleted) => serde_json::to_string(&DeleteResponse { deleted }).unwrap(),
        Err(e) => json_error(&format!("delete failed: {e}")),
    }
}

async fn handle_list(req: ListRequest, state: &Arc<Mutex<AppState>>) -> String {
    let guard = state.lock().await;
    match guard.db.list(&req.source, req.limit, req.offset) {
        Ok((items, total)) => serde_json::to_string(&ListResponse { items, total }).unwrap(),
        Err(e) => json_error(&format!("list failed: {e}")),
    }
}

async fn handle_feedback(req: FeedbackRequest, state: &Arc<Mutex<AppState>>) -> String {
    let guard = state.lock().await;
    match guard.db.feedback(req.id, req.useful) {
        Ok(new_score) => {
            serde_json::json!({"ok": true, "id": req.id, "feedback_score": new_score}).to_string()
        }
        Err(e) => json_error(&format!("feedback failed: {e}")),
    }
}

async fn handle_stale(req: StaleRequest, state: &Arc<Mutex<AppState>>) -> String {
    let guard = state.lock().await;
    match guard.db.stale_query(&req) {
        Ok(resp) => serde_json::to_string(&resp).unwrap(),
        Err(e) => json_error(&format!("stale query failed: {e}")),
    }
}

async fn handle_stats(state: &Arc<Mutex<AppState>>) -> String {
    let guard = state.lock().await;
    match guard.db.stats(&guard.db_path) {
        Ok(mut stats) => {
            stats.model_loaded = guard.model.is_some();
            stats.model_circuit = guard.model_breaker.state_name().to_string();
            stats.embed_cache_entries = guard.embed_cache.entries.len();
            stats.embed_cache_hits = guard.embed_cache.hits;
            stats.embed_cache_misses = guard.embed_cache.misses;
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
