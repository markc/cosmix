# Axum Migration Plan: Laravel → Pure Rust Web Stack

> Replace markweb's Laravel/FrankenPHP with an Axum API server that is also a cosmix port on the mesh.

**Date:** 2026-03-09
**Status:** Planning
**Depends on:** Phase 7 mesh integration (in progress), AMP v0.4 wire format

---

## 1. What We're Replacing

markweb is a Laravel 12 + React 19 application serving:

| Service Area | Routes | Complexity |
|---|---|---|
| AI Agent chat | `/chat`, WebSocket via Reverb | High — streaming, sessions, tool execution |
| Mail (JMAP proxy) | `/mail`, `/api/jmap/*` | Medium — proxies to Stalwart |
| Calendar/Contacts | `/calendars`, `/addressbooks` | Medium — SabreDAV PHP, replaceable by Stalwart CalDAV |
| Text chat | `/text-chat` | Medium — channels, messages, reactions |
| Mesh bridge | `/api/mesh/*` | Low — replaced entirely by cosmix mesh |
| System events | `/api/system-events` | Low |
| File sharing | `/shared-files` | Low |
| Workspace/settings | `/workspace`, `/settings` | Low |

**Infrastructure being retired:**
- FrankenPHP (PHP 8.4 + Caddy)
- Laravel framework + Eloquent ORM
- Composer dependency management
- Laravel Reverb (WebSocket server)
- SabreDAV (CalDAV/CardDAV — Stalwart handles this natively)

---

## 2. What We're Building

A single Rust binary: `cosmix-web` (or extend `cosmix daemon` with a `--web` flag).

```
cosmix-web
├── Static file serving (React build output)
├── REST API (replaces Laravel routes)
├── WebSocket (replaces Reverb)
├── JMAP proxy (to Stalwart)
├── cosmix-port (AMP on mesh)
└── PostgreSQL via SeaORM
```

### Crate Structure Option A: Separate Binary

```
crates/cosmix-web/
├── Cargo.toml
├── src/
│   ├── main.rs          ← CLI + server startup
│   ├── app.rs           ← Axum Router assembly
│   ├── config.rs        ← Web server config
│   ├── db/              ← SeaORM entities + migrations
│   │   ├── mod.rs
│   │   ├── entities/    ← Generated from existing schema
│   │   └── migration/   ← Sea-orm-migration
│   ├── routes/
│   │   ├── mod.rs
│   │   ├── auth.rs      ← Login, session, middleware
│   │   ├── mail.rs      ← JMAP proxy to Stalwart
│   │   ├── chat.rs      ← Text chat channels
│   │   ├── agent.rs     ← AI agent sessions
│   │   ├── files.rs     ← Shared file management
│   │   └── mesh.rs      ← Mesh status/control (cosmix-port calls)
│   ├── ws/              ← WebSocket handling
│   │   ├── mod.rs
│   │   ├── chat.rs      ← Real-time chat
│   │   └── agent.rs     ← Streaming agent responses
│   ├── services/
│   │   ├── jmap.rs      ← Stalwart JMAP client
│   │   ├── agent.rs     ← Agent runtime (LLM calls)
│   │   └── memory.rs    ← Embedding + pgvector search
│   └── middleware/
│       ├── auth.rs      ← Session-based auth
│       └── cors.rs
```

### Crate Structure Option B: Feature-gated in cosmix-daemon

Add `--web` flag to the existing `cosmix` binary:

```toml
[features]
web = ["axum", "sea-orm", "tower-sessions", "axum-login"]
```

**Recommendation:** Option A (separate binary). Reason: the web server has very different dependencies (SeaORM, template rendering, session storage) from the desktop daemon. A separate crate keeps compile times and binary sizes independent. They communicate over the mesh like any other cosmix ports.

---

## 3. Technology Mapping

