# Cosmix: Implementation Plan
*Sovereign personal intelligence stack — mesh node + mail + AI inference*

---

## Vision Summary

Cosmix is a **self-hosted sovereign intelligence stack** built entirely in Rust, targeting the COSMIC desktop on Linux and distributable cross-platform via Dioxus. The system combines:

- **cosmix-jmap** — a native JMAP server (RFC 8620/8621) extended with CalDAV/CardDAV (*dav), replacing the need for Stalwart or any third-party mail server
- **cosmix-mail** — a cross-platform dioxus-desktop mail/calendar/contacts client and AI chat interface
- **cosmix-mesh** — the AMP (AppMesh Protocol) WireGuard mesh that ties nodes together
- **AI inference pipeline** — pgvector semantic search → local Ollama LLM → frontier model fallback, all triggered by and responding via email/JMAP
- **Webmail** — the same dioxus codebase compiled to WASM for browser access

The key architectural insight: **the intelligence lives in cosmix-jmap, not the client**. Any JMAP-capable client (Thunderbird, Apple Mail, browser, cosmix-mail) gets the AI capabilities transparently via email — send a message, get an AI reply. cosmix-mail just gets a richer native interface to those same capabilities.

The SMTP bypass is a long-term goal: cosmix-jmap nodes on the same mesh can exchange messages directly without touching port 25, while still federating with the outside world via standard SMTP.

---

## What Is Already in Place

Based on existing Cosmix work:

- [x] **AppMesh (AMP)** — ARexx-inspired IPC/orchestration with Lua scripting layer
- [x] **nodemesh** — Rust WebSocket mesh daemon
- [x] **markweb** — Markdown-first Rust web server
- [x] **WireGuard mesh** — multi-node networking between Proxmox/BinaryLane VPS nodes
- [x] **pgvector schema** — PostgreSQL vector store for semantic memory
- [x] **Ollama integration** — local LLM inference (CPU, Qwen/mistral Q4_K_M quants)
- [x] **CachyOS + COSMIC 1.0** — daily driver with cosmic-comp compositor
- [x] **NetServa** — hosting management platform (~1000 customers, Spiderweb/RentaNet)
- [x] **COSMIX.md** — persistent Claude Code context document
- [x] **cosmix-setup-prompt.md** — nine-phase Claude Code setup prompt

## What Needs to Be Built

- [ ] **cosmix-jmap** — JMAP core server (RFC 8620 + RFC 8621)
- [ ] **cosmix-jmap *dav** — CalDAV + CardDAV extensions
- [ ] **cosmix-jmap AI bridge** — inference pipeline triggered by inbound mail
- [ ] **cosmix-mail** — dioxus-desktop client (Linux/Win/Mac)
- [ ] **cosmix-mail webmail** — dioxus-web WASM build for browser access
- [ ] **cosmix-jmap SMTP bypass** — direct mesh-to-mesh delivery (long-term)

---

## Environment & Toolchain

```
OS:         CachyOS with COSMIC 1.0
Compositor: cosmic-comp (floating windows — essential for dioxus dev/test)
Shell:      COSMIC session (cosmic-comp + cosmic-settings-daemon + cosmic-bg
            + waybar + fuzzel as panel replacement if running minimal)
Working:    ~/.ns/
AUR:        paru
Rust:       stable via rustup
LLM local:  Ollama on Intel Core Ultra 5 125H, CPU-only, Q4_K_M quants
DB:         PostgreSQL + pgvector
Dioxus:     0.7.x stable, dioxus-desktop initially, dioxus-native when Blitz matures
```

**Critical note on compositor:** Development and testing of dioxus-desktop apps
REQUIRES cosmic-comp or another floating/stacking compositor. Niri's tiling model
is incompatible with testing window resize behaviour, multi-window layout, and
the floating window metaphor that Windows/macOS users will experience.

---

## Phase 1: Desktop Environment — Get Off Niri

**Goal:** Working cosmic-comp session with floating windows for dioxus development.

### Steps

