# cosmix-jmap: Minimal JMAP Server Implementation Plan

**Date:** 2026-03-10
**Status:** Proposal
**Replaces:** Stalwart Mail Server (Pattern B → Pattern A)

## Overview

Build a minimal, single-binary JMAP server in Rust that implements JMAP Mail, JMAP Calendars (JSCalendar), JMAP Contacts (JSContact), JMAP Files, and SMTP — with PostgreSQL as the sole backend and cosmix-port built in. Uses stalwart's published ecosystem crates (`mail-parser`, `mail-auth`, `mail-send`) as foundations, and stalwart's 295K-line codebase as the reference implementation.

**Pure JSON, no XML.** The JMAP spec includes JSON-native replacements for CalDAV, CardDAV, and WebDAV — no DAV protocols, no XML parsing, no `quick-xml` dependency. Everything goes through the same `/api` endpoint using the same JSON-RPC pattern.

### JMAP protocol family

| Legacy (XML/DAV) | JMAP replacement (JSON) | Data format | Spec |
|---|---|---|---|
| IMAP | JMAP for Mail | RFC 8621 | RFC 8620 + 8621 |
| CalDAV | JMAP for Calendars | JSCalendar | RFC 8984, draft-ietf-jmap-calendars |
| CardDAV | JMAP for Contacts | JSContact | RFC 9553, draft-ietf-jmap-contacts |
| WebDAV | JMAP Blob methods | JMAP Core | RFC 8620 §6 |

### Why not just keep stalwart?

| Concern | Stalwart | cosmix-jmap |
|---------|----------|-------------|
| Binary size | 77 MB | Target ~8-12 MB |
| RSS memory | 174-450 MB (RocksDB creep) | Target 20-40 MB |
| Backend | 8 backends × 21K LOC store abstraction | PostgreSQL only |
| Protocols | JMAP+IMAP+POP3+SMTP+ManageSieve+DAV (XML) | JMAP+SMTP+Sieve (pure JSON) |
| Scripting | None | cosmix-port (Lua orchestration) |
| Configuration | TOML + database-stored settings | TOML only |
| Licensing | AGPL-3.0 + proprietary (enterprise) | Same as cosmix (permissive) |
| Architecture | Monolithic 25-crate workspace | Single focused crate |

### Client strategy

**No IMAP. No DAV. No XML.** We control both ends:

- **cosmix-web** (Axum + HTMX) = webmail + calendar + contacts UI, talks JMAP to cosmix-jmap
- **cosmix-mail** (libcosmic) = native COSMIC desktop mail client, talks JMAP
- **cosmix-port** = Lua scripting interface for automation

Desktop clients like Thunderbird have no JMAP support as of March 2026 (tracking bug open since 2016). We don't need them — we build our own clients. This eliminates IMAP (~5K LOC of stateful protocol), CalDAV/CardDAV (~3K LOC of XML), and WebDAV (~1K LOC) — replaced by JMAP methods that share the same dispatcher, auth, and change tracking.

### Design principles

1. **PostgreSQL is the only backend** — no abstraction layer, direct sqlx queries
2. **JMAP is the only API** — no IMAP, no POP3, no DAV, consumed by our own clients
3. **Reuse stalwartlabs crates** — don't rewrite mail parsing, auth, or sieve
4. **Stalwart is the reference** — when in doubt, read stalwart's implementation
5. **cosmix-port built in** — scriptable from day one
6. **Pure JSON** — JSCalendar + JSContact, no iCalendar/vCard/XML

## Stalwart ecosystem crates we reuse

Published on crates.io under permissive licenses (Apache-2.0 OR MIT) unless noted:

