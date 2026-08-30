# cosmix-wgd

`cosmix-wgd` is the Cos-layer WireGuard mesh control-plane daemon. It verifies this node's signed mesh inventory, derives the peer set that the kernel should contain, reads the live WireGuard state, and reports drift without changing the kernel or inventory. In the `bus <- mix <- cos` dependency chain it belongs to `cos`; it depends directly on Bus client, protocol, build-information, and logging crates, and declares no direct Mix dependency.

## Synopsis

```text
cosmix-wgd [--iface NAME] [--self NAME] [--interval SECONDS]
           [--genesis PATH] [--signed PATH] [--baseline PATH]
           [--once]

cosmix-wgd --version
cosmix-wgd -V
```

`cosmix-wgd` is a binary crate. It does not expose a Rust library API.

The crate is phase P2: derive and dry-run only. It contains no apply path, peer-allocation path, key-rotation writer, inventory writer, or kernel mutation verb.

## Operation

Each reconcile pass performs these operations:

1. Read the genesis public key, signed inventory, and rollback baseline.
2. Verify the signed inventory against the genesis trust state and baseline.
3. Derive the intended peer set for this node.
4. Run `wg show IFACE dump` without invoking a shell.
5. Compare intended peers with the parsed live peer set.
6. Replace the shared snapshot consumed by the Bus handlers.
7. Log whether the interface is in sync or has drift.

Trust or derivation failure leaves the last good snapshot in place. The daemon logs the failure and retries at the next interval.

A live WireGuard read failure is non-fatal. The new snapshot retains the derived intent, records the live error, and remains available through Bus.

## Intended peer derivation

The signed inventory is the membership authority. `cosmix-wgd` does not maintain a parallel membership registry.

Derivation:

- accepts members whose status is `active`;
- skips members whose status is `tombstoned`;
- excludes the local node from its own peer list;
- requires active names and mesh addresses to be unique;
- requires each mesh address to be inside the inventory subnet;
- rejects a WireGuard public key shared by different active members;
- selects `kind: "wg"` credentials valid at the accepted inventory epoch;
- accepts either key during a legitimate credential-overlap window;
- assigns each peer a single `/32` IPv4 or `/128` IPv6 host route;
- sorts peers by mesh address for deterministic output.

An active member without a current WireGuard credential is skipped and recorded as a warning. Malformed membership, an unknown status, duplicate identity data, an invalid subnet, or an absent local member stops that reconcile pass.

## Drift model

The peer mesh address is the stable join key between intent and live state. A public key may rotate, so it is not used as the join key.

The dry-run report classifies:

| Kind | Meaning |
|---|---|
| `missing` | An intended peer has no live peer routing its mesh address. |
| `extra` | A live peer routes no intended mesh address. |
| `key_mismatch` | The live peer routes the right address with no currently accepted key. |
| `allowed_ips_drift` | The peer identity and key match, but `allowed_ips` is not exactly the intended host route. |
| `duplicate_kernel_claimant` | More than one live peer claims the same intended mesh address. |

Endpoint, persistent keepalive, and preshared-key values are not compared. The signed inventory does not provide intended values for those fields.

Correctly configured peers receive a liveness state of `pending`, `connected`, or `offline`, derived from the latest handshake time.

## Bus service

In normal mode the daemon registers the Bus service name `wgd`. Broker connection failure does not stop reconciliation. The Bus task retries with exponential backoff from one second to 60 seconds.

The service exposes four read-only verbs:

| Verb | Result |
|---|---|
| `wgd.iface.status` | Interface, inventory, live-read, synchronisation, and refresh summary. |
| `wgd.peer.status` | Live status for peers that currently match intent. |
| `wgd.topology.snapshot` | Derived topology plus any available drift items. |
| `wgd.drift` | The complete dry-run drift report. |

Before the first successful reconcile, every read verb returns a successful response with `ready: false`.

Unknown verbs and write-shaped verbs return caller error `rc=10`. The daemon does not publish drift or peer-status topics.

See [Bus verbs](verbs.md) for response fields and drift item shapes.

## Runtime configuration

The daemon has no crate-specific configuration file. Runtime inputs come from command-line overrides, the node configuration used to resolve the local node name, and the trust files owned by the inventory authority.

| Option | Purpose | Default |
|---|---|---|
| `--iface NAME` | Override the derived WireGuard interface name. | First DNS label of the verified mesh identity. |
| `--self NAME` | Override the local mesh member name. | `node` from the node configuration. |
| `--interval SECONDS` | Set the reconcile period. | 30 seconds; values below one become one. |
| `--genesis PATH` | Override the genesis public-key path. | `/etc/cosmix/noded/genesis.pub` |
| `--signed PATH` | Override the signed-inventory path. | `/var/lib/cosmix/noded/inventory.signed` |
| `--baseline PATH` | Override the rollback-baseline path. | `/var/lib/cosmix/noded/inventory.baseline` |
| `--once` | Reconcile once, print a summary, and exit without Bus. | Disabled. |

An interface override must contain 1 to 15 ASCII alphanumeric, `.`, `-`, or `_` characters. `.` and `..` are rejected.

See [Configuration and lifecycle](configuration.md) for trust handling, one-shot operation, and signals.

## Modules

The binary is divided into private modules:

| Module | Responsibility |
|---|---|
| `trust` | Read the trust inputs, enforce the rollback floor, and verify the signed inventory. |
| `derive` | Purely derive the intended peer set and validate membership coherence. |
| `live` | Execute and parse `wg show IFACE dump`. |
| `reconcile` | Purely compare intended and live peer sets. |
| `runner` | Run one pass or the periodic reconcile loop and publish snapshots. |
| `state` | Hold the latest complete snapshot behind a shared mutex. |
| `bus` | Register the service and answer read-only verbs. |
| `citizen` | Report the daemon identity and advisory effective UID/GID check. |

## Dependencies

The principal Cosmix dependencies are:

| Crate | Use |
|---|---|
| `cosmix-lib-wg` | WireGuard value types, interface naming, dump parsing, CIDR parsing, and peer liveness. |
| `cosmix-lib-mesh-trust` | Signed-inventory verification and epoch-based WireGuard credential selection. |
| `cosmix-lib-config` | Node configuration and default Bus client connection. |
| `cosmix-lib-bus` | Bus service registration provenance. |
| `cosmix-lib-client` | Native broker client and command handling. |
| `cosmix-lib-buildinfo` | Build provenance and version-discovery data. |
| `cosmix-lib-log` | Daemon logging initialisation. |

The manifest defines no Cargo features. The citizen stack and native Bus client are unconditional.

## Exit status

Argument errors return status 2.

Startup failure to resolve the local node name returns failure.

`--once` returns failure when inventory trust or intended-set derivation fails. A live-read failure is reported in the printed summary but does not invalidate a successfully derived snapshot.

Normal mode exits successfully after `SIGINT` or `SIGTERM`.

## Limits

`cosmix-wgd` requires Linux for its advisory identity check and expects `wg` from `wireguard-tools` on `PATH` for live reads.

It does not mutate WireGuard state. Drift entries include future-action labels for reporting, but no action is executed.

It reads the rollback baseline but never advances or rewrites it.

It serves the most recent complete snapshot; readers never observe a partially replaced snapshot.
