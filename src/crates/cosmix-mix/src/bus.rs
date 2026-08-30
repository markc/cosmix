use std::cell::{Cell, RefCell};
use std::collections::BTreeMap;
use std::future::Future;
use std::pin::Pin;

use cosmix_mix::error::MixResult;
use cosmix_mix::evaluator::{BusHandler, IncomingEvent, RC_UNAVAILABLE};
use cosmix_mix::value::Value;
use indexmap::IndexMap;
use tokio::sync::mpsc::UnboundedReceiver;

/// Lazy-probe state machine for the pre-noded discovery path.
/// Distinguishes bare-host (silent nil from `send`/`emit`) from
/// configured-mesh-outage (loud `mesh unavailable` from
/// `register_as`/`reply`): the *first* Bus form's probe outcome
/// decides which branch the handler is on for the rest of the
/// process. This is the load-bearing piece of the auto-upgrade
/// story — a `mix` binary that was bare-on-first-invocation can be
/// upgraded to mesh-viable just by starting `cosmix-noded`.
enum MeshState {
    /// No Bus form has run yet; the cached probe has not been performed.
    Unprobed,
    /// First probe found no broker. Bus forms return nil. Sticky for
    /// the life of the process (the auto-upgrade invariant applies to
    /// *subsequent* `mix` invocations) unless the script explicitly
    /// calls `bus_reconnect()`.
    NeverPresent,
    /// Probe succeeded; this is the live broker client. Bus forms
    /// call through normally.
    Connected(std::sync::Arc<cosmix_lib_client::NodedClient>),
    /// The cached `Connected` handle failed a call (noded restart,
    /// broker gone). Bus forms raise `mesh unavailable: …` until the
    /// script explicitly calls `bus_reconnect()` to reset to
    /// `Unprobed`.
    Lost,
}

/// Outcome of [`MixBusHandler::noded_access`]: either a live broker
/// client, or one of the two failure modes the caller must distinguish.
enum MeshErr {
    /// Caller should return a nil/no-op success for the in-flight Bus
    /// form. The host has never had noded.
    NeverPresent,
    /// Caller should raise `MixError::RuntimeError` with a
    /// `mesh unavailable` message. The host had noded but the
    /// connection is broken.
    Lost,
}

/// Construct the `mesh unavailable` runtime error from a free-text
/// detail string. Centralised so the loud-raise path has a consistent
/// surface and the message names the recovery primitive (`bus_reconnect`).
fn mesh_unavailable(detail: &str) -> cosmix_mix::error::MixError {
    cosmix_mix::error::MixError::RuntimeError {
        span: None,
        msg: format!("mesh unavailable: {detail} (call bus_reconnect() to retry the probe)"),
    }
}

/// Bus handler that routes commands to cosmix ports.
///
/// Routing strategy: try local Unix socket first for simple names (no dots).
/// If socket doesn't exist, or target contains dots / `.bus` addresses,
/// route via WebSocket broker. The broker URL is `ws://127.0.0.1:4200/ws`
/// when `node.conf.mix` is absent (the bare/loopback path), and
/// `ws://{wg_ip}:{noded.port}/ws` derived from the local node's WG IP
/// when `node.conf.mix` is present (the mesh-citizen path) — see
/// `cosmix_lib_client::NodedClient::connect_anonymous_default()`.
///
/// On a host with no noded, the first Bus form's probe records
/// `MeshState::NeverPresent` and subsequent Bus forms return nil
/// without re-probing — the no-boot-cost discovery path. On a host
/// that had noded but lost the connection, the state transitions to
/// `MeshState::Lost` and Bus forms raise `mesh unavailable: …`.
pub struct MixBusHandler {
    /// Cached probe state + broker client. `Mutex` rather than `OnceCell`
    /// so [`BusHandler::reconnect`] can reset to `Unprobed` (SPEC 18 §3.3:
    /// the WS8 acceptance harness induces a `cosmix-noded` bounce and
    /// must re-dial its OWN client — `OnceCell` has no &self reset).
    /// `Arc` inside `Connected` lets a caller hold the client across
    /// its `.await` without borrowing `self`.
    mesh: tokio::sync::Mutex<MeshState>,
    /// Incoming-message receiver, taken from the `NodedClient` on first
    /// `next_incoming` call and stored here so subsequent calls can re-await.
    ///
    /// `RefCell<Option<...>>` gives interior mutability over a `!Send` field.
    /// The take/restore dance around `recv().await` is implemented via the
    /// [`ReceiverGuard`] RAII type below — the borrow is never held across
    /// an await point, and the destructor unconditionally restores the
    /// receiver even on future cancellation.
    ///
    /// The structural guarantee (drop-guard) replaces what was previously a
    /// documentation-based invariant. Holding `borrow_mut()` across an await
    /// would panic on reentrance; a dropped future mid-await without a
    /// restore-on-drop would leave the stream permanently stuck as `None`.
    /// The guard avoids both hazards without relying on caller discipline.
    incoming: RefCell<Option<UnboundedReceiver<cosmix_lib_client::IncomingCommand>>>,
    /// Sticky "no more incoming possible" flag. Once the receiver returns
    /// `None` (connection closed) or we fail to initialize the broker, we set
    /// this so future calls short-circuit to `None` without re-trying.
    incoming_closed: RefCell<bool>,
    /// Sticky "receiver guard corruption" flag — set if the drop-guard
    /// ever fails to restore the receiver to its slot (the "impossible"
    /// RefCell-already-borrowed case). Distinct from `incoming_closed`
    /// because the transport is still alive; we just can't reach it
    /// through this handler. Separating the two gives debuggers a
    /// different error message when the failure surfaces.
    incoming_broken: RefCell<bool>,
    /// Monotonic incarnation counter for the incoming stream, bumped
    /// by `reconnect()`. A `ReceiverGuard` captures the value at
    /// `take()` time and checks it on `Drop`: if the counter has
    /// advanced (a `bus_reconnect()` ran during the await), the guard
    /// drops the stale receiver rather than restoring it into a slot
    /// the reconnect explicitly cleared. Without this check, a future
    /// cancellation mid-await would silently revive the old receiver
    /// and undo the reset. `Cell` is fine — `MixBusHandler` is `!Send`,
    /// only the current-thread evaluator touches it.
    incoming_generation: Cell<u64>,
}

/// RAII guard that restores a `UnboundedReceiver` to a shared `RefCell` slot
/// when dropped, including on future cancellation mid-await.
///
/// This exists specifically to avoid the "dropped future strands the receiver"
/// bug: `next_incoming` moves the receiver out of the cell before awaiting on
/// `recv()`, and the guard's `Drop` impl puts it back unconditionally — so
/// whether the future completes normally or is cancelled mid-await, the next
/// call to `next_incoming` still finds the receiver in the cell.
///
/// The guard's invariant: it holds `Some(receiver)` while the caller is
/// awaiting, and `None` only after `take()` has extracted the receiver (which
/// only happens at the end of a successful await path in `next_incoming`).
struct ReceiverGuard<'a> {
    slot: &'a RefCell<Option<UnboundedReceiver<cosmix_lib_client::IncomingCommand>>>,
    /// Flag that the drop impl sets if it fails to restore the receiver.
    /// The outer `MixBusHandler` checks this and sets `incoming_broken`
    /// so subsequent `next_incoming` calls can short-circuit with a
    /// diagnostic message distinct from the normal closed case.
    broken_flag: &'a RefCell<bool>,
    /// Incarnation snapshot at the time of `take()`. If the live
    /// counter has advanced by `Drop` (a `bus_reconnect()` ran during
    /// the await), restoring the receiver would undo the reset — the
    /// guard drops it instead. See `MixBusHandler::incoming_generation`.
    generation_at_take: u64,
    /// Live incarnation counter — read at `Drop` time to compare with
    /// `generation_at_take`.
    generation_now: &'a Cell<u64>,
    receiver: Option<UnboundedReceiver<cosmix_lib_client::IncomingCommand>>,
}

impl<'a> ReceiverGuard<'a> {
    /// Take the receiver out of the slot into a new guard.
    ///
    /// Returns `None` if the slot is empty (either not yet initialized or
    /// already held by another guard — the latter indicates a reentrant call,
    /// which is not supported and should short-circuit to end-of-stream).
    fn take(
        slot: &'a RefCell<Option<UnboundedReceiver<cosmix_lib_client::IncomingCommand>>>,
        broken_flag: &'a RefCell<bool>,
        generation: &'a Cell<u64>,
    ) -> Option<Self> {
        let receiver = slot.borrow_mut().take()?;
        Some(ReceiverGuard {
            slot,
            broken_flag,
            generation_at_take: generation.get(),
            generation_now: generation,
            receiver: Some(receiver),
        })
    }

    /// Get a mutable reference to the receiver for awaiting.
    fn as_mut(&mut self) -> &mut UnboundedReceiver<cosmix_lib_client::IncomingCommand> {
        self.receiver
            .as_mut()
            .expect("ReceiverGuard receiver taken before drop")
    }
}