| Laravel/React | Axum/Rust | Crate |
|---|---|---|
| `Route::get/post` | `axum::Router::new().route()` | axum 0.8 |
| Controllers | `async fn handler()` | axum |
| Blade/Inertia views | askama templates (HTML fragments) | askama, askama_axum |
| React components | Vanilla HTML + base.css + base.js | — (no framework) |
| Eloquent ORM | SeaORM entities | sea-orm |
| Migrations | sea-orm-migration | sea-orm-migration |
| Middleware | Tower layers | tower, tower-http |
| Auth sessions | axum-login + tower-sessions | axum-login, tower-sessions |
| Password hashing | bcrypt (existing hashes) | bcrypt |
| Request validation | Custom extractors or validator | validator |
| JSON responses | `axum::Json<T>` or HTML fragments | axum, askama |
| File uploads | `axum::extract::Multipart` | axum |
| WebSocket (Reverb+Echo) | Native Axum WS (simple JSON) | axum (built-in) |
| Queue/jobs | tokio::spawn + channels | tokio |
| Mail sending | lettre or direct SMTP to Stalwart | lettre |
| Caching | moka (in-memory) or Redis | moka / redis |
| Logging | tracing | tracing, tracing-subscriber |
| Config | TOML config file | toml |
| Static files | tower-http ServeDir | tower-http |
| CORS | tower-http CorsLayer | tower-http |
| LLM streaming | Server-Sent Events (SSE) | axum (Sse response) |

---

## 4. Migration Phases

### Phase W1: DCS Shell + Auth + Panel Endpoints

**Goal:** Axum serves the vanilla DCS shell, handles login, and renders sidebar panels as HTML fragments.

**Work:**
1. Create `crates/cosmix-web/` with Axum skeleton
2. Copy base.css + base.js from dcs.spa into `templates/static/`
3. Create index.html DCS shell with panel structure (askama template)
4. Generate SeaORM entities from existing PostgreSQL schema (`sea-orm-cli generate entity`)
5. Implement session-based auth (axum-login + tower-sessions with PostgreSQL store, **bcrypt** for existing passwords)
6. Panel endpoints: `GET /api/panel/{id}` returning HTML fragments via askama
7. Page endpoints: `GET /page/{name}` for main content area (HTMX swaps into `<main>`)
8. Vendor htmx.min.js + ws.js + sse.js into `static/`
9. Write app.js (~50 lines): nav link handling, SSE session switching
10. Deploy alongside FrankenPHP on a different port

**Verification:** User can load the DCS interface from Axum, log in, panels load via HTMX `hx-trigger="revealed"`, navigation works.

**New deps:** `axum`, `sea-orm`, `tower-http`, `tower-sessions`, `axum-login`, `bcrypt`, `askama`, `askama_axum`, `serde`, `tokio`

### Phase W2: Mail + Calendar (Stalwart Direct)

**Goal:** Remove the Laravel JMAP/CalDAV proxy layer. Axum proxies JMAP directly to Stalwart.

**Work:**
1. JMAP proxy route: `POST /api/jmap` → forward to Stalwart JMAP endpoint
2. Attachment proxy: `GET /api/jmap/attachment/:blobId` → Stalwart blob download
3. CalDAV/CardDAV: Reverse-proxy `/.well-known/{caldav,carddav}` to Stalwart
4. Remove SabreDAV PHP dependency entirely

**Verification:** React mail UI works through Axum. Calendar sync with Thunderbird works.

**Key insight:** The React frontend already talks JMAP directly via `jmap-jam`. The Laravel layer is mostly a proxy with auth. Axum just needs to forward authenticated requests.

### Phase W3: Text Chat + WebSocket

**Goal:** Real-time chat and live panel updates via native Axum WebSocket.

**Work:**
1. WebSocket endpoint: `GET /ws` with session auth on upgrade
2. Simple JSON message protocol (no Pusher, no channels abstraction):
   - Server→client: `{ type: "chat_message", channel: "...", html: "..." }`
   - Server→client: `{ type: "panel_update", panel: "conversations", html: "..." }`
   - Client→server: `{ type: "typing", channel: "..." }`
