pub mod amp;

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;

use anyhow::Result;
use serde::{Deserialize, Serialize};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::UnixListener;
use tokio::sync::mpsc;

// ── RC codes (ARexx convention) ──

pub const RC_SUCCESS: u8 = 0;
pub const RC_WARNING: u8 = 5;
pub const RC_ERROR: u8 = 10;
pub const RC_FAILURE: u8 = 20;

// ── Wire format ──

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
    pub rc: u8,
    pub ok: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub data: Option<serde_json::Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

impl PortResponse {
    pub fn success(data: serde_json::Value) -> Self {
        Self { rc: RC_SUCCESS, ok: true, data: Some(data), error: None }
    }

    pub fn ok() -> Self {
        Self { rc: RC_SUCCESS, ok: true, data: None, error: None }
    }

    pub fn warning(msg: &str) -> Self {
        Self { rc: RC_WARNING, ok: true, data: None, error: Some(msg.to_string()) }
    }

    pub fn error(msg: &str) -> Self {
        Self { rc: RC_ERROR, ok: false, data: None, error: Some(msg.to_string()) }
    }

    pub fn failure(msg: &str) -> Self {
        Self { rc: RC_FAILURE, ok: false, data: None, error: Some(msg.to_string()) }
    }
}

// ── Script info (for macro menus) ──

/// Metadata for a Lua script that appears in an app's Scripts menu.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ScriptInfo {
    /// Display name (derived from filename, e.g. "Add Watermark")
    pub display_name: String,
    /// Full path to the .lua file
    pub path: String,
}

// ── Port events (notification channel for UI updates) ──

#[derive(Debug, Clone)]
pub enum PortEvent {
    /// A command was dispatched on the port
    Command { name: String, ok: bool },
    /// App should bring its window to front
    Activate,
    /// Scripts menu was updated by daemon
    ScriptsUpdated(Vec<ScriptInfo>),
}

// ── Command metadata and handler ──

type CommandFn = Box<dyn Fn(serde_json::Value) -> Result<serde_json::Value> + Send + Sync>;

struct CommandEntry {
    handler: CommandFn,
    description: String,
}

// ── Port builder ──

pub struct Port {
    name: String,
    commands: HashMap<String, CommandEntry>,
    notifier: Option<mpsc::UnboundedSender<PortEvent>>,
    app_name: Option<String>,
    app_version: Option<String>,
    wants_help: bool,
    wants_activate: bool,
}

impl Port {
    pub fn new(name: &str) -> Self {
        Self {
            name: name.to_string(),
            commands: HashMap::new(),
            notifier: None,
            app_name: None,
            app_version: None,
            wants_help: false,
            wants_activate: false,
        }
    }

    /// Register a command with a description and handler.
    pub fn command<F>(mut self, name: &str, description: &str, handler: F) -> Self
    where
        F: Fn(serde_json::Value) -> Result<serde_json::Value> + Send + Sync + 'static,
    {
        self.commands.insert(name.to_string(), CommandEntry {
            handler: Box::new(handler),
            description: description.to_string(),
        });
        self
    }

    /// Attach a notification channel for UI updates.
    pub fn events(mut self, tx: mpsc::UnboundedSender<PortEvent>) -> Self {
        self.notifier = Some(tx);
        self
    }

    /// Auto-generate a HELP command from registered command metadata.
    pub fn standard_help(mut self) -> Self {
        self.wants_help = true;
        self
    }

    /// Auto-generate an INFO command returning port/app metadata.
    pub fn standard_info(mut self, app_name: &str, version: &str) -> Self {
        self.app_name = Some(app_name.to_string());
        self.app_version = Some(version.to_string());
        self
    }

    /// Auto-generate an ACTIVATE command that signals the UI to focus.
    /// Requires `.events()` to be set.
    pub fn standard_activate(mut self) -> Self {
        self.wants_activate = true;
        self
    }

    pub fn socket_path(name: &str) -> PathBuf {
        let uid = unsafe { libc::getuid() };
        PathBuf::from(format!("/run/user/{uid}/cosmix/ports/{name}.sock"))
    }

