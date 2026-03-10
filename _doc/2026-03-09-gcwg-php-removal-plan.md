# gcwg: PHP Removal & Pure Rust+Lua Migration Plan

> Replace FrankenPHP + Laravel + React with cosmix-web (Axum + HTMX + PostgreSQL).

**Date:** 2026-03-09
**Status:** In Progress — W1 complete (auth + sessions + DCS shell)
**Updated:** Revised after discovering cosmix-web crate already exists

---

## 1. Current State — What's Running on gcwg

| Service | Stack | Port(s) | Status |
|---------|-------|---------|--------|
| **FrankenPHP + Laravel (markweb)** | PHP 8.4 + React | 443 | **REPLACING** with cosmix-web |
| **cosmix-web** | Rust (Axum + HTMX) | 3000 | **W1 COMPLETE** |
| **Stalwart** | Rust | 25, 465, 993, 8443 | **KEEP** |
| **meshd** | Rust | 9800 | **ABSORB** into cosmix-daemon |
| **PostgreSQL 17** | C | 5432 | **KEEP** |
| **Valkey** | C | 6379 | **REMOVE** when Laravel gone |
| **Ollama** | Go | 11434 | **KEEP** |
| **PowerDNS** | C++ | 53, 8081 | **KEEP** |
| **Caddy** | Go | 443 | **REPLACE** with Axum TLS |
| **Laravel Reverb** | PHP | 8080 | **REPLACE** with Axum WebSocket |

---

## 2. What's Already Built (cosmix-web W1)

The `cosmix-web` crate is a standalone Axum server with:

- **Auth:** PostgreSQL session store (tower-sessions), bcrypt against markweb users table
- **DCS shell:** Three-column HTMX layout with carousel sidebars
- **Panels:** nav, conversations, mailboxes, theme, mesh (fragment endpoints)
- **Pages:** chat, mail, settings (fragment endpoints)
- **CSS:** base.css (908 lines, mobile-first design system) + app.css (OKLCH themes)
- **JS:** base.js (422 lines, sidebar/carousel/theme state) + app.js (HTMX nav sync)
- **Static serving:** `/static/*` via tower-http
- **Config:** `~/.config/cosmix/web.toml` (listen address + database URL)

### Architecture Decision: HTMX over React SPA

This is a better choice than the original plan's React SPA because:
- No Node.js, npm, or build step needed
- Server-driven UI — Axum renders HTML fragments, HTMX swaps them
- Hot-reloadable templates (via Askama or include_str)
- Simpler deployment — single Rust binary serves everything
- Lua can generate HTML fragments (ARexx for web)

---

## 3. Feature Migration Map

### What Laravel Does → What Replaces It

| Feature | Laravel Routes | cosmix-web Status | Approach |
|---------|---------------|-------------------|----------|
| **Auth/Sessions** | Fortify (6 routes) | **DONE** (W1) | bcrypt + tower-sessions |
| **Dashboard** | 2 routes | **Stub** | HTMX page fragment |
| **AI Agent Chat** | 12 routes + Reverb WS | **Stub** (chat.html) | Axum WS + Ollama HTTP |
| **Mail (JMAP)** | 6 routes | **Stub** (mail.html) | JMAP proxy to Stalwart |
| **Calendar** | 5 routes | Not started | CalDAV proxy to Stalwart |
| **Contacts** | 5 routes | Not started | CardDAV proxy to Stalwart |
| **Team Chat** | 12 routes + Reverb WS | Not started | Axum WS broadcast |
| **Desktop Automation** | 14 API routes | Not started | Delegate to cosmix-daemon IPC |
| **Mesh** | 8 routes | **Stub** (mesh.html) | cosmix-daemon mesh IPC |
| **User Management** | 4 routes | Not started | Direct SQL + Stalwart API |
| **File Sharing** | 4 routes | Not started | Static file serve + DB |
| **Settings** | 3 routes | **Stub** (settings.html) | HTMX form + SQL update |
| **System Events** | 7 routes | Not started | Axum WS broadcast |
| **Docs** | 2 routes | Not started | Markdown render |

### Features That Become cosmix-daemon IPC Calls

These are already implemented in cosmix-daemon. cosmix-web just needs thin HTTP handlers that call the daemon over IPC:

```
cosmix-web                    cosmix-daemon
    │                              │
    ├─ GET /api/windows ──────────→ IpcRequest::ListWindows
    ├─ POST /api/port/call ───────→ IpcRequest::CallPort
    ├─ GET /api/mesh/status ──────→ IpcRequest::MeshStatus
    ├─ GET /api/mesh/peers ───────→ IpcRequest::MeshPeers
    ├─ POST /api/dbus ────────────→ IpcRequest::DbusCall
    ├─ GET /api/config/* ─────────→ IpcRequest::ConfigList/Read/Write
    ├─ GET /api/clips ────────────→ IpcRequest::ListClips
    ├─ POST /api/screenshot ──────→ IpcRequest::Screenshot
    └─ etc.
```

### Features That Go Direct to Stalwart

No PHP proxy needed — cosmix-web reverse-proxies to Stalwart's HTTP API:

```
cosmix-web                    Stalwart (127.0.0.1:8443)
    │                              │
    ├─ /jmap/* ───────────────────→ JMAP API (mail)
    ├─ /dav/* ────────────────────→ CalDAV/CardDAV
    └─ /admin/* ──────────────────→ Stalwart admin API
```