3. Chat message CRUD routes: `GET/POST /api/chat/channels`, `GET/POST /api/chat/messages`
4. Presence tracking: `HashMap<UserId, Vec<WsSender>>` in shared state
5. askama templates for chat message fragments

**Data model (from existing schema):**
- `chat_channels` — name, type (public/private/dm), created_by
- `chat_channel_members` — channel_id, user_id, role, last_read_at
- `chat_messages` — channel_id, user_id, content, type, parent_id
- `chat_reactions` — message_id, user_id, emoji

**Verification:** Text chat works end-to-end. Messages appear in real-time via WebSocket. No Echo/Pusher involved.

### Phase W4: AI Agent Runtime

**Goal:** Port the agent chat system to Rust.

This is the most complex migration. markweb's agent system includes:
- `AgentRuntime` — orchestrates LLM calls with tool use
- `ContextAssembler` — builds conversation context from history + memories
- `IntentRouter` — classifies user intent
- `ModelRegistry` — multi-provider LLM support
- `ToolExecutor` — runs bash, HTTP, and custom tools
- Streaming responses via WebSocket

**Work:**
1. LLM client: `reqwest` calls to Anthropic/OpenAI APIs (streaming SSE)
2. Agent session management: create, load, message history
3. Tool execution framework: bash exec, HTTP calls, cosmix port calls
4. Memory/embedding: Ollama embeddings + pgvector similarity search
5. Streaming WebSocket responses to React frontend
6. System prompt template management

**Alternative approach:** The agent runtime could be a cosmix port itself. Desktop Lua scripts could invoke it. The web frontend would call the same agent port via Axum.

**Verification:** Agent chat works end-to-end. Tool calls execute. Streaming responses render.

### Phase W5: Remaining Routes + Retire Laravel

**Goal:** Migrate all remaining routes, shut down FrankenPHP.

**Work:**
1. Shared files management
2. System events API
3. User settings
4. Workspace management
5. Remove reverse-proxy to Laravel
6. Stop FrankenPHP service
7. Update systemd units

**Verification:** Full application works entirely from Axum. No PHP processes running.

---

## 5. Database Strategy

### Keep the existing schema

The PostgreSQL schema is language-agnostic. SeaORM generates Rust entities from the existing tables:

```bash
sea-orm-cli generate entity -u postgres://... -o crates/cosmix-web/src/db/entities/
```

This produces a Rust struct per table with derives for `DeriveEntityModel`, `ActiveModelBehavior`, etc. No migration needed — same tables, same data.

### Migration tool

New migrations (post-Laravel) use `sea-orm-migration`:

```rust
#[async_trait]
impl MigrationTrait for Migration {
    async fn up(&self, manager: &SchemaManager) -> Result<(), DbErr> {
        manager.create_table(
            Table::create()
                .table(MyNewTable::Table)
                .col(ColumnDef::new(MyNewTable::Id).uuid().not_null().primary_key())
                .to_owned(),
        ).await
    }
}
```

### pgvector

SeaORM supports pgvector through `sea-orm-pgvector` extension or raw SQL:

```rust
// Similarity search
let memories = Entity::find()
    .from_raw_sql(Statement::from_sql_and_values(
        DbBackend::Postgres,
        "SELECT * FROM memories WHERE embedding <=> $1::vector ORDER BY embedding <=> $1::vector LIMIT $2",
        [embedding_vec.into(), limit.into()],
    ))
    .all(&db)
    .await?;
```

---

## 6. Frontend: Vanilla DCS Replaces React Entirely

### The insight

The current React layer (Inertia + Zustand + Radix + Tailwind + Echo) is a heavyweight reimplementation of the DCS pattern that already works in ~950 lines of vanilla CSS+JS. The React app-dual-sidebar-layout.tsx, panel-carousel.tsx, sidebar.tsx, and theme-context.tsx collectively reimplement what base.css + base.js already do.

