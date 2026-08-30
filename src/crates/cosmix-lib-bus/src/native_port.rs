//! Unix socket port — native-only (not available in WASM).

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;

use anyhow::Result;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::UnixListener;
use tokio::sync::mpsc;

use crate::bus;
use crate::{CommandEntry, CommandFn, PortEvent, PortRequest, PortResponse, RC_ERROR, ScriptInfo};

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
        self.commands.insert(
            name.to_string(),
            CommandEntry {
                handler: Box::new(handler),
                description: description.to_string(),
            },
        );
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
                self.commands.insert(
                    "activate".to_string(),
                    CommandEntry {
                        description: "Bring application window to front".to_string(),
                        handler: Box::new(move |_| {
                            let _ = activate_tx.send(PortEvent::Activate);
                            Ok(serde_json::json!("activated"))
                        }),
                    },
                );
            } else {
                tracing::warn!("standard_activate() requires events() — skipping");
            }
        }

        // Inject standard INFO command (before HELP so HELP sees it)
        if let (Some(app_name), Some(app_version)) = (&self.app_name, &self.app_version) {
            let port_name = self.name.clone();
            let app = app_name.clone();
            let version = app_version.clone();
            let extra = if self.wants_help { 1 } else { 0 };
            let cmd_count = self.commands.len() + 1 + extra;
            let mut cmd_names: Vec<String> = self.commands.keys().cloned().collect();
            cmd_names.push("info".to_string());
            if self.wants_help {
                cmd_names.push("help".to_string());
            }
            cmd_names.sort();

            self.commands.insert(
                "info".to_string(),
                CommandEntry {
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
                },
            );
        }

        // Inject standard HELP command last (so it sees all commands)
        if self.wants_help {
            let mut meta: HashMap<String, String> = self
                .commands
                .iter()
                .map(|(k, v)| (k.clone(), v.description.clone()))
                .collect();
            meta.insert(
                "help".to_string(),
                "List commands or describe a specific command".to_string(),
            );
            let meta_arc = Arc::new(meta);

            self.commands.insert(
                "help".to_string(),
                CommandEntry {
                    description: "List commands or describe a specific command".to_string(),
                    handler: Box::new(move |args| {
                        let cmd_arg = args
                            .get("command")
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
                },
            );
        }

        // Extract handlers into the runtime map
        let handlers: HashMap<String, CommandFn> = self
            .commands
            .into_iter()
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
    // Read Bus request (client shuts down write side to signal EOF)
    let msg = bus::read_from_stream(stream).await?;

    let command = msg
        .get("command")
        .ok_or_else(|| anyhow::anyhow!("Missing 'command' header in Bus request"))?
        .to_string();

    let args: serde_json::Value = if msg.body.is_empty() {
        serde_json::Value::Null
    } else {
        serde_json::from_str(&msg.body)?
    };

    let request = PortRequest { command, args };
    let response = dispatch(&request, commands, notifier);

    // Build Bus response
    let mut resp_msg = bus::BusMessage::new();
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
            let scripts: Vec<ScriptInfo> =
                serde_json::from_value(request.args.clone()).unwrap_or_default();
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

pub async fn call_port(
    socket_path: &str,
    command: &str,
    args: serde_json::Value,
) -> Result<serde_json::Value> {
    // Preserve the original conflated contract for existing callers: an
    // application `rc >= 10` reply becomes an `Err` (the error message), a
    // transport failure also an `Err`. Callers that must distinguish the two
    // use `call_port_typed` (added for the Mix `$rc`-band contract).
    match call_port_typed(socket_path, command, args).await? {
        PortReply::Ok { value, .. } => Ok(value),
        PortReply::AppError { message, .. } => anyhow::bail!("{}", message),
    }
}

/// The two OUTCOMES of a port call that actually reached the peer, kept
/// distinct from a TRANSPORT failure (which is the `Err` of the enclosing
/// `Result`). Lets a caller map "the peer answered with rc>=10" (an
/// application error, a real status) separately from "I couldn't connect /
/// read / parse" (a transport failure) — the discrimination the Mix `$rc`
/// bands need (`>= 10` app vs `-1` transport). See `call_port_typed`.
#[derive(Debug, Clone)]
pub enum PortReply {
    /// Peer replied with a SUCCESS rc (`< RC_ERROR`, i.e. `0`, the warning
    /// `5`, or any sub-error status); `rc` is the exact value (so a warning
    /// `5` is not flattened to `0`) and `value` is the decoded body (`Null`
    /// when empty).
    Ok { rc: u8, value: serde_json::Value },
    /// Peer replied `rc >= 10` (an application error). `rc` is the exact
    /// status; `message` is the peer's error text (`error_message()`).
    AppError { rc: u8, message: String },
}

/// Like [`call_port`], but the `Result::Err` is ONLY a transport failure
/// (connect / write / read / size-cap / UTF-8 / parse) — a peer reply with
/// `rc >= 10` is `Ok(PortReply::AppError { rc, message })`, NOT an `Err`. So
/// a caller can map transport → one band and application-error → another
/// (the Mix handler maps them to `$rc = -1` and `$rc = rc` respectively).
pub async fn call_port_typed(
    socket_path: &str,
    command: &str,
    args: serde_json::Value,
) -> Result<PortReply> {
    let mut stream = tokio::net::UnixStream::connect(socket_path).await?;

    // Build Bus request
    let mut msg = bus::BusMessage::new();
    msg.set("command", command);
    if !args.is_null() {
        msg.body = serde_json::to_string(&args)?;
    }

    // Write request and signal end
    stream.write_all(&msg.to_bytes()).await?;
    stream.shutdown().await?;

    // Read Bus response (byte-capped to bound memory on a misbehaving
    // or hostile port; see `bus::MAX_MESSAGE_BYTES`).
    let mut buf = Vec::new();
    let mut limited = stream.take(bus::MAX_MESSAGE_BYTES as u64 + 1);
    limited.read_to_end(&mut buf).await?;
    if buf.len() > bus::MAX_MESSAGE_BYTES {
        anyhow::bail!("Bus response exceeds {} byte limit", bus::MAX_MESSAGE_BYTES);
    }
    let raw = String::from_utf8(buf)?;
    let resp = bus::parse(&raw)?;

    let rc: u8 = resp.get("rc").and_then(|s| s.parse().ok()).unwrap_or(0);
    // `rc < RC_ERROR` (< 10) is the SUCCESS band — 0, the warning 5, and any
    // other sub-error status — matching NodedClient::call_typed (which treats
    // only `rc >= 10` as an application error). The two typed paths MUST agree
    // on the mid-band (1-9), or the same reply would classify differently over
    // a local socket vs the broker.
    if rc < RC_ERROR {
        let value = if resp.body.is_empty() {
            serde_json::Value::Null
        } else {
            serde_json::from_str(&resp.body)?
        };
        Ok(PortReply::Ok { rc, value })
    } else {
        Ok(PortReply::AppError {
            rc,
            message: resp.error_message(),
        })
    }
}

#[cfg(test)]
mod call_port_typed_tests {
    use super::*;
    use crate::bus::BusMessage;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::UnixListener;

    /// Spawn a one-shot Bus port that replies with the given `rc`/`body`,
    /// returning its socket path. The listener accepts a single connection,
    /// drains the request to EOF (the client shutdown()s its write half),
    /// writes the canned reply, and closes.
    async fn one_shot_port(
        rc: &str,
        body: &str,
    ) -> (std::path::PathBuf, tokio::task::JoinHandle<()>) {
        let dir = std::env::temp_dir();
        // Unique-ish name without Math.random: pid + a monotonic-ish nanos.
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let path = dir.join(format!("cosmix-cpt-{}-{}.sock", std::process::id(), nanos));
        let _ = std::fs::remove_file(&path);
        let listener = UnixListener::bind(&path).unwrap();
        let (rc, body) = (rc.to_string(), body.to_string());
        let handle = tokio::spawn(async move {
            if let Ok((mut stream, _)) = listener.accept().await {
                let mut buf = Vec::new();
                let mut limited = (&mut stream).take(64 * 1024);
                let _ = limited.read_to_end(&mut buf).await;
                let mut reply = BusMessage::new();
                reply.set("rc", &rc);
                reply.body = body;
                let _ = stream.write_all(&reply.to_bytes()).await;
                let _ = stream.shutdown().await;
            }
        });
        (path, handle)
    }

    #[tokio::test]
    async fn typed_success_is_ok_value() {
        let (path, h) = one_shot_port("0", r#"{"pong":true}"#).await;
        let out = call_port_typed(path.to_str().unwrap(), "ping", serde_json::Value::Null)
            .await
            .expect("transport ok");
        match out {
            PortReply::Ok { value, .. } => assert_eq!(value, serde_json::json!({"pong": true})),
            other => panic!("expected Ok, got {other:?}"),
        }
        h.await.unwrap();
        let _ = std::fs::remove_file(&path);
    }

    #[tokio::test]
    async fn typed_app_error_preserves_rc_not_err() {
        // A peer rc>=10 reply is an APPLICATION error → Ok(AppError), NOT the
        // transport Err (the whole point of the typed variant).
        let (path, h) = one_shot_port("10", r#"{"error":"boom"}"#).await;
        let out = call_port_typed(path.to_str().unwrap(), "do", serde_json::Value::Null)
            .await
            .expect("app error must NOT be a transport Err");
        match out {
            PortReply::AppError { rc, message } => {
                assert_eq!(rc, 10);
                assert!(message.contains("boom"), "message: {message}");
            }
            other => panic!("expected AppError, got {other:?}"),
        }
        h.await.unwrap();
        let _ = std::fs::remove_file(&path);
    }

    #[tokio::test]
    async fn typed_connect_refused_is_transport_err() {
        // No socket at this path → a pure transport failure → Err.
        let path = std::env::temp_dir().join("cosmix-cpt-nonexistent.sock");
        let _ = std::fs::remove_file(&path);
        let r = call_port_typed(path.to_str().unwrap(), "x", serde_json::Value::Null).await;
        assert!(r.is_err(), "a missing socket must be a transport Err");
    }

    #[tokio::test]
    async fn compat_call_port_collapses_app_error_to_err() {
        // The legacy wrapper still turns an app rc>=10 into an Err (message).
        let (path, h) = one_shot_port("10", r#"{"error":"legacy"}"#).await;
        let e = call_port(path.to_str().unwrap(), "do", serde_json::Value::Null)
            .await
            .unwrap_err()
            .to_string();
        assert!(e.contains("legacy"), "err: {e}");
        h.await.unwrap();
        let _ = std::fs::remove_file(&path);
    }
}
