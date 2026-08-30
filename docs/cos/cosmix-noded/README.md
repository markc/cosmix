# cosmix-noded

`cosmix-noded` is the consolidated CosMix node daemon. It runs a WebSocket Bus broker, a system-monitor service, and a Bus traffic logger in one process. The crate belongs to the `cos` layer of the `bus <- mix <- cos` dependency chain: it consumes Bus protocol and client crates from `bus`, uses daemon and mesh substrate from `cos`, and has no direct dependency on `mix`.

## Synopsis

```text
cosmix-noded [serve] [OPTIONS]
cosmix-noded --help
cosmix-noded --version
```

`serve` is the only subcommand and is also the default when no subcommand is given.

## Description

The daemon starts the broker first and waits until its listener is bound. It then starts the monitor and logger as independent asynchronous tasks unless either module is disabled.

The broker accepts WebSocket connections at `/ws`. It registers local services, routes Bus requests and responses, bridges messages to configured mesh peers, manages retained topics and subscriptions, distributes specification chapters, and exposes node state through Bus property verbs.

Per-peer outbound queues are bounded. A slow peer loses messages after its queue fills instead of causing unbounded memory growth. Inbound WebSocket messages are limited to 16 MiB and individual frames to 8 MiB.

## Components

| Component | Registered name | Purpose |
|---|---|---|
| Broker | `noded` | Routes Bus traffic, owns node and topic verbs, and bridges mesh peers. |
| Monitor | `mon` | Reports host status, processes, and system properties. |
| Logger | `log` | Subscribes to the local traffic tap and appends a compact event record to `bus.log`. |

The monitor and logger connect back to the broker as ordinary Bus clients. The logger uses a separate anonymous connection for the traffic tap so tap backpressure cannot block its property service.

## Command-line options

| Option | Meaning |
|---|---|
| `--listen <ADDR>` | Override the broker listen address. |
| `--node <NAME>` | Override the node name. |
| `--mesh-config <PATH>` | Override the mesh configuration file. |
| `--no-monitor` | Do not start the `mon` service. |
| `--no-log` | Do not start the `log` service. |
| `--spec-dir <PATH>` | Set the directory containing specification chapters. |

The version string includes the crate version, source revision, and build time.

See [configuration.md](configuration.md) for precedence, environment variables, and runtime files.

## Bus surface

The broker provides these command families:

| Family | Purpose |
|---|---|
| `noded.*` | Registration, discovery, health, peer inventory, traffic tap, and authority state. |
| `topic.*` | Publish, subscribe, unsubscribe, inspect, and clear retained topics. |
| `noded.props.*` | Read and describe broker properties. |
| `props.watch` | Subscribe to `noded.props.changed`. |
| `spec.get` | Read a specification chapter by number or filename. |
| `mon.*` | Read system status and top processes. |
| `mon.props.*` | Read and describe monitor properties. |
| `log.props.*` | Read and describe logger properties and counters. |
| `ui.subscribe`, `ui.unsubscribe` | Maintain UI-event subscription records; event routing is not implemented. |

See [verbs.md](verbs.md) for the command arguments, results, retained topics, and access restrictions.

## Broker behaviour

Connections may register one service name with `noded.register`. Registered names are unique. Re-registering the same connection under a different name removes its old binding, while a collision with another live connection is rejected.

Requests addressed to a local registered service are forwarded over that service's WebSocket. Request identifiers are replaced with broker-local identifiers while in flight and restored on the response path, so independent callers may reuse their own local identifiers safely.

Messages addressed through the mesh layer are passed to `MeshPeers`. Cross-mesh targets are refused because federation transport is not implemented.

The broker rewrites a registered local sender's `from` header to its connection-bound service name before forwarding. It removes `from` from anonymous and mesh-originated traffic.

## Topics

Topic payloads are Bus messages carried inside `topic.publish`. The broker treats their application payload as opaque, but parses the Bus envelope and injects `topic`, `topic_seq`, `topic_stale`, and `topic_op` headers.

Retained snapshots default to enabled and are limited to 1 MiB. A new subscriber receives the latest retained snapshot when one exists. A snapshot becomes stale when its publisher disconnects and is removed after a 60-second grace period; the janitor checks every 10 seconds.

Topic names beginning with `$` are reserved. Property event topics also have owner and visibility restrictions enforced by the broker.

## Properties and retained state

The `noded` property tree reports:

- broker bind address, node name, and log level;
- process start time, uptime, health, and property conformance level;
- registered service names and count;
- active topic count and retained snapshot bytes.

Changes are published on `noded.props.changed`. A full broker snapshot is retained on `world.noded`.

When a specification directory is available, each canonical `NN_*.md` chapter is retained on `world.specs.NN`. `spec.get` returns the same chapter content directly.

The monitor property tree reports lifecycle data, CPU, memory, swap, load averages, selected disks, and its default process-list limit. The logger property tree reports its log path, tap state, event count, and bytes written.

## Authority and admission

The authority module verifies a cached signed inventory against a provisioned genesis public key and maintains an anti-rollback baseline. Verification failure produces an `unverified` posture instead of stopping the broker.

Inventory changes are watched and hot-reloaded. A failed reload does not replace a previously verified inventory. In enforcement mode, a verified membership revocation closes affected admitted sessions.

Admission supports `off`, `observe`, and `enforce` modes. Observe mode challenges and records verdicts without refusing a session. Enforce mode requires a valid inter-node proof and fails closed when the listener is not bound to the configured mesh address or no verified trust root is available. Same-node connections are not admission-gated.

The admission exchange uses broker-issued `noded.admit.challenge` frames and client `noded.admit.response` frames. These are session-handshake messages rather than general application RPCs.

## Modules

| Module | Responsibility |
|---|---|
| `noded` | WebSocket broker, routing, registry, mesh ingress and egress, command dispatch, and admission enforcement. |
| `subscription` | Topic snapshots, fan-out, replay, subscriber lifecycle, filtering, and UI subscription records. |
| `authority` | Signed-inventory verification, accepted member summaries, and rollback baseline handling. |
| `admission` | Challenge state and reconstruction of signed admission transcripts. |
| `props` | Broker property tree and change publication. |
| `props_reservation` | Ownership rules for property event topics. |
| `monitor`, `mon_props` | Host metrics, process reporting, and monitor properties. |
| `logger`, `log_props` | Bus traffic logging and logger properties. |
| `spec` | Specification discovery, parsing, lookup, and retained publication. |

## Cargo features

This crate defines no Cargo feature flags.

## Exit and shutdown

The process waits for a daemon shutdown signal or an unexpected broker exit. On shutdown it aborts the monitor and logger tasks and exits. Failure to load the required node configuration or bind the broker listener is fatal.
