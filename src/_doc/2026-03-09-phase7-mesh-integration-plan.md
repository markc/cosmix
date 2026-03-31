# Phase 7: Network Mesh Integration Plan

> Absorb nodemesh's peer networking into cosmix-daemon. One daemon, one binary, AMP everywhere.

**Date:** 2026-03-09
**Status:** Planning
**Depends on:** Phases 1–6 complete, AMP v0.4 wire format deployed

---

## 1. Architecture Decision: One Daemon

**Decision:** Absorb meshd into cosmix-daemon. No separate mesh process.

**Rationale:**
- Port registry is in-process — no IPC needed to resolve remote ports
- Single binary deployment (`cosmix daemon` does everything)
- nodemesh's Laravel bridge is dead weight in the pure-Rust stack
- Config, state, and lifecycle are all in one place
- The peer manager (~350 lines) and connection handler (~90 lines) are small enough to absorb directly

**What we port from nodemesh:**
| File | Lines | Action |
|------|-------|--------|
| `peer/manager.rs` | 353 | Port as `mesh/peers.rs` |
| `peer/connection.rs` | 91 | Port as `mesh/connection.rs` |
| `server.rs` | ~50 | Inline into `mesh/mod.rs` |
| `config.rs` (PeerConfig) | ~20 | Merge into `daemon/config.rs` |

**What we drop:**
| File | Reason |
|------|--------|
| `bridge/` | Laravel integration — dead in pure-Rust stack |
| `crates/sfu/` | WebRTC SFU — future phase, not mesh routing |
| `crates/docparse/` | Doc parser — unrelated |
| `crates/amp/` | Already merged into `cosmix-port::amp` |

---

## 2. Target Architecture

```
cosmix-daemon
├── daemon/          (existing)
│   ├── mod.rs       ← spawns mesh if config.mesh.enabled
│   ├── config.rs    ← expanded MeshSection + PeerConfig
│   ├── state.rs     ← adds MeshState (peer map, inbound channel)
│   ├── registry.rs  (existing port registry)
│   └── ...
├── ipc/             (existing)
│   ├── mod.rs       ← new dispatch arms: MeshSend, MeshStatus, MeshPeers
│   └── protocol.rs  ← new IpcRequest variants
├── mesh/            ← NEW module
│   ├── mod.rs       ← start_mesh(), WebSocket accept server, message router
│   ├── peers.rs     ← PeerManager (from nodemesh peer/manager.rs)
│   ├── connection.rs← run_connection() (from nodemesh peer/connection.rs)
│   └── router.rs    ← route_inbound(): dispatch remote port calls locally
└── lua/
    └── mesh.rs      ← cosmix.mesh.send(), cosmix.mesh.peers(), cosmix.mesh.call()
```

---

## 3. Config Changes

Current `MeshSection`:
```rust
pub struct MeshSection {
    pub enabled: bool,
    pub meshd_socket: String,  // unused relic
    pub node_name: String,
}
```

New `MeshSection`:
```rust
pub struct MeshSection {
    pub enabled: bool,
    pub node_name: String,       // e.g. "cachyos"
    pub listen_port: u16,        // WebSocket listen port (default 9800)
    pub wg_ip: String,           // this node's WireGuard IP (default from interface)
    pub peers: HashMap<String, PeerConfig>,  // named peers
}

pub struct PeerConfig {
    pub wg_ip: String,           // peer's WireGuard IP
    pub port: u16,               // peer's listen port (default 9800)
}
```

Example `config.toml`:
```toml
[mesh]
enabled = true
node_name = "cachyos"
listen_port = 9800
wg_ip = "172.16.2.5"

[mesh.peers.mko]
wg_ip = "172.16.2.210"
port = 9800

[mesh.peers.mmc]
wg_ip = "172.16.2.9"
port = 9800
```

---

## 4. State Changes

Add to `DaemonState`:
```rust
pub struct DaemonState {
    // ... existing fields ...

    /// Mesh peer manager (None if mesh disabled)
    pub mesh: Option<MeshHandle>,
}

pub struct MeshHandle {
    pub peers: Arc<tokio::sync::RwLock<HashMap<String, PeerState>>>,
    pub send_tx: mpsc::Sender<AmpMessage>,  // send outbound messages
}
```