| Crate | Version | Purpose | License |
|-------|---------|---------|---------|
| `mail-parser` | 0.11 | RFC 5322/MIME parsing | Apache-2.0/MIT |
| `mail-builder` | 0.4 | Construct RFC 5322 messages | Apache-2.0/MIT |
| `mail-auth` | 0.8 | DKIM/SPF/DMARC/ARC validation + signing | Apache-2.0/MIT |
| `mail-send` | 0.5 | SMTP client for outbound delivery | Apache-2.0/MIT |
| `smtp-proto` | 0.2 | SMTP protocol parser | Apache-2.0/MIT |

**Not needed:** `calcard` (iCalendar/vCard parser) — we use JSCalendar/JSContact (native JSON), not legacy formats. `sieve-rs` excluded (AGPL-3.0) — Sieve filtering will use a custom minimal implementation or simple rule engine.

## Architecture

```
cosmix-jmap (single binary, ~12-15K LOC)
├── main.rs              — CLI (clap), config loading, server bootstrap
├── config.rs            — TOML config (listen, TLS, PostgreSQL, cosmix)
├── db/
│   ├── mod.rs           — sqlx PostgreSQL pool, migrations
│   ├── schema.sql       — table definitions
│   ├── email.rs         — email CRUD (store, fetch, query, delete)
│   ├── mailbox.rs       — mailbox CRUD
│   ├── thread.rs        — thread grouping
│   ├── calendar.rs      — calendar + event CRUD (JSCalendar JSONB)
│   ├── contact.rs       — addressbook + contact CRUD (JSContact JSONB)
│   ├── blob.rs          — blob storage (filesystem + DB metadata)
│   ├── changelog.rs     — modification sequence log
│   └── account.rs       — user accounts, authentication
├── jmap/
│   ├── mod.rs           — Axum router, session resource
│   ├── request.rs       — JMAP request dispatcher (/api, method routing)
│   ├── email.rs         — Email/get, Email/set, Email/query, Email/changes
│   ├── mailbox.rs       — Mailbox/get, Mailbox/set, Mailbox/query, Mailbox/changes
│   ├── thread.rs        — Thread/get
│   ├── identity.rs      — Identity/get, Identity/set
│   ├── submission.rs    — EmailSubmission/set (send via SMTP)
│   ├── calendar.rs      — Calendar/get, Calendar/set, CalendarEvent/get, CalendarEvent/set, /query, /changes
│   ├── contact.rs       — AddressBook/get, AddressBook/set, Contact/get, Contact/set, /query, /changes
│   ├── blob.rs          — upload/download endpoints + File/get, File/set
│   ├── push.rs          — EventSource push (SSE)
│   └── sieve.rs         — SieveScript/get, SieveScript/set, SieveScript/validate
├── smtp/
│   ├── mod.rs           — TCP listener, TLS upgrade
│   ├── session.rs       — SMTP state machine (EHLO→AUTH→MAIL→RCPT→DATA)
│   ├── auth.rs          — AUTH PLAIN/LOGIN against account DB
│   ├── inbound.rs       — receive mail, run sieve, deliver to JMAP store
│   ├── queue.rs         — outbound queue (PostgreSQL rows)
│   └── delivery.rs      — background MX delivery with retry
├── auth/
│   ├── mod.rs           — authentication middleware
│   ├── basic.rs         — HTTP Basic auth
│   └── password.rs      — bcrypt/argon2 verification
└── port.rs              — cosmix-port integration (commands: status, accounts, deliver, search)
```

**Key difference from the DAV approach:** No `dav/` module. Calendars and contacts are just more JMAP method handlers in `jmap/calendar.rs` and `jmap/contact.rs` — same dispatcher, same auth, same change tracking, same JSON wire format.

### External dependencies

| Crate | Purpose |
|-------|---------|
| `axum` | HTTP server (JMAP API) |
| `tokio` | Async runtime |
| `sqlx` | PostgreSQL with compile-time query checking |
| `rustls` + `tokio-rustls` | TLS for SMTP + HTTPS |
| `mail-parser` | Parse inbound email (RFC 5322/MIME) |
| `mail-builder` | Construct outbound email |
| `mail-auth` | DKIM signing, SPF/DMARC validation |
| `mail-send` | SMTP client (relay outbound mail) |
| `smtp-proto` | SMTP protocol parser (inbound) |
| `serde` / `serde_json` | JSON serialization (JMAP + JSCalendar + JSContact) |
| `clap` | CLI |
| `cosmix-port` | AMP IPC + command registry |

