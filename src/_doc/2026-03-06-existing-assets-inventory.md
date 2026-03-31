# Existing Assets Inventory

Date: 2026-03-06

What we already have across the four subprojects, and what carries forward into Cosmix.

## appmesh — Desktop Automation

### Carries Forward (Rust)

| Asset | LOC | Location | Notes |
|-------|-----|----------|-------|
| AMP wire format parser | 371 | `crates/appmesh-core/src/amp.rs` | Markdown frontmatter protocol, shared with nodemesh |
| Unix socket server | 249 | `crates/appmesh-core/src/socket.rs` | Tokio async, length-prefixed |
| AppMeshPort trait | 61 | `crates/appmesh-core/src/port.rs` | Core abstraction — the "ARexx port" |
| ClipboardPort | 92 | `crates/appmesh-core/src/ports/clipboard.rs` | zbus D-Bus, needs COSMIC rework |
| NotifyPort | 102 | `crates/appmesh-core/src/ports/notify.rs` | freedesktop standard, works on COSMIC |
| ScreenshotPort | 89 | `crates/appmesh-core/src/ports/screenshot.rs` | Needs COSMIC rework |
| WindowsPort | 205 | `crates/appmesh-core/src/ports/windows.rs` | Needs COSMIC rework |
| InputPort + EIS | 454 | `crates/appmesh-core/src/eis.rs` + `ports/input.rs` | Needs COSMIC input portal check |
| MailPort (JMAP) | 1115 | `crates/appmesh-core/src/ports/mail.rs` | Desktop-agnostic, 17 commands |
| CLI binary | — | `crates/appmesh-cli/` | Rework into `cosmix` binary |

### Carries Forward (Knowledge)

| Asset | Location | Notes |
|-------|----------|-------|
| AMP protocol spec | `_doc/2026-02-28-amp-protocol-specification.md` | 442 lines, definitive spec |
| Plugin architecture pattern | `CLAUDE.md`, `.claude/runbooks/add-plugin.md` | Port pattern, tool registration |
| D-Bus app exploration docs | `_doc/` (27 files) | KDE-specific but methodology transfers |
| Skills and guidelines | `.claude/skills/`, `.claude/guidelines/` | Claude Code integration patterns |

### Gets Dropped

| Asset | Reason |
|-------|--------|
| PHP MCP server (3,429 LOC) | Replaced by Lua |
| QML/Qt6 UI (mail client, plasmoids) | COSMIC uses iced |
| C++ Kate plugin | COSMIC has cosmic-edit |
| KDE-specific D-Bus calls | Replaced by COSMIC D-Bus |
| `reis` custom fork (EIS) | Evaluate COSMIC input portals |

## nodemesh — Mesh Control Plane

### Carries Forward (All of It)

| Asset | LOC | Location | Notes |
|-------|-----|----------|-------|
| meshd daemon | ~2000 | `crates/meshd/src/` | Complete: peers, bridge, server |
| AMP crate | ~400 | `crates/amp/src/` | Wire format parser (separate from appmesh's) |
| SFU (WebRTC) | ~1500 | `crates/sfu/src/` | str0m-based, rooms, relay, sessions |
| Peer manager | — | `crates/meshd/src/peer/` | WebSocket peer connections |
| Bridge (Unix socket) | — | `crates/meshd/src/bridge/` | Laravel ↔ meshd communication |
| Bridge (WebSocket) | — | `crates/meshd/src/bridge/ws.rs` | Persistent bidirectional bridge |
| Config (TOML) | — | `config/*.toml` | Node, peers, bridge, SFU settings |

### Minor Addition

- Embed mlua for scriptable mesh operations (send AMP, query peers, manage rooms)

## markweb — Web Platform

### No Changes (Self-Contained Web World)

| Asset | Notes |
|-------|-------|
| Laravel 12 + Inertia/React | Mature web stack |
| DCS (Dual Carousel Sidebars) | 7 left + 4 right panels |
| AI Agent runtime | Anthropic + Ollama providers |
| JMAP mail integration | Stalwart backend |
| Text chat | Channels, threads, reactions via Reverb |
| SearchX | 6-engine concurrent search |
| Mesh bridge service | Unix socket client to meshd |
| AmpMessage DTO | AMP parsing in PHP |
| Deploy tooling | `bin/deploy` script |

markweb stays Laravel+React. Lua scripts in the desktop world call markweb's REST/WS APIs.

## cosmic — Desktop Patches

### Carries Forward

| Asset | Location | Notes |
|-------|----------|-------|
| cosmic-comp submodule | `cosmic-comp/` | Upstream tracking with local patches |
| Animation patches | `src/shell/mod.rs`, `src/shell/layout/tiling/mod.rs` | 200ms → 1ms |
| Patch tracking docs | `_notes.md`, `_journal/` | 8 additional candidates documented |
| Build system knowledge | `CLAUDE.md` | Dependencies, features, Makefile |

### Future Additions

- COSMIC panel applets for cosmix status
- Additional compositor patches as needed
- D-Bus interface documentation for COSMIC apps

## Shared Assets (Cross-Project)

| Asset | Used By | Notes |
|-------|---------|-------|
| AMP protocol | appmesh, nodemesh, markweb | Two implementations (Rust in both, PHP in markweb) — consolidate |
| WireGuard mesh | nodemesh, markweb | 4-node topology, already operational |
| D-Bus automation | appmesh, cosmic | Pattern transfers from KDE to COSMIC |
| Journal/doc conventions | all | `_journal/YYYY-MM-DD.md`, `_doc/YYYY-MM-DD-title.md` |

## LOC Summary

| Project | Rust | PHP | QML/C++ | Total | Carries Forward |
|---------|------|-----|---------|-------|-----------------|
| appmesh | 3,155 | 3,429 | ~1,000 | ~7,584 | ~2,700 (Rust core) |
| nodemesh | ~4,000 | — | — | ~4,000 | ~4,000 (all) |
| markweb | — | ~15,000 | — | ~15,000 | ~15,000 (unchanged) |
| cosmic | — (upstream) | — | — | patches only | patches |
| **Total** | **~7,155** | **~18,429** | **~1,000** | **~26,584** | **~21,700** |

The refactor drops ~4,884 LOC of PHP/QML from appmesh and replaces it with Lua plugins (~2,000 LOC estimated) plus the mlua embedding layer (~500 LOC Rust).
