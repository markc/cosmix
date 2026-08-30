# cosmix-mesh-sign

`cosmix-mesh-sign` is the operator CLI for creating Ed25519 mesh trust keys, signing an authored mesh inventory, and verifying the resulting signed inventory. It belongs to the `cos` layer of the `bus ← mix ← cos` dependency chain: it uses `cos` trust and configuration libraries, including the strict-data parser shared with Mix tooling, and does not provide a reusable Rust library API.

## Synopsis

```text
cosmix-mesh-sign [OPTIONS] <COMMAND>
```

The binary implements the signing side of the SPEC 13 mesh inventory trust root. It converts an unsigned `inventory.mix` into `inventory.signed`, signs the canonical payload, and verifies the emitted bytes before writing them.

See [CLI reference](cli.md) for complete command and option details.

## Commands

| Command | Purpose |
|---|---|
| `genesis` | Generate and store the mesh genesis Ed25519 signing key. |
| `sign` | Convert an unsigned Mix inventory into a signed JSON inventory. |
| `verify` | Verify a signed inventory against the stored genesis key. |
| `pubkey` | Print the genesis public verify key as base64. |
| `d2-gen` | Generate and store a node-specific d2 admission key. |
| `d2-pubkey` | Print a node's d2 public key as base64. |

The CLI exposes no Bus verbs and runs as a finite operator command, not as a daemon.

## Global options

| Option | Meaning |
|---|---|
| `--secrets-db <PATH>` | Select the SQLite operator secrets database. A leading `~/` expands against `HOME`. |
| `--mesh <FQDN>` | Select the mesh identifier stored in the database `domain` column. |
| `-h`, `--help` | Print command help. |

Use an explicit secrets database path in scripts:

```text
cosmix-mesh-sign \
  --secrets-db ./secrets.db \
  --mesh mesh.example.com \
  genesis
```

## Signing pipeline

`sign` performs the following operations:

1. Loads the genesis signing key from the secrets database.
2. Parses the input with `cosmix_config::load_mix_data`.
3. Selects the top-level `inventory` value.
4. Requires the authored inventory to contain `unsigned: true`.
5. Rejects signer-owned fields already present in the authored input.
6. Converts strict-data values to JSON without silently changing integers.
7. Adds canonical encoding, signing time, validity horizon, and verify-key data.
8. Optionally adds recovery metadata.
9. Deserialises the result as a typed `InventoryPayload`.
10. Signs the shared canonical byte representation.
11. Serialises and reparses the signed JSON.
12. Verifies the emitted representation before writing the output file.

The signer and verifier both use `cosmix-lib-mesh-trust` for canonicalisation and verification. This keeps the signed byte representation common to both sides.

## Authored inventory requirements

The input is strict Mix data, not executable Mix code. Its top level must contain an `inventory` map:

```text
inventory: {
  unsigned: true,
  epoch: 1,
  members: [
    {
      name: "alpha",
      status: "active",
      bus: true,
      credentials: []
    }
  ]
}
```

The authored map must not contain fields owned by the signer:

- `canonical_encoding`
- `signed_at`
- `valid_until`
- `verify_keys`
- `signatures`
- `recovery`
- `recovery_generation`

The `unsigned` marker is removed from the signed payload.

## Numeric conversion

Mix numbers arrive as `f64` values. Whole values are emitted as JSON integers so typed fields such as `epoch` remain integers.

The signer rejects:

- non-finite numbers;
- whole numbers outside the exactly representable `f64` integer range, `±(2^53 - 1)`;
- strict-data values that unexpectedly contain bytes, functions, or buffers;
- values that do not match the typed inventory payload.

These checks fail closed rather than signing a reshaped value.

## Key storage

Keys are stored as base64-encoded 32-byte Ed25519 seeds in a SQLite `secrets` table. Records are selected by the tuple:

```text
(vnode, domain, service, username)
```

The database schema is created when required. A unique index on the full tuple provides atomic refusal of an existing key.

Genesis keys are mesh-wide. D2 admission keys are stored per node.

`genesis` and `d2-gen` refuse to replace an existing key unless `--force` is supplied. Forced genesis replacement invalidates signatures made by the previous genesis key. Forced d2 replacement invalidates that node's in-flight admission proofs.

Public-key commands print base64 public keys. Private seeds remain in the selected secrets database.

## Recovery inventories

`sign --recovery <GENERATION>` emits:

```json
{
  "recovery": true,
  "recovery_generation": 2
}
```

The generation must be greater than the last accepted recovery generation at a node. The local self-check uses a zero baseline and does not know a node's cached epoch or recovery generation, so a self-verified artifact can still be rejected by a node as stale.

`valid_until` is advisory. It is not used as a security gate by this binary.

## Output and failure behaviour

`sign` writes pretty-printed JSON with a trailing newline. It writes only after the serialised artifact reparses and verifies successfully.

`verify` reads the genesis signing key from the secrets database, derives its public key, and verifies the supplied artifact against a trust state containing that genesis key.

Commands return an error for missing keys, malformed base64, incorrect seed length, invalid inventory structure, failed canonical verification, or database and file I/O failures.

## Cargo surface

The crate defines one binary target named `cosmix-mesh-sign`.

It declares no Cargo features.

Its principal internal dependencies are:

- `cosmix-lib-mesh-trust`, with default features disabled, for inventory types, canonical bytes, and verification;
- `cosmix-lib-config` for strict Mix data loading.

Cryptographic signing uses `ed25519-dalek` with operating-system randomness supplied through `rand`.
