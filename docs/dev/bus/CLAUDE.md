# CLAUDE.md — markc/bus

Guidance for Claude Code sessions working in `$COSMIX/`.

## What this repo is

The CosMix Agent Bus library family. Five crates: `cosmix-lib-bus` (wire format), `cosmix-lib-client` (broker WebSocket client, native + wasm32), `cosmix-lib-props-core` (SPEC 07 property read surface), `cosmix-lib-log` (tracing/stats), `cosmix-lib-buildinfo` (build metadata).

bus is the *protocol layer* — every byte that travels between Bus peers is defined here. It deliberately holds no substrate (storage, TLS, auto-resolve, config-file loaders). Anything that needs files, sockets beyond the broker WebSocket, or persistent state belongs in [cos](https://github.com/markc/cos), not here.

## Four-repo split

Part of the Cosmix four-repo constellation (extracted 2026-05-29). One-way dependency order — **bus ← mix ← cos** (and bus ← cos directly); the private **cosmix** hub orchestrates all three and is depended on by none.

| Repo | Path | Visibility | Role |
|---|---|---|---|
| **bus** | `$COSMIX/` | public · markc/bus | Bus protocol family — `cosmix-lib-bus`, `cosmix-lib-client`, `cosmix-lib-props-core`, `cosmix-lib-log`, `cosmix-lib-buildinfo` (5 crates). Depends on nothing. |
| **mix** | `$COSMIX/` | public · markc/mix | Mix language — `cosmix-lib-mix` + `cosmix-mix` + `mix-bench` (3 crates). Depends on bus. |
| **cos** | `$COSMIX/` | public · markc/cos | Substrate libraries + daemon family (27 crates). Depends on bus + mix. |
| **cosmix** | `$COSMIX/` | private · markc/cosmix | Orchestration hub: docs, specs, journals, mesh-private operational state, deploy scripts. No code; drives the three public repos. |

**→ This repo is `bus`** — the protocol layer; it builds standalone, no sibling repos required.

## Layout

```
$COSMIX/src/
├── Cargo.toml                          workspace (5 members)
└── crates/
    ├── cosmix-lib-bus/                 Bus wire format
    ├── cosmix-lib-buildinfo/           build metadata
    ├── cosmix-lib-client/              broker WebSocket client
    ├── cosmix-lib-log/                 tracing/stats init
    └── cosmix-lib-props-core/          SPEC 07 read surface
```

## Build / test / lint

bus builds standalone — no sibling repos required.

```sh
cd $COSMIX/src
cargo build --workspace
cargo test --workspace
cargo clippy --workspace --all-targets -- -D warnings
```

The zero-warning baseline is enforced: any new clippy warning is a regression.

**On the workstation build host, wrap `cargo build`/`cargo test` in `memguard`** (`~/.mc/_bin/memguard.mix`,
on PATH) — caps the build in a `MemoryMax=48G` systemd user scope so a runaway parallel
build OOMs its own scope, not the desktop (an unguarded overnight build caused the
2026-07-07 kernel OOM storm). Exit 137/143 = cgroup OOM → retry with `-j8`.

## Internal dep graph

- `cosmix-lib-bus` → no internal deps.
- `cosmix-lib-client` → `cosmix-lib-bus` (sibling path).
- `cosmix-lib-props-core` → `cosmix-lib-bus` (sibling path, optional under the `bus` feature).
- `cosmix-lib-log` → `cosmix-lib-client` (sibling path, optional under the `bus-handlers` feature).
- `cosmix-lib-buildinfo` → no internal deps.

External consumers (mix, cos, third-party agents) path-dep or version-dep these five crates; bus never depends back.

## What goes here, what doesn't

✅ **Belongs in bus:**
- Bus wire format additions (new message kinds, new field codecs).
- Broker client primitives (`NodedClient` surface, reconnect strategy, request/reply correlation).
- SPEC 07 read-surface types and the Bus-wire dispatcher.
- Standalone unit tests + doctests; manual broker-acceptance examples under `examples/`.

❌ **Doesn't belong in bus:**
- Storage backends, audit, persistence — those live in cos's `cosmix-lib-props-store`.
- TLS, ACME, SNI, certificate machinery — cos's `cosmix-lib-daemon` (tls feature).
- TOML / config-file loaders, broker URL auto-resolution from `node.toml` — cos's `cosmix-lib-config` (`client_helpers` feature).
- Anything that needs a `cosmix-noded` binary at build time (it's runtime-only; the protocol library compiles without one).

If a contribution would force a dep on cos, mix, or any sibling repo outside this workspace, it's in the wrong repo.

## Versioning

Each crate carries its own `version` in its `Cargo.toml`. Path-dep consumers (mix, cos) follow whatever's on `main`; version bumps become load-bearing once the crates publish to crates.io.

## License

MIT.
