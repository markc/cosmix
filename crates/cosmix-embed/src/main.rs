use anyhow::{Context, Result};
use candle_core::{DType, Device, Tensor};
use candle_nn::VarBuilder;
use candle_transformers::models::nomic_bert::{self, NomicBertModel};
use hf_hub::{api::sync::Api, Repo, RepoType};
use serde::{Deserialize, Serialize};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;
use tokenizers::Tokenizer;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::UnixListener;
use tracing::{error, info};

const MODEL_ID: &str = "nomic-ai/nomic-embed-text-v1.5";
const SOCKET_DIR: &str = "/run/cosmix";
const SOCKET_PATH: &str = "/run/cosmix/embed.sock";
const IDLE_TIMEOUT: Duration = Duration::from_secs(60);

/// Number of active connections. When this drops to 0, the idle timer starts.
static ACTIVE_CONNECTIONS: AtomicUsize = AtomicUsize::new(0);

#[derive(Deserialize)]
struct EmbedRequest {
    texts: Vec<String>,
    #[serde(default = "default_prefix")]
    prefix: String,
}

fn default_prefix() -> String {
    "search_document: ".into()
}

#[derive(Serialize)]
struct EmbedResponse {
    embeddings: Vec<Vec<f32>>,
}

#[derive(Serialize)]
struct ErrorResponse {
    error: String,
}

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

        let vb = unsafe {
            VarBuilder::from_mmaped_safetensors(&[weights_path], dtype, &device)?
        };
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

        // Prepend prefix to each text
        let prefixed: Vec<String> = texts.iter().map(|t| format!("{prefix}{t}")).collect();

        // Tokenize with padding
        let tokens = self
            .tokenizer
            .encode_batch(prefixed.iter().map(|s| s.as_str()).collect::<Vec<_>>(), true)
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
        let input_ids =
            Tensor::from_vec(all_ids, (batch_size, max_len), &self.device)?;
        let attention_mask =
            Tensor::from_vec(all_mask, (batch_size, max_len), &self.device)?;
        let token_type_ids =
            Tensor::from_vec(all_type_ids, (batch_size, max_len), &self.device)?;

        let hidden = self
            .model
            .forward(&input_ids, Some(&token_type_ids), Some(&attention_mask))?;

        // Ensure f32 for pooling/normalization (no-op if already f32)
        let hidden = hidden.to_dtype(DType::F32)?;

        // Mean pooling over non-padding tokens, then L2 normalize
        let pooled = nomic_bert::mean_pooling(&hidden, &attention_mask)?;
        let normalized = nomic_bert::l2_normalize(&pooled)?;

        // Convert to Vec<Vec<f32>>
        let mut results = Vec::with_capacity(batch_size);
        for i in 0..batch_size {
            let emb = normalized.get(i)?.to_vec1::<f32>()?;
            results.push(emb);
        }

        Ok(results)
    }
}

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::from_default_env()
                .add_directive(tracing::Level::INFO.into()),
        )
        .init();

    // Check for systemd socket activation first
    let listener = if let Ok(listener) = try_systemd_socket() {
        info!("using systemd socket activation");
        listener
    } else {
        // Create socket directory and bind
        std::fs::create_dir_all(SOCKET_DIR)
            .with_context(|| format!("creating {SOCKET_DIR}"))?;
        // Remove stale socket
        let _ = std::fs::remove_file(SOCKET_PATH);
        let listener = UnixListener::bind(SOCKET_PATH)
            .with_context(|| format!("binding {SOCKET_PATH}"))?;
        // Make socket world-readable so other users can embed
        std::fs::set_permissions(SOCKET_PATH, std::os::unix::fs::PermissionsExt::from_mode(0o666))?;
        info!("listening on {SOCKET_PATH}");
        listener
    };

    // Parse --f32 flag for full precision (default: f16)
    let dtype = if std::env::args().any(|a| a == "--f32") {
        DType::F32
    } else {
        DType::F16
    };

    // Load model
    let model = EmbedModel::load(dtype)?;
    let model = std::sync::Arc::new(model);

    info!("ready for requests (idle timeout: {}s)", IDLE_TIMEOUT.as_secs());

    // Spawn idle watchdog — sends signal when idle timeout expires
    let (idle_tx, mut idle_rx) = tokio::sync::oneshot::channel::<()>();
    tokio::spawn(async move {
        idle_watchdog().await;
        let _ = idle_tx.send(());
    });

    loop {
        tokio::select! {
            result = listener.accept() => {
                let (stream, _) = result?;
                let model = model.clone();
                ACTIVE_CONNECTIONS.fetch_add(1, Ordering::Relaxed);

                tokio::spawn(async move {
                    if let Err(e) = handle_connection(stream, &model).await {
                        error!("connection error: {e}");
                    }
                    ACTIVE_CONNECTIONS.fetch_sub(1, Ordering::Relaxed);
                });
            }
            _ = &mut idle_rx => {
                break;
            }
        }
    }

    info!("idle timeout reached, shutting down");
    Ok(())
}

/// Exits when there have been no active connections for IDLE_TIMEOUT.
async fn idle_watchdog() {
    // Give the service time to receive its first connection after socket activation
    tokio::time::sleep(IDLE_TIMEOUT).await;

    loop {
        if ACTIVE_CONNECTIONS.load(Ordering::Relaxed) == 0 {
            return;
        }
        // Re-check every 5 seconds while connections are active
        tokio::time::sleep(Duration::from_secs(5)).await;
    }
}

async fn handle_connection(
    stream: tokio::net::UnixStream,
    model: &EmbedModel,
) -> Result<()> {
    let (reader, mut writer) = stream.into_split();
    let mut reader = BufReader::new(reader);
    let mut line = String::new();

    // Read newline-delimited JSON requests
    loop {
        line.clear();
        let n = reader.read_line(&mut line).await?;
        if n == 0 {
            break; // EOF
        }

        let response = match serde_json::from_str::<EmbedRequest>(line.trim()) {
            Ok(req) => match model.embed(&req.texts, &req.prefix) {
                Ok(embeddings) => {
                    serde_json::to_string(&EmbedResponse { embeddings }).unwrap()
                }
                Err(e) => serde_json::to_string(&ErrorResponse {
                    error: e.to_string(),
                })
                .unwrap(),
            },
            Err(e) => serde_json::to_string(&ErrorResponse {
                error: format!("invalid request: {e}"),
            })
            .unwrap(),
        };

        writer.write_all(response.as_bytes()).await?;
        writer.write_all(b"\n").await?;
        writer.flush().await?;
    }

    Ok(())
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

    // fd 3 is the first passed fd per sd_listen_fds(3) convention
    let std_listener = unsafe { std::os::unix::net::UnixListener::from_raw_fd(3) };
    std_listener.set_nonblocking(true)?;
    let listener = UnixListener::from_std(std_listener)?;
    Ok(listener)
}