**Not needed:** `quick-xml`, `calcard`, `hyper` (axum handles HTTP).

## PostgreSQL Schema (core tables)

```sql
-- Accounts
CREATE TABLE accounts (
    id          SERIAL PRIMARY KEY,
    email       TEXT UNIQUE NOT NULL,
    password    TEXT NOT NULL,           -- bcrypt/argon2 hash
    name        TEXT,
    quota       BIGINT DEFAULT 0,       -- bytes, 0 = unlimited
    created_at  TIMESTAMPTZ DEFAULT NOW()
);

-- Mailboxes (JMAP Mailbox objects)
CREATE TABLE mailboxes (
    id          UUID PRIMARY KEY DEFAULT gen_random_uuid(),
    account_id  INT REFERENCES accounts(id),
    name        TEXT NOT NULL,
    parent_id   UUID REFERENCES mailboxes(id),
    role        TEXT,                   -- inbox, sent, drafts, trash, junk, archive
    sort_order  INT DEFAULT 0,
    UNIQUE(account_id, parent_id, name)
);

-- Threads
CREATE TABLE threads (
    id          UUID PRIMARY KEY DEFAULT gen_random_uuid(),
    account_id  INT REFERENCES accounts(id)
);

-- Emails (JMAP Email objects)
CREATE TABLE emails (
    id          UUID PRIMARY KEY DEFAULT gen_random_uuid(),
    account_id  INT REFERENCES accounts(id),
    thread_id   UUID REFERENCES threads(id),
    mailbox_ids UUID[] NOT NULL,        -- array of mailbox UUIDs
    blob_id     UUID NOT NULL,          -- reference to blob store
    size        INT NOT NULL,
    received_at TIMESTAMPTZ NOT NULL,
    -- Parsed headers (denormalized for query performance)
    message_id  TEXT,
    in_reply_to TEXT[],
    subject     TEXT,
    from_addr   JSONB,                  -- [{name, email}]
    to_addr     JSONB,
    cc_addr     JSONB,
    date        TIMESTAMPTZ,
    preview     TEXT,                   -- first 256 chars
    has_attachment BOOLEAN DEFAULT FALSE,
    keywords    JSONB DEFAULT '{}',     -- {$seen: true, $flagged: true, ...}
    created_at  TIMESTAMPTZ DEFAULT NOW()
);
CREATE INDEX idx_emails_account_mailbox ON emails USING GIN (mailbox_ids);
CREATE INDEX idx_emails_thread ON emails (thread_id);
CREATE INDEX idx_emails_received ON emails (account_id, received_at DESC);
CREATE INDEX idx_emails_message_id ON emails (message_id);

-- Blobs (raw data on filesystem, metadata in DB)
CREATE TABLE blobs (
    id          UUID PRIMARY KEY DEFAULT gen_random_uuid(),
    account_id  INT REFERENCES accounts(id),
    size        INT NOT NULL,
    hash        TEXT NOT NULL,          -- blake3 content hash
    created_at  TIMESTAMPTZ DEFAULT NOW()
);

-- Calendars (JMAP Calendar objects)
CREATE TABLE calendars (
    id          UUID PRIMARY KEY DEFAULT gen_random_uuid(),
    account_id  INT REFERENCES accounts(id),
    name        TEXT NOT NULL,
    color       TEXT,
    description TEXT,
    is_visible  BOOLEAN DEFAULT TRUE,
    default_alerts JSONB,               -- JSCalendar default alerts
    timezone    TEXT DEFAULT 'UTC'
);

-- Calendar events (JSCalendar format — RFC 8984)
CREATE TABLE calendar_events (
    id          UUID PRIMARY KEY DEFAULT gen_random_uuid(),
    calendar_id UUID REFERENCES calendars(id),
    account_id  INT REFERENCES accounts(id),
    uid         TEXT NOT NULL,
    data        JSONB NOT NULL,         -- full JSCalendar Event object
    -- Denormalized for queries
    title       TEXT,
    start_dt    TIMESTAMPTZ,
    end_dt      TIMESTAMPTZ,
    updated_at  TIMESTAMPTZ DEFAULT NOW(),
    UNIQUE(calendar_id, uid)
);
CREATE INDEX idx_events_account ON calendar_events (account_id);
CREATE INDEX idx_events_range ON calendar_events (calendar_id, start_dt, end_dt);

-- Address books (JMAP AddressBook objects)
CREATE TABLE addressbooks (
    id          UUID PRIMARY KEY DEFAULT gen_random_uuid(),
    account_id  INT REFERENCES accounts(id),
    name        TEXT NOT NULL,
    description TEXT
);

-- Contacts (JSContact format — RFC 9553)
CREATE TABLE contacts (
    id              UUID PRIMARY KEY DEFAULT gen_random_uuid(),
    addressbook_id  UUID REFERENCES addressbooks(id),
    account_id      INT REFERENCES accounts(id),
    uid             TEXT NOT NULL,
    data            JSONB NOT NULL,     -- full JSContact Card object
    -- Denormalized for queries
    full_name       TEXT,
    email           TEXT,               -- primary email
    company         TEXT,
    updated_at      TIMESTAMPTZ DEFAULT NOW(),
    UNIQUE(addressbook_id, uid)
);
CREATE INDEX idx_contacts_account ON contacts (account_id);
CREATE INDEX idx_contacts_name ON contacts (full_name);

-- Files (JMAP File objects — blob metadata with path)
CREATE TABLE files (
    id          UUID PRIMARY KEY DEFAULT gen_random_uuid(),
    account_id  INT REFERENCES accounts(id),
    blob_id     UUID REFERENCES blobs(id),
    name        TEXT NOT NULL,
    parent_id   UUID REFERENCES files(id),
    content_type TEXT,
    size        INT NOT NULL,
    created_at  TIMESTAMPTZ DEFAULT NOW(),
    updated_at  TIMESTAMPTZ DEFAULT NOW(),
    UNIQUE(account_id, parent_id, name)
);

-- Sieve scripts
CREATE TABLE sieve_scripts (
    id          UUID PRIMARY KEY DEFAULT gen_random_uuid(),
    account_id  INT REFERENCES accounts(id),
    name        TEXT NOT NULL,
    script      TEXT NOT NULL,
    is_active   BOOLEAN DEFAULT FALSE,
    compiled    BYTEA,                  -- pre-compiled sieve bytecode
    updated_at  TIMESTAMPTZ DEFAULT NOW(),
    UNIQUE(account_id, name)
);

-- Change log (JMAP modification sequences)
CREATE TABLE changelog (
    id          BIGSERIAL PRIMARY KEY,  -- monotonically increasing = modseq
    account_id  INT REFERENCES accounts(id),
    object_type TEXT NOT NULL,          -- Email, Mailbox, Thread, CalendarEvent, Contact, File
    object_id   UUID NOT NULL,
    change_type TEXT NOT NULL,          -- created, updated, destroyed
    changed_at  TIMESTAMPTZ DEFAULT NOW()
);
CREATE INDEX idx_changelog_account_type ON changelog (account_id, object_type, id);

-- SMTP outbound queue
CREATE TABLE smtp_queue (
    id          BIGSERIAL PRIMARY KEY,
    from_addr   TEXT NOT NULL,
    to_addrs    TEXT[] NOT NULL,
    blob_id     UUID REFERENCES blobs(id),
    attempts    INT DEFAULT 0,
    next_retry  TIMESTAMPTZ DEFAULT NOW(),
    last_error  TEXT,
    created_at  TIMESTAMPTZ DEFAULT NOW()
);
CREATE INDEX idx_queue_retry ON smtp_queue (next_retry) WHERE attempts < 10;
```

