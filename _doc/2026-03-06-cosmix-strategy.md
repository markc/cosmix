# Cosmix Strategy: ARexx for COSMIC

Date: 2026-03-06

## Origin

This document captures the strategic direction that emerged from a conversation about Rust web development, scripting languages, and the observation that COSMIC desktop (all Rust) creates a unique opportunity for a universal scripting layer — the same role ARexx played on the Amiga.

## The Insight

ARexx worked because every Amiga app exposed an ARexx port — a standardised scripting interface. The apps were native (C/68k), the glue was ARexx, and the result was an integrated desktop where any app could talk to any other app through scripts.

COSMIC is the first desktop environment since the Amiga era where the entire stack is one language (Rust). Compositor, panel, settings, file manager, terminal, text editor — all Rust, all iced. This is a platform, not just a desktop.

## Two Worlds, One Mesh

The architecture splits cleanly into two domains connected by WebSocket:

### Desktop World (Rust + Lua)
- **Rust binaries:** meshd, appmesh runtime, cosmic patches
- **Lua scripts:** the primary development language for automation, tools, workflows
- **D-Bus:** communication with COSMIC apps
- **Local:** runs on the COSMIC workstation

### Web World (Laravel + React)
- **Laravel 12 + Inertia/React + Reverb:** markweb platform
- **Self-contained:** serves browsers, manages state, provides APIs
- **Remote:** runs on any server, talks to desktop world via meshd WebSocket

The boundary is clean and natural. Neither world tries to be the other.

## The Neovim Model

The framing isn't "Rust apps with Lua scripting." It's "Lua apps with Rust engines."

- Neovim is a "C program" — but everything users touch is Lua
- Love2D games are "C++ programs" — but games are written in Lua
- OpenResty is "nginx" — but apps are Lua scripts

Cosmix follows the same pattern:
- The Rust runtime embeds LuaJIT via mlua
- Exposes UI, mesh, system, D-Bus APIs to Lua
- Every app, automation, and tool is a Lua script
- Hot-reloadable, no recompile during normal development
- Drop to Rust only when adding new capabilities to the runtime

## Lua as the Scripting Language

Why Lua over alternatives:

| Factor | Lua | Python | PHP | JavaScript |
|--------|-----|--------|-----|------------|
| Embeddable in Rust | mlua (excellent) | painful | ugly | V8 (heavy) |
| Runtime size | 300KB | 50MB+ | 30MB+ | 100MB+ |
| Learning curve | 1 day (from PHP) | n/a (banned) | already known | medium |
| Design philosophy | mechanism, not policy | batteries included | web-first | browser-first |
| Precedent | Neovim, Redis, nginx, game engines | data science | web apps | browsers |
| Hot-reload | native | possible | native | possible |
| COSMIC ecosystem fit | natural (Rust host) | foreign | foreign | foreign |

Lua can also replace Python as Claude's "reach for it" utility scripting language for regex, JSON processing, file manipulation, and automation tasks.

## What Changes in Each Subproject

### appmesh (MAJOR REFACTOR)
Current: Rust FFI core + PHP MCP plugins + QML/Qt6 UI (targeting KDE Plasma)
Future: Pure Rust runtime + Lua plugin system (targeting COSMIC)

Key changes:
- Drop PHP MCP server layer → Lua plugin system
- Drop QML/Qt6/Kirigami UI → iced (COSMIC-native) or Lua-driven UI
- Drop KDE-specific D-Bus (KWin, Klipper, Spectacle) → COSMIC D-Bus interfaces
- Keep Rust ports (AppMeshPort trait) → expose to Lua via mlua
- Keep AMP protocol → unchanged, already Rust
- Keep Unix socket server → unchanged
- Add mlua embedding → Lua scripts call port commands directly

The 66 tools become Lua-callable functions instead of PHP MCP tools.

### nodemesh (MINOR CHANGES)
Current: Pure Rust daemon (meshd), AMP protocol, WebRTC SFU
Future: Same, plus Lua scripting for mesh automation

Key changes:
- Add mlua embedding for scriptable mesh operations
- Lua scripts can send/receive AMP messages, manage peers, query status
- Core daemon logic stays Rust
- SFU stays Rust (performance-critical)

### markweb (NO CHANGES)
Current: Laravel 12 + Inertia/React + Reverb + DCS
Future: Same. Self-contained web world.

The web stack is mature, well-defined, and serves a different audience (browsers). No benefit from rewriting in Rust. Lua scripts in the desktop world can call markweb APIs via HTTP/WebSocket.

### cosmic (INCREMENTAL)
Current: cosmic-comp patches (animation timing)
Future: Additional COSMIC app patches, possibly new COSMIC applets

Key changes:
- More compositor patches as needed
- COSMIC panel applets (iced) for cosmix status/control
- Integration with appmesh for COSMIC-specific automation

## Migration Path

The refactor is incremental, not a rewrite:

1. **Phase 1: Lua runtime** — Add mlua to appmesh-core, expose existing Rust ports to Lua. PHP plugins continue working alongside.
2. **Phase 2: Port Lua plugins** — Rewrite PHP plugins as Lua modules one at a time. Each plugin is ~200-700 lines, straightforward translation.
3. **Phase 3: COSMIC targeting** — Replace KDE D-Bus calls with COSMIC equivalents as COSMIC apps stabilise their D-Bus interfaces.
4. **Phase 4: Drop PHP/QML** — Remove PHP MCP server and QML UI once all functionality is in Lua.
5. **Phase 5: Lua UI** — Explore iced bindings for Lua, or keep UI in Rust with Lua driving the logic.

## The ARexx Comparison

| ARexx (Amiga, 1987) | Cosmix (COSMIC, 2026) |
|----------------------|-------------------------|
| ARexx interpreter | LuaJIT (via mlua in Rust) |
| ARexx ports per app | AppMeshPort trait per service |
| AREXX: address command | mesh.port("name"):command() |
| String-based IPC | AMP (markdown frontmatter) |
| AmigaOS message ports | D-Bus + Unix sockets |
| CLI `rx script.rexx` | `cosmix run script.lua` |
| Every app has a port | Every COSMIC app has D-Bus |

The key difference: ARexx was limited to the local machine. Cosmix extends across the mesh — a Lua script on cachyos can orchestrate apps on mko and mmc transparently through meshd.

## Success Criteria

1. A Lua script can control COSMIC desktop apps (files, terminal, editor) via D-Bus
2. A Lua script can send/receive AMP messages through meshd to remote nodes
3. A Lua script can call markweb APIs (deploy, query, manage)
4. All of the above in a single script, hot-reloadable, no compilation
5. The Rust runtime compiles once and rarely changes during normal development

## Naming

**Cosmix** — your AI-powered desktop companion that works alongside you, automating tasks across your COSMIC desktop and mesh network. The name reflects the collaborative nature: Lua scripts are the instructions, Rust is the capable cosmix that executes them.
