# CLAUDE.md

This file provides guidance to Claude Code (claude.ai/code) when working with code in this repository.

## Project

Cosmix is a self-hosted sovereign intelligence stack: JMAP mail server + Dioxus cross-platform client + AMP mesh networking + AI inference pipeline. All Rust, no Python/Node/JavaScript except as Dioxus build output.

**Current phase:** Phase 3 (cosmix-maild core server). Phases 4–7 build on top: CalDAV/CardDAV, AI inference, Dioxus client, mesh SMTP bypass.

## Workspace

Monorepo with Cargo workspace under `src/`. Top level is clean (docs + config only). Rust 2024 edition. Naming convention distinguishes crate types:

- `cosmix-lib-*` — libraries (Rust module name: `cosmix_*`)
- `cosmix-*` — GUI apps (Dioxus desktop/web)
- `cosmix-*d` — headless daemons/services

### Libraries

| Crate | Rust module | Purpose |
|-------|------------|---------|
| `cosmix-lib-amp` | `cosmix_amp` | AMP wire format + IPC |
| `cosmix-lib-client` | `cosmix_client` | AMP WebSocket client (native + WASM) |
| `cosmix-lib-config` | `cosmix_config` | Typed config structs + TOML load/save |
| `cosmix-lib-mesh` | `cosmix_mesh` | WireGuard mesh networking, WebSocket peer sync |
| `cosmix-lib-script` | `cosmix_script` | Mix script discovery, runtime bridge, cosmix prelude, User menu |
| `cosmix-lib-ui` | `cosmix_ui` | **THE shared UI library** — theme, MenuBar, dx-components, icons, app shell. Priority: centralize ALL shared UI here. |

### GUI Apps

| Crate | Purpose |
|-------|---------|
| `cosmix-backup` | Proxmox Backup Server dashboard |
| `cosmix-dialog` | Transient dialog utility (zenity replacement) |
| `cosmix-dns` | DNS zone management UI |
| `cosmix-edit` | Text editor |
| `cosmix-files` | File browser |
| `cosmix-mail` | JMAP mail client (Dioxus desktop/web) |
| `cosmix-menu` | System tray app launcher |
| `cosmix-mon` | System monitor GUI (desktop + WASM) |
| `cosmix-scripts` | Mix + Bash script manager |
| `cosmix-settings` | Settings/preferences editor |
| `cosmix-shell` | DCS shell — primary UI surface (desktop + WASM) |
| `cosmix-view` | Markdown/image viewer |
| `cosmix-wg` | WireGuard mesh admin |

### Daemons

| Crate | Purpose |
|-------|---------|
| `cosmix-noded` | **Consolidated node daemon** — hub + config + monitor + logger in one binary |
| `cosmix-claude` | Claude Code agent daemon |
| `cosmix-indexd` | Semantic indexing + vector storage (candle + sqlite-vec) |
| `cosmix-maild` | JMAP (RFC 8620/8621) + SMTP mail server |
| `cosmix-mcp` | Model Context Protocol bridge for Claude Code |
| `cosmix-webd` | WASM app server + CMS API |


External path dependencies:
- `spamlite` at `~/.gh/spamlite` (Bayesian spam classifier)
- `mix-core` at `~/.mix/src/crates/mix-core` (Mix scripting language core)

## Build Commands

```bash
cd src                                            # Cargo workspace root
cargo build                                       # entire workspace
cargo build -p cosmix-maild                        # single crate
cargo build -p cosmix-maild --release              # release build
cargo check                                        # type-check without codegen
```

Dioxus client (requires `dx` CLI — `cargo binstall dioxus-cli`):
```bash
cd src/crates/cosmix-mail && dx serve                  # desktop hot-reload
cd src/crates/cosmix-mail && dx serve --platform web    # browser WASM
cd src/crates/cosmix-mail && dx serve --hotpatch        # Rust hot-patch
```

No test suite yet. Manual validation via curl (JMAP) and nc/swaks (SMTP).

## cosmix-maild CLI

