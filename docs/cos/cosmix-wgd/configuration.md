# Configuration and lifecycle

`cosmix-wgd` has no crate-specific configuration file. It resolves the local member from the node configuration and reads three trust-state files. Command-line flags override those inputs for controlled tests and one-shot inspection.

## Command line

```text
cosmix-wgd [--iface NAME] [--self NAME] [--interval SECONDS]
           [--genesis PATH] [--signed PATH] [--baseline PATH]
           [--once]
```

The parser accepts no positional arguments. An unknown argument or a missing flag value prints an error and returns status 2.

`--version` and `-V` print the package version and exit successfully.

The binary does not implement a help flag.

## Local identity

`--self NAME` selects the local inventory member explicitly.

Without `--self`, the daemon loads the node configuration and uses its non-empty `node` value. Missing node configuration or an empty node value prevents startup.

The selected name must identify an active member in the verified inventory. Otherwise derivation fails and no new snapshot is produced.

## Interface selection

`--iface NAME` selects the WireGuard interface explicitly.

Without the override, the daemon derives the interface from the first DNS label of the verified mesh identity by using `cosmix-lib-wg`.

An override:

- is 1 to 15 bytes long;
- contains only ASCII letters, digits, `.`, `-`, or `_`;
- is not `.` or `..`.

The daemon passes the validated name to `wg` as one argument. It does not invoke a shell.

Example:

```text
cosmix-wgd --self alpha --iface wg-example --once
```

## Reconcile interval

`--interval SECONDS` changes the normal-mode reconcile cadence.

The default is 30 seconds. Zero is accepted and normalised to one second. Non-integer values are argument errors.

Each interval begins a new pass that re-reads all trust inputs. There is no separate reload operation.

## Trust inputs

| Input | Default path | Content |
|---|---|---|
| Genesis anchor | `/etc/cosmix/noded/genesis.pub` | Bare base64 Ed25519 public key, with an optional trailing newline. |
| Signed inventory | `/var/lib/cosmix/noded/inventory.signed` | JSON signed-inventory envelope containing `payload` and `signatures`. |
| Rollback baseline | `/var/lib/cosmix/noded/inventory.baseline` | JSON object containing numeric `epoch` and optional `recovery_generation`. |

The override flags are `--genesis`, `--signed`, and `--baseline`.

The genesis anchor must decode to exactly 32 bytes. The signed inventory must parse and verify against a trust state containing the active `genesis` key and the persisted rollback floor.

The baseline is read-only:

- a missing baseline is treated as epoch zero and recovery generation zero;
- another read error fails closed;
- malformed JSON fails closed;
- a missing or non-numeric `epoch` fails closed;
- a missing `recovery_generation` is treated as zero.

`cosmix-wgd` never writes the baseline, signed inventory, or genesis anchor.

Paths passed as overrides may point to test fixtures. Do not place private key material in these files; the genesis input is a public verification key.

## One-shot mode

`--once`:

1. verifies the trust inputs;
2. derives the intended peer set;
3. attempts one live WireGuard read;
4. stores one snapshot;
5. prints an interface, mesh, epoch, self, intended-peer, and live-state summary;
6. exits without connecting to the Bus broker.

Trust or derivation failure returns failure. Live-read failure remains part of a valid derived snapshot, so the command prints `live UNAVAILABLE` and exits successfully.

Example with isolated trust fixtures:

```text
cosmix-wgd \
  --once \
  --self alpha \
  --iface wg-example \
  --genesis ./fixtures/genesis.pub \
  --signed ./fixtures/inventory.signed \
  --baseline ./fixtures/inventory.baseline
```

## Normal mode

Normal mode starts two independent tasks:

- a periodic reconcile loop;
- a Bus registration and command loop.

Reconcile failure is logged and retried. The latest good snapshot continues to serve.

Bus connection failure is retried with exponential backoff. Reconciliation continues while the broker is unavailable.

## Signals

| Signal | Behaviour |
|---|---|
| `SIGINT` | Abort both tasks and exit successfully. |
| `SIGTERM` | Abort both tasks and exit successfully. |
| `SIGHUP` | Log an advisory message. The periodic loop already re-reads inputs, so no explicit reload occurs. |

## Live WireGuard requirement

The live reader executes:

```text
wg show IFACE dump
```

`wg` must be available on `PATH`. A spawn failure, non-zero exit, non-UTF-8 output, or dump parse error marks live state unavailable without stopping the daemon.

The daemon reads only. It does not call a WireGuard set operation or mutate the kernel.

[Back to cosmix-wgd](README.md)
