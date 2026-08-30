# AGENTS.md — markc/cos

Guidance for Codex sessions working in `$COSMIX/`.

For historical fixes and project-specific feedback, use `$cos-memory`. Read its index first and only targeted entries.

## What this repo is

The cosmix daemon family + substrate libraries. 27 workspace members organised in three groups:

- **Substrate libraries** (`cosmix-lib-*`, 13 crates) — code that runs inside daemons but isn't a daemon itself: config loaders, daemon framework, prop storage, mesh peering, logging, DNS codec, agent runtime helpers, display protocol.
- **Daemon-family crates** (13 members) — 7 long-running daemons (`cosmix-noded`, `cosmix-maild`, `cosmix-webd`, `cosmix-dnsd`, `cosmix-indexd`, `cosmix-disp-skia`, `cosmix-agentd`) that hold SPEC-10 identities; plus 6 helper/CLI/adapter crates (`cosmix-mcp`, `cosmix-claud`, `cosmix-mds`, `cosmix-maild-auth`, `cosmix-maild-rules`, `cosmix-maild-bayesian`) that link into the daemons or expose subcommand binaries.
- **Apps** (`cosmix-mail`, 1 crate) — headless Bus citizens that render their UI through `cosmix-disp-skia`.

## Four-repo split

Part of the Cosmix public four-repo constellation (extracted 2026-05-29). One-way code dependency order — **bus ← mix ← cos** (and bus ← cos directly). The public **cosmix** repo holds the umbrella specifications and build harness; the separate private `~/.cmctl` checkout holds real operational state.

| Repo | Path | Visibility | Role |
|---|---|---|---|
| **bus** | `$COSMIX/` | public · markc/bus | Bus protocol family — `cosmix-lib-bus` + `cosmix-lib-client` + `cosmix-lib-props-core` (3 crates). Depends on nothing. |
| **mix** | `$COSMIX/` | public · markc/mix | Mix language — `cosmix-lib-mix` + `cosmix-mix` + `mix-bench` (3 crates). Depends on bus. |
| **cos** | `$COSMIX/` | public · markc/cos | Substrate libraries + daemon family (27 crates). Depends on bus + mix. |
| **cosmix** | `$COSMIX/` | public · markc/cosmix | Sanitised umbrella specs, decisions, and build harness. No application code or private mesh state. |

**→ This repo is `cos`** — the daemon family + substrate libraries; needs bus + mix present as sibling checkouts to build.

## Layout

```
$COSMIX/src/
├── Cargo.toml                  workspace (27 members)
├── _etc/
│   └── sysusers/
│       ├── cosmix.conf                   SPEC-10 daemon identities (UIDs 500+)
│       └── cosmix-nodeexport-foreign.conf  foreign-host node_exporter UID
└── crates/
    ├── cosmix-lib-*/           substrate libraries
    ├── cosmix-{noded,maild,webd,...}/  daemons
    └── cosmix-mail/            mesh-aware app
```

`_etc/sysusers/cosmix.conf` is normative: every daemon's UID assignment comes from there. The `cosmix-dnsd` test `spec10_identity_matches_checked_in_sysusers_fragment` cross-checks in-code SPEC-10 constants against this file — keep them in sync.

## Build / test / lint

