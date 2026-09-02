# bus — CosMix Agent Bus

**bus is the pure-Rust protocol family for messaging between named,
agent-operable services on a WireGuard-secured mesh.** It defines the wire
message, broker client, property read types, build provenance, and common
logging surface. The broker and the operating substrate live elsewhere.

This page is the orientation: what Bus is, which crates bus contains, where the
protocol boundary sits, and how bus fits with mix and cos.

## What Bus is

Bus follows the AmigaOS ARexx convention: every service exposes a named port.
A peer can send that port a command, call it and wait for a reply, or subscribe
to its event topics. Local names such as `maild` resolve through the node's
service registry; `maild.delta.bus` addresses the service on a named mesh node.

Every node runs a `cosmix-noded` broker, supplied by the
[cos](https://github.com/markc/cosmix) repo. Citizens connect over WebSocket,
register a service name, and exchange Bus messages through that broker. Remote
node traffic crosses the WireGuard mesh. Trust is granted per WireGuard subnet:
peers admitted to that subnet are inside the corresponding Bus trust boundary.

The design separates the message contract from the machinery that operates it.
A bus consumer can construct, parse, and validate a `BusMessage` without a
broker or mesh being present.

## The crate family

| Crate | What it is |
|---|---|
| [`cosmix-lib-bus`](wire-format.md) | Bus header/body wire format, addresses, return codes, discovery records, and native Unix-socket port helpers. |
| [`cosmix-lib-client`](client.md) | `NodedClient`, the native and browser WebSocket client; native builds also expose `SupervisedClient`. |
| [`cosmix-lib-props-core`](props-core.md) | SPEC 07 property read types and, behind `bus`, read dispatch and event-message builders. |
| [`cosmix-lib-buildinfo`](buildinfo.md) | Dependency-free compile-time package, git, dirty-tree, and build-time provenance. |
| [`cosmix-lib-log`](log.md) | Common tracing, sink, filter-reload, and metrics/statistics surface. |

The workspace builds standalone. Its crates use ordinary third-party Rust
dependencies and sibling dependencies within bus, but no crate depends on mix
or cos.

## The protocol boundary

bus owns data shapes and protocol-facing client primitives. It deliberately
does not own:

- persistent application/property backends or audit state;
- TLS, ACME, WireGuard setup, mesh inventory, or broker implementation;
- service configuration files or broker URL discovery; or
- general daemon lifecycle and system-service integration.

Those are substrate concerns in cos. In particular,
`cosmix-lib-props-store` supplies the SPEC 12 mutation and storage side paired
with bus's property core, while `cosmix-noded` supplies the broker. The
logging crate's rolling-file and stats JSONL sinks are operational output, not
a service-state backend.

## How it fits — bus ← mix ← cos

The public repositories have a one-way dependency order:

- **bus** — protocol libraries at the bottom; depends on neither sibling.
- **[mix](https://github.com/markc/cosmix)** — the ARexx-flavoured language and shell; depends on bus for `send`, `address`, `emit`, subscriptions, and serving.
- **[cos](https://github.com/markc/cosmix)** — the daemon and substrate family; depends on bus and mix, and ships `cosmix-noded`.

Neither mix nor cos is required to compile the bus workspace.

## Building

```sh
git clone https://github.com/markc/cosmix ~/.bus
cd ~/.bus/src
cargo build --workspace
cargo test --workspace
```

## See also

- [wire format](wire-format.md) — messages, framing, addresses, and native ports
- [client](client.md) — connect, call, send, subscribe, and reconnect
- [property core](props-core.md) — the SPEC 07 read surface
- [build information](buildinfo.md) — compile-time provenance
- [logging](log.md) — tracing sinks, reload, and statistics
- [the manual index](README.md) — every page in this manual
