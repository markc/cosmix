# 2026-03-26 — Appmesh Phase 3: mesh bridge for cross-node communication

## What was done

### cosmix-mesh peer management (peer.rs)
- `MeshConfig` — TOML-based peer config (node_name + peer list with mesh_ip/hub_port)
- `PeerConfig` — per-peer config with WireGuard mesh IP and hub WebSocket port
- `MeshPeers` — manages WebSocket connections to remote hubs:
  - `call(node, msg)` — request/response with 30s timeout, auto-connects on first use
  - `send(node, msg)` — fire-and-forget relay
  - `ensure_connected()` — lazy WebSocket connection with sender/reader task spawn
  - Registers as `bridge-{node_name}` on remote hubs
  - Incoming messages from remote hubs forwarded via channel to local hub
  - Connection pool: reuses existing connections, cleans up on disconnect

### cosmix-mesh WireGuard query (wg.rs)
- `query_interface(name)` — read existing WG interface via wireguard-control crate
  - Returns peers with public key, endpoint, allowed IPs, last handshake, transfer stats
- `list_interfaces()` — enumerate all WG interfaces on the system
- Read-only — no tunnel creation, just status query

### cosmix-hub mesh bridge
- New CLI flags: `--node` (this node's name), `--mesh-config` (config path)
- Routing upgraded with three-tier address resolution:
  1. `to: "hub"` → hub internal command
  2. `to: "files.mko.amp"` → parse AmpAddress, check if remote → bridge via MeshPeers
  3. `to: "files"` → plain service name → local registry lookup
- For remote nodes: spawns async task, calls `mesh.call()`, relays response back
- For local AMP addresses (e.g. `files.cachyos.amp` on cachyos): extracts service name, routes locally
- New `hub.peers` command — returns node name and configured peer list
- `route_local()` extracted as shared function for local service delivery
- Listens on `0.0.0.0` instead of `127.0.0.1` (remote hubs connect over WireGuard)
- Incoming mesh messages delivered to local services via background task

### Example mesh config
- `crates/cosmix-hub/mesh.toml.example` — sample config for cachyos with mko and pve5 peers

## Architecture

```
App (cosmix-view)                    Remote node (mko)
    │                                    │
    ├─ call("files.mko.amp", ...)        │
    │                                    │
    ▼                                    │
cosmix-hub (cachyos)                     │
    │                                    │
    ├─ parse AmpAddress → node="mko"     │
    ├─ MeshPeers::call("mko", msg)       │
    │   ├─ ensure_connected()            │
    │   ├─ ws://172.16.2.210:4200/ws ────┤
    │   └─ register as "bridge-cachyos"  │
    │                                    ▼
    │                            cosmix-hub (mko)
    │                                    │
    │                                    ├─ route to local "files" service
    │                                    ├─ response back over WebSocket
    │                                    │
    ◄────────────────────────────────────┘
    │
    ├─ relay response to cosmix-view
```

## New/modified files
- `crates/cosmix-mesh/src/peer.rs` — mesh peer management + WebSocket connections
- `crates/cosmix-mesh/src/wg.rs` — WireGuard interface query
- `crates/cosmix-mesh/src/lib.rs` — re-exports
- `crates/cosmix-mesh/Cargo.toml` — added toml, dirs-next, base64 deps
- `crates/cosmix-hub/src/main.rs` — rewritten with mesh bridge, AMP address routing
- `crates/cosmix-hub/Cargo.toml` — added cosmix-mesh dependency
- `crates/cosmix-hub/mesh.toml.example` — sample mesh config

## Decisions
- **Config-based peers, not DB**: simple TOML file listing known peers. No need for
  PostgreSQL peer discovery at this stage — the mesh is small (3-5 nodes) and
  manually configured via WireGuard.
- **Lazy connections**: hub connects to remote peers on first message, not at startup.
  Avoids blocking startup if peers are unreachable.
- **Bridge registration**: remote hub sees us as `bridge-cachyos` service. This
  distinguishes bridge connections from regular app connections.
- **0.0.0.0 binding**: hub now listens on all interfaces so remote peers can
  connect over WireGuard. Previous 127.0.0.1 binding was local-only.