---

## 4. Implementation Phases (Revised)

### W2: Desktop Automation + Mesh Panels (next)

**Goal:** Wire cosmix-web to cosmix-daemon for desktop features and mesh status.

1. Add cosmix-daemon IPC client to cosmix-web (connect to daemon Unix socket)
2. `/api/panel/mesh` → call `IpcRequest::MeshPeers`, render peer list HTML
3. `/api/windows` → call `IpcRequest::ListWindows`, return HTML table
4. `/api/ports` → call `IpcRequest::ListPorts`, return HTML
5. `/api/port/{port}/{cmd}` → call `IpcRequest::CallPort`

**Dependencies:** cosmix-port (already in Cargo.toml)

### W3: Mail (JMAP Proxy)

**Goal:** Mail page works through Stalwart.

1. Add reverse proxy middleware: `/jmap/*` → `http://127.0.0.1:8443`
2. HTMX mail page: mailbox list, message list, message view
3. Compose form with HTMX submission
4. JMAP session management (store token in PostgreSQL session)

### W4: AI Agent Chat

**Goal:** Streaming chat with Ollama.

1. Axum WebSocket endpoint `/ws/chat`
2. HTTP client to Ollama (`http://127.0.0.1:11434/api/chat`)
3. Chat session + message persistence in PostgreSQL
4. HTMX chat UI: message history, streaming response display
5. SSE fallback for non-WebSocket clients

### W5: Settings, Users, Calendar/Contacts

**Goal:** Complete the remaining admin features.

1. Settings page: profile edit, password change
2. User CRUD (if multi-user needed)
3. Calendar/contacts: reverse-proxy to Stalwart CalDAV/CardDAV
4. HTMX calendar view (month/week/day)

### W6: TLS + Production + Decommission PHP

**Goal:** Replace Caddy, go live, remove PHP.

1. Add rustls TLS to Axum (load existing certs)
2. Switch from :3000 to :443
3. systemd service for cosmix-web
4. Stop Laravel, Reverb, Caddy, Valkey services
5. Remove PHP, FrankenPHP, Composer, Node.js packages
6. Point DNS/Caddy config to cosmix-web

---

## 5. Relationship: cosmix-web vs cosmix-daemon

**Two binaries, complementary roles:**

| | cosmix-daemon | cosmix-web |
|--|---------------|------------|
| **Runs on** | Desktop (cachyos) | Server (gcwg, mko, mmc) |
| **Purpose** | Desktop automation, mesh, Lua scripting | Web UI, API, mail proxy |
| **IPC** | Unix socket server | Unix socket client (to daemon) |
| **Users** | Single user (desktop owner) | Multi-user (web auth) |
| **Frontend** | None (CLI + Lua) | HTMX + server-rendered HTML |
| **Database** | In-memory state + clip list file | PostgreSQL |

cosmix-web calls cosmix-daemon over IPC when it needs desktop/mesh features. On server nodes without a desktop (gcwg), cosmix-daemon still runs for mesh networking and Lua scripting.

---

## 6. What Gets Removed from gcwg

| Remove | When |
|--------|------|
| Laravel Reverb service | W4 (after chat migrated) |
| Laravel Scheduler service | W5 (after all features migrated) |
| FrankenPHP + Caddy | W6 |
| PHP 8.4 packages | W6 |
| Valkey/Redis | W6 |
| Node.js/npm/bun | W6 (no React build needed) |
| Composer packages | W6 |
| meshd service | Already done (Phase 7) |

### What Remains

| Service | Stack | Purpose |
|---------|-------|---------|
| **cosmix-web** | Rust (Axum) | Web UI + API |
| **cosmix-daemon** | Rust | Mesh + desktop automation + Lua |
| **Stalwart** | Rust | Mail/calendar/contacts |
| **PostgreSQL** | C | App DB + Stalwart + PowerDNS |
| **Ollama** | Go | LLM inference |
| **PowerDNS** | C++ | DNS authority |

---

## 7. Lua's Role in cosmix-web

Lua scripts can define custom web endpoints:

```lua
-- ~/.config/cosmix/http/greeting.lua
cosmix.http.route("GET", "/api/custom/greeting", function(req)
    local windows = cosmix.windows()
    return {
        greeting = "Hello " .. (req.query.name or "world"),
        desktop_windows = #windows,
        node = cosmix.hostname(),
    }
end)
```

This is future work (Phase 8 territory) but the architecture supports it: cosmix-web calls cosmix-daemon IPC, Lua runs in the daemon.

---

## 8. Success Criteria

Migration is **complete** when:

- [ ] cosmix-web on gcwg serves the DCS shell on :443 with TLS
- [ ] AI agent chat works (Ollama streaming via WebSocket)
- [ ] Mail works through Stalwart JMAP proxy
- [ ] Calendar/contacts work through Stalwart DAV proxy
- [ ] Mesh status panel shows live peer data from cosmix-daemon
- [ ] Desktop automation panel calls cosmix-daemon IPC
- [ ] No PHP processes running on gcwg
- [ ] No Caddy, Valkey, or FrankenPHP installed
- [ ] `systemctl list-units | grep -E "php|caddy|valkey|reverb"` returns nothing