## Phased Implementation

### Phase J1: JMAP Core + Email (MVP)

**Goal:** cosmix-web can list mailboxes, read email, move/flag messages via JMAP.

**Estimated LOC:** ~3,500
**Duration:** 1-2 weeks

#### Deliverables

1. **HTTP server** (Axum)
   - `/.well-known/jmap` → session resource (capabilities, account, API URL)
   - `/jmap` → JMAP request endpoint (method dispatch)
   - `/jmap/blob/{id}` → download
   - `/jmap/upload/{accountId}` → upload
   - Basic auth middleware

2. **JMAP methods**
   - `Core/echo`
   - `Mailbox/get`, `Mailbox/set`, `Mailbox/query`, `Mailbox/changes`
   - `Email/get`, `Email/set`, `Email/query`, `Email/changes`
   - `Thread/get`
   - `Identity/get`

3. **PostgreSQL storage**
   - Schema migration (sqlx migrate)
   - Email CRUD with keyword (flag) updates
   - Mailbox CRUD with role enforcement
   - Thread grouping by message-id / in-reply-to
   - Changelog with modseq tracking

4. **Blob store**
   - Filesystem storage: `$DATA_DIR/blobs/{hash[0:2]}/{hash[2:4]}/{hash}`
   - Upload/download via Axum