The `PeerManager` itself lives on the tokio runtime (not in shared state). Only the peer map and a send channel are exposed to the rest of the daemon.

---

## 5. Implementation Sub-Phases

### Phase 7a: Peer Connections (Session 1)

**Goal:** cosmix-daemon connects to peer nodes over WebSocket/WireGuard, maintains connections with keepalive and reconnection.

**Files to create/modify:**
1. `mesh/mod.rs` — `start_mesh(config, state) -> Result<()>`
   - Bind Axum WebSocket listener on `0.0.0.0:{listen_port}`
   - Create PeerManager, start outbound connections
   - Spawn inbound message router task
   - Return MeshHandle for state

2. `mesh/peers.rs` — Adapted from nodemesh `peer/manager.rs`
   - Remove `bridge` references
   - Use `cosmix_port::amp::AmpMessage` instead of `amp::AmpMessage`
   - Keep: PeerState, connect_to_peers(), handle_inbound(), send_to(), route(), status()
   - Keep: exponential backoff (1s → 30s), hello handshake, duplicate connection resolution

3. `mesh/connection.rs` — Direct port from nodemesh `peer/connection.rs`
   - Keep: KEEPALIVE_INTERVAL (15s), READ_TIMEOUT (45s)
   - Keep: bidirectional select loop with keepalive
   - Use `cosmix_port::amp::AmpMessage`

4. `daemon/config.rs` — Add PeerConfig, expand MeshSection
5. `daemon/state.rs` — Add `mesh: Option<MeshHandle>`
6. `daemon/mod.rs` — Spawn mesh in `run_async()` after port watcher, before shutdown wait

**New dependency:** `tokio-tungstenite` (add to Cargo.toml)
**Existing deps already available:** `axum`, `futures-util`

**Verification:** Two nodes connect, exchange hello, keepalive flows, reconnection works after kill.

### Phase 7b: Remote Port Calls (Session 2)

**Goal:** A Lua script or CLI command on node A can call a port command on node B.

**Files to create/modify:**
1. `mesh/router.rs` — Route inbound messages to local port handlers
   ```rust
   async fn route_inbound(
       peer_name: &str,
       msg: AmpMessage,
       state: &SharedState,
   ) -> Option<AmpMessage> {
       // Parse to: address
       let to = msg.to_addr()?;
       let addr = AmpAddress::parse(to)?;

       // If addressed to a local port, call it
       if addr.is_for_node(&local_node_name) {
           if let Some(port_name) = &addr.port {
               let socket = state.read().unwrap()
                   .port_registry.socket_path(port_name)?;
               let command = msg.command_name()?;
               let args = msg.json_payload().unwrap_or(serde_json::Value::Null);
               let result = cosmix_port::call_port(&socket, command, args).await;
               // Build response AMP with from/to swapped
               ...
           }
       }
   }
   ```

2. `ipc/protocol.rs` — Add IpcRequest variants:
   ```rust
   /// Send an AMP message to a remote node
   MeshSend { to: String, command: String, args: Option<serde_json::Value> },
   /// Get mesh peer status
   MeshStatus,
   /// List connected peers
   MeshPeers,
   ```

3. `ipc/mod.rs` — Dispatch new mesh commands in `dispatch()`

4. CLI — `cosmix mesh send <node> <command>`, `cosmix mesh status`, `cosmix mesh peers`

**Key design:** Remote port calls are **fire-and-wait**. The daemon on node A sends a `type: request` AMP to node B. Node B's router dispatches it to the local port, gets the response, and sends back a `type: response` AMP. Node A's daemon matches the response by `id:` header and unblocks the caller.

**Request/response correlation:**
```
Node A                              Node B
  |                                   |
  |-- type:request, id:abc123 ------->|
  |   to: cosmix-toot.cosmix.mko.amp |
  |   command: get_timeline           |
  |                                   |-- call_port("cosmix-toot", "get_timeline")
  |                                   |
  |<-- type:response, id:abc123 ------|
  |    rc: 0                          |
  |    {timeline data}                |
```

