# cosmix-webd

`cosmix-webd` is the CosMix web daemon: an Axum-based, host-routed HTTP and HTTPS server with static-file delivery, a SQLite CMS API, trusted embedded Mix handlers, JMAP and broker WebSocket proxies, mail-client autoconfiguration, and per-vhost TLS lifecycle management. It belongs to the `cos` layer of the `bus <- mix <- cos` dependency chain. It consumes Bus citizenship and client crates directly, embeds the Mix evaluator for HTTP handlers, and uses the shared Cos daemon, configuration, property-store, and logging substrates.

## Synopsis

```text
cosmix-webd <COMMAND>
```

The binary provides server, database, certificate, vhost, ACME, route, statistics, TLS, and autoconfiguration commands.

See [commands and configuration](commands-and-configuration.md) for the full command and configuration surface.

See [Bus verbs](bus-verbs.md) for the daemon's Bus service and SPEC 12 property namespaces.

## What it provides

- Host-routed HTTP and HTTPS serving for multiple virtual hosts.
- Per-listener host allowlists and separate TLS identity buckets.
- Static-file fallback rooted at each vhost's `www_dir`.
- A SQLite-backed posts API and session-revocation store.
- Trusted, operator-authored `.mix` request handlers.
- Per-route Mix capabilities for database, JMAP, network, delegated Bus, auxiliary database, public-read, public-cache, and accelerator-wake access.
- JMAP HTTP proxying and broker WebSocket proxying.
- Markdown documentation rendering below `/docs`.
- Mail-client autoconfiguration with a served-domain admission allowlist.
- Manual PEM TLS and ACME HTTP-01 certificate provisioning.
- Runtime listener guards, kill switches, TLS status, response counters, and live property publication.

## HTTP surface

Known hosts receive the per-vhost router. Unknown hosts return `404`; missing or malformed `Host` headers return `400`.

| Path | Methods | Purpose |
|---|---|---|
| `/api/posts` | `GET`, `POST` | List or create CMS posts |
| `/api/posts/{id}` | `GET`, `PUT`, `DELETE` | Read, update, or delete one post |
| `/jmap` | Any | Proxy a JMAP request to the vhost upstream |
| `/jmap/{path}` | Any | Proxy a JMAP sub-path |
| `/.well-known/jmap` | Any | Proxy JMAP session discovery |
| `/auth/login` | `GET`, `POST` | Session login |
| `/auth/login/verify` | `POST` | Email second-factor verification |
| `/auth/logout` | `POST` | Session logout |
| `/portal/auth/login` | `POST` | Customer portal login |
| `/portal/auth/set-password` | `POST` | Customer password setup |
| `/portal/auth/change-password` | `POST` | Customer password change |
| `/admin/media/upload` | `POST` | CMS image upload |
| `/admin/media/delete` | `POST` | CMS image deletion |
| `/ws` | `GET` | Broker WebSocket proxy |
| `/docs` | `GET` | Markdown documentation index |
| `/docs/{path}` | `GET` | Render a Markdown document |
| `/assets/{path}` | `GET` | Serve an asset |
| all other paths | Any | Match a Mix handler, then fall back to static files |

The autoconfiguration branch adds:

| Path | Methods | Purpose |
|---|---|---|
| `/mail/config-v1.1.xml` | `GET` | Mozilla mail autoconfiguration |
| `/.well-known/autoconfig/mail/config-v1.1.xml` | `GET` | Well-known mail autoconfiguration |
| `/autodiscover/autodiscover.xml` | `POST` | Reserved; currently returns `404` |

A plain-HTTP listener also serves ACME HTTP-01 challenges and redirects admitted non-autoconfiguration requests to HTTPS.

## Embedded Mix handlers

The `webd.handlers` namespace maps a method and path pattern to a `.mix` file below the vhost's `www_dir`. Exact paths and one trailing `/*` glob are supported. `ANY` matches every request method.

Handlers receive request globals including `$METHOD`, `$PATH`, `$QUERY`, `$HOST`, `$BODY`, `$HEADERS`, `$SIGNALS`, `$SESSION`, and `$CSRF` when the corresponding state exists.