```bash
cosmix-maild migrate                    # apply SQL migrations
cosmix-maild account add <email> <pwd>  # create account (auto-creates default mailboxes + PIM)
cosmix-maild account list
cosmix-maild account delete <email>
cosmix-maild queue list                 # SMTP outbound queue
cosmix-maild queue flush                # retry queued messages
cosmix-maild serve                      # start JMAP HTTP + SMTP listeners
```

## Configuration

Config loaded from `~/.config/cosmix/jmap.toml` or `/etc/cosmix/jmap.toml`. Key fields:

- `database_url` — PostgreSQL connection (requires pgvector extension for future AI phase)
- `blob_dir` — on-disk blob storage path
- `smtp.listen_inbound` — SMTP port (default 2525, production 25)
- `tls.cert` / `tls.key` — rustls TLS
- `dkim.selector` / `dkim.private_key` — outbound DKIM signing
- `spam.enabled` / `spam.db_dir` — per-account spamlite SQLite databases

## Database Architecture

PostgreSQL with sqlx (async, compile-time checked queries). Four migrations in `src/crates/cosmix-maild/migrations/`:

**State tracking:** All mutations write to `changelog(account_id, object_type, object_id, change_type)`. JMAP state = max changelog ID per (account_id, object_type). This powers `/changes` and `/query` efficiently.

**UUID primary keys** on mailboxes, threads, blobs, calendars, events, contacts. Account IDs are SERIAL INT.

**JSONB for flexibility:** email addresses, keywords, calendar events (JSCalendar RFC 8984), contacts (JSContact RFC 9553) stored as JSONB.

**Blob storage:** BLAKE3 hashed, size tracked, stored on disk at `{blob_dir}/{id}`.

**Account creation** triggers `create_default_mailboxes()` (Inbox/Drafts/Sent/Trash/Junk/Archive) and `create_default_pim()` (Personal calendar + Contacts addressbook).

## JMAP Server Architecture

**Endpoints:**
- `GET /.well-known/jmap` — session resource (capabilities, account URLs)
- `POST /jmap` — method dispatch over `methodCalls[]` array
- `GET/POST /jmap/blob/{blobId}` — blob download/upload

**Method dispatch** (`src/jmap/mod.rs`): decodes `JmapRequest`, iterates `methodCalls`, routes to `mailbox.rs`, `email.rs`, `calendar.rs`, `contact.rs`, `submission.rs`, aggregates into `JmapResponse`.

**SMTP inbound** (`src/smtp/`): accepts MAIL FROM/RCPT TO/DATA, parses RFC 5322 via mail-parser, stores blob + Email row, classifies spam via spamlite, records changelog entry.

**SMTP outbound:** EmailSubmission/set → mail-builder constructs MIME → mail-send delivers. Failed deliveries go to `smtp_queue` with exponential backoff (max 10 attempts).

## AMP Wire Format (cosmix-lib-amp)

All cosmix IPC uses AMP (AppMesh Protocol) — markdown frontmatter with BTreeMap headers + optional body:

```
---
command: mailbox.list
rc: 0
---
[{"id": "...", "name": "Inbox"}]
```

RC codes: 0=success, 5=warning, 10=error, 20=failure. Used across Unix sockets (local), WebSockets (mesh), and log files.

## Key Decisions

- **axum** for all HTTP — not actix, not warp
- **sqlx** for database — not sea-orm, not diesel
- **tokio** async runtime throughout
- **Dioxus 0.7** for client — not libcosmic (old stack was libcosmic, pivoted to Dioxus)
- **AI lives in the server** — any JMAP client gets AI via email; cosmix-mail just gets richer UI
- **No Docker** — Incus or Proxmox containers only
- **Mix** for scripting — pure-Rust language at `~/.mix/`, replaces Lua. Native AMP IPC via `send`/`address`/`emit` keywords. `mix-core` embedded in `cosmix-lib-script` and `cosmix-scripts` as path dep.
- **mimalloc** allocator in all binaries
- **`paru`** for AUR packages on CachyOS/Arch

## cosmix-lib-ui — The Shared UI Foundation (PRIORITY)