cos has two cross-repo dependencies:
- [bus](https://github.com/markc/bus) at `$COSMIX/` — for `cosmix-lib-bus`, `cosmix-lib-client`, `cosmix-lib-props-core`.
- [mix](https://github.com/markc/mix) at `$COSMIX/` — for `cosmix-lib-mix` (used by `cosmix-lib-config::mix_data` for the strict-data parser).

Both must be present as sibling checkouts under `$HOME`:

```sh
cd $COSMIX/src
cargo build --workspace
cargo test --workspace
cargo clippy --workspace --all-targets -- -D warnings
```

The zero-warning baseline is enforced.

For native CosMix Desktop applications, every successful release build must
also install the resulting executable from the shared `desktop/target/` into
`~/.local/bin/` (for example,
`install -m 0755 desktop/target/release/cosmix-filemgr ~/.local/bin/cosmix-filemgr`).

**Do NOT wrap `cargo` in `memguard` as an agent** (dropped 2026-08-29). Memory caps
belong to the unit that runs the build (a build-cluster worker or a `MemoryMax`-capped
systemd unit), not to a wrapper the agent adds; inside the codex sandbox `systemd-run
--user` cannot reach the bus, so memguard either refuses to run the gate (foreman task
80: five tasks committed uncompiled code in one day) or falls through unguarded anyway.
Run plain `cargo build`/`cargo test`. Exit 137/143 = the unit cgroup OOM-killed the
build — report it, do not retry blind. (`cosmix-foreman` itself is halted as of
2026-08-30 until further notice; its fleet units no longer exist.)

## Cross-repo dep direction

- `cos → bus` (one-way) — every cos crate that talks Bus path-deps `cosmix-lib-bus` / `cosmix-lib-client`; daemons that own a SPEC 12 namespace also path-dep `cosmix-lib-props-core`.
- `cos → mix` (one-way) — five cos crates path-dep `cosmix-lib-mix`: `cosmix-lib-config`, `cosmix-lib-dns`, `cosmix-claud`, `cosmix-mcp`, `cosmix-maild`.
- cos never depends on the public cosmix specification hub, the private cmctl overlay, or any mesh-private substrate context.

## Where to put new functionality

| Want to add … | Goes in |
|---|---|
| New daemon | `crates/cosmix-<name>d/` — new workspace member; add a row to `_etc/sysusers/cosmix.conf` (next free UID in the 500-599 daemon band) and bump the SPEC-10 version in `cosmix-dnsd/src/citizen.rs` (the test enforces alignment). |
| New Bus method on an existing daemon | That daemon's `bus/` or `*.rs` handler module — most daemons follow a `register_*` pattern. |
| New SPEC 12 property namespace | The daemon's `props/` module + `register_namespace` call at daemon startup. Schema declared with `cosmix-lib-props-store`'s `NamespaceSpec`. |
| New shared library code | `crates/cosmix-lib-<name>/` if non-trivial; otherwise an existing `cosmix-lib-*` crate where the functionality fits. |
| Anything wire-format / broker-client / SPEC 07 read surface | **Don't add here** — those live in [bus](https://github.com/markc/bus). |
| Interpreter / Mix language features | **Don't add here** — those live in [mix](https://github.com/markc/mix). |

## What goes here, what doesn't

✅ **Belongs in cos:**
- Daemon implementations + their substrate libraries.
- Per-daemon storage backends, audit, lifecycle machinery.
- TLS / ACME / SNI machinery (cosmix-lib-daemon's `tls` feature).
- TOML / config-file loaders (`cosmix-lib-config`).
- Broker URL auto-resolve helpers (`cosmix-lib-config::client_helpers`).
- Display-protocol consumers (`cosmix-lib-display`, `cosmix-disp-skia`).

❌ **Doesn't belong in cos:**
- Bus wire format, `BusMessage`, `NodedClient` — those live in [bus](https://github.com/markc/bus).
- Mix interpreter / lexer / evaluator / builtins — those live in [mix](https://github.com/markc/mix).
- Per-host mesh-deployment artifacts (hostnames, mesh IPs, delta/epsilon-specific configs, proxmox-specific tokens) — operational state, not cos identity. Test fixtures using public-domain examples (`example.com`) are fine.
- Public project-mandate docs, decisions, and specs live in `$COSMIX`; private journals and operational direction live in `~/.cmctl`, not in cos.

## Versioning

Each crate carries its own `version`. Daemon binaries follow their own semver cycles (e.g. `cosmix-maild` 0.1.0 is independent of `cosmix-webd` 0.1.0). Substrate libraries that a daemon API-depends on bump together with the daemon when the surface changes.

## License

MIT.

## No real environment values — ever

This is a PUBLIC repository. Never copy a real username, home directory,
hostname, domain, IP, or a Claude-projects transcript slug (the encoded
`-home-<user>--<repo>` form) from the machine you are running on into code,
tests, fixtures, docs or commit messages — not even as test data. Use
placeholders: `/home/alpha`, `-home-alpha--<repo>`, `example.org`, RFC 5737
addresses, node names alpha/beta/gamma. A pre-commit gate refuses commits
that carry real values, so the commit fails in your run; fix the fixture,
do not work around the gate. Production code must derive paths from `$HOME`
/ XDG at runtime, never embed them.
