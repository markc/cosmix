# bus — CosMix Agent Bus

Documentation for the **bus** protocol library family: the pure-Rust crates
that define Bus messaging between named, agent-operable services on a
WireGuard-secured mesh. bus sits at the base of the `bus <- mix <- cos`
dependency chain — the Mix language and the cos substrate consume its wire
messages, client, and support libraries, while bus itself depends on nothing
above it. The broker and the operating substrate live elsewhere, in cos.

This section holds one guide page per protocol concern plus a reference README
per crate. All links are relative, so the index works when browsed on GitHub,
from a local clone, or through the consolidated cosmix.dev docs application.

## Guides

| Page | Description |
|---|---|
| [overview](overview.md) | What Bus is, the crate family, the protocol boundary, and how bus fits with mix and cos. |
| [wire format](wire-format.md) | `BusMessage`: the frame, ordered headers, message types, addresses, and return codes — usable without a broker. |
| [client](client.md) | `NodedClient`: building and correlating Bus requests over WebSocket, native and `wasm32` targets, supervised reconnect. |
| [property core](props-core.md) | The pure-type half of the SPEC 07 property surface: typed dotted property trees without storage, hooks, or the mutation router. |
| [build information](buildinfo.md) | Compile-time package, version, git revision, dirty-tree state, and build timestamp for fleet inventory and service registration. |
| [logging](log.md) | The common logging bootstrap: CLI options, filters, stderr/file/journald sinks, live reload, and the core metrics recorder. |

## Crates

| Crate | Role |
|---|---|
| [cosmix-lib-bus](cosmix-lib-bus/README.md) | Protocol-layer crate defining the Bus wire format, messages, addresses, discovery records, and native Unix-socket IPC primitives; base of the `bus <- mix <- cos` chain. |
| [cosmix-lib-client](cosmix-lib-client/README.md) | Async Bus WebSocket client library for connecting Cosmix apps to a `cosmix-noded` broker; native (Tokio) and `wasm32` client implementations plus a reconnecting `SupervisedClient`. |
| [cosmix-lib-props-core](cosmix-lib-props-core/README.md) | Pure Rust types and read-side helpers for the SPEC 07 property surface: paths, values, schemas, snapshots, redaction, diffs, revisioned writes; optional Bus dispatch/publish via the `bus` feature. |
| [cosmix-lib-buildinfo](cosmix-lib-buildinfo/README.md) | Dependency-free leaf library recording compile-time build provenance (version, git state, build time) for Cosmix daemons and other Rust consumers. |
| [cosmix-lib-log](cosmix-lib-log/README.md) | Shared logging/process-stats library: reloadable tracing subscriber, stderr/file/journald sinks, cardinality-bounded metrics recorder with JSONL, Bus, and Prometheus surfaces. |
