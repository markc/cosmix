# cosmix-maild

`cosmix-maild` is the Cosmix mail, calendar, and contacts daemon. It serves JMAP, SMTP, IMAP, CalDAV, and CardDAV; owns account and mail-domain state; and exposes administration through a command-line interface and Bus. It belongs to the `cos` daemon layer in the `bus ← mix ← cos` dependency chain: it uses Bus client and wire libraries directly, and uses the Mix interpreter for strict-data configuration and inbound routing scripts.

## Package

The Cargo package and binary are named `cosmix-maild`. The library import name is `cosmix_maild`.

The crate has no crate-defined Cargo features.

## What it provides

- A JMAP server for core, mail, submission, calendar, contacts, and vacation-response operations.
- SMTP inbound delivery, implicit-TLS submission, outbound queuing and delivery, STARTTLS, DKIM signing, and mail-auth verification.
- An IMAPS server with password authentication, mailbox management, message fetch and mutation, search, copy, move, expunge, and IDLE.
- CalDAV and CardDAV discovery, property queries, reports, object reads, writes, and deletes.
- Basic and bearer-token HTTP authentication.
- SQLite metadata, content-addressed mailbox storage through `cosmix-mds`, blob storage, search indexing, retention, and Bayesian retraining.
- A SPEC 12 property substrate for accounts, aliases, domains, engine settings, retention, TLS identities, and per-account rules overrides.
- Bus verbs for administration, diagnostics, statistics, reloads, and token management.
- A reusable production runtime for integration tests and embedding.

See [cli.md](cli.md) for command syntax and [bus.md](bus.md) for the Bus surface.

## Network surfaces

### JMAP and HTTP

The HTTP listener serves:

| Route | Purpose |
|---|---|
| `GET /.well-known/jmap` | Authenticated JMAP Session resource |
| `POST /jmap` | JMAP method endpoint |
| `GET /jmap/blob/{blobId}` | Account-scoped blob download |
| `POST /jmap/upload/{accountId}` | Account-scoped blob upload |
| `GET /jmap/eventsource` | JMAP state-change event stream |
| `POST /auth/tokens/issue` | Exchange Basic credentials for a bearer token |
| `POST /auth/tokens/verify` | Verify a bearer token |
| `POST /auth/tokens/revoke` | Revoke a bearer token |

Implemented JMAP method families are `Mailbox`, `Email`, `Thread`, `EmailSubmission`, `Identity`, `Calendar`, `CalendarEvent`, `AddressBook`, `Contact`, and `VacationResponse`. `Core/echo` is also available.

#### MIME projections (0.10.0)

`Email/get` derives `hasAttachment`, `attachments`, `textBody`, `htmlBody` and
requested `bodyValues` from one bounded MIME inspection. A filename or an
`attachment` disposition marks an attachment; an inline part with a filename
also qualifies. Embedded-message children are inspectable but are not outer
message body parts or duplicate outer attachments.

All projections use the same part paths: root `1`, children `1.1`, `1.2`, and
so on. An embedded message's root appends `.1`. These replace the old sequential
text-only IDs; part IDs are per-response and must not be persisted by clients.
Part blob IDs are `mp1_<32hex item UUID>_<64hex message hash>_<path with underscores>`.
They are canonical lowercase ASCII and bind a part to the account-owned message
and its current content hash. `Email/import` still accepts upload UUIDs, not part
blob IDs.
Part downloads retain the sender's content type, but force attachment disposition
with an RFC 5987 filename, `X-Content-Type-Options: nosniff` and
`Content-Security-Policy: sandbox`.

Inspection reads at most 64 MiB of raw message, with a 64 MiB decoded-part cap,
depth 32, 1,000 MIME parts and path length 64. These limits apply independently
of the configured inbound message limit, so messages admitted through IMAP can
be inspected. Unreadable storage, corrupt message hashes or over-limit messages return
`hasAttachment: null` and omit `attachments` and body projections; they never
report a false negative or a partial attachment list. Property filtering still
applies. Downloadable parts preserve transfer-decoded octets, including original
text charset bytes; body display values may be charset-converted to UTF-8.

