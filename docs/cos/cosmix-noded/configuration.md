# cosmix-noded configuration

`cosmix-noded` loads the required node configuration, applies command-line overrides, and then starts its broker and optional client modules.

## Startup

```text
cosmix-noded
cosmix-noded serve [OPTIONS]
```

Omitting `serve` has the same effect as specifying it.

The daemon fails if the required node configuration cannot be loaded, an explicit mesh configuration cannot be read, or the broker address cannot be bound.

## Effective settings

The crate reads these values from the node configuration:

| Value | Use |
|---|---|
| Node name | Broker identity and the default mesh configuration selector. |
| WireGuard or mesh address | Admission boundary self-check. |
| Noded listen address | WebSocket bind address when `--listen` is absent. |
| Noded mesh configuration path | Mesh peer configuration when `--mesh-config` is absent. |
| Noded admission mode | Selects `off`, `observe`, or `enforce`. |

The node configuration loader is required even when command-line overrides provide the node name and listen address.

The exact node configuration grammar belongs to the configuration substrate. This crate consumes the loaded values but does not define that grammar.

## Command-line precedence

| Effective value | First choice | Fallback |
|---|---|---|
| Listen address | `--listen` | Derived noded listen address from node configuration. |
| Node name | `--node` | Node configuration node name. |
| Mesh configuration path | `--mesh-config` | Optional noded mesh path from node configuration. |
| Specification directory | `--spec-dir` | Environment and working-directory discovery. |

`--no-monitor` and `--no-log` are process-local switches. They do not alter the loaded node configuration.

## Listen address

The listen value is a socket address such as:

```text
192.0.2.10:4200
```

The broker serves WebSockets at:

```text
ws://192.0.2.10:4200/ws
```

The built-in monitor and logger connect to that local URL after the listener reports readiness.

Admission enforcement checks that the listen address uses the node's configured mesh address. A wildcard, loopback, malformed, or different address causes enforcement to adopt a refuse-all posture for inter-node sessions. The process still starts so the posture remains observable.

## Mesh configuration

When a mesh configuration path is present, the daemon loads that file. Otherwise it asks the mesh substrate for the default configuration associated with the effective node name.

The resulting configuration supplies the local mesh identity and peer roster used for message routing and `noded.peers`.

## Admission mode

| Mode | Behaviour |
|---|---|
| `off` | Sends no admission challenge and preserves open registration behaviour. |
| `observe` | Challenges peers, verifies available proofs, and records verdicts without refusing registration. |
| `enforce` | Requires valid proof for inter-node registration and checks the proven node against the registered bridge identity. |

Same-node connections are not proof-gated.

Enforce mode refuses all inter-node registrations if no verified trust root is loaded or the listener is not bound to the configured mesh address.

## Authority files

The authority plane uses fixed runtime paths:

| Path | Purpose |
|---|---|
| `/etc/cosmix/noded/genesis.pub` | Provisioned Ed25519 genesis verification key. |
| `/var/lib/cosmix/noded/inventory.signed` | Cached signed authority inventory. |
| `/var/lib/cosmix/noded/inventory.baseline` | Persisted anti-rollback baseline. |
| `/etc/cosmix/noded/d2.seed` | Base64-encoded 32-byte admission signing seed. |

Missing, malformed, stale, or unverifiable authority data produces an unverified posture. It does not stop broker startup.

The daemon watches the directory containing `inventory.signed`. A verified reload replaces the live authority snapshot. A bad or partial reload does not replace a previously verified snapshot.

A missing admission seed leaves the node unable to prove itself to a peer. An unreadable or malformed seed also disables proving and produces a warning.

## Specification directory

The specification directory is resolved in this order:

1. `--spec-dir <PATH>`.
2. `COSMIX_SPEC_DIR`.
3. `_spec` below `COSMIX_SRC`.
4. An `_spec` directory found while walking from the current directory towards its parents.

If no directory is found, the broker still starts. `spec.get` reports that the directory is not configured, and no `world.specs.NN` retained topics are seeded.

Canonical chapter files use the `NN_*.md` form. Files with letters after the numeric prefix, such as `01b_*.md`, are not used as canonical chapter entries.

## Logging

The broker reports `RUST_LOG` as `config.log_level` on its property surface. When the variable is unset, that property reports `cosmix_noded=info`.

The logger module creates the daemon log directory supplied by the daemon substrate and appends traffic records to `bus.log`. Each record contains a local timestamp, sender, command, and body length. It does not write the message body to that file.

Use `--no-log` to prevent the logger module and its traffic-tap subscription from starting.

## Monitor

Use `--no-monitor` to prevent the monitor module from starting. The broker continues to run, but the `mon` service and all `mon.*` verbs are unavailable.

## Shutdown

The daemon listens for the shared daemon shutdown signal. It also treats an unexpected broker task exit as a reason to stop. Shutdown aborts the monitor and logger tasks before the process exits.
