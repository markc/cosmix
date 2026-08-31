//! Native (non-WASM) noded client using tokio-tungstenite.
//!
//! `lib-client` is bus-bound (will move to `markc/bus` at extraction
//! time) and must NOT depend on `cosmix-lib-config` (which stays in
//! cos) — that's the dep direction the bus-cos extraction plan
//! enforces. The previous `resolve_noded_url()` /
//! `connect_default()` / `connect_anonymous_default()` convenience
//! helpers depended on `cosmix-lib-config::node::load_node_config()`
//! for broker URL discovery. As of 2026-05-28 pre-extraction step 2,
//! those helpers move to `cosmix_config::client_helpers` (gated under
//! lib-config's opt-in `client-helpers` Cargo feature). lib-client
//! retains only the explicit-URL primitives `NodedClient::connect()`
//! and `NodedClient::connect_anonymous()`.

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex as StdMutex, RwLock};

use anyhow::{Context, Result};
use cosmix_bus::bus::{self, BusMessage};
use futures_util::stream::SplitSink;
use futures_util::{SinkExt, StreamExt};
use tokio::net::TcpStream;
use tokio::sync::{Mutex, mpsc, oneshot};
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::{MaybeTlsStream, WebSocketStream};

use crate::types::IncomingCommand;

type WsSink = SplitSink<WebSocketStream<MaybeTlsStream<TcpStream>>, Message>;
type PendingMap = HashMap<String, oneshot::Sender<BusMessage>>;

/// Abort a spawned task if construction is cancelled before ownership of the
/// task has safely transferred to the returned client.
struct AbortOnDrop {
    handle: Option<tokio::task::AbortHandle>,
}

impl AbortOnDrop {
    fn new(handle: tokio::task::AbortHandle) -> Self {
        Self {
            handle: Some(handle),
        }
    }

    fn disarm(&mut self) {
        self.handle = None;
    }
}

impl Drop for AbortOnDrop {
    fn drop(&mut self) {
        if let Some(handle) = self.handle.take() {
            handle.abort();
        }
    }
}

/// RAII removal guard for an entry in [`NodedClient::pending`].
///
/// Inserting a `(id, oneshot::Sender)` entry into the pending-request map
/// is the second of two coupled effects (the first is the outbound
/// `send_raw`). The slot must be removed on **every** exit path that
/// does NOT consume it — including a cancellation that drops the
/// awaiting `call()` future mid-flight (SPEC 18 Phase 2 WS4 per-`send`
/// `timeout=<sec>` wraps `call()` in `tokio::time::timeout`, and on
/// elapsed the `call()` future is dropped without running any of its
/// `?`/`bail!` cleanup arms). Without RAII the entry would survive
/// until either a late broker reply happened to land with that id
/// (auto-removed by `reader_loop`) or the connection closed
/// (`reader_loop` clears the map on exit) — both unbounded waits for a
/// downstream that may never reply.
///
/// `pending` is intentionally a `std::sync::Mutex` (not
/// `tokio::sync::Mutex`) so this Drop can lock synchronously without
/// needing an executor. Every existing access pattern is brief
/// (`insert` / `remove` / `get` / `clear`); none hold the guard across
/// an `.await`.
struct PendingGuard {
    pending: Arc<StdMutex<PendingMap>>,
    id: Option<String>,
}
impl PendingGuard {
    fn arm(pending: Arc<StdMutex<PendingMap>>, id: String) -> Self {
        Self {
            pending,
            id: Some(id),
        }
    }
    /// Mark the entry as already-consumed by the response path so
    /// `Drop` doesn't acquire the lock for a no-op `remove`. Safe to
    /// skip — the no-op `remove` on a missing key is cheap.
    fn disarm(&mut self) {
        self.id = None;
    }
}
impl Drop for PendingGuard {
    fn drop(&mut self) {
        if let Some(id) = self.id.take() {
            // Recover from poisoning rather than silently dropping the
            // cleanup — the guard's whole point is "the entry MUST be
            // removed on every exit path". A panicked prior holder
            // leaves the map structurally fine; the data is valid
            // because every access is a short insert/remove/get/clear
            // that cannot leave a partial mutation behind.
            let mut p = match self.pending.lock() {
                Ok(g) => g,
                Err(poisoned) => poisoned.into_inner(),
            };
            p.remove(&id);
        }
    }
}

