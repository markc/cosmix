# bus

**CosMix Agent Bus (the Bus)** — a pure-Rust library family for messaging across a WireGuard-secured mesh of agent-operable services.

Bus follows the AmigaOS ARexx convention: every service in the mesh exposes a named, addressable port; any peer can `send` it a command, `call` it for a reply, or subscribe to its event topics. The broker (`cosmix-noded`, in [cos](https://github.com/markc/cos)) routes messages between peers; everything inside the mesh is trusted on a per-WireGuard-subnet basis. bus is the protocol library that lets a service participate.

## Crates

| Crate | What it is |
|---|---|
| **`cosmix-lib-bus`** | Wire format. `BusMessage` request/reply/event types, serialisation independent of target. The `native` feature adds Unix-socket helpers used by the broker; client code typically leaves the default features on. |
| **`cosmix-lib-client`** | `NodedClient` — broker WebSocket client. Native (`tokio` + `tokio-tungstenite`) and `wasm32` (`gloo-net`) targets. Caller supplies the broker URL. |
| **`cosmix-lib-props-core`** | SPEC 07 property read surface — `PropTree`, `PropPath`, `PropValue` by default; the `dispatch_props` Bus-wire handler and `publish::*` event builders gated behind the opt-in `bus` feature. Paired with the substrate-side `cosmix-lib-props-store` in cos. |
| **`cosmix-lib-log`** | Tracing/logging init and the stats pipeline (Bus-message counters, Prometheus/JSONL sinks) shared by every daemon. |
| **`cosmix-lib-buildinfo`** | Build metadata (version, git revision) embedded into binaries at compile time. |

bus deliberately holds the *protocol* layer only — no storage, no TLS, no config-file loading, no auto-resolve of broker URLs. Anything substrate-shaped lives in cos.

## Building

bus builds standalone — no sibling checkouts required.

```sh
git clone https://github.com/markc/bus $COSMIX
cd $COSMIX/src && cargo build --workspace
```

Tests:

```sh
cd $COSMIX/src && cargo test --workspace
```

Lints:

```sh
cd $COSMIX/src && cargo clippy --workspace --all-targets -- -D warnings
```

## Using as a library

bus is not on crates.io yet. Cargo's git dependency form has no sub-directory selector and this workspace lives under `src/`, so depend on it via path-deps to a sibling checkout:

```sh
git clone https://github.com/markc/bus $COSMIX
```

```toml
[dependencies]
cosmix-lib-bus    = { path = "../../bus/src/crates/cosmix-lib-bus" }
cosmix-lib-client = { path = "../../bus/src/crates/cosmix-lib-client" }
```

(Once the crates publish, depend via version: `cosmix-lib-bus = "0.1"` etc.)

Minimal `NodedClient` usage:

```rust
use cosmix_client::NodedClient;
use serde_json::json;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let client = NodedClient::connect("my-service", "ws://localhost:4200/ws").await?;
    let pong = client.call("noded", "noded.ping", json!(null)).await?;
    println!("pong: {pong}");
    Ok(())
}
```

`examples/test_noded.rs` under `cosmix-lib-client/` is a manual broker-acceptance harness — run it against a live `cosmix-noded` to exercise the wire end-to-end.

## Where the broker comes from

bus ships the *client* and *protocol* — not the broker. The reference broker (`cosmix-noded`) lives in [cos](https://github.com/markc/cos). A bare-system bus consumer can still serialise / deserialise `BusMessage` values without a broker present; `NodedClient::connect` is the call that requires one.

## Related projects

- **[mix](https://github.com/markc/mix)** — ARexx-flavoured scripting language with `send` / `address` / `emit` / `on … do` as first-class keywords, built on bus.
- **[cos](https://github.com/markc/cos)** — the cosmix daemon family: broker, mail, web, DNS, knowledge indexer, display compositor. Consumer of bus; ships the broker.

## History

The CosMix Agent Bus (the Bus) — formerly AMP — was renamed before its initial public release; the pre-rename history is archived read-only at [markc/amp-final](https://github.com/markc/amp-final).

## License

MIT. See `LICENSE`.
