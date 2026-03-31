# Cosmix — Current State

Last updated: 2026-03-31

## Repo Structure (changed 2026-03-31)

- Repo moved from `~/.gh/cosmixos` to `~/.cosmix/`, GitHub renamed to `markc/cosmix`
- Cargo workspace lives in `src/` — top level is clean (README, CLAUDE.md, LICENSE)
- 28 crates: 7 libs, 13 apps, 8 daemons (including cosmix-claude)

## Scripting: Mix (not Lua)

**Lua is fully removed.** All scripting uses [Mix](https://github.com/markc/mix) (`~/.mix/`), a pure-Rust ARexx-inspired language purpose-built for cosmix:

- `send`, `address`, `emit` are language keywords (not library calls)
- Pure Rust, no C deps, compiles to `wasm32-unknown-unknown`
- Real interactive shell with pipes, redirects, tab completion, job control
- `mix-core` embedded in `cosmix-lib-script` and `cosmix-scripts` as path dep
- AMP IPC over Unix sockets; full mesh addresses reserved for future WebSocket routing

## App Launch Policy (decided 2026-03-31)

**The hub does NOT auto-start apps.** If `send "view" view.open` fails because cosmix-view isn't running, the script gets `rc: 10`. This is deliberate:

- Keeps the hub dumb — it routes messages, not manages processes
- No hidden latency from cold starts on what looks like a simple IPC call
- Fits the ARexx model: the script is the orchestrator, not the infrastructure

**Recommended patterns:**

```mix
-- Pattern 1: Mix helper function
function ensure_running($app)
    if not port_exists($app) then
        sh "cosmix-${app} &"
        sleep 1
    end
end

ensure_running("view")
send "view" view.open path="/tmp/file.md"

-- Pattern 2: AMP via cosmix-menu
send "menu" menu.launch app="cosmix-view"
send "view" view.open path="/tmp/file.md"
```

Both keep launch policy visible in the script. Pattern 2 is preferred when cosmix-menu is running (it already knows all app binaries).

## Target Mesh Node Architecture (decided 2026-03-25)

### Two-Tier Design: Lightweight Nodes + Powerful Inference Servers

**Mesh nodes** are sovereign, self-contained, minimal-footprint:

| Component | Owns | RAM idle |
|-----------|------|----------|
| cosmix-maild | `mail.db` (accounts, mailboxes, emails, calendars, contacts, changelog) — rusqlite + FTS5 | ~10MB |
| cosmix-indexd | `vectors.db` (embeddings via sqlite-vec) + nomic-embed-text model (on-demand) | ~7MB |
| cosmix-lib-script + cosmix-scripts | Mix scripting + Bash | ~5MB |
| cosmix-webd | HTTPS frontend + WASM app server | ~5MB |
| **Total** | **Two SQLite files, zero daemons** | **~27MB** |

No PostgreSQL, no pgvector, no DNS, no Nginx, no Postfix, no Docker, no Ollama.

**Inference servers** (ollama2/ollama3, 64GB RAM) provide shared AI services:
- PostgreSQL + pgvector — cross-node corpus, large-scale analytics
- Ollama — LLM chat (qwen2.5) + bulk embeddings
- Frontier fallback — Claude/GPT for complex queries

### Data Separation

```
cosmix-maild (mail.db via rusqlite)
├── Auth, accounts, mailboxes, emails, threads, blobs     <- sub-ms, local
├── Calendar, contacts, changelog                          <- sub-ms, local
├── Keyword/FTS search (SQLite FTS5)                       <- instant, no model
│
cosmix-indexd (vectors.db via rusqlite + sqlite-vec)
├── Ingest: text -> nomic-embed-text -> 768-dim vector -> store  <- loads model on demand
├── Semantic search: text -> embed query -> KNN in sqlite-vec    <- loads model on demand
└── Model unloads after 60s idle (7MB -> 298MB -> 7MB)
```

Two separate SQLite databases = two independent write locks = no single-writer contention.

### Cluster-Wide Queries (no shared database needed)

| Query type | Mechanism |
|---|---|
| Local mail search | SQLite FTS5 — instant |
| Semantic search (local) | cosmix-indexd KNN — sub-10ms for <50K vectors |
| Cross-node search | AMP fan-out to peers, each queries locally, merge results |
| AI chat/RAG | Ollama CT queries each node's cosmix-indexd, merges, runs LLM |
| Service discovery | AMP port_list/port_update gossip over WireGuard |

### Key Decisions

- **SQLite replaces PostgreSQL** on mesh nodes — rusqlite with `bundled` feature, zero system deps
- **sqlite-vec replaces pgvector** — flat scan, fine for <50K embeddings per node
- **FTS5 for keyword search** — no model needed, covers 90% of mail searches
- **Semantic search loads model** — only for AI-powered queries, a few times per day
- **cosmix-indexd absorbs vector storage** — embed + store + search in one service
- **No DNS** — WireGuard IPs + AMP gossip (decided 2026-03-16)
- **LLM inference is remote** — Ollama CTs or frontier APIs, never on mesh nodes

## Implementation Roadmap

### Phase A: SQLite Migration (NEXT — prerequisite for mesh rollout)

Migrate cosmix-maild from sqlx+PostgreSQL to rusqlite+SQLite. This is the critical path to 3-node real-world testing.

1. **Add rusqlite dep** to cosmix-maild, feature `bundled`
2. **Port migrations** — convert 4 PostgreSQL migrations to SQLite DDL (no UUID type, no JSONB — use TEXT + JSON functions, BLOB for UUIDs)
3. **Port db/ modules** — rewrite ~10 db/*.rs files from sqlx queries to rusqlite (accounts, mailboxes, emails, threads, blobs, changelog, calendar, contacts)
4. **Port auth** — bcrypt verify stays, just change the query layer
5. **Add FTS5** — create FTS index on emails(subject, preview, from_addr) for keyword search
6. **Extend cosmix-indexd** — add `store`, `search`, `delete` commands + sqlite-vec database
7. **Test on mko** — deploy, migrate existing data, verify JMAP + SMTP + embed flow
8. **Roll out to 3 nodes** — mko + 2 more, real-world testing

### Phase B: Complete cosmix-mail Client

After nodes are deployed and receiving real mail:

1. Test compose end-to-end against live node
2. Calendar/Contacts UI
3. Push notifications (EventSource)

### Phase C: Hardening + Polish

1. Rate limiting, health check, graceful shutdown
2. Wire Mix scripting into cosmix-maild
3. JMAP compliance tests
4. Sieve filtering

### Phase D: Mesh Networking

1. AMP peer sync over WireGuard
2. SMTP mesh bypass
3. Cross-node search fan-out

## AMP-Addressable UI (completed 2026-03-29)

Full implementation of AMP-driven UI control (the ARexx-at-mesh-scale vision):
- **Widget types:** AmpButton, AmpToggle, AmpInput — auto-register on mount, deregister on unmount
- **Commands:** ui.list, ui.get, ui.invoke, ui.highlight, ui.set, ui.batch
- **Menu commands:** menu.list, menu.invoke, menu.highlight, menu.close
- **Cross-app scripting:** editor content -> rendered markdown in viewer, all via AMP
- **Key insight:** `id` = transport (WS multiplexing), `to`/`from` = application (endpoint addressing)

## What's Complete

- **Phase 1-2:** COSMIC desktop + Dioxus 0.7 toolchain validated
- **Phase 3 J1-J3:** JMAP Core + Email + SMTP + Calendars + Contacts (22 methods)
- **Phase 4:** CalDAV/CardDAV pivoted to native JMAP methods
- **cosmix-lib-script + cosmix-scripts:** Mix scripting integrated (Lua removed)
- **cosmix-mail UI:** Compose, Reply, Forward, Actions toolbar, unread badges
- **cosmix-indexd:** Lazy load/unload, 7MB idle, deployed to mko
- **Server Email/set create:** Upload blob -> create email record from MIME
- **AMP-addressable UI:** Full ui.* and menu.* command vocabulary
- **Repo restructure:** src/ layout, GitHub renamed to markc/cosmix

## Fixes Applied (2026-03-25)

- Installed `webkit2gtk-4.1` on host — cosmix-mail desktop crash fixed
- cosmix-indexd lazy load/unload deployed to mko, systemd `Restart=on-failure`
- pdns removed from mko (service, packages, database, config)
- Redis, stalwart-mail, 5 PHP/Laravel services removed from mko
- 3 stale databases + 3 stale PostgreSQL users dropped from mko
- journald on mko capped to 16MB (was 2.1GB from embed restart loop)
- mko cleaned: 6 services running (5 cosmix + PostgreSQL), 94MB RAM used, 3.2GB disk