**Decision: Drop React entirely. Use the vanilla DCS shell (base.css + base.js) with Axum serving HTML fragments.**

### What dies

- React 19
- Inertia v2
- Tailwind CSS (replaced by DCS base.css OKLCH tokens)
- Zustand (replaced by base.js localStorage state)
- Radix UI (replaced by native HTML + base.css components)
- Laravel Echo / Pusher protocol (replaced by raw WebSocket)
- Bun / npm / node_modules (no build step)
- Wayfinder route generation
- All JSX compilation

### What replaces it

```
Axum serves:
  GET /                        → index.html (DCS shell with base.css + base.js)
  GET /api/panel/{id}          → HTML fragment (sidebar panel content)
  GET /api/page/{name}         → HTML fragment (main content area)
  POST /api/chat/{id}/send     → SSE stream (LLM response)
  GET /api/mail/status         → JSON (JMAP proxy)
  WS /ws                       → live updates (panel refresh, notifications)
```

### Architecture

```
index.html (static DCS shell)
├── base.css     ← layout, carousel, sidebars, theming (~700 lines)
├── base.js      ← state, sidebar toggle, panel nav, theme (~250 lines)
├── app.css      ← OKLCH color schemes, app-specific styles
└── app.js       ← fetch panel content, WebSocket, SSE streaming (~200 lines)

Axum backend
├── Static file serving (index.html + CSS + JS)
├── /api/panel/{id}    → HTML fragment endpoints (askama templates)
├── /api/page/{name}   → Main content area HTML
├── /api/chat/...      → Agent chat (SSE streaming)
├── /api/jmap/...      → JMAP proxy to Stalwart
├── /ws                → WebSocket (live panel updates)
└── cosmix-port        → mesh integration
```

### Panel content via HTML fragments

Each sidebar panel is an HTML fragment served by Axum. Panels load on demand:

```javascript
// app.js — extend base.js with panel loading
async function loadPanel(panelId) {
    const el = document.getElementById(`panel-${panelId}`);
    if (el && !el.dataset.loaded) {
        el.innerHTML = await (await fetch(`/api/panel/${panelId}`)).text();
        el.dataset.loaded = '1';
    }
}
```

Axum renders HTML fragments using askama (Jinja2-like Rust templates):

```rust
#[derive(Template)]
#[template(path = "panels/conversations.html")]
struct ConversationsPanel {
    sessions: Vec<AgentSession>,
}

async fn panel_conversations(
    State(state): State<AppState>,
    session: Session,
) -> impl IntoResponse {
    let user = session.user()?;
    let sessions = db::agent_sessions::latest_for_user(&state.db, user.id, 50).await?;
    ConversationsPanel { sessions }.into_response()
}
```

### WebSocket: Simple, No Pusher

Raw WebSocket — no protocol complexity, no channel auth:

```javascript
// app.js
const ws = new WebSocket(`wss://${location.host}/ws`);
ws.onmessage = (e) => {
    const msg = JSON.parse(e.data);
    switch (msg.type) {
        case 'panel_update':
            document.getElementById(`panel-${msg.panel}`).innerHTML = msg.html;
            break;
        case 'chat_delta':
            appendChatDelta(msg.session, msg.delta);
            break;
        case 'notification':
            Base.toast(msg.text, msg.level);
            break;
    }
};
```

Axum WS handler:

```rust
async fn ws_handler(
    ws: WebSocketUpgrade,
    session: Session,  // auth check happens here
    State(state): State<AppState>,
) -> impl IntoResponse {
    let user = session.user()?;
    ws.on_upgrade(move |socket| handle_ws(socket, user, state))
}
```

Session auth on the HTTP upgrade request — once connected, no per-message auth needed.

### Agent chat: SSE streaming

LLM responses stream via Server-Sent Events (simpler than WebSocket for unidirectional streaming):

```javascript
// app.js
async function sendChat(sessionId, message) {
    const response = await fetch(`/api/chat/${sessionId}/send`, {
        method: 'POST',
        body: JSON.stringify({ message }),
        headers: { 'Content-Type': 'application/json' },
    });
    const reader = response.body.getReader();
    const decoder = new TextDecoder();
    while (true) {
        const { done, value } = await reader.read();
        if (done) break;
        appendChatDelta(sessionId, decoder.decode(value));
    }
}
```

### HTMX: Adopted

HTMX (14KB, zero deps) replaces all custom fetch/SSE/WebSocket JS with HTML attributes. This eliminates app.js as a data-fetching layer entirely.

**Panel loading:**
```html
<div class="panel" hx-get="/panel/conversations" hx-trigger="revealed" hx-swap="innerHTML">
    Loading...
