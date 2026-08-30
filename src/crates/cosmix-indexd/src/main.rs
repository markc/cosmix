// Use system allocator — it returns memory to OS on free, unlike mimalloc
// which holds freed pages in its pool. Critical for model unload to actually
// reduce RSS.

mod metrics;
mod props;
mod vindex;
mod world;

use metrics::{METRICS, Outcome, ReqTiming, RequestContext, classify_response};

use anyhow::{Context, Result};
use candle_core::{DType, Device, Tensor};
use candle_nn::VarBuilder;
use candle_transformers::models::nomic_bert::{self, NomicBertModel};
use clap::Parser;
use hf_hub::{Cache, Repo, RepoType, api::sync::Api};
use rusqlite::{Connection, OptionalExtension};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, HashMap};
use std::io::Read;
use std::os::unix::fs::OpenOptionsExt;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU8, Ordering};
use std::sync::{Arc, OnceLock};
use std::time::{Duration, Instant};
use tokenizers::Tokenizer;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::UnixListener;
use tokio::sync::Mutex;
use tracing::{error, info, warn};

/// System-mode config directory. The systemd unit's config lives here;
/// an ad-hoc CLI invocation falls through to it after `--config` and
/// before the XDG user-mode fallback. The directory's config file is
/// `config.conf.mix` (the legacy `.toml` fallback was removed in C11 —
/// see `_doc/planned/2026-05-31-c11-toml-fallback-removal.md`).
const SYSTEM_CFG_DIR: &str = "/etc/cosmix/indexd";

/// Per-service config loaded at startup from one of the paths searched
/// by [`load_indexd_config`]. Set once in `main()`; consulted by
/// `validate_store_entry` on every incoming chunk. Lock-free reads
/// because the value never changes during a single process lifetime.
static INDEXD_CFG: OnceLock<Config> = OnceLock::new();

/// Absolute path the config was actually loaded from. Lets
/// `validate_store_entry` point unknown-source-type errors back at the
/// file the operator must edit, rather than hard-coding one of the
/// search-path candidates.
static INDEXD_CFG_PATH: OnceLock<PathBuf> = OnceLock::new();

fn indexd_cfg() -> &'static Config {
    INDEXD_CFG.get().expect(
        "INDEXD_CFG not initialized — main() must call INDEXD_CFG.set() before serving requests",
    )
}

/// Indexd owns its memory/admission settings locally so this hardening arc
/// stays within the daemon crate. The shared config crate's source-policy
/// types remain reusable, while old deployed files deserialize unchanged
/// through `#[serde(default)]`.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
struct Config {
    schema_version: u32,
    service: ServiceConfig,
    source_types: BTreeMap<String, cosmix_config::SourceTypeSpec>,
}

impl Default for Config {
    fn default() -> Self {
        let base = cosmix_config::IndexdSettings::default();
        Self {
            schema_version: base.schema_version,
            service: ServiceConfig::from_shared(base.service),
            source_types: base.source_types,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
struct ServiceConfig {
    vectors_db: String,
    model_id: String,
    socket_path: String,
    idle_timeout_secs: u64,
    dtype: String,
    vindex_dtype: String,

    // Memory/admission bounds (bytes are decoded payload bytes, not chars):
    // - max_sequence_tokens: tokenizer truncation ceiling, clamped to the
    //   trained 2048-token range, model n_positions, and max_forward_tokens.
    // - max_forward_tokens: maximum padded tokens in one model forward.
    // - max_request_text_bytes: aggregate UTF-8 bytes across embedded texts,
    //   including one copy of the prefix per text.
    // - max_prefix_bytes: standalone UTF-8 byte cap for an embedding prefix.
    // - max_request_line_bytes: complete newline-delimited JSON ingress cap.
    // - max_response_bytes: maximum serialised socket response; list/search
    //   stop materialising full-content rows before crossing this allowance.
    // - max_ingress_bytes: aggregate allowance for concurrent socket frames.
    // - connection_idle_timeout_secs: maximum pause between frame reads.
    // - request_frame_timeout_secs: total first-byte-to-newline deadline.
    // - max_connections: socket admission.
    // - inference_admission_timeout_secs: maximum wait for the model permit.
    // - background_queue_max_bytes/jobs: queued index_file ownership budget.
    // - background_retry_max_attempts/initial_backoff_secs: bounded retry of
    //   accepted jobs deferred by transient inference/model admission.
    // - max_file_bytes: both on-disk and caller-supplied index_file bodies.
    // - journal_size_limit_bytes: retained main-database WAL ceiling.
    // - vindex_dtype: in-memory mirror precision (f32 default; f16 opt-in).
    max_sequence_tokens: usize,
    max_forward_tokens: usize,
    max_request_text_bytes: usize,
    max_prefix_bytes: usize,
    max_request_line_bytes: usize,
    max_response_bytes: usize,
    max_ingress_bytes: usize,
    connection_idle_timeout_secs: u64,
    request_frame_timeout_secs: u64,
    max_connections: usize,
    inference_admission_timeout_secs: u64,
    background_queue_max_bytes: usize,
    background_queue_max_jobs: usize,
    background_retry_max_attempts: usize,
    background_retry_initial_backoff_secs: u64,
    max_file_bytes: usize,
    journal_size_limit_bytes: u64,
}

impl ServiceConfig {
    fn from_shared(base: cosmix_config::IndexdServiceSettings) -> Self {
        Self {
            vectors_db: base.vectors_db,
            model_id: base.model_id,
            socket_path: base.socket_path,
            idle_timeout_secs: base.idle_timeout_secs,
            dtype: base.dtype,
            vindex_dtype: "f32".to_string(),
            max_sequence_tokens: 2048,
            // At 2048 tokens, four or eight sequences still create large
            // quadratic attention tensors. Two full-length sequences per
            // forward leaves headroom below the separate 2 GiB service cap.
            max_forward_tokens: 4096,
            max_request_text_bytes: 8 * 1024 * 1024,
            max_prefix_bytes: 4 * 1024,
            max_request_line_bytes: 20 * 1024 * 1024,
            max_response_bytes: 20 * 1024 * 1024,
            max_ingress_bytes: 64 * 1024 * 1024,
            connection_idle_timeout_secs: 300,
            request_frame_timeout_secs: 600,
            max_connections: 64,
            inference_admission_timeout_secs: 30,
            background_queue_max_bytes: 128 * 1024 * 1024,
            background_queue_max_jobs: 512,
            background_retry_max_attempts: 5,
            background_retry_initial_backoff_secs: 2,
            max_file_bytes: 16 * 1024 * 1024,
            journal_size_limit_bytes: 64 * 1024 * 1024,
        }
    }
}

impl Default for ServiceConfig {
    fn default() -> Self {
        Self::from_shared(cosmix_config::IndexdServiceSettings::default())
    }
}

/// `--version` line with git sha + build time (version-discovery
/// contract). `COSMIX_*` set by build.rs → `cosmix_buildinfo::emit()`.
const VERSION: &str = concat!(
    env!("CARGO_PKG_VERSION"),
    " (",
    env!("COSMIX_GIT_SHA"),
    ", built ",
    env!("COSMIX_BUILD_TIME"),
    ")"
);

#[derive(Parser, Debug)]
#[command(
    name = "cosmix-indexd",
    version = VERSION,
    about = "Semantic indexing + vector storage daemon using nomic-embed-text via candle"
)]
struct Cli {
    /// Path to the indexd config (`.conf.mix`).
    /// Default search order when this flag is absent:
    ///   1. `/etc/cosmix/indexd/config.conf.mix` — system mode; what the
    ///      systemd unit reads.
    ///   2. `~/.config/cosmix/indexd.conf.mix` — dev-mode fallback;
    ///      auto-materialised with defaults if missing.
    #[arg(short = 'c', long)]
    config: Option<PathBuf>,