impl<'a> Drop for ReceiverGuard<'a> {
    fn drop(&mut self) {
        if let Some(receiver) = self.receiver.take() {
            // Generation check: if `reconnect()` ran during the
            // `recv().await` (incrementing the counter), the receiver
            // we hold is stale and the new `incoming` slot should
            // stay None. Drop the receiver silently in that case —
            // doing so is exactly the property the reset wanted.
            if self.generation_now.get() != self.generation_at_take {
                tracing::debug!(
                    "ReceiverGuard::drop: incoming_generation advanced \
                     during await (was {}, now {}); dropping stale receiver \
                     so bus_reconnect()'s reset is preserved",
                    self.generation_at_take,
                    self.generation_now.get()
                );
                return;
            }

            // try_borrow_mut guards against the "impossible" reentrant-borrow
            // case. In correct single-threaded use this always succeeds; a
            // failure here means someone is borrowing the slot while the
            // guard is dropping, which is a logic error. Fail loudly rather
            // than silently drop the receiver on the floor AND set the
            // broken_flag so future `next_incoming` calls can short-circuit
            // with a distinct diagnostic message. Without the flag, the
            // only signal of corruption would be one `tracing::error!` line
            // at the moment of failure; subsequent silent deafness would
            // be diagnosable only by correlating that log line with the
            // current symptom minutes later. The flag keeps the error fresh.
            match self.slot.try_borrow_mut() {
                Ok(mut slot) => *slot = Some(receiver),
                Err(_) => {
                    if let Ok(mut flag) = self.broken_flag.try_borrow_mut() {
                        *flag = true;
                    }
                    tracing::error!(
                        "ReceiverGuard::drop: RefCell was already borrowed — \
                         invariant violation, receiver dropped, incoming_broken set"
                    );
                }
            }
        }
    }
}

impl MixBusHandler {
    pub fn new() -> Self {
        MixBusHandler {
            mesh: tokio::sync::Mutex::new(MeshState::Unprobed),
            incoming: RefCell::new(None),
            incoming_closed: RefCell::new(false),
            incoming_broken: RefCell::new(false),
            incoming_generation: Cell::new(0),
        }
    }

    /// Check if a local Unix socket exists for this target.
    fn has_local_socket(target: &str) -> bool {
        if target.contains('.') {
            return false;
        }
        let uid = crate::cosmix_paths::current_uid();
        let path = format!("/run/user/{uid}/cosmix/ports/{target}.sock");
        std::path::Path::new(&path).exists()
    }

    /// Resolve a simple target name to a Unix socket path.
    fn socket_path(target: &str) -> String {
        let uid = crate::cosmix_paths::current_uid();
        format!("/run/user/{uid}/cosmix/ports/{target}.sock")
    }

    /// Probe-or-fetch the broker client per the lazy-probe state machine.
    ///
    /// On `Unprobed`, performs the lazy probe via
    /// `NodedClient::connect_anonymous(&node_config::resolve_noded_url())`
    /// and transitions to `Connected(handle)` or `NeverPresent`. On
    /// `Connected`, returns the cached handle. On `NeverPresent` or
    /// `Lost`, returns the corresponding `MeshErr` so the caller
    /// can apply the (silent-nil / loud-raise) policy.
    ///
    /// Holds the mutex across the probe so concurrent first-callers do
    /// not race a thundering herd of connects; the evaluator is
    /// current-thread so this serialization is effectively free.
    async fn noded_access(
        &self,
    ) -> Result<std::sync::Arc<cosmix_lib_client::NodedClient>, MeshErr> {
        let mut state = self.mesh.lock().await;
        match &*state {
            MeshState::Connected(c) => Ok(c.clone()),
            MeshState::NeverPresent => Err(MeshErr::NeverPresent),
            MeshState::Lost => Err(MeshErr::Lost),
            MeshState::Unprobed => {
                let url = crate::node_config::resolve_noded_url();
                match cosmix_lib_client::NodedClient::connect_anonymous(&url).await {
                    Ok(client) => {
                        let arc = std::sync::Arc::new(client);
                        *state = MeshState::Connected(arc.clone());
                        Ok(arc)
                    }
                    Err(_) => {
                        // The first probe failed: stick at NeverPresent
                        // until the script calls `bus_reconnect()`. Per
                        // the locked contract the discovery path is
                        // *zero-boot-cost*, which means a failed initial
                        // probe must not retry on every subsequent Bus
                        // form (the old "leave slot None, retry next
                        // call" behaviour). Retry is explicit, not
                        // ambient.
                        *state = MeshState::NeverPresent;
                        Err(MeshErr::NeverPresent)
                    }
                }
            }
        }
    }

    /// Transition `Connected → Lost`, but only if the cached state is
    /// still the same `Arc<NodedClient>` the failing call held. Avoids
    /// two interleaved hazards:
    ///   1. An older in-flight call's failure clobbering a newer
    ///      successful `reconnect()` + probe (its result Arc would be
    ///      different).
    ///   2. A `NeverPresent`/`Lost`/`Unprobed` state being clobbered
    ///      back to `Lost` by a stale failure from a long-dropped Arc.
    ///
    /// `Arc::ptr_eq` is the identity check — same allocation = same
    /// connection generation.
    async fn mark_lost_if_current(
        &self,
        expected: &std::sync::Arc<cosmix_lib_client::NodedClient>,
    ) {
        let mut state = self.mesh.lock().await;
        if let MeshState::Connected(current) = &*state
            && std::sync::Arc::ptr_eq(current, expected)
        {
            *state = MeshState::Lost;
        }
    }
}