</div>
```

**Form submission:**
```html
<form hx-post="/chat/send" hx-swap="beforeend" hx-target="#messages">
    <input name="message" /><button type="submit">Send</button>
</form>
```

**LLM streaming via SSE:**
```html
<div hx-ext="sse" sse-connect="/chat/stream/current" sse-swap="token" hx-swap="beforeend">
</div>
```

**Live updates via WebSocket (OOB swaps):**
```html
<div hx-ext="ws" ws-connect="/ws">
    <!-- Server pushes: <div id="panel-conversations" hx-swap-oob="innerHTML">...</div> -->
    <!-- HTMX automatically swaps the element by ID -->
</div>
```

**Agent chat SSE on Axum side:**
```rust
async fn chat_stream(
    Path(session_id): Path<Uuid>,
    State(state): State<AppState>,
) -> Sse<impl Stream<Item = Result<Event, Infallible>>> {
    let stream = llm_stream(&state, session_id).await;
    Sse::new(stream.map(|token| {
        Ok(Event::default()
            .event("token")
            .data(format!("<span>{}</span>", html_escape(&token))))
    }))
}
```

**WebSocket OOB push on Axum side:**
```rust
async fn handle_ws(mut socket: WebSocket, user: User, state: AppState) {
    let mut rx = state.broadcast.subscribe();
    while let Ok(event) = rx.recv().await {
        if event.user_id == user.id {
            let html = render_panel(&state.db, &event.panel, user.id).await;
            let oob = format!(
                r#"<div id="{}" hx-swap-oob="innerHTML">{html}</div>"#,
                event.panel
            );
            socket.send(Message::Text(oob)).await.ok();
        }
    }
}
```

**Result:** app.js shrinks to ~50 lines (just DCS-specific glue like nav link handling and SSE session switching). All data fetching, streaming, and live updates are declarative HTML attributes.

### Template engine: askama

Axum + askama for server-rendered HTML fragments:

```toml
[dependencies]
askama = "0.12"
askama_axum = "0.4"
```

### File layout

```
static/                          ← served by tower-http ServeDir
├── base.css                     ← DCS framework (from dcs.spa)
├── base.js                      ← DCS state management (from dcs.spa)
├── app.css                      ← OKLCH color schemes, app-specific
├── app.js                       ← ~50 lines DCS glue (nav, SSE session switch)
├── htmx.min.js                  ← 14KB, vendored
└── ext/
    ├── ws.js                    ← HTMX WebSocket extension
    └── sse.js                   ← HTMX SSE extension

templates/                       ← askama templates
├── index.html                   ← DCS shell (panels with hx-get, ws-connect, etc.)
├── panels/
│   ├── nav.html                 ← L1: navigation links
│   ├── conversations.html       ← L2: agent session list
│   ├── docs.html                ← L3: documentation
│   ├── mailboxes.html           ← L4: mail folders
│   ├── theme.html               ← R1: color scheme picker
│   ├── usage.html               ← R2: token usage stats
│   └── notifications.html       ← R3: notifications
├── pages/
│   ├── chat.html                ← main content: agent chat
│   ├── mail.html                ← main content: mail view
│   ├── text-chat.html           ← main content: team chat
│   └── settings.html            ← main content: user settings
└── fragments/
    ├── message.html             ← single chat message (for HTMX swap)
    ├── conversation-row.html    ← sidebar conversation entry
    └── email-row.html           ← single email list item
