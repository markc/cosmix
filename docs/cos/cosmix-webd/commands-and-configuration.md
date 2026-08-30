# Commands and configuration

`cosmix-webd` is a subcommand-driven daemon binary. Server flags override matching values resolved from `node.conf.mix`.

## Commands

| Command | Purpose |
|---|---|
| `cosmix-webd serve` | Start production serving |
| `cosmix-webd init` | Initialise the SQLite CMS schema |
| `cosmix-webd mkcert` | Create a self-signed EC P-256 certificate |
| `cosmix-webd vhost` | Add, list, show, or remove vhost property rows |
| `cosmix-webd acme` | Force renewal or inspect one ACME vhost |
| `cosmix-webd routes list` | Print the current route-map snapshot |
| `cosmix-webd stats` | Print per-vhost response counters |
| `cosmix-webd tls status` | Print TLS identity and ACME status |
| `cosmix-webd tls reload` | Reload manual PEM files atomically |
| `cosmix-webd autoconfig served-domains` | Print the autoconfiguration allowlist |

`--version` includes the package version, source revision, and build time.

## Serve

```text
cosmix-webd serve [OPTIONS]
```

| Option | Meaning |
|---|---|
| `--listen <ADDR>` | Override the primary listen address |
| `--http-listen <ADDR>` | Add a plain-HTTP redirect and ACME listener |
| `--www-dir <PATH>` | Override the legacy static root |
| `--db-path <PATH>` | Override the legacy SQLite path |
| `--jmap-upstream <URL>` | Override the legacy JMAP upstream |
| `--noded-ws <URL>` | Override the legacy broker WebSocket URL |
| `--docs-dir <PATH>` | Serve Markdown files below `/docs` |
| `--static-dir <PATH>` | Enter loopback-only static preview mode |
| `--tls-cert <PATH>` | Override the legacy certificate PEM path |
| `--tls-key <PATH>` | Override the legacy private-key PEM path |

The normal resolution order is CLI, node configuration, then the source-defined default. The database defaults below the CosMix variable-data directory. The legacy static root defaults to its `www` subdirectory.

`--http-listen` is opt-in. Without a CLI value or configured value, no secondary plain-HTTP listener is created.

### Static preview

```text
cosmix-webd serve --static-dir ./site
```

Static preview bypasses node configuration, SQLite, Bus, ACME, TLS, vhost registration, Mix handlers, the CMS API, JMAP, and WebSocket proxying. It serves static files plus the Markdown and asset routes.

The default address is `127.0.0.1:8080`. An explicit `--listen` must resolve entirely to loopback addresses and must provide an IPv4 loopback result.

## Initialise a database

```text
cosmix-webd init [--db-path <PATH>]
```

The command creates parent directories and applies the `posts` and `session_epochs` schema with `CREATE TABLE IF NOT EXISTS`.

## Create a local certificate

```text
cosmix-webd mkcert <FQDN> <CERT-PEM> <KEY-PEM>
```

The command creates a self-signed EC P-256 certificate for an internal or `.localhost` vhost. The FQDN is used for the common name and DNS subject alternative name; the certificate also contains a loopback IP subject alternative name.

## Manage vhosts

```text
cosmix-webd vhost add <FQDN> <WWW-DIR> [OPTIONS]
cosmix-webd vhost list
cosmix-webd vhost show <FQDN>
cosmix-webd vhost remove <FQDN>
```

`vhost` commands connect to the running daemon through the default broker.

`vhost add` accepts:

| Option | Meaning |
|---|---|
| `--acme-provider <NAME>` | Select an ACME provider |
| `--acme-challenge <TYPE>` | Select the ACME challenge |
| `--acme-contact-email <ADDRESS>` | Set the ACME account contact |
| `--tls-cert-path <PATH>` | Set a manual certificate path |
| `--tls-key-path <PATH>` | Set a manual private-key path |
| `--disabled` | Create the row with `enabled = false` |

The ACME provider, challenge, and contact fields form one mode. Manual certificate and key fields form the other mode. The modes are mutually exclusive, and certificate/key half-pairs are rejected.

`vhost remove` reads the current row version before deleting it. Missing rows are treated as an idempotent success; concurrent changes require a retry.