1. **Install minimum cosmic-* suite on CachyOS**
   ```bash
   paru -S cosmic-comp cosmic-session cosmic-settings-daemon \
            cosmic-bg xdg-desktop-portal-cosmic
   ```

2. **Install shell replacements** (no cosmic-panel needed)
   ```bash
   paru -S waybar fuzzel mako swaylock swayidle
   ```

3. **Configure greetd or use existing display manager** to offer a
   `cosmic-comp` session entry alongside the full COSMIC session

4. **Port niri config** — keybindings, monitor layout, environment variables —
   to cosmic-comp RON config files in `~/.config/cosmic/`

5. **Verify floating window behaviour** — open two terminal windows, resize
   freely, drag, snap to edges. Confirm it mirrors the Windows/macOS experience.

6. **Set COSMIC tiling to OFF** on the primary workspace used for dioxus dev.
   Keep tiling ON on secondary workspaces for terminal/editor work if preferred.

---

## Phase 2: Dioxus Hello World — Validate the Toolchain

**Goal:** A working dioxus-desktop app on CachyOS before writing any real code.

### Steps

1. **Install system dependencies**
   ```bash
   paru -S webkit2gtk-4.1 libxdo pkg-config lld
   ```

2. **Install dioxus CLI**
   ```bash
   cargo binstall dioxus-cli
   ```

3. **Verify environment**
   ```bash
   dx doctor
   ```

4. **Create skeleton app**
   ```bash
   cd ~/.ns
   dx new cosmix-mail
   cd cosmix-mail
   ```

5. **Apply the Linux WebKit black screen fix** — in `src/main.rs`:
   ```rust
   fn main() {
       std::env::set_var("WEBKIT_DISABLE_COMPOSITING_MODE", "1");
       dioxus::launch(App);
   }
   ```

6. **Run and verify**
   ```bash
   dx serve
   ```
   Confirm a floating, resizable window appears with decorations.
   Confirm hot-reload works on RSX changes.

7. **Test hot-patch** (Rust code changes without restart):
   ```bash
   dx serve --hotpatch
   ```

8. **Test web target** — same code in browser:
   ```bash
   dx serve --platform web
   ```

---

## Phase 3: cosmix-jmap Server — Core JMAP Implementation

**Goal:** A working JMAP Core (RFC 8620) + JMAP Mail (RFC 8621) server in Rust,
testable against a real client. Must be compatible with Stalwart for
cross-validation of spec compliance.

### Architecture

```
cosmix-jmap/
├── src/
│   ├── main.rs              # axum entry point
│   ├── session.rs           # JMAP Session resource (/.well-known/jmap)
│   ├── core/                # RFC 8620 — JMAP Core
│   │   ├── request.rs       # Request/Response objects, method dispatch
│   │   ├── push.rs          # EventSource + WebSocket push
│   │   └── blob.rs          # Blob upload/download
│   ├── mail/                # RFC 8621 — JMAP Mail
│   │   ├── mailbox.rs       # Mailbox/get, /set, /changes, /query
│   │   ├── email.rs         # Email/get, /set, /changes, /query, /parse
│   │   ├── thread.rs        # Thread/get, /changes
│   │   ├── submission.rs    # EmailSubmission/get, /set (outbound SMTP)
│   │   └── vacation.rs      # VacationResponse/get, /set
│   ├── dav/                 # *DAV extensions (Phase 4)
│   │   ├── caldav.rs        # CalDAV — calendar events
│   │   └── carddav.rs       # CardDAV — contacts/vCards
│   ├── inference/           # AI pipeline (Phase 5)
│   │   ├── pipeline.rs      # pgvector → Ollama → frontier cascade
│   │   └── router.rs        # Which addresses trigger inference
│   ├── smtp/                # SMTP ingress/egress
│   │   ├── ingress.rs       # Receive inbound mail (via SMTP or mesh)
│   │   └── egress.rs        # Send outbound mail (SMTP submission)
│   ├── storage/             # Persistence layer
│   │   ├── postgres.rs      # PostgreSQL via sqlx
│   │   └── schema.sql       # Tables: mailboxes, emails, blobs, state_changes
│   └── auth.rs              # Authentication (Bearer token, Basic)
├── Cargo.toml
└── config.toml              # Server config (ports, DB URL, SMTP relay, etc)
```

