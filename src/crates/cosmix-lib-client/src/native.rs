//! Native (non-WASM) hub client using tokio-tungstenite.

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;

use anyhow::{Context, Result};
use cosmix_amp::amp::{self, AmpMessage};
use futures_util::stream::SplitSink;
use futures_util::{SinkExt, StreamExt};
use tokio::net::TcpStream;
use tokio::sync::{mpsc, oneshot, Mutex};
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::{MaybeTlsStream, WebSocketStream};

use crate::types::IncomingCommand;

/// Default hub WebSocket URL.
pub const DEFAULT_HUB_URL: &str = "ws://localhost:4200/ws";

type WsSink = SplitSink<WebSocketStream<MaybeTlsStream<TcpStream>>, Message>;
type PendingMap = HashMap<String, oneshot::Sender<AmpMessage>>;

/// AMP WebSocket client for communicating with cosmix-hub.
pub struct HubClient {
    service_name: String,
    sink: Arc<Mutex<WsSink>>,
    pending: Arc<Mutex<PendingMap>>,
    incoming_rx: Mutex<Option<mpsc::UnboundedReceiver<IncomingCommand>>>,
    next_id: AtomicU64,
    connected: Arc<AtomicBool>,
}

impl HubClient {
    /// Connect to the hub at the given URL and register as a named service.
    pub async fn connect(service_name: &str, hub_url: &str) -> Result<Self> {
        let (ws_stream, _) = tokio_tungstenite::connect_async(hub_url)
            .await
            .context("failed to connect to hub")?;

        let (sink, stream) = ws_stream.split();
        let sink = Arc::new(Mutex::new(sink));
        let pending: Arc<Mutex<PendingMap>> = Arc::new(Mutex::new(HashMap::new()));
        let connected = Arc::new(AtomicBool::new(true));
        let (incoming_tx, incoming_rx) = mpsc::unbounded_channel();

        // Spawn the reader task
        let reader_pending = pending.clone();
        let reader_connected = connected.clone();
        let reader_service = service_name.to_string();
        tokio::spawn(Self::reader_loop(
            stream,
            reader_pending,
            incoming_tx,
            reader_connected,
            reader_service,
        ));

        let client = Self {
            service_name: service_name.to_string(),
            sink,
            pending,
            incoming_rx: Mutex::new(Some(incoming_rx)),
            next_id: AtomicU64::new(1),
            connected,
        };

        // Register with the hub
        client.register().await?;

        Ok(client)
    }

    /// Connect to the hub at the default URL (`ws://localhost:4200/ws`).
    pub async fn connect_default(service_name: &str) -> Result<Self> {
        Self::connect(service_name, DEFAULT_HUB_URL).await
    }

    /// Connect to the hub without registering a service name.
    ///
    /// Useful for GUI clients that only make `call()` requests (e.g. WASM apps
    /// or desktop monitors that don't need to receive incoming commands).
    pub async fn connect_anonymous(hub_url: &str) -> Result<Self> {
        let (ws_stream, _) = tokio_tungstenite::connect_async(hub_url)
            .await
            .context("failed to connect to hub")?;

        let (sink, stream) = ws_stream.split();
        let sink = Arc::new(Mutex::new(sink));
        let pending: Arc<Mutex<PendingMap>> = Arc::new(Mutex::new(HashMap::new()));
        let connected = Arc::new(AtomicBool::new(true));
        let (incoming_tx, incoming_rx) = mpsc::unbounded_channel();

        let reader_pending = pending.clone();
        let reader_connected = connected.clone();
        tokio::spawn(Self::reader_loop(
            stream,
            reader_pending,
            incoming_tx,
            reader_connected,
            "anonymous".to_string(),
        ));

        Ok(Self {
            service_name: "anonymous".to_string(),
            sink,
            pending,
            incoming_rx: Mutex::new(Some(incoming_rx)),
            next_id: AtomicU64::new(1),
            connected,
        })
    }

    /// Connect anonymously to the default hub URL.
    pub async fn connect_anonymous_default() -> Result<Self> {
        Self::connect_anonymous(DEFAULT_HUB_URL).await
    }

    /// Send a command to another service and wait for the response.
    pub async fn call(
        &self,
        to: &str,
        command: &str,
        args: serde_json::Value,
    ) -> Result<serde_json::Value> {
        let id = self.next_id.fetch_add(1, Ordering::Relaxed).to_string();

        let (tx, rx) = oneshot::channel();
        self.pending.lock().await.insert(id.clone(), tx);

        let mut msg = AmpMessage::new()
            .with_header("command", command)
            .with_header("from", &self.service_name)
            .with_header("to", to)
            .with_header("type", "request")
            .with_header("id", &id);

        if !args.is_null() {
            msg.body = serde_json::to_string(&args)?;
        }

        self.send_raw(&msg).await?;

        let response = rx.await.context("hub connection closed before response")?;

        // Check for error
        if let Some(rc) = response.get("rc") {
            let rc: u8 = rc.parse().unwrap_or(0);
            if rc >= 10 {
                let error = response
                    .get("error")
                    .unwrap_or("unknown error");
                anyhow::bail!("{error}");
            }
        }

        if response.body.is_empty() {
            Ok(serde_json::Value::Null)
        } else {
            Ok(serde_json::from_str(&response.body)?)
        }
    }