## ACME, routes, statistics, TLS, and autoconfiguration

```text
cosmix-webd acme renew <FQDN>
cosmix-webd acme status <FQDN>
cosmix-webd routes list
cosmix-webd stats
cosmix-webd tls status
cosmix-webd tls reload
cosmix-webd autoconfig served-domains
```

`acme renew` bypasses timing gates but does not bypass a disabled vhost or other policy gates. It returns after queuing the renewal; use `acme status` to observe the outcome.

`tls reload` is available only when manual PEM identities own the live listener resolvers. It validates all replacement material before swapping any resolver.

## Node configuration consumed by the daemon

The crate loads `node.conf.mix` through `cosmix-lib-config`. Its source directly consumes the following web configuration fields.

| Field | Use |
|---|---|
| `www_dir` | Legacy static root |
| `http_listen` | Optional redirect and HTTP-01 listener |
| `tls_cert` | Legacy manual certificate path |
| `tls_key` | Legacy manual private-key path |
| `tls_server_name` | Legacy host-routing and certificate names |
| `served_mail_domains` | Mail autoconfiguration admission allowlist |
| `autoconfig_mail_host` | Optional advertised mail host override |
| `vhost` | Per-vhost configuration rows |
| `listener` | Per-interface listener rows |
| `listeners.operators` | Bus service names allowed to mutate listener state |

The daemon also resolves the node's web listen address, JMAP upstream, and broker URL through configuration helper methods.

## Per-vhost configuration

Each `[[webd.vhost]]` row supplies:

| Field | Use |
|---|---|
| `host` | Primary hostname |
| `aliases` | Additional hostnames sharing the vhost state |
| `www_dir` | Required static root |
| `tls_cert`, `tls_key` | Manual TLS pair |
| `acme` | ACME provider, challenge, and contact configuration |
| `cms_db_path` | Optional per-vhost CMS SQLite file |
| `aux_dbs` | Named auxiliary SQLite files attached to the CMS connection |
| `jmap_upstream` | Optional JMAP proxy target |
| `noded_ws` | Optional broker WebSocket proxy target |
| `docs_dir` | Optional Markdown document root |
| `dev_session_email`, `dev_session_password` | Paired internal-development auto-session credential |
| `public_read_email`, `public_read_password` | Paired anonymous content-read credential |
| `system_sender_email`, `system_sender_password` | Paired transactional-mail credential |
| `mfa_break_glass` | Permit password-only login when second-factor state is indeterminate |

Hostnames are normalised and checked for duplicates before filesystem mutation. `www_dir` must exist. Operational failures for one vhost disable that vhost while healthy vhosts continue; an empty healthy set still prevents startup.

Manual TLS and ACME are mutually exclusive. A public-facing manual certificate is validated against the primary name and aliases. A vhost with a development auto-session must be assigned only to explicit internal listeners with internal bind addresses.

Auxiliary database paths must be absolute and unique within the vhost. They require `cms_db_path`. Schema names match `[a-z_][a-z0-9_]{0,31}` and cannot be `main` or `temp`.

## Listener configuration

Each explicit `[[webd.listener]]` row supplies:

| Field | Use |
|---|---|
| `id` | Stable listener identifier |
| `bind` | Socket address |
| `external` | Public/untrusted-facing classification |
| `enabled` | Initial enabled state |
| `vhosts` | Vhosts assigned to the listener |

Configuration validates unique IDs and binds, assigns every served host to one enabled listener, and rejects wildcard/specific clashes. Without explicit rows, the daemon creates one implicit internal listener at the resolved primary address and assigns every host to it.

Configuration seeds `enabled` and guard state only when a `webd.listeners` property row is absent. Existing property state wins across restarts. The daemon always refreshes its own `external` field from configuration.

## Property-backed configuration

The daemon registers four property namespaces:

- `webd.vhosts` for vhost identity, static root, TLS mode, and provisioner status.
- `webd.handlers` for embedded Mix routes.
- `webd.listeners` for listener guards, kill switches, and live status.
- `webd.log` for live log-filter control.

The three crate-defined namespaces use optimistic concurrency. Writes require the current row version.

See [Bus verbs](bus-verbs.md) for namespace keys, capabilities, ownership rules, lifecycle verbs, and listener lockout guards.