### Key Crates

```toml
[dependencies]
axum = { version = "0.8", features = ["ws"] }    # HTTP + WebSocket
sqlx = { version = "0.8", features = ["postgres", "runtime-tokio"] }
pgvector = "0.4"                                  # Vector similarity search
serde = { version = "1", features = ["derive"] }
serde_json = "1"
tokio = { version = "1", features = ["full"] }
uuid = { version = "1", features = ["v4"] }
lettre = "0.11"                                   # SMTP egress
rustls = "0.23"                                   # TLS
tower = "0.5"
tower-http = { version = "0.6", features = ["cors", "auth"] }
```

### Implementation Steps

1. **Scaffold axum server** with JMAP session endpoint (`/.well-known/jmap`)
   returning capability advertisement

2. **Implement JMAP Core request/response** — the method dispatch table,
   `using` capability checking, `methodCalls` array processing, `created`/
   `updated`/`destroyed` response objects

3. **PostgreSQL schema** — design tables for:
   - `accounts` — user accounts
   - `mailboxes` — JMAP Mailbox objects
   - `emails` — Email objects with full RFC 5322 storage
   - `blobs` — Binary large objects (attachments)
   - `state` — per-object-type state strings for change tracking
   - `threads` — Thread grouping

4. **Implement Mailbox methods** — `Mailbox/get`, `Mailbox/set`,
   `Mailbox/changes`, `Mailbox/query`

5. **Implement Email methods** — `Email/get`, `Email/set`, `Email/query`,
   `Email/changes`, `Email/import`, `Email/parse`

6. **Implement Thread methods** — `Thread/get`, `Thread/changes`

7. **SMTP ingress** — accept inbound email on port 25 (or 587 for submission),
   parse RFC 5322, store as JMAP Email objects, trigger state change events

8. **SMTP egress** — `EmailSubmission/set` triggers outbound delivery via
   lettre to configured relay or direct MX lookup

9. **Push notifications** — EventSource (`text/event-stream`) for long-poll
   clients, WebSocket upgrade for persistent connections

10. **Cross-validate against Stalwart** — run cosmix-mail against a local
    Stalwart instance to confirm client-side JMAP is spec-compliant.
    Run Thunderbird (or JMAP test suite) against cosmix-jmap to confirm
    server-side compliance.

---

## Phase 4: *DAV Extensions

**Goal:** CalDAV (RFC 4791) and CardDAV (RFC 6352) on the same axum server
as cosmix-jmap, sharing the same auth and storage layer.

### Architecture Notes

CalDAV and CardDAV are WebDAV (RFC 4918) subsets — HTTP with additional
verbs (`PROPFIND`, `REPORT`, `MKCALENDAR`). axum handles these cleanly
via custom method extractors.

```
/dav/
├── calendars/{account}/     # CalDAV principal
│   ├── {calendar-id}/       # Calendar collection
│   │   └── {event-id}.ics   # iCalendar objects (RFC 5545)
└── contacts/{account}/      # CardDAV principal
    ├── {addressbook-id}/    # Addressbook collection
    │   └── {contact-id}.vcf # vCard objects (RFC 6350)
```

### Key Crates

```toml
ical = "0.10"      # iCalendar parsing/generation
vcard = "0.5"      # vCard parsing/generation (or hand-roll — small spec)
```

### Implementation Steps

1. **WebDAV base layer** — `PROPFIND`, `PROPPATCH`, `MKCOL`, `REPORT`
   HTTP verb handling in axum via `axum::extract::Method`

2. **CalDAV** — calendar collection discovery, `MKCALENDAR`, iCal
   object CRUD, `calendar-query` REPORT for date-range queries

3. **CardDAV** — addressbook collection discovery, vCard CRUD,
   `addressbook-query` REPORT for contact search