5. **Configuration**
   - TOML config: listen address, TLS cert paths, PostgreSQL URL, data directory
   - CLI: `cosmix-jmap serve`, `cosmix-jmap migrate`, `cosmix-jmap account add/list/delete`

#### Stalwart reference files

| cosmix-jmap module | Stalwart reference |
|--------------------|-------------------|
| `jmap/request.rs` | `crates/jmap/src/api/request.rs` |
| `jmap/email.rs` | `crates/jmap/src/email/` (get.rs, set.rs, query.rs) |
| `jmap/mailbox.rs` | `crates/jmap/src/mailbox/` |
| `jmap/thread.rs` | `crates/jmap/src/thread/get.rs` |
| `jmap/blob.rs` | `crates/jmap/src/blob/` |
| `db/changelog.rs` | `crates/store/src/write/log.rs` |

#### Verification

- Import test mailbox (mbox or Maildir) via CLI
- cosmix-web mail panel lists folders, reads messages, flags/moves/deletes
- `curl` against JMAP API for raw protocol validation

---

### Phase J2: SMTP Inbound + Outbound

**Goal:** Receive email via SMTP, deliver to JMAP store. Send email via SMTP submission.

**Estimated LOC:** ~2,500
**Duration:** 1-2 weeks

#### Deliverables

1. **SMTP server** (port 25 inbound, port 465/587 submission)
   - `smtp-proto` for parsing
   - STARTTLS via `tokio-rustls`
   - AUTH PLAIN/LOGIN (submission only)
   - State machine: EHLO → AUTH → MAIL FROM → RCPT TO → DATA → deliver

2. **Inbound delivery**
   - Parse with `mail-parser`
   - Thread assignment (message-id / in-reply-to lookup)
   - Store blob + create email record
   - Run active Sieve script (if Phase J4 complete, otherwise skip)

3. **Outbound delivery**
   - `EmailSubmission/set` JMAP method → queue row
   - Background worker: poll queue, deliver via `mail-send`
   - DKIM signing via `mail-auth`
   - Retry with exponential backoff (1m, 5m, 30m, 2h, 8h)
   - Bounce generation on permanent failure

4. **JMAP EmailSubmission**
   - `EmailSubmission/get`, `EmailSubmission/set`, `EmailSubmission/query`
   - Ties into outbound queue status