```

---

## 7. Axum as a Cosmix Port

The Axum web server is a first-class participant on the mesh:

```rust
// cosmix-web registers as a port
let port = cosmix_port::Port::new("cosmix-web")
    .command("status", "Web server status", |_| {
        PortResponse::success(json!({
            "uptime": ...,
            "active_sessions": ...,
            "connected_websockets": ...,
        }))
    })
    .command("broadcast", "Send message to all connected clients", |args| {
        // Push to all WebSocket connections
        ...
    })
    .standard_help()
    .standard_info("Cosmix Web", env!("CARGO_PKG_VERSION"));

port.start()?;
```

Lua scripts can control the web server:
```lua
local web = cosmix.port("cosmix-web")
web:call("broadcast", { message = "Server maintenance in 5 minutes" })
```

And the web server can call other ports:
```rust
// Axum handler that calls a cosmix port
async fn handle_mesh_command(
    State(state): State<AppState>,
    Json(req): Json<MeshCommandRequest>,
) -> impl IntoResponse {
    let result = cosmix_port::call_port(&req.port_socket, &req.command, req.args).await;
    Json(result)
}
```

---

## 8. Deployment

### Per-node services (systemd)

```
[cachyos — dev workstation]
  cosmix.service          ← daemon + mesh (existing)
  cosmix-web.service      ← Axum API + React static (NEW)
  stalwart.service        ← mail/calendar (existing)
  postgresql.service      ← database (existing)

[mko — production primary]
  cosmix.service          ← daemon + mesh
  cosmix-web.service      ← Axum API + React static
  stalwart.service        ← mail/calendar
  postgresql.service      ← database

[mmc — production secondary]
  (same as mko)
```

### TLS

- **External traffic:** Caddy (standalone, not FrankenPHP) or Axum with `axum-server` + `rustls`
- **Mesh traffic:** WireGuard (already encrypted)
- **Local traffic:** Plain HTTP on Unix socket or localhost

### Build + deploy

```bash
# Build release binary
cargo build --release --package cosmix-web

# Copy to node
scp target/release/cosmix-web mko:~/.local/bin/

# Restart service
ssh mko 'sudo systemctl restart cosmix-web'
```

Single binary. No `composer install`. No `npm ci`. No `php artisan migrate`. Just copy and restart.

---

## 9. WebRTC: Data Plane (Future)

WebRTC is the **data plane** complement to AMP's control plane, as defined in the AMP v0.4 spec (§8.2). It handles binary streams that don't belong in WebSocket text frames: audio, video, screen share, file transfer.

**Not needed for W1–W3.** All DCS panel content, chat messages, and mail proxying are text/HTML — WebSocket + SSE + HTMX cover everything.

**When it matters (W4+):**
- Voice/video calls between mesh nodes
- Screen sharing for remote assistance
- Large file transfer between peers
- Agent-driven media workflows (e.g., "show me your screen")

**Architecture:**
```
Browser                    cosmix-web (Axum)         cosmix-daemon (mesh)
   |                              |                        |
   |-- SDP offer (AMP) --------->|-- AMP to remote peer ->|
   |   (via WebSocket)           |   (via mesh WS)        |
   |                              |                        |
   |-- WebRTC data channel =====>|-- relay/SFU ----------->|
   |   (STUN via WireGuard)      |   (str0m)              |