`GET /jmap/blob/{blobId}` accepts these part IDs alongside upload UUIDs and
whole-message hashes. Authentication and account ownership precede file reads.
A foreign item, missing item, stale message hash or absent part returns the
same HTTP 404 `blob not found`. Malformed IDs return HTTP 400 `invalid blob id`;
an inspection limit returns HTTP 413 `too_large: …`; unreadable or corrupt
owned messages return HTTP 500 `unreadable: message`. No part bytes are stored
as new CAS blobs by inspection or download.

Blobd export references (`b3:` IDs) are separate from JMAP part/upload IDs.
Exporting a part or message does not replace its JMAP `blobId`, redirect
`/jmap/blob` to blobd, or change `Email/import`'s upload-UUID input.

### Blob byte lane

Maild uses the local blobd HTTP byte lane, discovered over Bus with
`blob.props.get {"path":"lane"}`. Uploads use owner `maild:<account_id>` and
an advisory `blob.quota` check; unavailable advisory quota does not suppress
the authoritative HTTP admission check. Discovery and quota have 10-second
deadlines, HTTP connects have a 10-second deadline, and upload progress and
response reads have 30-second idle limits with no total transfer deadline.
Redirects, proxies and automatic response decompression are disabled.

Names are hints: non-printable bytes, spaces, percent signs and UTF-8 bytes
are percent-encoded, and the name header is omitted if that exceeds 128
bytes. Blobd's first-writer-wins name and MIME are returned unchanged.
Downloads use only the local lane and verify BLAKE3 and length before use;
an absent local blob is never fetched implicitly from its reference origin.

The Bus verbs `maild.attachment.list`, `maild.attachment.ref` and
`maild.message.ref` inspect or export account-owned mail. Lists and exports run in
eight tracked tasks; a full pool returns `busy:` immediately, and a session
end cancels outstanding transfers. Validated references are saved separately
from messages, including origin and the message hash; repeats reuse the
saved reference. Pins and bookkeeping survive mail/account deletion until
later reconciliation. No detach or message rewrite occurs. `message.ref`
is export, not a new `Email/import` input. See [bus.md](bus.md) for arguments,
reply fields, errors and lifecycle details.

`maild.rules.explain` and `maild.bayesian.classify` accept exactly one of a
local `blob` reference/ID or the retained `message_b64` compatibility input.
Both use the configured `max_message_size` bound (default 25 MiB), and blob
downloads are verified before evaluation. Blob-input diagnostics share the
export task pool; legacy calls retain serial dispatch. Replies retain the
existing verdict/explanation
shape; new clients should carry references over Bus. Missing local blobs
return `not_present:` rather than initiating a mesh fetch.

### SMTP

`smtp_inbound` enables inbound SMTP. `smtp_smtps` enables implicit-TLS authenticated submission. Either setting accepts one listen address or a list.

Inbound delivery resolves local accounts and aliases, applies mail-auth checks, the rules engine, Bayesian classification, optional Mix routing, and retention-related metadata before committing mail to the mailbox store. Remote submissions enter the retry queue and outbound delivery path.

### IMAP

`imap_imaps` enables implicit-TLS IMAP. The default advertised capabilities are:

```text
IMAP4rev2 IMAP4rev1 SASL-IR AUTH=PLAIN AUTH=LOGIN ID
NAMESPACE CHILDREN SPECIAL-USE UNSELECT LITERAL+ UIDPLUS MOVE IDLE
```

The command handlers cover authentication, capability and session control, mailbox listing and mutation, selection and status, fetch, search, flags, store, append, copy, move, expunge, subscriptions, and IDLE.

### CalDAV and CardDAV

The DAV router serves `/.well-known/caldav`, `/.well-known/carddav`, and `/dav/...`. It implements `OPTIONS`, `PROPFIND`, `REPORT`, `GET`, `PUT`, and `DELETE`. DAV data uses the same calendar and contact stores as JMAP.

## Library surface

The library exports the daemon module tree. Important entry points are:

