# Substrate libraries

The `cosmix-lib-*` crates are the shared logic every cos daemon links against.
They follow a **core-first, citizen-on-feature** rule: a new library defaults to
a pure-logic *core* with no Cosmix-internal dependencies, so
`cargo test --no-default-features` is its full test surface. Bus / mesh / config
integration sits behind a feature (usually `cosmix`) that pulls the protocol
crates. Inherently-mesh libraries are exempt. This keeps most substrate logic
mesh-free and fast to test — the granularity an agent-modifiable system needs.

The Bus protocol crates (`cosmix-lib-bus`, `-client`, `-props-core`, `-log`,
`-buildinfo`) live in the sibling [bus](https://github.com/markc/cosmix) repo; the
Mix language crates live in [mix](https://github.com/markc/cosmix). The crates below
are the cos-side substrate.

## Substrate

| Crate | What it is |
|---|---|
| `cosmix-lib-config` | Typed per-service settings. Each service owns a `*Settings` struct backed by a `~/.config/cosmix/<service>.conf.mix` file; a missing file is materialised with defaults. Opt-in `client-helpers` feature adds broker auto-resolve from `node.toml`. |
| `cosmix-lib-daemon` | Daemon framework shared by every long-running daemon: log-dir path, graceful-shutdown signal, HTTP host, and an optional `tls` feature (rustls + ACME + SNI resolver). |
| `cosmix-lib-props-store` | The SPEC 12 substrate mutation surface — namespace specs, lifecycle, records + events, hooks, capabilities, the in-memory and SQLite storage backends, per-row audit HMAC, and the mutation router. Pairs with `cosmix-lib-props-core` (the SPEC 07 read surface) in the bus repo. |
| `cosmix-lib-log-props` | The SPEC 12 `<svc>.log` namespace + a per-daemon `LogHandle` watcher that mirrors log config through the property surface. |
| `cosmix-lib-files` | Pure core of the files-as-truth markdown corpus manager (daemon: `cosmix-filesd`). Surgical byte-preserving frontmatter writer, atomic write-then-rename, BLAKE3 content hashing, UUIDv7 identity, `[[wikilink]]`/link extraction, index SQL schema, and the reconcile diff. Also the generic live-filesystem layer for the file manager. |

## Mesh & identity

| Crate | What it is |
|---|---|
| `cosmix-lib-mesh` | Bus mesh networking over a WireGuard overlay — peer management, WebSocket bridge connections to remote hubs, and interface-status queries. |
| `cosmix-lib-mesh-trust` | Cross-mesh trust + grant verification. Core: envelope parse, Ed25519 verify, SPEC 13 signed-inventory verification, freshness, and capability-bag math against passed-in fixtures. The `cosmix` feature adds the `AuthPolicy` combinator and the namespace-mirroring cache. |
| `cosmix-lib-node-id` | Stable 5-hex-char node identifier derived from `/etc/machine-id`. Intentionally tiny, no Cosmix-internal deps. |
| `cosmix-lib-wg` | Pure-logic primitives for the WireGuard daemon: key material, interface/peer value types, config rendering, IPAM, `wg show` parsing, netlink message construction. No syscalls beyond CSPRNG seeding. |

## Agent runtime

| Crate | What it is |
|---|---|
| `cosmix-lib-llm` | Generic multi-backend LLM client — Anthropic, OpenAI-compatible (OpenAI/vLLM/LMStudio), Ollama, and Bus (route through `cosmix-claud` on the mesh). One `LlmClient::from_config` entry point. |
| `cosmix-lib-agent` | The agent loop: drives multi-turn conversations against an `LlmClient`, dispatching model tool calls through a pluggable `ToolRegistry`; sessions persist as JSONL. Backs [agentd](agentd.md). |
| `cosmix-lib-skills` | The skill-learning loop — evaluate an interaction, extract a reusable skill, retrieve relevant skills, and refine confidence over time. Backs the knowledge augmentation in `cosmix-claud`. |

## Protocol codecs

| Crate | What it is |
|---|---|
| `cosmix-lib-dns` | Pure core of the authoritative WG-mesh DNS daemon (`cosmix-dnsd`): zone model, `.mix` zone loader, hickory-proto codec, resolver, and serve loops. Two-layer data model separates owner-attributed source from the flattened, served snapshot. No Bus/mesh/config deps. |
| `cosmix-lib-davproto` | CalDAV/CardDAV codecs shared by the maild DAV server and a future DAV client: JSCalendar↔iCalendar (RFC 8984/5545), JSContact↔vCard 4.0 (RFC 9553/6350), and strong content-hash ETags (RFC 4791/6352). |

## See also

- [overview](overview.md) — the substrate at a glance
- [noded](noded.md) — the Bus broker every node runs
- [desktop](desktop.md) — the CosMix desktop (compositor, toolkit, apps)
- [agentd](agentd.md) — agent supervision (consumes `cosmix-lib-agent` / `-llm`)
