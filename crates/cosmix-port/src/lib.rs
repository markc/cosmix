use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;

use anyhow::Result;
use serde::{Deserialize, Serialize};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::UnixListener;

// ── Wire format (same as daemon IPC) ──

#[derive(Debug, Deserialize)]
pub struct PortRequest {
    pub command: String,
    #[serde(default = "default_args")]
    pub args: serde_json::Value,
}

fn default_args() -> serde_json::Value {
    serde_json::Value::Null
}

#[derive(Debug, Serialize, Deserialize)]
pub struct PortResponse {
    pub ok: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub data: Option<serde_json::Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

impl PortResponse {
    pub fn success(data: serde_json::Value) -> Self {
        Self { ok: true, data: Some(data), error: None }
    }

    pub fn ok() -> Self {
        Self { ok: true, data: None, error: None }
    }

    pub fn error(msg: &str) -> Self {
        Self { ok: false, data: None, error: Some(msg.to_string()) }
    }
}

// ── Command handler type ──

type CommandFn = Box<dyn Fn(serde_json::Value) -> Result<serde_json::Value> + Send + Sync>;

// ── Port ──

pub struct Port {
    name: String,
    commands: HashMap<String, CommandFn>,
}

impl Port {
    pub fn new(name: &str) -> Self {
        Self {
            name: name.to_string(),
            commands: HashMap::new(),
        }
    }

    pub fn command<F>(mut self, name: &str, handler: F) -> Self
    where
        F: Fn(serde_json::Value) -> Result<serde_json::Value> + Send + Sync + 'static,
    {
        self.commands.insert(name.to_string(), Box::new(handler));
        self
    }

    pub fn socket_path(name: &str) -> PathBuf {
        let uid = unsafe { libc::getuid() };
        PathBuf::from(format!("/run/user/{uid}/cosmix/ports/{name}.sock"))
    }

    pub fn start(self) -> Result<PortHandle> {
        let socket_path = Self::socket_path(&self.name);

        // Ensure directory exists
        if let Some(parent) = socket_path.parent() {
            std::fs::create_dir_all(parent)?;
        }

        // Remove stale socket
        let _ = std::fs::remove_file(&socket_path);

        let commands = Arc::new(self.commands);
        let name = self.name.clone();
        let path = socket_path.clone();

        let (shutdown_tx, mut shutdown_rx) = tokio::sync::oneshot::channel::<()>();

        std::thread::spawn(move || {
            let rt = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .expect("failed to create tokio runtime for port");

            rt.block_on(async move {
                let listener = match UnixListener::bind(&path) {
                    Ok(l) => l,
                    Err(e) => {
                        tracing::error!("Port {name}: failed to bind {}: {e}", path.display());
                        return;
                    }
                };

                tracing::info!("Port {name} listening on {}", path.display());

                loop {
                    tokio::select! {
                        accept = listener.accept() => {
                            match accept {
                                Ok((stream, _)) => {
                                    let cmds = commands.clone();
                                    tokio::spawn(handle_connection(stream, cmds));
                                }
                                Err(e) => {
                                    tracing::debug!("Port {name}: accept error: {e}");
                                }
                            }
                        }
                        _ = &mut shutdown_rx => {
                            tracing::info!("Port {name} shutting down");
                            break;
                        }
                    }
                }

                let _ = std::fs::remove_file(&path);
            });
        });

        Ok(PortHandle {
            _shutdown: shutdown_tx,
            socket_path,
        })
    }
}

pub struct PortHandle {
    _shutdown: tokio::sync::oneshot::Sender<()>,
    pub socket_path: PathBuf,
}

async fn handle_connection(
    mut stream: tokio::net::UnixStream,
    commands: Arc<HashMap<String, CommandFn>>,
) {
    if let Err(e) = handle_connection_inner(&mut stream, &commands).await {
        tracing::debug!("Port connection error: {e}");
    }
}

async fn handle_connection_inner(
    stream: &mut tokio::net::UnixStream,
    commands: &HashMap<String, CommandFn>,
) -> Result<()> {
    // Read 4-byte length prefix
    let mut len_buf = [0u8; 4];
    stream.read_exact(&mut len_buf).await?;
    let len = u32::from_be_bytes(len_buf) as usize;

    if len > 1_048_576 {
        anyhow::bail!("Request too large: {len}");
    }

    let mut buf = vec![0u8; len];
    stream.read_exact(&mut buf).await?;

    let request: PortRequest = serde_json::from_slice(&buf)?;
    let response = dispatch(&request, commands);

    let resp_json = serde_json::to_vec(&response)?;
    let resp_len = (resp_json.len() as u32).to_be_bytes();
    stream.write_all(&resp_len).await?;
    stream.write_all(&resp_json).await?;

    Ok(())
}

fn dispatch(request: &PortRequest, commands: &HashMap<String, CommandFn>) -> PortResponse {
    match commands.get(&request.command) {
        Some(handler) => match handler(request.args.clone()) {
            Ok(data) => PortResponse::success(data),
            Err(e) => PortResponse::error(&e.to_string()),
        },
        None => {
            let available: Vec<&str> = commands.keys().map(|s| s.as_str()).collect();
            PortResponse::error(&format!(
                "Unknown command '{}'. Available: {}",
                request.command,
                available.join(", ")
            ))
        }
    }
}

// ── Client helper (for daemon to call ports) ──

pub async fn call_port(socket_path: &str, command: &str, args: serde_json::Value) -> Result<serde_json::Value> {
    let mut stream = tokio::net::UnixStream::connect(socket_path).await?;

    let request = serde_json::json!({ "command": command, "args": args });
    let req_json = serde_json::to_vec(&request)?;
    let req_len = (req_json.len() as u32).to_be_bytes();
    stream.write_all(&req_len).await?;
    stream.write_all(&req_json).await?;

    let mut len_buf = [0u8; 4];
    stream.read_exact(&mut len_buf).await?;
    let len = u32::from_be_bytes(len_buf) as usize;

    let mut buf = vec![0u8; len];
    stream.read_exact(&mut buf).await?;

    let response: PortResponse = serde_json::from_slice(&buf)?;
    if response.ok {
        Ok(response.data.unwrap_or(serde_json::Value::Null))
    } else {
        anyhow::bail!(response.error.unwrap_or_else(|| "unknown error".into()))
    }
}