impl BusHandler for MixBusHandler {
    fn send<'a>(
        &'a self,
        target: &'a str,
        command: &'a str,
        args: &'a Value,
    ) -> Pin<Box<dyn Future<Output = MixResult<(i32, Value)>> + 'a>> {
        Box::pin(async move {
            let json_args = value_to_json(args);

            // Try local Unix socket first for simple names. The local
            // socket path is independent of mesh state — it talks to
            // sibling user-services on the same machine and does not
            // need the broker. Typed replies preserve the peer rc (success
            // `< 10` incl. warnings, or an application `>= 10`); a TRANSPORT
            // failure returns `Err` (→ `$rc = -1`). The bare-vs-mesh policy
            // applies only to the broker-routed path below.
            if Self::has_local_socket(target) {
                let socket = Self::socket_path(target);
                // call_port_typed keeps TRANSPORT failures (connect/read/parse)
                // as `Err` — the evaluator maps those to `$rc = -1` (RC_TRANSPORT)
                // — while a peer `rc >= 10` reply is a distinct AppError that
                // preserves the real status in `$rc` (the rc-band contract; the
                // old call_port collapsed both into rc=10).
                return match cosmix_lib_bus::call_port_typed(&socket, command, json_args).await {
                    Ok(cosmix_lib_bus::PortReply::Ok { rc, value }) => {
                        Ok((i32::from(rc), json_to_value(&value)))
                    }
                    Ok(cosmix_lib_bus::PortReply::AppError { rc, message }) => {
                        Ok((i32::from(rc), Value::String(message)))
                    }
                    Err(e) => Err(mesh_unavailable(&format!(
                        "call_port({target}, {command}) transport failure: {e}"
                    ))),
                };
            }

            // Broker-routed path: apply the bare-vs-mesh policy.
            let client = match self.noded_access().await {
                Ok(c) => c,
                // Broker NEVER present (a bare host): a NON-FATAL "not
                // delivered" signal, NOT a fake `(0, Nil)` success (2026-07-02
                // audit, Codex-ruled). `$rc == 0` now strictly means
                // "delivered+accepted"; a bare host reads RC_UNAVAILABLE while
                // the script still runs (and lights up the moment a broker
                // appears — graceful degrade preserved, just no longer
                // masquerading as success).
                Err(MeshErr::NeverPresent) => {
                    return Ok((
                        RC_UNAVAILABLE,
                        Value::String("Bus unavailable: broker never present".to_string()),
                    ));
                }
                Err(MeshErr::Lost) => {
                    return Err(mesh_unavailable(
                        "noded was reachable but the cached connection is broken",
                    ));
                }
            };

            // A `body=` key — or a SPEC-12 `*.props.*` command (see
            // `wants_header_routing`) — selects header routing: scalar args
            // → Bus headers, `body` → the Bus body. `send` AWAITS its reply,
            // so this must use the reply-COLLECTING `call_with_headers_raw` —
            // the fire-and-forget `send_with_headers` (right for `emit`)
            // registers no `pending` slot, so the broker's response is
            // dropped and `$result` strands at nil even though the handler
            // replied. The raw `(rc, body, error_header)` triple is mapped
            // through the same rc-band contract as the JSON-body
            // `call_typed` path below.
            if let Value::Map(map) = args
                && wants_header_routing(command, map)
            {
                let (headers, body) = split_headers_body(map)?;
                return match client
                    .call_with_headers_raw(target, command, &headers, &body)
                    .await
                {
                    Ok((rc, reply_body, error_header)) => {
                        Ok(headers_reply_to_result(rc, reply_body, error_header))
                    }
                    // A TRANSPORT failure (broker close, send error, 60s
                    // timeout) → mark Lost (if our Arc is still the cached
                    // one) and raise; the evaluator maps the raise to
                    // `$rc = -1`.
                    Err(e) => {
                        self.mark_lost_if_current(&client).await;
                        Err(mesh_unavailable(&format!(
                            "call_with_headers({target}, {command}) transport failure: {e}"
                        )))
                    }
                };
            }

            // `call_typed` STRUCTURALLY separates the two failure kinds that
            // `call()` conflated: a peer `rc >= 10` reply (an ordinary
            // "unknown service" / "command rejected" application error) is
            // `Ok(AppError { rc, message })` — its EXACT rc reaches `$rc` (a
            // peer rc=42 is no longer flattened to 10), and it must NOT poison
            // the handler to `Lost`. A transport failure (broker closed,
            // send_raw failed, 60s timeout) is an `Err` → transition to Lost
            // and raise `mesh unavailable` (→ `$rc = -1`). A success reply
            // carries its rc too (0 or a warning 5).
            match client.call_typed(target, command, json_args).await {
                Ok(cosmix_lib_bus::PortReply::Ok { rc, value }) => {
                    Ok((i32::from(rc), json_to_value(&value)))
                }
                Ok(cosmix_lib_bus::PortReply::AppError { rc, message }) => {
                    Ok((i32::from(rc), Value::String(message)))
                }
                Err(e) => {
                    self.mark_lost_if_current(&client).await;
                    Err(mesh_unavailable(&format!(
                        "call({target}, {command}) transport failure: {e}"
                    )))
                }
            }
        })
    }

    fn emit<'a>(
        &'a self,
        target: &'a str,
        command: &'a str,
        args: &'a Value,
    ) -> Pin<Box<dyn Future<Output = MixResult<()>> + 'a>> {
        Box::pin(async move {
            // Local Unix socket fast path — mesh state irrelevant.
            // emit is fire-and-forget by definition, so the local
            // socket failure is dropped silently (matches the
            // existing local-socket behaviour).
            if Self::has_local_socket(target) {
                let socket = Self::socket_path(target);
                let json_args = value_to_json(args);
                let _ = cosmix_lib_bus::call_port(&socket, command, json_args).await;
                return Ok(());
            }

            // Broker-routed emit: apply the bare-vs-mesh
            // policy. NeverPresent → silent return (bare host); Lost
            // → loud raise (mesh-configured outage). A transport
            // failure on Connected transitions to Lost AND raises
            // this emit — the script's choice of emit (vs send)
            // signals "don't block on reply", not "don't surface
            // transport errors", so a broken broker still gets a
            // loud signal.
            let client = match self.noded_access().await {
                Ok(c) => c,
                Err(MeshErr::NeverPresent) => return Ok(()),
                Err(MeshErr::Lost) => {
                    return Err(mesh_unavailable(
                        "noded was reachable but the cached connection is broken",
                    ));
                }
            };

            // Header routing is the default for `emit target cmd ...` — the
            // common Bus message shape is "scalar headers, optional body".
            // A script that wants JSON body routing (RPC-style) should use
            // `call` instead, or pass a single non-Map arg. Any Map arg is
            // treated as a header set; `body=` is the explicit body channel.
            //
            // Both `send` and `send_with_headers` are fire-and-forget at
            // the broker (no rc parsed), so any error here is transport.
            // Mark Lost (if our Arc is still the cached one) and raise.
            if let Value::Map(map) = args {
                let (headers, body) = split_headers_body(map)?;
                return match client
                    .send_with_headers(target, command, &headers, &body)
                    .await
                {
                    Ok(()) => Ok(()),
                    Err(e) => {
                        self.mark_lost_if_current(&client).await;
                        Err(mesh_unavailable(&format!(
                            "emit send_with_headers({target}, {command}) failed: {e}"
                        )))
                    }
                };
            }

            let json_args = value_to_json(args);
            match client.send(target, command, json_args).await {
                Ok(()) => Ok(()),
                Err(e) => {
                    self.mark_lost_if_current(&client).await;
                    Err(mesh_unavailable(&format!(
                        "emit send({target}, {command}) failed: {e}"
                    )))
                }
            }
        })
    }

    fn port_exists<'a>(
        &'a self,
        target: &'a str,
    ) -> Pin<Box<dyn Future<Output = MixResult<bool>> + 'a>> {
        Box::pin(async move {
            // Check local Unix socket first
            if Self::has_local_socket(target) {
                return Ok(true);
            }

            // Broker-routed query: apply the bare-vs-mesh
            // policy. NeverPresent → "no, can't be reached" (false);
            // Lost → loud raise. A transport failure on Connected
            // transitions to Lost AND raises this query.
            let client = match self.noded_access().await {
                Ok(c) => c,
                Err(MeshErr::NeverPresent) => return Ok(false),
                Err(MeshErr::Lost) => {
                    return Err(mesh_unavailable(
                        "noded was reachable but the cached connection is broken",
                    ));
                }
            };

            // `list_services` goes through `call` and so conflates
            // transport with application errors. Discriminate via
            // `is_connected()` for the same reason `send` does.
            let result = client.list_services().await;
            match result {
                Ok(services) => Ok(services.iter().any(|s| s == target)),
                Err(e) => {
                    if !client.is_connected() {
                        self.mark_lost_if_current(&client).await;
                        Err(mesh_unavailable(&format!(
                            "port_exists list_services failed: {e}"
                        )))
                    } else {
                        // Application error from broker — surface as a
                        // generic runtime error (not mesh_unavailable),
                        // no state transition.
                        Err(cosmix_mix::error::MixError::RuntimeError {
                            span: None,
                            msg: format!("port_exists list_services failed: {e}"),
                        })
                    }
                }
            }
        })
    }

    fn next_incoming<'a>(&'a self) -> Pin<Box<dyn Future<Output = Option<IncomingEvent>> + 'a>> {
        Box::pin(async move {
            if *self.incoming_closed.borrow() {
                return None;
            }
            if *self.incoming_broken.borrow() {
                // Receiver guard corruption — distinct from normal close.
                // Log on every call so the visibility stays fresh in the
                // journal; a future debugger correlating symptoms back to
                // the root cause can grep for this message.
                tracing::error!(
                    "next_incoming: receiver slot corrupted by guard drop failure; \
                     connection still alive but unreachable via this handler"
                );
                return None;
            }

            // Lazy init: connect to the broker on first call, then take the
            // receiver out of the client and stash it in our RefCell.
            // next_incoming returns Option (no MixError surface), so both
            // NeverPresent and Lost are mapped to closed — a transient
            // `mix script.mix` invocation can't subscribe to a stream
            // that has no broker behind it. The send/emit/etc. paths
            // remain the loud-failure surface for Lost.
            if self.incoming.borrow().is_none() {
                let client = match self.noded_access().await {
                    Ok(c) => c,
                    Err(MeshErr::NeverPresent) | Err(MeshErr::Lost) => {
                        *self.incoming_closed.borrow_mut() = true;
                        return None;
                    }
                };
                let rx = client.incoming_async().await;
                match rx {
                    Some(rx) => *self.incoming.borrow_mut() = Some(rx),
                    None => {
                        // Receiver already taken (shouldn't happen — we own
                        // the only MixBusHandler for this process) or the
                        // client doesn't provide one. Treat as closed.
                        *self.incoming_closed.borrow_mut() = true;
                        return None;
                    }
                }
            }

            // Take the receiver via the RAII guard. The guard's destructor
            // restores the receiver to the slot whether `recv().await`
            // completes normally OR the future is cancelled mid-await.
            // See [`ReceiverGuard`] for the rationale.
            let mut guard = match ReceiverGuard::take(
                &self.incoming,
                &self.incoming_broken,
                &self.incoming_generation,
            ) {
                Some(g) => g,
                None => {
                    // Slot was empty — reentrant call (someone else already
                    // took the receiver) or unexpected state. Return
                    // end-of-stream for this tick; the real receiver will
                    // be available again on the next call.
                    return None;
                }
            };

            let result = guard.as_mut().recv().await;

            // `guard` drops here (either via normal return or via future
            // cancellation), restoring the receiver to the slot. If
            // `recv()` returned None (connection closed), mark closed so
            // future calls short-circuit.
            drop(guard);

            match result {
                Some(cmd) => Some(IncomingEvent {
                    command: cmd.command,
                    headers: cmd.headers,
                    body: cmd.body,
                }),
                None => {
                    *self.incoming_closed.borrow_mut() = true;
                    None
                }
            }
        })
    }

    fn register_as<'a>(
        &'a self,
        name: &'a str,
    ) -> Pin<Box<dyn Future<Output = MixResult<()>> + 'a>> {
        Box::pin(async move {
            // register_as is an explicit RPC — a script that calls it
            // wants to be an addressable Bus citizen. There is no
            // "silent nil" interpretation: a bare host that asks to
            // register must surface the failure, not pretend success.
            // So NeverPresent raises here, distinguishing this from
            // the send/emit silent-nil treatment.
            let client = match self.noded_access().await {
                Ok(c) => c,
                Err(MeshErr::NeverPresent) => {
                    return Err(mesh_unavailable(
                        "register_as requires a broker; no noded available on this host",
                    ));
                }
                Err(MeshErr::Lost) => {
                    return Err(mesh_unavailable(
                        "register_as: noded was reachable but the cached connection is broken",
                    ));
                }
            };
            // `register_as` goes through `call` (it issues
            // `noded.register`), so the error conflates transport
            // failures with broker-level rejections (name collision,
            // invalid identifier, etc.). Discriminate via
            // `is_connected()`: dead → Lost + mesh_unavailable; alive
            // → generic runtime error with the broker's message,
            // state unchanged.
            let result = client.register_as(name).await;
            match result {
                Ok(()) => Ok(()),
                Err(e) => {
                    if !client.is_connected() {
                        self.mark_lost_if_current(&client).await;
                        Err(mesh_unavailable(&format!("register_as failed: {e}")))
                    } else {
                        Err(cosmix_mix::error::MixError::RuntimeError {
                            span: None,
                            msg: format!("broker register failed: {e}"),
                        })
                    }
                }
            }
        })
    }

    // SPEC 18 WS2 — Ch03 topic (un)subscription.
    //
    // This `MixBusHandler` owns a bare anonymous `NodedClient`
    // (transient `mix <script>` mode): it issues the
    // `topic.subscribe`/`topic.unsubscribe` RPC and there is **no**
    // `SubscriptionRegistry` because there is no supervisor — a
    // transient citizen has nothing to replay (no reconnect path; the
    // process is gone if the broker bounces). Anonymous connections
    // still have full topic pub/sub via the Ch03 §3.11.1
    // connection-scoped synthetic peer identity, so the broker accepts
    // these.
    //
    // The registry-aware transactional chokepoint is
    // `cosmix_lib_client::SupervisedClient::{subscribe,unsubscribe}_topic`.
    // **WS3 wires serve mode's `MixBusHandler` to the supervised client
    // so `subscribe()` from init-body or handler-body mutates that one
    // shared registry** (the §3.3 replay guarantee proven at the
    // cosmix-lib-client layer in WS2's tests). Until then this transient
    // path is correct precisely because it has no registry to keep
    // honest. The `name` header carries the topic (the broker's
    // `topic.subscribe` requires it; empty body forces header routing).

    fn subscribe_topic<'a>(
        &'a self,
        name: &'a str,
    ) -> Pin<Box<dyn Future<Output = MixResult<()>> + 'a>> {
        Box::pin(async move {
            // (Un)subscribe is an explicit topic operation — same
            // raise-on-NeverPresent rationale as register_as: a script
            // that asks the broker to do something it cannot pretend
            // succeeded.
            let client = match self.noded_access().await {
                Ok(c) => c,
                Err(MeshErr::NeverPresent) => {
                    return Err(mesh_unavailable(
                        "subscribe_topic requires a broker; no noded available on this host",
                    ));
                }
                Err(MeshErr::Lost) => {
                    return Err(mesh_unavailable(
                        "subscribe_topic: noded was reachable but the cached connection is broken",
                    ));
                }
            };
            let headers = BTreeMap::from([("name".to_string(), name.to_string())]);
            let result = client
                .call_with_headers("noded", "topic.subscribe", &headers, "")
                .await;
            // call_with_headers conflates transport + app errors;
            // discriminate via is_connected().
            match result {
                Ok(_) => Ok(()),
                Err(e) => {
                    if !client.is_connected() {
                        self.mark_lost_if_current(&client).await;
                        Err(mesh_unavailable(&format!("topic.subscribe failed: {e}")))
                    } else {
                        Err(cosmix_mix::error::MixError::RuntimeError {
                            span: None,
                            msg: format!("topic.subscribe failed: {e}"),
                        })
                    }
                }
            }
        })
    }

    fn unsubscribe_topic<'a>(
        &'a self,
        name: &'a str,
    ) -> Pin<Box<dyn Future<Output = MixResult<()>> + 'a>> {
        Box::pin(async move {
            let client = match self.noded_access().await {
                Ok(c) => c,
                Err(MeshErr::NeverPresent) => {
                    return Err(mesh_unavailable(
                        "unsubscribe_topic requires a broker; no noded available on this host",
                    ));
                }
                Err(MeshErr::Lost) => {
                    return Err(mesh_unavailable(
                        "unsubscribe_topic: noded was reachable but the cached connection is broken",
                    ));
                }
            };
            let headers = BTreeMap::from([("name".to_string(), name.to_string())]);
            let result = client
                .call_with_headers("noded", "topic.unsubscribe", &headers, "")
                .await;
            match result {
                Ok(_) => Ok(()),
                Err(e) => {
                    if !client.is_connected() {
                        self.mark_lost_if_current(&client).await;
                        Err(mesh_unavailable(&format!("topic.unsubscribe failed: {e}")))
                    } else {
                        Err(cosmix_mix::error::MixError::RuntimeError {
                            span: None,
                            msg: format!("topic.unsubscribe failed: {e}"),
                        })
                    }
                }
            }
        })
    }

    // SPEC 18 WS-R — answer the request the current `on` handler is
    // servicing. The evaluator captured the correlation parts off the
    // in-flight event and passes them here; this is the transport
    // adapter. `respond_parts` is the byte-identical part-wise core of
    // `cosmix-lib-client::NodedClient::respond`, used so this neutral
    // boundary need not fabricate an `IncomingCommand` (a future
    // `respond` reading more fields would silently misbehave against a
    // fake — the partial-truth shape this crate's review loop exists to
    // catch). This `MixBusHandler` owns the bare anonymous
    // `NodedClient` (transient `mix <script>` mode); WS3 wires serve
    // mode's handler to the supervised client.
    fn reply<'a>(
        &'a self,
        to: &'a str,
        command: &'a str,
        id: Option<&'a str>,
        rc: u8,
        body: &'a str,
    ) -> Pin<Box<dyn Future<Output = MixResult<()>> + 'a>> {
        Box::pin(async move {
            // reply is only reachable from inside an `on … do` handler
            // that the event pump dispatched, so NeverPresent should be
            // unreachable in practice (we wouldn't have received an
            // event to reply to). Treat it as a loud error if it
            // somehow happens, alongside the Lost case.
            let client = match self.noded_access().await {
                Ok(c) => c,
                Err(MeshErr::NeverPresent) => {
                    return Err(mesh_unavailable(
                        "reply called without an active broker; \
                         this should be unreachable from inside an `on` handler",
                    ));
                }
                Err(MeshErr::Lost) => {
                    return Err(mesh_unavailable(
                        "reply: noded was reachable but the cached connection is broken",
                    ));
                }
            };
            // `respond_parts` is fire-and-forget at the broker (no rc
            // parsed from a reply we never await), so any error is
            // transport.
            match client.respond_parts(to, command, id, rc, body).await {
                Ok(()) => Ok(()),
                Err(e) => {
                    self.mark_lost_if_current(&client).await;
                    Err(mesh_unavailable(&format!("reply failed: {e}")))
                }
            }
        })
    }

    fn reconnect<'a>(&'a self) -> Pin<Box<dyn Future<Output = MixResult<()>> + 'a>> {
        Box::pin(async move {
            // Reset the state machine to `Unprobed`: the next Bus form
            // re-runs the lazy probe. This is the recovery primitive
            // for both NeverPresent (a script that just `apt install
            // cosmix-noded` and wants to try again without restarting)
            // and Lost (the §9(b) post-bounce recovery loop polling
            // until cosmix-noded is back). The local-Unix-socket fast
            // path (call_port) opens a fresh connection per call and
            // is unaffected — nothing to reset there. MixServeHandler
            // deliberately does NOT override this (the SupervisedClient
            // self-recovers; a citizen calling bus_reconnect() is a
            // harmless trait-default no-op).
            *self.mesh.lock().await = MeshState::Unprobed;
            // Also clear the incoming-receiver state. Without this, a
            // `next_incoming` that hit `NeverPresent` (closing the
            // sticky `incoming_closed` flag) or a previously corrupted
            // ReceiverGuard (sticky `incoming_broken` flag) would stay
            // dead after a successful re-probe — the next call to
            // `next_incoming` would short-circuit to None despite
            // having a fresh broker handle. Resetting all three slots
            // makes the next call re-fetch the receiver from the new
            // client.
            *self.incoming.borrow_mut() = None;
            *self.incoming_closed.borrow_mut() = false;
            *self.incoming_broken.borrow_mut() = false;
            // Bump the generation: any `ReceiverGuard` currently
            // awaiting `recv()` on the old receiver will, on Drop,
            // see the advanced generation and refuse to restore the
            // stale receiver into the (now-cleared) slot. Without
            // this, a future-cancellation drop could quietly revive
            // the old receiver and undo this reset.
            self.incoming_generation
                .set(self.incoming_generation.get().wrapping_add(1));
            Ok(())
        })
    }
}

