# Cosmix — ARexx for COSMIC

Umbrella project unifying desktop automation, mesh networking, web services, and COSMIC desktop integration under a single Rust+Lua architecture.

## Vision

Build the modern ARexx: a universal scripting layer where Lua scripts orchestrate Rust-powered desktop apps, mesh services, and web APIs. COSMIC is the platform, Lua is the lingua franca, Rust is the engine.

## Name

**Cosmix** — COSMIC + remix/mix. Your AI-powered desktop companion that scripts, automates, and orchestrates across the COSMIC desktop and mesh network.

Previously "Coworker" (renamed 2026-03-07).

## Layered Architecture

The system is built in three layers, each independently useful, each building on the last:

```
Layer 3: libcosmic ports         ← deep per-app commands (the goal)
Layer 2: cosmic-comp scripting   ← compositor-level window/workspace/input
Layer 1: external daemon         ← AT-SPI2, D-Bus, EIS (works today)
```

### Layer 1: External Daemon (No App Modifications)

A `cosmix` daemon controls apps from the outside:
- **Window management** via Wayland protocols (ext-foreign-toplevel-list)
- **Input injection** via EIS/libei
- **UI introspection** via AT-SPI2 (accessibility tree)
- **Notifications** via freedesktop D-Bus
- **Whatever D-Bus** COSMIC apps happen to expose

Zero app modifications needed. Works with any app, not just COSMIC ones.

### Layer 2: Compositor Scripting (cosmic-comp)

Embed mlua directly in cosmic-comp (already being patched for animations):
- Lua scripts control workspaces, tiling, window rules
- Compositor-level input interception
- Deep window management without per-app cooperation

### Layer 3: libcosmic Integration (The Endgame)

A `cosmix-port` crate integrated into libcosmic behind a feature flag:
- `cosmic::app::run()` automatically starts a lightweight IPC listener
- Apps register commands with minimal code (~5-20 lines per app)
- Cosmix daemon discovers running apps and routes Lua calls to them
- The ARexx model: every app has a port, scripts orchestrate them

```rust
// Per-app code — this is ALL that's needed
impl App {
    fn cosmix_commands() -> Vec<CosmixCommand> {
        vec![
            command!("open", |path: String| self.open_file(path)),
            command!("search", |query: String| self.search(query)),
            command!("selection", || self.get_selection()),
        ]
    }
}
```

## Subprojects

| Subproject | Location | Stack | Role |
|------------|----------|-------|------|
| **appmesh** | `~/.gh/appmesh/` | Rust+Lua (refactoring from PHP/QML) | Desktop automation — ARexx port system, D-Bus, input injection, COSMIC app control |
| **nodemesh** | `~/.gh/nodemesh/` | Rust | Mesh control plane — meshd daemon, AMP protocol, WebRTC SFU |
| **markweb** | `~/.gh/markweb/` | Laravel 12 + React + Reverb | Web interface — AI agent, mail, chat, DCS panels. Self-contained web world. |
| **cosmic** | `~/.gh/cosmic/` | Rust | COSMIC desktop patches and extensions (cosmic-comp, future cosmic apps) |

## Two Worlds

```
+-------------------------------------+     +--------------------------+
|        COSMIC DESKTOP WORLD         |     |       WEB WORLD          |
|                                     |     |                          |
|  Lua scripts ("ARexx")              |     |  Laravel + React + Reverb|
|    v calls into                     |     |                          |
|  Rust runtime (mlua)                |<--->|  markweb                 |
|    +-- cosmix daemon (Layer 1)      | WS  |  REST/WS APIs            |
|    +-- cosmic-comp (Layer 2)        |     |  DCS panels              |
|    +-- cosmix-port in apps (Layer 3)|     |                          |
|    +-- meshd (mesh control plane)   |     |                          |
+-------------------------------------+     +--------------------------+
```

## Language Policy

| Domain | Language | Rationale |
|--------|----------|-----------|
| Performance, daemons, desktop | **Rust** | Native, safe, COSMIC-native |
| Scripting, automation, glue | **Lua** | Embeddable via mlua, hot-reloadable, minimal |
| Web frontend + backend | **PHP + React** | Laravel ecosystem, self-contained |
| Interactive shell one-liners | **bash** | Stays for interactive use, not programming |

**NO Python. Ever.**

## GUI Policy

All cosmix GUI apps **must use libcosmic** (`pop-os/libcosmic`), not raw `iced`. libcosmic provides the COSMIC header bar, theming, window decorations, and navigation — apps should not reimplement these. Use `cosmic::Application` trait, `cosmic::app::run()`, and `cosmic::widget::*`.

## Key Crates

