# The Pure Rust Stack: AmigaOS for the Modern Distributed Desktop

> A unified architecture where Rust powers everything — desktop apps, web services,
> mail infrastructure, mesh networking, and inter-process scripting — with only a
> thin React layer for browser UI.

## The Insight

The pieces already exist. They just haven't been assembled together before:

| Layer | Component | Language | Status |
|-------|-----------|----------|--------|
| Desktop toolkit | libcosmic / iced | Rust | Stable (System76 shipping) |
| Desktop apps | cosmix-calc, cosmix-toot, etc. | Rust | Building now |
| IPC scripting | cosmix-port (ARexx model) | Rust + Lua | Phase 2 complete |
| Mesh networking | meshd + AMP protocol | Rust | Phase 1 |
| Web API | Axum | Rust | Production-ready |
| Mail/Calendar | Stalwart (JMAP/IMAP/SMTP/CalDAV) | Rust | Feature-complete, pre-1.0 |
| Database | PostgreSQL | C | Mature (decades) |
| Web UI | React | TypeScript | The one concession |
| VPN mesh | WireGuard | C/Rust | Kernel-level, mature |

**No PHP. No Python. No Docker. No Node on the server. No Java. No Go.**

Single-language, single-binary deployments everywhere. Type safety from database
to desktop. Memory safety guaranteed by the compiler. And every component can
talk to every other component through cosmix-port ARexx commands.

## Why This Matters: The AmigaOS Parallel

On the Amiga, ARexx was the universal glue:

- Every application exposed a **port** with named commands
- Scripts could orchestrate multiple applications seamlessly
- The OS provided the IPC infrastructure transparently
- Any app could control any other app

Cosmix recreates this model, but extends it across the network:

```
AmigaOS (1987)                    Cosmix (2026)
─────────────────                 ─────────────────────────────
ARexx ports                   →   cosmix-port (Unix sockets)
ARexx scripts                 →   Lua scripts
Single machine                →   WireGuard mesh (multi-node)
Desktop only                  →   Desktop + web + mail + mesh
App-to-app IPC                →   App-to-app + node-to-node + web API
Manual discovery              →   inotify auto-discovery + HELP handshake
```

The critical difference: **the web is included**. An Axum API server is just
another port on the mesh. Stalwart mail is just another service with an API.
Browser UIs consume the same endpoints that desktop apps and Lua scripts do.

## Architecture

```
┌──────────────────────────────────────────────────────────┐
│                    COSMIC Desktop                         │
│                                                           │
│  cosmix-toot   cosmix-mail   cosmix-view   cosmix-calc   │
│       │             │             │             │         │
│       └─────────────┴─────────────┴─────────────┘         │
│                         │                                 │
│                  cosmix-port (ARexx IPC)                   │
│                  Unix sockets + Lua scripting              │
│                         │                                 │
│                   cosmix daemon                           │
│              Port registry + routing                      │
├───────────────────────┬──────────────────────────────────┤
│                       │                                   │
│     ┌─────────────────┼─────────────────────┐             │
│     │                 │                     │             │
│  Axum API        Stalwart              meshd              │
│  (web backend)   (mail/cal)        (mesh control)         │
│     │                 │                     │             │
│     └────────┬────────┘                     │             │
│              │                              │             │
│         PostgreSQL                    WireGuard            │
│                                     (inter-node)          │
├──────────────────────────────────────────────────────────┤
│                                                           │
│              React (browser UI)                           │
│         Talks to Axum API via REST                        │
│    (only non-Rust component in the stack)                 │
│                                                           │
└──────────────────────────────────────────────────────────┘
```

## The Stack in Detail

### Desktop: libcosmic + cosmix-port

Every COSMIC app gets an ARexx-style port with ~5-20 lines of integration code.
Scripts orchestrate them:

```lua
local mail = cosmix.port("cosmix-mail")
local toot = cosmix.port("cosmix-toot")

-- Check mail, post summary to Mastodon
local status = mail:call("status")
if status.unread > 0 then
    toot:call("post", "I have " .. status.unread .. " unread emails")
end
```

Standard commands (HELP, INFO, ACTIVATE) are auto-generated. Custom commands
are app-specific. The daemon handles discovery, routing, and heartbeat.

### Web API: Axum (replacing Laravel/FrankenPHP)

Axum replaces the entire Laravel stack:

| Laravel | Axum equivalent |
|---------|----------------|
| Routes | `axum::Router` |
| Controllers | Handler functions |
| Eloquent ORM | SeaORM or Diesel |
| Middleware | Tower middleware |
| Auth | axum-login + tower-sessions |
| Queues | tokio tasks + Redis |
| Mail sending | lettre crate |
| WebSocket | axum built-in |

The Axum API is also a cosmix port — Lua scripts and desktop apps consume the
same endpoints as the browser UI.

### Mail & Calendar: Stalwart