/// Serve-mode Bus handler (SPEC 18 WS3): a Mix script run as a
/// long-lived, supervised Bus daemon citizen via `mix --serve`.
///
/// Where [`MixBusHandler`] owns a *bare anonymous* `NodedClient`
/// (transient `mix <script>` mode — no reconnect, no registry, the
/// process dies with a broker bounce), `MixServeHandler` wraps the WS1
/// [`SupervisedClient`](cosmix_lib_client::SupervisedClient). Every
/// outbound call, `reply()`, and `subscribe()`/`unsubscribe()` routes
/// through the reconnect supervisor and the §3.3 subscription
/// registry; the incoming stream is the supervisor's **replaceable**
/// receiver, so a `cosmix-noded` restart is a *transient drop the
/// citizen serves through*, not the sticky terminal close that kills
/// the transient handler. `None` on that receiver means only a
/// **fatal** shutdown — which the pump correctly treats as exit. That
/// transient-vs-fatal distinction is the entire point of Phase 1
/// (finding #1 / §3.3).
///
/// Serve mode deliberately has **no** local-Unix-socket shortcut: the
/// citizen has exactly one connection with one registered identity
/// (its Bus name), so all `send`/`emit` go over the supervised broker
/// link — routing a subset through an unregistered local socket would
/// split its identity and is the legibility regression the substrate
/// criteria forbid.
pub struct MixServeHandler {
    supervised: std::sync::Arc<cosmix_lib_client::SupervisedClient>,
    /// The supervisor's outward incoming receiver, taken **once** at
    /// construction (it survives reconnects underneath). `None` here
    /// means it was already taken — a programming error — and
    /// `next_incoming` reports closed rather than silently going deaf.
    incoming: RefCell<Option<UnboundedReceiver<cosmix_lib_client::IncomingCommand>>>,
    /// Sticky "fatal shutdown / no receiver" flag (see [`MixBusHandler`]
    /// for the rationale; identical RAII discipline via [`ReceiverGuard`]).
    incoming_closed: RefCell<bool>,
    /// Sticky receiver-guard-corruption flag (the "impossible"
    /// reentrant-borrow case); kept distinct from `incoming_closed` so
    /// the diagnostic is unambiguous.
    incoming_broken: RefCell<bool>,
    /// Generation counter required by the shared [`ReceiverGuard`].
    /// Serve mode does not override `reconnect()` (the supervised
    /// client manages its own reconnect/replay), so this counter
    /// stays at 0 for the life of the handler — the generation-check
    /// in `Drop` is effectively a no-op for serve mode.
    incoming_generation: Cell<u64>,
    /// The fixed registered service name (the citizen's Bus `<d>`
    /// token, established at `--serve` launch by the supervised
    /// connect). Used to keep `register_as` honest: serve mode cannot
    /// re-register under a different name without lying.
    service_name: String,
}

