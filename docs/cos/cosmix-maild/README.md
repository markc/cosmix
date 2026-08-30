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

Mail metadata and operational state use SQLite. Mailbox content uses `cosmix-mds` through `SqliteMailStore`. The runtime also starts upload-expiry, IMAP retraining, rule-stat flush, retention, SMTP delivery, Bus, and protocol listener tasks as applicable.

Rule statistics are diagnostic counters, not Bayesian training data. Their SQLite store uses periodic snapshots and does not perform a final graceful-shutdown flush.
