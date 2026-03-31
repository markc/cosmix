# Daemon Consolidation — cosmix-noded

**Date:** 2026-03-31
**Status:** Design

## The Problem

Cosmix currently has 8 daemon crates. On a mesh node, most of them run simultaneously:

| Daemon | Lines | RAM | Role |
|--------|-------|-----|------|
| cosmix-hubd | 415 | ~5MB | WebSocket message broker |
| cosmix-configd | 278 | ~3MB | Config watcher, settings via AMP |
| cosmix-mond | 198 | ~3MB | System metrics collector |
| cosmix-logd | 77 | ~2MB | Log aggregation |
| cosmix-mcp | 201 | ~3MB | MCP bridge for Claude Code |
| cosmix-maild | 6,550 | ~15MB | JMAP + SMTP mail server |
| cosmix-indexd | 666 | ~7MB idle, ~300MB active | Semantic indexing (candle) |
| cosmix-webd | 590 | ~5MB | WASM app server + HTTP |

That's 8 separate processes, 8 systemd units, 8 hub connections, 8 binaries to build and deploy. For a lightweight mesh node targeting ~27MB total RAM, this is a lot of overhead from process isolation that provides no real benefit (single operator, single trust domain).

## The Proposal: cosmix-noded

Consolidate the lightweight daemons into a single **cosmix-noded** binary that runs as one process with multiple async tasks:

### Absorb into cosmix-noded

| Module | Why |
|--------|-----|
| **hub** | The message broker is the foundation — everything else connects to it. Making it in-process eliminates 7 WebSocket connections. |
| **configd** | 278 lines. Watches config files, serves settings. Trivial module. |
| **mond** | 198 lines. Collects system metrics on a timer. Trivial module. |
| **logd** | 77 lines. Barely a daemon at all. |
| **mixd** | NEW — the remote Mix execution handler from the companion doc. |

### Keep separate

| Daemon | Why |
|--------|-----|
| **cosmix-maild** | 6,550 lines, complex domain (JMAP/SMTP/spam), own database, own TLS listener. Genuinely independent. |
| **cosmix-indexd** | Heavy deps (candle for ML inference), spiky memory (7MB→300MB), optional per-node. Should be able to crash without taking down the node. |
| **cosmix-webd** | Own HTTP listener (axum), serves WASM apps, potentially public-facing. Different security boundary. |
| **cosmix-mcp** | Launched by Claude Code on demand, not a long-running service. Different lifecycle. |

### The result

**Before:** 8 processes on a mesh node
**After:** 3 processes (noded + maild + webd) + 1 optional (indexd) + 1 on-demand (mcp)

## Architecture

```
cosmix-noded
├── hub module      — WebSocket broker (was cosmix-hubd)
├── config module   — Config watcher + AMP settings (was cosmix-configd)
├── monitor module  — System metrics (was cosmix-mond)
├── log module      — Log aggregation (was cosmix-logd)
├── mix module      — Remote Mix execution (NEW: mix.eval handler)
└── main            — CLI, signal handling, module lifecycle
```

Each module is a tokio task spawned at startup. They communicate through the in-process hub rather than WebSocket connections. The hub module still accepts external WebSocket connections from GUI apps and other daemons (maild, webd).

### CLI

```bash
cosmix-noded serve                    # start all modules
cosmix-noded serve --no-monitor       # skip mond
cosmix-noded serve --no-mix           # skip mixd
cosmix-noded status                   # query running modules
```

Or configure via TOML:

```toml
# ~/.config/cosmix/noded.toml
[modules]
hub = true          # always true — everything depends on it
config = true
monitor = true
log = true
mix = true

[hub]
listen = "127.0.0.1:4200"
# ... existing hubd config

[monitor]
interval_secs = 30

[mix]
enable_sh = false   # disable shell commands in remote mix.eval
timeout_secs = 30
```

### Module trait

```rust
#[async_trait]
trait NodeModule: Send + Sync {
    fn name(&self) -> &str;
    async fn start(&self, hub: Arc<HubClient>) -> anyhow::Result<()>;
    async fn handle_command(&self, cmd: &IncomingCommand) -> Option<Result<String, String>>;
}
```

Each module registers its command prefixes with the hub. When the hub receives a command like `config.get`, it routes to the config module directly (no WebSocket serialization).

### In-process hub optimization

The biggest win: modules inside cosmix-noded don't need WebSocket connections. The hub can route messages between in-process modules via tokio channels:

```rust
// External clients: WebSocket → AMP parse → route → AMP serialize → WebSocket
// Internal modules: channel send → route → channel recv
```

This eliminates serialization overhead for intra-noded communication. External clients (GUI apps, maild, webd) still connect via WebSocket as before.

## Migration Path

### Phase 1: Create cosmix-noded with hub only

- New crate `cosmix-noded`
- Move hubd code in as the hub module
- Verify all external clients still connect
- Deploy alongside existing daemons (they connect as external clients)

### Phase 2: Absorb configd + mond + logd

- Move each into noded as a module
- In-process hub routing for their commands
- Remove the standalone crates
- Update systemd units

### Phase 3: Add mixd module

- Implement `mix.eval` command handler
- Wire to cosmix-lib-script's `execute_mix()`
- Test with local and remote scripts

### Phase 4: Remove standalone crate shells

- Delete cosmix-hubd, cosmix-configd, cosmix-mond, cosmix-logd crates
- Update workspace Cargo.toml
- Update documentation

## What About the GUI Apps?

GUI apps (cosmix-edit, cosmix-view, cosmix-shell, etc.) remain as separate binaries. They're user-facing processes with their own windows — consolidating them would mean one crash takes down all apps. They connect to cosmix-noded's hub module via WebSocket, same as they connected to cosmix-hubd before.

## What About cosmix-claude?

cosmix-claude is a new daemon for Claude Code agent integration. It could go either way:

- **Absorb:** It's lightweight, always-on, benefits from in-process hub access
- **Keep separate:** It has a unique lifecycle (may be restarted independently, may need different resource limits)

Decision can wait until cosmix-claude's scope is clearer.

## RAM Impact

Rough estimate for a mesh node:

| Before | After |
|--------|-------|
| hubd: ~5MB | cosmix-noded: ~8MB (one process, shared runtime) |
| configd: ~3MB | |
| mond: ~3MB | |
| logd: ~2MB | |
| maild: ~15MB | maild: ~15MB (unchanged) |
| webd: ~5MB | webd: ~5MB (unchanged) |
| **Total: ~33MB** | **Total: ~28MB** |

The savings come from shared tokio runtime, shared allocator, eliminated WebSocket connections, and reduced per-process overhead. Not dramatic, but cleaner.

## The Real Win: Operational Simplicity

The RAM savings are modest. The real benefits are:

1. **One binary to build and deploy** for the core node functionality
2. **One systemd unit** instead of four
3. **One log stream** for all core services
4. **In-process communication** between hub, config, monitor, and mix modules
5. **Atomic startup/shutdown** — the node either works or it doesn't, no partial states
6. **Simpler debugging** — one process to attach to, one set of logs to read

## Name

**cosmix-noded** — "the node daemon." Every machine on the mesh runs one. It IS the node's cosmix presence. GUI apps and heavyweight services (mail, indexing, web) connect to it.
