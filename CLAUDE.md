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
| `cosmix-lib-script` | `cosmix_script` | Script discovery, TOML defs, Mix runtime bridge, User menu |
| `cosmix-lib-ui` | `cosmix_ui` | Shared Dioxus components, theme, icons |

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
| `cosmix-configd` | Config watcher, serves settings via AMP |
| `cosmix-hubd` | WebSocket message broker |
| `cosmix-indexd` | Semantic indexing + vector storage (candle + sqlite-vec) |
| `cosmix-maild` | JMAP (RFC 8620/8621) + SMTP mail server |
| `cosmix-mond` | System monitor daemon |
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

## Gotchas

- Linux WebKit black screen: cosmix-mail sets `WEBKIT_DISABLE_COMPOSITING_MODE=1` before Dioxus launch
- `spamlite` is a sibling path dep (`../spamlite`), not on crates.io
- Spam databases are per-account SQLite files, not shared — prevents cross-user model contamination
- Thread formation (Message-ID + In-Reply-To matching) is not yet implemented
- The `src/_doc/` directory contains 30+ design documents — check these before guessing at architectural intent