    /// Send a fire-and-forget message to another service.
    pub async fn send(
        &self,
        to: &str,
        command: &str,
        args: serde_json::Value,
    ) -> Result<()> {
        let mut msg = AmpMessage::new()
            .with_header("command", command)
            .with_header("from", &self.service_name)
            .with_header("to", to)
            .with_header("type", "request");

        if !args.is_null() {
            msg.body = serde_json::to_string(&args)?;
        }

        self.send_raw(&msg).await
    }

    /// Send a response to an incoming command.
    pub async fn respond(
        &self,
        cmd: &IncomingCommand,
        rc: u8,
        body: &str,
    ) -> Result<()> {
        let mut msg = AmpMessage::new()
            .with_header("command", &cmd.command)
            .with_header("from", &self.service_name)
            .with_header("to", &cmd.from)
            .with_header("type", "response")
            .with_header("rc", &rc.to_string());

        if let Some(ref id) = cmd.id {
            msg = msg.with_header("id", id);
        }

        if !body.is_empty() {
            msg.body = body.to_string();
        }

        self.send_raw(&msg).await
    }

    /// Take the receiver for incoming commands from other services.
    ///
    /// Can only be called once; subsequent calls return `None`.
    pub fn incoming(&self) -> Option<mpsc::UnboundedReceiver<IncomingCommand>> {
        self.incoming_rx.blocking_lock().take()
    }

    /// Take the receiver for incoming commands (async version).
    pub async fn incoming_async(&self) -> Option<mpsc::UnboundedReceiver<IncomingCommand>> {
        self.incoming_rx.lock().await.take()
    }

    /// List all services registered on the hub.
    pub async fn list_services(&self) -> Result<Vec<String>> {
        let result = self
            .call("hub", "hub.list", serde_json::Value::Null)
            .await?;

        let services: Vec<String> = serde_json::from_value(result)?;
        Ok(services)
    }

    /// Check if the hub connection is alive.
    pub fn is_connected(&self) -> bool {
        self.connected.load(Ordering::Relaxed)
    }

    // ── Internal ──

    async fn register(&self) -> Result<()> {
        self.call("hub", "hub.register", serde_json::Value::Null).await?;
        Ok(())
    }

    async fn send_raw(&self, msg: &AmpMessage) -> Result<()> {
        let wire = msg.to_wire();
        self.sink
            .lock()
            .await
            .send(Message::Text(wire.into()))
            .await
            .context("failed to send message to hub")?;
        Ok(())
    }

    async fn reader_loop(
        mut stream: futures_util::stream::SplitStream<
            WebSocketStream<MaybeTlsStream<TcpStream>>,
        >,
        pending: Arc<Mutex<PendingMap>>,
        incoming_tx: mpsc::UnboundedSender<IncomingCommand>,
        connected: Arc<AtomicBool>,
        service_name: String,
    ) {
        while let Some(result) = stream.next().await {
            let data = match result {
                Ok(Message::Text(text)) => text.to_string(),
                Ok(Message::Close(_)) => break,
                Ok(Message::Ping(_)) => continue,
                Ok(_) => continue,
                Err(e) => {
                    tracing::warn!("{service_name}: WebSocket error: {e}");
                    break;
                }
            };

            let msg = match amp::parse(&data) {
                Ok(m) => m,
                Err(e) => {
                    tracing::debug!("{service_name}: failed to parse AMP message: {e}");
                    continue;
                }
            };

            let msg_id = msg.get("id").map(|s| s.to_string());
            let msg_type = msg.get("type").unwrap_or("unknown");
            let msg_cmd = msg.get("command").unwrap_or("?");
            let msg_from = msg.get("from").unwrap_or("?");
            eprintln!("[hub-client:{service_name}] recv type={msg_type} cmd={msg_cmd} from={msg_from} id={msg_id:?}");

            // If message is a response with an id that matches a pending request, resolve it
            let is_response = if msg.get("type").is_some_and(|t| t == "response") {
                if let Some(ref id) = msg_id {
                    let mut p = pending.lock().await;
                    if let Some(tx) = p.remove(id) {
                        let _ = tx.send(msg.clone());
                        true
                    } else {
                        false
                    }
                } else {
                    false
                }
            } else {
                false
            };

            if !is_response && msg.get("command").is_some() {
                let cmd = IncomingCommand {
                    from: msg.get("from").unwrap_or("").to_string(),
                    command: msg.get("command").unwrap_or("").to_string(),
                    id: msg_id,
                    args: if msg.body.is_empty() {
                        serde_json::Value::Null
                    } else {
                        serde_json::from_str(&msg.body).unwrap_or(serde_json::Value::Null)
                    },
                    body: msg.body.clone(),
                };
                if incoming_tx.send(cmd).is_err() {
                    tracing::debug!("{service_name}: incoming channel closed");
                    break;
                }
            }
        }

        connected.store(false, Ordering::Relaxed);
        tracing::info!("{service_name}: disconnected from hub");

        // Resolve all pending requests with an error
        let mut pending = pending.lock().await;
        pending.clear();
    }
}