[Stalwart](https://github.com/stalwartlabs/stalwart) provides:

- **JMAP** — modern JSON-based mail protocol (what cosmix-mail talks to)
- **IMAP4** — legacy client support
- **SMTP** — sending and receiving
- **CalDAV / CardDAV / WebDAV** — calendar and contacts
- **Built-in web admin UI**
- **Single Rust binary** — no Java, no Docker, no external dependencies
- **PostgreSQL backend** — same database as everything else

Stalwart replaces the need for any PHP-based mail handling in markweb.

### Mesh: meshd + WireGuard

Nodes communicate over WireGuard tunnels using the AMP protocol:

```
---
amp: 1
type: request
from: toot.cosmix.cachyos.amp
to: mail.cosmix.mko.amp
command: status
---
```

Desktop apps on one node can control apps on another node transparently.
The mesh daemon bridges local cosmix-port sockets to remote nodes.

### Browser UI: React (the pragmatic choice)

React remains for browser interfaces because:

1. DOM-heavy CRUD apps don't benefit from WASM performance
2. React ecosystem is massive and proven
3. You already know it from Inertia
4. It talks to the same Axum API as everything else

**Future option:** Incrementally replace React pages with Leptos (Rust → WASM)
where it makes sense. Or don't — React behind an Axum API is perfectly fine.

### Optional future: Leptos for browser UI

If/when the React layer becomes a burden, Leptos offers full-stack Rust:

```rust
#[server]
async fn get_posts() -> Result<Vec<Post>, ServerFnError> {
    // Runs on server — native Rust, database access
    db::query_posts().await
}

#[component]
fn PostList() -> impl IntoView {
    // Runs in browser as WASM, server for SSR
    let posts = create_resource(|| (), |_| get_posts());
    view! {
        <For each=move || posts.get() key=|p| p.id let:post>
            <div>{post.title}</div>
        </For>
    }
}
```

This would eliminate JavaScript entirely, but it's not required for the
architecture to work.

## Who Else Is Doing This?

**Short answer:** Nobody has assembled this exact stack. But the components
are all proven in production separately.

### Rust in production at scale

- **Nearly half of all companies** now use Rust in some capacity (2025 State of Rust Survey)
- **Discord** — rewrote critical services from Go to Rust (10x latency improvement)
- **Cloudflare** — Rust powers their edge network
- **AWS** — Firecracker (Lambda), Bottlerocket OS, multiple services
- **Microsoft** — adopting Rust for Windows kernel components
- **Google** — Android, Chrome, Fuchsia OS components
- **Dropbox** — file sync engine rewritten in Rust

### Rust for cross-platform apps

- **Dioxus** (YC-backed) — single Rust codebase for web + desktop + mobile.
  Used by **Airbus**, **European Space Agency** (collision avoidance),
  **Huawei** (production apps), **Satellite.im** (P2P Discord alternative)
- **Tauri** — Rust backend + web frontend for desktop apps. 85K+ GitHub stars
- **System76** — entire COSMIC desktop environment in Rust

### Stalwart in production

- Feature-complete mail server approaching 1.0
- JMAP + IMAP + SMTP + CalDAV in a single Rust binary
- Growing adoption as a Postfix/Dovecot replacement

### What's unique about Cosmix

Nobody has combined:

1. A **desktop scripting layer** (ARexx model) with
2. A **web API backend** (Axum) with
3. A **mail server** (Stalwart) with
4. A **mesh network** (WireGuard + AMP) with
5. **All in Rust** with a unified IPC protocol

The closest historical parallel is genuinely AmigaOS + ARexx, but that was
single-machine, single-user, no networking. Cosmix extends that vision across
the network and into the web.

## Migration Path

This is not a rewrite-everything-at-once plan. It's incremental:

| Phase | Action | Replaces | Risk |
|-------|--------|----------|------|
| **Now** | Continue building cosmix desktop apps with ports | — | None |
| **Next** | Stand up Axum API alongside Laravel | Nothing yet | Low |
| **Then** | Migrate markweb endpoints one by one to Axum | Laravel routes | Low |
| **Then** | Point cosmix-mail at Stalwart JMAP directly | Laravel mail middleware | Low |
| **Then** | React frontend talks to Axum instead of Laravel | FrankenPHP | Medium |
| **Then** | Retire Laravel/FrankenPHP | PHP entirely | — |
| **Future** | Optionally replace React with Leptos | TypeScript | Optional |

At every phase, the system works. Nothing breaks. Each step is independently
useful and reversible.

## Node Topology

| Node | WireGuard IP | Role | Rust Services |
|------|-------------|------|---------------|
| cachyos | 172.16.2.5 | Dev workstation | COSMIC desktop + cosmix + Axum |
| gcwg | 172.16.2.4 | Container host | Stalwart + Axum API |
| mko | 172.16.2.210 | Production primary | Stalwart + Axum + React UI |
| mmc | 172.16.2.9 | Production secondary | Stalwart + Axum + React UI |

All nodes connected via WireGuard mesh. All services are Rust single binaries.
meshd bridges cosmix-port commands between nodes.

## What Dies

| Technology | Replaced by | When |
|------------|------------|------|
| PHP | Rust (Axum) | Phase by phase |
| Laravel | Axum + SeaORM + tower | Phase by phase |
| FrankenPHP | Axum (native HTTP) | When Laravel is gone |
| Composer | Cargo | With PHP |
| Node.js (server) | Nothing — never needed | With PHP |
| Docker | Single binaries + systemd | Already policy |
| Postfix/Dovecot (if used) | Stalwart | Direct replacement |

## What Survives

| Technology | Why |
|------------|-----|
| **PostgreSQL** | Best relational database, period |
| **React** | Browser UI, pragmatic choice |
| **WireGuard** | Kernel-level VPN, nothing better |
| **systemd** | Process management, proven |
| **Lua** | Scripting glue, embeddable, hot-reloadable |
| **bash** | Interactive shell (not programming) |

## The Vision

A developer sits at their COSMIC desktop. They write a Lua script that:

1. Queries their Mastodon feed via `cosmix-toot` port
2. Searches their email via `cosmix-mail` port (talking to Stalwart JMAP)
3. Fetches data from the web API via `cosmix-web` port (Axum)
4. Posts results to a mesh node via `meshd`
5. Opens a browser tab showing the same data via React UI

Every step uses the same IPC protocol. Every service is a Rust binary.
Every component is discoverable, scriptable, and composable.

**It's AmigaOS ARexx, but for the distributed web era.**

---

*Document created: 2026-03-08*
*Status: Architectural vision — not yet a committed implementation plan*
