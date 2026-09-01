# cosmix-lib-client — broker WebSocket client

**`cosmix-lib-client` connects Rust code to `cosmix-noded`.** The exported
`NodedClient` builds and correlates Bus requests over WebSocket; its exact
surface depends on whether the target is native or `wasm32`.

The caller supplies the broker URL. URL discovery from node configuration is a
cos substrate concern and is not part of this crate.

## What it is

On native targets, `NodedClient::connect(service, url)` opens the WebSocket and
registers the service name with `noded.register`. `connect_with_provenance`
also sends a `RegisterProvenance` record, while `connect_anonymous` opens a
call-capable connection without registering a name.

Named connection setup is cancellation-safe. If an outer timeout or task
cancellation drops the connect future while `noded.register` is awaiting its
reply, the half-built client's socket reader is aborted and the socket closes;
retries cannot accumulate orphan reader tasks. Once a client has been returned,
call `close()` for deterministic teardown because plain drop does not abort its
long-lived reader.

Incoming non-response messages become `IncomingCommand` values. They expose
`from`, `command`, `id`, parsed JSON `args`, the original body, and every Bus
header. A client takes the incoming receiver once with `incoming()` or
`incoming_async()` and replies with `respond()` or `respond_parts()`.

## Sending and calling

The native client exposes:

- `send(to, command, args)` — fire and forget; JSON-serialises non-null arguments.
- `call(to, command, args)` — sends a correlated request and waits up to 60 seconds for its response.
- `call_typed` — keeps application replies (`PortReply::AppError`) distinct from transport errors.
- `send_with_headers`, `call_with_headers`, and `call_with_headers_raw` — explicit header and body forms for protocol-specific verbs.
- `send_raw` — sends an already-built `BusMessage`.
- `list_services` and `service_inventory` — broker registry names or full `ServiceInfo` records.
- `register_as`, `deregister`, `is_connected`, and `close` — connection lifecycle primitives.

`call` returns a decoded JSON value, `Null` for an empty body, or a string when
the native peer returns a non-JSON body. A peer `rc >= 10` becomes an error;
use `call_typed` or `call_with_headers_raw` when the exact application return
code or error body matters.

## Subscriptions and supervision

The native-only `SupervisedClient` is the resident-service wrapper. Construct
it with `connect_supervised` or `connect_supervised_with_provenance`. It:

- limits the initial connect-and-register sequence to `MAX_INITIAL_ATTEMPTS` (five);
- reconnects indefinitely after a later transport loss, using exponential backoff with full jitter;
- keeps one outward incoming receiver alive across reconnects;
- fails outbound work immediately with a typed `SupervisedError` while disconnected; and
- re-registers and replays confirmed subscriptions in recorded order before returning to `ConnState::Connected`.

The compatibility default remains an unbounded reconnect-stable receiver from
`incoming()`. A subscriber that needs a hard memory bound can instead use
`connect_options(...).bounded_incoming(capacity)` and take
`incoming_bounded()`. Both the socket-reader lane and the reconnect-stable
outward lane then use non-blocking `try_send`: a full lane drops the new
command, increments `overflow_count()`, and emits one
`BoundedIncomingEvent::Overflow { dropped }` observation for the accumulated
loss. State-reconstructing consumers should invalidate conservatively before
accepting later commands.

Use `subscribe_topic(topic)` and `unsubscribe_topic(topic)` on
`SupervisedClient`. These call the broker's `topic.subscribe` /
`topic.unsubscribe` verbs and update the `SubscriptionRegistry` only after a
successful reply. Plain `NodedClient` has no `subscribe` method; callers using
it directly must issue the topic verb themselves, normally through
`call_with_headers`.

`connection_generation()` is the reliable reconnect signal. It starts at `1`
after the initial connection and increments only after a new socket is
registered and all recorded subscriptions have been replayed. Comparing this
monotonic value cannot miss a fast disconnect/reconnect that occurs between
two samples of `state()`.

## Native and browser targets

| Target | Backend | Surface |
|---|---|---|
| native (default) | Tokio + `tokio-tungstenite` | Named or anonymous connections, send/call, incoming commands, replies, discovery, subscriptions through `SupervisedClient`, and supervised reconnect. |
| `wasm32` | `gloo-net` | Anonymous, call-only `NodedClient`; no registration, incoming-command receiver, send, subscriptions, or supervisor. |

The browser client provides `connect_anonymous`, `call`, `is_connected`,
`noded_url_from_origin`, and `connect_anonymous_default`. Its origin helper
maps HTTP(S) page origins to `ws://` / `wss://` on `/ws`.

## Minimal native use

```rust
use cosmix_client::NodedClient;
use serde_json::json;

# async fn run() -> anyhow::Result<()> {
let client = NodedClient::connect(
    "my-service",
    "ws://127.0.0.1:4200/ws",
).await?;
let pong = client.call("noded", "noded.ping", json!(null)).await?;
# Ok(())
# }
```

## See also

- [wire format](wire-format.md) — `BusMessage`, addressing, and return codes
- [build information](buildinfo.md) — values used to construct registration provenance
- [overview](overview.md) — where the broker and configuration layers live