**Pending response map:** `HashMap<String, oneshot::Sender<AmpMessage>>` keyed by request ID. Stored in MeshHandle. Router checks inbound responses against this map.

**Verification:** `cosmix mesh send mko info` from cachyos calls `info` on mko's daemon and prints the response.

### Phase 7c: Port Discovery Exchange (Session 3)

**Goal:** Nodes share their port registries so you can see what's available on remote nodes.

**New AMP commands between peers:**
- `port_list` — Request: send me your port list. Response: array of port names + commands.
- `port_update` — Push: a port was added/removed on my node.

**Flow:**
1. On hello handshake, both nodes exchange `port_list` requests
2. Remote port info stored in `DaemonState` as `remote_ports: HashMap<String, Vec<RemotePortInfo>>`
3. When port watcher detects local port add/remove, push `port_update` to all connected peers
4. `cosmix list-ports` shows both local and remote ports (tagged by node)

**Files:**
1. `mesh/router.rs` — Handle `port_list` and `port_update` commands
2. `daemon/state.rs` — Add `remote_ports` field
3. `ipc/mod.rs` — Extend `ListPorts` to include remote ports

**Verification:** Start app with port on node A, `cosmix list-ports` on node B shows it.

### Phase 7d: Lua Scripting + CLI (Session 4)

**Goal:** Lua scripts can transparently call remote ports. CLI has mesh subcommands.

**Lua API:**
```lua
-- Explicit remote call
local result = cosmix.mesh.call("mko", "cosmix-toot", "get_timeline")

-- Transparent via address
local toot = cosmix.port("cosmix-toot@mko")
local timeline = toot:get_timeline()

-- List peers
local peers = cosmix.mesh.peers()
for _, p in ipairs(peers) do
    print(p.name, p.connected, p.wg_ip)
end

-- Send raw AMP
cosmix.mesh.send("mko", {
    command = "deploy",
    args = { branch = "main" },
})
```

**Files:**
1. `lua/mesh.rs` — Register `cosmix.mesh` table with call/send/peers functions
2. `lua/ports.rs` — Extend port resolution to handle `name@node` syntax

**CLI subcommands:**
```bash
cosmix mesh status           # show connected peers
cosmix mesh peers            # list peer names + IPs
cosmix mesh send <node> <cmd> [args]  # send command to remote node
cosmix mesh call <node> <port> <cmd> [args]  # call remote port command
```

**Verification:** `cosmix run scripts/remote-test.lua` calls port on remote node, gets result.

---

## 6. Critical Design Decisions

### 6.1 Transport: WebSocket over WireGuard

- **WebSocket** for persistent bidirectional connections (not raw TCP)
- **WireGuard** handles encryption, authentication, and routing
- No TLS needed on the WebSocket layer — WireGuard already encrypts
- Port 9800 (configurable) — only accessible within WireGuard mesh
- Axum provides the WebSocket server (already a dependency via potential future web API)

### 6.2 Request/Response: Correlation by ID

- Each request gets a UUID `id:` header
- Responses echo the same `id:`
- Pending map with timeout (30s default) — stale requests get cleaned up
- No caching — every call goes to the remote node

### 6.3 Error Propagation