```

**Rust crate:** `str0m` (sans-I/O WebRTC, no C++ deps, already proven in nodemesh's SFU prototype). Signalling flows through existing AMP/WebSocket paths — no new transport needed.

**Deferred to:** Phase W4 (Agent Runtime) or a dedicated W6 phase if media features are prioritized independently.

---

## 10. Known Corner Cases & Gotchas

### 10.1 Sidebar Data Loading (Replaces Inertia Shared Data)

With vanilla DCS, sidebar panels load via `GET /api/panel/{id}`. The data that Inertia previously injected on every page (conversations, stats, docs) becomes panel-specific endpoints. Panels load on first view and refresh via WebSocket push. No polling needed — the WS connection pushes `panel_update` messages when data changes.

### 10.2 WebSocket: Simple, But Needs Presence

Without Pusher/Echo, the WebSocket is a simple JSON message stream. However, presence tracking (who's online, typing indicators in text chat) still needs explicit server-side implementation — a `HashMap<UserId, Vec<WsSender>>` in Axum's shared state.

### 10.3 Password Hashing: bcrypt, NOT argon2

markweb uses **bcrypt** (BCRYPT_ROUNDS=12). All existing password hashes in the database are bcrypt. Axum must use the `bcrypt` crate to verify existing passwords. New passwords could migrate to argon2, but dual-verification is needed during transition.

### 10.4 Two-Factor Authentication

Fortify provides full 2FA: TOTP secret storage (encrypted), 8 recovery codes (one-time use), challenge flow (separate page after password). Must be explicitly implemented in Axum — the `totp-rs` crate handles TOTP, but recovery codes and the challenge flow need custom work.

### 10.5 Laravel Encrypted Attributes

Several models use `$casts = ['field' => 'encrypted']` (e.g., `jmap_token_encrypted`). This uses APP_KEY with AES-256-CBC + HMAC-SHA256 (Laravel's `Encrypter`). Options:
- Implement Laravel's exact encryption scheme in Rust (read `Illuminate\Encryption\Encrypter`)
- Re-encrypt all values during migration using a one-time script
- Store new values with Rust-native encryption, keep a Laravel-compatible decryptor for old values

### 10.6 Session Table Incompatibility

Laravel's `sessions` table stores `payload` as base64-encoded PHP-serialized data. `tower-sessions` uses a different format. **Users will need to re-login** after the switch. During Phase W1 (parallel running), sessions are independent — each server manages its own.

### 10.7 CalDAV/CardDAV: SabreDAV Custom Backends

markweb has custom SabreDAV backends (`CalendarBackend`, `CardDavBackend`, `PrincipalBackend`) with PostgreSQL storage in `dav_*` tables. Two options:
- **If Stalwart's CalDAV covers the same features:** proxy `/.well-known/{caldav,carddav}` to Stalwart, migrate data
- **If custom backends have unique logic:** keep a tiny PHP process just for DAV, or implement WebDAV in Rust (no mature crate exists)

**Verify Stalwart CalDAV coverage before committing to Phase W2.**

### 10.8 JMAP Client (No Rust Crate)

No Rust JMAP client library exists. The React frontend uses `jmap-jam` (browser-side). Server-side JMAP operations (agent email ingestion, `ProcessEmailMessage` job) need raw `reqwest` calls to Stalwart's JMAP endpoint. JMAP is JSON-over-HTTP, so this is feasible but tedious — each method call (Email/query, Email/get, Email/set) needs hand-coded request/response types.

### 10.9 Queue Drain on Cutover

Laravel queue jobs in Redis are PHP-serialized. Axum's tokio worker can't deserialize them. **Drain the Redis queue before switching.** Run `php artisan queue:work --stop-when-empty` before stopping Laravel.

### 10.10 Every-Second Heartbeat

`mesh:self-heartbeat` runs every second via Laravel's scheduler (writes Redis, broadcasts Echo, sends AMP to peers). In Axum: `tokio::spawn` with `tokio::time::interval(Duration::from_secs(1))`. Simple, but must be explicitly included.

### 10.11 Wayfinder Route Generation

markweb uses Wayfinder to auto-generate TypeScript route helpers from Laravel routes. After migration, these generated helpers become dead code. Replace with a simple route constants file or generate from Axum router.

### 10.12 Routine Event Dispatcher (Wildcard Listener)

`RoutineEventDispatcher` listens on ALL `App\*` events, queries `scheduled_actions` from DB, and dispatches matching routines. Has a re-entrancy guard (via static `$dispatching` flag). In Axum, this becomes a tokio broadcast channel subscriber with similar loop protection.

## 11. Risk Assessment

| Risk | Impact | Mitigation |
|---|---|---|
| bcrypt password verification | Low | Use bcrypt crate — well-tested, same algorithm |
| Laravel encrypted attributes | Medium | Port Laravel's Encrypter or re-encrypt in migration |
| SabreDAV replacement | High | Verify Stalwart CalDAV coverage first |
| No Rust JMAP client | Medium | Raw reqwest calls — tedious but doable |
| Session incompatibility | Low | Users re-login once; no data loss |
| Queue drain timing | Low | Run queue:work --stop-when-empty before cutover |
| Agent SSE streaming | Medium | Simpler than WebSocket; reqwest + Axum Sse response |
| 2FA implementation | Medium | Use totp-rs crate + custom recovery code logic |
| DCS feature parity | Low | base.css/js already proven; panel content is just HTML |

---

## 12. Revised Effort Estimates

| Phase | Sessions | Complexity | Notes |
|---|---|---|---|
| W1: Skeleton + Auth + DCS Shell | 2 | Medium | Axum + askama + base.css/js + auth + panel endpoints |
| W2: Mail + Calendar proxy | 1 | Low | JMAP pass-through; verify Stalwart CalDAV |
| W3: Text Chat + WebSocket | 1–2 | Medium | Simple WS, presence tracking, askama chat templates |
| W4: AI Agent Runtime | 3–4 | High | SSE streaming, tool execution, embeddings |
| W5: Remaining + Retire | 1 | Low | Queue drain, systemd cutover |
| **Total** | **8–10 sessions** | |

Dropping React eliminates the highest-risk items from the original plan (Inertia→SPA conversion, Pusher protocol, Echo client migration, Tailwind build pipeline). The vanilla DCS shell is the frontend — Axum just serves HTML fragments.

**Pre-flight tasks** (before Phase W1):
- [ ] Verify Stalwart CalDAV covers SabreDAV custom backend features
- [ ] Document Laravel Encrypter's exact AES-256-CBC scheme for Rust port
- [ ] Audit which `encrypted` model attributes exist and whether they need migration
- [ ] Copy base.css + base.js from dcs.spa into cosmix-web templates

---

## 13. What Dies, What Lives

### Dies
- PHP 8.4 runtime
- Laravel 12 framework
- FrankenPHP / Caddy (as PHP server)
- Composer + vendor/
- SabreDAV (CalDAV/CardDAV PHP)
- Laravel Reverb (WebSocket)
- Laravel Echo / Pusher protocol (JS WebSocket client)
- Inertia v2 (server-side rendering)
- React 19 + JSX compilation
- Tailwind CSS (replaced by DCS base.css OKLCH tokens)
- Zustand (replaced by base.js localStorage)
- Radix UI (replaced by native HTML + base.css)
- Bun / npm / node_modules — **no JavaScript build step at all**
- Wayfinder route generation

### Lives
- PostgreSQL (same database, same schema)
- DCS shell (base.css + base.js — vanilla HTML/CSS/JS, ~950 lines)
- Stalwart mail server
- WireGuard mesh
- systemd
- TLS certificates (same domains)
- All user data (zero data migration)
- OKLCH color schemes (carried from markweb.css into DCS base.css)

---

*Plan authored 2026-03-09*
*Status: Planning — awaiting Phase 7 mesh integration completion*
