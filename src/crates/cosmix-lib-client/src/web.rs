//! WASM broker client using gloo-net WebSocket.
//!
//! Call-only client for browser apps — no incoming command handling.

use std::cell::RefCell;
use std::collections::HashMap;
use std::rc::Rc;
use std::sync::atomic::{AtomicU64, Ordering};

use anyhow::{Context, Result};
use cosmix_bus::bus::{self, BusMessage};
use futures_util::{SinkExt, StreamExt};
use gloo_net::websocket::Message;
use gloo_net::websocket::futures::WebSocket;

type PendingMap = HashMap<String, futures_channel::oneshot::Sender<BusMessage>>;

/// RAII removal guard for an entry in [`NodedClient::pending`].
///
/// Mirrors the native-side [`super::native::PendingGuard`] doc invariant:
/// the slot must be removed on **every** exit path that doesn't consume
/// it — including an outer-future drop (SPEC 18 Phase 2 WS4 per-`send`
/// `timeout=<sec>` wraps `call()` in `tokio::time::timeout`, and on
/// elapsed the inner future is dropped without running any cleanup
/// arm). `Rc<RefCell<_>>` is single-thread-WASM so the borrow inside
/// `Drop` is sound — every existing access is a brief
/// `insert`/`remove`/`get`/`clear` with no `.await` inside the borrow.
struct PendingGuard {
    pending: Rc<RefCell<PendingMap>>,
    id: Option<String>,
}
impl PendingGuard {
    fn arm(pending: Rc<RefCell<PendingMap>>, id: String) -> Self {
        Self {
            pending,
            id: Some(id),
        }
    }
    /// Mark the entry as already-consumed by the response path so
    /// `Drop` doesn't borrow for a no-op `remove`.
    fn disarm(&mut self) {
        self.id = None;
    }
}
impl Drop for PendingGuard {
    fn drop(&mut self) {
        if let Some(id) = self.id.take() {
            // `borrow_mut()` rather than `try_borrow_mut()`: there is
            // no reentrant-drop scenario in the call path (Drop runs
            // after `rx.await` resolves or after the outer future is
            // dropped; no other code is holding a borrow at that
            // point). A live borrow at Drop time would be a structural
            // bug, and panicking surfaces it loudly instead of
            // silently skipping the cleanup the guard exists for.
            self.pending.borrow_mut().remove(&id);
        }
    }
}

/// Bus WebSocket client for WASM browser apps.
///
/// This is a call-only client — it can send requests and receive responses,
/// but does not register as a service or accept incoming commands.
pub struct NodedClient {
    sink: Rc<RefCell<Option<futures_util::stream::SplitSink<WebSocket, Message>>>>,
    pending: Rc<RefCell<PendingMap>>,
    next_id: AtomicU64,
    connected: Rc<RefCell<bool>>,
}

impl NodedClient {
    /// Derive broker WebSocket URL from the current page origin.
    ///
    /// `https://node:8443` → `wss://node:8443/ws`
    /// `http://node:8080` → `ws://node:8080/ws`
    pub fn noded_url_from_origin() -> Result<String> {
        let window = web_sys::window().context("no window object")?;
        let location = window.location();
        let protocol = location.protocol().unwrap_or_else(|_| "https:".to_string());
        let host = location
            .host()
            .unwrap_or_else(|_| "localhost:4200".to_string());

        let ws_scheme = if protocol.starts_with("https") {
            "wss"
        } else {
            "ws"
        };
        Ok(format!("{ws_scheme}://{host}/ws"))
    }

    /// Connect anonymously (call-only, no registration).
    pub fn connect_anonymous(noded_url: &str) -> Result<Self> {
        let ws = WebSocket::open(noded_url)
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
        let url = Self::noded_url_from_origin()?;
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
        // PendingGuard mirrors the native side: removes the parked
        // oneshot on every early-return path (incl. caller-future
        // drop), so an outer `tokio::time::timeout`-style cancellation
        // cannot leak the entry. Disarmed on the success path because
        // `reader_loop` already removed the entry when resolving the
        // oneshot.
        let mut guard = PendingGuard::arm(self.pending.clone(), id.clone());

        let mut msg = BusMessage::new()
            .with_header("command", command)
            .with_header("from", "browser")
            .with_header("to", to)
            .with_header("type", "request")
            .with_header("id", &id);

        if !args.is_null() {
            msg.body = serde_json::to_string(&args)?;
        }

        self.send_raw(&msg).await?;

        let response = rx
            .await
            .context("broker connection closed before response")?;
        guard.disarm();

        if let Some(rc) = response.get("rc") {
            let rc: u8 = rc.parse().unwrap_or(0);
            if rc >= 10 {
                anyhow::bail!("{}", response.error_message());
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

    async fn send_raw(&self, msg: &BusMessage) -> Result<()> {
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

            let msg = match bus::parse(&data) {
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
        tracing::info!("Disconnected from broker");
    }
}