/// Bus WebSocket client for communicating with cosmix-noded.
pub struct NodedClient {
    service_name: RwLock<String>,
    sink: Arc<Mutex<WsSink>>,
    pending: Arc<StdMutex<PendingMap>>,
    incoming_rx: Mutex<Option<mpsc::UnboundedReceiver<IncomingCommand>>>,
    next_id: AtomicU64,
    connected: Arc<AtomicBool>,
    /// Join handle for the detached reader task. The task owns the read
    /// half of the split stream, so dropping a `NodedClient` does **not**
    /// close the socket — [`close`](Self::close) aborts this to make
    /// teardown deterministic.
    reader_handle: Mutex<Option<tokio::task::JoinHandle<()>>>,
    /// Build provenance sent in the `noded.register` body (version /
    /// git_sha / build_time / pid / …). `None` for anonymous clients and
    /// for citizens that don't supply it; re-sent on every `register()`
    /// so a reconnect re-publishes it.
    provenance: Option<cosmix_bus::RegisterProvenance>,
}

/// The requested Bus service name is already owned by another connection.
///
/// `cosmix-noded` reports this as the sole application error from
/// `noded.register`. Keeping it as a concrete error in the `anyhow` source
/// chain lets resident clients treat a collision as terminal without parsing
/// the broker's diagnostic text.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NameCollision {
    service: String,
}

impl NameCollision {
    fn new(service: String) -> Self {
        Self { service }
    }

    /// The service name whose registration was rejected.
    pub fn service(&self) -> &str {
        &self.service
    }
}

impl std::fmt::Display for NameCollision {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "Bus service name '{}' is already registered",
            self.service
        )
    }
}

impl std::error::Error for NameCollision {}

impl NodedClient {
    /// Connect to the broker at the given URL and register as a named service.
    pub async fn connect(service_name: &str, noded_url: &str) -> Result<Self> {
        Self::connect_with_provenance(service_name, noded_url, None).await
    }

    /// Connect and register as a named service, sending `provenance`
    /// (version / git_sha / build_time / pid / …) in the `noded.register`
    /// body so it surfaces in `noded.list` / `noded.info`. The provenance
    /// is re-sent on every `register()`, so a supervised reconnect
    /// re-publishes it (build the value ONCE at process start and clone
    /// it in, so `started_at` stays the true process start). Version-
    /// discovery contract.
    pub async fn connect_with_provenance(
        service_name: &str,
        noded_url: &str,
        provenance: Option<cosmix_bus::RegisterProvenance>,
    ) -> Result<Self> {
        let (ws_stream, _) = tokio_tungstenite::connect_async(noded_url)
            .await
            .context("failed to connect to broker")?;

        let (sink, stream) = ws_stream.split();
        let sink = Arc::new(Mutex::new(sink));
        let pending: Arc<StdMutex<PendingMap>> = Arc::new(StdMutex::new(HashMap::new()));
        let connected = Arc::new(AtomicBool::new(true));
        let (incoming_tx, incoming_rx) = mpsc::unbounded_channel();

        // Spawn the reader task
        let reader_pending = pending.clone();
        let reader_connected = connected.clone();
        let reader_service = service_name.to_string();
        let reader_handle = tokio::spawn(Self::reader_loop(
            stream,
            reader_pending,
            incoming_tx,
            reader_connected,
            reader_service,
        ));
        // `register()` awaits a broker response after the reader has taken the
        // socket's read half. If an outer timeout drops this connect future,
        // the half-built `NodedClient` alone cannot abort that detached reader.
        // Keep an independent abort handle armed until registration succeeds.
        let mut reader_guard = AbortOnDrop::new(reader_handle.abort_handle());

        let client = Self {
            service_name: RwLock::new(service_name.to_string()),
            sink,
            pending,
            incoming_rx: Mutex::new(Some(incoming_rx)),
            next_id: AtomicU64::new(1),
            connected,
            reader_handle: Mutex::new(Some(reader_handle)),
            provenance,
        };

        // Register with the broker. On failure the detached reader
        // task already owns the socket, so a bare `?` early-return
        // would leak a live (broker-side) connection plus its reader
        // task — and an unbounded supervisor retry against a
        // persistent register failure (e.g. a name collision that
        // real `cosmix-noded` answers rc=10 *without* hanging up)
        // would accumulate them. Tear the half-built client down
        // explicitly before surfacing the error.
        if let Err(e) = client.register().await {
            client.close().await;
            return Err(e);
        }

        reader_guard.disarm();

        Ok(client)
    }

