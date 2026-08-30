# cos

**cos** is the cosmix daemon family: every Rust crate that runs a node in a cosmix mesh — broker, mail, web, DNS, knowledge indexer, display compositor, agent runtime, MCP bridge — plus the substrate libraries they share.

27 workspace members: 13 substrate libraries, 13 daemon-family crates (long-running daemons plus their helper / CLI subcrates), and 1 mesh-aware app (`cosmix-mail`).

## What's in here

### Substrate libraries (`src/crates/cosmix-lib-*`)

| Crate | What it is |
|---|---|
| `cosmix-lib-config` | `node.toml` loader, per-daemon settings, opt-in `client-helpers` for broker auto-resolve. |
| `cosmix-lib-daemon` | Daemon framework: `init_tracing`, `http_host`, graceful shutdown; optional `tls` feature (rustls + ACME + SNI resolver). |
| `cosmix-lib-props-store` | SPEC 12 substrate surface — sqlite / memory storage backends, audit HMAC, namespace lifecycle. Pairs with `cosmix-lib-props-core` in [bus](https://github.com/markc/bus). |
| `cosmix-lib-mesh`, `cosmix-lib-mesh-trust` | Mesh peering + cross-mesh authz. |
| `cosmix-lib-log` | SPEC 12 `log` namespace + per-daemon `LogHandle::attach_props` watcher. |
| `cosmix-lib-node-id`, `cosmix-lib-wg` | Node identity + WireGuard config helpers. |
| `cosmix-lib-skills`, `cosmix-lib-agent`, `cosmix-lib-llm` | Agent runtime building blocks. |
| `cosmix-lib-display` | Display-protocol consumer (markdown-over-Bus UI). |
| `cosmix-lib-dns` | Authoritative DNS codec library used by `cosmix-dnsd`. |

### Daemons + helpers (`src/crates/cosmix-*d`, `cosmix-mcp`, `cosmix-claud`, `cosmix-mds`, `cosmix-maild-*`)

Long-running daemons:

| Binary | What it does |
|---|---|
| `cosmix-noded` | Bus broker. Every mesh node runs one. |
| `cosmix-maild` | JMAP-native mail daemon (SMTP + IMAP + JMAP + Bayesian classifier). |
| `cosmix-webd` | Multi-vhost HTTPS + ACME. |
| `cosmix-dnsd` | Authoritative WG-mesh DNS. |
| `cosmix-indexd` | Vector indexer / knowledge base. |
| `cosmix-disp-skia` | Display-protocol Skia compositor (winit + softbuffer + tiny-skia + cosmic-text). |
| `cosmix-agentd` | Agent supervision. |

Helpers + adapters (not long-running daemons themselves):

| Crate | What it is |
|---|---|
| `cosmix-mcp` | Claude Code MCP bridge — exposes cosmix surfaces as MCP tools. |
| `cosmix-claud` | Claude SDK agent runtime adapter. |
| `cosmix-mds` | Mail data store helper used by `cosmix-maild`. |
| `cosmix-maild-auth` | Authentication library for `cosmix-maild` (no binary). |
| `cosmix-maild-rules` | Filter engine library + CLI subcrate. |
| `cosmix-maild-bayesian` | Spam classifier library + CLI subcrate. |

### Apps (`src/crates/cosmix-mail`)

`cosmix-mail` — a mesh-aware JMAP mail client. Reads / composes / sends via a local or remote `cosmix-maild`; renders through `cosmix-disp-skia`.

### Desktop (`desktop/`)

CosMix Desktop is the native windowed application family. It has a separate
Cargo workspace at `desktop/Cargo.toml`, so the headless `src/` workspace does
not pull the Bevy dependency tree. The shared workspace contains:

- `ctk` — the CosMix Tool Kit used by every native desktop surface.
- `cosmix-studio` — CosMix Studio, the recording-studio/DAW application that
  drives `musicd`.
- `cosmix-filemgr` — CosMix FileMgr, the twin-pane file manager distinct from
  `filesd`.

Application identity and slug allocation are defined in
[`desktop/APPS.md`](desktop/APPS.md). Desktop builds share
`desktop/target/`:

```sh
cd $COSMIX/src/desktop
cargo build --workspace
cargo test --workspace
```

## Building

cos needs two sibling repos: [bus](https://github.com/markc/bus) (the Bus protocol library) at `$COSMIX/`, and [mix](https://github.com/markc/mix) at `$COSMIX/` (the strict-data parser in `cosmix-lib-mix` is consumed by five cos-side crates: `cosmix-lib-config`, `cosmix-lib-dns`, `cosmix-claud`, `cosmix-mcp`, `cosmix-maild`).

```sh
git clone https://github.com/markc/bus $COSMIX
git clone https://github.com/markc/mix $COSMIX
git clone https://github.com/markc/cos $COSMIX
cd $COSMIX/src && cargo build --workspace --release
```

Tests:

```sh
cd $COSMIX/src && cargo test --workspace
```

Lints:

```sh
cd $COSMIX/src && cargo clippy --workspace --all-targets -- -D warnings
```

## Installing

Daemon binaries install to `/opt/cosmix/bin/`:

```sh
sudo install -m 0755 $COSMIX/src/target/release/cosmix-noded /opt/cosmix/bin/
sudo install -m 0755 $COSMIX/src/target/release/cosmix-maild /opt/cosmix/bin/
# ... repeat per binary
```

SPEC-10 daemon identities (system users / UIDs) install from the bundled sysusers fragment:

```sh
sudo install -m 0644 $COSMIX/src/_etc/sysusers/cosmix.conf /usr/lib/sysusers.d/cosmix.conf
sudo systemd-sysusers --create
```

Per-daemon systemd units are not currently shipped in this repo (only the SPEC-10 sysusers fragment is). Operators provide their own units, or generate them from each daemon's `cosmix-X --help` and the user-name list in the sysusers fragment.

## Configuration

Each daemon reads `/etc/cosmix/node.toml` (system) or `~/.config/cosmix/node.toml` (per-user) for shared node config, plus its own per-daemon config under `/etc/cosmix/<daemon>/config.toml` where applicable. `cosmix-lib-config` defines the schema; see each daemon's source for the per-daemon block.

## Related projects

- **[bus](https://github.com/markc/bus)** — the CosMix Agent Bus library family that every cos daemon links against.
- **[mix](https://github.com/markc/mix)** — the ARexx-flavoured scripting language with first-class Bus keywords; the natural way to script + orchestrate cos daemons.

## License

MIT. See `LICENSE`.