impl MixServeHandler {
    /// Wrap a connected [`SupervisedClient`]. Takes its outward
    /// incoming receiver immediately; the supervisor keeps that
    /// receiver alive across reconnects.
    pub fn new(supervised: std::sync::Arc<cosmix_lib_client::SupervisedClient>) -> Self {
        let service_name = supervised.service_name().to_string();
        let incoming = supervised.incoming();
        MixServeHandler {
            supervised,
            incoming: RefCell::new(incoming),
            incoming_closed: RefCell::new(false),
            incoming_broken: RefCell::new(false),
            incoming_generation: Cell::new(0),
            service_name,
        }
    }
}

impl BusHandler for MixServeHandler {
    fn send<'a>(
        &'a self,
        target: &'a str,
        command: &'a str,
        args: &'a Value,
    ) -> Pin<Box<dyn Future<Output = MixResult<(i32, Value)>> + 'a>> {
        Box::pin(async move {
            // Header/body shape (`body=` present, or a SPEC-12 `*.props.*`
            // command — see `wants_header_routing`) → header routing;
            // otherwise JSON-body RPC. Same split as the transient handler,
            // but always over the supervised link.
            if let Value::Map(map) = args
                && wants_header_routing(command, map)
            {
                let (headers, body) = split_headers_body(map)?;
                // `send` AWAITS its reply, so use the reply-COLLECTING
                // `call_with_headers_raw`, not the fire-and-forget
                // `send_with_headers` (which drops the peer's response and
                // strands `$result` at nil). A TRANSPORT failure stays `Err`
                // (→ `$rc = -1`); a peer `rc >= 10` keeps its real status via
                // the shared rc-band mapping.
                return match self
                    .supervised
                    .call_with_headers_raw(target, command, &headers, &body)
                    .await
                {
                    Ok((rc, reply_body, error_header)) => {
                        Ok(headers_reply_to_result(rc, reply_body, error_header))
                    }
                    Err(e) => Err(mesh_unavailable(&format!(
                        "serve call_with_headers({target}, {command}) transport failure: {e}"
                    ))),
                };
            }
            let json_args = value_to_json(args);
            // call_typed keeps TRANSPORT failures (gate/disconnect/inner) as
            // `Err` (→ `$rc = -1`) while a peer `rc >= 10` reply preserves its
            // real status (→ `$rc = rc`), matching the rc-band contract (was
            // every Err → rc=10, conflating transport with application error).
            match self.supervised.call_typed(target, command, json_args).await {
                Ok(cosmix_lib_bus::PortReply::Ok { rc, value }) => {
                    Ok((i32::from(rc), json_to_value(&value)))
                }
                Ok(cosmix_lib_bus::PortReply::AppError { rc, message }) => {
                    Ok((i32::from(rc), Value::String(message)))
                }
                Err(e) => Err(mesh_unavailable(&format!(
                    "serve call({target}, {command}) transport failure: {e}"
                ))),
            }
        })
    }

    fn emit<'a>(
        &'a self,
        target: &'a str,
        command: &'a str,
        args: &'a Value,
    ) -> Pin<Box<dyn Future<Output = MixResult<()>> + 'a>> {
        Box::pin(async move {
            // Fire-and-forget: a disconnected supervised link drops the
            // emit (the typed error is swallowed, matching the
            // transient handler's broker-unavailable behaviour). `emit`
            // promises no delivery guarantee; a citizen that needs one
            // uses `send`/`call` and inspects the rc.
            if let Value::Map(map) = args {
                let (headers, body) = split_headers_body(map)?;
                let _ = self
                    .supervised
                    .send_with_headers(target, command, &headers, &body)
                    .await;
                return Ok(());
            }
            let json_args = value_to_json(args);
            let _ = self.supervised.send(target, command, json_args).await;
            Ok(())
        })
    }

    fn port_exists<'a>(
        &'a self,
        target: &'a str,
    ) -> Pin<Box<dyn Future<Output = MixResult<bool>> + 'a>> {
        Box::pin(async move {
            match self.supervised.list_services().await {
                Ok(services) => Ok(services.iter().any(|s| s == target)),
                // Disconnected / transport error: report "not
                // reachable" rather than a hard script error — the
                // citizen cannot vouch for a service it cannot ask
                // about. Mirrors the transient handler.
                Err(_) => Ok(false),
            }
        })
    }

    fn next_incoming<'a>(&'a self) -> Pin<Box<dyn Future<Output = Option<IncomingEvent>> + 'a>> {
        Box::pin(async move {
            if *self.incoming_closed.borrow() {
                return None;
            }
            if *self.incoming_broken.borrow() {
                tracing::error!(
                    service = %self.service_name,
                    "next_incoming: supervised receiver slot corrupted by guard drop \
                     failure; supervisor still running but unreachable via this handler"
                );
                return None;
            }
            if self.incoming.borrow().is_none() {
                // The receiver was never present (already taken before
                // this handler, a construction-time programming error).
                // Treat as closed — never silently deaf.
                *self.incoming_closed.borrow_mut() = true;
                return None;
            }

            let mut guard = match ReceiverGuard::take(
                &self.incoming,
                &self.incoming_broken,
                &self.incoming_generation,
            ) {
                Some(g) => g,
                None => return None,
            };
            let result = guard.as_mut().recv().await;
            drop(guard);

            match result {
                Some(cmd) => Some(IncomingEvent {
                    command: cmd.command,
                    headers: cmd.headers,
                    body: cmd.body,
                }),
                None => {
                    // §3.3: the supervisor's outward receiver yields
                    // `None` ONLY on a fatal shutdown — never on a
                    // transient drop (it forwards across reconnects).
                    // So this is a genuine terminal: mark closed; the
                    // pump exits.
                    *self.incoming_closed.borrow_mut() = true;
                    None
                }
            }
        })
    }

    fn register_as<'a>(
        &'a self,
        name: &'a str,
    ) -> Pin<Box<dyn Future<Output = MixResult<()>> + 'a>> {
        Box::pin(async move {
            // Serve mode's registered identity is fixed at `--serve`
            // launch (the supervised connect registered it, and the
            // supervisor *re-registers it* on every reconnect). A
            // re-register under the SAME name is idempotently true; a
            // DIFFERENT name cannot be honoured without lying — the
            // supervisor would still reconnect as the original name.
            // Returning a silent `Ok` there is exactly the
            // partial-truth shape the trait's hard-error defaults guard
            // against, so fail loudly instead.
            if name == self.service_name {
                Ok(())
            } else {
                Err(cosmix_mix::error::MixError::RuntimeError {
                    msg: format!(
                        "serve mode is registered as '{}' (fixed at --serve launch); \
                         noded_register('{}') cannot rename a supervised citizen",
                        self.service_name, name
                    ),
                    span: None,
                })
            }
        })
    }

    fn subscribe_topic<'a>(
        &'a self,
        name: &'a str,
    ) -> Pin<Box<dyn Future<Output = MixResult<()>> + 'a>> {
        Box::pin(async move {
            // The §3.3 registry-mutating chokepoint: records the topic
            // transactionally (only after an RC-0 broker subscribe) so
            // a reconnect replays it in recorded order.
            self.supervised.subscribe_topic(name).await.map_err(|e| {
                cosmix_mix::error::MixError::RuntimeError {
                    msg: format!("subscribe('{name}') failed: {e}"),
                    span: None,
                }
            })
        })
    }

    fn unsubscribe_topic<'a>(
        &'a self,
        name: &'a str,
    ) -> Pin<Box<dyn Future<Output = MixResult<()>> + 'a>> {
        Box::pin(async move {
            self.supervised.unsubscribe_topic(name).await.map_err(|e| {
                cosmix_mix::error::MixError::RuntimeError {
                    msg: format!("unsubscribe('{name}') failed: {e}"),
                    span: None,
                }
            })
        })
    }

    fn reply<'a>(
        &'a self,
        to: &'a str,
        command: &'a str,
        id: Option<&'a str>,
        rc: u8,
        body: &'a str,
    ) -> Pin<Box<dyn Future<Output = MixResult<()>> + 'a>> {
        Box::pin(async move {
            // WS-R correlation parts, captured by the evaluator off the
            // in-flight event, transmitted over the *supervised*
            // connection. A reply attempted while disconnected fails
            // fast with the typed error surfaced to the script — no
            // queue (§3.3); the script decides, never a silent drop.
            self.supervised
                .respond_parts(to, command, id, rc, body)
                .await
                .map_err(|e| cosmix_mix::error::MixError::RuntimeError {
                    msg: format!("reply failed: {e}"),
                    span: None,
                })
        })
    }

    fn reply_shutdown_synth<'a>(
        &'a self,
        to: &'a str,
        command: &'a str,
        id: Option<&'a str>,
        rc: u8,
        body: &'a str,
    ) -> Pin<Box<dyn Future<Output = MixResult<()>> + 'a>> {
        Box::pin(async move {
            // SPEC 18 Phase 2 WS3-C.7f — drain Phase 3 synth path.
            // Routes through `respond_parts_shutdown_synth` (NOT
            // `respond_parts`) so the supervised gate's
            // `ShuttingDown` rejection — atomically armed by
            // `deregister()` before its RPC — does not turn every
            // C.7f synth attempt into `synth_failed`. The WS is
            // still live (deregister is a single RPC, does not close
            // the socket), so the synth reply DOES reach the
            // pending caller. See [`BusHandler::reply_shutdown_synth`]
            // and [`SupervisedClient::respond_parts_shutdown_synth`]
            // for the gate-bypass scope rationale.
            self.supervised
                .respond_parts_shutdown_synth(to, command, id, rc, body)
                .await
                .map_err(|e| cosmix_mix::error::MixError::RuntimeError {
                    msg: format!("shutdown-synth reply failed: {e}"),
                    span: None,
                })
        })
    }
}