    /// Connect to the broker without registering a service name.
    ///
    /// Useful for GUI clients that only make `call()` requests (e.g. WASM apps
    /// or desktop monitors that don't need to receive incoming commands).
    pub async fn connect_anonymous(noded_url: &str) -> Result<Self> {
        let (ws_stream, _) = tokio_tungstenite::connect_async(noded_url)
            .await
            .context("failed to connect to broker")?;

        let (sink, stream) = ws_stream.split();
        let sink = Arc::new(Mutex::new(sink));
        let pending: Arc<StdMutex<PendingMap>> = Arc::new(StdMutex::new(HashMap::new()));
        let connected = Arc::new(AtomicBool::new(true));
        let (incoming_tx, incoming_rx) = mpsc::unbounded_channel();

        let reader_pending = pending.clone();
        let reader_connected = connected.clone();
        let reader_handle = tokio::spawn(Self::reader_loop(
            stream,
            reader_pending,
            incoming_tx,
            reader_connected,
            "anonymous".to_string(),
        ));

        Ok(Self {
            service_name: RwLock::new("anonymous".to_string()),
            sink,
            pending,
            incoming_rx: Mutex::new(Some(incoming_rx)),
            next_id: AtomicU64::new(1),
            connected,
            reader_handle: Mutex::new(Some(reader_handle)),
            provenance: None,
        })
    }

    /// Read the current service name.
    fn name(&self) -> String {
        self.service_name.read().unwrap().clone()
    }

    /// Re-register this client under a new service name on the broker.
    ///
    /// Updates the client's identity so future outbound messages carry
    /// the new name in their `from` header, and the broker routes messages
    /// addressed to that name back to this connection.
    pub async fn register_as(&self, name: &str) -> Result<()> {
        *self.service_name.write().unwrap() = name.to_string();
        self.register().await
    }

    /// Send a command to another service and wait for the response.
    pub async fn call(
        &self,
        to: &str,
        command: &str,
        args: serde_json::Value,
    ) -> Result<serde_json::Value> {
        // Compat wrapper: an application `rc >= 10` reply collapses back into
        // an `Err` (its message), preserving the historic conflated contract.
        // Callers that must distinguish transport from application errors use
        // `call_typed` (the Mix `$rc`-band contract).
        match self.call_typed(to, command, args).await? {
            cosmix_bus::PortReply::Ok { value, .. } => Ok(value),
            cosmix_bus::PortReply::AppError { message, .. } => anyhow::bail!("{}", message),
        }
    }

    /// Like [`call`], but the `Result::Err` is ONLY a transport failure
    /// (send_raw, broker-close, 60s timeout) — a peer reply with `rc >= 10`
    /// is `Ok(PortReply::AppError { rc, message })`, so a caller can map
    /// transport → `$rc = -1` and application error → `$rc = rc` distinctly.
    pub async fn call_typed(
        &self,
        to: &str,
        command: &str,
        args: serde_json::Value,
    ) -> Result<cosmix_bus::PortReply> {
        let id = self.next_id.fetch_add(1, Ordering::Relaxed).to_string();

        // Serialize the body BEFORE registering in `pending` — a
        // serialization failure here would otherwise park a `oneshot`
        // sender that the response path can never consume, leaking the
        // slot until disconnect cleanup.
        let body = if args.is_null() {
            String::new()
        } else {
            serde_json::to_string(&args)?
        };

        let (tx, rx) = oneshot::channel();
        // Insert + arm the RAII guard in one move. Every early-return
        // path below — `send_raw` error, broker-close, internal 60s
        // timeout — used to call `self.pending.lock().await.remove(&id)`
        // explicitly; now the guard's `Drop` covers them, AND also
        // covers the previously-uncovered case of an outer caller
        // dropping this future mid-`rx`-await (SPEC 18 WS4 per-`send`
        // `timeout=<sec>` wraps this in `tokio::time::timeout`, which
        // drops the inner future on elapsed without running any of
        // these arms). The success path calls `guard.disarm()` so the
        // drop is a no-op — the response path already removed the
        // entry via `reader_loop`.
        self.pending
            .lock()
            .expect("pending mutex poisoned")
            .insert(id.clone(), tx);
        let mut guard = PendingGuard::arm(self.pending.clone(), id.clone());

        let mut msg = BusMessage::new()
            .with_header("command", command)
            .with_header("from", &self.name())
            .with_header("to", to)
            .with_header("type", "request")
            .with_header("id", &id);

        if !body.is_empty() {
            msg.body = body;
        }

        // `send_raw` can fail before the broker ever sees the request
        // (e.g. WS sink closed mid-send). The PendingGuard's Drop
        // removes the parked oneshot on the early-return so it doesn't
        // accumulate until disconnect cleanup.
        self.send_raw(&msg).await?;

        // Timeout is a safety net — the broker returns instant errors for
        // unknown services/nodes. The 30s mesh peer timeout covers remote
        // targets. This 60s client timeout only fires if the broker itself
        // is unresponsive.
        let response = match tokio::time::timeout(std::time::Duration::from_secs(60), rx).await {
            Ok(Ok(resp)) => resp,
            Ok(Err(_)) => anyhow::bail!("broker connection closed before response"),
            Err(_) => anyhow::bail!("send to '{to}' timed out after 60s"),
        };

        // Response received — `reader_loop` already removed the pending
        // entry as part of resolving the oneshot. Disarm to avoid a
        // pointless lock + no-op remove in Drop.
        guard.disarm();

        // A peer `rc >= 10` is an APPLICATION error (a real status), NOT a
        // transport failure — surface it as `Ok(AppError)` so the caller keeps
        // the rc (was an `Err`, conflating it with a broken connection).
        if let Some(rc) = response.get("rc") {
            let rc: u8 = rc.parse().unwrap_or(0);
            if rc >= 10 {
                return Ok(cosmix_bus::PortReply::AppError {
                    rc,
                    message: response.error_message(),
                });
            }
        }

        // Success rc (0 or RC_WARNING) — carry the exact rc so a warning is
        // not flattened to 0.
        let rc: u8 = response.get("rc").and_then(|s| s.parse().ok()).unwrap_or(0);
        let value = if response.body.is_empty() {
            serde_json::Value::Null
        } else {
            // Most cosmix services return JSON bodies, but `spec.get` and
            // similar surface-the-body-verbatim commands return arbitrary
            // payloads (e.g. markdown). Fall back to a String value rather
            // than bailing — callers that need parsed JSON can serde-decode
            // the string themselves.
            serde_json::from_str(&response.body)
                .unwrap_or_else(|_| serde_json::Value::String(response.body.clone()))
        };
        Ok(cosmix_bus::PortReply::Ok { rc, value })
    }