cosmix-lib-ui is the most important crate for GUI consistency. **Centralize everything shared here.** Every reusable component, theme logic, menu system, and layout pattern must live in this library — never duplicated across apps.

### What it provides

- **Theme system** (`theme.rs`) — OKLCH `generate_css()` outputs all CSS custom properties including the dx-components bridge vars. `use_theme_css()` hook reactively injects theme CSS. Light/dark mode toggle works instantly because bridge vars use concrete OKLCH values, not CSS variable chains.
- **MenuBar** (`menu/`) — Standard app menu bar with File/Help menus, keyboard shortcuts, AMP integration, caption buttons. Used by every app.
- **dx-components** (`dx_components/`) — 40 official DioxusLabs styled components (Button, Card, Dialog, Sidebar, Tabs, etc.) imported via `dx components add` then moved here for shared access. All apps import from `cosmix_ui::dx_components::*`.
- **Primitives** (`primitives.rs`) — Re-exports from `dioxus-primitives` (34 unstyled accessible behavioural components).
- **Icons** (`icons.rs`) — `lucide-dioxus` re-exports (1000+ icons) via `cosmix_ui::icons::lucide::*`.
- **Components** (`components/`) — Custom cosmix widgets: DataTable, StatusBar, ErrorBanner, AMP UI registry.
- **App init** (`app_init.rs`) — `launch_desktop()`, hub connection, theme polling, command dispatch.

### Standard app shell pattern

Every cosmix app MUST follow this layout:

```rust
fn app() -> Element {
    use_theme_css();  // Reactive theme injection
    rsx! {
        div { class: "flex flex-col w-full h-full",
            MenuBar { menu: app_menu, on_action: handler }  // Always on top
            div { class: "flex-1 overflow-y-auto",           // Content fills below
                // App content here (may include Sidebar, Tabs, etc.)
            }
        }
    }
}
```

### Preview app

`cosmix-preview` is the test bed for all components. Launch with `cd src/crates/cosmix-preview && dx serve`. Demonstrates all 40 dx-components + custom components using Tailwind v4 classes. Use it to validate any UI changes before propagating to other apps.

## WebKitGTK UI Rules (CRITICAL — read before any UI work)

Dioxus desktop uses WebKitGTK on Linux. These rules prevent layout shifts and rendering bugs:

- **Borders: use `rgba()` concrete values, NEVER CSS variables** — `var(--border)` can resolve to transparent, collapsing borders to 0px and shifting layout. Use `rgba(128,128,128,0.4)` for borders that must survive state changes.
- **State changes: only change `background` and `color`** — never change border, padding, margin, or font properties on hover/selected/active states. Any property change can trigger WebKitGTK reflow.
- **Buttons: use `div` + `onclick`** for app chrome — bypasses all WebKitGTK native form control styling. The global `all: unset` in theme.rs helps but `div` is safest.
- **No native HTML5 drag** — crashes Wayland compositor. Use mousedown/mousemove/mouseup for custom drag.
- **Font weight: specify lighter than intended** — WebKitGTK renders ~100 weight units heavier (300 renders as 400).
- **Prefer `rem` over `px`** — pixel units render differently in WebKitGTK vs Chromium.
- **No `backdrop-filter`** — not implemented in WebKitGTK.
- **Global reset already in theme.rs** — `all: unset` on form elements, `box-sizing: border-box` on everything.
- **UI foundation plan:** see `src/_doc/2026-03-31-dioxus-ui-foundation.md` — Tailwind v4 + dioxus-primitives + custom widgets.

## Gotchas

- Linux WebKit black screen: cosmix-mail sets `WEBKIT_DISABLE_COMPOSITING_MODE=1` before Dioxus launch
- `spamlite` is a path dep at `~/.gh/spamlite`, not on crates.io
- Spam databases are per-account SQLite files, not shared — prevents cross-user model contamination
- Thread formation (Message-ID + In-Reply-To matching) is not yet implemented
- The `src/_doc/` directory contains 30+ design documents — check these before guessing at architectural intent
