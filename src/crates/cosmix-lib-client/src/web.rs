//! WASM hub client using gloo-net WebSocket.
//!
//! Call-only client for browser apps — no incoming command handling.

use std::cell::RefCell;
use std::collections::HashMap;
use std::rc::Rc;
use std::sync::atomic::{AtomicU64, Ordering};

use anyhow::{Context, Result};
use cosmix_amp::amp::{self, AmpMessage};
use futures_util::{SinkExt, StreamExt};
use gloo_net::websocket::futures::WebSocket;
use gloo_net::websocket::Message;

type PendingMap = HashMap<String, futures_channel::oneshot::Sender<AmpMessage>>;

/// AMP WebSocket client for WASM browser apps.
///
/// This is a call-only client — it can send requests and receive responses,
/// but does not register as a service or accept incoming commands.
pub struct HubClient {
    sink: Rc<RefCell<Option<futures_util::stream::SplitSink<WebSocket, Message>>>>,
    pending: Rc<RefCell<PendingMap>>,
    next_id: AtomicU64,
    connected: Rc<RefCell<bool>>,
}

impl HubClient {
    /// Derive hub WebSocket URL from the current page origin.
    ///
    /// `https://node:8443` → `wss://node:8443/ws`
    /// `http://node:8080` → `ws://node:8080/ws`
    pub fn hub_url_from_origin() -> Result<String> {
        let window = web_sys::window().context("no window object")?;
        let location = window.location();
        let protocol = location.protocol().unwrap_or_else(|_| "https:".to_string());
        let host = location.host().unwrap_or_else(|_| "localhost:4200".to_string());

        let ws_scheme = if protocol.starts_with("https") { "wss" } else { "ws" };
        Ok(format!("{ws_scheme}://{host}/ws"))
    }

    /// Connect anonymously (call-only, no registration).
    pub fn connect_anonymous(hub_url: &str) -> Result<Self> {
        let ws = WebSocket::open(hub_url)
            .map_err(|e| anyhow::anyhow!("WebSocket connect failed: {e}"))?;

        let (sink, stream) = ws.split();
        let sink = Rc::new(RefCell::new(Some(sink)));
        let pending: Rc<RefCell<PendingMap>> = Rc::new(RefCell::new(HashMap::new()));
        let connected = Rc::new(RefCell::new(true));

        // Spawn reader loop
        let reader_pending = pending.clone();
        let reader_connected = connected.clone();
        wasm_bindgen_futures::spawn_local(Self::reader_loop(
            stream,
            reader_pending,
            reader_connected,
        ));

        Ok(Self {
            sink,
            pending,
            next_id: AtomicU64::new(1),
            connected,
        })
    }

    /// Connect anonymously using URL derived from page origin.
    pub fn connect_anonymous_default() -> Result<Self> {
        let url = Self::hub_url_from_origin()?;
        Self::connect_anonymous(&url)
    }

    /// Send a command to a service and wait for the response.
    pub async fn call(
        &self,
        to: &str,
        command: &str,
        args: serde_json::Value,
    ) -> Result<serde_json::Value> {
        let id = self.next_id.fetch_add(1, Ordering::Relaxed).to_string();

        let (tx, rx) = futures_channel::oneshot::channel();
        self.pending.borrow_mut().insert(id.clone(), tx);

        let mut msg = AmpMessage::new()
            .with_header("command", command)
            .with_header("from", "browser")
            .with_header("to", to)
            .with_header("type", "request")
            .with_header("id", &id);

        if !args.is_null() {
            msg.body = serde_json::to_string(&args)?;
        }

        self.send_raw(&msg).await?;

        let response = rx.await.context("hub connection closed before response")?;

        if let Some(rc) = response.get("rc") {
            let rc: u8 = rc.parse().unwrap_or(0);
            if rc >= 10 {
                let error = response.get("error").unwrap_or("unknown error");
                anyhow::bail!("{error}");
            }
        }

        if response.body.is_empty() {
            Ok(serde_json::Value::Null)
        } else {
            Ok(serde_json::from_str(&response.body)?)
        }
    }

    /// Check if still connected.
    pub fn is_connected(&self) -> bool {
        *self.connected.borrow()
    }

    // ── Internal ──

    async fn send_raw(&self, msg: &AmpMessage) -> Result<()> {
        let wire = msg.to_wire();
        let mut sink_ref = self.sink.borrow_mut();
        let sink = sink_ref.as_mut().context("WebSocket sink not available")?;
        sink.send(Message::Text(wire))
            .await
            .map_err(|e| anyhow::anyhow!("failed to send: {e}"))?;
        Ok(())
    }

    async fn reader_loop(
        mut stream: futures_util::stream::SplitStream<WebSocket>,
        pending: Rc<RefCell<PendingMap>>,
        connected: Rc<RefCell<bool>>,
    ) {
        while let Some(result) = stream.next().await {
            let data = match result {
                Ok(Message::Text(text)) => text,
                Ok(Message::Bytes(_)) => continue,
                Err(e) => {
                    tracing::warn!("WebSocket error: {e}");
                    break;
                }
            };

            let msg = match amp::parse(&data) {
                Ok(m) => m,
                Err(_) => continue,
            };

            // Match responses to pending requests by id
            if let Some(id) = msg.get("id") {
                let mut p = pending.borrow_mut();
                if let Some(tx) = p.remove(id) {
                    let _ = tx.send(msg);
                }
            }
        }

        *connected.borrow_mut() = false;
        tracing::info!("Disconnected from hub");
    }
}