    /// Like [`call`] but with caller-supplied headers and an optional
    /// caller-supplied body. Used by callers whose target verb reads
    /// parameters from headers (e.g. `noded.props.subscribe_grant`'s
    /// `topic`/`target_peer`/`namespace`) rather than the JSON-args
    /// body. A non-empty `body` is forwarded verbatim — most callers
    /// pre-serialise a JSON object (e.g. `<svc>.props.set` value).
    /// Callers that need the simpler `call`-style "serialise this
    /// args value" shape should keep using [`call`].
    ///
    /// The fire-and-forget [`send_with_headers`] cannot be used as a
    /// substitute for this method — it doesn't register a `pending`
    /// slot, so the broker's response is dropped on the floor.
    ///
    /// Framing headers (`command`, `from`, `to`, `type`, `id`) are
    /// applied *after* caller-supplied headers so caller entries cannot
    /// override routing/identity. This is a defense-in-depth guard
    /// against accidental misuse: a future caller's stray `from`
    /// header could otherwise spoof a routing identity.
    ///
    /// On `rc >= 10` this method bails with an `anyhow!` carrying — in
    /// preference order — the body's `message` JSON field, the body's
    /// `error` JSON field, the response's `error` header (used by
    /// header-only responders like `noded.props.subscribe_grant`),
    /// or `rc=N (no error body)` if nothing structured is available.
    /// Callers that need to inspect the response body even on error
    /// (partial results, error-code-based branching) should use
    /// [`call_with_headers_raw`] instead.
    ///
    /// [`call`]: Self::call
    /// [`send_with_headers`]: Self::send_with_headers
    /// [`call_with_headers_raw`]: Self::call_with_headers_raw
    pub async fn call_with_headers(
        &self,
        to: &str,
        command: &str,
        headers: &std::collections::BTreeMap<String, String>,
        body: &str,
    ) -> Result<serde_json::Value> {
        let (rc, body_str, error_header) = self
            .call_with_headers_raw(to, command, headers, body)
            .await?;
        if rc >= 10 {
            // Error-message precedence:
            //   1. body's structured `message` field (SPEC 12 §9 —
            //      `err_with` in `cosmix-lib-props-store/src/bus/mutation.rs`)
            //   2. body's `error` field (other services' convention)
            //   3. response `error` header (noded.props.subscribe_grant
            //      and other header-only responders return errors here
            //      with empty bodies — `_resolve_grant_failed`
            //      `target_peer_not_connected` etc.)
            //   4. `rc=N (no error body)` sentinel
            let parsed: Option<serde_json::Value> = if body_str.is_empty() {
                None
            } else {
                serde_json::from_str(&body_str).ok()
            };
            let from_body = parsed
                .as_ref()
                .and_then(|v| v.get("message").or_else(|| v.get("error")))
                .and_then(|v| v.as_str())
                .map(str::to_string);
            let msg = from_body.or(error_header).unwrap_or_else(|| {
                if body_str.is_empty() {
                    format!("rc={rc} (no error body)")
                } else {
                    body_str.clone()
                }
            });
            anyhow::bail!("{msg}");
        }
        if body_str.is_empty() {
            Ok(serde_json::Value::Null)
        } else {
            let parsed = serde_json::from_str(&body_str);
            Ok(parsed.unwrap_or(serde_json::Value::String(body_str)))
        }
    }

