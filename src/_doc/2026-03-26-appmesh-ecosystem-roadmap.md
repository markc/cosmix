# AppMesh Ecosystem Roadmap

> Date: 2026-03-26

## Vision

A sovereign application mesh where every app is both a UI and a service port.
Apps communicate via AMP over WebSocket through a local hub that bridges to
remote peers via WireGuard mesh. The same Dioxus code runs on desktop (WebKitGTK)
and browser (WASM) — the distinction between "native" and "web" dissolves.

The breakthrough: Dioxus + Rust + AMP + WebSocket = one language, one framework,
every target, every app a mesh citizen.

## Crate Architecture

### Libraries (shared by all apps)

| Crate | Purpose |
|-------|---------|
| `cosmix-port` | AMP wire format (exists) |
| `cosmix-mesh` | WireGuard mesh, peer sync (exists) |
| `cosmix-client` | WebSocket + AMP client for apps (new) |
| `cosmix-ui` | Shared Dioxus components, theme, hub hook (new) |

### The Hub

| Crate | Purpose |
|-------|---------|
| `cosmix-hub` | Local WebSocket broker, app registry, mesh bridge (new) |

### Desktop/Web Apps

| App | UI | Service port | Status |
|-----|-----|-------------|--------|
| `cosmix-mail` | Email client | `mail.*` | Exists |
| `cosmix-view` | Markdown/DOT/image viewer | `view.*` | Exists |
| `cosmix-files` | Twin-pane file manager | `file.*` | Planned |
| `cosmix-edit` | Text editor (CodeMirror 6) | `edit.*` | Planned |
| `cosmix-paint` | Image annotation/screenshot | `paint.*` | Planned |
| `cosmix-term` | Terminal emulator | `term.*` | Planned |

### Infrastructure Services (service + optional web UI)

| App | Purpose | Service port |
|-----|---------|-------------|
| `cosmix-jmap` | JMAP/SMTP mail server | `jmap.*` | Exists |
| `cosmix-mon` | System monitoring | `mon.*` | Planned |
| `cosmix-log` | Log viewer/aggregator | `log.*` | Planned |
| `cosmix-dns` | DNS zone management | `dns.*` | Planned |
| `cosmix-wg` | WireGuard mesh admin | `wg.*` | Planned |
| `cosmix-backup` | PBS backup management | `backup.*` | Planned |

## How Apps Communicate

```
cosmix-view                     cosmix-files
    │                               │
    ├── ws://localhost:4200 ────────┤
    │           │                   │
    │       cosmix-hub              │
    │           │                   │
    │     mesh bridge               │
    │           │                   │
    │     remote hubs (mko, pve5)   │
```

- Apps connect to the local hub via WebSocket
- Hub maintains a registry: `{ "view": conn1, "mail": conn2, "files": conn3 }`
- Apps send AMP commands addressed by service name
- Hub routes locally or bridges to remote peers
- Apps don't know or care whether the target is local or remote

## Implementation Phases

### Phase 1 — Foundation

**Goal:** shared component library and local message broker.

- `cosmix-ui`: extract shared code from cosmix-mail and cosmix-view
  - Icons (already duplicated)
  - Theme/CSS constants
  - Common widget patterns (toolbar, modal, error banner)
- `cosmix-hub`: minimal axum WebSocket server
  - Accept connections, app registration
  - Route AMP messages by service name prefix
  - No mesh bridge yet — local IPC only
- `cosmix-client`: app-side WebSocket + AMP library
  - `connect()`, `send()`, `on()` API
  - Auto-reconnect
  - `use_hub()` Dioxus hook in cosmix-ui

### Phase 2 — Prove Inter-App Communication

**Goal:** two apps talking through the hub.

- `cosmix-files`: first hub-native app
  - Twin-pane file manager UI using cosmix-ui FileBrowser component
  - Registers as "files" service, handles `file.*` commands
  - `file.list`, `file.read`, `file.stat`, `file.pick`
- Retrofit `cosmix-view` to use hub
  - Send `file.pick` instead of calling rfd
  - Fallback to rfd if hub not running
- **Test:** cosmix-view asks cosmix-files to pick a file via AMP

### Phase 3 — Prove Mesh Communication

**Goal:** apps on different machines communicating transparently.

- `cosmix-hub`: add mesh bridge via cosmix-mesh
  - Connect to remote hubs over WireGuard WebSocket
  - Route commands addressed to remote peers
- `cosmix-mon`: lightweight system monitor
  - Runs on each peer, reports CPU/mem/disk/network
  - WASM dashboard aggregates status from all peers
- **Test:** browser on cachyos shows system status from mko and pve5

### Phase 4 — Productivity Apps

**Goal:** daily-driver desktop tools, all mesh-aware.

- `cosmix-edit`: text editor
  - CodeMirror 6 for editing (JS, embedded in WebKit)
  - Service port: `edit.open`, `edit.goto`, `edit.compose`
  - Other apps delegate: mail sends `edit.compose`, files sends `edit.open`
  - Edit files on remote peers transparently
- `cosmix-paint`: image annotation
  - HTML5 Canvas drawing tools (arrow, rect, text, blur)
  - Screenshot capture (`grim` on Wayland)
  - Service port: `paint.annotate`, `paint.screenshot`
- Retrofit `cosmix-mail` to use hub
  - `edit.compose` for email drafts
  - `file.pick` for attachments

### Phase 5 — Infrastructure Apps

**Goal:** replace bespoke admin scripts with mesh-native tools.

- `cosmix-dns`: replaces pdns-admin/nsctl workflows
- `cosmix-wg`: replaces wg-admin
- `cosmix-backup`: PBS dashboard across all nodes
- All accessible from any browser on the mesh

## Design Principles

1. **Every app is a service.** If it has a UI, it also has an AMP command port.
   cosmix-view is a viewer AND a `view.pick` service.

2. **The hub routes, apps implement.** The hub never implements business logic.
   File operations live in cosmix-files, not in the hub.

3. **Thin clients, smart services.** WASM apps are first-class because all
   heavy lifting happens server-side via AMP commands.

4. **Fallback gracefully.** If the hub isn't running, apps work standalone
   (direct filesystem access, rfd for file picking, etc.).

5. **One language, one framework.** Rust + Dioxus for everything. No Python,
   no Node, no JavaScript except as embedded engines (CodeMirror for editing).

6. **No Docker.** Incus or Proxmox containers. Binaries deployed directly.
