# Cosmix

A self-hosted sovereign intelligence stack: JMAP mail server, Dioxus cross-platform client, AMP mesh networking, AI inference pipeline, and a complete desktop computing environment — all Rust, scripted with [Mix](https://github.com/markc/mix). No Python, no Node, no Docker.

## What Cosmix Is

Cosmix is a personal computing platform that owns its entire stack. Email arrives via SMTP and is served over JMAP — no Postfix, no Dovecot, no third-party mail server. The desktop client renders with Dioxus across native Linux, macOS, Windows, and WASM in the browser. Nodes on a WireGuard mesh communicate using AMP (AppMesh Protocol) — a markdown-frontmatter wire format that makes every application scriptable from any other application, on any node, using the Mix shell. AI inference runs on dedicated servers and is accessed as a JMAP capability, so any standard email client gets AI features for free.

The name reads two ways: **Cos**mic + Mi**x** (the desktop shell powered by its scripting language), or simply "cosmix" — a mix of everything you need to own your digital life.

## Quick Start

```bash
git clone https://github.com/markc/cosmix ~/.cosmix
cd ~/.cosmix/src
cargo build --release
cp target/release/cosmix-{hubd,configd,menu,edit,view,shell} ~/.local/bin/
```

**Requirements:** Rust toolchain (2024 edition), Linux with WebKitGTK (for Dioxus desktop apps), PostgreSQL (for cosmix-maild).

Start the message broker and config daemon:

```bash
cosmix-hubd &                    # WebSocket message broker
cosmix-configd &                 # Config watcher, serves settings via AMP
```

Launch a GUI app:

```bash
cosmix-edit                      # Text editor
cosmix-shell                     # DCS shell (primary desktop surface)
```

Send an AMP command from Mix:

```mix
send "edit" ui.list              -- list all widgets in the editor
send "edit" ui.get id="path"     -- get the current file path
```

## Architecture

Cosmix is a 28-crate Rust monorepo. Every crate falls into one of three categories:

- **`cosmix-lib-*`** — libraries, imported as `cosmix_*` in Rust
- **`cosmix-*`** — GUI applications (Dioxus desktop/web)
- **`cosmix-*d`** — headless daemons and services

### Libraries

| Crate | Purpose |
|-------|---------|
| `cosmix-lib-amp` | AMP wire format — parse, serialize, route messages |
| `cosmix-lib-client` | AMP WebSocket client (native + WASM) |
| `cosmix-lib-config` | Typed config structs, TOML load/save, live reload |
| `cosmix-lib-daemon` | Shared daemon bootstrap (signal handling, logging, PID) |
| `cosmix-lib-mesh` | WireGuard mesh networking, WebSocket peer sync |
| `cosmix-lib-script` | Script discovery, TOML definitions, Mix runtime bridge, User menu |
| `cosmix-lib-ui` | Shared Dioxus components, OKLCH theme engine, icons |

### GUI Apps

| Crate | Purpose |
|-------|---------|
| `cosmix-backup` | Proxmox Backup Server dashboard |
| `cosmix-dialog` | Transient dialog utility (zenity replacement) |
| `cosmix-dns` | DNS zone management UI |
| `cosmix-edit` | Text editor with AMP-addressable widgets |
| `cosmix-files` | File browser |
| `cosmix-mail` | JMAP mail client (desktop + WASM) |
| `cosmix-menu` | System tray app launcher |
| `cosmix-mon` | System monitor with master-detail node list |
| `cosmix-scripts` | Mix + Bash script manager |
| `cosmix-settings` | Settings/preferences editor |
| `cosmix-shell` | DCS shell — primary UI surface (desktop + WASM) |
| `cosmix-view` | Markdown/image viewer with AMP-addressable widgets |
| `cosmix-wg` | WireGuard mesh admin |

### Daemons

| Crate | Purpose |
|-------|---------|
| `cosmix-configd` | Config file watcher, serves settings via AMP |
| `cosmix-hubd` | WebSocket message broker — all AMP traffic routes through here |
| `cosmix-indexd` | Semantic indexing + vector storage (candle + sqlite-vec) |
| `cosmix-logd` | Structured log aggregation |
| `cosmix-maild` | JMAP (RFC 8620/8621) + SMTP mail server |
| `cosmix-mcp` | Model Context Protocol bridge for Claude Code |
| `cosmix-mond` | System monitor daemon |
| `cosmix-webd` | WASM app server + CMS API |

## AMP — AppMesh Protocol

AMP is the nervous system of Cosmix. Every inter-process message — local or across the mesh — uses the same wire format: markdown frontmatter with BTreeMap headers and an optional body.

```
---
command: mailbox.list
to: maild
from: mail
id: a1b2c3
---
```

A response:

```
---
command: mailbox.list
rc: 0
type: response
id: a1b2c3
---
[{"id": "inbox-uuid", "name": "Inbox", "totalEmails": 42}]
```

**RC codes:** 0 = success, 5 = warning, 10 = error, 20 = failure.

**Transport layers:**
- **Local:** WebSocket connections to cosmix-hubd (all apps connect on startup)
- **Mesh:** WebSocket tunnels over WireGuard between nodes
- **Log files:** AMP messages are valid log entries — `grep` and `jq` work on them

**Addressing:** The `to` field routes messages. Local names (`edit`, `maild`) route within the node. Full mesh addresses (`edit.cosmix.mko.amp`) route across nodes via the mesh bridge.

### AMP-Addressable UI

Every GUI widget can be registered with AMP and controlled remotely. This is the ARexx vision brought to modern desktop computing — any application is scriptable from any other application:

```mix
-- Query all widgets in the editor
send "edit" ui.list

-- Toggle line numbers
send "edit" ui.invoke id="line-numbers"

-- Get the current file path
send "edit" ui.get id="path"

-- Set a value
send "edit" ui.set id="path" value="/tmp/new-file.txt"

-- Batch multiple operations
send "edit" ui.batch commands='[
    {"command": "ui.set", "id": "path", "value": "/tmp/test.md"},
    {"command": "ui.invoke", "id": "line-numbers"}
]'
```

Widget types: `AmpButton` (click), `AmpToggle` (on/off), `AmpInput` (text value). Widgets auto-register on mount and deregister on unmount via Dioxus lifecycle hooks.

### Cross-App Scripting

The real power emerges when commands chain across applications:

```mix
-- Get editor content, render it as markdown in the viewer
send "edit" edit.get-content
send "view" view.show-markdown body=$result

-- Open whatever file the editor has in the viewer
send "edit" edit.get-path
send "view" view.open path=$result
```

In Mix, this is a first-class language feature:

```mix
address "edit"
    edit.get-content
end
$content = $result

address "view"
    view.show-markdown body=$content
end
```

## Mix — The Scripting Language

Cosmix is scripted with [Mix](https://github.com/markc/mix), a pure-Rust language purpose-built for the cosmix ecosystem. Mix blends ARexx (PARSE, ADDRESS, everything-is-a-string), bash (pipes, `$sigils`, command substitution), and BASIC (keyword-driven, no braces) into a lightweight interpreter that compiles to WASM.

**Why Mix, not Lua/Python/bash?**
- `send`, `address`, and `emit` are language keywords, not library calls
- Pure Rust — no C dependencies, no `lua5.4-dev`, compiles to `wasm32-unknown-unknown`
- It's a real shell — bare words run commands, pipes work, tab completion works
- Purpose-built for AMP — result codes, JSON responses, and port discovery are native

Mix lives at `~/.mix/` and is linked into Cosmix as a path dependency (`mix-core`). The `cosmix-lib-script` crate bridges Mix with the hub client, injecting AMP context variables and wiring `send`/`address`/`emit` to real WebSocket connections.

See the [Mix README](https://github.com/markc/mix) for the full language reference.

## JMAP Mail Server (cosmix-maild)

A standards-compliant JMAP mail server implementing RFC 8620 (JMAP Core) and RFC 8621 (JMAP Mail), with SMTP inbound and outbound.

### Endpoints

- `GET /.well-known/jmap` — session resource (capabilities, account URLs)
- `POST /jmap` — method dispatch over `methodCalls[]` array
- `GET/POST /jmap/blob/{blobId}` — blob download/upload

### Supported Methods

**Core:** `Core/echo`, `Blob/upload`, `Blob/download`

**Mailbox:** `Mailbox/get`, `Mailbox/set`, `Mailbox/changes`, `Mailbox/query`

**Email:** `Email/get`, `Email/set`, `Email/changes`, `Email/query`, `Email/queryChanges`

**Calendar (JSCalendar RFC 8984):** `CalendarEvent/get`, `CalendarEvent/set`, `CalendarEvent/changes`, `CalendarEvent/query`

**Contacts (JSContact RFC 9553):** `ContactCard/get`, `ContactCard/set`, `ContactCard/changes`, `ContactCard/query`

**Submission:** `EmailSubmission/set` (SMTP outbound via mail-builder + mail-send)

### CLI

```bash
cosmix-maild migrate                    # apply SQL migrations
cosmix-maild account add <email> <pwd>  # create account (auto-creates Inbox/Drafts/Sent/Trash/Junk/Archive + Personal calendar + Contacts)
cosmix-maild account list
cosmix-maild account delete <email>
cosmix-maild queue list                 # SMTP outbound queue
cosmix-maild queue flush                # retry queued messages
cosmix-maild serve                      # start JMAP HTTP + SMTP listeners
```

### Database

PostgreSQL with sqlx (async, compile-time checked queries). State tracking via `changelog(account_id, object_type, object_id, change_type)` — JMAP state = max changelog ID per (account, type), powering `/changes` and `/query` efficiently. UUID primary keys. JSONB for email addresses, keywords, calendar events, and contacts.

### SMTP

**Inbound:** Accepts MAIL FROM / RCPT TO / DATA, parses RFC 5322 via mail-parser, stores blob + Email row, classifies spam via spamlite (per-account Bayesian classifier with SQLite databases), records changelog entry.

**Outbound:** `EmailSubmission/set` triggers mail-builder to construct MIME, mail-send to deliver. Failed deliveries queue in `smtp_queue` with exponential backoff (max 10 attempts).

### Spam Filtering

Each account gets its own spamlite SQLite database — no cross-user model contamination. The spamlite crate (at `~/.gh/spamlite`) provides Bayesian classification trained on the user's own mail. Moving messages to/from Junk trains the classifier.

## Mesh Networking

Cosmix nodes form a WireGuard mesh. Each node runs the full stack (hubd, configd, maild, mond) and is sovereign — no central server, no cloud dependency. The mesh provides:

- **AMP routing:** Messages addressed to `service.cosmix.node.amp` route across the mesh automatically
- **Peer sync:** Node discovery and health via WebSocket heartbeats over WireGuard
- **SMTP bypass:** Direct mail delivery between mesh nodes without touching the public internet (planned)

### Current Mesh

Four nodes on WireGuard /24:

| Node | Role |
|------|------|
| cachyos | Development workstation (CachyOS/Arch) |
| mko | Production mail + web server |
| gcwg | Secondary server |
| mmc | Mobile/auxiliary node |

## The DCS Shell

cosmix-shell is the primary user interface — a Dioxus desktop WebView window containing a complete application environment:

- **Left sidebar carousel:** Launcher, Files browser, Navigator
- **Centre panel:** Active absorbed application (full-width)
- **Right sidebar carousel:** System Monitor, Settings, Notifications, Help
- **Top nav:** Fixed bar with sidebar toggles and theme switcher

Apps can be **absorbed** (rendered as Dioxus components inside the shell's WebView — same process, instant switching) or **floating** (spawned as separate tao windows via `DesktopContext::new_window()`). Pop-out/pull-in buttons let users switch any panel between modes.

The same components compile to WASM. A browser tab connects to the hub over `wss://` and gets full capability via AMP delegation to daemons — the WASM build has zero native dependencies. cosmix-shell in a browser is a complete remote desktop.

**Compositor compatibility:** Any Wayland compositor (KDE, GNOME, sway, Hyprland, niri, cosmic-comp) and X11. Only dependency: WebKitGTK.

## Configuration

Config lives at `~/.config/cosmix/`. Key files:

- `settings.toml` — global settings (font size, theme hue, launcher config)
- `jmap.toml` — mail server config (database URL, blob dir, SMTP ports, TLS, DKIM, spam)
- `scripts/{service}/*.toml` — TOML script definitions for the User menu
- `scripts/{service}/*.mx` — Mix scripts for the User menu

## Build Commands

```bash
cd src                                       # Cargo workspace root
cargo build                                  # entire workspace (28 crates)
cargo build -p cosmix-maild                  # single crate
cargo build -p cosmix-maild --release        # release build
cargo check                                  # type-check without codegen
```

Dioxus client (requires `dx` CLI — `cargo binstall dioxus-cli`):

```bash
cd src/crates/cosmix-mail && dx serve                   # desktop hot-reload
cd src/crates/cosmix-mail && dx serve --platform web     # browser WASM
cd src/crates/cosmix-mail && dx serve --hotpatch         # Rust hot-patch
```

## Repository Structure

```
~/.cosmix/                         THE repo — clone and go
  CLAUDE.md                        AI agent context
  README.md                        This file
  LICENSE                          MIT
  .mcp.json                       Model Context Protocol server config
  src/                             Developer space (Cargo workspace root)
    Cargo.toml                     Workspace manifest (28 crates)
    Cargo.lock                     Dependency lockfile
    crates/
      cosmix-lib-amp/              AMP wire format + IPC
      cosmix-lib-client/           WebSocket client (native + WASM)
      cosmix-lib-config/           Typed config structs + TOML
      cosmix-lib-daemon/           Daemon bootstrap (signals, logging, PID)
      cosmix-lib-mesh/             WireGuard mesh networking
      cosmix-lib-script/           Mix runtime bridge + script discovery
      cosmix-lib-ui/               Shared Dioxus components + theme
      cosmix-backup/               Proxmox Backup Server dashboard
      cosmix-configd/              Config watcher daemon
      cosmix-dialog/               Transient dialog utility
      cosmix-dns/                  DNS zone management
      cosmix-edit/                 Text editor
      cosmix-files/                File browser
      cosmix-hubd/                 WebSocket message broker
      cosmix-indexd/               Semantic indexing + vector storage
      cosmix-logd/                 Log aggregation daemon
      cosmix-mail/                 JMAP mail client (desktop + WASM)
      cosmix-maild/                JMAP + SMTP mail server
      cosmix-mcp/                  MCP bridge for Claude Code
      cosmix-menu/                 System tray app launcher
      cosmix-mon/                  System monitor GUI
      cosmix-mond/                 System monitor daemon
      cosmix-scripts/              Mix + Bash script manager
      cosmix-settings/             Settings editor
      cosmix-shell/                DCS shell (primary UI surface)
      cosmix-view/                 Markdown/image viewer
      cosmix-webd/                 WASM app server + CMS API
      cosmix-wg/                   WireGuard mesh admin
    _doc/                          Design documents (30+ architectural specs)
    _journal/                      Development log
    _etc/                          Systemd service files
    _notes.md                      Current state and working notes
    db/                            Database schemas
    scripts/                       Script templates
    target/                        Cargo build artifacts (gitignored)
```

## External Dependencies

| Dependency | Path | Purpose |
|-----------|------|---------|
| `spamlite` | `~/.gh/spamlite` | Per-account Bayesian spam classifier (SQLite) |
| `mix-core` | `~/.mix/src/crates/mix-core` | Mix scripting language interpreter |

Both are path dependencies — not on crates.io, linked at build time.

## Key Technology Decisions

Every choice in Cosmix is deliberate. Here's what we use and what we don't.

| Decision | Choice | Not |
|----------|--------|-----|
| HTTP framework | axum | actix, warp |
| Database | sqlx (async, compile-time checked) | sea-orm, diesel |
| Async runtime | tokio | async-std |
| UI framework | Dioxus 0.7 (desktop + WASM) | libcosmic, Iced, egui |
| Scripting | Mix (pure Rust, AMP-native) | Lua, Python, bash |
| Allocator | mimalloc | system allocator |
| Containers | Incus, Proxmox | Docker (never) |
| Packages | paru (AUR on CachyOS/Arch) | — |
| TLS | rustls | OpenSSL |
| Hashing | BLAKE3 (blobs) | SHA-256 |

**AI lives in the server.** Inference runs on dedicated servers (Ollama, frontier APIs) and is exposed as JMAP capabilities. Any standard JMAP email client gets AI features. cosmix-mail gets richer UI, but the intelligence is a server concern, not a client concern.

## The ARexx Lineage

If you used an Amiga in the late 1980s, you encountered something remarkable: a scripting language where every application was scriptable and inter-process communication was a built-in statement, not a library. You could write an ARexx script that told your text editor to open a file, told your compiler to build it, told your debugger to run it, and collected the results — all using the same language primitives you'd use to add two numbers.

Cosmix brings this back. The combination of AMP (the wire protocol), Mix (the scripting language), and the hub (the message broker) recreates the ARexx ecosystem at mesh scale. A Mix script on your laptop can query the editor on your desktop, check system metrics on your server, and compose an email on your mail node — all with `send` statements, all over WireGuard, all using the same protocol.

```mix
-- Check disk usage across the mesh, email a report if any node is >80%
for each $node in ["cachyos", "mko", "gcwg"]
    send "mon.cosmix.${node}.amp" system.disk
    if $rc == 0 then
        $disks = json_parse($result)
        for each $disk in $disks
            if $disk.percent > 80 then
                $alert = "${node}: ${disk.mount} at ${disk.percent}%"
                send "maild" email.create to="mc@cosmix.dev" subject="Disk alert" body=$alert
            end
        end
    end
end
```

This script is readable by anyone who has written shell scripts. The `send` statements are as natural as `echo`. The mesh routing is transparent. The protocol handles serialization. Mix handles the logic. That's the vision: ARexx-at-mesh-scale for a sovereign computing platform.

## Design Philosophy

1. **Own the stack.** No SaaS dependencies in the data path. Email, calendar, contacts, files, AI — all self-hosted, all local-first.
2. **Protocol-first.** AMP dissolves the boundary between local and remote. A "local app" and a "remote app" are architecturally identical — both are AMP endpoints.
3. **One language.** Rust for the engine, Mix for the glue. No Python, no Node, no JavaScript except as Dioxus build output.
4. **Scriptable by default.** Every application exposes an AMP port. Every widget can be addressed. The user can automate anything without modifying source code.
5. **Self-contained.** Clone `~/.cosmix/`, build, run. One directory has everything. The same is true of `~/.mix/`. No scattered config files, no global installs.
6. **WASM-ready.** Desktop and browser are two render targets for the same code. The DCS shell in a browser tab is a complete remote desktop.
7. **No Docker.** Incus containers or Proxmox VMs. Docker adds a layer of abstraction that obscures the system rather than simplifying it.

## Gotchas

- **WebKit black screen:** cosmix-mail sets `WEBKIT_DISABLE_COMPOSITING_MODE=1` before Dioxus launch on Linux
- **spamlite path dep:** Lives at `~/.gh/spamlite`, not on crates.io
- **Spam isolation:** Per-account SQLite databases prevent cross-user model contamination
- **Thread formation:** Message-ID + In-Reply-To matching is not yet implemented
- **Design docs:** The `src/_doc/` directory contains 30+ architectural specifications — check these before guessing at intent

## Contributing

Cosmix is a personal sovereign computing project. Contributions are welcome but should align with the design philosophy above. The codebase is AI-assisted — Claude Code is part of the development workflow, and `CLAUDE.md` provides full project context for AI agents.

## License

MIT License — Copyright (c) 2026 Mark Constable <mc@cosmix.dev>