    /// Same wire path as [`call_with_headers`] but returns the raw
    /// `(rc, body, error_header)` triple without treating `rc >= 10`
    /// as a transport error. Use this when the response body is
    /// meaningful even on error rc:
    ///
    /// - `<svc>.props.delete` returns `{"error_code":"not_found", …}`
    ///   with `rc=10` — callers may want to special-case that vs. a
    ///   genuine storage failure.
    /// - `maild.accounts.seed_mailboxes` returns a `results: [...]`
    ///   array of per-account outcomes with `rc=10` when any element
    ///   failed; the body still contains every successful entry.
    ///
    /// The third element is the response's `error` header (if any).
    /// Some responders (notably `noded.props.subscribe_grant`) carry
    /// their error tokens in the header with an empty body. Callers
    /// that don't care can `let (rc, body, _) = …`.
    ///
    /// **Precedence rule for raw callers:** prefer the body when it
    /// is present and parseable — the SPEC 12 props surface puts the
    /// canonical `error_code` / `message` / structured detail there,
    /// and the header is a legacy carrier for responders that didn't
    /// emit a structured body. Reading the header first will hide the
    /// fine-grained token (e.g. `not_found` vs `version_mismatch`)
    /// behind a coarser sentinel.
    ///
    /// The body string is returned verbatim (not parsed). Transport
    /// failures (timeout, closed connection, send error) still bubble
    /// as `Err`.
    ///
    /// [`call_with_headers`]: Self::call_with_headers
    pub async fn call_with_headers_raw(
        &self,
        to: &str,
        command: &str,
        headers: &std::collections::BTreeMap<String, String>,
        body: &str,
    ) -> Result<(u8, String, Option<String>)> {
        let id = self.next_id.fetch_add(1, Ordering::Relaxed).to_string();

        let (tx, rx) = oneshot::channel();
        // See [`Self::call`] for the PendingGuard rationale — the same
        // cancellation-safety guarantee applies here. SPEC 18 WS4's
        // per-`send` `timeout=<sec>` wraps Mix `send` through this
        // method when the target expects headers; without the guard,
        // an outer-timeout drop would leak the pending slot.
        self.pending
            .lock()
            .expect("pending mutex poisoned")
            .insert(id.clone(), tx);
        let mut guard = PendingGuard::arm(self.pending.clone(), id.clone());

        let mut msg = BusMessage::new();
        for (k, v) in headers {
            msg = msg.with_header(k, v);
        }
        // Apply framing headers LAST — overrides any caller entry that
        // attempted to spoof routing/identity. See doc above.
        msg = msg
            .with_header("command", command)
            .with_header("from", &self.name())
            .with_header("to", to)
            .with_header("type", "request")
            .with_header("id", &id);
        if !body.is_empty() {
            msg.body = body.to_string();
        }

        self.send_raw(&msg).await?;

        let response = match tokio::time::timeout(std::time::Duration::from_secs(60), rx).await {
            Ok(Ok(resp)) => resp,
            Ok(Err(_)) => anyhow::bail!("broker connection closed before response"),
            Err(_) => anyhow::bail!("send to '{to}' timed out after 60s"),
        };
        guard.disarm();

        let rc: u8 = response.get("rc").and_then(|s| s.parse().ok()).unwrap_or(0);
        let error_header = response.get("error").map(str::to_string);
        Ok((rc, response.body, error_header))
    }

    /// Send a fire-and-forget message to another service.
    pub async fn send(&self, to: &str, command: &str, args: serde_json::Value) -> Result<()> {
        let mut msg = BusMessage::new()
            .with_header("command", command)
            .with_header("from", &self.name())
            .with_header("to", to)
            .with_header("type", "request");

        if !args.is_null() {
            msg.body = serde_json::to_string(&args)?;
        }

        self.send_raw(&msg).await
    }

    /// Send a message with explicit Bus headers and body (used by Mix scripting).
    pub async fn send_with_headers(
        &self,
        to: &str,
        command: &str,
        headers: &std::collections::BTreeMap<String, String>,
        body: &str,
    ) -> Result<()> {
        // Allocate an id even though we don't await a response — without one,
        // the broker's response carries no id, and any peer that sees an orphan
        // `type=response` may misroute it as a fresh command (closed-loop
        // amplification observed in indexd↔noded, 2026-05-04).
        let id = self.next_id.fetch_add(1, Ordering::Relaxed).to_string();

        let mut msg = BusMessage::new()
            .with_header("command", command)
            .with_header("from", &self.name())
            .with_header("to", to)
            .with_header("type", "request")
            .with_header("id", &id);

        for (k, v) in headers {
            msg = msg.with_header(k, v);
        }

        if !body.is_empty() {
            msg.body = body.to_string();
        }

        self.send_raw(&msg).await
    }