| Module | Main surface |
|---|---|
| `runtime` | `build_runtime`, `RuntimeOpts`, and `BuiltMaild` |
| `config` | `Config`, `ListenSpec`, DKIM config types, config loading, and TLS resolution |
| `jmap` | HTTP handlers, `AppState`, JMAP request dispatch, and state events |
| `smtp` | `SmtpConfig`, `SmtpState`, `SmtpHandle`, and `start` |
| `imap` | IMAP configuration, listener, session, codec, sequence, response, and operation modules |
| `dav` | DAV router and resource routing |
| `mailstore` | `MailStore`, `SqliteMailStore`, query types, records, and retention operations |
| `db` | SQLite connection, migrations, accounts, blobs, calendars, contacts, tokens, and vacation data |
| `props` | Property schemas, hooks, mappings, and namespace registration |
| `bus` | Broker registration, reconnecting dispatch, action handlers, and event publishers |
| `auth` | Basic and bearer-token authentication |
| `tls` | SNI certificate resolver, live TLS slot, and server-config cache |
| `keyword` | Shared IMAP/JMAP user-keyword validation and normalisation |
| `vtoken` | Opaque virtual-address token store and resolver |

`build_runtime(&Config, RuntimeOpts)` opens the stores, registers property namespaces, starts SMTP and IMAP listeners and background workers, and returns the HTTP router. The caller binds the HTTP listener and drives `axum::serve`.

`BuiltMaild` must remain alive while serving. SMTP and worker tasks are detached; process exit is the current shutdown path.

## Configuration

Configuration is strict-data `.conf.mix`, deserialised into `config::Config`. An explicit path is selected with `--config`.

Without `--config`, the binary checks:

1. `/etc/cosmix/maild/config.conf.mix`
2. The Cosmix user configuration path ending in `jmap.conf.mix`
3. Node configuration, converted into maild settings
4. Built-in defaults

Only a missing optional file falls through. Read, parse, permission, and validation failures stop startup.

### Core keys

| Key | Default or state | Purpose |
|---|---|---|
| `listen` | `127.0.0.1:8088` | JMAP and DAV HTTP listen address |
| `base_url` | `http://127.0.0.1:8088` | Public JMAP URL prefix |
| `database_path` | Cosmix variable-data path | SQLite metadata database |
| `blob_dir` | Cosmix variable-data path | Blob storage directory |
| `mds_dir` | Cosmix variable-data path | Mailbox Data Store root |
| `hostname` | `localhost` | SMTP greeting, identity, and legacy TLS name |
| `max_message_size` | 25 MiB at runtime | SMTP message-size limit |
| `inbound_filter` | unset | Mix script used to choose an inbound mailbox |

The inbound filter receives `FROM`, `TO`, `SUBJECT`, `HEADER_FROM`, `SPAM_VERDICT`, and `SPAM_SCORE` globals and returns a mailbox name.

### Listener and TLS keys

| Key | Default or state | Purpose |
|---|---|---|
| `smtp_inbound` | `0.0.0.0:2525` | One or more inbound SMTP binds; unset disables |
| `require_starttls_inbound` | empty | Exact inbound binds that reject mail before STARTTLS |
| `smtp_smtps` | unset | One or more implicit-TLS submission binds |
| `imap_imaps` | unset | One or more implicit-TLS IMAP binds |
| `tls_cert`, `tls_key` | unset | Legacy single-identity certificate and key |
| `tls` | default TLS config | Multi-identity SNI configuration and strict-SNI policy |
| `tls_key_root` | maild variable-data path | Root for substrate-managed TLS PEM files |

If `tls.identity` contains rows, those rows take precedence over `tls_cert` and `tls_key`. Otherwise a complete legacy pair becomes one default identity named by `hostname`. Incomplete TLS material does not produce an identity.

### IMAP keys

| Key | Purpose |
|---|---|
| `imap_max_literal_bytes` | Maximum APPEND literal size |
| `imap_idle_status_interval_secs` | IDLE keepalive interval |
| `imap_pre_auth_timeout_secs` | Pre-authentication idle timeout |
| `imap_max_auth_failures` | Authentication failure cap per connection |
| `imap_max_bad_commands_pre_auth` | Bad-command cap before authentication |
| `imap_max_bad_commands_post_auth` | Bad-command cap after authentication |
| `imap_max_concurrent_per_account` | Concurrent connection cap per account |
| `imap_advertise_capabilities` | Override the advertised capability list |

### Spam and rules keys