#### Stalwart reference files

| cosmix-jmap module | Stalwart reference |
|--------------------|-------------------|
| `smtp/session.rs` | `crates/smtp/src/inbound/session.rs` |
| `smtp/auth.rs` | `crates/smtp/src/inbound/auth.rs` |
| `smtp/inbound.rs` | `crates/smtp/src/inbound/data.rs` |
| `smtp/queue.rs` | `crates/smtp/src/queue/` |
| `smtp/delivery.rs` | `crates/smtp/src/outbound/` |
| `jmap/submission.rs` | `crates/jmap/src/submission/` |

#### Verification

- Send email from cosmix-web → cosmix-jmap → external MX
- Receive email from external sender → cosmix-jmap → visible in cosmix-web
- Check DKIM signature passes (use mail-tester.com)

---

### Phase J3: JMAP Calendars + Contacts

**Goal:** cosmix-web provides calendar and contact management, all via JMAP.

**Estimated LOC:** ~2,000
**Duration:** 1-2 weeks

**This phase is significantly simpler than the DAV approach** because calendars and contacts are just more JMAP methods — same dispatcher, same auth, same change tracking. No XML, no PROPFIND, no REPORT, no MKCALENDAR.

#### Deliverables

1. **JMAP Calendar methods** (JSCalendar format, RFC 8984)
   - `Calendar/get`, `Calendar/set`, `Calendar/query`, `Calendar/changes`
   - `CalendarEvent/get`, `CalendarEvent/set`, `CalendarEvent/query`, `CalendarEvent/changes`
   - Events stored as JSONB (native JSCalendar)
   - Denormalized `start_dt`/`end_dt` for range queries

2. **JMAP Contact methods** (JSContact format, RFC 9553)
   - `AddressBook/get`, `AddressBook/set`, `AddressBook/query`, `AddressBook/changes`
   - `Contact/get`, `Contact/set`, `Contact/query`, `Contact/changes`
   - Contacts stored as JSONB (native JSContact)
   - Denormalized `full_name`/`email` for search

3. **Session resource update**
   - Add `urn:ietf:params:jmap:calendars` capability
   - Add `urn:ietf:params:jmap:contacts` capability

#### Why this is simpler than CalDAV/CardDAV

| CalDAV/CardDAV approach | JMAP approach |
|---|---|
| New HTTP methods (PROPFIND, REPORT, MKCALENDAR) | Same POST /jmap |
| XML request/response parsing | Same JSON request/response |
| Separate auth + discovery (well-known, principal) | Same session resource |
| ETag-based sync | Same modseq change tracking |
| iCalendar/vCard text parsing | JSCalendar/JSContact native JSON |
| `quick-xml` + `calcard` dependencies | Just `serde_json` |
| ~3,000 LOC | ~2,000 LOC |

#### Stalwart reference files

| cosmix-jmap module | Stalwart reference |
|--------------------|-------------------|
| `jmap/calendar.rs` | `crates/jmap/src/calendar/` + `crates/jmap/src/calendar_event/` |
| `jmap/contact.rs` | `crates/jmap/src/contact/` + `crates/jmap/src/addressbook/` |
| `db/calendar.rs` | `crates/store/src/write/` (event storage) |
| `db/contact.rs` | `crates/store/src/write/` (contact storage) |

#### Verification

- cosmix-web calendar view: create/edit/delete events, date range queries
- cosmix-web contacts view: create/edit/delete contacts, search by name/email
- `curl` against JMAP API testing Calendar/get, Contact/get

---

### Phase J4: Sieve Filtering

**Goal:** Users can manage server-side mail filtering rules.

**Estimated LOC:** ~800
**Duration:** 3-5 days

#### Deliverables

1. **Sieve script management**
   - JMAP: `SieveScript/get`, `SieveScript/set`, `SieveScript/validate`
   - Store scripts in `sieve_scripts` table
   - Pre-compile on save (cache bytecode)
   - One active script per account