    /// Send a response to an incoming command.
    pub async fn respond(&self, cmd: &IncomingCommand, rc: u8, body: &str) -> Result<()> {
        self.respond_parts(&cmd.from, &cmd.command, cmd.id.as_deref(), rc, body)
            .await
    }

    /// Send a response from its correlation parts rather than a whole
    /// [`IncomingCommand`].
    ///
    /// This is the part-wise core that [`respond`](Self::respond) delegates
    /// to; the two produce byte-identical wire output (same headers, same
    /// order). It exists so callers holding only the correlation tuple —
    /// notably the SPEC 18 WS-R Mix `reply` path, whose neutral
    /// `cosmix-lib-mix` `BusHandler` boundary must not name this crate's
    /// `IncomingCommand` wire type — can answer a request without
    /// fabricating a synthetic `IncomingCommand`. Fabrication would be a
    /// latent partial-truth hazard: a future `respond` that reads more
    /// `IncomingCommand` fields would silently misbehave against the
    /// fake. `to` is the requester (the inbound `from`), `command` echoes
    /// the inbound command, `id` correlates the response (omitted from
    /// the wire when `None`, matching `respond`'s prior behaviour for an
    /// id-less command), `rc` is the Bus return code, `body` the payload.
    pub async fn respond_parts(
        &self,
        to: &str,
        command: &str,
        id: Option<&str>,
        rc: u8,
        body: &str,
    ) -> Result<()> {
        let mut msg = BusMessage::new()
            .with_header("command", command)
            .with_header("from", &self.name())
            .with_header("to", to)
            .with_header("type", "response")
            .with_header("rc", &rc.to_string());

        if let Some(id) = id {
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

    /// List the names of all services registered on the broker.
    ///
    /// A name-only **shim** over the rich `noded.list` reply: parses
    /// `[ServiceInfo]` and projects `.name`. Because `ServiceInfo`
    /// dual-parses (a bare name string OR an object), this tolerates both
    /// an old broker still emitting `["name", …]` and a new broker
    /// emitting objects — the §9 client-first rollout contract. For full
    /// provenance use [`Self::service_inventory`].
    pub async fn list_services(&self) -> Result<Vec<String>> {
        let services = self.service_inventory().await?;
        Ok(services.into_iter().map(|s| s.name).collect())
    }

    /// List all registered services with full build provenance
    /// (version / git_sha / build_time / pid / …) — the rich counterpart
    /// to [`Self::list_services`]. Version-discovery contract.
    pub async fn service_inventory(&self) -> Result<Vec<cosmix_bus::ServiceInfo>> {
        let result = self
            .call("noded", "noded.list", serde_json::Value::Null)
            .await?;
        let services: Vec<cosmix_bus::ServiceInfo> = serde_json::from_value(result)?;
        Ok(services)
    }

    /// Check if the broker connection is alive.
    pub fn is_connected(&self) -> bool {
        self.connected.load(Ordering::Relaxed)
    }

    // ── Internal ──

    async fn register(&self) -> Result<()> {
        // Send build provenance in the register body when present, so it
        // surfaces in noded.list/noded.info; re-sent on every register so
        // a reconnect re-publishes it. Absent → empty body (old citizen).
        let body = match &self.provenance {
            Some(p) => serde_json::to_value(p).unwrap_or(serde_json::Value::Null),
            None => serde_json::Value::Null,
        };
        match self.call_typed("noded", "noded.register", body).await? {
            cosmix_bus::PortReply::Ok { .. } => Ok(()),
            // `noded.register` has one application rejection: the requested
            // service name is already owned. Classify the typed AppError at
            // this command boundary; do not couple lifecycle decisions to the
            // broker's human-readable diagnostic text.
            cosmix_bus::PortReply::AppError { .. } => Err(NameCollision::new(self.name()).into()),
        }
    }

    /// Deregister this connection's service name from the broker and
    /// await confirmation (SPEC 18 §3.5 graceful shutdown). The broker
    /// keys the removal off the connection's registered name and guards
    /// it same-channel, so this cannot strip a name a newer connection
    /// holds; it is idempotent (RC 0 even when nothing is registered).
    /// On success the local `service_name` is cleared so a subsequent
    /// reconnect does not silently re-register an intentionally-dropped
    /// name. This is the RPC primitive; the supervised
    /// deregister-before-exit sequencing lives in the serve-mode
    /// shutdown path (SPEC 18 §3.5, WS5).
    pub async fn deregister(&self) -> Result<()> {
        self.call("noded", "noded.deregister", serde_json::Value::Null)
            .await?;
        self.service_name.write().unwrap().clear();
        Ok(())
    }

    /// Explicitly tear down this connection: best-effort WS Close,
    /// abort the detached reader task, mark disconnected, and drain
    /// parked callers.
    ///
    /// **Why this exists:** [`connect`](Self::connect) spawns a
    /// *detached* `reader_loop` that owns the read half of the split
    /// stream. Dropping a `NodedClient` therefore does **not** close
    /// the socket — the reader task keeps it alive until the next
    /// inbound frame or error, and the broker keeps the name
    /// registered. The supervised reconnect path (SPEC 18 §3.3) must
    /// discard a freshly-connected client *deterministically* on a
    /// post-connect stop or a replay failure, so the broker's
    /// WS-close path reaps the half-registered name immediately rather
    /// than retaining a live-but-orphaned connection (the §3.5 "broker
    /// registry must not retain a dead name" requirement). Plain drop
    /// cannot provide that guarantee; this can.
    ///
    /// Idempotent and best-effort: every step tolerates an
    /// already-dead socket and a second call.
    pub async fn close(&self) {
        // Reflect the teardown in `is_connected()` before any await so
        // a concurrent observer never reads a closing connection as
        // live.
        self.connected.store(false, Ordering::Relaxed);
        // Best-effort graceful WS close so the broker observes the
        // disconnect and reaps the registered name immediately rather
        // than on a later TCP error/timeout.
        {
            let mut sink = self.sink.lock().await;
            let _ = sink.send(Message::Close(None)).await;
            let _ = sink.close().await;
        }
        // Abort the detached reader — it owns the read half; without
        // this the socket stays half-open until the next inbound
        // frame and the reader task lingers.
        if let Some(h) = self.reader_handle.lock().await.take() {
            h.abort();
        }
        // The reader's normal exit drains `pending`; on abort that
        // never runs, so do it here — any parked caller gets a
        // closed-channel error now instead of a 60s timeout.
        self.pending.lock().expect("pending mutex poisoned").clear();
    }

    /// Send a raw Bus message to the broker (no request/response framing).
    pub async fn send_raw(&self, msg: &BusMessage) -> Result<()> {
        let wire = msg.to_wire();
        let result = self
            .sink
            .lock()
            .await
            .send(Message::Text(wire.into()))
            .await;
        if result.is_err() {
            // A WebSocket sink failure is definitive proof the broker
            // connection is dead — flip `connected` here rather than
            // waiting for `reader_loop` to detect close on its next
            // iteration. Without this, the race window between
            // send-failure and reader-detection lets `is_connected()`
            // return `true` immediately after a transport failure,
            // which the SPEC-18 mix carve-out's two-state probe
            // discriminator (see `cosmix-mix::bus`) relies on. The
            // race window is brief in practice but real; closing it
            // here is the cheapest fix.
            self.connected.store(false, Ordering::Relaxed);
        }
        result.context("failed to send message to broker")?;
        Ok(())
    }

    async fn reader_loop(
        mut stream: futures_util::stream::SplitStream<WebSocketStream<MaybeTlsStream<TcpStream>>>,
        pending: Arc<StdMutex<PendingMap>>,
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

            let msg = match bus::parse(&data) {
                Ok(m) => m,
                Err(e) => {
                    tracing::debug!("{service_name}: failed to parse Bus message: {e}");
                    continue;
                }
            };

            let msg_id = msg.get("id").map(|s| s.to_string());
            let msg_type = msg.get("type").unwrap_or("unknown");
            let msg_cmd = msg.get("command").unwrap_or("?");
            let msg_from = msg.get("from").unwrap_or("?");
            tracing::debug!(
                "[noded-client:{service_name}] recv type={msg_type} cmd={msg_cmd} from={msg_from} id={msg_id:?}"
            );

            // If message is a response with an id that matches a pending request, resolve it.
            // Responses are NEVER incoming commands: an orphan response (no id, or id with no
            // pending caller) is dropped, not redispatched. Treating it as a command was the
            // origin of the indexd↔noded feedback loop fixed on 2026-05-04.
            let msg_type_is_response = msg.get("type").is_some_and(|t| t == "response");
            if msg_type_is_response {
                if let Some(ref id) = msg_id {
                    // sync-mutex: `pending` deliberately uses
                    // `std::sync::Mutex` so [`PendingGuard::drop`] can
                    // remove an entry without an executor; this brief
                    // remove/send sequence holds no `.await` inside
                    // the lock guard.
                    let removed = pending.lock().expect("pending mutex poisoned").remove(id);
                    if let Some(tx) = removed {
                        let _ = tx.send(msg.clone());
                    } else {
                        tracing::debug!(
                            "{service_name}: dropping orphan response cmd={msg_cmd} id={id} (no pending caller)"
                        );
                    }
                } else {
                    tracing::debug!(
                        "{service_name}: dropping orphan response cmd={msg_cmd} (no id)"
                    );
                }
                continue;
            }

            if msg.get("command").is_some() {
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
                    headers: msg.headers.clone(),
                };
                if incoming_tx.send(cmd).is_err() {
                    tracing::debug!("{service_name}: incoming channel closed");
                    break;
                }
            }
        }

        connected.store(false, Ordering::Relaxed);
        tracing::info!("{service_name}: disconnected from broker");

        // Resolve all pending requests with an error
        pending.lock().expect("pending mutex poisoned").clear();
    }
}

