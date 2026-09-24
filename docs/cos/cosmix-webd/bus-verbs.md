# Bus verbs

`cosmix-webd` registers the Bus service name `webd`. The service reconnects after initial broker failure or a later stream end. HTTP serving and certificate renewal do not depend on the Bus session remaining online.

Successful verbs return `rc=0`. Caller, validation, authorisation, and unsupported-action failures use `rc=10` with a JSON error body.

## Read-only snapshots

| Verb | Arguments | Result |
|---|---|---|
| `webd.routes.list` | None | Sorted vhosts with aliases and `has_cms`, `has_jmap`, `has_ws`, and `has_docs` flags |
| `webd.stats` | None | Sorted per-vhost response-class counters |
| `webd.tls.status` | None | Manual identity names, ACME plan summary, disabled vhosts, and per-vhost issuance state |
| `webd.autoconfig.served_domains` | None | Sorted mail-autoconfiguration admission domains |

These snapshots are built from atomically published runtime views. Route and statistics results contain one record per primary vhost; aliases share the primary's state.

## Vhost verbs

| Verb | Required arguments | Optional arguments | Capability |
|---|---|---|---|
| `webd.vhost.add` | `fqdn`, `www_dir` | `enabled`, ACME trio, or manual TLS pair | `props.write:webd.vhosts` |
| `webd.vhost.remove` | `fqdn` | None | `props.write:webd.vhosts` |
| `webd.vhost.list` | None | None | `props.read:webd.vhosts` |

`webd.vhost.add` accepts dotted or underscored names for:

- `acme.provider` or `acme_provider`
- `acme.challenge` or `acme_challenge`
- `acme.contact_email` or `acme_contact_email`
- `tls.cert_path` or `tls_cert_path`
- `tls.key_path` or `tls_key_path`

Dotted names take precedence. `enabled` accepts `true`, `false`, `1`, or `0` and defaults to true.

The add verb stamps `source = "bus_runtime"` and uses a tombstone-aware version anchor. It supports re-adding a previously removed FQDN.

Runtime rows survive restarts, so every write to `webd.vhosts` is checked against the `node.conf.mix` the next restart will read. The check lives in the namespace hooks, so it covers `webd.vhost.add` and `webd.vhost.remove` and also raw `webd.props.set` and `webd.props.delete`. The hooks re-read the config from disk on every guarded write.

On a node with an explicit `[[webd.listener]]` array, a write that makes a row enabled is refused unless exactly one enabled listener lists the FQDN in its `vhosts`, once. "Makes enabled" means a new row, a re-add over a removed one, or a flip from `enabled = false`. The refused cases are: no listener names the host, only a disabled listener names it, or it appears in two allowlist slots. Each would make the next restart abort and take every vhost on the node down. A refused write stores nothing: no row, no tombstone, no provisioner event. Writes to a row that is already enabled are not re-checked, so a later config edit never blocks a certificate writeback. Rows sourced from `[[webd.vhost]]` blocks are validated by startup itself and are not checked here. A node without explicit listeners serves every host, so the check always passes there.

A disabled row needs no listener. At startup, rows the routing directory does not serve are skipped in listener resolution, just like vhosts whose configuration failed to load. That covers disabled rows and runtime rows whose `www_dir` does not exist. A listener naming one of them no longer aborts the boot.

Removing a row is refused while a listener still names the FQDN and no `[[webd.vhost]]` block defines it as a host or alias. Otherwise the listener would point at an unknown vhost and abort the next boot. Drop the host from the listener first.

The config edit and the write must happen back to back, with no restart between them. Adding a host to a listener's `vhosts` before its row exists, then restarting, still aborts the boot on an unknown vhost.

Refusals are validation errors with rc 10. If the config file does not load, the write is refused with rc 20, a node fault, because the next restart would fail anyway.

On a node with explicit listeners, a runtime-added host is not served until the next restart, because each listener's allowlist is read at startup. The add reply then carries `"served_after": "restart"`.

After a restart, webd loads the on-disk certificate of every runtime-added ACME vhost before the listeners bind. The first handshake gets the vhost's own certificate, not another vhost's. A listener whose only TLS hosts are runtime-added comes up with TLS when those certificates load. Each listener keeps its `strict_sni` policy across these reloads and across renewals. Rows with no servable certificate on disk are logged at startup and issued once the listeners are bound. After that, every 6-hour renewal tick retries them once their cooldown has passed.

All of this needs the certificate provisioner. The provisioner only exists on a node whose `node.conf.mix` declares at least one ACME vhost, and which has at least one TLS listener at startup. On a node where every ACME vhost is runtime-added, those certificates are neither issued nor loaded.

The verbs cannot see a front proxy. On a node behind an nginx SNI router, the router's `stream` map entry and its `:80` `server_name` entry still have to be added by the operator.

The list verb adds a derived `acme_status` to each row. Secret fields are returned only when the caller has the secret-read capability.

## ACME verbs

| Verb | Arguments | Capability |
|---|---|---|
| `webd.acme.renew` | `fqdn` | `webd.acme.renew:webd.vhosts` |
| `webd.acme.status` | `fqdn` | `props.read:webd.vhosts` |