4. **pgvector integration** — embed contact names/bios and calendar
   event descriptions into the vector store so the AI pipeline can
   retrieve them semantically (e.g. "what do I know about this person")

5. **Sync tokens** — implement `sync-collection` REPORT for efficient
   client sync (mirrors JMAP's state change tracking)

6. **Test against Thunderbird + iOS** — CalDAV/CardDAV are well-tested
   by these clients; use them to validate compliance before building
   the cosmix-mail *dav UI

---

## Phase 5: AI Inference Pipeline

**Goal:** Inbound email to a designated address (e.g. `ai@node`,
`ask@node`) triggers the inference cascade and replies automatically.

### Pipeline Architecture

```
Inbound email arrives (SMTP or mesh-direct)
          │
          ▼
   cosmix-jmap ingress
          │
          ├─ Is destination address in inference_routes config?
          │  NO  → normal delivery to mailbox
          │  YES ↓
          │
   Extract query from email body (strip quoted text, signatures)
          │
          ▼
   1. pgvector semantic search
      SELECT content, metadata
      FROM embeddings
      ORDER BY embedding <=> query_embedding
      LIMIT 10
          │
          ├─ Sufficient context? (similarity threshold)
          │  YES → compose reply from vector results alone
          │  NO  ↓
          │
   2. Ollama local LLM
      POST /api/chat
      model: qwen2.5 (or configured model)
      messages: [system_prompt, context_from_pgvector, user_query]
          │
          ├─ Satisfactory? (confidence heuristic or explicit flag)
          │  YES → compose reply
          │  NO  ↓
          │
   3. Frontier model (Claude/GPT via API)
      Only if local inference insufficient
      Log that external call was made
          │
          ▼
   Compose RFC 5322 reply
   Store in sender's mailbox (JMAP Email/import)
   Deliver via EmailSubmission (triggers push to cosmix-mail)
```

### Configuration (config.toml)

```toml
[inference]
enabled = true
routes = ["ai@", "ask@", "help@"]   # Address prefixes that trigger inference
model = "qwen2.5:14b"               # Ollama model
similarity_threshold = 0.75         # pgvector cosine similarity cutoff
frontier_model = "claude-sonnet-4"  # Fallback if local insufficient
frontier_api_key_env = "ANTHROPIC_API_KEY"

[embeddings]
model = "nomic-embed-text"          # Ollama embedding model
dimensions = 768
```

### Implementation Steps

1. **Embedding pipeline** — on mail ingest, generate embeddings for
   body text via Ollama (`nomic-embed-text`) and store in pgvector.
   Do the same for calendar events and contacts from *dav.

2. **Query embedding** — when inference is triggered, embed the
   inbound query and run cosine similarity search against pgvector

3. **Ollama client** — simple `reqwest` POST to `/api/chat` with
   streaming response support

4. **Frontier fallback** — `reqwest` to Anthropic/OpenAI API,
   key sourced from environment (never hardcoded)

5. **Reply composition** — assemble RFC 5322 reply with proper
   `In-Reply-To`, `References` headers so it threads correctly
   in any mail client

6. **JMAP custom capability** — advertise `cosmix:inference` in
   the JMAP session capabilities object so cosmix-mail knows
   to offer richer UI (streaming display, model selection)

---

## Phase 6: cosmix-mail — Dioxus Desktop Client

**Goal:** Cross-platform mail/calendar/contacts/AI client in a single
dioxus codebase. Ships as native desktop (Linux/Win/Mac) and browser
WASM from the same source tree.

### Project Structure

```
cosmix-mail/
├── Cargo.toml
├── Dioxus.toml              # dx build config — desktop + web targets
├── src/
│   ├── main.rs              # Entry point + WEBKIT_DISABLE_COMPOSITING_MODE
│   ├── app.rs               # Root component + router
│   ├── jmap/                # JMAP client library
│   │   ├── client.rs        # HTTP + WebSocket session management
│   │   ├── auth.rs          # Bearer token auth, session bootstrap
│   │   ├── mailbox.rs       # Mailbox/get, /query methods
│   │   ├── email.rs         # Email/get, /query, /set methods
│   │   └── push.rs          # EventSource subscription for push
│   ├── dav/                 # *DAV client
│   │   ├── caldav.rs        # Calendar fetch/sync
│   │   └── carddav.rs       # Contacts fetch/sync
│   ├── views/               # UI components
│   │   ├── sidebar.rs       # Mailbox tree + nav
│   │   ├── message_list.rs  # Email list panel
│   │   ├── message_view.rs  # Reading pane (renders HTML email)
│   │   ├── compose.rs       # Compose window
│   │   ├── calendar.rs      # Calendar view
│   │   ├── contacts.rs      # Contacts view
│   │   └── ai_chat.rs       # cosmix:inference chat interface
│   ├── state/               # Global app state (signals)
│   │   ├── session.rs       # JMAP session + account state
│   │   ├── mailboxes.rs     # Mailbox cache
│   │   └── emails.rs        # Email cache + pagination
│   └── config.rs            # Server URL, credentials, preferences
├── assets/
│   ├── main.css             # Tailwind-based styles
│   └── icons/               # SVG icons
└── index.html               # Web target template
```

### Cargo.toml Features

```toml
[features]
default = ["desktop"]
desktop = ["dioxus/desktop"]
web = ["dioxus/web"]
native = ["dioxus/native"]   # Future: swap to Blitz when ready

[dependencies]
dioxus = { version = "0.7", features = [] }  # feature set by above
reqwest = { version = "0.12", features = ["json", "stream"] }
serde = { version = "1", features = ["derive"] }
serde_json = "1"
tokio = { version = "1", features = ["full"] }
```

### UI Layout (Three-Pane Classic)

```
┌─────────────────────────────────────────────────────────┐
│  [≡] cosmix-mail          [compose] [search]       [⚙]  │
├──────────┬──────────────────────┬───────────────────────┤
│          │                      │                       │
│ Inbox    │ From: alice@example  │ Subject: Re: Invoice  │
│ Sent     │ Subject: Re: Invoice │                       │
│ Drafts   │ 10:42am    ★        │ Hi Mark,              │
│ Archive  │──────────────────────│                       │
│ Spam     │ From: bob@example    │ Thanks for the ...    │
│          │ Subject: Meeting     │                       │
│ ──────   │ Yesterday            │                       │
│ Calendar │──────────────────────│                       │
│ Contacts │ From: carol@example  │ [Reply] [Forward]     │
│ AI Chat  │ Subject: Update      │ [Archive] [Delete]    │
│          │ Monday               │                       │
└──────────┴──────────────────────┴───────────────────────┘
```

### Implementation Steps

1. **JMAP session bootstrap** — fetch `/.well-known/jmap`, parse
   capabilities (including `cosmix:inference` if present),
   store session in global signal state

2. **Mailbox tree** — `Mailbox/get` all mailboxes, render as
   sidebar tree with unread counts

3. **Email list** — `Email/query` with `filter` + `sort` +
   `position`/`limit` for pagination; display sender, subject,
   date, preview snippet

4. **Reading pane** — `Email/get` with `bodyValues` property,
   render HTML body safely inside a dioxus `iframe`-equivalent
   or sanitised HTML component. Handle `text/plain` fallback.

5. **Push updates** — subscribe to EventSource; on `state` change
   event for `Email` type, re-query changed emails only

6. **Compose** — multi-window compose using `dioxus-desktop`
   window spawning; `Email/set` + `EmailSubmission/set` for send

7. **CalDAV view** — fetch calendar events for visible date range,
   month/week/day toggle, event create/edit/delete

8. **CardDAV view** — contact list + search, vCard display,
   click-to-compose from contact

9. **AI Chat view** — only shown if server advertises
   `cosmix:inference` capability. Renders the inference thread
   as a chat timeline. Streams token output if server supports
   `cosmix:inference:stream` extension.

10. **Web target** — `dx build --platform web` produces WASM
    bundle served by cosmix-jmap's axum HTTP layer. Same
    components, same state, WebSocket instead of EventSource
    for push. No code changes required — just the feature flag.

---

## Phase 7: SMTP Bypass (Long-Term, Mesh-Internal)

**Goal:** cosmix-jmap nodes on the same WireGuard mesh can exchange
messages directly without SMTP, while transparently appearing as
normal email to the end user.

### Architecture

```
Node A cosmix-jmap
    EmailSubmission/set → destination MX lookup
         │
         ├─ Is destination on same mesh? (DNS TXT record or AMP lookup)
         │  NO  → standard SMTP delivery (lettre → port 25/587)
         │  YES ↓
         │
    Direct JMAP push to Node B
    POST https://nodeb.mesh/jmap
    Authorization: mesh-token (WireGuard-authenticated)
    Body: JMAP Email/import method call
         │
         ▼
    Node B stores email directly in recipient mailbox
    Node B sends push event to cosmix-mail clients
    (No SMTP, no port 25, no MX record needed for mesh traffic)
```

### Discovery Mechanism

Add a DNS TXT record or AMP peer advertisement:
```
_cosmix._jmap.yourdomain.com TXT "v=cosmix1 url=https://node.yourdomain.com/jmap"
```

cosmix-jmap checks this before falling back to MX/SMTP delivery.

---

## Build Order Summary

```
Phase 1: ████████░░░░░░░░░░░░  Desktop env (cosmic-comp + waybar)
Phase 2: ░░░████████░░░░░░░░░  Dioxus hello world + toolchain validation
Phase 3: ░░░░░░████████████░░  cosmix-jmap JMAP core server
Phase 4: ░░░░░░░░░░░████████░  *DAV extensions (CalDAV + CardDAV)
Phase 5: ░░░░░░░░░░░░░░████░░  AI inference pipeline
Phase 6: ░░░░░░░░████████████  cosmix-mail client (parallel with Phase 3+)
Phase 7: ░░░░░░░░░░░░░░░░░███  SMTP mesh bypass (long-term)
```

Phases 3 and 6 can proceed in parallel once Phase 2 is validated —
build a thin JMAP stub server early so the client has something to
connect to, then deepen both together.

---

## Key Decisions Locked In

| Decision | Rationale |
|---|---|
| cosmix-jmap, not Stalwart | Own the server stack; no external dependency |
| Stalwart as test target | Validate JMAP spec compliance from client side |
| dioxus-desktop now | Floating windows required for dev/test; cross-platform identical |
| dioxus-native later | Wait for Blitz `position:fixed` + form inputs (~6 months) |
| cosmic-comp, not niri | Floating window metaphor matches Win/Mac; needed for dioxus dev |
| AI in server, not client | Any JMAP client (Thunderbird etc) gets AI transparently via email |
| pgvector first, Ollama second | Privacy gate — most queries never leave the node |
| *dav on same axum server | Shared auth, shared storage, single binary, single port |
| JMAP push via EventSource/WS | No polling; real-time updates to all connected clients |
| `cosmix:inference` capability | Advertised in JMAP session; cosmix-mail enables richer UI if present |

---

## Claude Code Context

When starting a Claude Code session on any Cosmix component, load this
file alongside `COSMIX.md`. Key reminders for Claude Code:

- Working directory: `~/.ns/`
- All Rust — no Python, no Node, no JavaScript except as dioxus build output
- axum for all HTTP — not actix, not warp
- sqlx for database — async, compile-time checked queries
- tokio for async runtime throughout
- `paru` for AUR packages on CachyOS
- pgvector extension must be enabled: `CREATE EXTENSION IF NOT EXISTS vector;`
- Ollama API: `POST http://localhost:11434/api/chat` (local, no auth)
- COSMIC desktop — libcosmic/iced for any native COSMIC-specific UI work
- Dioxus 0.7.x stable — not 0.6, not main branch
- File outputs always as `.md` — never `.docx` unless explicitly requested