2. **Sieve execution on inbound delivery**
   - Hook into SMTP inbound pipeline (Phase J2)
   - Actions: `keep`, `discard`, `redirect`, `fileinto` (move to mailbox), `flag`
   - Custom minimal Sieve interpreter (subset of RFC 5228) or simple rule engine
   - Provide mail envelope + parsed message as filter context

#### Stalwart reference files

| cosmix-jmap module | Stalwart reference |
|--------------------|-------------------|
| `jmap/sieve.rs` | `crates/jmap/src/sieve/` |
| Sieve execution | `crates/email/src/sieve/` |

#### Verification

- Create sieve rule via JMAP: "if from contains 'newsletter' then fileinto 'Newsletters'"
- Send matching email → appears in correct mailbox
- cosmix-web sieve editor can list/edit/activate scripts

---

### Phase J5: JMAP Files

**Goal:** Personal file storage via JMAP blob methods.

**Estimated LOC:** ~600
**Duration:** 2-3 days

Since we use JMAP instead of WebDAV, file storage is built on top of JMAP's existing blob infrastructure (upload/download) plus a File object type for metadata.

#### Deliverables

1. **JMAP File methods**
   - `File/get`, `File/set`, `File/query`, `File/changes`
   - Create directories (File with no blob_id)
   - Upload: blob upload → File/set to associate metadata
   - Download: existing blob download endpoint
   - Move/copy via File/set (update parent_id or name)

2. **Session resource update**
   - Add file storage capability

#### Verification

- cosmix-web file manager: upload, download, rename, delete, create folders
- Quota enforcement against account quota

---

### Phase J6: cosmix-port Integration

**Goal:** Lua scripts can query and control the JMAP server.

**Estimated LOC:** ~500
**Duration:** 2-3 days

#### Deliverables

1. **Port commands**
   - `status` — account count, queue depth, storage usage
   - `accounts` — list accounts
   - `deliver` — inject email into a mailbox (for scripting)
   - `search` — search emails by query
   - `queue` — list/flush/retry outbound queue
   - `sieve` — list/activate sieve scripts
   - Standard: `help`, `info`, `activate`

2. **Lua integration** (via cosmix daemon)
   ```lua
   local jmap = cosmix.port("jmap")
   local results = jmap:call("search", { query = "from:boss", limit = 5 })
   cosmix.notify("Boss sent " .. #results .. " emails")

   -- Calendar integration
   local events = jmap:call("events", { calendar = "work", days = 7 })
   cosmix.notify("You have " .. #events .. " events this week")
   ```

3. **Event notifications**
   - `PortEvent::NewMail { account, from, subject }` on inbound delivery
   - Daemon can trigger Lua scripts on mail arrival

---

### Phase J7: Hardening + Production

**Goal:** Production-ready deployment replacing stalwart on gcwg/mko/mmc.

**Estimated LOC:** ~2,000 (tests, error handling, edge cases)
**Duration:** 1-2 weeks

#### Deliverables

1. **Security**
   - Rate limiting on SMTP (per-IP, per-account)
   - SPF/DMARC validation on inbound (via `mail-auth`)
   - Brute-force protection on auth endpoints
   - TLS 1.2+ enforcement

2. **Reliability**
   - Graceful shutdown (drain SMTP connections, flush queue)
   - Connection pooling (sqlx pool sizing)
   - Health check endpoint (`/health`)
   - Structured logging (tracing)

3. **Migration tooling**
   - Import from stalwart export (`--import` from stalwart export directory)
   - Import from Maildir/mbox
   - Account migration script

4. **Systemd service**
   - `cosmix-jmap.service` unit file
   - `RuntimeDirectory=cosmix-jmap`

## Size Estimate Summary