    pub fn start(mut self) -> Result<PortHandle> {
        let socket_path = Self::socket_path(&self.name);

        // Ensure directory exists
        if let Some(parent) = socket_path.parent() {
            std::fs::create_dir_all(parent)?;
        }

        // Remove stale socket
        let _ = std::fs::remove_file(&socket_path);

        // Inject standard ACTIVATE command (before HELP so HELP sees it)
        if self.wants_activate {
            if let Some(ref tx) = self.notifier {
                let activate_tx = tx.clone();
                self.commands.insert("activate".to_string(), CommandEntry {
                    description: "Bring application window to front".to_string(),
                    handler: Box::new(move |_| {
                        let _ = activate_tx.send(PortEvent::Activate);
                        Ok(serde_json::json!("activated"))
                    }),
                });
            } else {
                tracing::warn!("standard_activate() requires events() — skipping");
            }
        }

        // Inject standard INFO command (before HELP so HELP sees it)
        if let (Some(app_name), Some(app_version)) = (&self.app_name, &self.app_version) {
            let port_name = self.name.clone();
            let app = app_name.clone();
            let version = app_version.clone();
            // Count includes info itself + help if requested
            let extra = if self.wants_help { 1 } else { 0 };
            let cmd_count = self.commands.len() + 1 + extra; // +1 for info, +1 for help
            let mut cmd_names: Vec<String> = self.commands.keys().cloned().collect();
            cmd_names.push("info".to_string());
            if self.wants_help {
                cmd_names.push("help".to_string());
            }
            cmd_names.sort();

            self.commands.insert("info".to_string(), CommandEntry {
                description: "Return port and application metadata".to_string(),
                handler: Box::new(move |_| {
                    Ok(serde_json::json!({
                        "port": port_name,
                        "app": app,
                        "version": version,
                        "commands": cmd_count,
                        "command_list": cmd_names,
                    }))
                }),
            });
        }

        // Inject standard HELP command last (so it sees all commands)
        if self.wants_help {
            let mut meta: HashMap<String, String> = self.commands.iter()
                .map(|(k, v)| (k.clone(), v.description.clone()))
                .collect();
            meta.insert("help".to_string(), "List commands or describe a specific command".to_string());
            let meta_arc = Arc::new(meta);

            self.commands.insert("help".to_string(), CommandEntry {
                description: "List commands or describe a specific command".to_string(),
                handler: Box::new(move |args| {
                    let cmd_arg = args.get("command")
                        .and_then(|v| v.as_str())
                        .or_else(|| args.as_str());

                    if let Some(cmd_name) = cmd_arg {
                        match meta_arc.get(cmd_name) {
                            Some(desc) => Ok(serde_json::json!({
                                "command": cmd_name,
                                "description": desc,
                            })),
                            None => anyhow::bail!("Unknown command: {cmd_name}"),
                        }
                    } else {
                        let mut cmds: Vec<&str> = meta_arc.keys().map(|s| s.as_str()).collect();
                        cmds.sort();
                        Ok(serde_json::json!({ "commands": cmds }))
                    }
                }),
            });
        }

        // Extract handlers into the runtime map
        let handlers: HashMap<String, CommandFn> = self.commands.into_iter()
            .map(|(k, v)| (k, v.handler))
            .collect();

        let commands = Arc::new(handlers);
        let notifier = self.notifier;
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
                                    let ntf = notifier.clone();
                                    tokio::spawn(handle_connection(stream, cmds, ntf));
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
    notifier: Option<mpsc::UnboundedSender<PortEvent>>,
) {
    if let Err(e) = handle_connection_inner(&mut stream, &commands, &notifier).await {
        tracing::debug!("Port connection error: {e}");
    }
}

async fn handle_connection_inner(
    stream: &mut tokio::net::UnixStream,
    commands: &HashMap<String, CommandFn>,
    notifier: &Option<mpsc::UnboundedSender<PortEvent>>,
) -> Result<()> {
    // Read AMP request (client shuts down write side to signal EOF)
    let msg = amp::read_from_stream(stream).await?;

    let command = msg.get("command")
        .ok_or_else(|| anyhow::anyhow!("Missing 'command' header in AMP request"))?
        .to_string();

    let args: serde_json::Value = if msg.body.is_empty() {
        serde_json::Value::Null
    } else {
        serde_json::from_str(&msg.body)?
    };

    let request = PortRequest { command, args };
    let response = dispatch(&request, commands, notifier);

    // Build AMP response
    let mut resp_msg = amp::AmpMessage::new();
    resp_msg.set("rc", &response.rc.to_string());
    if let Some(ref error) = response.error {
        resp_msg.set("error", error);
    }
    if let Some(ref data) = response.data {
        resp_msg.body = serde_json::to_string(data)?;
    }

    stream.write_all(&resp_msg.to_bytes()).await?;

    Ok(())
}

fn dispatch(
    request: &PortRequest,
    commands: &HashMap<String, CommandFn>,
    notifier: &Option<mpsc::UnboundedSender<PortEvent>>,
) -> PortResponse {
    // Internal command: daemon pushes script list updates
    if request.command == "__scripts__" {
        if let Some(tx) = notifier {
            let scripts: Vec<ScriptInfo> = serde_json::from_value(request.args.clone())
                .unwrap_or_default();
            let count = scripts.len();
            let _ = tx.send(PortEvent::ScriptsUpdated(scripts));
            return PortResponse::success(serde_json::json!({"updated": count}));
        }
        return PortResponse::ok();
    }

    let response = match commands.get(&request.command) {
        Some(handler) => match handler(request.args.clone()) {
            Ok(data) => PortResponse::success(data),
            Err(e) => PortResponse::error(&e.to_string()),
        },
        None => {
            let mut available: Vec<&str> = commands.keys().map(|s| s.as_str()).collect();
            available.sort();
            PortResponse::error(&format!(
                "Unknown command '{}'. Available: {}",
                request.command,
                available.join(", ")
            ))
        }
    };

    if let Some(tx) = notifier {
        let _ = tx.send(PortEvent::Command {
            name: request.command.clone(),
            ok: response.ok,
        });
    }

    response
}

// ── Client helper (for daemon to call ports) ──

pub async fn call_port(socket_path: &str, command: &str, args: serde_json::Value) -> Result<serde_json::Value> {
    let mut stream = tokio::net::UnixStream::connect(socket_path).await?;

    // Build AMP request
    let mut msg = amp::AmpMessage::new();
    msg.set("command", command);
    if !args.is_null() {
        msg.body = serde_json::to_string(&args)?;
    }

    // Write request and signal end
    stream.write_all(&msg.to_bytes()).await?;
    stream.shutdown().await?;

    // Read AMP response
    let mut buf = Vec::new();
    stream.read_to_end(&mut buf).await?;
    let raw = String::from_utf8(buf)?;
    let resp = amp::parse(&raw)?;

    let rc: u8 = resp.get("rc").and_then(|s| s.parse().ok()).unwrap_or(0);
    if rc == 0 || rc == RC_WARNING {
        if resp.body.is_empty() {
            Ok(serde_json::Value::Null)
        } else {
            Ok(serde_json::from_str(&resp.body)?)
        }
    } else {
        let error = resp.get("error").unwrap_or("unknown error");
        anyhow::bail!("{error}")
    }
}