| Key | Default or state | Purpose |
|---|---|---|
| `spam_enabled` | `true` | Enable Bayesian spam filtering |
| `spam_db_dir` | Cosmix variable-data path | Per-account Bayesian databases |
| `spam_baseline_db` | unset | Baseline database for new accounts |
| `spam_base_rate_prior` | off | Enable the experimental observed-base-rate prior |
| `spam_base_rate_pseudocount` | engine default | Shrink the observed prior towards 0.5 |
| `spam_base_rate_min`, `spam_base_rate_max` | engine defaults | Clamp the observed prior |
| `rules_pack_path` | unset | Rule-pack file; unset uses the embedded pack |
| `rule_stats_flush_interval_secs` | `60` | Persistent rule-counter flush cadence |
| `rule_stats_dir` | maild variable-data path | Global rule-counter database root |

### Operator allowlists

| Key | Empty-list behaviour |
|---|---|
| `retention_operators` | No Bus peer may run retention |
| `vtoken_operators` | No Bus peer may use the global vtoken management path |
| `vtoken_delegated_peers` | No peer may use delegated vtoken calls |

### DKIM keys

Legacy signing uses `dkim_selector` and `dkim_private_key`.

The `dkim` subsection contains `key_root` and a `domain` list. Each domain row has `domain`, `selector`, `algorithm`, `key_path`, optional `canonicalization`, optional `headers`, `active_for_signing`, and `allow_body_length_tag`.

Supported algorithms are `rsa-sha256` and `ed25519-sha256`. Key files are read and validated at startup. At most one row per domain may be active for signing.

## Property namespaces

The runtime registers:

| Namespace | Shape |
|---|---|
| `maild.accounts` | Account collection; password is secret |
| `maild.account_overrides` | Per-account rule overrides |
| `maild.aliases` | Local single-hop aliases |
| `maild.domains` | Per-domain delivery, identity, DKIM, and policy settings |
| `maild.engine_config` | Required singleton rules-engine configuration |
| `maild.retention` | Inert-by-default retention singleton |
| `maild.tls_identities` | Read-only projection of active TLS identities |
| `maild.log` | Live logging filter configuration |

The retention defaults delete nothing: both age windows are zero, `dry_run` is true, and no accounts are armed.

## Storage and background work

JMAP uploads write only the MDS CAS and an account-scoped, expiring upload
alias. Hash downloads require live mail ownership or a valid alias in the
authenticated account; old uploads retain an account-scoped legacy fallback.
Global CAS presence never grants download access. Legacy UUID downloads and
imports remain supported, and legacy files and database rows are retained.

`maild.blob.migrate` performs bounded, resumable legacy migration inside the
running daemon (dry-run by default). It preserves old upload UUIDs with durable
holding items and backfills old outbound queue entries. Drive it over Bus after
restart; there is no local migration CLI or second writer. See [bus.md](bus.md)
for cursors, per-account counts, refusal tokens and the retained-data contract.

Mail metadata and operational state use SQLite. Mailbox content uses `cosmix-mds` through `SqliteMailStore`. The runtime also starts upload-expiry, IMAP retraining, rule-stat flush, retention, SMTP delivery, Bus, and protocol listener tasks as applicable.

Rule statistics are diagnostic counters, not Bayesian training data. Their SQLite store uses periodic snapshots and does not perform a final graceful-shutdown flush.

## MIME inspection limits

MIME inspection has a hard 64 MiB raw-message cap regardless of
`max_message_size`. Startup logs one warning if the configured admission
limit exceeds it. Larger admitted messages project `hasAttachment: null`
and omit `attachments`; attachment inspection/export returns `too_large:`.
Diagnostic blob inputs still use the configured `max_message_size`.

Before parsing, a linear header-aware scan permits at most 2,000 potential
header blocks and 32 embedded-message media types (`message/rfc822` or
`message/global`). It recognises whitespace in header names, folded values
and comments. Quoted header text in message bodies counts too and can cause a false-positive
`too_large:` refusal. Parse, walk and tree destruction use a dedicated 64 MiB
thread stack. The walk permits depth 32, 1,000 parts, path length 64,
64 MiB per decoded part, an aggregate 128 MiB decoded-byte budget, and at most
two nested encoded re-parses beyond the parser's own encoded nesting limit.

An undecodable part is listed with `undecodable: true` and has no download
ID or exportable bytes. Other parts remain available; body values retain
the parser's recovered display text. Export of that part returns `unreadable:`.