Remote errors use the same RC codes:
- Network error (can't reach peer) → RC 20 (failure)
- Remote port not found → RC 10 (error)
- Remote command error → propagate remote RC and error string
- Timeout waiting for response → RC 20 with "timeout" error

### 6.4 Duplicate Connection Resolution

Kept from nodemesh: both nodes try to connect to each other. The one with the lower WireGuard IP keeps its outbound connection; the higher IP drops its outbound if an inbound already exists. This avoids maintaining two connections per peer.

### 6.5 No Authentication Beyond WireGuard

WireGuard peers are pre-authorized by key exchange. If you're on the WireGuard network, you're trusted. No additional auth tokens, no TLS certificates, no HMAC signing. Keep it simple.

---

## 7. New Dependencies

| Crate | Version | Purpose |
|-------|---------|---------|
| `tokio-tungstenite` | 0.26 | WebSocket client (outbound connections) |
| `futures-util` | 0.3 | Stream split for WebSocket |
| `uuid` | 1.x | Request ID generation |

`axum` is only needed if we want an Axum-based WebSocket accept server (recommended for consistency). If not already a dep, add it. Alternative: use `tokio-tungstenite`'s server accept directly.

Check current Cargo.toml — `axum` may already be there from web section placeholder.

---

## 8. IPC Protocol Additions

New `IpcRequest` variants:
```rust
/// Send a raw AMP message to a mesh peer
MeshSend { to: String, command: String, args: Option<serde_json::Value> },
/// Call a port command on a remote node (blocking, waits for response)
MeshCall { node: String, port: String, command: String, args: Option<serde_json::Value> },
/// Get mesh status (peers, connections)
MeshStatus,
/// List connected mesh peers
MeshPeers,
```

---

## 9. File Count Estimate

| Phase | New files | Modified files |
|-------|-----------|---------------|
| 7a | 3 (mesh/mod, peers, connection) | 4 (config, state, daemon/mod, Cargo.toml) |
| 7b | 1 (mesh/router) | 3 (protocol, ipc/mod, cli) |
| 7c | 0 | 3 (router, state, ipc/mod) |
| 7d | 1 (lua/mesh) | 2 (lua/mod, lua/ports) |
| **Total** | **5 new files** | **~12 modifications** |

---

## 10. Testing Strategy

### Unit tests
- AMP address parsing (already in cosmix-port::amp)
- Config parsing with mesh peers
- Request/response correlation map (insert, match, timeout cleanup)

### Integration tests (two-node)
Both nodes on WireGuard mesh (cachyos + mko):

1. **Connection lifecycle:** Start daemon on both, verify peers connect, kill one, verify reconnection
2. **Remote port call:** Start app with port on mko, call from cachyos CLI
3. **Port discovery:** Start app on mko, verify it appears in `cosmix list-ports` on cachyos
4. **Lua remote call:** Run script on cachyos that calls port on mko
5. **Error cases:** Call non-existent port, call disconnected peer, timeout test

### Smoke test script
```bash
#!/bin/bash
# Run on cachyos after both daemons are running
cosmix mesh status
cosmix mesh peers
cosmix call mko.cosmix-toot info
cosmix mesh send mko ping
```

---

## 11. Risk Mitigation

| Risk | Mitigation |
|------|-----------|
| WebSocket connection storms on flaky WireGuard | Exponential backoff (1s→30s), already proven in nodemesh |
| Deadlock between mesh and port registry locks | Mesh uses tokio::sync::RwLock (async), port registry uses std::sync::RwLock (sync) — different lock types, no nesting |
| Message floods from misbehaving peer | Channel backpressure (256 buffer), drop messages if channel full |
| State bloat from remote port caches | Only store port names + commands, no data. Evict on peer disconnect |
| Breaking existing IPC | All new IpcRequest variants are additive — existing CLI commands unchanged |

---

## 12. Success Criteria

Phase 7 is **complete** when:

- [ ] `cosmix daemon` with `[mesh] enabled = true` connects to configured peers
- [ ] `cosmix mesh status` shows connected peers with latency
- [ ] `cosmix mesh call mko cosmix-toot info` returns valid JSON from remote app
- [ ] `cosmix list-ports` shows both local and remote ports
- [ ] Lua script can call `cosmix.mesh.call("mko", "cosmix-toot", "get_timeline")` and get data
- [ ] Peers reconnect automatically after network interruption
- [ ] No regression in local IPC (all existing commands still work)

---

## 13. What Comes After (Phase 8)

Phase 8 (AI Agents) builds on mesh:
- MCP server exposes mesh as tools → Claude can call remote ports
- Natural language → Lua script generation → remote execution
- Agent templates for common cross-node workflows
- The mesh becomes the AI's hands across the network
