# innernet Absorption Plan — WireGuard Foundation for Cosmix Mesh

> Absorb the best of innernet's Rust codebase into cosmix-daemon to replace the shell-based mesh-hub.sh with native WireGuard control, direct peering, NAT traversal, and full Lua scriptability via cosmix-port.

## Why

The current mesh works but has fundamental limitations:

1. **Shell scripts managing WireGuard** — `mesh-hub.sh` uses `wg set` and `sed` to manage peers. Fragile (base64 in sed), no atomicity, no error recovery.
2. **Hub-and-spoke only** — all traffic routes through gw's `wg1`. No direct peering between nodes, even on the same LAN.
3. **No programmatic WireGuard control** — can't add/remove peers, rotate keys, or query interface state from Lua scripts or the daemon.
4. **Registration is SSH-based** — new nodes SSH to gw to register. Works, but requires key distribution and doesn't scale beyond a single hub.

innernet solves all of these in pure Rust (MIT). Rather than depending on it as a binary, we absorb the components we need into cosmix-daemon.

## What to Absorb

### Take: `wireguard-control` (~520 LOC)

innernet's crown jewel. Native Linux netlink interface to WireGuard — no shelling out to `wg`.

**Key types:**
- `DeviceUpdate` — atomic interface configuration (set private key, listen port, replace/add peers)
- `PeerConfigBuilder` — fluent API for peer configuration (pubkey, endpoint, allowed IPs, keepalive)
- `Device` / `Peer` — read current interface and peer state
- `Key` / `KeyPair` — generate, parse, format WireGuard keys

**What this replaces:** All `wg set`, `wg show`, `wg genkey`, `wg pubkey` shell calls. The `wireguard-control` crate talks directly to the kernel via netlink.

**Integration:** Add as a cargo dependency. It's a standalone crate with minimal deps (libc, nix). Already published on crates.io as `wireguard-control`.

### Take: NAT Traversal + Endpoint Discovery (from `client-core`, ~200 LOC)

innernet's `fetch()` function handles:
- Querying the coordination server for peer endpoints
- Updating local WireGuard peer endpoints when they change
- Detecting NAT'd peers and updating their external endpoints

**What this replaces:** Nothing — we don't have this yet. This enables direct peering between nodes behind NAT.