A handler returns one of:

- A string, served as `200 text/html`.
- Raw bytes, preserved for binary responses.
- A map containing `status`, `headers`, and `body`.
- Printed output, used as the response body.

The default sandbox permits pure operations and filesystem reads. Filesystem writes, process execution, environment access, shell syntax, and pipes remain denied. Route capabilities selectively add database, JMAP, Bus, or outbound-network access.

Evaluation has a 1 MiB request-body cap, a five-second deadline, the Mix default recursion limit of 16, collection limits of 100,000 entries, and a 4 MiB string limit. Parsed handler ASTs are cached and refreshed when the script modification time changes.

## Persistence

`cosmix-webd init` creates the legacy database schema. It contains:

- `posts`, the CMS post table.
- `session_epochs`, per-account counters used to invalidate sealed session cookies.

Production serving also stores SPEC 12 property rows in the configured web database. Per-vhost CMS databases are optional and may attach named auxiliary databases.

## Virtual hosts and listeners

Each configured vhost has its own static root and optional CMS database, JMAP upstream, broker WebSocket upstream, documentation directory, and authentication credentials. Primary hosts and configured aliases share runtime state and counters.

Explicit listeners partition vhosts by interface. Each listener has a stable ID, bind address, external/internal classification, enabled state, host allowlist, connection limits, rate limit, CIDR policy, and strict-SNI setting.

Listener state is persisted in `webd.listeners`. Configuration seeds new rows; later runtime changes remain authoritative across restarts.

## TLS

Manual TLS uses a certificate and private-key path. Public-facing manual chains are validated for the configured names before serving. Internal-only listeners may use an internal or self-signed certificate.

ACME mode supports production and staging providers with HTTP-01. The provisioner performs startup recovery, issuance, renewal, backoff, atomic staging and promotion, archive pruning, resolver hot-swap, and status publication.

Manual-PEM deployments can reload certificate contents without restarting through `webd.tls.reload`. Validation failure keeps every previous resolver active.

## Bus citizenship

The daemon registers the Bus service name `webd`. Broker connection failures and mid-session disconnects enter an exponential reconnect loop capped at 60 seconds. HTTP serving and ACME renewal continue while the Bus surface is offline.

The service exposes read-only snapshots, ergonomic lifecycle verbs, listener controls, session revocation, TLS reload, and the generic `webd.props.*` family. Property changes publish on `webd.props.records.changed`; watch grants are refreshed after broker reconnection.

## Cargo features

This crate defines no Cargo features. Bus citizenship and the embedded Mix runtime are unconditional dependencies.

## Main source modules

| Module | Responsibility |
|---|---|
| `main` | CLI, configuration resolution, HTTP routes, runtime assembly, and serving |
| `bus` | Bus registration, reconnect loop, dispatch, verbs, and property publication |
| `vhosts_namespace` | `webd.vhosts` schema, policy, validation, and events |
| `handlers_namespace` | `webd.handlers` schema and route-table reload signalling |
| `listeners_namespace` | `webd.listeners` schema, guards, status, and control events |
| `mix_handler` | Trusted Mix route matching, sandboxing, evaluation, and response mapping |
| `acme_provisioner` | ACME issuance, renewal, recovery, persistence, and TLS publication |
| `vhost_directory` | Atomically published host-routing directory |
| `listeners_reaction` | Applies persisted listener changes to the live listener set |
| `session` | Sealed session cookies and CSRF tokens |
| `db` | Capability-scoped SQLite access for Mix |
| `jmap_handler` | Capability-scoped JMAP calls for Mix |
| `file_share` | Tokenised file-share records and path confinement |
| `media` | Authenticated CMS image storage |
| `mxresolve` | Cached MX lookup and implicit-TLS probes |
| `public_response_cache` | Short-lived anonymous response cache |

## Package metadata

The crate builds one binary, `cosmix-webd`. Its package description is “Lightweight web server daemon — axum-based CMS API backed by SQLite”.