`webd.acme.renew` queues an immediate sweep and returns `state = "pending"`. It bypasses cooldown and the normal renewal window. It rejects missing, disabled, non-ACME, or unattached vhosts.

`webd.acme.status` returns:

- `fqdn`
- derived `acme_status`
- `not_after`
- `last_attempt`
- `last_error_count`
- `last_error`
- `next_attempt_after`
- `issued`

`last_error` is redacted unless the caller has `props.read:webd.vhosts:secrets`.

## Listener verbs

| Verb | Arguments | Capability |
|---|---|---|
| `webd.listener.enable` | `id` | `props.write:webd.listeners` |
| `webd.listener.disable` | `id` | `props.write:webd.listeners` |
| `webd.listener.status` | Optional `id` | `props.read:webd.listeners` |

Enable and disable update the persisted `enabled` field. The listener reaction loop performs the bind or unbind asynchronously and writes observed state back. Read `webd.listener.status` to confirm `bound`.

Disabling a non-external listener is rejected to preserve the control path.

Status returns the listener ID, enabled and external flags, bind state, active connections, strict-SNI state, limits, CIDR lists, and last transition. `last_error` requires `props.read:webd.listeners:secrets`.

## TLS reload

| Verb | Arguments | Capability |
|---|---|---|
| `webd.tls.reload` | None | `props.write:webd.listeners` |

The verb is available on manual-PEM deployments. It:

1. Reads each configured certificate and key again.
2. Validates each certificate against every name it covers.
3. Builds all replacement SNI resolvers.
4. Swaps the resolvers only if every read, validation, and build succeeds.
5. Republishes the TLS status snapshot.

Failure leaves the previous resolvers serving. ACME-managed deployments refresh their resolver set through the provisioner.

## Session revocation

| Verb | Arguments | Capability |
|---|---|---|
| `webd.session.revoke` | `email` | `webd.session.revoke:webd.vhosts` |

The verb increments the account's `session_epochs` row in every vhost CMS database. Cookie-authorised requests compare the sealed epoch with the current value, so the increment invalidates outstanding web cookies.

The response reports successful vhosts, per-vhost failures, and the number of vhosts without a CMS database. A failure on one vhost does not hide successful revocations on others.

This verb revokes the web cookie path only. It does not revoke bearer tokens owned by another daemon.

## Property verbs

Every command below the `webd.props.*` prefix is passed to the daemon's SPEC 12 property router.

The crate registers:

| Namespace | Primary key | Purpose |
|---|---|---|
| `vhosts` | `fqdn` | Vhost declaration and TLS lifecycle state |
| `handlers` | `route_id` | Embedded Mix route declarations |
| `listeners` | `id` | Listener controls, guards, and observed state |
| `log` | Namespace-defined | Live logging filter |

The source names the following generic verbs:

- `webd.props.get`
- `webd.props.list`
- `webd.props.set`
- `webd.props.delete`
- `webd.props.watch`
- `webd.props.audit.watch`

Property requests identify a namespace and, where applicable, a key. The crate-defined collection namespaces require optimistic-concurrency versions for writes.

`webd.props.watch` and `webd.props.audit.watch` establish broker topic grants. Committed changes are published on `webd.props.records.changed`; dispatcher tasks survive broker reconnects and use the refreshed client handle.

## Capability policy

`webd.vhosts` grants public read, secret read, describe, audit, write, ACME-renew, and session-revoke capabilities under its current peer policy.

`webd.handlers` grants read, write, describe, and audit capabilities under its current peer policy.

`webd.listeners` deliberately differs:

- Read, describe, and audit are available under the normal peer policy.
- Write and secret-read capabilities require the caller's Bus service name to appear in `listeners.operators`.
- An empty operator list grants no remote listener writes.

Daemon-origin writes do not pass through caller capability resolution. They remain subject to schema and cross-field validation.

## Property ownership

Caller writes cannot set daemon-owned fields.

For `webd.vhosts`, daemon-owned fields are `source`, `cert_blob_id`, `key_blob_id`, `not_after`, `last_attempt`, `last_error_count`, and `last_error`.

For `webd.listeners`, daemon-owned fields are `external`, `bound`, `bound_addr`, `active_conns`, `last_transition`, and `last_error`.

The vhost schema also enforces:

- Canonical FQDN and row-key agreement.
- Empty aliases on the runtime property surface.
- ACME provider, challenge, and contact completeness.
- Manual certificate and key completeness.
- Mutual exclusion between ACME and manual TLS.
- A disabled state when no TLS mode is present.

The handler schema enforces:

- A known primary vhost.
- A supported method and `mix` handler kind.
- An absolute request path pattern with at most one trailing glob.
- A relative `.mix` handler reference with no parent traversal.
- Known capability tokens and explicit delegated-verb allowlists.

## Unknown verbs

An unsupported `webd.*` command returns `rc=10`, identifies the action, and reports the fixed read-only snapshot list. Mutation verbs are accepted only by their explicit asynchronous dispatchers or the property router.