| Crate | Purpose |
|-------|---------|
| `mlua` | Embed LuaJIT/Lua 5.4 in Rust, expose APIs to Lua scripts |
| `axum` | HTTP/WebSocket server (meshd, future APIs) |
| `zbus` | D-Bus client (COSMIC app automation) |
| `atspi` | AT-SPI2 accessibility tree introspection |
| `tokio` | Async runtime |
| `tokio-tungstenite` | WebSocket (mesh peer connections) |
| `str0m` | WebRTC SFU (media plane) |
| `iced` | COSMIC UI toolkit (future native apps) |
| `serde` / `serde_json` | Serialization |
| `clap` | CLI argument parsing |

## Transport

| Scope | Transport | Why |
|-------|-----------|-----|
| Per-app IPC (Layer 3) | **Unix socket** (`/run/user/$UID/cosmix/`) | Local, fast, auto-discoverable |
| Cross-node mesh | **WebSocket** (over WireGuard) | meshd handles this |
| Consuming existing interfaces | **D-Bus** | For what COSMIC already exposes |

## AMP Protocol

All mesh communication uses AMP (markdown frontmatter wire format):

```
---
amp: 1
type: request
from: clipboard.appmesh.cachyos.amp
to: deploy.markweb.mko.amp
command: deploy
id: <uuid>
---
Optional markdown body.
```

- **Three-reader principle:** machines route on headers, humans read markdown, AI reasons on full text
- **Transport:** WebSocket (inter-node over WireGuard), Unix socket (local bridge)
- **Address format:** `[port.]app.node.amp`

## The ARexx Model

Like Amiga ARexx, every app exposes a **port** with named commands:

```lua
-- Lua script example (the "ARexx" experience)
local clip = cosmix.port("clipboard")
local win = cosmix.port("windows")
local mail = cosmix.port("mail")

-- Get text from clipboard, search mail, notify
local text = clip:get()
local results = mail:search({ query = text, limit = 5 })
cosmix.notify("Found " .. #results .. " matching emails")

-- Activate a specific window
win:activate({ title = "cosmic-files" })
```

Ports are Rust implementations exposed to Lua via mlua. Scripts are hot-reloadable, no compilation needed.

## The cosmix-port Crate

The shared library for Layer 3 integration:

```
cosmix-port (crate)
+-- IPC listener (Unix socket, auto-discovered)
+-- Command registry (apps register functions)
+-- AMP message handling (wire format)
+-- Optional: meshd client (cross-node scripting)
```

Does NOT contain Lua — Lua lives in the daemon. Per-app library is just IPC + command registry. Tiny, optional, feature-gated.

## Mesh Topology

| Node | WireGuard IP | Role |
|------|-------------|------|
| cachyos | 172.16.2.5 | Dev workstation (COSMIC desktop) |
| gcwg | 172.16.2.4 | Incus container on cachyos |
| mko | 172.16.2.210 | Production primary (web.kanary.org) |
| mmc | 172.16.2.9 | Production secondary (web.motd.com) |

## Implementation Phases

See `_doc/2026-03-08-arexx-adoption-plan.md` for the full 8-phase ARexx adoption plan.

| Phase | Focus | Work |
|-------|-------|------|
| 1 | Port Registry | inotify discovery, HELP handshake, daemon routing to app sockets |
| 2 | Standard Vocab | OPEN/SAVE/QUIT/HELP/INFO/ACTIVATE, RC 0/5/10/20 error codes |
| 3 | Clip List | SETCLIP/GETCLIP shared key/value store, named queues |
| 4 | Macro Menus | Script directory scanning, Scripts menu in apps, pre-addressed scripts |
| 5 | Orchestration | ADDRESS, process launching, wait_for_port, orchestrator/watcher patterns |
| 6 | Modules | Lua function libraries in ~/.config/cosmix/modules/ |
| 7 | Network Mesh | meshd bridge, @node.amp addressing, markweb web gateway |
| 8 | AI Agents | MCP server, natural language → Lua, agent templates |

## Standard Command Vocabulary

Every cosmix-port app MUST support: `OPEN`, `SAVE`, `SAVEAS`, `CLOSE`, `QUIT`, `HELP`, `INFO`, `ACTIVATE`.
Error codes: 0=success, 5=warning, 10=error, 20=failure (matching ARexx convention).
See `_doc/2026-03-08-arexx-adoption-plan.md` §3.2 for full specification.

## Commands

```bash
# Run Lua scripts
cosmix run scripts/my-automation.lua

# Start the daemon
cosmix daemon

# Interactive Lua shell
cosmix shell

# Cosmic-comp patches
./cosmic/patch.sh status
./cosmic/patch.sh install
```

## Document Conventions

- `_doc/` for design documents, `_journal/` for operational logs
- File names: `YYYY-MM-DD-lower-case-title.md`
- Date format: always `YYYY-MM-DD` with hyphens
