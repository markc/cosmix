# cosmix-lib-client

`cosmix-lib-client` is the async Bus WebSocket client library for connecting
Cosmix applications to a `cosmix-noded` broker. It occupies the `bus` end of
the `bus <- mix <- cos` dependency chain: it depends on `cosmix-lib-bus` for
wire messages and exposes broker-client primitives without depending on
`mix`, `cos`, storage, or configuration loaders.

## Synopsis

The Cargo package is `cosmix-lib-client`; Rust code imports `cosmix_client`.

The crate selects its `NodedClient` implementation by compilation target:

| Target | Client model |
|---|---|
| Non-WASM | Named or anonymous Tokio client with incoming commands and replies |
| `wasm32` | Anonymous, call-only browser client |

Native builds also export the reconnecting `SupervisedClient`. The crate has no
binary, command-line interface, or configuration file format.

## Native client

`NodedClient` is the direct, one-connection client for non-WASM targets.

| Method | Purpose |
|---|---|
| `connect` | Connect with a caller-supplied URL and register a service name |
| `connect_with_provenance` | Register a name and optional build provenance |
| `connect_anonymous` | Connect without registering a service name |
| `register_as` | Re-register the current connection under a new name |

The caller supplies the broker URL. The native client does not read a config file
and does not discover a broker URL.

Request methods:

| Method | Result |
|---|---|
| `call` | Sends JSON arguments and returns a JSON value |
| `call_typed` | Separates transport failure from a Bus application error |
| `call_with_headers` | Sends caller headers and a verbatim body, then parses the reply |
| `call_with_headers_raw` | Returns `(rc, body, error_header)` without collapsing application errors |
| `send` | Sends JSON arguments without waiting for a reply |
| `send_with_headers` | Sends explicit Bus headers and a verbatim body without waiting |
| `send_raw` | Sends an already-built `BusMessage` |

`call` treats a Bus return code of 10 or greater as an error. `call_typed`
instead returns `PortReply::AppError` for that reply and reserves `Err` for
transport failures. Successful typed replies retain the Bus return code,
including warning codes.

The native request path correlates replies by message ID. A call waits for at
most 60 seconds. Cancelling removes its pending entry. Unmatched replies are
discarded.

`call_with_headers` protects the framing headers `command`, `from`, `to`, `type`,
and `id` from caller overrides. `call_with_headers_raw` leaves the response body
unparsed and preserves its optional `error` header.

## Incoming commands and replies

`incoming` and `incoming_async` transfer ownership of the client's incoming
command receiver. The receiver can be taken once.

Each `IncomingCommand` contains:

| Field | Meaning |
|---|---|
| `from` | Sending service |
| `command` | Bus command name |
| `id` | Optional request correlation ID |
| `args` | Body parsed as JSON, or `null` when absent or invalid |
| `body` | Original body text |
| `headers` | All Bus headers |

`IncomingCommand::header` reads any preserved header. `target`, `parent`, and
`source` are shortcuts. `is_ui_command` tests for a `ui.` command prefix.

Use `respond` for an `IncomingCommand`; use `respond_parts` with its correlation
parts.

## Discovery and lifecycle

`list_services` returns registered service names. `service_inventory` returns
`cosmix_bus::ServiceInfo` values with the broker's full service metadata.

`is_connected` reports the connection flag. `deregister` removes the registered
name and clears the local name after success. `close` performs best-effort
WebSocket shutdown, stops the reader task, marks the client disconnected, and
releases pending callers.

Dropping a direct `NodedClient` does not by itself provide deterministic
socket teardown. Call `close` when that guarantee is required.

## Supervised client

`SupervisedClient` wraps a native `NodedClient` for resident services. Create it
with `connect_supervised` or `connect_supervised_with_provenance`.

The initial connection has a bounded `MAX_INITIAL_ATTEMPTS` budget of five;
exhaustion returns `SupervisedError::InitialConnectFailed`.

After an established connection is lost, the supervisor reconnects without an
attempt limit. It uses full-jitter exponential backoff from 250 milliseconds
to a 30-second ceiling. A successful reconnect re-registers the service,
re-sends optional provenance, and replays recorded topic subscriptions in
their original order.

The outward receiver returned by `incoming` survives transient reconnects.
`state` returns `ConnState`, and `connection_generation` increments after each
fully established connection. The states are `Connecting`, `Connected`,
`Disconnected`, `ShuttingDown`, and `Fatal`.

Outbound supervised methods fail immediately unless the state is
`Connected`. The client does not queue work while disconnected.
`SupervisedError::Disconnected` and `SupervisedError::ShuttingDown` make
those states distinguishable from `SupervisedError::Transport`.

The supervised client proxies the direct client's call, send, response,
header, and service-list operations.

`shutdown` stops the supervisor without deregistering. `deregister` first
stops the supervisor, then deregisters over the remaining live connection.

## Topic subscriptions

`subscribe_topic` calls `topic.subscribe` and records the topic only after the
broker accepts the subscription. `unsubscribe_topic` calls
`topic.unsubscribe` and removes the topic only after broker acceptance.

`SubscriptionRegistry` is a cloneable, shared, ordered set. It provides
`record`, `remove`, `snapshot`, `len`, and `is_empty`. Duplicate records are
ignored. Removing and later recording a topic appends it at the end of replay
order.

## Browser client

On `wasm32`, `NodedClient` is an anonymous call-only client based on the
browser WebSocket API. It does not register a service and does not receive
incoming commands.

`connect_anonymous` accepts an explicit WebSocket URL.
`connect_anonymous_default` derives `/ws` from the current page origin, using
`wss` for HTTPS pages and `ws` otherwise. `call` sends a JSON request and
waits for its correlated response. `is_connected` reports whether the reader
loop remains live.

The browser client treats return codes of 10 or greater as errors and expects a
non-empty successful response body to contain valid JSON.

## Cargo features

| Feature | Default | Effect |
|---|---|---|
| `native` | Yes | Enables Tokio, Tokio Tungstenite, native futures utilities, jitter generation, and `cosmix-lib-bus` native support |

The browser backend is selected by the `wasm32` target, not by a `web` Cargo
feature. WASM dependencies include `gloo-net`, `wasm-bindgen-futures`,
`web-sys`, `futures-channel`, and `futures-util`.

## Example

```rust
use cosmix_client::{NodedClient, PortReply};

#[tokio::main]
async fn main() -> anyhow::Result<()> {
let client =
    NodedClient::connect("alpha", "wss://broker.example.com/ws").await?;

match client
    .call_typed("beta", "status.get", serde_json::Value::Null)
    .await?
{
    PortReply::Ok { rc, value } => println!("rc={rc} value={value}"),
    PortReply::AppError { rc, message } => {
        eprintln!("application error rc={rc}: {message}");
    }
}

client.close().await;
Ok(())
}
```

`PortReply` is re-exported on non-WASM targets. The direct client returns
`anyhow::Result`; the supervised client uses `SupervisedError` for lifecycle
and transport failures.

## Dependency boundary

The crate always depends on `cosmix-lib-bus` with that dependency's default
features disabled. It also uses `serde`, `serde_json`, `tracing`, and
`anyhow`.

Configuration loading, persistent state, broker deployment, and service
implementations remain outside this crate. Native callers provide an explicit
broker URL; browser callers may derive one only from the current page origin.