/// Split a Mix args Map into Bus headers + body.
///
/// Scalar values (String, Number, Bool) become Bus headers.
/// The "body" key becomes the Bus message body (markdown, JSON, template text).
/// Complex values (List, Map) other than "body" are JSON-serialized as headers.
///
/// `Value::Bytes` is rejected on the body slot rather than silently
/// stringified — the existing Bus wire shape carries body as UTF-8
/// text, so a `<bytes:N>` placeholder would mask the real "binary
/// body not supported" signal. Encode via `base64_encode($bytes)` (or
/// wait for a typed bytes body wire change) instead.
/// Should this `send` use header routing (scalar headers + body channel)
/// instead of JSON-body RPC framing?
///
/// Two triggers: an explicit `body=` key (the caller is speaking the
/// headers+body shape), or a SPEC-12 *namespace-mode* property call — a
/// `<svc>.props.<op>` command (incl. multi-segment ops like
/// `props.audit.watch`) whose args carry `namespace=`. The SPEC-12 surface
/// reads its arguments from Bus HEADERS (body reserved for row payloads/
/// projections), and `namespace` is its required discriminator header; a
/// kv-only namespace-mode `send` without `body=` used to fall through to
/// JSON-body framing, so the server saw no headers at all and refused with
/// "missing required header: namespace" (2026-07-17, `webd.props.delete`
/// against a live mesh service).
///
/// The `namespace=` condition is load-bearing: the SPEC-07 *flat-path* read
/// surface shares the `.props.` verb names (`noded.props.get path=…`,
/// `filesd.props.list`) but reads `path`/args from the JSON body — routing
/// those through headers would silently drop their args (the server would
/// answer for the root tree). Flat-path and every non-props verb keep the
/// JSON-body contract; `emit` already header-routes all map args.
fn wants_header_routing(command: &str, map: &IndexMap<String, Value>) -> bool {
    map.contains_key("body") || (command.contains(".props.") && map.contains_key("namespace"))
}

fn split_headers_body(
    map: &IndexMap<String, Value>,
) -> MixResult<(BTreeMap<String, String>, String)> {
    let mut headers = BTreeMap::new();
    let mut body = String::new();

    for (k, v) in map {
        if k == "body" {
            if matches!(v, Value::Bytes(_) | Value::Buffer(_)) {
                return Err(cosmix_mix::error::MixError::RuntimeError {
                    span: None,
                    msg: "bus: `body=` does not accept bytes/buffer; base64_encode($v) first \
                          (typed bytes body is a future wire-format change)"
                        .to_string(),
                });
            }
            body = v.to_mix_string();
            continue;
        }
        let header_val = match v {
            Value::String(s) => s.clone(),
            Value::Number(n) => {
                if *n == (*n as i64) as f64 {
                    format!("{}", *n as i64)
                } else {
                    format!("{n}")
                }
            }
            Value::Bool(b) => b.to_string(),
            Value::Nil => continue,
            Value::Bytes(_) | Value::Buffer(_) => {
                return Err(cosmix_mix::error::MixError::RuntimeError {
                    span: None,
                    msg: format!(
                        "bus: header `{k}` does not accept bytes/buffer; \
                         base64_encode($v) first"
                    ),
                });
            }
            other => serde_json::to_string(&value_to_json(other)).unwrap_or_default(),
        };
        headers.insert(k.clone(), header_val);
    }

    Ok((headers, body))
}

/// Convert a Mix Value to serde_json::Value.
fn value_to_json(val: &Value) -> serde_json::Value {
    match val {
        Value::String(s) => serde_json::Value::String(s.clone()),
        // Mix has a single numeric type (f64). Emit whole numbers as JSON
        // *integers* rather than floats: `limit=2` must reach a peer as `2`,
        // not `2.0`. serde rejects a JSON float for an integer-typed field
        // (`#[serde] usize`/`i64`), so the float form turns every integer
        // Bus arg — `indexd.search limit=`, `.list offset=`, `.delete ids=`,
        // any daemon `id=` — into a `rc=10 invalid type: floating point`
        // deserialize failure. Only fractional / non-finite / beyond-2^53
        // (not exactly representable) values stay floats.
        Value::Number(n) => {
            let f = *n;
            if f.is_finite() && f.fract() == 0.0 && f.abs() < 9_007_199_254_740_992.0 {
                serde_json::json!(f as i64)
            } else {
                serde_json::json!(f)
            }
        }
        Value::Bool(b) => serde_json::Value::Bool(*b),
        Value::Nil => serde_json::Value::Null,
        Value::List(items) => serde_json::Value::Array(items.iter().map(value_to_json).collect()),
        Value::Map(map) => {
            let obj: serde_json::Map<String, serde_json::Value> = map
                .iter()
                .map(|(k, v)| (k.clone(), value_to_json(v)))
                .collect();
            serde_json::Value::Object(obj)
        }
        // Lambdas are not serializable over Bus — emit Null rather
        // than panic. A script that sends a map containing a function
        // value will see the field as null on the peer.
        Value::Function(_) => serde_json::Value::Null,
        // Bytes have no native JSON type. Same policy as Function:
        // emit Null instead of panicking; callers who genuinely need
        // bytes on the wire must `base64_encode($bytes)` themselves
        // (and base64_decode on the peer). A typed Bus bytes envelope
        // is a separate future change.
        Value::Bytes(_) => serde_json::Value::Null,
        // Same policy as Bytes — a mutable byte buffer has no JSON/Bus
        // wire form; `base64_encode(freeze($buf))` to send it explicitly.
        Value::Buffer(_) => serde_json::Value::Null,
    }
}