    /// Force the embedding model to f32 precision regardless of the
    /// config's `dtype` field. Folds the legacy bare `--f32` flag into
    /// clap so all argument parsing flows through a single path.
    #[arg(long = "f32")]
    force_f32: bool,
}

/// Resolve and load the indexd config per the search order documented
/// on [`Cli::config`]. Returns `(loaded_path, settings)`.
///
/// Only `NotFound` falls through. Permission-denied / I/O errors /
/// parse errors all hard-fail with the offending path in the error
/// chain so a typo or wrong perms can't silently re-route the daemon to
/// a different config.
fn load_indexd_config(cli_path: Option<&Path>) -> Result<(PathBuf, Config)> {
    // 1. Explicit `--config <path>` — hard-load the `.conf.mix` file.
    //    NotFound is an error here because the operator named this file
    //    explicitly.
    if let Some(p) = cli_path {
        let cfg = cosmix_config::load_conf_mix_path::<Config>(p)
            .with_context(|| format!("--config {}", p.display()))?;
        return Ok((p.to_path_buf(), cfg));
    }

    // 2. System mode — read config.conf.mix.
    //    NotFound falls through; any other error hard-fails.
    if let Some((path, cfg)) = load_system_config(Path::new(SYSTEM_CFG_DIR))? {
        return Ok((path, cfg));
    }

    // 3. Dev mode — `load_service` auto-materialises defaults on
    //    missing so the file is always a self-documenting view of
    //    live config (now `.conf.mix`, per C4).
    let cfg = cosmix_config::store::load_service::<Config>("indexd")
        .context("loading user-mode indexd config")?;
    let path = cosmix_config::store::config_dir().join("indexd.conf.mix");
    Ok((path, cfg))
}

/// System-mode resolution within `dir`: read `config.conf.mix`. Returns
/// `Ok(None)` only when the file does not exist; any other I/O error (bad
/// perms, etc.) or a parse error hard-fails with the path in the error
/// chain — preserving the "a typo or wrong perms can't silently
/// re-route the daemon" invariant.
fn load_system_config(dir: &Path) -> Result<Option<(PathBuf, Config)>> {
    let conf_mix = dir.join("config.conf.mix");
    match std::fs::read_to_string(&conf_mix) {
        Ok(content) => {
            let cfg: Config = cosmix_config::from_conf_mix_str(&content)
                .map_err(|e| anyhow::anyhow!("parsing {}: {e}", conf_mix.display()))?;
            Ok(Some((conf_mix, cfg)))
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(e) => {
            Err(anyhow::Error::new(e)).with_context(|| format!("reading {}", conf_mix.display()))
        }
    }
}

const EMBEDDING_DIM: usize = 768;

/// Wall-clock cap on a single off-lock embed batch. A hung candle forward
/// pass must not pin the embed circuit breaker open forever; on expiry the
/// The request returns an error on expiry, but the blocking worker retains its
/// inference permit until the OS thread really exits.
const EMBED_TIMEOUT_SECS: u64 = 120;

/// Max texts accepted in a single embed/store request. Token/byte budgets are
/// enforced separately; count alone is not a memory bound.
const MAX_TEXTS_PER_REQUEST: usize = 256;

/// Max metadata filters in a single search. Each filter adds a bound SQL
/// clause; an unbounded array forces O(n) query-string assembly + param
/// boxing. A real query uses a handful.
const MAX_METADATA_FILTERS: usize = 32;

/// Max sections one `index_file` request may produce after splitting
/// caller-supplied `content`. Each section is embedded sequentially, so
/// without this a single request (foreground OR background) can
/// monopolise the embedding worker indefinitely — bypassing
/// `MAX_TEXTS_PER_REQUEST` because each internal store is a 1-element
/// batch. Generous for a large real document (≈ content_bytes /
/// MAX_CHUNK_CHARS), low enough to reject a flood of tiny sections.
const MAX_INDEX_FILE_SECTIONS: usize = 4000;

fn transient_background_failure(response: &str) -> bool {
    serde_json::from_str::<serde_json::Value>(response)
        .ok()
        .and_then(|value| value.get("error")?.as_str().map(str::to_owned))
        .is_some_and(|error| {
            error.contains("inference_busy:")
                || error.contains("model loading suspended")
                || error.contains("embed circuit open")
        })
}

fn background_retry_backoff(initial_secs: u64, failed_attempt: usize) -> Duration {
    let mut backoff = Duration::from_secs(initial_secs.clamp(1, 60));
    for _ in 1..failed_attempt {
        if backoff >= Duration::from_secs(60) {
            break;
        }
        backoff = (backoff * 2).min(Duration::from_secs(60));
    }
    backoff
}

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
                    warn!(
                        "model circuit breaker OPEN after {} consecutive failures",
                        self.consecutive_failures
                    );
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
    for byte in source
        .bytes()
        .chain(b":".iter().copied())
        .chain(text.bytes())
    {
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
    Supersede(SupersedeRequest),
    Stale(StaleRequest),
    IndexFile(IndexFileRequest),
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
struct SupersedeRequest {
    /// ID of the older chunk being superseded (will be hidden from context_search).
    old_id: i64,
    /// ID of the newer chunk that replaces it.
    new_id: i64,
    /// Optional human-readable reason (logged but not persisted in this version).
    #[serde(default)]
    reason: String,
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

#[derive(Deserialize, Clone)]
struct IndexFileRequest {
    /// Absolute path to the markdown file to index. Used for metadata
    /// (path/filename/date) and source/domain auto-detection. When
    /// `content` is supplied the file itself is never read, so the path
    /// need not be readable by the daemon — only well-formed.
    path: String,
    /// Optional file body. When present, the daemon indexes this text
    /// instead of reading `path` from disk. This is the only way to index
    /// files the daemon cannot read itself: under `ProtectSystem=strict`
    /// with `ProtectHome=yes` the `/home/<user>/…` tree is invisible to
    /// the indexd sandbox, so a git post-commit hook (running as the user,
    /// who *can* read the file) reads it and passes the bytes here.
    #[serde(default)]
    content: Option<String>,
    /// Source type: "doc" or "journal". Auto-detected from path if omitted.
    #[serde(default)]
    source: String,
    /// Domain name (e.g. "cosmix"). Auto-detected from path if omitted.
    #[serde(default)]
    domain: String,
    /// Opt-in fire-and-forget mode. When `true`, `index_file` enqueues the
    /// request onto the background job queue and returns an immediate ack
    /// instead of indexing synchronously — so a large doc whose sequential
    /// per-section embed exceeds the caller's Bus `send` timeout no longer
    /// produces a false `rc=10` while indexd quietly finishes. Defaults to
    /// `false`, preserving the synchronous path byte-for-byte for existing
    /// callers (MCP, interactive). The field is named `background` rather
    /// than `async` because `async` is a Rust keyword.
    #[serde(default)]
    background: bool,
}

fn default_stale_age_days() -> i64 {
    90
}
fn default_low_value_retrievals() -> i64 {
    3
}
fn default_dormant_days() -> i64 {
    180
}
fn default_stale_limit() -> usize {
    50
}

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

const MAX_LIST_LIMIT: usize = 1000;
const MAX_STALE_BUCKET_LIMIT: usize = 200;

fn clamp_list_limit(limit: usize) -> usize {
    limit.min(MAX_LIST_LIMIT)
}

fn clamp_stale_limit(limit: usize) -> usize {
    limit.min(MAX_STALE_BUCKET_LIMIT)
}

fn sqlite_count(value: usize, field: &str) -> Result<i64, String> {
    i64::try_from(value).map_err(|_| format!("{field} exceeds SQLite integer range"))
}

fn charge_response_item(used: &mut usize, fields: &[&str], max_bytes: usize) -> Result<()> {
    let item_bytes = fields.iter().try_fold(256usize, |total, field| {
        let serialised_len = serde_json::to_string(field)?.len();
        total
            .checked_add(serialised_len)
            .ok_or_else(|| anyhow::anyhow!("response byte count overflow"))
    })?;
    let next = used
        .checked_add(item_bytes)
        .ok_or_else(|| anyhow::anyhow!("response byte count overflow"))?;
    if next > max_bytes {
        anyhow::bail!("full-content response exceeds configured max {max_bytes} bytes");
    }
    *used = next;
    Ok(())
}

fn total_text_bytes(texts: &[String]) -> Option<usize> {
    texts
        .iter()
        .try_fold(0usize, |total, text| total.checked_add(text.len()))
}

fn total_embedding_input_bytes(texts: &[String], prefix: &str) -> Option<usize> {
    total_text_bytes(texts)?.checked_add(prefix.len().checked_mul(texts.len())?)
}

fn validate_embedding_budget(texts: &[String], prefix: &str) -> Result<(), String> {
    let max_prefix = indexd_cfg().service.max_prefix_bytes;
    if prefix.len() > max_prefix {
        return Err(format!(
            "prefix is {} bytes (max {max_prefix})",
            prefix.len()
        ));
    }
    let max = indexd_cfg().service.max_request_text_bytes;
    let bytes = total_embedding_input_bytes(texts, prefix)
        .ok_or_else(|| "embedding input byte count overflow".to_string())?;
    if bytes > max {
        Err(format!(
            "texts plus repeated prefixes total {bytes} bytes exceeds max {max}"
        ))
    } else {
        Ok(())
    }
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
    embed_circuit: String,
    embed_cache_entries: usize,
    embed_cache_hits: u64,
    embed_cache_misses: u64,
    queue_depth: usize,
    queued_bytes: u64,
    inference_in_flight: usize,
    timed_out_still_running: usize,
    model_generation: u64,
    rss_before_load_bytes: u64,
    rss_after_load_bytes: u64,
    rss_after_unload_bytes: u64,
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
    #[serde(skip_serializing_if = "Option::is_none")]
    code: Option<String>,
}

// --- Embedding model ---

struct EmbedModel {
    model: NomicBertModel,
    tokenizer: Tokenizer,
    device: Device,
    /// Rotary-position ceiling (`config.n_positions`). The candle nomic-bert
    /// rope tables are precomputed for exactly this many positions; feeding a
    /// longer token sequence makes `candle_nn::rotary_emb::rope` bail with
    /// "inconsistent last dim size in rope". `embed()` truncates to this as a
    /// hard backstop so an oversized input can never crash an embed call.
    max_seq_len: usize,
    /// Maximum `batch * padded_sequence_length` passed to one forward.
    max_forward_tokens: usize,
}

impl EmbedModel {
    fn load(
        dtype: DType,
        model_id: &str,
        configured_max_seq_len: usize,
        configured_forward_tokens: usize,
    ) -> Result<Self> {
        let device = Device::Cpu;

        // Cache-first reload: probe the local HF cache before any network
        // check. `Api::new()` issues an HF network probe per file even when the
        // model is already cached ("downloading model files" log), and now that
        // idle=300 unloads the model, every reload would pay that latency.
        // `HF_HUB_OFFLINE` is not honoured by hf-hub 0.4.3, so we probe the
        // cache directly instead. `Cache::default()` resolves the SAME path
        // `Api::new()` uses internally (it calls `Cache::default()`) —
        // `dirs::home_dir()/.cache/huggingface` — so this points at the cache
        // the daemon already populated under its HOME-redirected $HOME. Only
        // hit the network when a file is missing.
        let repo_spec = Repo::new(model_id.to_string(), RepoType::Model);
        let cache = Cache::default().repo(repo_spec.clone());
        let cfg = cache.get("config.json");
        let tok = cache.get("tokenizer.json");
        let wts = cache.get("model.safetensors");
        let (config_path, tokenizer_path, weights_path) = match (cfg, tok, wts) {
            (Some(c), Some(t), Some(w)) => {
                info!("loading model from local HF cache for {model_id}");
                (c, t, w)
            }
            _ => {
                info!("model cache incomplete; fetching from {model_id}");
                let api = Api::new()?;
                let repo = api.repo(repo_spec);
                (
                    repo.get("config.json").context("downloading config.json")?,
                    repo.get("tokenizer.json")
                        .context("downloading tokenizer.json")?,
                    repo.get("model.safetensors")
                        .context("downloading model.safetensors")?,
                )
            }
        };

        info!("loading model with {dtype:?} precision...");
        let config: nomic_bert::Config = serde_json::from_str(
            &std::fs::read_to_string(&config_path).context("reading config.json")?,
        )?;
        let mut tokenizer =
            Tokenizer::from_file(&tokenizer_path).map_err(|e| anyhow::anyhow!("{e}"))?;
        let max_seq_len = effective_sequence_limit(
            configured_max_seq_len,
            configured_forward_tokens,
            config.n_positions,
        )?;
        if max_seq_len != configured_max_seq_len {
            warn!(
                configured = configured_max_seq_len,
                effective = max_seq_len,
                n_positions = config.n_positions,
                max_forward_tokens = configured_forward_tokens,
                "lowered embedding sequence length to model/operator ceilings"
            );
        }
        tokenizer
            .with_truncation(Some(tokenizers::TruncationParams {
                max_length: max_seq_len,
                ..Default::default()
            }))
            .map_err(|e| anyhow::anyhow!("configuring tokenizer truncation: {e}"))?;
        let max_forward_tokens = configured_forward_tokens;

        // F16 service loads always mmap an already-converted artefact. The
        // one-time conversion is completed and atomically renamed before the
        // model builder starts, so normal cold loads never retain the 522 MiB
        // F32 mapping alongside the resident F16 model.
        let runtime_weights = if dtype == DType::F16 {
            match ensure_f16_weights(&weights_path) {
                Ok(path) => path,
                Err(e) => {
                    warn!(
                        source = %weights_path.display(),
                        "f16 artefact unavailable ({e:#}); falling back to mmap F32 weights with runtime F16 casts"
                    );
                    weights_path
                }
            }
        } else {
            weights_path
        };
        let vb =
            unsafe { VarBuilder::from_mmaped_safetensors(&[runtime_weights], dtype, &device)? };
        let model = NomicBertModel::load(vb, &config)?;

        info!("model loaded successfully");
        Ok(Self {
            model,
            tokenizer,
            device,
            max_seq_len,
            max_forward_tokens,
        })
    }

    fn embed(&self, texts: &[String], prefix: &str) -> Result<Vec<Vec<f32>>> {
        if texts.is_empty() {
            return Ok(vec![]);
        }

        let prefixed: Vec<String> = texts.iter().map(|t| format!("{prefix}{t}")).collect();

        let mut tokens = self
            .tokenizer
            .encode_batch(
                prefixed.iter().map(|s| s.as_str()).collect::<Vec<_>>(),
                true,
            )
            .map_err(|e| anyhow::anyhow!("tokenization: {e}"))?;

        // Tokenizer truncation is configuration, not a memory-safety boundary.
        // Retain a release-mode backstop so a tokenizer/version regression can
        // never feed more positions than the model's rope tables hold.
        for encoding in &mut tokens {
            if encoding.get_ids().len() > self.max_seq_len {
                encoding.truncate(self.max_seq_len, 0, tokenizers::TruncationDirection::Right);
            }
        }
        let lengths: Vec<usize> = tokens.iter().map(|t| t.get_ids().len()).collect();
        let batches = plan_micro_batches(&lengths, self.max_forward_tokens);
        let mut results: Vec<Option<Vec<f32>>> = vec![None; tokens.len()];

        for batch in batches {
            let max_len = batch
                .iter()
                .map(|&i| tokens[i].get_ids().len())
                .max()
                .unwrap_or(0);
            let batch_size = batch.len();
            let mut all_ids = Vec::with_capacity(batch_size * max_len);
            let mut all_mask = Vec::with_capacity(batch_size * max_len);
            let mut all_type_ids = Vec::with_capacity(batch_size * max_len);

            for &original_idx in &batch {
                let encoding = &tokens[original_idx];
                let pad_len = max_len - encoding.get_ids().len();
                all_ids.extend_from_slice(encoding.get_ids());
                all_ids.extend(std::iter::repeat_n(0u32, pad_len));
                all_mask.extend_from_slice(encoding.get_attention_mask());
                all_mask.extend(std::iter::repeat_n(0u32, pad_len));
                all_type_ids.extend_from_slice(encoding.get_type_ids());
                all_type_ids.extend(std::iter::repeat_n(0u32, pad_len));
            }

            let input_ids = Tensor::from_vec(all_ids, (batch_size, max_len), &self.device)?;
            let attention_mask = Tensor::from_vec(all_mask, (batch_size, max_len), &self.device)?;
            let token_type_ids =
                Tensor::from_vec(all_type_ids, (batch_size, max_len), &self.device)?;
            let hidden =
                self.model
                    .forward(&input_ids, Some(&token_type_ids), Some(&attention_mask))?;
            let hidden = hidden.to_dtype(DType::F32)?;
            let pooled = nomic_bert::mean_pooling(&hidden, &attention_mask)?;
            let normalized = nomic_bert::l2_normalize(&pooled)?;
            for (batch_idx, &original_idx) in batch.iter().enumerate() {
                results[original_idx] = Some(normalized.get(batch_idx)?.to_vec1::<f32>()?);
            }
        }
        results
            .into_iter()
            .map(|value| value.ok_or_else(|| anyhow::anyhow!("missing micro-batch output")))
            .collect()
    }
}

fn effective_sequence_limit(
    configured: usize,
    max_forward_tokens: usize,
    n_positions: usize,
) -> Result<usize> {
    const MIN_SEQUENCE_TOKENS: usize = 16;
    const TRAINED_MAX_SEQUENCE_TOKENS: usize = 2048;
    if n_positions < MIN_SEQUENCE_TOKENS {
        anyhow::bail!(
            "model declares n_positions={n_positions}, below the minimum supported {MIN_SEQUENCE_TOKENS}"
        );
    }
    let configured = configured.clamp(MIN_SEQUENCE_TOKENS, TRAINED_MAX_SEQUENCE_TOKENS);
    let effective = configured
        .min(n_positions.min(TRAINED_MAX_SEQUENCE_TOKENS))
        .min(max_forward_tokens);
    if effective == 0 {
        anyhow::bail!("max_forward_tokens must be at least 1");
    }
    Ok(effective)
}

/// Length-aware ascending grouping minimises padding while keeping every
/// forward at or below `budget` padded tokens. Indices let `embed()` restore
/// caller order after the sorted execution plan.
fn plan_micro_batches(lengths: &[usize], budget: usize) -> Vec<Vec<usize>> {
    let mut ordered: Vec<usize> = (0..lengths.len()).collect();
    ordered.sort_by_key(|&i| lengths[i]);
    let budget = budget.max(1);
    let mut batches = Vec::new();
    let mut current = Vec::new();
    let mut current_max = 0usize;
    for idx in ordered {
        let candidate_max = current_max.max(lengths[idx].max(1));
        let candidate_cost = candidate_max.saturating_mul(current.len() + 1);
        if !current.is_empty() && candidate_cost > budget {
            batches.push(std::mem::take(&mut current));
            current_max = 0;
        }
        current_max = current_max.max(lengths[idx].max(1));
        current.push(idx);
    }
    if !current.is_empty() {
        batches.push(current);
    }
    batches
}

fn ensure_f16_weights(weights_path: &Path) -> Result<PathBuf> {
    use candle_core::safetensors::Load;

    let target = weights_path.with_file_name("model.f16.safetensors");
    cleanup_f16_tmp_files(weights_path)?;
    if target.is_file() {
        match validate_f16_weights(weights_path, &target) {
            Ok(()) => return Ok(target),
            Err(e) => {
                warn!(target = %target.display(), "discarding invalid f16 artefact: {e:#}");
                std::fs::remove_file(&target)
                    .with_context(|| format!("removing invalid {}", target.display()))?;
            }
        }
    }
    let tmp =
        weights_path.with_file_name(format!(".model.f16.safetensors.tmp-{}", std::process::id()));
    info!(
        source = %weights_path.display(),
        target = %target.display(),
        "converting cached model weights to f16 (one time)"
    );
    let mapped = unsafe { candle_core::safetensors::MmapedSafetensors::new(weights_path)? };
    let mut converted = HashMap::new();
    for (name, view) in mapped.tensors() {
        if DType::try_from(view.dtype())? != DType::F32 {
            anyhow::bail!(
                "weight {name} is {:?}, expected all-F32 source",
                view.dtype()
            );
        }
        converted.insert(name, view.load(&Device::Cpu)?.to_dtype(DType::F16)?);
    }
    let install = (|| -> Result<PathBuf> {
        candle_core::safetensors::save(&converted, &tmp)
            .with_context(|| format!("writing {}", tmp.display()))?;
        validate_f16_weights(weights_path, &tmp)?;
        std::fs::File::open(&tmp)?.sync_all()?;
        std::fs::rename(&tmp, &target)
            .with_context(|| format!("installing {}", target.display()))?;
        if let Some(parent) = target.parent() {
            std::fs::File::open(parent)?.sync_all()?;
        }
        validate_f16_weights(weights_path, &target)?;
        Ok(target)
    })();
    if install.is_err() {
        let _ = std::fs::remove_file(&tmp);
    }
    install
}

fn cleanup_f16_tmp_files(weights_path: &Path) -> Result<()> {
    const STALE_AFTER: Duration = Duration::from_secs(60 * 60);
    let Some(parent) = weights_path.parent() else {
        return Ok(());
    };
    for entry in std::fs::read_dir(parent)? {
        let entry = entry?;
        let file_name = entry.file_name();
        if let Some(name) = file_name
            .to_str()
            .filter(|name| name.starts_with(".model.f16.safetensors.tmp-"))
        {
            let stale = entry
                .metadata()
                .and_then(|metadata| metadata.modified())
                .ok()
                .and_then(|modified| modified.elapsed().ok())
                .is_some_and(|age| age > STALE_AFTER);
            let owner_dead = name
                .strip_prefix(".model.f16.safetensors.tmp-")
                .and_then(|pid| pid.parse::<u32>().ok())
                .is_some_and(|pid| !Path::new(&format!("/proc/{pid}")).exists());
            if !stale && !owner_dead {
                continue;
            }
            match std::fs::remove_file(entry.path()) {
                Ok(()) => {
                    tracing::debug!(path = %entry.path().display(), "removed stale f16 conversion temporary")
                }
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
                Err(e) => {
                    return Err(e).with_context(|| format!("removing {}", entry.path().display()));
                }
            }
        }
    }
    Ok(())
}

fn validate_f16_weights(source: &Path, target: &Path) -> Result<()> {
    let source = unsafe { candle_core::safetensors::MmapedSafetensors::new(source)? };
    let target = unsafe { candle_core::safetensors::MmapedSafetensors::new(target)? };
    let source_tensors = source.tensors();
    let target_tensors = target.tensors();
    if source_tensors.len() != target_tensors.len() {
        anyhow::bail!(
            "f16 tensor count {} != source count {}",
            target_tensors.len(),
            source_tensors.len()
        );
    }
    let target_by_name: HashMap<_, _> = target_tensors.into_iter().collect();
    for (name, source_view) in source_tensors {
        let target_view = target_by_name
            .get(&name)
            .ok_or_else(|| anyhow::anyhow!("f16 artefact missing tensor {name}"))?;
        if DType::try_from(target_view.dtype())? != DType::F16 {
            anyhow::bail!("f16 tensor {name} has dtype {:?}", target_view.dtype());
        }
        if target_view.shape() != source_view.shape() {
            anyhow::bail!(
                "f16 tensor {name} shape {:?} != source {:?}",
                target_view.shape(),
                source_view.shape()
            );
        }
    }
    Ok(())
}

// --- Vector database ---

struct VectorDb {
    conn: Connection,
    /// In-memory exact search mirror of `vec_chunks` — see `vindex.rs`
    /// for the layout and the writer/reader lock contract. Shared with
    /// the off-mutex search path via `Arc`.
    search_index: Arc<std::sync::RwLock<vindex::VectorIndex>>,
    checkpoint_owed: CheckpointOwed,
}

enum StoreAttempt {
    Committed { ids: Vec<i64>, duplicates: usize },
    NeedsEmbeddings(Vec<usize>),
}

#[derive(Default)]
struct CheckpointOwed(AtomicBool);

static CHECKPOINT_RETRY_NOTIFY: tokio::sync::Notify = tokio::sync::Notify::const_new();

impl CheckpointOwed {
    fn mark(&self) {
        self.0.store(true, Ordering::Release);
        CHECKPOINT_RETRY_NOTIFY.notify_one();
    }

    fn is_owed(&self) -> bool {
        self.0.load(Ordering::Acquire)
    }

    fn record_result(&self, busy: bool) {
        self.0.store(busy, Ordering::Release);
    }
}

impl VectorDb {
    fn open(path: &str, vindex_dtype: vindex::VectorDtype) -> Result<Self> {
        unsafe {
            rusqlite::ffi::sqlite3_auto_extension(Some(std::mem::transmute::<
                *const (),
                unsafe extern "C" fn(
                    *mut rusqlite::ffi::sqlite3,
                    *mut *mut i8,
                    *const rusqlite::ffi::sqlite3_api_routines,
                ) -> i32,
            >(
                sqlite_vec::sqlite3_vec_init as *const (),
            )));
        }

        if let Some(parent) = std::path::Path::new(path).parent() {
            std::fs::create_dir_all(parent)?;
        }

        let mut conn = Connection::open(path)?;
        conn.execute_batch(
            "PRAGMA journal_mode=WAL; \
             PRAGMA synchronous=NORMAL; \
             PRAGMA busy_timeout=5000; \
             PRAGMA cache_size=-2000;",
        )?;
        let journal_limit = i64::try_from(indexd_cfg().service.journal_size_limit_bytes)
            .context("journal_size_limit_bytes exceeds SQLite i64 range")?;
        conn.pragma_update(None, "journal_size_limit", journal_limit)?;
        // Default new transactions to BEGIN IMMEDIATE so writers grab the write lock up
        // front (no deferred→write upgrade that can deadlock two writers under WAL).
        conn.set_transaction_behavior(rusqlite::TransactionBehavior::Immediate);

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

        // Migration: add superseded_by column for explicit chunk-supersede relationships
        // (e.g. a later journal entry that invalidates an earlier one). Chunks with
        // superseded_by IS NOT NULL are filtered out of context_search results by default
        // but remain in the database for audit/rollback.
        let has_superseded_col: bool = conn
            .prepare("SELECT superseded_by FROM chunks LIMIT 0")
            .is_ok();
        if !has_superseded_col {
            info!("migrating: adding superseded_by column");
            conn.execute_batch(
                "ALTER TABLE chunks ADD COLUMN superseded_by INTEGER DEFAULT NULL;",
            )?;
        }

        // Ensure content_hash index exists (must come AFTER content_hash migration above).
        conn.execute_batch(
            "CREATE UNIQUE INDEX IF NOT EXISTS idx_chunks_content_hash
                 ON chunks(content_hash) WHERE content_hash IS NOT NULL;",
        )?;

        // Candidate preselection for the in-memory search path: active
        // (non-superseded) ids by source, index-only.
        conn.execute_batch(
            "CREATE INDEX IF NOT EXISTS idx_chunks_active_source
                 ON chunks(source, id) WHERE superseded_by IS NULL;",
        )?;

        // Build the in-memory search index from vec_chunks. FAIL LOUDLY
        // on any inconsistency (bad blob width, duplicate rowid,
        // chunks↔vectors id-set drift) — silently serving a partial
        // index would return wrong search results while looking healthy.
        //
        // The rollback switch must work even when the LOADER is what's
        // broken: under COSMIX_INDEXD_SEARCH=sqlite the mirror is
        // neither built nor validated, so a bad corpus can't stop the
        // daemon from starting in fallback mode.
        if sqlite_search_fallback() {
            let version: String = conn.query_row("SELECT vec_version()", [], |r| r.get(0))?;
            info!(
                "vector db opened at {path} (sqlite-vec {version}; in-memory search DISABLED via COSMIX_INDEXD_SEARCH=sqlite)"
            );
            return Ok(Self {
                conn,
                search_index: Arc::new(std::sync::RwLock::new(vindex::VectorIndex::new(
                    EMBEDDING_DIM,
                    vindex_dtype,
                ))),
                checkpoint_owed: CheckpointOwed::default(),
            });
        }
        let load_t0 = Instant::now();
        let row_count: usize = conn
            .query_row("SELECT COUNT(*) FROM vec_chunks", [], |row| {
                row.get::<_, i64>(0)
            })?
            .try_into()
            .context("negative or oversized vec_chunks row count")?;
        let mut index = vindex::VectorIndex::with_capacity(EMBEDDING_DIM, row_count, vindex_dtype);
        {
            let mut stmt = conn.prepare("SELECT rowid, embedding FROM vec_chunks")?;
            let rows = stmt.query_map([], |row| {
                Ok((row.get::<_, i64>(0)?, row.get::<_, Vec<u8>>(1)?))
            })?;
            for row in rows {
                let (id, blob) = row?;
                if blob.len() != EMBEDDING_DIM * 4 {
                    anyhow::bail!(
                        "vec_chunks rowid {id}: blob {} bytes, expected {}",
                        blob.len(),
                        EMBEDDING_DIM * 4
                    );
                }
                let emb: Vec<f32> = blob
                    .chunks_exact(4)
                    .map(|b| f32::from_le_bytes([b[0], b[1], b[2], b[3]]))
                    .collect();
                let before = index.len();
                index.upsert(id, &emb).map_err(|e| anyhow::anyhow!(e))?;
                if index.len() == before {
                    anyhow::bail!("vec_chunks duplicate rowid {id}");
                }
            }
        }
        // Identity validation, both directions — equal COUNTs can hide
        // a missing vector plus an orphan cancelling out. Surface the
        // actual offending ids so repair is actionable.
        let missing_vecs: Vec<i64> = conn
            .prepare("SELECT id FROM chunks EXCEPT SELECT rowid FROM vec_chunks LIMIT 10")?
            .query_map([], |r| r.get(0))?
            .collect::<rusqlite::Result<_>>()?;
        if !missing_vecs.is_empty() {
            anyhow::bail!(
                "chunks without vectors (first 10): {missing_vecs:?} — repair before serving \
                 (or COSMIX_INDEXD_SEARCH=sqlite to boot in fallback mode)"
            );
        }
        let orphan_vecs: Vec<i64> = conn
            .prepare("SELECT rowid FROM vec_chunks EXCEPT SELECT id FROM chunks LIMIT 10")?
            .query_map([], |r| r.get(0))?
            .collect::<rusqlite::Result<_>>()?;
        if !orphan_vecs.is_empty() {
            anyhow::bail!(
                "orphan vectors without chunks (first 10): {orphan_vecs:?} — repair before serving \
                 (or COSMIX_INDEXD_SEARCH=sqlite to boot in fallback mode)"
            );
        }

        let version: String = conn.query_row("SELECT vec_version()", [], |r| r.get(0))?;
        info!(
            vectors = index.len(),
            load_ms = load_t0.elapsed().as_millis() as u64,
            "vector db opened at {path} (sqlite-vec {version}; in-memory search index loaded)"
        );
        Ok(Self {
            conn,
            search_index: Arc::new(std::sync::RwLock::new(index)),
            checkpoint_owed: CheckpointOwed::default(),
        })
    }

    /// Read-only optimistic duplicate probe. This deliberately does not update
    /// metadata: no durable side effect may precede successful inference and
    /// the final IMMEDIATE transaction.
    fn preflight_duplicate_ids(&self, texts: &[String], source: &str) -> Result<Vec<Option<i64>>> {
        let mut stmt = self
            .conn
            .prepare("SELECT id FROM chunks WHERE content_hash = ?1")?;
        let ids: rusqlite::Result<Vec<_>> = texts
            .iter()
            .map(|text| {
                let hash = content_hash(text, source);
                stmt.query_row([hash], |row| row.get(0)).optional()
            })
            .collect();
        Ok(ids?)
    }

    /// Final all-or-nothing store. Every item skipped by the optimistic
    /// preflight is re-resolved after BEGIN IMMEDIATE and before the first
    /// write. If any disappeared, the transaction rolls back cleanly and the
    /// caller computes just those missing embeddings off-lock before retrying.
    fn store_revalidated(
        &self,
        embeddings: &[Option<Vec<f32>>],
        preflight_ids: &[Option<i64>],
        texts: &[String],
        source: &str,
        metadata: &[String],
    ) -> Result<StoreAttempt> {
        if embeddings.len() != texts.len() || preflight_ids.len() != texts.len() {
            anyhow::bail!("store input lengths do not match");
        }
        let mut ids = Vec::with_capacity(embeddings.len());
        let mut duplicates = 0usize;

        // Wrap the whole multi-chunk store in a single transaction so it's one commit
        // (and one write-lock acquisition) instead of one tx per row. The connection's
        // default transaction behavior is set to IMMEDIATE in `open()`, so this
        // `unchecked_transaction` begins with BEGIN IMMEDIATE (grabs the write lock up
        // front, avoiding a deferred→write upgrade that can deadlock under concurrency).
        // `unchecked_transaction` works on `&self`; access to this connection is
        // serialized by the AppState tokio Mutex (every caller holds the lock guard),
        // so no two transactions overlap on one connection. All embedding/CPU work is
        // already done before this call — nothing CPU-bound runs inside the tx.
        //
        // Search-index write gate: hold the write lock across commit +
        // memory patch so a concurrent search (read lock + its own read
        // transaction) sees before-state or after-state, never a mix.
        let mut vindex = self
            .search_index
            .write()
            .expect("search index RwLock poisoned");
        let mut inserted_vecs: Vec<(i64, usize)> = Vec::new();
        let tx = self.conn.unchecked_transaction()?;

        let mut vanished = Vec::new();
        {
            let mut stmt = tx.prepare("SELECT id FROM chunks WHERE content_hash = ?1")?;
            for (i, (text, preflight_id)) in texts.iter().zip(preflight_ids).enumerate() {
                if preflight_id.is_some() && embeddings[i].is_none() {
                    let hash = content_hash(text, source);
                    if stmt
                        .query_row([hash], |row| row.get::<_, i64>(0))
                        .optional()?
                        .is_none()
                    {
                        vanished.push(i);
                    }
                }
            }
        }
        if !vanished.is_empty() {
            tx.rollback()?;
            return Ok(StoreAttempt::NeedsEmbeddings(vanished));
        }

        for (i, (embedding, text)) in embeddings.iter().zip(texts.iter()).enumerate() {
            let meta = metadata.get(i).map(|s| s.as_str()).unwrap_or("");
            let hash = content_hash(text, source);

            if embedding.is_none() {
                let existing_id: i64 = tx.query_row(
                    "SELECT id FROM chunks WHERE content_hash = ?1",
                    [&hash],
                    |r| r.get(0),
                )?;
                if !meta.is_empty() {
                    tx.execute(
                        "UPDATE chunks SET metadata = ?1 WHERE id = ?2",
                        rusqlite::params![meta, existing_id],
                    )?;
                }
                ids.push(existing_id);
                duplicates += 1;
                continue;
            }

            // INSERT OR IGNORE — unique index on content_hash rejects exact duplicates
            let inserted = tx.execute(
                "INSERT OR IGNORE INTO chunks (content, source, metadata, content_hash) VALUES (?1, ?2, ?3, ?4)",
                rusqlite::params![text, source, meta, hash],
            )?;

            if inserted == 0 {
                // Duplicate — return existing ID, optionally update metadata
                let existing_id: i64 = tx.query_row(
                    "SELECT id FROM chunks WHERE content_hash = ?1",
                    [&hash],
                    |r| r.get(0),
                )?;
                if !meta.is_empty() {
                    tx.execute(
                        "UPDATE chunks SET metadata = ?1 WHERE id = ?2",
                        rusqlite::params![meta, existing_id],
                    )?;
                }
                ids.push(existing_id);
                duplicates += 1;
                continue;
            }

            let rowid = tx.last_insert_rowid();

            let blob = vec_to_blob(embedding.as_deref().expect("embedding checked above"));
            tx.execute(
                "INSERT INTO vec_chunks (rowid, embedding) VALUES (?1, ?2)",
                rusqlite::params![rowid, blob],
            )?;

            inserted_vecs.push((rowid, i));
            ids.push(rowid);
        }

        tx.commit()?;
        // Patch AFTER the commit succeeded; a tx error above unwinds
        // without touching the in-memory index.
        for (rowid, i) in inserted_vecs {
            if let Err(e) = vindex.upsert(
                rowid,
                embeddings[i]
                    .as_deref()
                    .expect("inserted rows always have embeddings"),
            ) {
                warn!("search index patch failed for chunk {rowid}: {e}");
            }
        }

        Ok(StoreAttempt::Committed { ids, duplicates })
    }

    fn search(
        &self,
        query_embedding: &[f32],
        limit: usize,
        source_filter: &str,
        metadata_filters: &[MetadataFilter],
        max_response_bytes: usize,
    ) -> Result<Vec<SearchResult>> {
        let blob = vec_to_blob(query_embedding);

        // Build the base query — sqlite-vec requires MATCH + k in the WHERE clause.
        // Superseded chunks are excluded here so they cannot surface through normal
        // retrieval; they remain queryable by explicit id for audit/rollback.
        let mut sql = String::from(
            "SELECT v.rowid, v.distance, c.content, c.source, c.metadata, c.feedback_score,
                    c.retrieval_count, c.last_retrieved, c.created
             FROM vec_chunks v
             JOIN chunks c ON c.id = v.rowid
             WHERE v.embedding MATCH ?1
             AND k = ?2
             AND c.superseded_by IS NULL",
        );

        let limit = sqlite_count(limit.min(MAX_SEARCH_LIMIT), "search.limit")
            .map_err(anyhow::Error::msg)?;
        let mut params: Vec<Box<dyn rusqlite::types::ToSql>> =
            vec![Box::new(blob), Box::new(limit)];
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

        let param_refs: Vec<&dyn rusqlite::types::ToSql> =
            params.iter().map(|p| p.as_ref()).collect();
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
        let mut response_bytes = 0usize;
        for row in rows {
            let result = row?;
            charge_response_item(
                &mut response_bytes,
                &[
                    &result.content,
                    &result.source,
                    &result.metadata,
                    &result.created,
                ],
                max_response_bytes,
            )?;
            results.push(result);
        }
        // Re-sort by adjusted distance: feedback boost - implicit negative - staleness penalty.
        // Lower adjusted distance = better rank.
        let now_days = days_since_epoch_utc();
        results.sort_by(|a, b| {
            let adj_a = adjusted_distance(a, now_days);
            let adj_b = adjusted_distance(b, now_days);
            adj_a
                .partial_cmp(&adj_b)
                .unwrap_or(std::cmp::Ordering::Equal)
        });
        Ok(results)
    }

    /// Fire-and-forget: mark these chunk IDs as retrieved (increment count, update timestamp).
    /// Silently ignores errors — retrieval tracking is best-effort.
    fn mark_retrieved(&self, ids: &[i64]) {
        if ids.is_empty() {
            return;
        }
        let placeholders = std::iter::repeat_n("?", ids.len())
            .collect::<Vec<_>>()
            .join(",");
        let sql = format!(
            "UPDATE chunks SET retrieval_count = retrieval_count + 1,
                               last_retrieved = datetime('now')
             WHERE id IN ({placeholders})"
        );
        let params: Vec<Box<dyn rusqlite::types::ToSql>> = ids
            .iter()
            .map(|id| Box::new(*id) as Box<dyn rusqlite::types::ToSql>)
            .collect();
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

        // One transaction for the row update + vector swap (previously
        // three autocommits — a crash between them could strand a chunk
        // without its vector), gated on the search-index write lock.
        let mut vindex = self
            .search_index
            .write()
            .expect("search index RwLock poisoned");
        let tx = self.conn.unchecked_transaction()?;

        // Reject a nonexistent id BEFORE touching vec_chunks: the old
        // code happily inserted a vector row for an id with no chunk (a
        // caller typo minted an orphan), which the startup identity
        // check would now turn into a boot failure.
        let exists = match tx.query_row("SELECT 1 FROM chunks WHERE id = ?1", [id], |_| Ok(())) {
            Ok(()) => true,
            Err(rusqlite::Error::QueryReturnedNoRows) => false,
            Err(e) => return Err(e.into()),
        };
        if !exists {
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
            tx.execute(&sql, &*param_refs)?;
        }

        if let Some(emb) = new_embedding {
            let blob = vec_to_blob(emb);
            // sqlite-vec: delete old + insert new for the same rowid
            tx.execute("DELETE FROM vec_chunks WHERE rowid = ?1", [id])?;
            tx.execute(
                "INSERT INTO vec_chunks (rowid, embedding) VALUES (?1, ?2)",
                rusqlite::params![id, blob],
            )?;
        }

        tx.commit()?;
        if let Some(emb) = new_embedding
            && let Err(e) = vindex.upsert(id, emb)
        {
            warn!("search index patch failed for chunk {id}: {e}");
        }

        Ok(true)
    }

    fn list(
        &self,
        source_filter: &str,
        limit: i64,
        offset: i64,
        max_response_bytes: usize,
    ) -> Result<(Vec<ListItem>, usize)> {
        let has_filter = !source_filter.is_empty();

        let total: usize = if has_filter {
            self.conn.query_row(
                "SELECT COUNT(*) FROM chunks WHERE source = ?1",
                [source_filter],
                |r| r.get::<_, i64>(0).map(|v| v as usize),
            )?
        } else {
            self.conn
                .query_row("SELECT COUNT(*) FROM chunks", [], |r| {
                    r.get::<_, i64>(0).map(|v| v as usize)
                })?
        };

        let sql = if has_filter {
            "SELECT id, content, source, metadata, created FROM chunks WHERE source = ?1 ORDER BY created DESC LIMIT ?2 OFFSET ?3"
        } else {
            "SELECT id, content, source, metadata, created FROM chunks WHERE 1=1 ORDER BY created DESC LIMIT ?1 OFFSET ?2"
        };

        let mut stmt = self.conn.prepare(sql)?;
        let mut items = Vec::new();
        let mut response_bytes = 0usize;

        if has_filter {
            let rows = stmt.query_map(rusqlite::params![source_filter, limit, offset], |row| {
                Ok(ListItem {
                    id: row.get(0)?,
                    content: row.get(1)?,
                    source: row.get(2)?,
                    metadata: row.get(3)?,
                    created: row.get(4)?,
                })
            })?;
            for row in rows {
                let item = row?;
                charge_response_item(
                    &mut response_bytes,
                    &[&item.content, &item.source, &item.metadata, &item.created],
                    max_response_bytes,
                )?;
                items.push(item);
            }
        } else {
            let rows = stmt.query_map(rusqlite::params![limit, offset], |row| {
                Ok(ListItem {
                    id: row.get(0)?,
                    content: row.get(1)?,
                    source: row.get(2)?,
                    metadata: row.get(3)?,
                    created: row.get(4)?,
                })
            })?;
            for row in rows {
                let item = row?;
                charge_response_item(
                    &mut response_bytes,
                    &[&item.content, &item.source, &item.metadata, &item.created],
                    max_response_bytes,
                )?;
                items.push(item);
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

    /// Mark `old_id` as superseded by `new_id`. Supersede-don't-delete semantics:
    /// the old chunk stays in the database for audit/rollback but is excluded from
    /// context_search results via the `WHERE c.superseded_by IS NULL` clause.
    /// Returns the number of rows updated (0 if old_id doesn't exist, 1 on success).
    /// Guards against self-supersede and verifies new_id exists before updating.
    fn supersede(&self, old_id: i64, new_id: i64) -> Result<usize> {
        if old_id == new_id {
            anyhow::bail!("cannot supersede a chunk by itself (id={old_id})");
        }
        let new_exists: bool = self
            .conn
            .query_row("SELECT 1 FROM chunks WHERE id = ?1", [new_id], |_| Ok(true))
            .unwrap_or(false);
        if !new_exists {
            anyhow::bail!("new_id={new_id} does not exist in chunks table");
        }
        let updated = self.conn.execute(
            "UPDATE chunks SET superseded_by = ?1 WHERE id = ?2",
            rusqlite::params![new_id, old_id],
        )?;
        Ok(updated)
    }

    fn delete(&self, ids: &[i64]) -> Result<usize> {
        let mut deleted = 0usize;
        // Single transaction so a multi-id delete is one commit (IMMEDIATE via the
        // connection's default behavior set in `open()`), gated on the
        // search-index write lock (commit first, then memory patch).
        let mut vindex = self
            .search_index
            .write()
            .expect("search index RwLock poisoned");
        let tx = self.conn.unchecked_transaction()?;
        for id in ids {
            deleted += tx.execute("DELETE FROM chunks WHERE id = ?1", [id])?;
            tx.execute("DELETE FROM vec_chunks WHERE rowid = ?1", [id])?;
        }
        tx.commit()?;
        for id in ids {
            vindex.remove(*id);
        }
        Ok(deleted)
    }

    /// Return staleness candidates split into 3 buckets:
    /// 1. Never retrieved AND older than `never_retrieved_age_days` days
    /// 2. Retrieved > `low_value_min_retrievals` times with feedback_score <= 0
    /// 3. Last retrieved older than `long_dormant_days` days ago
    fn stale_query(&self, req: &StaleRequest) -> Result<StaleResponse> {
        let total: usize = self
            .conn
            .query_row("SELECT COUNT(*) FROM chunks", [], |r| {
                r.get::<_, i64>(0).map(|v| v as usize)
            })?;

        // Build optional source clause — always put source param at position 3 if present.
        let (src_clause, src_param): (&str, Option<&str>) = if req.source.is_empty() {
            ("", None)
        } else {
            (" AND source = ?3", Some(req.source.as_str()))
        };
        let limit = sqlite_count(
            clamp_stale_limit(req.per_bucket_limit),
            "stale.per_bucket_limit",
        )
        .map_err(anyhow::Error::msg)?;

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
            .query_row("SELECT COUNT(*) FROM chunks", [], |r| {
                r.get::<_, i64>(0).map(|v| v as usize)
            })?;
        let db_size = std::fs::metadata(db_path).map(|m| m.len()).unwrap_or(0);

        let mut by_source = Vec::new();
        let mut stmt = self.conn.prepare(
            "SELECT source, COUNT(*) FROM chunks GROUP BY source ORDER BY COUNT(*) DESC",
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
            embed_circuit: String::new(),
            embed_cache_entries: 0,
            embed_cache_hits: 0,
            embed_cache_misses: 0,
            queue_depth: 0,
            queued_bytes: 0,
            inference_in_flight: 0,
            timed_out_still_running: 0,
            model_generation: 0,
            rss_before_load_bytes: 0,
            rss_after_load_bytes: 0,
            rss_after_unload_bytes: 0,
            by_source,
        })
    }

    fn mark_checkpoint_owed(&self) {
        self.checkpoint_owed.mark();
    }

    fn checkpoint_truncate(&self) -> Result<()> {
        let was_owed = self.checkpoint_owed.is_owed();
        let (busy, log, checkpointed): (i64, i64, i64) =
            self.conn
                .query_row("PRAGMA wal_checkpoint(TRUNCATE)", [], |row| {
                    Ok((row.get(0)?, row.get(1)?, row.get(2)?))
                })?;
        let is_busy = busy == 1;
        self.checkpoint_owed.record_result(is_busy);
        if is_busy {
            tracing::debug!(
                was_owed,
                log_frames = log,
                checkpointed_frames = checkpointed,
                "WAL checkpoint busy; retry owed on next background drain"
            );
        }
        Ok(())
    }
}

fn vec_to_blob(v: &[f32]) -> Vec<u8> {
    v.iter().flat_map(|f| f.to_le_bytes()).collect()
}

// --- In-memory exact search (off the global mutex) ---

/// One-release rollback switch: `COSMIX_INDEXD_SEARCH=sqlite` in the
/// unit environment restores the old sqlite-vec KNN path AND bypasses
/// the in-memory index load/validation at startup — the switch must
/// work even when the loader itself is what's broken. Remove after the
/// in-memory path has soaked.
fn sqlite_search_fallback() -> bool {
    static V: OnceLock<bool> = OnceLock::new();
    *V.get_or_init(|| std::env::var("COSMIX_INDEXD_SEARCH").as_deref() == Ok("sqlite"))
}

/// Hard cap on `SearchRequest.limit` — an unbounded limit performs that
/// many point fetches inside the read gate. Generous versus real
/// callers (context_search asks for ≤10).
const MAX_SEARCH_LIMIT: usize = 200;

/// Admission gate for concurrent in-memory scans. The socket is
/// world-writable, and each scan walks the full 164MiB embedding array
/// — unbounded concurrency would trash memory-bandwidth locality and
/// starve the shared blocking pool that embeds also run on. Two
/// permits: one active scan plus one on deck.
static SEARCH_GATE: tokio::sync::Semaphore = tokio::sync::Semaphore::const_new(2);

#[derive(Default)]
struct SearchPhases {
    candidate_db_us: u64,
    vector_us: u64,
    fetch_db_us: u64,
}

/// Exact search on the in-memory index, entirely OFF the global
/// AppState mutex: eligibility filters run first as SQL on a dedicated
/// read-only connection (fixing the vec0 filter-after-KNN under-fill
/// defect — results are now exactly `min(limit, eligible)`), then an
/// exact L2 top-k over the candidate slots, then row materialisation
/// inside the same read transaction. The index read guard is held
/// across the whole transaction so a concurrent writer (write lock →
/// commit → patch) can never present mixed db/cache state. Runs inside
/// `spawn_blocking`; never call on the async runtime directly.
fn search_exact(
    db_path: &str,
    index: &std::sync::RwLock<vindex::VectorIndex>,
    query: &[f32],
    limit: usize,
    source_filter: &str,
    metadata_filters: &[MetadataFilter],
    max_response_bytes: usize,
) -> Result<(Vec<SearchResult>, SearchPhases)> {
    let _reader = LongReaderGuard::new();
    let mut phases = SearchPhases::default();
    let vguard = index.read().expect("search index RwLock poisoned");
    let conn = Connection::open_with_flags(db_path, rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY)?;
    conn.execute_batch("PRAGMA busy_timeout=5000;")?;
    let tx = conn.unchecked_transaction()?;

    // Candidate preselection — the SAME predicate semantics the old
    // path applied post-KNN (json_extract comparisons, LIKE contains),
    // now applied BEFORE distance selection. Sequential `?` binding.
    let t0 = Instant::now();
    let mut sql = String::from("SELECT id FROM chunks WHERE superseded_by IS NULL");
    let mut params: Vec<Box<dyn rusqlite::types::ToSql>> = Vec::new();
    if !source_filter.is_empty() {
        sql.push_str(" AND source = ?");
        params.push(Box::new(source_filter.to_string()));
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
        sql.push_str(&format!(" AND json_extract(metadata, ?) {op_str} ?"));
        params.push(Box::new(json_path));
        if matches!(filter.op, FilterOp::Contains) {
            let pattern = format!("%{}%", filter.value.as_str().unwrap_or(""));
            params.push(Box::new(pattern));
        } else {
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
    }
    let param_refs: Vec<&dyn rusqlite::types::ToSql> = params.iter().map(|p| p.as_ref()).collect();
    let mut stmt = tx.prepare(&sql)?;
    let candidates: Vec<i64> = stmt
        .query_map(&*param_refs, |row| row.get(0))?
        .collect::<rusqlite::Result<_>>()?;
    drop(stmt);
    phases.candidate_db_us = t0.elapsed().as_micros() as u64;

    // Exact distance top-k over the candidates. A missing candidate is
    // mirror/database divergence — hard error, never a silent skip.
    let t0 = Instant::now();
    let top = vguard
        .top_k(query, &candidates, limit)
        .map_err(|e| anyhow::anyhow!(e))?;
    phases.vector_us = t0.elapsed().as_micros() as u64;

    // Materialise the selected rows inside the same transaction.
    let t0 = Instant::now();
    let mut stmt = tx.prepare(
        "SELECT content, source, metadata, feedback_score, retrieval_count,
                last_retrieved, created
         FROM chunks WHERE id = ?1",
    )?;
    let mut results: Vec<SearchResult> = Vec::with_capacity(top.len());
    let mut response_bytes = 0usize;
    for (id, dist) in top {
        let row = stmt.query_row([id], |row| {
            Ok(SearchResult {
                id,
                distance: dist as f64,
                content: row.get(0)?,
                source: row.get(1)?,
                metadata: row.get(2)?,
                feedback_score: row.get::<_, i64>(3).unwrap_or(0),
                retrieval_count: row.get::<_, i64>(4).unwrap_or(0),
                last_retrieved: row.get::<_, Option<String>>(5).ok().flatten(),
                created: row.get::<_, String>(6).unwrap_or_default(),
            })
        });
        match row {
            Ok(r) => {
                charge_response_item(
                    &mut response_bytes,
                    &[&r.content, &r.source, &r.metadata, &r.created],
                    max_response_bytes,
                )?;
                results.push(r);
            }
            // Candidate vanished between selection and fetch is
            // impossible while we hold the index read guard (writers
            // block on the write lock) — treat as a hard error.
            Err(e) => anyhow::bail!("search fetch failed for chunk {id}: {e}"),
        }
    }
    drop(stmt);
    tx.finish()?;
    phases.fetch_db_us = t0.elapsed().as_micros() as u64;
    drop(vguard);

    // Re-sort by adjusted distance — identical semantics to the old
    // path: feedback boost - implicit negative - staleness penalty
    // applied over the top-`limit` set.
    let now_days = days_since_epoch_utc();
    results.sort_by(|a, b| {
        let adj_a = adjusted_distance(a, now_days);
        let adj_b = adjusted_distance(b, now_days);
        adj_a
            .partial_cmp(&adj_b)
            .unwrap_or(std::cmp::Ordering::Equal)
    });
    Ok((results, phases))
}

// --- Shared state ---

/// Cached corpus aggregates serving the 1 Hz world tick and flat props
/// reads. The authoritative values come from the off-mutex reconciler
/// task (a dedicated READ-ONLY sqlite connection, every 30s on
/// spawn_blocking); mutation handlers adjust the counts incrementally
/// between reconciles so world snapshots stay roughly fresh. Drift
/// (e.g. per-source counts after a delete, whose sources we don't
/// re-query) is bounded by the reconcile interval.
///
/// This replaces running `db.stats()` (COUNT + GROUP BY over the whole
/// corpus) UNDER THE GLOBAL MUTEX once per second — measured at ~12%
/// of a core continuously (18,931 calls / 2,240s db time in 5.25h on
/// 2026-07-25) plus lock contention against every request.
#[derive(Default, Clone)]
struct CorpusCache {
    total_vectors: u64,
    db_size_bytes: u64,
    by_source: std::collections::BTreeMap<String, u64>,
}

/// The reconciler's aggregation, on its OWN connection — never called
/// with the AppState mutex held. WAL mode permits this concurrent
/// reader. Plain tables only (no sqlite-vec extension needed).
fn corpus_aggregate(db_path: &str) -> Result<CorpusCache> {
    let _reader = LongReaderGuard::new();
    let conn =
        rusqlite::Connection::open_with_flags(db_path, rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY)?;
    // ONE read transaction for both queries: separate autocommit reads
    // could straddle a mutation and yield total != sum(by_source).
    let tx = conn.unchecked_transaction()?;
    let total: u64 = tx.query_row("SELECT COUNT(*) FROM chunks", [], |r| {
        r.get::<_, i64>(0).map(|v| v as u64)
    })?;
    let mut by_source = std::collections::BTreeMap::new();
    {
        let mut stmt = tx.prepare("SELECT source, COUNT(*) FROM chunks GROUP BY source")?;
        let rows = stmt.query_map([], |row| {
            Ok((row.get::<_, String>(0)?, row.get::<_, i64>(1)? as u64))
        })?;
        for row in rows {
            let (source, count) = row?;
            by_source.insert(source, count);
        }
    }
    tx.finish()?;
    let db_size_bytes = std::fs::metadata(db_path).map(|m| m.len()).unwrap_or(0);
    Ok(CorpusCache {
        total_vectors: total,
        db_size_bytes,
        by_source,
    })
}

struct AppState {
    model: Option<Arc<EmbedModel>>,
    dtype: DType,
    model_id: String,
    db: VectorDb,
    db_path: String,
    socket_path: String,
    idle_timeout_secs: u64,
    started: Instant,
    started_at_iso: String,
    /// Guards model *loading* (download/init) — opens after repeated load
    /// failures so a broken model dir doesn't retry-storm.
    model_breaker: CircuitBreaker,
    /// Guards model *inference* — opens after repeated embed failures or
    /// timeouts so a wedged forward pass doesn't keep blocking requests.
    embed_breaker: CircuitBreaker,
    embed_cache: EmbeddingCache,
    /// Exactly one model forward may own this permit. The owned permit is
    /// moved into the blocking closure, so dropping a timed-out JoinHandle
    /// cannot admit another inference while the OS thread still runs.
    inference_gate: Arc<tokio::sync::Semaphore>,
    /// Sender for the background indexing job queue. A `background=true`
    /// `index_file` request is pushed here and drained by the single worker
    /// task spawned in `main()`; the worker runs jobs serially (embeds are
    /// serial anyway). The channel bounds count; `queue_budget` separately
    /// reserves owned request bytes before enqueue.
    job_tx: tokio::sync::mpsc::Sender<IndexJob>,
    queue_budget: Arc<QueueBudget>,
    /// See [`CorpusCache`] — read by `collect_props`, written by the
    /// reconciler task, nudged by mutation handlers. Held inside the
    /// state mutex, but only for memory ops (never a query).
    corpus_cache: CorpusCache,
    /// Bumped by EVERY corpus-count-affecting mutation nudge. The
    /// reconciler snapshots this before aggregating and discards its
    /// result if the epoch moved — otherwise a store landing between
    /// the aggregate read and the cache swap would be overwritten by
    /// the stale aggregate (world.indexd would publish a false
    /// decrement, then re-correct: a phantom count wobble).
    corpus_epoch: u64,
}

/// Shared request/worker lease for the sole inference permit. The caller keeps
/// one reference until its request-owned model Arc is dropped; the blocking
/// worker keeps another until the forward really exits (including timeout or
/// panic). The permit therefore outlives every model reference from the request.
struct InferenceLease {
    _permit: tokio::sync::OwnedSemaphorePermit,
}

/// Atomic lifecycle for a blocking inference: 0=running, 1=caller timed out,
/// 2=worker finished. This closes the timeout/finish race while keeping the
/// timed-out-still-running gauge exact.
struct InferenceWorkerGuard {
    _lease: Arc<InferenceLease>,
    lifecycle: Arc<AtomicU8>,
}

impl Drop for InferenceWorkerGuard {
    fn drop(&mut self) {
        metrics::INFERENCE_IN_FLIGHT.fetch_sub(1, Ordering::Relaxed);
        if self.lifecycle.swap(2, Ordering::AcqRel) == 1 {
            metrics::TIMED_OUT_IN_FLIGHT.fetch_sub(1, Ordering::Relaxed);
        }
    }
}

fn mark_inference_timed_out(lifecycle: &AtomicU8) {
    if lifecycle
        .compare_exchange(0, 1, Ordering::AcqRel, Ordering::Acquire)
        .is_ok()
    {
        metrics::TIMED_OUT_IN_FLIGHT.fetch_add(1, Ordering::Relaxed);
    }
}

fn rss_bytes() -> u64 {
    let resident_pages = std::fs::read_to_string("/proc/self/statm")
        .ok()
        .and_then(|s| s.split_whitespace().nth(1)?.parse::<u64>().ok())
        .unwrap_or(0);
    let page_size = unsafe { libc::sysconf(libc::_SC_PAGESIZE) };
    if page_size <= 0 {
        0
    } else {
        resident_pages.saturating_mul(page_size as u64)
    }
}

/// Background indexing job envelope. Carries the origin context so the
/// worker's CPU is attributed to the requester that queued it — the
/// foreground `index_file` ack is near-zero cost, the real work lands
/// here later as `index_file_job`.
struct IndexJob {
    req: IndexFileRequest,
    /// `REQUEST_SEQ` of the foreground request that queued this job.
    seq: u64,
    /// "transport:peer" of the original requester.
    peer: String,
    /// Byte size of the original request line.
    bytes: u64,
    enqueued: Instant,
    /// One-based inference/model attempt number for the next worker run.
    attempt: usize,
    reservation: QueueReservation,
}

#[derive(Default)]
struct QueueUsage {
    jobs: usize,
    bytes: usize,
}

struct QueueBudget {
    max_jobs: usize,
    max_bytes: usize,
    usage: std::sync::Mutex<QueueUsage>,
}

impl QueueBudget {
    fn new(max_jobs: usize, max_bytes: usize) -> Arc<Self> {
        Arc::new(Self {
            max_jobs,
            max_bytes,
            usage: std::sync::Mutex::new(QueueUsage::default()),
        })
    }

    fn try_reserve(self: &Arc<Self>, bytes: usize) -> Result<QueueReservation, ()> {
        let mut usage = self.usage.lock().expect("queue budget mutex poisoned");
        let next_bytes = usage.bytes.checked_add(bytes).ok_or(())?;
        if usage.jobs >= self.max_jobs || next_bytes > self.max_bytes {
            return Err(());
        }
        usage.jobs += 1;
        usage.bytes = next_bytes;
        metrics::QUEUE_DEPTH.store(usage.jobs, Ordering::Relaxed);
        metrics::QUEUED_BYTES.store(usage.bytes as u64, Ordering::Relaxed);
        Ok(QueueReservation {
            budget: self.clone(),
            bytes,
            active: true,
        })
    }

    fn is_empty(&self) -> bool {
        self.usage.lock().expect("queue budget mutex poisoned").jobs == 0
    }
}

struct QueueReservation {
    budget: Arc<QueueBudget>,
    bytes: usize,
    active: bool,
}

impl QueueReservation {
    /// Returns true exactly when this release transitions queued jobs to zero.
    fn release(&mut self) -> bool {
        if !self.active {
            return false;
        }
        self.active = false;
        let mut usage = self
            .budget
            .usage
            .lock()
            .expect("queue budget mutex poisoned");
        usage.jobs -= 1;
        usage.bytes = usage.bytes.saturating_sub(self.bytes);
        metrics::QUEUE_DEPTH.store(usage.jobs, Ordering::Relaxed);
        metrics::QUEUED_BYTES.store(usage.bytes as u64, Ordering::Relaxed);
        usage.jobs == 0
    }
}

impl Drop for QueueReservation {
    fn drop(&mut self) {
        self.release();
    }
}

enum DeferredRequeueFailure {
    Closed(IndexJob),
    Full { job: IndexJob, attempts: usize },
}

async fn defer_background_job(
    mut job: IndexJob,
    job_tx: tokio::sync::mpsc::Sender<IndexJob>,
    initial_backoff: Duration,
    max_attempts: usize,
) -> Result<(), DeferredRequeueFailure> {
    let max_attempts = max_attempts.max(1);
    let mut backoff = initial_backoff;
    for requeue_attempt in 1..=max_attempts {
        tokio::time::sleep(backoff).await;
        match job_tx.try_send(job) {
            Ok(()) => return Ok(()),
            Err(tokio::sync::mpsc::error::TrySendError::Closed(returned)) => {
                return Err(DeferredRequeueFailure::Closed(returned));
            }
            Err(tokio::sync::mpsc::error::TrySendError::Full(returned)) => {
                job = returned;
                if requeue_attempt == max_attempts {
                    return Err(DeferredRequeueFailure::Full {
                        job,
                        attempts: requeue_attempt,
                    });
                }
                backoff = (backoff * 2).min(Duration::from_secs(60));
                warn!(
                    seq = job.seq,
                    path = %job.req.path,
                    requeue_attempt,
                    retry_in_secs = backoff.as_secs(),
                    "indexd deferred background job still waiting for queue capacity"
                );
            }
        }
    }
    unreachable!("bounded deferred requeue loop always returns")
}

async fn release_background_reservation(
    job: &mut IndexJob,
    state: &Arc<Mutex<AppState>>,
    queue_budget: &Arc<QueueBudget>,
) {
    let drained_at_completion = job.reservation.release();
    if drained_at_completion && queue_budget.is_empty() {
        if LONG_READERS.load(Ordering::Acquire) == 0 {
            let guard = state.lock().await;
            if let Err(e) = guard.db.checkpoint_truncate() {
                guard.db.mark_checkpoint_owed();
                tracing::debug!("background-drain WAL checkpoint skipped: {e}");
            }
        } else {
            state.lock().await.db.mark_checkpoint_owed();
        }
        // Bulk indexing can leave freed allocator spans resident; trim once
        // at the queue-empty transition, never per job.
        unsafe {
            libc::malloc_trim(0);
        }
    }
}

/// Aggregate ownership cap for socket request frames. A connection reserves
/// its full configured frame allowance when the first byte arrives. This is
/// intentionally conservative: it avoids allocator-capacity guesswork and
/// guarantees the sum of live frame buffers cannot exceed the configured
/// budget (apart from each connection's small `BufReader` buffer).
struct IngressBudget {
    max_bytes: usize,
    used_bytes: std::sync::atomic::AtomicUsize,
}

impl IngressBudget {
    fn new(max_bytes: usize) -> Arc<Self> {
        Arc::new(Self {
            max_bytes,
            used_bytes: std::sync::atomic::AtomicUsize::new(0),
        })
    }

    fn try_reserve(self: &Arc<Self>, bytes: usize) -> Result<IngressReservation, ()> {
        let mut used = self.used_bytes.load(Ordering::Acquire);
        loop {
            let Some(next) = used.checked_add(bytes) else {
                return Err(());
            };
            if next > self.max_bytes {
                return Err(());
            }
            match self.used_bytes.compare_exchange_weak(
                used,
                next,
                Ordering::AcqRel,
                Ordering::Acquire,
            ) {
                Ok(_) => {
                    return Ok(IngressReservation {
                        budget: self.clone(),
                        bytes,
                    });
                }
                Err(actual) => used = actual,
            }
        }
    }

    #[cfg(test)]
    fn used_bytes(&self) -> usize {
        self.used_bytes.load(Ordering::Acquire)
    }
}

struct IngressReservation {
    budget: Arc<IngressBudget>,
    bytes: usize,
}

impl Drop for IngressReservation {
    fn drop(&mut self) {
        self.budget
            .used_bytes
            .fetch_sub(self.bytes, Ordering::AcqRel);
    }
}

fn index_job_owned_bytes(req: &IndexFileRequest) -> usize {
    req.path
        .len()
        .saturating_add(req.source.len())
        .saturating_add(req.domain.len())
        .saturating_add(req.content.as_ref().map_or(0, String::len))
}

static LONG_READERS: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);

struct LongReaderGuard;

impl LongReaderGuard {
    fn new() -> Self {
        LONG_READERS.fetch_add(1, Ordering::Relaxed);
        Self
    }
}

impl Drop for LongReaderGuard {
    fn drop(&mut self) {
        if LONG_READERS.fetch_sub(1, Ordering::AcqRel) == 1 {
            CHECKPOINT_RETRY_NOTIFY.notify_one();
        }
    }
}

#[tokio::main]
async fn main() -> Result<()> {
    let cli = Cli::parse();
    // Keep the handle: SPEC-12 `indexd.log` needs its `LogReloadHandle`
    // for live EnvFilter swaps (attach_props below).
    //
    // Stats ON — indexd is the fleet canary for the shared stats
    // recorder (2026-07-25 observability arc): event counters +
    // process gauges (uptime/RSS/fds/cpu_seconds_total), served via
    // `indexd.stats.snapshot`. The 60s roll-up driver task below
    // keeps the built-in gauges fresh.
    let log_handle = cosmix_log::init(
        &cosmix_log::LogOpts::default(),
        &cosmix_log::StatsOpts::default(),
        cosmix_log::LogDefaults::daemon("cosmix-indexd").with_stats(true),
    )
    .expect("logging init failed");

    // Load per-service config per the search order on `Cli::config`.
    // Must happen before any request is served because
    // validate_store_entry consults INDEXD_CFG on every incoming chunk.
    let (cfg_path, indexd_cfg) = load_indexd_config(cli.config.as_deref())?;
    info!(
        "loaded indexd config from {} with {} source types",
        cfg_path.display(),
        indexd_cfg.source_types.len()
    );
    let svc = indexd_cfg.service.clone();
    INDEXD_CFG
        .set(indexd_cfg)
        .expect("INDEXD_CFG already initialised — main() ran twice?");
    INDEXD_CFG_PATH
        .set(cfg_path)
        .expect("INDEXD_CFG_PATH already initialised — main() ran twice?");

    // CLI --f32 flag overrides config; env var COSMIX_VECTORS_DB
    // overrides config db path.
    let dtype = if cli.force_f32 || svc.dtype == "f32" {
        DType::F32
    } else {
        DType::F16
    };
    let vindex_dtype =
        vindex::VectorDtype::parse(&svc.vindex_dtype).map_err(|e| anyhow::anyhow!(e))?;
    let max_connections = svc.max_connections.max(1);
    let queue_max_jobs = svc.background_queue_max_jobs;
    let queue_max_bytes = svc.background_queue_max_bytes;
    let background_retry_max_attempts = svc.background_retry_max_attempts.max(1);
    let background_retry_initial_backoff_secs = svc.background_retry_initial_backoff_secs.max(1);
    let db_path = std::env::var("COSMIX_VECTORS_DB").unwrap_or(svc.vectors_db);
    let socket_path = svc.socket_path;
    let model_id = svc.model_id;
    let idle_timeout_secs = svc.idle_timeout_secs;

    let listener = if let Ok(listener) = try_systemd_socket() {
        info!("using systemd socket activation");
        listener
    } else {
        let socket_dir = std::path::Path::new(&socket_path)
            .parent()
            .map(|p| p.to_string_lossy().to_string())
            .unwrap_or_else(|| {
                cosmix_config::cosmix_path(cosmix_config::CosmixDir::Run)
                    .to_string_lossy()
                    .into_owned()
            });
        std::fs::create_dir_all(&socket_dir).with_context(|| format!("creating {socket_dir}"))?;
        let _ = std::fs::remove_file(&socket_path);
        let listener =
            UnixListener::bind(&socket_path).with_context(|| format!("binding {socket_path}"))?;
        std::fs::set_permissions(
            &socket_path,
            std::os::unix::fs::PermissionsExt::from_mode(0o666),
        )?;
        info!("listening on {socket_path}");
        listener
    };

    let db = VectorDb::open(&db_path, vindex_dtype)?;

    // SPEC 12 property substrate: `indexd.log` (runtime-tunable log
    // level/filter). Its sqlite store lives NEXT TO vectors.db, not in
    // it — props audit writes must never contend with vector
    // transactions, and the repair/backup boundary stays clean.
    // PRAGMAs are the caller's responsibility (sqlite.rs contract).
    let props_db_path = std::path::Path::new(&db_path)
        .parent()
        .map(|p| p.join("props.db"))
        .unwrap_or_else(|| PathBuf::from("props.db"));
    let props_journal_limit = i64::try_from(svc.journal_size_limit_bytes)
        .context("journal_size_limit_bytes exceeds SQLite i64 range")?;
    let props_conn = tokio::task::spawn_blocking(move || -> Result<rusqlite::Connection> {
        let conn = rusqlite::Connection::open(&props_db_path)?;
        conn.execute_batch(
            "PRAGMA journal_mode=WAL; \
             PRAGMA foreign_keys=ON; \
             PRAGMA busy_timeout=5000; \
             PRAGMA cache_size=-2000;",
        )?;
        conn.pragma_update(None, "journal_size_limit", props_journal_limit)?;
        Ok(conn)
    })
    .await??;
    let props_store = Arc::new(
        cosmix_props::sqlite::SqliteStore::new("indexd", props_conn)
            .map_err(|e| anyhow::anyhow!("open property store: {e}"))?,
    );
    let mut props_router = cosmix_props::bus::mutation::PropsRouter::new("indexd");
    let log_runtime = cosmix_log_props::register_log_namespace(&mut props_router, &props_store)?;
    let props_router = Arc::new(props_router);
    // Live `indexd.log` watcher — applies any persisted level/filter row
    // once at startup (restart persistence), then swaps the EnvFilter on
    // every accepted `indexd.props.set namespace=log` write.
    cosmix_log_props::attach_props(&log_handle, log_runtime).await?;

    let started = Instant::now();
    let started_at_iso = chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Secs, true);

    // Two-dimensional queue bound: tokio enforces job count and QueueBudget
    // accounts owned payload bytes before the request relinquishes them.
    let (job_tx, mut job_rx) = tokio::sync::mpsc::channel::<IndexJob>(queue_max_jobs.max(1));
    let queue_budget = QueueBudget::new(queue_max_jobs, queue_max_bytes);

    // Seed the corpus cache synchronously so the world publisher never
    // sees (and never publishes) a zeroed snapshot at boot; the
    // reconciler task below keeps it authoritative from then on.
    let corpus_cache = corpus_aggregate(&db_path).unwrap_or_else(|e| {
        warn!("corpus cache seed failed (reconciler will retry): {e}");
        CorpusCache::default()
    });
    let recon_db_path = db_path.clone();

    let state = Arc::new(Mutex::new(AppState {
        model: None,
        dtype,
        model_id,
        db,
        db_path,
        socket_path: socket_path.clone(),
        idle_timeout_secs,
        started,
        started_at_iso,
        model_breaker: CircuitBreaker::new(2, Duration::from_secs(60)),
        embed_breaker: CircuitBreaker::new(3, Duration::from_secs(30)),
        embed_cache: EmbeddingCache::new(),
        inference_gate: Arc::new(tokio::sync::Semaphore::new(1)),
        job_tx: job_tx.clone(),
        queue_budget: queue_budget.clone(),
        corpus_cache,
        corpus_epoch: 0,
    }));
    let connection_gate = Arc::new(tokio::sync::Semaphore::new(max_connections));
    let ingress_budget = IngressBudget::new(svc.max_ingress_bytes);

    // Lazy autonomous WAL-debt backstop. It sleeps only while debt is owed
    // (or after the last long reader exits), retries no more often than every
    // five minutes, and re-arms itself only if the checkpoint remains busy.
    let checkpoint_state = state.clone();
    let checkpoint_queue_budget = queue_budget.clone();
    tokio::spawn(async move {
        loop {
            CHECKPOINT_RETRY_NOTIFY.notified().await;
            loop {
                if !checkpoint_state.lock().await.db.checkpoint_owed.is_owed() {
                    break;
                }
                tokio::time::sleep(Duration::from_secs(5 * 60)).await;
                if LONG_READERS.load(Ordering::Acquire) != 0 || !checkpoint_queue_budget.is_empty()
                {
                    continue;
                }
                let guard = checkpoint_state.lock().await;
                if !guard.db.checkpoint_owed.is_owed() {
                    break;
                }
                if let Err(e) = guard.db.checkpoint_truncate() {
                    guard.db.mark_checkpoint_owed();
                    tracing::debug!("autonomous WAL checkpoint retry failed: {e}");
                }
            }
        }
    });

    // Corpus reconciler: authoritative refresh of the cache every 30s,
    // aggregation on a dedicated read-only connection inside
    // spawn_blocking — the AppState mutex is only taken for the final
    // in-memory swap. Replaces the 1 Hz under-mutex db.stats() world
    // tick (the measured ~12%-of-a-core standing burn).
    let recon_state = state.clone();
    tokio::spawn(async move {
        let mut tick = tokio::time::interval(Duration::from_secs(30));
        tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        // Failure-transition signalling: the reconcile is the ONLY thing
        // keeping per-source counts and db size honest (mutation nudges
        // don't cover them), so persistent failure must surface as a
        // warn, not vanish at debug. Warn on the first failure and every
        // 10th thereafter (~5 min), info on recovery. Every attempt is
        // also counted under `internal.corpus_reconcile` in
        // request_metrics so a failure streak is visible to samplers.
        let mut consecutive_failures: u64 = 0;
        loop {
            tick.tick().await;
            let path = recon_db_path.clone();
            let epoch_before = recon_state.lock().await.corpus_epoch;
            let t0 = Instant::now();
            let result = tokio::task::spawn_blocking(move || corpus_aggregate(&path)).await;
            let elapsed_us = t0.elapsed().as_micros() as u64;
            let mut t = ReqTiming {
                db_us: elapsed_us,
                ..Default::default()
            };
            let outcome = match result {
                Ok(Ok(cache)) => {
                    let mut guard = recon_state.lock().await;
                    let applied = guard.corpus_epoch == epoch_before;
                    if applied {
                        guard.corpus_cache = cache;
                    } else {
                        // A mutation nudged the cache while we were
                        // aggregating — its numbers are FRESHER than our
                        // snapshot. Discard; next tick re-aggregates.
                        // Surfaces as `cache_hits` on this bucket.
                        t.cache_hit = true;
                    }
                    drop(guard);
                    // Recovery means an authoritative swap actually
                    // LANDED — a discarded aggregate proves the query
                    // works but leaves the cache un-reconciled, so the
                    // failure streak stands until a swap applies.
                    if applied && consecutive_failures > 0 {
                        info!(
                            after_failures = consecutive_failures,
                            "corpus reconcile recovered"
                        );
                        consecutive_failures = 0;
                    }
                    Outcome::Ok
                }
                Ok(Err(e)) => {
                    consecutive_failures += 1;
                    if consecutive_failures == 1 || consecutive_failures.is_multiple_of(10) {
                        warn!(
                            consecutive_failures,
                            "corpus reconcile failing; cached per-source counts/db size going stale: {e}"
                        );
                    }
                    Outcome::Error
                }
                Err(join) => {
                    consecutive_failures += 1;
                    if consecutive_failures == 1 || consecutive_failures.is_multiple_of(10) {
                        warn!(
                            consecutive_failures,
                            "corpus reconcile task join error: {join}"
                        );
                    }
                    Outcome::Error
                }
            };
            METRICS
                .for_action("internal.corpus_reconcile")
                .record(&t, elapsed_us, 0, outcome);
        }
    });

    // Stats roll-up driver: the shared recorder validates an interval
    // but nothing in the core starts a periodic driver (the core is
    // runtime-agnostic — tokio is prometheus-only there), so the
    // daemon owns the task. Refreshes the built-in process gauges
    // (incl. cosmix_process_cpu_seconds_total) and drains sinks every
    // 60s; without it, gauges only update at shutdown.
    if let Some(recorder) = cosmix_log::stats::installed_recorder() {
        tokio::spawn(async move {
            let mut tick = tokio::time::interval(Duration::from_secs(60));
            tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
            loop {
                tick.tick().await;
                cosmix_log::stats::perform_rollup(&recorder, 60);
            }
        });
    }

    let (activity_tx, mut activity_rx) = tokio::sync::mpsc::channel::<()>(16);

    if idle_timeout_secs == 0 {
        info!("ready — model loads on first request, stays resident (idle unload disabled)");
    } else {
        info!("ready — model loads on first request, unloads after {idle_timeout_secs}s idle");
    }

    // Spawn supervised Bus broker registration (non-blocking — if the broker
    // isn't running, indexd still serves the socket API). Build provenance
    // ONCE here so `started_at` is the true process start, then hand it to the
    // reconnect loop which clones it per (re)connect attempt.
    let bus_state = state.clone();
    let bus_activity_tx = activity_tx.clone();
    let bi = cosmix_buildinfo::build_info!();
    let prov = cosmix_bus::RegisterProvenance::from_parts(
        bi.pkg,
        bi.version,
        bi.git_sha,
        bi.git_dirty,
        bi.build_time,
        cosmix_buildinfo::now_rfc3339(),
    );
    tokio::spawn(run_bus_client_loop(
        bus_state,
        bus_activity_tx,
        prov,
        props_router.clone(),
    ));

    // Spawn idle watchdog
    let watchdog_state = state.clone();
    tokio::spawn(async move {
        loop {
            if activity_rx.recv().await.is_none() {
                break;
            }
            while activity_rx.try_recv().is_ok() {}

            let timeout = watchdog_state.lock().await.idle_timeout_secs;
            // 0 = never unload. Don't run the countdown at all; just go back
            // to waiting for the next activity ping (which we drain above).
            if timeout == 0 {
                continue;
            }
            let mut idle_remaining = timeout;
            loop {
                tokio::select! {
                    _ = tokio::time::sleep(Duration::from_secs(1)) => {
                        idle_remaining = idle_remaining.saturating_sub(1);
                        if idle_remaining == 0 {
                            let mut guard = watchdog_state.lock().await;
                            if guard.inference_gate.available_permits() == 0 {
                                // A timed-out blocking worker may still own an
                                // Arc<Model>. Never drop the state Arc while
                                // that permit is held: a later request could
                                // otherwise load a second full model.
                                idle_remaining = 1;
                                continue;
                            }
                            if guard.model.is_some() {
                                info!("model idle for {timeout}s, unloading to free memory");
                                guard.model = None;
                                drop(guard);
                                unsafe { libc::malloc_trim(0); }
                                metrics::RSS_AFTER_UNLOAD_BYTES
                                    .store(rss_bytes(), Ordering::Relaxed);
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

    // Spawn the single background indexing worker. It owns `job_rx` and
    // drains the queue serially for the daemon's lifetime, reusing the exact
    // synchronous `handle_index_file` path (so background and foreground
    // indexing are byte-for-byte identical). `recv()` returns `None` only
    // once every `job_tx` clone is dropped (process teardown), at which point
    // the loop exits cleanly. Jobs are owned `IndexFileRequest` values, so the
    // future is `Send + 'static`.
    let worker_state = state.clone();
    let worker_activity_tx = activity_tx.clone();
    let worker_queue_budget = queue_budget.clone();
    let worker_job_tx = job_tx.clone();
    tokio::spawn(async move {
        while let Some(mut job) = job_rx.recv().await {
            let path = job.req.path.clone();
            let queue_us = job.enqueued.elapsed().as_micros() as u64;
            let queue_ms = queue_us / 1000;
            // The foreground request already returned its ack; without this
            // start record a stuck job leaves no evidence in the journal.
            info!(
                seq = job.seq,
                path = %path,
                queue_ms,
                peer = %job.peer,
                "indexd_job_started"
            );
            let ctx = RequestContext {
                transport: "background",
                peer: job.peer.clone(),
            };
            let t0 = Instant::now();
            let mut t = ReqTiming {
                queue_us,
                ..Default::default()
            };
            let max_attempts = background_retry_max_attempts;
            let resp =
                handle_index_file(job.req.clone(), &worker_state, &worker_activity_tx, &mut t)
                    .await;
            if transient_background_failure(&resp) {
                if job.attempt < max_attempts {
                    let backoff = background_retry_backoff(
                        background_retry_initial_backoff_secs,
                        job.attempt,
                    );
                    warn!(
                        seq = job.seq,
                        path = %path,
                        attempt = job.attempt,
                        retry_in_secs = backoff.as_secs(),
                        error = %resp,
                        "indexd background job deferred after transient inference failure"
                    );
                    job.attempt += 1;
                    let deferred_tx = worker_job_tx.clone();
                    let deferred_state = worker_state.clone();
                    let deferred_queue_budget = worker_queue_budget.clone();
                    tokio::spawn(async move {
                        let failure =
                            defer_background_job(job, deferred_tx, backoff, max_attempts).await;
                        let (mut abandoned, reason) = match failure {
                            Ok(()) => return,
                            Err(DeferredRequeueFailure::Closed(job)) => {
                                (job, "background job queue closed".to_string())
                            }
                            Err(DeferredRequeueFailure::Full { job, attempts }) => (
                                job,
                                format!(
                                    "background job queue remained full for {attempts} re-submit attempts"
                                ),
                            ),
                        };
                        error!(
                            seq = abandoned.seq,
                            path = %abandoned.req.path,
                            attempt = abandoned.attempt,
                            reason,
                            "indexd background job ABANDONED during deferred re-submit"
                        );
                        release_background_reservation(
                            &mut abandoned,
                            &deferred_state,
                            &deferred_queue_budget,
                        )
                        .await;
                    });
                    continue;
                } else {
                    error!(
                        seq = job.seq,
                        path = %path,
                        attempts = job.attempt,
                        error = %resp,
                        "indexd background job ABANDONED after transient inference failures"
                    );
                }
            }
            let outcome = classify_response(&resp);
            // On failure, record_request below emits the structured warn
            // (indexd_request_failed) carrying the error text, and the
            // start record above already carries the path — no separate
            // duplicate warn.
            record_request(
                job.seq,
                "index_file_job",
                &ctx,
                job.bytes,
                &t,
                t0.elapsed(),
                outcome,
                &resp,
            );
            // The reservation intentionally remains live for every retry, so
            // accepted work cannot evade the queue's byte/count budget while
            // deferred. Release only on success or loud final abandonment.
            release_background_reservation(&mut job, &worker_state, &worker_queue_budget).await;
        }
        info!("background indexing worker exiting (job queue closed)");
    });

    loop {
        // Never let an accept error escape this loop. If `?` propagated the
        // error out of main(), the local `listener` would be dropped (closing
        // the fd) but the runtime can't shut down cleanly while spawn_blocking
        // model work is in flight — the process stays alive with an orphaned
        // socket file, systemd still reports Active: running, and every client
        // gets ECONNREFUSED.
        let (stream, _) = match listener.accept().await {
            Ok(s) => s,
            Err(e) => {
                error!("accept error: {e}; backing off 100ms and retrying");
                tokio::time::sleep(Duration::from_millis(100)).await;
                continue;
            }
        };
        let app_state = state.clone();
        let tx = activity_tx.clone();
        let connection_ingress_budget = ingress_budget.clone();
        let connection_permit = match connection_gate.clone().try_acquire_owned() {
            Ok(permit) => permit,
            Err(_) => {
                tokio::spawn(async move {
                    let mut stream = stream;
                    let response = json_code_error(
                        "connection_limit",
                        "too many concurrent indexd connections",
                    );
                    let _ =
                        write_response_line(&mut stream, &response, Duration::from_secs(300)).await;
                    let _ = stream.shutdown().await;
                });
                continue;
            }
        };

        tokio::spawn(async move {
            let _connection_permit = connection_permit;
            if let Err(e) =
                handle_connection(stream, &app_state, &tx, &connection_ingress_budget).await
            {
                // A peer that hangs up before reading its response is normal
                // socket lifecycle, not a daemon fault: fire-and-forget
                // clients (cmm-tick emits) and timeout-bounded callers
                // (cosmix-mcp killed mid context_search) do this by design,
                // and the requested work has already completed by the time
                // the response write fails.
                let peer_gone = e.downcast_ref::<std::io::Error>().is_some_and(|io| {
                    matches!(
                        io.kind(),
                        std::io::ErrorKind::BrokenPipe
                            | std::io::ErrorKind::ConnectionReset
                            | std::io::ErrorKind::UnexpectedEof
                    )
                });
                if peer_gone {
                    tracing::debug!("client disconnected before response: {e}");
                } else {
                    error!("connection error: {e}");
                }
            }
        });
    }
}

/// Handle incoming Bus commands from the broker mesh.
/// Maps Bus commands to the same JSON protocol used by the Unix socket.
/// Supervised Bus broker registration: register, serve commands until the
/// connection drops, then reconnect forever with exponential backoff (1s →
/// 60s cap). This is what lets indexd survive a `noded` restart without an
/// indexd restart — the one-shot spawn it replaces left a CLOSE-WAIT socket
/// and silently stopped Bus registration, `world.indexd` publishing, and
/// command handling until indexd was bounced.
///
/// `prov` is built ONCE at process start (so `started_at` is the true start)
/// and cloned per attempt, per the `connect_default_with_provenance` contract.
/// The socket-only fallback is unaffected: this runs as its own spawned task,
/// so the Unix `embed.sock` accept loop in `main` serves the embed API
/// regardless of broker state.
async fn run_bus_client_loop(
    state: Arc<Mutex<AppState>>,
    activity_tx: tokio::sync::mpsc::Sender<()>,
    prov: cosmix_bus::RegisterProvenance,
    props_router: Arc<cosmix_props::bus::mutation::PropsRouter>,
) {
    let mut backoff = Duration::from_secs(1);
    loop {
        match cosmix_config::client_helpers::connect_default_with_provenance("indexd", prov.clone())
            .await
        {
            Ok(client) => {
                info!("registered as Bus service 'indexd' on broker");
                backoff = Duration::from_secs(1);
                let client = std::sync::Arc::new(client);

                // SPEC 07 §3+§4 — periodic snapshot diff publishing
                // (props.changed events + world.indexd retained snapshot).
                // Aborted on disconnect below so it can't leak across
                // reconnects (a fresh run_loop is spawned each connect).
                let world = tokio::spawn(world::run_loop(client.clone(), state.clone()));

                // Returns when the broker connection closes (incoming
                // stream ends).
                handle_bus_commands(
                    client,
                    state.clone(),
                    activity_tx.clone(),
                    props_router.clone(),
                )
                .await;

                world.abort();
                warn!("broker connection closed; reconnecting");
            }
            Err(e) => {
                info!("broker unavailable; retrying in {backoff:?}: {e}");
            }
        }
        tokio::time::sleep(backoff).await;
        backoff = (backoff * 2).min(Duration::from_secs(60));
    }
}

/// Bridge an incoming Bus props command onto the SPEC-12 `PropsRouter`
/// dispatch surface (maild's `dispatch_props` pattern). Scalar args are
/// projected into Bus headers first, then raw headers override — the
/// router reads `namespace`/`key`/`merge` from headers, while MCP
/// `bus_call` clients supply them as args.
async fn dispatch_router_props(
    router: &cosmix_props::bus::mutation::PropsRouter,
    suffix: &str,
    cmd: &cosmix_client::IncomingCommand,
) -> (u8, String) {
    let mut msg = cosmix_bus::bus::BusMessage::new();
    if let Some(obj) = cmd.args.as_object() {
        for (k, v) in obj {
            if k == "body" {
                continue;
            }
            let s = match v {
                serde_json::Value::String(s) => s.clone(),
                other => other.to_string(),
            };
            msg.set(k, &s);
        }
    }
    for (k, v) in &cmd.headers {
        msg.set(k, v);
    }
    // Body: arg-only clients (MCP bus_call has no body channel) supply
    // the record as a `body` arg, which WINS over cmd.body — noded
    // echoes the whole args object into the message body for such
    // clients, so trusting a non-empty cmd.body would store the args
    // envelope as the record (bit the first live `indexd.log` set:
    // the row held {"body":{...},"namespace":"log"} and the watcher
    // read no `level` at all). Real Bus clients don't send a `body`
    // arg and their true bodies pass through untouched.
    msg.body = match cmd.args.get("body") {
        Some(serde_json::Value::String(s)) => s.clone(),
        Some(other) => other.to_string(),
        None => cmd.body.clone(),
    };
    let peer = cosmix_props::namespace::PeerIdentity {
        service_name: if cmd.from.is_empty() {
            None
        } else {
            Some(cmd.from.clone())
        },
        ..Default::default()
    };
    let resp = router.dispatch(suffix, &msg, &peer).await;
    // PropsResponse.rc is the §9 numeric rc (0 / 10 / 20); clamp for the
    // NodedClient u8 — anything >=10 surfaces as an error to the caller.
    let rc: u8 = u8::try_from(resp.rc.max(0)).unwrap_or(20);
    (rc, resp.body)
}

async fn handle_bus_commands(
    client: std::sync::Arc<cosmix_client::NodedClient>,
    state: Arc<Mutex<AppState>>,
    activity_tx: tokio::sync::mpsc::Sender<()>,
    props_router: Arc<cosmix_props::bus::mutation::PropsRouter>,
) {
    let mut rx = match client.incoming_async().await {
        Some(rx) => rx,
        None => return,
    };

    while let Some(cmd) = rx.recv().await {
        // Broker control frames are not requests: legacy peers ignore the
        // admit challenge. Letting it reach process_request pollutes the
        // invalid counters and warns falsely on every reconnect.
        if cmd.command == "noded.admit.challenge" {
            continue;
        }

        // Shared stats snapshot verb (fleet canary). Caps: operator-class
        // default per plan §4.3 — snapshot allowed, restricted-family
        // labels come back hashed. Counted like every other request
        // (snapshot scans the registry and serialises up to 1 MiB);
        // successful reads are sampler-suppressed like `stats`.
        if cmd.command == "indexd.stats.snapshot" || cmd.command == "stats.snapshot" {
            let seq = metrics::REQUEST_SEQ.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            let t0 = Instant::now();
            let (rc, body) = match cosmix_log::stats::installed_recorder() {
                Some(recorder) => cosmix_log::stats::handle_snapshot_bus(
                    &cmd,
                    &recorder,
                    &cosmix_log::stats::SnapshotCaps {
                        has_snapshot: true,
                        has_raw_labels: false,
                    },
                ),
                None => (10, json_error("stats recorder not installed")),
            };
            let ctx = RequestContext {
                transport: "bus",
                peer: cmd.from.clone(),
            };
            record_request(
                seq,
                "stats.snapshot",
                &ctx,
                cmd.body.len() as u64,
                &ReqTiming::default(),
                t0.elapsed(),
                if rc == 0 { Outcome::Ok } else { Outcome::Error },
                &body,
            );
            if let Err(e) = client.respond(&cmd, rc, &body).await {
                error!("failed to send Bus stats.snapshot response: {e}");
            }
            continue;
        }

        // SPEC 12 namespace detection runs BEFORE the flat watch sugar:
        // a `namespace=log` watch must reach the PropsRouter (which,
        // without a subscribe granter installed, deliberately refuses —
        // the SPEC-12-correct degraded state) instead of falsely being
        // answered with the SPEC-07 world topic.
        let namespaced = cmd.header("namespace").is_some()
            || cmd
                .args
                .as_object()
                .is_some_and(|o| o.contains_key("namespace"));

        // SPEC 07 §3 — `indexd.props.watch` is L2's "subscribe to changes"
        // sugar. As a peer (not the broker) indexd can't enroll the caller's
        // connection in the broker directly, so it returns the topic name
        // and the caller subscribes themselves via `topic.subscribe`. The
        // L2 contract is the topic existing + emitting events, which the
        // periodic-diff world loop satisfies.
        if !namespaced && (cmd.command == "indexd.props.watch" || cmd.command == "props.watch") {
            let body = serde_json::json!({
                "topic": world::PROPS_CHANGED_TOPIC,
                "info": "Subscribe to this topic via topic.subscribe to receive props.changed events.",
            })
            .to_string();
            if let Err(e) = client.respond(&cmd, 0, &body).await {
                error!("failed to send Bus props.watch response: {e}");
            }
            continue;
        }

        // SPEC 07 §2 — `indexd.props.{get,list,describe}` against the
        // observable property surface. Intercept *before* the legacy
        // strip-`indexd.` dispatch because process_request's Request enum
        // does not (and shouldn't) know about props subcommands.
        if let Some(suffix) = cmd.command.strip_prefix("indexd.props.") {
            // SPEC 12: namespaced calls (namespace=log today) go to the
            // PropsRouter substrate; the flat no-namespace surface keeps
            // serving the live SPEC-07 PropTree snapshot unchanged.
            if namespaced {
                // Router calls get the same completion accounting as
                // every other request — a props.set that flips the log
                // level is absolutely a request worth attributing.
                let seq = metrics::REQUEST_SEQ.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                let t0 = Instant::now();
                let t = ReqTiming::default();
                let (rc, body) = dispatch_router_props(&props_router, suffix, &cmd).await;
                let ctx = RequestContext {
                    transport: "bus",
                    peer: cmd.from.clone(),
                };
                record_request(
                    seq,
                    props_bucket(suffix),
                    &ctx,
                    cmd.body.len() as u64,
                    &t,
                    t0.elapsed(),
                    if rc == 0 { Outcome::Ok } else { Outcome::Error },
                    &body,
                );
                if let Err(e) = client.respond(&cmd, rc, &body).await {
                    error!("failed to send Bus props response: {e}");
                }
                continue;
            }
            let snapshot = collect_props(&state, "props.get").await;
            let args_json = props::parse_args(cmd.header("args")).or_else(|| {
                if cmd.args.is_object() && !cmd.args.as_object().unwrap().is_empty() {
                    Some(cmd.args.clone())
                } else if cmd.body.is_empty() {
                    None
                } else {
                    serde_json::from_str(&cmd.body).ok()
                }
            });
            let resp_inner = cosmix_props::bus::dispatch_props(
                &snapshot,
                suffix,
                args_json.as_ref(),
                /* redact_sensitive = */ true,
            );
            let rc_u8: u8 = resp_inner.rc.clamp(0, 255) as u8;
            if let Err(e) = client.respond(&cmd, rc_u8, &resp_inner.body).await {
                error!("failed to send Bus props response: {e}");
            }
            continue;
        }

        // The Bus command args ARE the JSON request, just add the "action" field
        // e.g. bus_call("indexd", "indexd.search", {"query": "...", "limit": 5})
        // becomes {"action": "search", "query": "...", "limit": 5}
        let action = cmd.command.strip_prefix("indexd.").unwrap_or(&cmd.command);

        let request_json = if cmd.args.is_object() {
            let mut args = cmd.args.clone();
            args.as_object_mut()
                .unwrap()
                .insert("action".into(), serde_json::Value::String(action.into()));
            args.to_string()
        } else {
            serde_json::json!({"action": action}).to_string()
        };

        let ctx = RequestContext {
            transport: "bus",
            peer: cmd.from.clone(),
        };
        let response = process_request(&request_json, &state, &activity_tx, &ctx).await;

        // Structural error check: every indexd error response starts with
        // `{"error":` (json_error). The old `.contains("\"error\"")` could
        // misclassify successful payloads whose data mentions that key.
        let rc = if classify_response(&response) == Outcome::Error {
            10
        } else {
            0
        };
        if let Err(e) = client.respond(&cmd, rc, &response).await {
            error!("failed to send Bus response: {e}");
        }
    }

    info!("broker connection closed");
}

/// Best-effort unix peer description, resolved ONCE per connection.
/// UID/GID/PID come from `SO_PEERCRED` (kernel-authenticated for the
/// *connector* — a passed fd can be written by someone else later);
/// `comm` is display-only and commonly unavailable: the unit runs with
/// `ProtectProc=invisible`, so cross-UID `/proc/<pid>/comm` reads fail.
/// Never weaken the sandbox for a nicer label — tolerate `?`.
fn describe_unix_peer(stream: &tokio::net::UnixStream) -> String {
    match stream.peer_cred() {
        Ok(cred) => {
            let pid = cred.pid();
            let comm = pid
                .and_then(|p| std::fs::read_to_string(format!("/proc/{p}/comm")).ok())
                .map(|s| s.trim().to_string())
                .unwrap_or_else(|| "?".to_string());
            match pid {
                Some(p) => format!("uid={} gid={} pid={p} comm={comm}", cred.uid(), cred.gid()),
                None => format!("uid={} gid={} pid=? comm=?", cred.uid(), cred.gid()),
            }
        }
        Err(_) => "unknown".to_string(),
    }
}

async fn write_response_line<W: tokio::io::AsyncWrite + Unpin>(
    writer: &mut W,
    response: &str,
    deadline: Duration,
) -> Result<()> {
    tokio::time::timeout(deadline, async {
        writer.write_all(response.as_bytes()).await?;
        writer.write_all(b"\n").await?;
        writer.flush().await
    })
    .await
    .map_err(|_| anyhow::anyhow!("response write timed out after {}s", deadline.as_secs()))??;
    Ok(())
}

async fn handle_connection(
    stream: tokio::net::UnixStream,
    state: &Arc<Mutex<AppState>>,
    activity_tx: &tokio::sync::mpsc::Sender<()>,
    ingress_budget: &Arc<IngressBudget>,
) -> Result<()> {
    let ctx = RequestContext {
        transport: "unix",
        peer: describe_unix_peer(&stream),
    };
    let (reader, mut writer) = stream.into_split();
    let mut reader = BufReader::new(reader);
    let mut line = Vec::new();
    let svc = &indexd_cfg().service;
    let write_timeout = Duration::from_secs(if svc.connection_idle_timeout_secs == 0 {
        300
    } else {
        svc.connection_idle_timeout_secs
    });

    loop {
        line.clear();
        let mut ingress_reservation = None;
        let idle_timeout = if svc.connection_idle_timeout_secs == 0 {
            None
        } else {
            Some(Duration::from_secs(svc.connection_idle_timeout_secs))
        };
        let frame_timeout = if svc.request_frame_timeout_secs == 0 {
            None
        } else {
            Some(Duration::from_secs(svc.request_frame_timeout_secs))
        };
        let read = read_request_line(
            &mut reader,
            &mut line,
            svc.max_request_line_bytes,
            idle_timeout,
            frame_timeout,
            ingress_budget,
            &mut ingress_reservation,
        )
        .await?;
        match read {
            RequestLineRead::Eof => break,
            RequestLineRead::TimedOut => {
                let response = json_code_error("read_timeout", "connection read timed out");
                drop(ingress_reservation.take());
                write_response_line(&mut writer, &response, write_timeout).await?;
                break;
            }
            RequestLineRead::FrameTimedOut => {
                let response =
                    json_code_error("frame_timeout", "request frame exceeded its total deadline");
                drop(ingress_reservation.take());
                write_response_line(&mut writer, &response, write_timeout).await?;
                break;
            }
            RequestLineRead::IngressFull => {
                let response = json_code_error(
                    "ingress_busy",
                    "aggregate request ingress byte budget exhausted",
                );
                drop(ingress_reservation.take());
                write_response_line(&mut writer, &response, write_timeout).await?;
                break;
            }
            RequestLineRead::TooLarge => {
                let response = json_code_error(
                    "request_too_large",
                    "request line exceeds configured byte limit",
                );
                drop(ingress_reservation.take());
                write_response_line(&mut writer, &response, write_timeout).await?;
                break;
            }
            RequestLineRead::Complete => {}
        }
        let input = match std::str::from_utf8(&line) {
            Ok(input) => input,
            Err(_) => {
                let response = json_error("invalid request: input is not UTF-8");
                drop(ingress_reservation.take());
                write_response_line(&mut writer, &response, write_timeout).await?;
                break;
            }
        };
        let response = process_request(input, state, activity_tx, &ctx).await;
        // Input ownership/accounting ends with request processing, not with
        // peer consumption of the response. Release the 20 MiB reservation
        // and large frame allocation before entering a potentially slow write.
        drop(ingress_reservation.take());
        if line.capacity() > 1024 * 1024 {
            line = Vec::new();
        } else {
            line.clear();
        }
        write_response_line(&mut writer, &response, write_timeout).await?;
    }

    Ok(())
}

enum RequestLineRead {
    Eof,
    Complete,
    TooLarge,
    TimedOut,
    FrameTimedOut,
    IngressFull,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ReadTimeoutKind {
    Idle,
    Frame,
}

fn next_read_timeout(
    idle_timeout: Option<Duration>,
    frame_timeout: Option<Duration>,
    frame_elapsed: Option<Duration>,
) -> Option<(Duration, ReadTimeoutKind)> {
    let frame_remaining = match (frame_timeout, frame_elapsed) {
        (Some(limit), Some(elapsed)) => Some(limit.saturating_sub(elapsed)),
        _ => None,
    };
    match (idle_timeout, frame_remaining) {
        (Some(idle), Some(frame)) if frame <= idle => Some((frame, ReadTimeoutKind::Frame)),
        (Some(idle), _) => Some((idle, ReadTimeoutKind::Idle)),
        (None, Some(frame)) => Some((frame, ReadTimeoutKind::Frame)),
        (None, None) => None,
    }
}

async fn read_request_line<R: tokio::io::AsyncBufRead + Unpin>(
    reader: &mut R,
    line: &mut Vec<u8>,
    max_bytes: usize,
    idle_timeout: Option<Duration>,
    frame_timeout: Option<Duration>,
    ingress_budget: &Arc<IngressBudget>,
    ingress_reservation: &mut Option<IngressReservation>,
) -> std::io::Result<RequestLineRead> {
    let mut frame_started: Option<Instant> = None;
    loop {
        let wait = next_read_timeout(
            idle_timeout,
            frame_timeout,
            frame_started.map(|started| started.elapsed()),
        );
        let available = if let Some((timeout, kind)) = wait {
            match tokio::time::timeout(timeout, reader.fill_buf()).await {
                Ok(result) => result?,
                Err(_) => {
                    return Ok(match kind {
                        ReadTimeoutKind::Idle => RequestLineRead::TimedOut,
                        ReadTimeoutKind::Frame => RequestLineRead::FrameTimedOut,
                    });
                }
            }
        } else {
            reader.fill_buf().await?
        };
        if available.is_empty() {
            return Ok(if line.is_empty() {
                RequestLineRead::Eof
            } else {
                RequestLineRead::Complete
            });
        }
        if frame_started.is_none() {
            frame_started = Some(Instant::now());
            match ingress_budget.try_reserve(max_bytes) {
                Ok(reservation) => *ingress_reservation = Some(reservation),
                Err(()) => return Ok(RequestLineRead::IngressFull),
            }
        }
        let newline = available.iter().position(|&byte| byte == b'\n');
        // The configured cap is payload bytes, consistently with the Bus
        // path's `input.len()`: the delimiter is framing, not payload.
        let payload_take = newline.unwrap_or(available.len());
        if line
            .len()
            .checked_add(payload_take)
            .is_none_or(|len| len > max_bytes)
        {
            return Ok(RequestLineRead::TooLarge);
        }
        line.try_reserve_exact(payload_take)
            .map_err(|e| std::io::Error::other(format!("reserving request frame: {e}")))?;
        line.extend_from_slice(&available[..payload_take]);
        reader.consume(payload_take + usize::from(newline.is_some()));
        if newline.is_some() {
            return Ok(RequestLineRead::Complete);
        }
    }
}

/// Wire name of a parsed request, for metrics/log attribution.
fn action_name(req: &Request) -> &'static str {
    match req {
        Request::Embed(_) => "embed",
        Request::Store(_) => "store",
        Request::Search(_) => "search",
        Request::Update(_) => "update",
        Request::Delete(_) => "delete",
        Request::List(_) => "list",
        Request::Feedback(_) => "feedback",
        Request::Supersede(_) => "supersede",
        Request::Stale(_) => "stale",
        Request::IndexFile(_) => "index_file",
        Request::Stats => "stats",
    }
}

/// Requests slower than this log at info even when otherwise cheap.
const SLOW_REQUEST_MS: u128 = 250;

/// Metrics bucket for a SPEC-12 router props suffix. Unknown suffixes
/// land in `invalid` — the router rejects them anyway.
fn props_bucket(suffix: &str) -> &'static str {
    match suffix {
        "set" => "props.set",
        "delete" => "props.delete",
        "get" => "props.get",
        "list" => "props.list",
        "describe" => "props.describe",
        "watch" => "props.watch",
        _ => "invalid",
    }
}

/// One structured completion record + counter update per request/job.
/// Levels: warn = failed; info = anything expensive, cold, or mutating;
/// debug = cheap list / cache-hit reads; successful `stats` logs nothing
/// (a minutely sampler would add 1,440 useless lines a day — it is
/// still counted).
#[allow(clippy::too_many_arguments)]
fn record_request(
    seq: u64,
    action: &str,
    ctx: &RequestContext,
    bytes: u64,
    t: &ReqTiming,
    elapsed: Duration,
    outcome: Outcome,
    response: &str,
) {
    let elapsed_us = elapsed.as_micros() as u64;
    METRICS
        .for_action(action)
        .record(t, elapsed_us, bytes, outcome);

    if matches!(outcome, Outcome::Error | Outcome::Invalid) {
        // Error bodies are indexd-generated (json_error), not user data;
        // truncate defensively anyway.
        let brief: String = response.chars().take(200).collect();
        warn!(
            seq,
            action,
            transport = ctx.transport,
            peer = %ctx.peer,
            bytes,
            outcome = outcome.as_str(),
            elapsed_ms = elapsed.as_millis() as u64,
            error = %brief,
            "indexd_request_failed"
        );
        return;
    }
    if matches!(action, "stats" | "stats.snapshot") {
        // Sampler-safe: successful metrics reads are counted, never
        // logged (a minutely poller would add 1,440 noise lines/day).
        return;
    }
    // Only pure reads may demote to debug — a cache-hit store/update
    // still mutated durable state and stays at info per the policy.
    let cheap_read =
        matches!(action, "list") || (t.cache_hit && matches!(action, "embed" | "search"));
    let debug_level = cheap_read && !t.cold_model && elapsed.as_millis() < SLOW_REQUEST_MS;
    macro_rules! emit {
        ($lvl:ident) => {
            tracing::$lvl!(
                seq,
                action,
                transport = ctx.transport,
                peer = %ctx.peer,
                bytes,
                outcome = outcome.as_str(),
                cold_model = t.cold_model,
                cache_hit = t.cache_hit,
                work = t.work,
                lock_wait_us = t.lock_wait_us,
                model_load_us = t.model_load_us,
                embed_us = t.embed_us,
                db_us = t.db_us,
                vector_us = t.vector_us,
                elapsed_ms = elapsed.as_millis() as u64,
                "indexd_request_complete"
            )
        };
    }
    if debug_level {
        emit!(debug);
    } else {
        emit!(info);
    }
}

fn finish_request(
    seq: u64,
    action: &str,
    ctx: &RequestContext,
    bytes: u64,
    t: &ReqTiming,
    started: Instant,
    response: String,
) -> String {
    record_request(
        seq,
        action,
        ctx,
        bytes,
        t,
        started.elapsed(),
        classify_response(&response),
        &response,
    );
    response
}

/// Lock the global state, charging the wait to the request's timing.
/// Mutex wait is tracked separately from `db_us` so lock contention
/// can't hide inside apparent SQLite time.
async fn lock_timed<'a>(
    state: &'a Arc<Mutex<AppState>>,
    t: &mut ReqTiming,
) -> tokio::sync::MutexGuard<'a, AppState> {
    let t0 = Instant::now();
    let guard = state.lock().await;
    t.lock_wait_us += t0.elapsed().as_micros() as u64;
    guard
}

async fn process_request(
    input: &str,
    state: &Arc<Mutex<AppState>>,
    activity_tx: &tokio::sync::mpsc::Sender<()>,
    ctx: &RequestContext,
) -> String {
    let seq = metrics::REQUEST_SEQ.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let bytes = input.len() as u64;
    let t0 = Instant::now();
    let mut t = ReqTiming::default();

    if input.len() > indexd_cfg().service.max_request_line_bytes {
        let response =
            json_code_error("request_too_large", "request exceeds configured byte limit");
        record_request(
            seq,
            "invalid",
            ctx,
            bytes,
            &t,
            t0.elapsed(),
            Outcome::Invalid,
            &response,
        );
        return response;
    }

    let req: Request = match serde_json::from_str(input) {
        Ok(r) => r,
        Err(e) => {
            let response = json_error(&format!("invalid request: {e}"));
            // Charge a malformed-but-recognisable action to its own
            // bucket (outcome=invalid) so its real traffic volume isn't
            // understated; only actionless garbage lands in `invalid`.
            record_request(
                seq,
                &metrics::action_of_invalid(input),
                ctx,
                bytes,
                &t,
                t0.elapsed(),
                Outcome::Invalid,
                &response,
            );
            return response;
        }
    };
    let action = action_name(&req);

    let response = match req {
        Request::Embed(req) => handle_embed(req, state, activity_tx, &mut t).await,
        Request::Store(req) => handle_store(req, state, activity_tx, &mut t).await,
        Request::Search(req) => handle_search(req, state, activity_tx, &mut t).await,
        Request::Update(req) => handle_update(req, state, activity_tx, &mut t).await,
        Request::Delete(req) => handle_delete(req, state, &mut t).await,
        Request::List(req) => handle_list(req, state, &mut t).await,
        Request::Feedback(req) => handle_feedback(req, state, &mut t).await,
        Request::Supersede(req) => handle_supersede(req, state, &mut t).await,
        Request::Stale(req) => handle_stale(req, state, &mut t).await,
        Request::IndexFile(req) => {
            let max_file_bytes = indexd_cfg().service.max_file_bytes;
            if req
                .content
                .as_ref()
                .is_some_and(|content| content.len() > max_file_bytes)
            {
                json_error(&format!(
                    "caller-supplied content exceeds max {max_file_bytes} bytes"
                ))
            } else if req.background {
                // Opt-in fire-and-forget: enqueue the job and ack immediately
                // so a slow multi-section embed can't blow the caller's Bus
                // `send` timeout into a false rc=10. The worker drains the
                // queue and runs the *same* `handle_index_file` path. The
                // envelope carries origin context so the worker's CPU is
                // attributed to the real requester, not left orphaned.
                let filepath = req
                    .path
                    .replace("~", &std::env::var("HOME").unwrap_or_default());
                let file = std::path::Path::new(&filepath)
                    .file_name()
                    .and_then(|f| f.to_str())
                    .map(|s| s.to_string())
                    .unwrap_or(filepath);
                // Reserve owned bytes before transferring the request into the
                // bounded channel. Both count and byte overflow are explicit
                // queue_full responses; no work is silently discarded.
                let (job_tx, queue_budget) = {
                    let guard = lock_timed(state, &mut t).await;
                    (guard.job_tx.clone(), guard.queue_budget.clone())
                };
                let owned_bytes = index_job_owned_bytes(&req);
                let reservation = match queue_budget.try_reserve(owned_bytes) {
                    Ok(reservation) => reservation,
                    Err(()) => {
                        return finish_request(
                            seq,
                            action,
                            ctx,
                            bytes,
                            &t,
                            t0,
                            json_code_error(
                                "queue_full",
                                "background index queue count/byte budget exhausted",
                            ),
                        );
                    }
                };
                let job = IndexJob {
                    req,
                    seq,
                    peer: format!("{}:{}", ctx.transport, ctx.peer),
                    bytes,
                    enqueued: Instant::now(),
                    attempt: 1,
                    reservation,
                };
                match job_tx.try_send(job) {
                    Ok(()) => serde_json::json!({
                        "accepted": true,
                        "queued": true,
                        "file": file,
                    })
                    .to_string(),
                    Err(tokio::sync::mpsc::error::TrySendError::Full(_job)) => json_code_error(
                        "queue_full",
                        "background index queue count budget exhausted",
                    ),
                    Err(tokio::sync::mpsc::error::TrySendError::Closed(job)) => {
                        warn!("background job queue closed; indexing {file} synchronously");
                        handle_index_file(job.req, state, activity_tx, &mut t).await
                    }
                }
            } else {
                handle_index_file(req, state, activity_tx, &mut t).await
            }
        }
        Request::Stats => handle_stats(state, &mut t).await,
    };
    let response = if response.len() > indexd_cfg().service.max_response_bytes {
        json_code_error(
            "response_too_large",
            "serialised response exceeds configured byte limit",
        )
    } else {
        response
    };

    record_request(
        seq,
        action,
        ctx,
        bytes,
        &t,
        t0.elapsed(),
        classify_response(&response),
        &response,
    );
    response
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
    metrics::RSS_BEFORE_LOAD_BYTES.store(rss_bytes(), Ordering::Relaxed);
    let svc = &indexd_cfg().service;
    match EmbedModel::load(
        state.dtype,
        &state.model_id,
        svc.max_sequence_tokens,
        svc.max_forward_tokens,
    ) {
        Ok(model) => {
            state.model = Some(Arc::new(model));
            state.model_breaker.record_success();
            metrics::MODEL_GENERATION.fetch_add(1, Ordering::Relaxed);
            metrics::RSS_AFTER_LOAD_BYTES.store(rss_bytes(), Ordering::Relaxed);
            Ok(())
        }
        Err(e) => {
            state.model_breaker.record_failure();
            Err(e)
        }
    }
}

/// Compute embeddings for `texts` (with `prefix`) WITHOUT holding the
/// global `AppState` lock across the model forward pass. This is the core
/// concurrency fix: a slow embed must not block reads, search, or health.
///
/// Lock discipline (short critical sections, never across the embed):
///  1. lock → cache lookup and clone the inference gate, then drop lock.
///  2. acquire the inference permit OFF-lock, then re-lock/recheck the cache,
///     load/clone the model while the permit protects it from idle unload.
///  3. (no lock) run `model.embed` on a blocking thread under a wall-clock
///     timeout; on any error/timeout re-lock just to record the breaker
///     failure.
///  4. lock → record breaker success, store fresh embeddings into the cache,
///     fill the result vec, drop lock.
///
/// Returns the embeddings for `texts` in order. Errors are human-readable
/// strings the callers wrap into `json_error`.
async fn compute_embeddings(
    state: &Arc<Mutex<AppState>>,
    texts: &[String],
    prefix: &str,
    activity_tx: &tokio::sync::mpsc::Sender<()>,
    t: &mut ReqTiming,
) -> Result<Vec<Vec<f32>>, String> {
    if texts.is_empty() {
        return Ok(vec![]);
    }
    validate_embedding_budget(texts, prefix)?;

    // Cache fast path, then permit-before-model ordering. The second cache
    // lookup is required because another admitted request may populate the
    // misses while this request waits for the permit.
    let inference_gate = {
        let mut guard = lock_timed(state, t).await;
        let (cached, needs_embed) = guard.embed_cache.lookup_batch(texts, prefix);
        if needs_embed.is_empty() {
            t.cache_hit = true;
            return Ok(cached.into_iter().map(|o| o.unwrap()).collect());
        }
        guard.inference_gate.clone()
    };

    let permit = acquire_inference_permit(
        inference_gate,
        Duration::from_secs(indexd_cfg().service.inference_admission_timeout_secs.max(1)),
    )
    .await?;

    let (mut cached, needs_embed, model) = {
        let mut guard = lock_timed(state, t).await;
        let (cached, needs_embed) = guard.embed_cache.lookup_batch(texts, prefix);
        if needs_embed.is_empty() {
            t.cache_hit = true;
            return Ok(cached.into_iter().map(|o| o.unwrap()).collect());
        }

        if !guard.embed_breaker.allow_request() {
            return Err(format!(
                "embed circuit open (state {}, cooldown active)",
                guard.embed_breaker.state_name()
            ));
        }

        let was_cold = guard.model.is_none();
        let load_t0 = Instant::now();
        let load_res = ensure_model(&mut guard).await;
        if was_cold {
            // Commit BEFORE checking the result: a slow FAILED load is
            // exactly the burn this instrumentation must expose, not
            // unexplained elapsed time with cold_model=false.
            t.cold_model = true;
            t.model_load_us += load_t0.elapsed().as_micros() as u64;
        }
        if let Err(e) = load_res {
            return Err(format!("model load failed: {e}"));
        }
        let model = guard.model.clone().unwrap();
        (cached, needs_embed, model)
    };

    let texts_to_embed: Vec<String> = needs_embed.iter().map(|&i| texts[i].clone()).collect();

    // Reset the idle watchdog clock now that we're about to do real work.
    let _ = activity_tx.send(()).await;

    // --- Off-lock embed on a blocking thread, under a wall-clock timeout ---
    metrics::INFERENCE_IN_FLIGHT.fetch_add(1, Ordering::Relaxed);
    let lifecycle = Arc::new(AtomicU8::new(0));
    let worker_lifecycle = lifecycle.clone();
    let inference_lease = Arc::new(InferenceLease { _permit: permit });
    let worker = InferenceWorkerGuard {
        _lease: inference_lease.clone(),
        lifecycle: worker_lifecycle,
    };
    let model2 = model.clone();
    let to_embed = texts_to_embed.clone();
    let pfx = prefix.to_string();
    let embed_t0 = Instant::now();
    let res = tokio::time::timeout(
        Duration::from_secs(EMBED_TIMEOUT_SECS),
        tokio::task::spawn_blocking(move || {
            let _worker = worker;
            model2.embed(&to_embed, &pfx)
        }),
    )
    .await;
    t.embed_us += embed_t0.elapsed().as_micros() as u64;

    let new_embeddings: Vec<Vec<f32>> = match res {
        Ok(Ok(Ok(v))) => v,
        Ok(Ok(Err(e))) => {
            record_embed_failure(state, t).await;
            drop(model);
            drop(inference_lease);
            return Err(format!("embedding failed: {e}"));
        }
        Ok(Err(join)) => {
            record_embed_failure(state, t).await;
            drop(model);
            drop(inference_lease);
            return Err(format!("embedding worker panicked: {join}"));
        }
        Err(_elapsed) => {
            mark_inference_timed_out(&lifecycle);
            record_embed_failure(state, t).await;
            drop(model);
            drop(inference_lease);
            return Err(format!("embedding timed out after {EMBED_TIMEOUT_SECS}s"));
        }
    };

    // The model must return exactly one embedding per requested text. A short
    // result would otherwise panic the indexing below; treat it as a failure
    // (record + return) rather than recording success and indexing out of
    // bounds. This also keeps the empty-result case a clean error for callers.
    if new_embeddings.len() != needs_embed.len() {
        record_embed_failure(state, t).await;
        drop(model);
        drop(inference_lease);
        return Err(format!(
            "embedding failed: model returned {} embeddings, expected {}",
            new_embeddings.len(),
            needs_embed.len()
        ));
    }

    // --- Short lock #2: record success + cache store + fill results ---
    {
        let mut guard = lock_timed(state, t).await;
        guard.embed_breaker.record_success();
        for (embed_idx, &original_idx) in needs_embed.iter().enumerate() {
            let emb = new_embeddings[embed_idx].clone();
            guard
                .embed_cache
                .store(&texts[original_idx], prefix, emb.clone());
            cached[original_idx] = Some(emb);
        }
    }

    let embeddings = cached.into_iter().map(|o| o.unwrap()).collect();
    drop(model);
    drop(inference_lease);
    Ok(embeddings)
}

async fn acquire_inference_permit(
    gate: Arc<tokio::sync::Semaphore>,
    deadline: Duration,
) -> Result<tokio::sync::OwnedSemaphorePermit, String> {
    match tokio::time::timeout(deadline, gate.acquire_owned()).await {
        Ok(Ok(permit)) => Ok(permit),
        Ok(Err(_)) => Err("inference_busy: inference gate closed".to_string()),
        Err(_) => Err(format!(
            "inference_busy: inference permit unavailable after {}s",
            deadline.as_secs()
        )),
    }
}

/// Re-lock state solely to record an embed-breaker failure (error/timeout).
/// The re-lock wait is charged to the failing request's timing — failed
/// embeds must not under-report lock contention.
async fn record_embed_failure(state: &Arc<Mutex<AppState>>, t: &mut ReqTiming) {
    lock_timed(state, t).await.embed_breaker.record_failure();
}

async fn handle_embed(
    req: EmbedRequest,
    state: &Arc<Mutex<AppState>>,
    activity_tx: &tokio::sync::mpsc::Sender<()>,
    t: &mut ReqTiming,
) -> String {
    if req.texts.len() > MAX_TEXTS_PER_REQUEST {
        return json_error(&format!(
            "too many texts: {} (max {MAX_TEXTS_PER_REQUEST})",
            req.texts.len()
        ));
    }
    if let Err(e) = validate_embedding_budget(&req.texts, &req.prefix) {
        return json_error(&e);
    }
    t.work = req.texts.len() as u64;
    match compute_embeddings(state, &req.texts, &req.prefix, activity_tx, t).await {
        Ok(embeddings) => serde_json::to_string(&EmbedResponse { embeddings }).unwrap(),
        Err(e) => json_error(&e),
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
    if s.len() < 10 {
        return None;
    }
    let b = s.as_bytes();
    if b[4] != b'-' || b[7] != b'-' {
        return None;
    }
    let y: i64 = s[0..4].parse().ok()?;
    let m: i64 = s[5..7].parse().ok()?;
    let d: i64 = s[8..10].parse().ok()?;
    // Days from epoch: count years+leap days, month days, day of month.
    // Using the common algorithm (Howard Hinnant's date algorithms, shifted-year form).
    let y = y - (if m <= 2 { 1 } else { 0 });
    let era = if y >= 0 { y } else { y - 399 } / 400;
    let yoe = y - era * 400;
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
            } else {
                0.0
            }
        }
        Some(lr) => {
            if let Some(lr_days) = sqlite_datetime_to_days(lr) {
                let since = now_days - lr_days;
                if since > 180 { 0.02 } else { 0.0 }
            } else {
                0.0
            }
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
    let filename = meta
        .get("filename")
        .and_then(|v| v.as_str())
        .map(String::from);
    (path, filename)
}

/// Quick YYYY-MM-DD validator (no external date crate needed).
fn is_valid_ymd(s: &str) -> bool {
    if s.len() != 10 {
        return false;
    }
    let b = s.as_bytes();
    if b[4] != b'-' || b[7] != b'-' {
        return false;
    }
    let year: u32 = match s[0..4].parse() {
        Ok(n) => n,
        Err(_) => return false,
    };
    let month: u32 = match s[5..7].parse() {
        Ok(n) => n,
        Err(_) => return false,
    };
    let day: u32 = match s[8..10].parse() {
        Ok(n) => n,
        Err(_) => return false,
    };
    (1970..=9999).contains(&year) && (1..=12).contains(&month) && (1..=31).contains(&day)
}

/// Validate source type + required metadata fields. Enforces layer separation:
/// each source type has a distinct contract declared in the indexd config
/// TOML loaded at startup (path resolved per [`Cli::config`]). Adding a new
/// source type is a TOML edit plus `systemctl restart cosmix-indexd` — no
/// rebuild required.
///
/// Rules remain in Rust for mechanisms (date-format check, JSON parsing);
/// the TOML declares policy (which source types are accepted, which
/// fields each requires, which field is a date).
fn validate_store_entry(source: &str, metadata_json: &str) -> Result<(), String> {
    // Skip validation for empty metadata (legacy/test path) or unset source.
    if metadata_json.is_empty() || source.is_empty() {
        return Ok(());
    }
    let meta: serde_json::Value = serde_json::from_str(metadata_json)
        .map_err(|e| format!("metadata is not valid JSON: {e}"))?;
    let has = |f: &str| meta.get(f).is_some_and(|v| !v.is_null());

    let cfg = indexd_cfg();
    let spec = cfg.source_types.get(source).ok_or_else(|| {
        let cfg_path = INDEXD_CFG_PATH
            .get()
            .map(|p| p.display().to_string())
            .unwrap_or_else(|| "the loaded indexd config".to_string());
        format!("unknown source type: {source} (add to {cfg_path})")
    })?;

    for f in &spec.required {
        if !has(f) {
            return Err(format!("{source} metadata missing required field: {f}"));
        }
    }

    if let Some(date_field) = &spec.date_field {
        let date_str = meta.get(date_field).and_then(|v| v.as_str()).unwrap_or("");
        if !is_valid_ymd(date_str) {
            return Err(format!(
                "{source} {date_field} '{date_str}' not valid YYYY-MM-DD"
            ));
        }
    }

    Ok(())
}

async fn handle_store(
    req: StoreRequest,
    state: &Arc<Mutex<AppState>>,
    activity_tx: &tokio::sync::mpsc::Sender<()>,
    t: &mut ReqTiming,
) -> String {
    if req.texts.len() > MAX_TEXTS_PER_REQUEST {
        return json_error(&format!(
            "too many texts: {} (max {MAX_TEXTS_PER_REQUEST})",
            req.texts.len()
        ));
    }
    let prefix = "search_document: ";
    if let Err(e) = validate_embedding_budget(&req.texts, prefix) {
        return json_error(&e);
    }
    // Layer separation enforcement: validate each entry's metadata
    for meta_str in req.metadata.iter() {
        if let Err(e) = validate_store_entry(&req.source, meta_str) {
            return json_error(&format!("validation failed: {e}"));
        }
    }

    // Optimistic read-only duplicate probe: normal unchanged re-indexing does
    // no inference. The final IMMEDIATE transaction revalidates every skipped
    // item before any metadata/insert write. A vanished duplicate rolls the
    // whole attempt back; only those vanished items are embedded off-lock.
    let preflight_ids = {
        let guard = lock_timed(state, t).await;
        let db_t0 = Instant::now();
        let result = guard.db.preflight_duplicate_ids(&req.texts, &req.source);
        t.db_us += db_t0.elapsed().as_micros() as u64;
        match result {
            Ok(ids) => ids,
            Err(e) => return json_error(&format!("store preflight failed: {e}")),
        }
    };
    let mut embeddings: Vec<Option<Vec<f32>>> = vec![None; req.texts.len()];
    let apparent_misses: Vec<usize> = preflight_ids
        .iter()
        .enumerate()
        .filter_map(|(i, id)| id.is_none().then_some(i))
        .collect();
    if !apparent_misses.is_empty() {
        let texts: Vec<String> = apparent_misses
            .iter()
            .map(|&i| req.texts[i].clone())
            .collect();
        let computed = match compute_embeddings(state, &texts, prefix, activity_tx, t).await {
            Ok(embeddings) => embeddings,
            Err(e) => return json_error(&e),
        };
        for (embedding, &i) in computed.into_iter().zip(&apparent_misses) {
            embeddings[i] = Some(embedding);
        }
    }

    const MAX_VANISH_ATTEMPTS: usize = 3;
    for attempt in 1..=MAX_VANISH_ATTEMPTS {
        let mut guard = lock_timed(state, t).await;
        let db_t0 = Instant::now();
        let stored_res = guard.db.store_revalidated(
            &embeddings,
            &preflight_ids,
            &req.texts,
            &req.source,
            &req.metadata,
        );
        t.db_us += db_t0.elapsed().as_micros() as u64;
        match stored_res {
            Ok(StoreAttempt::Committed { ids, duplicates }) => {
                let stored = ids.len() - duplicates;
                // Nudge the corpus cache so world snapshots see new chunks
                // before the next 30s reconcile makes it authoritative.
                if stored > 0 {
                    guard.corpus_cache.total_vectors += stored as u64;
                    *guard
                        .corpus_cache
                        .by_source
                        .entry(req.source.clone())
                        .or_insert(0) += stored as u64;
                    guard.corpus_epoch += 1;
                }
                t.work = ids.len() as u64;
                return serde_json::to_string(&StoreResponse {
                    stored,
                    duplicates,
                    ids,
                })
                .unwrap();
            }
            Ok(StoreAttempt::NeedsEmbeddings(vanished)) => {
                drop(guard);
                if attempt == MAX_VANISH_ATTEMPTS {
                    return json_code_error(
                        "duplicate_vanished",
                        "store abandoned after repeated duplicate-delete races; transaction rolled back",
                    );
                }
                let missing: Vec<usize> = vanished
                    .into_iter()
                    .filter(|&i| embeddings[i].is_none())
                    .collect();
                if !missing.is_empty() {
                    let texts: Vec<String> =
                        missing.iter().map(|&i| req.texts[i].clone()).collect();
                    let computed =
                        match compute_embeddings(state, &texts, prefix, activity_tx, t).await {
                            Ok(embeddings) => embeddings,
                            Err(e) => return json_error(&e),
                        };
                    for (embedding, &i) in computed.into_iter().zip(&missing) {
                        embeddings[i] = Some(embedding);
                    }
                }
            }
            Err(e) => return json_error(&format!("store failed: {e}")),
        }
    }
    unreachable!("bounded store loop always returns or continues")
}

async fn handle_search(
    req: SearchRequest,
    state: &Arc<Mutex<AppState>>,
    activity_tx: &tokio::sync::mpsc::Sender<()>,
    t: &mut ReqTiming,
) -> String {
    if req.metadata_filter.len() > MAX_METADATA_FILTERS {
        return json_error(&format!(
            "too many metadata filters: {} (max {MAX_METADATA_FILTERS})",
            req.metadata_filter.len()
        ));
    }
    let limit = req.limit.min(MAX_SEARCH_LIMIT);
    let prefix = "search_query: ";

    // Off-lock embed (compute_embeddings handles the cache + model load).
    let query_emb = match compute_embeddings(
        state,
        std::slice::from_ref(&req.query),
        prefix,
        activity_tx,
        t,
    )
    .await
    {
        Ok(mut embs) if !embs.is_empty() => embs.remove(0),
        Ok(_) => return json_error("empty query"),
        Err(e) => return json_error(&e),
    };

    if sqlite_search_fallback() {
        // Rollback path: old sqlite-vec KNN under the global mutex.
        let guard = lock_timed(state, t).await;
        let db_t0 = Instant::now();
        let search_res = guard.db.search(
            &query_emb,
            limit,
            &req.source,
            &req.metadata_filter,
            indexd_cfg().service.max_response_bytes,
        );
        return match search_res {
            Ok(results) => {
                // Fire-and-forget: track that these chunks were retrieved
                // (implicit feedback signal).
                let ids: Vec<i64> = results.iter().map(|r| r.id).collect();
                guard.db.mark_retrieved(&ids);
                t.db_us += db_t0.elapsed().as_micros() as u64;
                t.work = results.len() as u64;
                serde_json::to_string(&SearchResponse { results }).unwrap()
            }
            Err(e) => {
                t.db_us += db_t0.elapsed().as_micros() as u64;
                json_error(&format!("search failed: {e}"))
            }
        };
    }

    // In-memory exact path: the global mutex is held only long enough
    // to clone the handle, then the scan runs in spawn_blocking against
    // the index + a read-only connection. Admission-gated: the gate
    // wait is charged to lock_wait_us.
    let gate_t0 = Instant::now();
    let _permit = SEARCH_GATE
        .acquire()
        .await
        .expect("search gate semaphore closed");
    t.lock_wait_us += gate_t0.elapsed().as_micros() as u64;
    let (db_path, index) = {
        let guard = lock_timed(state, t).await;
        (guard.db_path.clone(), guard.db.search_index.clone())
    };
    let source = req.source;
    let metadata_filter = req.metadata_filter;
    let joined = tokio::task::spawn_blocking(move || {
        search_exact(
            &db_path,
            &index,
            &query_emb,
            limit,
            &source,
            &metadata_filter,
            indexd_cfg().service.max_response_bytes,
        )
    })
    .await;
    match joined {
        Ok(Ok((results, phases))) => {
            t.db_us += phases.candidate_db_us + phases.fetch_db_us;
            t.vector_us += phases.vector_us;
            // Retrieval bookkeeping is the only remaining global-mutex
            // work — a small indexed UPDATE.
            let ids: Vec<i64> = results.iter().map(|r| r.id).collect();
            {
                let guard = lock_timed(state, t).await;
                let db_t0 = Instant::now();
                guard.db.mark_retrieved(&ids);
                t.db_us += db_t0.elapsed().as_micros() as u64;
            }
            t.work = results.len() as u64;
            serde_json::to_string(&SearchResponse { results }).unwrap()
        }
        Ok(Err(e)) => json_error(&format!("search failed: {e}")),
        Err(join) => json_error(&format!("search worker failed: {join}")),
    }
}

async fn handle_update(
    req: UpdateRequest,
    state: &Arc<Mutex<AppState>>,
    activity_tx: &tokio::sync::mpsc::Sender<()>,
    t: &mut ReqTiming,
) -> String {
    let prefix = "search_document: ";

    // Re-embed off-lock if content changed (compute_embeddings caches).
    // An empty result for a requested content embed is an error, not a silent
    // skip — persisting new content with a stale vector would corrupt search.
    let new_embedding = if let Some(ref content) = req.content {
        match compute_embeddings(state, std::slice::from_ref(content), prefix, activity_tx, t).await
        {
            Ok(mut embs) if !embs.is_empty() => Some(embs.remove(0)),
            Ok(_) => return json_error("embedding failed: empty result for content"),
            Err(e) => return json_error(&e),
        }
    } else {
        None
    };

    let guard = lock_timed(state, t).await;
    let db_t0 = Instant::now();
    let update_res = guard.db.update(
        req.id,
        req.content.as_deref(),
        req.metadata.as_deref(),
        req.source.as_deref(),
        new_embedding.as_deref(),
    );
    t.db_us += db_t0.elapsed().as_micros() as u64;
    match update_res {
        Ok(updated) => {
            t.work = updated as u64;
            serde_json::to_string(&UpdateResponse {
                updated,
                re_embedded: new_embedding.is_some(),
            })
            .unwrap()
        }
        Err(e) => json_error(&format!("update failed: {e}")),
    }
}

async fn handle_delete(
    req: DeleteRequest,
    state: &Arc<Mutex<AppState>>,
    t: &mut ReqTiming,
) -> String {
    let mut guard = lock_timed(state, t).await;
    let db_t0 = Instant::now();
    let res = guard.db.delete(&req.ids);
    t.db_us += db_t0.elapsed().as_micros() as u64;
    match res {
        Ok(deleted) => {
            // Total nudged down; the per-source split of deleted rows is
            // unknown here — the 30s reconcile corrects that drift. A
            // zero-row delete changes nothing, so it must not bump the
            // epoch (each bump discards an in-flight reconcile).
            if deleted > 0 {
                guard.corpus_cache.total_vectors = guard
                    .corpus_cache
                    .total_vectors
                    .saturating_sub(deleted as u64);
                guard.corpus_epoch += 1;
            }
            t.work = deleted as u64;
            serde_json::to_string(&DeleteResponse { deleted }).unwrap()
        }
        Err(e) => json_error(&format!("delete failed: {e}")),
    }
}

async fn handle_list(req: ListRequest, state: &Arc<Mutex<AppState>>, t: &mut ReqTiming) -> String {
    let limit = match sqlite_count(clamp_list_limit(req.limit), "list.limit") {
        Ok(value) => value,
        Err(e) => return json_error(&e),
    };
    let offset = match sqlite_count(req.offset, "list.offset") {
        Ok(value) => value,
        Err(e) => return json_error(&e),
    };
    let guard = lock_timed(state, t).await;
    let db_t0 = Instant::now();
    let res = guard.db.list(
        &req.source,
        limit,
        offset,
        indexd_cfg().service.max_response_bytes,
    );
    t.db_us += db_t0.elapsed().as_micros() as u64;
    match res {
        Ok((items, total)) => {
            t.work = items.len() as u64;
            serde_json::to_string(&ListResponse { items, total }).unwrap()
        }
        Err(e) => json_error(&format!("list failed: {e}")),
    }
}

async fn handle_feedback(
    req: FeedbackRequest,
    state: &Arc<Mutex<AppState>>,
    t: &mut ReqTiming,
) -> String {
    let guard = lock_timed(state, t).await;
    let db_t0 = Instant::now();
    let res = guard.db.feedback(req.id, req.useful);
    t.db_us += db_t0.elapsed().as_micros() as u64;
    match res {
        Ok(new_score) => {
            t.work = 1;
            serde_json::json!({"ok": true, "id": req.id, "feedback_score": new_score}).to_string()
        }
        Err(e) => json_error(&format!("feedback failed: {e}")),
    }
}

async fn handle_supersede(
    req: SupersedeRequest,
    state: &Arc<Mutex<AppState>>,
    t: &mut ReqTiming,
) -> String {
    if !req.reason.is_empty() {
        info!(
            old_id = req.old_id,
            new_id = req.new_id,
            reason = %req.reason,
            "chunk supersede"
        );
    } else {
        info!(old_id = req.old_id, new_id = req.new_id, "chunk supersede");
    }
    let guard = lock_timed(state, t).await;
    let db_t0 = Instant::now();
    let res = guard.db.supersede(req.old_id, req.new_id);
    t.db_us += db_t0.elapsed().as_micros() as u64;
    match res {
        Ok(updated) => {
            t.work = updated as u64;
            serde_json::json!({
                "ok": true,
                "old_id": req.old_id,
                "new_id": req.new_id,
                "updated": updated,
            })
            .to_string()
        }
        Err(e) => json_error(&format!("supersede failed: {e}")),
    }
}

async fn handle_stale(
    req: StaleRequest,
    state: &Arc<Mutex<AppState>>,
    t: &mut ReqTiming,
) -> String {
    let guard = lock_timed(state, t).await;
    let db_t0 = Instant::now();
    let res = guard.db.stale_query(&req);
    t.db_us += db_t0.elapsed().as_micros() as u64;
    match res {
        Ok(resp) => {
            t.work = (resp.never_retrieved_old.len()
                + resp.low_value.len()
                + resp.long_dormant.len()) as u64;
            serde_json::to_string(&resp).unwrap()
        }
        Err(e) => json_error(&format!("stale query failed: {e}")),
    }
}

/// Character budget per chunk before a `##` section is sub-split. The embed
/// model (nomic-embed-text-v1.5) is only *trained* to 2048 tokens — beyond that
/// it runs (up to the 8192 rope ceiling) but on untrained rotary positions, so
/// quality degrades. ~3.5 chars/token for dense markdown puts ~6000 chars at
/// ~1700 tokens, comfortably inside the trained range with headroom for the
/// "search_document: " prefix. Sub-splitting here keeps every chunk well-
/// embedded *and* below the rope ceiling, so no section is silently dropped or
/// truncated.
const MAX_CHUNK_CHARS: usize = 6000;

struct ChunkTokenizer {
    tokenizer: Tokenizer,
    max_seq_len: usize,
}

impl ChunkTokenizer {
    fn load(model_id: &str, configured_max_seq_len: usize) -> Result<Self> {
        // Load only config.json + tokenizer.json. This path must never touch
        // model.safetensors: token-aware file chunking is needed before the
        // first embed and must not force a full model allocation.
        let repo_spec = Repo::new(model_id.to_string(), RepoType::Model);
        let cache = Cache::default().repo(repo_spec.clone());
        let cfg = cache.get("config.json");
        let tok = cache.get("tokenizer.json");
        let (config_path, tokenizer_path) = match (cfg, tok) {
            (Some(config), Some(tokenizer)) => (config, tokenizer),
            _ => {
                let api = Api::new()?;
                let repo = api.repo(repo_spec);
                (
                    repo.get("config.json")
                        .context("downloading config.json for chunking")?,
                    repo.get("tokenizer.json")
                        .context("downloading tokenizer.json for chunking")?,
                )
            }
        };
        let config: nomic_bert::Config = serde_json::from_str(
            &std::fs::read_to_string(&config_path).context("reading chunker config.json")?,
        )?;
        let mut tokenizer =
            Tokenizer::from_file(tokenizer_path).map_err(|e| anyhow::anyhow!("{e}"))?;
        tokenizer
            .with_truncation(None)
            .map_err(|e| anyhow::anyhow!("disabling chunker truncation: {e}"))?;
        Ok(Self {
            tokenizer,
            max_seq_len: effective_sequence_limit(
                configured_max_seq_len,
                indexd_cfg().service.max_forward_tokens,
                config.n_positions,
            )?,
        })
    }

    fn count_document_tokens(&self, text: &str) -> Result<usize, String> {
        self.tokenizer
            .encode(format!("search_document: {text}"), true)
            .map(|encoding| encoding.get_ids().len())
            .map_err(|e| format!("tokenizing index_file chunk: {e}"))
    }
}

static CHUNK_TOKENIZER: OnceLock<std::sync::Mutex<Option<Arc<ChunkTokenizer>>>> = OnceLock::new();

async fn chunk_tokenizer() -> Result<Arc<ChunkTokenizer>, String> {
    let model_id = indexd_cfg().service.model_id.clone();
    let max_seq_len = indexd_cfg().service.max_sequence_tokens;
    tokio::task::spawn_blocking(move || {
        let slot = CHUNK_TOKENIZER.get_or_init(|| std::sync::Mutex::new(None));
        let mut guard = slot
            .lock()
            .map_err(|_| "chunk tokenizer cache lock poisoned".to_string())?;
        if let Some(tokenizer) = guard.as_ref() {
            return Ok(tokenizer.clone());
        }
        let tokenizer = Arc::new(
            ChunkTokenizer::load(&model_id, max_seq_len)
                .map_err(|e| format!("loading tokenizer for index_file chunking: {e:#}"))?,
        );
        *guard = Some(tokenizer.clone());
        Ok(tokenizer)
    })
    .await
    .map_err(|e| format!("chunk tokenizer worker failed: {e}"))?
}

/// Split one char-bounded piece further until the tokenizer (including the
/// real document prefix and special tokens) reports every output at or below
/// the effective model sequence limit. The initial count is the fast path;
/// binary search only runs for token-dense content.
fn split_token_aware<F>(
    text: &str,
    max_tokens: usize,
    token_count: F,
) -> Result<Vec<String>, String>
where
    F: Fn(&str) -> Result<usize, String>,
{
    if token_count(text)? <= max_tokens {
        return Ok(vec![text.to_string()]);
    }

    let mut boundaries: Vec<usize> = text.char_indices().map(|(offset, _)| offset).collect();
    boundaries.push(text.len());
    let char_count = boundaries.len().saturating_sub(1);
    let mut start = 0usize;
    let mut pieces = Vec::new();
    while start < char_count {
        let mut low = start + 1;
        let mut high = char_count;
        let mut best = None;
        while low <= high {
            let mid = low + (high - low) / 2;
            let candidate = &text[boundaries[start]..boundaries[mid]];
            if token_count(candidate)? <= max_tokens {
                best = Some(mid);
                low = mid + 1;
            } else {
                high = mid.saturating_sub(1);
            }
        }
        let end = best.ok_or_else(|| {
            format!(
                "one input character plus embedding prefix exceeds the {max_tokens}-token limit"
            )
        })?;
        let piece = text[boundaries[start]..boundaries[end]].to_string();
        if token_count(&piece)? > max_tokens {
            return Err("token-aware chunker produced an oversized piece".to_string());
        }
        pieces.push(piece);
        start = end;
    }
    Ok(pieces)
}

/// Split a section's text into chunks no larger than `max_chars`, breaking at
/// line boundaries so words and markdown stay intact. A single line that is
/// itself longer than `max_chars` (a long URL, base64 blob, minified JSON, a
/// wide table row) is hard-split on char (UTF-8) boundaries so its tail is
/// never silently lost. Returns one element (the original text) when it fits.
///
/// Guarantee: every content character of the input appears in some output
/// chunk, in order — no content is dropped or truncated. The chunk *boundaries*
/// normalize whitespace (the single `\n` separating a buffered line-group from
/// the next chunk, and the newlines around a hard-split overlong line, are not
/// re-emitted), so the pieces are independent search units, not a byte-exact
/// re-segmentation of the source.
fn split_oversized(text: &str, max_chars: usize) -> Vec<String> {
    if text.chars().count() <= max_chars {
        return vec![text.to_string()];
    }
    let mut out = Vec::new();
    let mut cur = String::new();
    let mut cur_len = 0usize; // running char count of `cur`, kept to avoid O(n^2)
    for line in text.lines() {
        let line_len = line.chars().count();

        // An overlong single line can't fit any line-boundary chunk: flush the
        // buffer, then hard-split the line on char boundaries (UTF-8 safe — we
        // iterate `chars()`, never byte offsets, so no codepoint is ever split).
        if line_len > max_chars {
            if !cur.is_empty() {
                out.push(std::mem::take(&mut cur));
                cur_len = 0;
            }
            let mut piece = String::new();
            let mut plen = 0usize;
            for ch in line.chars() {
                if plen == max_chars {
                    out.push(std::mem::take(&mut piece));
                    plen = 0;
                }
                piece.push(ch);
                plen += 1;
            }
            if !piece.is_empty() {
                out.push(piece);
            }
            continue;
        }

        if cur_len > 0 && cur_len + line_len + 1 > max_chars {
            out.push(std::mem::take(&mut cur));
            cur_len = 0;
        }
        if cur_len > 0 {
            cur.push('\n');
            cur_len += 1;
        }
        cur.push_str(line);
        cur_len += line_len;
    }
    if !cur.is_empty() {
        out.push(cur);
    }
    out
}

/// True when `s` is shaped like a `YYYY-MM-DD` date prefix: ten chars, a digit
/// year, and hyphens at positions 4 and 7. Month/day *range* validity is NOT
/// checked (that is [`is_valid_ymd`]'s job). This distinguishes "the author
/// intended a dated entry here" — keep it a journal so a malformed date
/// (`2026-13-01-x.md`) surfaces as a validation error — from "no date at all"
/// (`README.md`, `notes.md`) which is downgraded to a plain doc.
fn is_date_shaped(s: &str) -> bool {
    let b = s.as_bytes();
    s.len() == 10 && b[4] == b'-' && b[7] == b'-' && b[..4].iter().all(u8::is_ascii_digit)
}

fn read_open_file_bounded(mut file: std::fs::File, max_bytes: usize) -> Result<String, String> {
    let metadata = file.metadata().map_err(|e| format!("fstat failed: {e}"))?;
    if !metadata.is_file() {
        return Err("path is not a regular file".to_string());
    }
    if metadata.len() > max_bytes as u64 {
        return Err(format!(
            "file is {} bytes (max {max_bytes})",
            metadata.len()
        ));
    }

    // The handle may grow after fstat. Read one byte beyond the cap from this
    // same handle and reject on observation, so neither growth nor a path swap
    // can bypass the bound.
    let read_limit = max_bytes.saturating_add(1) as u64;
    let mut bytes = Vec::with_capacity(max_bytes.saturating_add(1));
    file.by_ref()
        .take(read_limit)
        .read_to_end(&mut bytes)
        .map_err(|e| format!("read failed: {e}"))?;
    if bytes.len() > max_bytes {
        return Err(format!(
            "file exceeded max {max_bytes} bytes while being read"
        ));
    }
    String::from_utf8(bytes).map_err(|e| format!("read failed: file is not UTF-8: {e}"))
}

fn read_file_bounded(path: &Path, max_bytes: usize) -> Result<String, String> {
    let file = std::fs::OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NONBLOCK | libc::O_NOFOLLOW)
        .open(path)
        .map_err(|e| format!("open failed: {e}"))?;
    read_open_file_bounded(file, max_bytes)
}

/// Index a single markdown file: read, split on ## headings, delete old entries, store new ones.
/// Called from git post-commit hooks via Bus: `indexd.index_file {"path": "/path/to/file.md"}`
async fn handle_index_file(
    req: IndexFileRequest,
    state: &Arc<Mutex<AppState>>,
    activity_tx: &tokio::sync::mpsc::Sender<()>,
    t: &mut ReqTiming,
) -> String {
    let t_start = std::time::Instant::now();
    let filepath = req
        .path
        .replace("~", &std::env::var("HOME").unwrap_or_default());
    let path = std::path::Path::new(&filepath);

    let filename = path.file_name().and_then(|f| f.to_str()).unwrap_or("?");

    // Auto-detect source from path
    let source = if !req.source.is_empty() {
        req.source.clone()
    } else if filepath.contains("/_doc/") {
        "doc".to_string()
    } else if filepath.contains("/_journal/") {
        "journal".to_string()
    } else {
        "doc".to_string()
    };

    // Auto-detect domain from path via settings [domains.map], fall back to "general"
    let domain = if !req.domain.is_empty() {
        req.domain.clone()
    } else {
        let domains =
            cosmix_config::store::load_service::<cosmix_config::DomainsSettings>("domains")
                .unwrap_or_default();
        domains
            .resolve(std::path::Path::new(&filepath))
            .unwrap_or_else(|| "general".to_string())
    };

    // Obtain content: caller-supplied bytes take precedence (the only way
    // to index files the sandboxed daemon cannot read — see the `content`
    // field doc); otherwise read `path` from disk.
    let max_file_bytes = indexd_cfg().service.max_file_bytes;
    let content = match req.content {
        Some(c) => {
            if c.len() > max_file_bytes {
                return json_error(&format!(
                    "caller-supplied content is {} bytes (max {max_file_bytes})",
                    c.len()
                ));
            }
            c
        }
        None => match read_file_bounded(path, max_file_bytes) {
            Ok(c) => c,
            Err(e) => return json_error(&e),
        },
    };

    // Split on ## headings
    let mut sections: Vec<(String, String)> = Vec::new();
    let mut current_title = filename.strip_suffix(".md").unwrap_or(filename).to_string();
    let mut current_lines: Vec<&str> = Vec::new();

    for line in content.lines() {
        if let Some(rest) = line.strip_prefix("## ") {
            if !current_lines.is_empty() {
                let text = current_lines.join("\n");
                let text = text.trim();
                if text.len() > 50 {
                    sections.push((current_title.clone(), text.to_string()));
                }
            }
            current_title = rest.trim().to_string();
            current_lines = vec![line];
        } else {
            current_lines.push(line);
        }
    }
    // Last section
    if !current_lines.is_empty() {
        let text = current_lines.join("\n");
        let text = text.trim();
        if text.len() > 50 {
            sections.push((current_title, text.to_string()));
        }
    }

    // R3: delete-AFTER-successful-store, not before. Capture this path's
    // existing chunk ids now, but do NOT delete yet — if a later section's
    // embed/store fails, the previous good copy must survive (a delete-first
    // approach loses the doc until a successful re-index). metadata holds a
    // JSON object with a "path" field.
    let old_ids: Vec<i64> = {
        let guard = lock_timed(state, t).await;
        let db_t0 = Instant::now();
        let ids = guard
            .db
            .conn
            .prepare("SELECT id FROM chunks WHERE json_extract(metadata, '$.path') = ?1")
            .and_then(|mut stmt| {
                stmt.query_map([&filepath], |row| row.get(0))
                    .map(|rows| rows.filter_map(|r| r.ok()).collect())
            })
            .unwrap_or_default();
        t.db_us += db_t0.elapsed().as_micros() as u64;
        ids
    };

    if sections.is_empty() {
        // File emptied / shrunk below the section threshold ⇒ purge all of its
        // stale chunks (there is nothing to store, so delete-after collapses to
        // delete-all).
        if !old_ids.is_empty() {
            let mut guard = lock_timed(state, t).await;
            let db_t0 = Instant::now();
            match guard.db.delete(&old_ids) {
                Ok(deleted) if deleted > 0 => {
                    guard.corpus_cache.total_vectors = guard
                        .corpus_cache
                        .total_vectors
                        .saturating_sub(deleted as u64);
                    guard.corpus_epoch += 1;
                }
                Ok(_) => {}
                Err(e) => warn!(file = %filename, "stale-chunk purge failed: {e}"),
            }
            t.db_us += db_t0.elapsed().as_micros() as u64;
        }
        info!(file = %filename, path = %filepath, "indexed 0 sections (empty/below-threshold)");
        return serde_json::json!({"indexed": true, "sections": 0, "file": filename}).to_string();
    }

    // Keep the 6000-char heuristic as the fast first pass, then count with a
    // tokenizer-only helper and split token-dense pieces further. The helper
    // loads no model weights, so chunking cannot allocate the model merely to
    // discover boundaries. Split pieces get a "(part i/n)" title suffix.
    let tokenizer = match chunk_tokenizer().await {
        Ok(tokenizer) => tokenizer,
        Err(e) => return json_error(&e),
    };
    let mut token_bounded_sections = Vec::new();
    for (title, text) in sections {
        let mut pieces = Vec::new();
        for char_piece in split_oversized(&text, MAX_CHUNK_CHARS) {
            match split_token_aware(&char_piece, tokenizer.max_seq_len, |piece| {
                tokenizer.count_document_tokens(piece)
            }) {
                Ok(mut bounded) => pieces.append(&mut bounded),
                Err(e) => return json_error(&e),
            }
        }
        let n = pieces.len();
        if n == 1 {
            token_bounded_sections.push((title, pieces.remove(0)));
        } else {
            token_bounded_sections.extend(
                pieces
                    .into_iter()
                    .enumerate()
                    .map(|(i, piece)| (format!("{title} (part {}/{n})", i + 1), piece)),
            );
        }
    }
    let sections = token_bounded_sections;

    // Bound the per-request embedding workload on the FINAL chunk list
    // (post sub-split): each chunk is embedded sequentially via
    // handle_store with a 1-element batch, so an oversized document would
    // otherwise monopolise the embedding worker and bypass
    // MAX_TEXTS_PER_REQUEST. Checked here (not before the sub-split) so a
    // few huge `##` sections that explode into many chunks are also caught.
    if sections.len() > MAX_INDEX_FILE_SECTIONS {
        return json_error(&format!(
            "file splits into too many chunks: {} (max {MAX_INDEX_FILE_SECTIONS})",
            sections.len()
        ));
    }

    // Extract date from filename (YYYY-MM-DD-title.md convention). A file under
    // _journal/ whose name is NOT shaped like a dated entry (e.g. a README or
    // notes file inside a journal artifacts dir) is documentation, not a dated
    // journal entry — when the source was auto-detected, index it as a doc
    // instead of rejecting it for a missing date. A name that IS date-shaped but
    // invalid (a typo like 2026-13-01-x.md) is deliberately left as a journal so
    // validate_store_entry surfaces the error rather than silently masking it.
    // An explicit caller-supplied source is also left intact (its contract is
    // honored, error and all).
    let date = filename.get(..10).unwrap_or("");
    let (source, date) = if req.source.is_empty() && source == "journal" && !is_date_shaped(date) {
        ("doc".to_string(), "")
    } else {
        (source, date)
    };

    // Store each section sequentially (per-section embed via handle_store —
    // NOT a single batched model.embed: the padding approach regressed large
    // docs and was reverted in 0.3.1). A store failure (e.g. metadata
    // validation) is a real error, not a silent skip: surface the first one
    // with rc>=10 so the caller learns indexing did not fully succeed, rather
    // than a misleading {"indexed":true}.
    //
    // R3: collect the ids each section's StoreResponse returns into
    // `kept_ids` — `ids` carries both newly-inserted AND dedup-matched-existing
    // ids, so an unchanged section dedups to an existing id that lands in
    // kept_ids and is therefore preserved by the delete-after step below. On a
    // structural failure we return WITHOUT deleting anything, leaving the
    // previous good copy intact.
    let total = sections.len();
    let mut kept_ids: Vec<i64> = Vec::new();
    for (idx, (title, text)) in sections.iter().enumerate() {
        let meta = serde_json::json!({
            "path": filepath,
            "filename": filename,
            "section": title,
            "domain": domain,
            "type": source,
            "date": date,
        });

        let store_req = StoreRequest {
            texts: vec![text.clone()],
            source: source.clone(),
            metadata: vec![meta.to_string()],
        };
        let resp = handle_store(store_req, state, activity_tx, t).await;
        // Detect a store failure structurally (top-level `error` field),
        // not by substring: a successful StoreResponse never carries one,
        // so this can't false-positive on data that merely contains the
        // token "error".
        let parsed = serde_json::from_str::<serde_json::Value>(&resp).ok();
        let store_failed = parsed.as_ref().is_some_and(|v| v.get("error").is_some());
        if store_failed {
            // Failure ⇒ old chunks survive (old_ids were never deleted), so a
            // re-run after the cause is fixed reconciles cleanly. The caller
            // (record_request) emits the structured warn carrying this
            // detail — no separate warn here, it would double-report.
            // `work` = sections actually completed before the failure, not
            // whatever the last section's handle_store happened to leave.
            t.work = idx as u64;
            let detail = format!(
                "index_file {filename}: stored {idx}/{total} sections, then section \
                 \"{title}\" failed: {resp}"
            );
            return json_error(&detail);
        }
        // Accumulate the ids this section stored or dedup-matched.
        if let Some(ids) = parsed
            .as_ref()
            .and_then(|v| v.get("ids"))
            .and_then(|v| v.as_array())
        {
            kept_ids.extend(ids.iter().filter_map(|i| i.as_i64()));
        }
    }

    // R3: now that every section stored successfully, delete the old chunks
    // that the new content did NOT keep (sections that were removed or whose
    // text changed). Unchanged sections dedup to an existing id which is in
    // kept_ids, so they are preserved; changed/removed sections leave a stale
    // id absent from kept_ids and are purged. During the re-index window old+new
    // chunks coexist and MAY both surface in results; that transient
    // duplication is accepted as strictly safer than delete-first.
    let to_delete: Vec<i64> = old_ids
        .into_iter()
        .filter(|id| !kept_ids.contains(id))
        .collect();
    if !to_delete.is_empty() {
        let mut guard = lock_timed(state, t).await;
        let db_t0 = Instant::now();
        match guard.db.delete(&to_delete) {
            Ok(deleted) if deleted > 0 => {
                guard.corpus_cache.total_vectors = guard
                    .corpus_cache
                    .total_vectors
                    .saturating_sub(deleted as u64);
                guard.corpus_epoch += 1;
            }
            Ok(_) => {}
            // Old chunks survive; the next successful re-index purges
            // them. Not silent any more — a stuck purge is visible.
            Err(e) => warn!(file = %filename, "old-chunk purge failed after re-index: {e}"),
        }
        t.db_us += db_t0.elapsed().as_micros() as u64;
    }

    // Per-section handle_store calls set `work` to their own counts;
    // for the whole file the meaningful dimension is sections indexed.
    t.work = total as u64;

    info!(
        file = %filename,
        path = %filepath,
        sections = total,
        elapsed_ms = t_start.elapsed().as_millis() as u64,
        "indexed"
    );
    serde_json::json!({
        "indexed": true,
        "file": filename,
        "sections": total,
        "domain": domain,
    })
    .to_string()
}

async fn handle_stats(state: &Arc<Mutex<AppState>>, t: &mut ReqTiming) -> String {
    let guard = lock_timed(state, t).await;
    let db_t0 = Instant::now();
    let res = guard.db.stats(&guard.db_path);
    t.db_us += db_t0.elapsed().as_micros() as u64;
    match res {
        Ok(mut stats) => {
            stats.model_loaded = guard.model.is_some();
            stats.model_circuit = guard.model_breaker.state_name().to_string();
            stats.embed_circuit = guard.embed_breaker.state_name().to_string();
            stats.embed_cache_entries = guard.embed_cache.entries.len();
            stats.embed_cache_hits = guard.embed_cache.hits;
            stats.embed_cache_misses = guard.embed_cache.misses;
            let runtime = metrics::runtime_snapshot();
            stats.queue_depth = runtime.queue_depth;
            stats.queued_bytes = runtime.queued_bytes;
            stats.inference_in_flight = runtime.inference_in_flight;
            stats.timed_out_still_running = runtime.timed_out_still_running;
            stats.model_generation = runtime.model_generation;
            stats.rss_before_load_bytes = runtime.rss_before_load_bytes;
            stats.rss_after_load_bytes = runtime.rss_after_load_bytes;
            stats.rss_after_unload_bytes = runtime.rss_after_unload_bytes;
            // Snapshot + serialisation happen OFF the global lock — the
            // counters have their own synchronisation.
            drop(guard);
            // Attach the per-action counters so external samplers can
            // attribute CPU to workload without scraping the journal.
            let mut v = serde_json::to_value(&stats).unwrap();
            if let Some(obj) = v.as_object_mut() {
                obj.insert("request_metrics".into(), METRICS.snapshot());
            }
            v.to_string()
        }
        Err(e) => json_error(&format!("stats failed: {e}")),
    }
}

fn json_error(msg: &str) -> String {
    serde_json::to_string(&ErrorResponse {
        error: msg.to_string(),
        code: None,
    })
    .unwrap()
}

fn json_code_error(code: &str, msg: &str) -> String {
    serde_json::to_string(&ErrorResponse {
        error: msg.to_string(),
        code: Some(code.to_string()),
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

/// Collect a fresh `IndexdPropsSnapshot` from live `AppState`. Holds the
/// state mutex briefly to read config + circuit + db handle, then runs
/// the corpus stats query (which can block on sqlite I/O) under the same
/// lock. For L1 the load is a single `props.get` per call — no concern;
/// L2/L3 (event-driven snapshots) would want to drop the lock around
/// stats() and accept slightly stale reads.
/// `bucket` attributes the cost to its real caller: the 1 Hz world
/// publisher charges `internal.world_snapshot` (its debounce suppresses
/// PUBLISHING, not this query — the COUNT + GROUP BY runs under the
/// global mutex every tick), while external flat props reads charge
/// `props.get`. Without this accounting, request logs can show zero
/// workload while indexd burns CPU here — exactly the
/// unattributed-burn failure mode this arc fixes.
async fn collect_props(
    state: &Arc<Mutex<AppState>>,
    bucket: &'static str,
) -> props::IndexdPropsSnapshot {
    let mut t = ReqTiming::default();
    let snap_t0 = Instant::now();
    let guard = lock_timed(state, &mut t).await;
    let circuit = guard.model_breaker.state_name().to_string();
    let embed_circuit = guard.embed_breaker.state_name().to_string();
    // Corpus numbers come from the CACHE (reconciled every 30s off the
    // mutex, nudged by mutations) — never from a query here. The 1 Hz
    // world tick calling db.stats() under this mutex was the measured
    // ~12%-of-a-core standing burn; ≤30s staleness on corpus counters
    // is inside the tolerance the SPEC-07 world surface already
    // documents ("accept slightly stale reads").
    let (chunks, bytes_db, kinds) = (
        guard.corpus_cache.total_vectors,
        guard.corpus_cache.db_size_bytes,
        guard.corpus_cache.by_source.clone(),
    );
    let snapshot = props::IndexdPropsSnapshot {
        socket_path: guard.socket_path.clone(),
        model_id: guard.model_id.clone(),
        dtype: match guard.dtype {
            DType::F16 => "f16".to_string(),
            DType::F32 => "f32".to_string(),
            other => format!("{other:?}"),
        },
        idle_timeout_secs: guard.idle_timeout_secs,
        embed_dim: EMBEDDING_DIM as u64,
        started_at: guard.started_at_iso.clone(),
        uptime_s: guard.started.elapsed().as_secs(),
        model_loaded: guard.model.is_some(),
        model_circuit: circuit,
        embed_circuit,
        corpus_chunks: chunks,
        corpus_bytes_db: bytes_db,
        corpus_kinds: kinds,
    };
    // Always Ok now: reads are cache lookups; reconcile failures are
    // counted by the reconciler's own logging, not here.
    METRICS
        .for_action(bucket)
        .record(&t, snap_t0.elapsed().as_micros() as u64, 0, Outcome::Ok);
    snapshot
}

#[cfg(test)]
mod config_tests {
    use super::*;

    fn temp_dir(tag: &str) -> PathBuf {
        let d =
            std::env::temp_dir().join(format!("cosmix-indexd-cfg-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    #[test]
    fn system_none_when_dir_empty() {
        let dir = temp_dir("empty");
        assert!(load_system_config(&dir).unwrap().is_none());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn system_reads_conf_mix() {
        let dir = temp_dir("prefer");
        std::fs::write(
            dir.join("config.conf.mix"),
            "schema_version: 7\nservice: { socket_path: \"/run/native.sock\" }\n",
        )
        .unwrap();
        let (path, cfg) = load_system_config(&dir).unwrap().expect("some");
        assert_eq!(path, dir.join("config.conf.mix"));
        assert_eq!(cfg.schema_version, 7);
        assert_eq!(cfg.service.socket_path, "/run/native.sock");
        assert_eq!(cfg.service.max_sequence_tokens, 2048);
        assert_eq!(cfg.service.max_forward_tokens, 4096);
        assert_eq!(cfg.service.vindex_dtype, "f32");
        assert_eq!(cfg.service.max_ingress_bytes, 64 * 1024 * 1024);
        assert_eq!(cfg.service.request_frame_timeout_secs, 600);
        assert_eq!(cfg.service.inference_admission_timeout_secs, 30);
        assert_eq!(cfg.service.max_prefix_bytes, 4096);
        assert_eq!(cfg.service.max_response_bytes, 20 * 1024 * 1024);
        assert_eq!(cfg.service.background_retry_max_attempts, 5);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn configured_memory_bounds_override_individually() {
        let cfg: Config = cosmix_config::from_conf_mix_str(
            "service: { max_sequence_tokens: 1024, max_forward_tokens: 4096, \
             max_request_text_bytes: 12345 }\n",
        )
        .unwrap();
        assert_eq!(cfg.service.max_sequence_tokens, 1024);
        assert_eq!(cfg.service.max_forward_tokens, 4096);
        assert_eq!(cfg.service.max_request_text_bytes, 12_345);
        assert_eq!(cfg.service.max_connections, 64);
    }

    #[test]
    fn limit_clamps_and_sqlite_count_validation() {
        assert_eq!(clamp_list_limit(usize::MAX), 1000);
        assert_eq!(clamp_stale_limit(usize::MAX), 200);
        assert_eq!(sqlite_count(42, "limit").unwrap(), 42);
        #[cfg(target_pointer_width = "64")]
        assert!(sqlite_count(usize::MAX, "offset").is_err());
    }

    #[test]
    fn aggregate_text_and_job_byte_accounting() {
        let texts = vec!["abc".to_string(), "é".to_string()];
        assert_eq!(total_text_bytes(&texts), Some(5));
        assert_eq!(
            total_embedding_input_bytes(&texts, "prefix"),
            Some(5 + 2 * 6),
            "prefix bytes are charged once per text"
        );
        assert_eq!(
            total_embedding_input_bytes(&vec![String::new(); 256], &"x".repeat(4096)),
            Some(256 * 4096)
        );
        let req = IndexFileRequest {
            path: "p".repeat(3),
            content: Some("body".repeat(2)),
            source: "doc".into(),
            domain: "d".into(),
            background: true,
        };
        assert_eq!(index_job_owned_bytes(&req), 3 + 8 + 3 + 1);
    }

    #[test]
    fn queue_budget_enforces_bytes_and_count() {
        let budget = QueueBudget::new(2, 10);
        let mut first = budget.try_reserve(6).unwrap();
        assert!(budget.try_reserve(5).is_err());
        let second = budget.try_reserve(4).unwrap();
        assert!(budget.try_reserve(0).is_err());
        assert!(!first.release());
        drop(second);
        assert!(budget.is_empty());
        assert!(budget.try_reserve(11).is_err());
    }

    #[tokio::test]
    async fn deferred_requeue_holds_reservation_until_loud_abandon() {
        fn job(path: &str, budget: &Arc<QueueBudget>) -> IndexJob {
            IndexJob {
                req: IndexFileRequest {
                    path: path.to_string(),
                    content: None,
                    source: "doc".to_string(),
                    domain: "test".to_string(),
                    background: true,
                },
                seq: 1,
                peer: "test:peer".to_string(),
                bytes: path.len() as u64,
                enqueued: Instant::now(),
                attempt: 2,
                reservation: budget.try_reserve(path.len()).unwrap(),
            }
        }

        let budget = QueueBudget::new(2, 1024);
        let (tx, mut rx) = tokio::sync::mpsc::channel(1);
        assert!(tx.try_send(job("queued", &budget)).is_ok());
        let deferred = job("deferred", &budget);
        let failure = defer_background_job(deferred, tx, Duration::ZERO, 2).await;
        let mut abandoned = match failure {
            Err(DeferredRequeueFailure::Full { job, attempts }) => {
                assert_eq!(attempts, 2);
                job
            }
            _ => panic!("full deferred requeue did not exhaust its bounded attempts"),
        };
        assert!(
            !budget.is_empty(),
            "deferred job still owns its reservation"
        );
        drop(rx.try_recv().unwrap());
        assert!(
            abandoned.reservation.release(),
            "final abandonment releases the last reservation"
        );
        assert!(budget.is_empty());
    }

    #[test]
    fn response_budget_charges_serialised_string_bytes() {
        let field = "\"\\\n\u{0001}";
        let serialised = serde_json::to_string(field).unwrap().len();
        assert!(serialised > field.len());

        let mut used = 0;
        assert!(charge_response_item(&mut used, &[field], 256 + field.len()).is_err());
        assert_eq!(used, 0);
        charge_response_item(&mut used, &[field], 256 + serialised).unwrap();
        assert_eq!(used, 256 + serialised);
    }

    #[test]
    fn micro_batches_stay_in_budget_and_cover_original_order() {
        assert_eq!(effective_sequence_limit(2048, 4096, 8192).unwrap(), 2048);
        assert_eq!(effective_sequence_limit(16_384, 4096, 8192).unwrap(), 2048);
        assert_eq!(effective_sequence_limit(0, 4096, 8192).unwrap(), 16);
        assert_eq!(
            effective_sequence_limit(2048, 512, 8192).unwrap(),
            512,
            "operator forward ceiling lowers the effective sequence limit"
        );
        assert!(
            effective_sequence_limit(2048, 4096, 8)
                .unwrap_err()
                .to_string()
                .contains("n_positions=8")
        );
        let lengths = [2048, 100, 401, 2048, 99, 400, 1000];
        let batches = plan_micro_batches(&lengths, 4096);
        let mut seen = Vec::new();
        for batch in &batches {
            let padded = batch.iter().map(|&i| lengths[i]).max().unwrap() * batch.len();
            assert!(padded <= 4096, "batch {batch:?} costs {padded}");
            seen.extend(batch.iter().copied());
        }
        seen.sort_unstable();
        assert_eq!(seen, (0..lengths.len()).collect::<Vec<_>>());
    }

    #[tokio::test]
    async fn bounded_line_reader_rejects_before_growing_past_limit() {
        let mut reader = BufReader::new(&b"123456789\n"[..]);
        let mut line = Vec::new();
        let budget = IngressBudget::new(64);
        let mut reservation = None;
        assert!(matches!(
            read_request_line(
                &mut reader,
                &mut line,
                8,
                None,
                None,
                &budget,
                &mut reservation,
            )
            .await
            .unwrap(),
            RequestLineRead::TooLarge
        ));
        assert!(line.len() <= 8);
    }

    #[tokio::test]
    async fn request_line_cap_excludes_newline_delimiter() {
        let mut reader = BufReader::new(&b"12345678\n"[..]);
        let mut line = Vec::new();
        let budget = IngressBudget::new(8);
        let mut reservation = None;
        assert!(matches!(
            read_request_line(
                &mut reader,
                &mut line,
                8,
                None,
                None,
                &budget,
                &mut reservation,
            )
            .await
            .unwrap(),
            RequestLineRead::Complete
        ));
        assert_eq!(line, b"12345678");
        assert!(line.capacity() <= 8);
    }

    #[tokio::test]
    async fn inference_permit_wait_is_bounded_and_clear() {
        let gate = Arc::new(tokio::sync::Semaphore::new(1));
        let _held = gate.clone().acquire_owned().await.unwrap();
        let error = acquire_inference_permit(gate, Duration::from_millis(5))
            .await
            .unwrap_err();
        assert!(error.starts_with("inference_busy:"), "{error}");
    }

    #[tokio::test]
    async fn inference_lease_holds_permit_until_caller_and_worker_release() {
        let gate = Arc::new(tokio::sync::Semaphore::new(1));
        let permit = gate.clone().acquire_owned().await.unwrap();
        let caller = Arc::new(InferenceLease { _permit: permit });
        let worker = caller.clone();
        drop(caller);
        assert_eq!(gate.available_permits(), 0);
        drop(worker);
        assert_eq!(gate.available_permits(), 1);
    }

    #[test]
    fn background_retry_only_classifies_transient_admission_failures() {
        assert!(transient_background_failure(&json_error(
            "inference_busy: inference permit unavailable"
        )));
        assert!(transient_background_failure(&json_error(
            "model load failed: model loading suspended (cooldown active)"
        )));
        assert!(!transient_background_failure(&json_error(
            "validation failed: bad metadata"
        )));
    }

    #[test]
    fn ingress_budget_accounts_and_releases_aggregate_bytes() {
        let budget = IngressBudget::new(10);
        let first = budget.try_reserve(6).unwrap();
        assert_eq!(budget.used_bytes(), 6);
        assert!(budget.try_reserve(5).is_err());
        let second = budget.try_reserve(4).unwrap();
        assert_eq!(budget.used_bytes(), 10);
        drop(first);
        assert_eq!(budget.used_bytes(), 4);
        drop(second);
        assert_eq!(budget.used_bytes(), 0);
    }

    #[test]
    fn frame_deadline_wins_over_per_read_idle_timeout() {
        assert_eq!(
            next_read_timeout(
                Some(Duration::from_secs(300)),
                Some(Duration::from_secs(600)),
                Some(Duration::from_secs(590)),
            ),
            Some((Duration::from_secs(10), ReadTimeoutKind::Frame))
        );
        assert_eq!(
            next_read_timeout(
                Some(Duration::from_secs(300)),
                Some(Duration::from_secs(600)),
                Some(Duration::from_secs(1)),
            ),
            Some((Duration::from_secs(300), ReadTimeoutKind::Idle))
        );
    }

    #[test]
    fn f16_safetensor_conversion_preserves_embedding_cosine() {
        let dir = temp_dir("f16-conversion");
        let source = dir.join("model.safetensors");
        let values = vec![0.125f32, -1.75, 2.345_678, 0.000_123];
        let tensor = Tensor::from_vec(values, (4,), &Device::Cpu).unwrap();
        let direct = tensor
            .to_dtype(DType::F16)
            .unwrap()
            .to_dtype(DType::F32)
            .unwrap()
            .to_vec1::<f32>()
            .unwrap();
        let mut tensors = HashMap::new();
        tensors.insert("embedding".to_string(), tensor);
        candle_core::safetensors::save(&tensors, &source).unwrap();

        let converted = ensure_f16_weights(&source).unwrap();
        let got = candle_core::safetensors::load(&converted, &Device::Cpu)
            .unwrap()
            .remove("embedding")
            .unwrap()
            .to_dtype(DType::F32)
            .unwrap()
            .to_vec1::<f32>()
            .unwrap();
        let dot: f32 = direct.iter().zip(&got).map(|(a, b)| a * b).sum();
        let norm_a: f32 = direct.iter().map(|v| v * v).sum::<f32>().sqrt();
        let norm_b: f32 = got.iter().map(|v| v * v).sum::<f32>().sqrt();
        assert!(dot / (norm_a * norm_b) >= 0.999);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn f16_conversion_cleans_tmp_and_replaces_invalid_target() {
        let dir = temp_dir("f16-recovery");
        let source = dir.join("model.safetensors");
        let tensor = Tensor::from_vec(vec![1.0f32, 2.0], (2,), &Device::Cpu).unwrap();
        let mut tensors = HashMap::new();
        tensors.insert("expected".to_string(), tensor);
        candle_core::safetensors::save(&tensors, &source).unwrap();
        let stale = dir.join(".model.f16.safetensors.tmp-4294967295");
        std::fs::write(&stale, b"stale").unwrap();
        let target = dir.join("model.f16.safetensors");
        std::fs::write(&target, b"not safetensors").unwrap();

        let converted = ensure_f16_weights(&source).unwrap();
        assert_eq!(converted, target);
        assert!(!stale.exists());
        validate_f16_weights(&source, &target).unwrap();
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn checkpoint_owed_stays_sticky_until_success() {
        let owed = CheckpointOwed::default();
        assert!(!owed.is_owed());
        owed.mark();
        assert!(owed.is_owed());
        owed.record_result(true);
        assert!(owed.is_owed(), "busy checkpoint must remain owed");
        owed.record_result(false);
        assert!(!owed.is_owed(), "successful retry clears owed state");
    }

    #[test]
    fn bounded_reader_rejects_non_regular_handles() {
        let error = read_file_bounded(Path::new("/dev/null"), 1024).unwrap_err();
        assert!(error.contains("not a regular file"), "{error}");
    }

    #[cfg(unix)]
    #[test]
    fn fifo_open_is_nonblocking_and_rejected() {
        let dir = temp_dir("fifo");
        let fifo = dir.join("input.fifo");
        let path = std::ffi::CString::new(fifo.as_os_str().as_encoded_bytes()).unwrap();
        assert_eq!(unsafe { libc::mkfifo(path.as_ptr(), 0o600) }, 0);
        let started = Instant::now();
        let error = read_file_bounded(&fifo, 1024).unwrap_err();
        assert!(error.contains("not a regular file"), "{error}");
        assert!(started.elapsed() < Duration::from_secs(1));
        let _ = std::fs::remove_dir_all(&dir);
    }

    fn test_vector_db() -> VectorDb {
        unsafe {
            rusqlite::ffi::sqlite3_auto_extension(Some(std::mem::transmute::<
                *const (),
                unsafe extern "C" fn(
                    *mut rusqlite::ffi::sqlite3,
                    *mut *mut i8,
                    *const rusqlite::ffi::sqlite3_api_routines,
                ) -> i32,
            >(
                sqlite_vec::sqlite3_vec_init as *const (),
            )));
        }
        let mut conn = Connection::open_in_memory().unwrap();
        conn.set_transaction_behavior(rusqlite::TransactionBehavior::Immediate);
        conn.execute_batch(&format!(
            "CREATE TABLE chunks (
                id INTEGER PRIMARY KEY AUTOINCREMENT,
                content TEXT NOT NULL,
                source TEXT NOT NULL DEFAULT '',
                metadata TEXT NOT NULL DEFAULT '',
                content_hash BLOB UNIQUE,
                created TEXT NOT NULL DEFAULT (datetime('now'))
            );
            CREATE VIRTUAL TABLE vec_chunks USING vec0(embedding float[{EMBEDDING_DIM}]);"
        ))
        .unwrap();
        VectorDb {
            conn,
            search_index: Arc::new(std::sync::RwLock::new(vindex::VectorIndex::new(
                EMBEDDING_DIM,
                vindex::VectorDtype::F32,
            ))),
            checkpoint_owed: CheckpointOwed::default(),
        }
    }

    #[test]
    fn pre_skipped_duplicate_vanish_rolls_back_then_retries_with_embedding() {
        let db = test_vector_db();
        let texts = vec!["unchanged".to_string(), "raced".to_string()];
        let source = "doc";
        let initial_embeddings = vec![
            Some(vec![0.0; EMBEDDING_DIM]),
            Some(vec![1.0; EMBEDDING_DIM]),
        ];
        let no_preflight = vec![None, None];
        let first = db
            .store_revalidated(
                &initial_embeddings,
                &no_preflight,
                &texts,
                source,
                &["old-a".to_string(), "old-b".to_string()],
            )
            .unwrap();
        let ids = match first {
            StoreAttempt::Committed { ids, .. } => ids,
            StoreAttempt::NeedsEmbeddings(_) => panic!("initial insert unexpectedly deferred"),
        };
        let preflight = db.preflight_duplicate_ids(&texts, source).unwrap();
        assert!(preflight.iter().all(Option::is_some));
        db.delete(&[ids[1]]).unwrap();

        let raced = db
            .store_revalidated(
                &[None, None],
                &preflight,
                &texts,
                source,
                &["new-a".to_string(), "new-b".to_string()],
            )
            .unwrap();
        assert!(matches!(raced, StoreAttempt::NeedsEmbeddings(ref v) if v == &[1]));
        let metadata: String = db
            .conn
            .query_row(
                "SELECT metadata FROM chunks WHERE id = ?1",
                [ids[0]],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(metadata, "old-a", "revalidation failure wrote metadata");

        let retry = db
            .store_revalidated(
                &[None, Some(vec![1.0; EMBEDDING_DIM])],
                &preflight,
                &texts,
                source,
                &["new-a".to_string(), "new-b".to_string()],
            )
            .unwrap();
        assert!(matches!(
            retry,
            StoreAttempt::Committed { duplicates: 1, .. }
        ));
        let count: i64 = db
            .conn
            .query_row("SELECT COUNT(*) FROM chunks", [], |row| row.get(0))
            .unwrap();
        assert_eq!(count, 2);
    }

    #[test]
    fn system_legacy_toml_is_ignored() {
        // C11: the TOML fallback is gone. A lone legacy config.toml is
        // never read — resolution reports "no config" (None).
        let dir = temp_dir("legacy");
        std::fs::write(
            dir.join("config.toml"),
            "schema_version = 3\n[service]\nsocket_path = \"/run/legacy.sock\"\n",
        )
        .unwrap();
        assert!(load_system_config(&dir).unwrap().is_none());
        assert!(!dir.join("config.conf.mix").exists());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn system_bad_conf_mix_is_hard_error() {
        let dir = temp_dir("badparse");
        // An unparseable .conf.mix must hard-fail.
        std::fs::write(dir.join("config.conf.mix"), "schema_version: $oops\n").unwrap();
        assert!(load_system_config(&dir).is_err());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn split_oversized_passthrough_when_fits() {
        let s = "line one\nline two\nline three";
        let out = split_oversized(s, 6000);
        assert_eq!(out, vec![s.to_string()]);
    }

    #[test]
    fn split_oversized_breaks_at_line_boundaries() {
        // Five 20-char lines, budget 50 → groups of 2 lines (~41 chars) per piece.
        let line = "x".repeat(20);
        let text = std::iter::repeat_n(line.as_str(), 5)
            .collect::<Vec<_>>()
            .join("\n");
        let out = split_oversized(&text, 50);
        assert!(out.len() > 1, "expected multiple pieces, got {}", out.len());
        // No piece exceeds the budget.
        for piece in &out {
            assert!(piece.chars().count() <= 50, "piece too big: {piece:?}");
        }
        // No line is ever split mid-content.
        for piece in &out {
            for l in piece.lines() {
                assert_eq!(l, line);
            }
        }
        // Reassembly is lossless (pieces rejoin to the original).
        assert_eq!(out.join("\n"), text);
    }

    #[test]
    fn split_oversized_hard_splits_overlong_single_line() {
        // A single line longer than the budget is hard-split on char boundaries
        // so its tail is not silently lost; concatenation is lossless.
        let huge = "y".repeat(100);
        let out = split_oversized(&huge, 50);
        assert!(out.len() >= 2, "expected a hard split, got {}", out.len());
        for piece in &out {
            assert!(piece.chars().count() <= 50);
        }
        assert_eq!(out.concat(), huge);
    }

    #[test]
    fn split_oversized_hard_split_is_utf8_safe() {
        // Multibyte chars must never be split mid-codepoint (would panic / corrupt).
        let s = "é".repeat(80); // 80 codepoints, 2 bytes each
        let out = split_oversized(&s, 30);
        for piece in &out {
            assert!(piece.chars().count() <= 30);
        }
        assert_eq!(out.concat(), s);
    }

    #[test]
    fn split_oversized_mixed_lines_drops_no_content() {
        // Normal short lines followed by an overlong line: every content char
        // must survive (chunk boundaries normalize separator newlines only).
        let long = "z".repeat(120);
        let text = format!("short one\nshort two\n{long}\ntail");
        let out = split_oversized(&text, 50);
        for piece in &out {
            assert!(piece.chars().count() <= 50, "piece too big: {piece:?}");
        }
        // No non-newline character is lost or reordered.
        assert_eq!(
            out.concat().replace('\n', ""),
            text.replace('\n', ""),
            "content must be preserved across mixed line-boundary + hard splits"
        );
    }

    #[test]
    fn token_aware_split_preserves_dense_tail_and_bounds_every_piece() {
        let text = "界".repeat(37);
        let count = |piece: &str| Ok(2 + piece.chars().count() * 2);
        let out = split_token_aware(&text, 16, count).unwrap();
        assert!(out.len() > 1);
        assert_eq!(out.concat(), text);
        for piece in out {
            assert!(count(&piece).unwrap() <= 16, "oversized piece {piece:?}");
        }
    }

    #[test]
    fn bounded_file_read_uses_open_handle_and_rejects_oversize() {
        let dir = temp_dir("bounded-read");
        let path = dir.join("doc.md");
        let replacement = dir.join("replacement.md");
        std::fs::write(&path, "original").unwrap();
        let opened = std::fs::File::open(&path).unwrap();
        std::fs::write(&replacement, "replacement path content").unwrap();
        std::fs::rename(&replacement, &path).unwrap();
        assert_eq!(read_open_file_bounded(opened, 8).unwrap(), "original");

        let oversized = std::fs::File::open(&path).unwrap();
        assert!(
            read_open_file_bounded(oversized, 8)
                .unwrap_err()
                .contains("max 8")
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn date_shaped_vs_valid() {
        // README/notes → not date-shaped → would downgrade to doc.
        assert!(!is_date_shaped("README.md"));
        assert!(!is_date_shaped("notes.md"));
        assert!(!is_date_shaped(""));
        // A real dated entry → date-shaped AND valid.
        assert!(is_date_shaped("2026-06-07"));
        assert!(is_valid_ymd("2026-06-07"));
        // A typo'd date → still date-shaped (stays journal) but NOT valid, so
        // validate_store_entry will surface the error instead of masking it.
        assert!(is_date_shaped("2026-13-01"));
        assert!(!is_valid_ymd("2026-13-01"));
        assert!(is_date_shaped("2026-06-aa")); // digit year + delimiters
        assert!(!is_valid_ymd("2026-06-aa"));
    }
}