#[cfg(test)]
mod pending_guard_tests {
    //! SPEC 18 Phase 2 WS4 — [`PendingGuard`] is the substrate fix
    //! for the R1 BLOCKER (per-`send` `timeout=` dropping the inner
    //! `call()` future would leak the pending-correlation entry). The
    //! integration test in `cosmix-lib-mix` exercises the upstream
    //! contract ("the inner future is actually dropped on elapsed");
    //! these unit tests pin the substrate side ("on drop, the guard
    //! removes the entry it armed; on disarm, the entry survives").
    use super::*;
    use tokio::sync::oneshot;

    fn mk_pending() -> Arc<StdMutex<PendingMap>> {
        Arc::new(StdMutex::new(HashMap::new()))
    }

    #[test]
    fn armed_drop_removes_the_pending_entry() {
        let pending = mk_pending();
        let (tx, _rx) = oneshot::channel();
        pending.lock().unwrap().insert("42".to_string(), tx);
        assert!(
            pending.lock().unwrap().contains_key("42"),
            "precondition: entry inserted"
        );

        {
            let _guard = PendingGuard::arm(pending.clone(), "42".to_string());
        }

        assert!(
            !pending.lock().unwrap().contains_key("42"),
            "armed PendingGuard's Drop must remove the entry it armed — \
             without this the per-send timeout path would leak a pending \
             slot per elapsed request (R1 BLOCKER)"
        );
    }