**Integration:** Reimplement the logic (it's ~200 lines) rather than depending on the full `client-core` crate, since we don't need its SQLite state, hosts file management, or systemd integration.

### Take: Peer State Tracking (from `client-core`, ~150 LOC)

innernet tracks:
- Last handshake time per peer
- Peer reachability (based on handshake recency)
- Stale peer detection and cleanup

**What this replaces:** Our `PeerManager` already does reconnection with exponential backoff at the WebSocket layer. This adds WireGuard-level peer health below that.

### Take: Key Generation + Management (from `shared`, ~100 LOC)

innernet wraps key generation with proper entropy and formatting. Small but important for correctness.

**What this replaces:** `wg genkey | wg pubkey` shell pipeline in `cosmixos-setup.sh`. Key generation moves into the daemon.

### Take: Interface Lifecycle (from `client-core`, ~150 LOC)

- Create WireGuard interface
- Set address, private key, listen port
- Bring interface up/down
- Teardown on shutdown

**What this replaces:** `wg-quick up wg0` / `wg-quick down wg0`. The daemon manages its own interface.

## What to Skip

| innernet Component | Why Skip |
|---|---|
| **CIDR-based ACLs** | Cosmix mesh is flat (172.16.2.0/24). No subnet hierarchy needed. |
| **Association groups** | Cosmix uses AMP addressing (`port.app.node.amp`) not group-based access control. |
| **Invite/redemption system** | Cosmix has auto-provisioning via `cosmixos-setup`. Invites add unnecessary complexity. |
| **Hosts file management** | Cosmix nodes use mesh IPs directly. DNS resolution handled separately (future cosmix-dns). |
| **SQLite state database** | Cosmix already has PostgreSQL (cosmix-web) and in-memory state (DaemonState). No SQLite. |
| **Multi-backend support** | We only target Linux (netlink). No need for userspace or cross-platform backends. |
| **Standalone CLI** | The daemon IS the CLI. No separate `innernet` binary. |
| **systemd integration** | Alpine uses OpenRC. Daemon manages its own lifecycle. |

**Estimated absorption:** ~1,100 LOC of innernet's ~4,000 LOC core. The rest is infrastructure we already have or don't need.

## Architecture

### New Module: `cosmix-daemon/src/wg/`

```
src/wg/
├── mod.rs          — public API, WgManager struct
├── interface.rs    — create/destroy/configure WireGuard interface
├── peers.rs        — add/remove/update peers, endpoint discovery
├── keys.rs         — key generation, parsing, formatting
└── registry.rs     — peer registry (replaces mesh-hub.sh JSON)
```

### WgManager

The central struct that owns the WireGuard interface:

```rust
pub struct WgManager {
    interface: String,           // "wg0"
    private_key: Key,
    listen_port: u16,
    mesh_ip: IpAddr,
    peers: HashMap<String, MeshPeer>,  // hostname → peer state
    registry: PeerRegistry,      // replaces mesh-registry.json
}

pub struct MeshPeer {
    pub name: String,
    pub public_key: Key,
    pub mesh_ip: IpAddr,
    pub endpoint: Option<SocketAddr>,  // LAN or public IP
    pub last_handshake: Option<Instant>,
    pub registered: DateTime<Utc>,
}
```

### Integration with DaemonState

```rust
// In daemon/state.rs
pub struct DaemonState {
    // ... existing fields ...
    pub wg: Option<WgManager>,  // None if WireGuard not configured
}
```

### Startup Sequence

WireGuard initializes BEFORE the mesh WebSocket layer:

```
1. Load config (MeshSection)
2. WgManager::new() — create/configure wg0 interface
3. Load peer registry
4. Configure all known peers
5. Start mesh listener (axum on wg_ip:9800)  ← existing code
6. Connect to known peers via WebSocket       ← existing code
```

This means WebSocket mesh traffic flows over the WireGuard interface that the daemon itself controls. No external `wg-quick` dependency.

### IPC Commands

New commands added to `ipc/protocol.rs`:

```rust
pub enum IpcRequest {
    // ... existing variants ...

    // WireGuard management
    WgStatus,                           // → interface + peer summary
    WgPeers,                            // → list all peers with state
    WgAddPeer { name: String, pubkey: String, endpoint: Option<String> },
    WgRemovePeer { name: String },
    WgSetEndpoint { name: String, endpoint: String },
    WgRotateKeys,                       // generate new keypair, re-register

    // Mesh registration (replaces mesh-hub.sh)
    MeshRegister { pubkey: String, name: String },  // → assigns IP
    MeshList,                                        // → all registered nodes
    MeshRemove { name: String },
}
```

### Lua Port Bindings

The `wg` port exposes WireGuard control to Lua scripts:

```lua
local wg = cosmix.port("wg")

-- Query state
local status = wg:status()
print(status.interface)      -- "wg0"
print(status.public_key)     -- "abc123..."
print(status.listen_port)    -- 51820
print(#status.peers)         -- 5

-- Peer management
wg:add_peer({
    name = "newnode",
    pubkey = "xyz789...",
    endpoint = "192.168.2.50:51820",
    allowed_ips = "172.16.2.25/32"
})

wg:remove_peer("newnode")

-- Direct peering (same-LAN optimization)
local peers = wg:peers()
for _, peer in ipairs(peers) do
    if peer.lan_ip and peer.lan_subnet == wg:lan_subnet() then
        wg:set_endpoint(peer.name, peer.lan_ip .. ":51820")
        print("Direct peer: " .. peer.name)
    end
end

-- Key rotation
wg:rotate_keys()  -- generates new keypair, re-registers with hub
```

### Coordination: Hub Node vs Regular Node

Every cosmix-daemon can be a hub or a regular node (or both). The role is config-driven:

```toml
# Regular node
[mesh]
wg_ip = "172.16.2.20"
listen_port = 9800
hub = "172.16.2.1:9800"   # register with this hub

# Hub node (on gw)
[mesh]
wg_ip = "172.16.2.1"
listen_port = 9800
hub_mode = true            # accept registrations
ip_range = "172.16.2.20-200"
reserved_ips = [1, 4, 5, 9, 210]
```

**Hub registration replaces SSH:** Instead of `ssh gw "register $PUBKEY $NAME"`, nodes send an AMP message:

```
---
amp: 1
type: request
from: wg.cosmix.newnode.amp
to: wg.cosmix.gw.amp
command: register
---
pubkey: abc123...
name: newnode
```

The hub responds with the assigned IP. This works over the initial WireGuard connection (new nodes connect to hub's endpoint first, get assigned an IP, then the hub adds them as a peer).

### Bootstrap Problem

A new node needs WireGuard configured to reach the hub, but the hub assigns the IP via WireGuard. innernet solves this with an invite file. We solve it simpler:

1. New node generates keypair
2. New node creates minimal `wg0.conf` with hub as only peer, using a bootstrap IP (e.g., `172.16.2.254/32` — reserved for bootstrapping)
3. Node connects to hub's WireGuard endpoint
4. Hub receives registration request, assigns real IP
5. Hub updates node's allowed-ips from `.254` to the assigned IP
6. Node reconfigures its own interface with the assigned IP
7. Node is now a full mesh member

This preserves the zero-touch provisioning model. `cosmixos-setup.sh` becomes simpler — it just starts the daemon with `hub = "..."` in config, and the daemon handles everything.

## Phases

### Phase 1: wireguard-control Dependency (Day 1)

**Goal:** Replace all `wg` shell commands with native Rust calls.

1. Add `wireguard-control` to `Cargo.toml`
2. Create `src/wg/mod.rs`, `interface.rs`, `keys.rs`
3. Implement `WgManager::new()` — create interface, set key, set address
4. Implement `WgManager::add_peer()`, `remove_peer()`, `list_peers()`
5. Add `WgStatus` and `WgPeers` IPC commands
6. Test: daemon creates wg0, adds a peer, queries state — all without shelling out

**Verification:** `cosmix wg status` shows interface state. `cosmix wg peers` lists peers. No `wg` binary needed.

### Phase 2: Peer Registry in Daemon (Day 1-2)

**Goal:** Replace `mesh-hub.sh` + `mesh-registry.json` with daemon-managed registry.

1. Create `src/wg/registry.rs` — `PeerRegistry` struct
2. Implement IP allocation (port of `next_ip()` logic from mesh-hub.sh)
3. Add `MeshRegister`, `MeshList`, `MeshRemove` IPC commands
4. Hub mode: accept registration requests via AMP, assign IPs, add WG peers
5. Persist registry to a JSON file (same format, for continuity) or PostgreSQL
6. Migrate existing `mesh-registry.json` data on first run

**Verification:** `cosmix mesh register <pubkey> <name>` assigns an IP. `cosmix mesh list` shows all nodes. Existing mesh-hub.sh data is preserved.

### Phase 3: Auto-Provisioning via Daemon (Day 2)

**Goal:** New nodes self-provision by talking to the hub daemon instead of SSH.

1. Implement bootstrap flow (connect with `.254`, get real IP, reconfigure)
2. Update `cosmixos-setup.sh` to start daemon with hub config instead of SSH registration
3. Hub daemon handles registration, IP assignment, and peer configuration
4. Test: launch fresh CT, it auto-provisions via daemon-to-daemon communication

**Verification:** New CT boots, joins mesh, gets IP, all without SSH to gw. `mesh-hub.sh` is no longer needed.

### Phase 4: Direct Peering (Day 2-3)

**Goal:** Nodes on the same LAN establish direct WireGuard connections, bypassing the hub.

1. Implement endpoint discovery — nodes report their LAN IP to the hub
2. Hub distributes peer endpoint information to all nodes
3. Nodes detect same-subnet peers and add direct WireGuard peers
4. Implement peer health monitoring (handshake age)
5. Fallback: if direct peer becomes unreachable, route through hub

**Verification:** Two nodes on lan2 (192.168.2.x) ping each other via direct WG peer. Latency drops from ~4ms (hub relay) to <1ms (direct). Hub relay still works for cross-subnet traffic.

### Phase 5: Lua Port + Scriptability (Day 3)

**Goal:** Full WireGuard control from Lua scripts.

1. Register `wg` port with cosmix-port
2. Expose all WgManager methods as Lua-callable commands
3. Expose mesh registration commands
4. Write example scripts: mesh status dashboard, peer health monitor, key rotation

**Verification:** `cosmix run scripts/mesh-status.lua` prints formatted peer table with latencies. Lua script can add/remove peers.

### Phase 6: Key Rotation + Security (Day 3-4)

**Goal:** Automated key rotation without service disruption.

1. Implement `WgManager::rotate_keys()` — generate new keypair, re-register with hub
2. Hub updates the peer's pubkey, notifies all other nodes
3. Other nodes update their peer config for the rotated node
4. Implement rotation schedule (configurable interval, default 30 days)
5. Audit: log all key changes

**Verification:** Node rotates keys, all peers update within 5 seconds, no traffic interruption.

## musl/Alpine Considerations

| Component | Status on musl |
|---|---|
| `wireguard-control` | Works — pure Rust + libc/nix netlink. No C dependencies. |
| Key generation | Works — uses `rand` crate, no OpenSSL dependency. |
| Netlink sockets | Works — kernel interface, no userspace library needed. |
| `rusqlite` (if used) | Needs `features = ["bundled"]` to statically compile SQLite. But we plan to skip SQLite entirely. |
| `nix` crate | Works on musl — well-tested, used by many musl projects. |

**No blockers.** The absorbed components are pure Rust with only libc/kernel dependencies. Perfect for static musl builds.

## Migration Path

### Week 1: Phases 1-3
- Daemon manages WireGuard natively
- Hub registration moves from SSH to AMP
- `mesh-hub.sh` becomes optional (kept as fallback)
- `cosmixos-setup.sh` simplified to just start daemon

### Week 2: Phases 4-6
- Direct peering eliminates hub relay for same-LAN traffic
- Lua scripts can manage the entire mesh
- Key rotation automated

### After: Deprecation
- `mesh-hub.sh` removed (daemon is the hub)
- `cosmixos-setup.sh` reduced to: install daemon config, start daemon
- SSH registration key (`mesh-register`) no longer needed
- `wg-quick` no longer needed (daemon manages interface directly)

## What This Enables

1. **Self-healing mesh** — daemon detects peer failures, re-routes through hub, re-establishes direct connections when peers recover
2. **Zero-touch scaling** — boot a new CT, it finds the hub, registers, gets peers, establishes direct connections. No human intervention.
3. **Scriptable infrastructure** — `cosmix run rotate-all-keys.lua` rotates every node's keys in sequence. `cosmix run mesh-report.lua` generates a health dashboard.
4. **ARexx for networking** — the mesh itself becomes a cosmix port. Scripts orchestrate network topology the same way they orchestrate desktop apps.
5. **No external dependencies** — no Tailscale account, no Docker containers, no Go binaries. Pure Rust, single binary, runs everywhere.

## Estimated Complexity

| Module | New LOC | Absorbed from innernet |
|---|---|---|
| `wg/interface.rs` | ~200 | wireguard-control API wrapping |
| `wg/peers.rs` | ~300 | peer management, endpoint updates |
| `wg/keys.rs` | ~80 | key generation, formatting |
| `wg/registry.rs` | ~250 | IP allocation (from mesh-hub.sh logic) |
| `wg/mod.rs` | ~150 | WgManager, startup/shutdown |
| IPC commands | ~100 | protocol.rs additions |
| Lua bindings | ~200 | wg port registration |
| **Total** | **~1,280** | |

The `wireguard-control` crate itself is ~520 LOC and comes as a cargo dependency — we don't need to absorb its code, just use it.