/// Convert a serde_json::Value to a Mix Value.
fn json_to_value(val: &serde_json::Value) -> Value {
    match val {
        serde_json::Value::Null => Value::Nil,
        serde_json::Value::Bool(b) => Value::Bool(*b),
        serde_json::Value::Number(n) => Value::Number(n.as_f64().unwrap_or(0.0)),
        serde_json::Value::String(s) => Value::String(s.clone()),
        serde_json::Value::Array(arr) => Value::list(arr.iter().map(json_to_value).collect()),
        serde_json::Value::Object(obj) => {
            let map: indexmap::IndexMap<String, Value> = obj
                .iter()
                .map(|(k, v)| (k.clone(), json_to_value(v)))
                .collect();
            Value::map(map)
        }
    }
}

/// Map a reply-awaiting `call_with_headers_raw` triple `(rc, body,
/// error_header)` into the `(i32, Value)` result the `send` keyword yields,
/// applying the SAME rc-band contract as the JSON-body `call_typed` path so a
/// `body=`-bearing send round-trips its reply exactly like a positional or
/// scalar-header send:
///
/// * `rc >= 10` — an application error: `$rc` keeps the EXACT peer rc and
///   `$result` is the error MESSAGE, resolved with `call_with_headers`'s
///   precedence (body `message` field → body `error` field → response `error`
///   header → `rc=N (no error body)` sentinel).
/// * `rc < 10` — success or a warning: `$rc` keeps the rc and `$result` is the
///   JSON-parsed body (empty → Nil; non-JSON → the verbatim String), matching
///   `call_typed`'s body handling.
fn headers_reply_to_result(rc: u8, body: String, error_header: Option<String>) -> (i32, Value) {
    if rc >= 10 {
        let parsed: Option<serde_json::Value> = if body.is_empty() {
            None
        } else {
            serde_json::from_str(&body).ok()
        };
        let from_body = parsed
            .as_ref()
            .and_then(|v| v.get("message").or_else(|| v.get("error")))
            .and_then(|v| v.as_str())
            .map(str::to_string);
        let message = from_body.or(error_header).unwrap_or_else(|| {
            if body.is_empty() {
                format!("rc={rc} (no error body)")
            } else {
                body
            }
        });
        return (i32::from(rc), Value::String(message));
    }
    let value = if body.is_empty() {
        Value::Nil
    } else {
        match serde_json::from_str::<serde_json::Value>(&body) {
            Ok(j) => json_to_value(&j),
            Err(_) => Value::String(body),
        }
    };
    (i32::from(rc), value)
}

#[cfg(test)]
mod routing_tests {
    //! Wire-framing selection for `send` — header routing vs JSON-body RPC.
    use super::*;

    fn kv(pairs: &[(&str, &str)]) -> IndexMap<String, Value> {
        pairs
            .iter()
            .map(|(k, v)| (k.to_string(), Value::String(v.to_string())))
            .collect()
    }

    #[test]
    fn spec12_namespace_mode_header_routes_without_body() {
        // The 2026-07-17 live-mesh failure shape: kv-only props.delete.
        let args = kv(&[("namespace", "handlers"), ("key", "x"), ("if_version", "1")]);
        assert!(wants_header_routing("webd.props.delete", &args));
        assert!(wants_header_routing("webd.props.get", &args));
        // Multi-segment SPEC-12 ops stay on the surface.
        assert!(wants_header_routing("maild.props.audit.watch", &args));
    }

    #[test]
    fn spec07_flat_path_reads_stay_json_body() {
        // SPEC-07 flat-path shares the `.props.` verb names but reads
        // `path`/args from the JSON body — MUST NOT header-route.
        let flat = kv(&[("path", "lifecycle.model_loaded")]);
        assert!(!wants_header_routing("noded.props.get", &flat));
        assert!(!wants_header_routing("statecache.props.get", &flat));
        // Arg-less list (e.g. `send svc filesd.props.list`) stays JSON-body.
        let none: IndexMap<String, Value> = IndexMap::new();
        assert!(!wants_header_routing("filesd.props.list", &none));
    }

    #[test]
    fn explicit_body_header_routes_any_verb() {
        let args = kv(&[("namespace", "handlers"), ("body", "")]);
        assert!(wants_header_routing("noded.anything", &args));
    }

    #[test]
    fn non_props_kv_stays_json_body() {
        // noded/indexd/maild verbs read args from the JSON body — the
        // default must NOT flip for them.
        let args = kv(&[("limit", "2"), ("note", "hello")]);
        assert!(!wants_header_routing("noded.ping", &args));
        assert!(!wants_header_routing("indexd.search", &args));
        // A verb merely ENDING in `.props` (no op segment) is not the surface.
        assert!(!wants_header_routing("svc.props", &args));
    }
}

#[cfg(test)]
mod tests {
    //! State-machine tests for `MixBusHandler`. Covers everything that
    //! does *not* need a live broker on the loopback port: the
    //! `NeverPresent` (silent-nil) and `Lost` (loud-raise) branches,
    //! plus `reconnect()`'s reset-to-Unprobed semantic. The
    //! `Connected → Lost` transition path (a call against a previously
    //! live broker failing) requires a real WebSocket peer and is
    //! covered by the live cosmix-noded integration tests, not here.
    //!
    //! The probe itself (`Unprobed → Connected | NeverPresent` via
    //! `NodedClient::connect_anonymous(&resolve_noded_url())`) is
    //! intentionally *not* unit-tested: it dials the real loopback
    //! broker URL on whatever host the tests run on, and a host with
    //! a `node.conf.mix` present would dial whatever broker that file
    //! points at. Probe correctness is verified at the integration
    //! layer instead.
    use super::*;

    impl MixBusHandler {
        async fn test_force_state(&self, s: MeshState) {
            *self.mesh.lock().await = s;
        }