    #[test]
    fn disarmed_drop_leaves_the_pending_entry_in_place() {
        let pending = mk_pending();
        let (tx, _rx) = oneshot::channel();
        pending.lock().unwrap().insert("7".to_string(), tx);

        {
            let mut guard = PendingGuard::arm(pending.clone(), "7".to_string());
            guard.disarm();
        }

        assert!(
            pending.lock().unwrap().contains_key("7"),
            "disarmed PendingGuard's Drop must be a no-op — the success \
             path disarms because reader_loop already removed the entry \
             when resolving the oneshot, and a double-remove would mask \
             real bugs where the response path was never reached"
        );
    }

    #[test]
    fn drop_after_response_path_removal_is_a_silent_no_op() {
        // Models the race that `disarm()` is the optimisation for: if a
        // caller forgot to `disarm` on the success path, Drop still runs
        // and finds the entry already gone (reader_loop removed it).
        // Must not panic, must not insert anything, must not poison.
        let pending = mk_pending();
        {
            let _guard = PendingGuard::arm(pending.clone(), "missing".to_string());
            // No insert — simulates the entry having already been
            // removed by the response path before Drop fires.
        }
        assert!(
            pending.lock().unwrap().is_empty(),
            "Drop on a guard whose entry no longer exists must be \
             harmless — covers the response-path-faster-than-disarm \
             race and the explicit-no-insert test setup"
        );
    }
}

#[cfg(test)]
mod connect_cancellation_tests {
    use super::*;
    use std::time::Duration;

    use futures_util::StreamExt;
    use tokio::net::TcpListener;
    use tokio::sync::oneshot;
    use tokio_tungstenite::accept_async;

    #[tokio::test]
    async fn cancelling_registration_aborts_reader_and_closes_socket() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let (register_seen_tx, register_seen_rx) = oneshot::channel();
        let (disconnected_tx, disconnected_rx) = oneshot::channel();
        let broker = tokio::spawn(async move {
            let (socket, _) = listener.accept().await.unwrap();
            let mut websocket = accept_async(socket).await.unwrap();
            let request = websocket.next().await.expect("registration request");
            assert!(request.is_ok());
            let _ = register_seen_tx.send(());
            // Deliberately withhold the registration response. Dropping the
            // connect future must still close this socket promptly.
            let _ = websocket.next().await;
            let _ = disconnected_tx.send(());
        });

        let connect = tokio::spawn(async move {
            NodedClient::connect("cancelled-service", &format!("ws://{address}")).await
        });
        register_seen_rx.await.unwrap();
        connect.abort();
        let _ = connect.await;

        tokio::time::timeout(Duration::from_secs(60), disconnected_rx)
            .await
            .expect("cancelled connect must not retain the reader/socket")
            .unwrap();
        broker.await.unwrap();
    }
}