| Phase | LOC | Cumulative |
|-------|-----|------------|
| J1: JMAP Core + Email | 3,500 | 3,500 |
| J2: SMTP | 2,500 | 6,000 |
| J3: Calendars + Contacts | 2,000 | 8,000 |
| J4: Sieve | 800 | 8,800 |
| J5: Files | 600 | 9,400 |
| J6: cosmix-port | 500 | 9,900 |
| J7: Hardening | 2,000 | 11,900 |
| **Tests** | ~3,000 | **~15,000** |

**Target: ~15K LOC** vs stalwart's 295K — a 20x reduction, enabled by:
- PostgreSQL only (no store abstraction)
- No IMAP/POP3
- No DAV/XML (pure JMAP with JSCalendar/JSContact)
- Reusing stalwartlabs ecosystem crates
- No enterprise features / multi-tenancy
- Simple TOML config (no database-stored settings)

The pure-JMAP approach saves ~2K LOC vs the DAV approach (Phase M3 was 3,000 LOC for CalDAV/CardDAV, now J3 is 2,000 LOC for JMAP calendars/contacts — and J5 files is 600 LOC vs M5 WebDAV at 1,000 LOC).

## Crate layout

```toml
# Cargo.toml (workspace member)
[package]
name = "cosmix-jmap"
version = "0.1.0"

[[bin]]
name = "cosmix-jmap"
path = "src/main.rs"

[dependencies]
cosmix-port = { path = "../cosmix-port" }
axum = "0.8"
tokio = { version = "1", features = ["full"] }
sqlx = { version = "0.8", features = ["runtime-tokio", "tls-rustls", "postgres", "uuid", "chrono", "json"] }
rustls = "0.23"
tokio-rustls = "0.26"
mail-parser = "0.11"
mail-builder = "0.4"
mail-auth = "0.8"
mail-send = "0.5"
smtp-proto = "0.2"
serde = { version = "1", features = ["derive"] }
serde_json = "1"
clap = { version = "4", features = ["derive"] }
uuid = { version = "1", features = ["v4"] }
chrono = { version = "0.4", features = ["serde"] }
blake3 = "1"
tracing = "0.1"
tracing-subscriber = "0.3"
toml = "0.8"
```

## Risk assessment

| Risk | Severity | Mitigation |
|------|----------|------------|
| JMAP change tracking complexity | High | Start with simple modseq, iterate. Stalwart reference is clear. |
| JSCalendar/JSContact spec maturity | Medium | RFC 8984 and RFC 9553 are published standards. Stalwart implements them. |
| Sieve interpreter complexity | Medium | Implement minimal subset (RFC 5228 core) or use simple JSON rule engine instead. |
| SMTP delivery edge cases | Medium | Use mail-send for delivery — it handles SMTP quirks. Simple retry queue. |
| No third-party client compat | Low | By design — we build our own clients (cosmix-web + cosmix-mail). |
| Migration from stalwart data | Low | Stalwart has `--export` which produces importable dumps. |

## Relationship to existing crates

- **cosmix-mail** (`crates/cosmix-mail/`) — remains the libcosmic desktop mail client (JMAP client). Talks to cosmix-jmap server.
- **cosmix-jmap** (`crates/cosmix-jmap/`) — new crate, the JMAP server. Replaces stalwart.
- **cosmix-web** (`crates/cosmix-web/`) — webmail UI (HTMX). Talks to cosmix-jmap server.

## Success criteria

After all phases:

1. cosmix-web provides full webmail UI via JMAP — read, send, flag, move, search
2. cosmix-web provides calendar + contacts UI via JMAP Calendars/Contacts
3. cosmix-mail (libcosmic) provides native COSMIC desktop mail client via JMAP
4. Sieve rules filter incoming mail server-side
5. File storage via JMAP blob/file methods
6. `cosmix.port("jmap")` enables Lua scripting of mail/calendar/contact operations
7. RSS < 40 MB, binary < 12 MB
8. All accounts and mail migrated from stalwart
