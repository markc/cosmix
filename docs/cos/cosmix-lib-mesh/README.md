# cosmix-lib-mesh

`cosmix-lib-mesh` provides mesh peer configuration, Bus message relay over broker WebSockets, mesh node records, and read-only WireGuard status queries. It belongs to the `cos` daemon and substrate layer in the `bus <- mix <- cos` dependency chain: it consumes Bus message primitives and supports Cos daemons, while defining neither the Bus wire format nor the Mix language.

## Package

The Cargo package is named `cosmix-lib-mesh`.

Rust code imports it as `cosmix_mesh`:

```rust
use cosmix_mesh::{MeshConfig, MeshNode, MeshPeers, PeerConfig};
```

The crate is a library. It installs no binary and provides no command-line interface.

## Modules

| Module | Purpose |
|---|---|
| `node` | Serializable mesh node identity and discovery record |
| `peer` | Peer configuration and asynchronous broker connections |
| `wg` | Read-only WireGuard interface and peer status |

`MeshNode`, `MeshConfig`, `MeshPeers`, and `PeerConfig` are re-exported from the crate root. WireGuard status types and query functions remain under `cosmix_mesh::wg`.

## Mesh node records

`MeshNode` is a serializable description of one node.

| Field | Type | Meaning |
|---|---|---|
| `id` | `Uuid` | Node identifier |
| `name` | `String` | Human-readable node name |
| `wg_pubkey` | `String` | WireGuard public key |
| `wg_endpoint` | `Option<String>` | Optional WireGuard endpoint |
| `jmap_url` | `Option<String>` | Optional JMAP URL |
| `mesh_ip` | `String` | Address on the WireGuard mesh |

The type derives `Debug`, `Clone`, `Serialize`, and `Deserialize`.

## Peer configuration

`PeerConfig` describes one remote broker.

| Field | Type | Meaning |
|---|---|---|
| `name` | `String` | Node name used for lookup and Bus addressing |
| `mesh_ip` | `String` | WireGuard mesh address |
| `noded_port` | `u16` | Broker WebSocket port; defaults to `4200` |

`PeerConfig::noded_url` produces `ws://<mesh_ip>:<noded_port>/ws`.

Peer entries reject unknown fields. In particular, the obsolete `hub_port` field is an error rather than an alias for `noded_port`.

`MeshConfig` contains:

| Field | Type | Meaning |
|---|---|---|
| `node_name` | `String` | Local node name |
| `peers` | `Vec<PeerConfig>` | Known remote peers; defaults to an empty list |
| `d2_seed` | `Option<[u8; 32]>` | Runtime-only input for admission proofs |

`d2_seed` is skipped by Serde. It is not read from `mesh.conf.mix`.

A minimal `mesh.conf.mix` file is:

```text
node_name: "alpha"
peers: [
  { name: "beta", mesh_ip: "192.0.2.2" },
  { name: "gamma", mesh_ip: "192.0.2.3", noded_port: 4300 }
]
```

Map entries inside the list use commas.

### Loading configuration

`MeshConfig::load(path)` reads an explicit `mesh.conf.mix` path. Read and parse failures return an error.

`MeshConfig::load_default(node_name)` checks the default Cosmix configuration directory for `mesh.conf.mix`.

If the default file is absent, it returns a configuration containing the supplied node name, no peers, and no admission-proof input.

If the default file exists but cannot be loaded, the function emits a warning and returns the same empty configuration.

`MeshConfig::find_peer(node_name)` returns the first peer with an exact matching name.

## Peer connections

`MeshPeers` manages reusable WebSocket connections to remote brokers.

Create it with a `MeshConfig` and an unbounded Tokio sender:

```rust
let (incoming_tx, mut incoming_rx) = mpsc::unbounded_channel();
let peers = MeshPeers::new(config, incoming_tx);
```

The sender receives inbound Bus messages that do not complete a pending request.

### Inspection methods

| Method | Result |
|---|---|
| `node_name()` | Local configured node name |
| `peer_names()` | Owned list of configured peer names |
| `peers()` | Borrowed slice of full peer records |
| `is_remote_peer(name)` | Whether the name exists in the configuration |

### Sending messages

`call(node_name, message)` sends a `BusMessage` and waits for a correlated response.

The method:

1. Rejects an unknown peer.
2. Opens the peer connection on first use.
3. Reuses an existing connection when present.
4. Preserves the message `id`, or assigns a UUID when none is present.
5. Matches a received message by `id`.
6. Returns an error if the response does not arrive within 30 seconds.

`send(node_name, message)` sends a `BusMessage` without waiting for a response. It also opens the connection on first use.

Disconnected reader tasks remove their peer from the active connection map. A later send or call can therefore establish a new connection.

### Broker registration and admission

Each new connection registers the local bridge with `noded.register`.

The connection recognises these admission messages:

| Bus command | Direction | Purpose |
|---|---|---|
| `noded.admit.challenge` | Remote broker to bridge | Supplies an admission transcript challenge |
| `noded.admit.response` | Bridge to remote broker | Returns a signed response when proof input is available |
| `noded.register` | Bridge to remote broker | Registers the bridge after admission handling |

The bridge waits up to one second for an initial challenge. A silent broker is treated as compatible with registration-first behaviour.

Malformed challenges or missing proof input produce no admission response. Registration still proceeds.

## WireGuard status

The `wg` module queries the kernel WireGuard backend. It does not create or modify interfaces.

`wg::list_interfaces()` returns the names of all WireGuard interfaces visible through that backend.

`wg::query_interface(name)` parses an interface name and returns `WgInterfaceStatus`.

| `WgInterfaceStatus` field | Type |
|---|---|
| `name` | `String` |
| `public_key` | `Option<String>` |
| `listen_port` | `Option<u16>` |
| `peers` | `Vec<WgPeerStatus>` |

Each `WgPeerStatus` reports the peer public key, optional endpoint, allowed IP ranges, seconds since the last handshake, transmitted bytes, and received bytes.

WireGuard keys are represented as standard Base64 strings.

Interface queries require root or `CAP_NET_ADMIN`. Invalid names and backend query failures return errors.

## Cargo features

The crate declares no Cargo features.

Its mesh-trust dependency is built with that dependency's default features disabled.

## Runtime dependencies

The main integration points are:

- `cosmix-lib-bus` for `BusMessage` parsing and serialization.
- `cosmix-lib-config` for `.conf.mix` loading and default paths.
- `cosmix-lib-mesh-trust` for admission transcript signing.
- Tokio, Tokio Tungstenite, and Futures Utilities for asynchronous WebSockets and channels.
- `wireguard-control` for kernel interface inspection.
- Serde and `serde_json` for configuration and admission message data.
- `tracing` for connection and configuration diagnostics.

## Error behaviour

Public fallible operations return `anyhow::Result`.

Configuration loaded from an explicit path fails closed. Default configuration loading is best-effort and falls back to an empty peer list.

Calls fail for unknown peers, connection errors, send failures, closed response channels, lost connections, and response timeouts.

WireGuard operations fail for invalid interface names, insufficient access, or kernel backend errors.
