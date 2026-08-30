# cosmix-dnsd

`cosmix-dnsd` is an authoritative DNS daemon for mesh-local zones. It serves
the same static zone snapshot over UDP and TCP, reloads that snapshot without
discarding the last-known-good state, and optionally exposes read-only Bus
status. It belongs to the `cos` daemon layer in the `bus <- mix <- cos`
dependency chain: the citizen build uses Bus libraries directly, while zone
data reaches the daemon through the `cosmix-lib-dns` substrate.

## Synopsis

```text
cosmix-dnsd --zones <zones.mix> --state <state-file> \
  --listen <ip:port> [--listen <ip:port> ...] \
  [--allow-non-loopback-listen]
```

```text
cosmix-dnsd --version
cosmix-dnsd --help
```

The daemon requires a zone file, a state file, and at least one explicit
listen address. Each address receives both a UDP socket and a TCP listener.

See [configuration.md](configuration.md) for the command-line, zone file,
state, and bind rules.

## Build modes

| Cargo feature | Default | Effect |
|---|---:|---|
| `cosmix` | yes | Builds the Cosmix citizen with node configuration, Bus registration, response counters, shared daemon logging, bind retry, and a best-effort loopback listener. |

Build with `--no-default-features` for the standalone server. That build keeps
the authoritative DNS service but omits citizen configuration, Bus, response
statistics, bind retry, and the implicit loopback listener.

## DNS service

The daemon loads a `StaticZoneStore` from `--zones` and persists serial-floor
state through `FilePersistence` at `--state`. All explicit listeners serve the
same shared snapshot.

The tested service behaviour includes:

- authoritative `A`, `MX`, `SRV`, `PTR`, apex `SOA`, and apex `NS` answers;
- in-zone address glue for `MX` and `SRV` targets;
- `NXDOMAIN` with an authority-section `SOA` for absent names;
- `NOERROR` with an empty answer and authority-section `SOA` for NODATA;
- `REFUSED` without authoritative data for out-of-zone queries;
- a minimal empty answer for `ANY`;
- UDP and DNS-over-TCP framing; and
- EDNS0 response support when the request carries an OPT record.

The crate is authoritative-only. It does not provide recursive resolution.

## Reload and state

`SIGHUP` asks the zone store to reload `zones.mix`. A valid candidate replaces
the current snapshot. A rejected candidate is logged and the daemon continues
serving the last-known-good snapshot.

A corrupt or unreadable state file is logged at startup and treated as absent;
the daemon continues. An invalid initial zone file with no usable last-known-
good state is a fatal startup error.

`Ctrl-C` aborts the serving and Bus tasks, then exits successfully.

## Bus service

The default citizen build registers the Bus service name `dnsd`. It provides
exactly two read-only actions:

| Action | Result |
|---|---|
| `dnsd.zone.snapshot` | Configuration hash, zone count, and sorted zone names. |
| `dnsd.stats` | Monotone response counts grouped by DNS response code. |

Unknown actions and write-shaped actions return caller-error code `10`. DNS
serving does not depend on broker availability; Bus registration retries with
bounded exponential backoff.

See [bus-verbs.md](bus-verbs.md) for response bodies and counter semantics.

## Bind policy

Wildcard addresses such as `0.0.0.0` and `::` are always rejected. Loopback is
always accepted.

The standalone build accepts another non-loopback address only when
`--allow-non-loopback-listen` is present. It logs that the address cannot be
identified as a mesh address.

The citizen build reads `wg_ip` from `node.conf.mix`. When that value is
usable, only the configured address or loopback is accepted; the command-line
override does not bypass this check. If node configuration is unavailable or
invalid, the daemon logs the degraded state and applies the standalone rule.

The citizen retries an address-not-available bind failure for a bounded startup
window. Other bind failures fail immediately. It also attempts
`127.0.0.1:53` as a best-effort listener unless that address was explicitly
requested.

## Internal modules

| Module | Build | Purpose |
|---|---|---|
| `main` | all | Argument parsing, bind validation, zone loading, socket tasks, reload, and shutdown. |
| `citizen` | `cosmix` | Daemon identity reporting, node configuration, address canonicalisation, and citizen bind policy. |
| `bus` | `cosmix` | Bus registration, reconnect loop, and read-only action dispatch. |
| `stats` | `cosmix` | Lock-free response-code counters used by `dnsd.stats`. |
| `bind_retry` | `cosmix` | Bounded retry for UDP and TCP binds when the address is not yet available. |

This package exports a binary, not a Rust library API.

## Exit status

| Status | Meaning |
|---:|---|
| `0` | Version output or orderly `Ctrl-C` shutdown. |
| `1` | Zone startup, socket bind, signal handler, or other runtime failure. |
| `2` | Invalid arguments or a rejected listen address. |

`--help` prints usage and exits with status `2`.

## Dependencies

The always-built path uses `cosmix-lib-dns`, Tokio, and tracing. The `cosmix`
feature adds configuration, Bus client and message types, build provenance,
shared logging, JSON response construction, and direct Hickory protocol access.

The build script records package build provenance for citizen Bus
registration.

## Tests

The integration prober starts the binary on loopback with a temporary zone and
state file. It sends real UDP and TCP queries and verifies authoritative
answers, negative responses, refusal, minimal `ANY`, and EDNS0 behaviour.

Unit tests cover citizen address policy, daemon identity consistency, Bus
dispatch, response JSON, and response-code accounting.