        async fn test_state_label(&self) -> &'static str {
            match &*self.mesh.lock().await {
                MeshState::Unprobed => "Unprobed",
                MeshState::NeverPresent => "NeverPresent",
                MeshState::Connected(_) => "Connected",
                MeshState::Lost => "Lost",
            }
        }
    }

    #[tokio::test(flavor = "current_thread")]
    async fn new_handler_starts_unprobed() {
        let h = MixBusHandler::new();
        assert_eq!(h.test_state_label().await, "Unprobed");
    }

    #[test]
    fn whole_numbers_serialize_as_json_integers() {
        // Regression guard: Mix's single f64 numeric type must reach the
        // wire as a JSON integer for whole values, else a peer field typed
        // `usize`/`i64` rejects the `2.0` float form ("invalid type:
        // floating point `2.0`, expected usize" => rc=10). See the
        // value_to_json comment.
        assert_eq!(value_to_json(&Value::Number(2.0)).to_string(), "2");
        assert_eq!(value_to_json(&Value::Number(0.0)).to_string(), "0");
        assert_eq!(value_to_json(&Value::Number(-7.0)).to_string(), "-7");
        // Fractional values stay floats.
        assert_eq!(value_to_json(&Value::Number(2.5)).to_string(), "2.5");
        // Non-finite values are not integers — stay as the float form
        // (serde_json renders these as JSON null, but must not panic or
        // be mis-coerced to an integer).
        assert!(!value_to_json(&Value::Number(f64::INFINITY)).is_i64());
        assert!(!value_to_json(&Value::Number(f64::NAN)).is_i64());
    }

    // --- headers_reply_to_result: the `send body=` reply mapping ---
    // Regression guard for the `send TARGET cmd body="…"` fire-and-forget
    // bug: a body-bearing send used to hard-return `(0, Nil)` and never
    // collect the reply. Now it routes through `call_with_headers_raw` and
    // this helper folds the raw `(rc, body, error_header)` into the same
    // rc-band `(rc, $result)` shape as the JSON-body `call_typed` path.

    #[test]
    fn headers_reply_success_json_object_body() {
        // A success reply with a JSON body → the parsed Map reaches `$result`,
        // rc preserved (this is exactly what a `reply({...})` handler yields).
        let (rc, v) = headers_reply_to_result(0, r#"{"pong":true,"n":2}"#.to_string(), None);
        assert_eq!(rc, 0);
        match v {
            Value::Map(ref m) => {
                assert_eq!(m.get("pong"), Some(&Value::Bool(true)));
                assert_eq!(m.get("n"), Some(&Value::Number(2.0)));
            }
            other => panic!("expected Map, got {other:?}"),
        }
    }

    #[test]
    fn headers_reply_success_empty_body_is_nil() {
        // An accepted reply with no body → `$result` is nil, NOT a fake
        // success string. Matches `call_typed`'s empty-body → Null → Nil.
        assert_eq!(
            headers_reply_to_result(0, String::new(), None),
            (0, Value::Nil)
        );
    }

    #[test]
    fn headers_reply_success_non_json_body_is_verbatim_string() {
        // A non-JSON payload (e.g. `reply("pong")` surfacing bare text) falls
        // back to the verbatim String, same as `call_typed`.
        assert_eq!(
            headers_reply_to_result(0, "pong".to_string(), None),
            (0, Value::String("pong".to_string()))
        );
    }

    #[test]
    fn headers_reply_warning_rc_is_preserved() {
        // A warning-band rc (1..9) is still success — the exact rc must
        // survive to `$rc`, not be flattened to 0.
        assert_eq!(
            headers_reply_to_result(5, "\"ok\"".to_string(), None),
            (5, Value::String("ok".to_string()))
        );
    }

    #[test]
    fn headers_reply_app_error_uses_body_message_field() {
        // rc >= 10 is an application error: `$rc` keeps the exact peer rc
        // and `$result` is the structured `message` field.
        let (rc, v) = headers_reply_to_result(
            42,
            r#"{"message":"nope","error":"secondary"}"#.to_string(),
            None,
        );
        assert_eq!(rc, 42);
        assert_eq!(v, Value::String("nope".to_string()));
    }

    #[test]
    fn headers_reply_app_error_falls_back_to_error_field_then_header() {
        // No `message` → the body `error` field wins over the header.
        let (rc, v) = headers_reply_to_result(
            10,
            r#"{"error":"from_body"}"#.to_string(),
            Some("from_header".to_string()),
        );
        assert_eq!((rc, v), (10, Value::String("from_body".to_string())));
        // Empty body → the response `error` header carries the token.
        let (rc, v) = headers_reply_to_result(10, String::new(), Some("from_header".to_string()));
        assert_eq!((rc, v), (10, Value::String("from_header".to_string())));
    }

    #[test]
    fn headers_reply_app_error_sentinel_when_nothing_structured() {
        // rc >= 10 with an empty body and no error header → the sentinel,
        // never a silent success.
        assert_eq!(
            headers_reply_to_result(10, String::new(), None),
            (10, Value::String("rc=10 (no error body)".to_string()))
        );
    }

    #[tokio::test(flavor = "current_thread")]
    async fn never_present_send_is_rc_unavailable() {
        let h = MixBusHandler::new();
        h.test_force_state(MeshState::NeverPresent).await;
        // Target contains a dot → no local socket short-circuit.
        let result = h.send("svc.example", "do_something", &Value::Nil).await;
        // A bare host: NON-FATAL "not delivered", NOT a fake (0, Nil) success
        // (2026-07-02 rc-band unification). rc=-3 = RC_UNAVAILABLE.
        assert_eq!(
            result.unwrap(),
            (
                RC_UNAVAILABLE,
                Value::String("Bus unavailable: broker never present".to_string())
            )
        );
        // State unchanged after the call.
        assert_eq!(h.test_state_label().await, "NeverPresent");
    }

    #[tokio::test(flavor = "current_thread")]
    async fn never_present_emit_silently_ok() {
        let h = MixBusHandler::new();
        h.test_force_state(MeshState::NeverPresent).await;
        let result = h.emit("svc.example", "do_something", &Value::Nil).await;
        assert!(result.is_ok());
        assert_eq!(h.test_state_label().await, "NeverPresent");
    }

    #[tokio::test(flavor = "current_thread")]
    async fn never_present_port_exists_returns_false() {
        let h = MixBusHandler::new();
        h.test_force_state(MeshState::NeverPresent).await;
        let result = h.port_exists("svc.example").await;
        assert!(!result.unwrap());
    }

    #[tokio::test(flavor = "current_thread")]
    async fn never_present_register_as_raises() {
        let h = MixBusHandler::new();
        h.test_force_state(MeshState::NeverPresent).await;
        let result = h.register_as("test-svc").await;
        let err = result.unwrap_err();
        let msg = format!("{err}");
        assert!(
            msg.contains("mesh unavailable"),
            "expected 'mesh unavailable' in error, got: {msg}"
        );
        assert!(msg.contains("register_as"));
    }

    #[tokio::test(flavor = "current_thread")]
    async fn never_present_subscribe_topic_raises() {
        let h = MixBusHandler::new();
        h.test_force_state(MeshState::NeverPresent).await;
        let result = h.subscribe_topic("test.topic").await;
        let msg = format!("{}", result.unwrap_err());
        assert!(msg.contains("mesh unavailable"));
        assert!(msg.contains("subscribe_topic"));
    }

    #[tokio::test(flavor = "current_thread")]
    async fn lost_send_raises() {
        let h = MixBusHandler::new();
        h.test_force_state(MeshState::Lost).await;
        let result = h.send("svc.example", "do_something", &Value::Nil).await;
        let msg = format!("{}", result.unwrap_err());
        assert!(msg.contains("mesh unavailable"));
        assert!(msg.contains("bus_reconnect"));
    }

    #[tokio::test(flavor = "current_thread")]
    async fn lost_emit_raises() {
        let h = MixBusHandler::new();
        h.test_force_state(MeshState::Lost).await;
        let result = h.emit("svc.example", "do_something", &Value::Nil).await;
        assert!(result.is_err());
        let msg = format!("{}", result.unwrap_err());
        assert!(msg.contains("mesh unavailable"));
    }

    #[tokio::test(flavor = "current_thread")]
    async fn lost_port_exists_raises() {
        let h = MixBusHandler::new();
        h.test_force_state(MeshState::Lost).await;
        let result = h.port_exists("svc.example").await;
        assert!(result.is_err());
    }

    #[tokio::test(flavor = "current_thread")]
    async fn reconnect_resets_never_present_to_unprobed() {
        let h = MixBusHandler::new();
        h.test_force_state(MeshState::NeverPresent).await;
        h.reconnect().await.unwrap();
        assert_eq!(h.test_state_label().await, "Unprobed");
    }

    #[tokio::test(flavor = "current_thread")]
    async fn reconnect_resets_lost_to_unprobed() {
        let h = MixBusHandler::new();
        h.test_force_state(MeshState::Lost).await;
        h.reconnect().await.unwrap();
        assert_eq!(h.test_state_label().await, "Unprobed");
    }

    #[tokio::test(flavor = "current_thread")]
    async fn reconnect_clears_incoming_closed_flag() {
        // SPEC 18 / Codex round-2 finding: `next_incoming` sets
        // `incoming_closed = true` when it hits NeverPresent or Lost.
        // Without this, a subsequent `bus_reconnect()` + successful
        // re-probe would leave the receiver-stream permanently dead
        // because `next_incoming` short-circuits on the sticky flag.
        let h = MixBusHandler::new();
        // Simulate the post-NeverPresent state where next_incoming
        // closed the stream.
        h.test_force_state(MeshState::NeverPresent).await;
        *h.incoming_closed.borrow_mut() = true;
        *h.incoming_broken.borrow_mut() = true; // also flip the
        // distinct guard-corruption flag, both should clear.

        let pre_gen = h.incoming_generation.get();
        h.reconnect().await.unwrap();
        assert_eq!(h.test_state_label().await, "Unprobed");
        assert!(!*h.incoming_closed.borrow());
        assert!(!*h.incoming_broken.borrow());
        assert!(h.incoming.borrow().is_none());
        // Generation must advance so any in-flight ReceiverGuard
        // restoring an old receiver is detected as stale.
        assert_eq!(h.incoming_generation.get(), pre_gen + 1);
    }

    #[tokio::test(flavor = "current_thread")]
    async fn receiver_guard_drop_after_reconnect_drops_stale_receiver() {
        // Codex round-3 finding: an in-flight `next_incoming` guard
        // could survive a `reconnect()` and restore the stale receiver
        // into the cleared slot, undoing the reset. The generation
        // counter on `ReceiverGuard` prevents this — verify by:
        //   1. Constructing a guard around a stub receiver.
        //   2. Bumping the generation (simulating reconnect).
        //   3. Dropping the guard.
        //   4. Asserting the slot stays None.
        use tokio::sync::mpsc;

        let h = MixBusHandler::new();
        // Seed the incoming slot with a stub receiver.
        let (_tx, rx) = mpsc::unbounded_channel::<cosmix_lib_client::IncomingCommand>();
        *h.incoming.borrow_mut() = Some(rx);

        // Take into a guard.
        let guard = ReceiverGuard::take(&h.incoming, &h.incoming_broken, &h.incoming_generation)
            .expect("slot must hold a receiver");
        assert!(h.incoming.borrow().is_none(), "take() empties the slot");

        // Simulate `bus_reconnect()` bumping the generation between
        // take() and drop() — the guard captured the pre-bump value.
        h.incoming_generation
            .set(h.incoming_generation.get().wrapping_add(1));

        // Drop the guard. Because the generation advanced, the guard
        // should drop the receiver silently rather than restoring it.
        drop(guard);

        assert!(
            h.incoming.borrow().is_none(),
            "stale receiver must NOT be restored into the slot after a reconnect"
        );
        // The broken_flag must NOT be set — this is the "stale" path,
        // not the "RefCell-already-borrowed" path.
        assert!(!*h.incoming_broken.borrow());
    }
}
