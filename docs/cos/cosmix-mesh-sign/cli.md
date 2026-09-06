# cosmix-mesh-sign CLI

## Name

`cosmix-mesh-sign` — create mesh trust keys, sign an inventory, and verify a signed inventory.

## Synopsis

```text
cosmix-mesh-sign [--secrets-db <PATH>] [--mesh <FQDN>] <COMMAND>
```

Global options may be used with every command.

## Global options

### `--secrets-db <PATH>`

Select the SQLite database containing operator keys.

A value beginning with `~/` expands against the `HOME` environment variable. Other paths are used as supplied.

### `--mesh <FQDN>`

Select the mesh identifier. The value forms the database `domain` component of key lookups.

Examples use `mesh.example.com`.

## Node-local WireGuard keys (0.8.0)

```text
cosmix-mesh-sign wg-gen --private-file /etc/wireguard/mesh.key
cosmix-mesh-sign wg-pubkey --private-file /etc/wireguard/mesh.key
```

These commands use `cosmix-lib-wg` and do not access the operator secrets DB.
Run generation on the node that will own the key. The absolute parent directory
must already exist. `wg-gen` creates a new private file with mode 0600 (subject
to a more restrictive umask), syncs it and its directory, and prints only the
public key. It never replaces an existing file and has no force option. An I/O
failure may leave an incomplete file requiring explicit operator inspection.

`wg-pubkey` refuses symlinks, non-regular files, group/other permissions and
invalid or oversized encodings. It prints only the derived public key, allowing
an interrupted enrolment to resume without replacing the private key. Key
generation alone does not enrol a node or configure a WireGuard interface.

## `genesis`

Generate a mesh-wide Ed25519 genesis keypair and store the private seed.

```text
cosmix-mesh-sign [GLOBAL OPTIONS] genesis [--force]
```

On success, the command prints the public key and the database record identity. The public key is base64.

The command creates the `secrets` table and unique index when they do not exist.

Without `--force`, insertion is atomic and fails when the genesis record already exists.

With `--force`, the command updates an existing record or inserts a new one. Replacing genesis invalidates all signatures made by the previous key and requires separate provisioning of the new public key.

## `sign`

Sign an authored strict-data inventory and self-verify the emitted artifact.

```text
cosmix-mesh-sign [GLOBAL OPTIONS] sign \
  <INVENTORY> \
  --out <PATH> \
  [--valid-days <DAYS>] \
  [--recovery <GENERATION>]
```

Arguments and options:

| Surface | Meaning |
|---|---|
| `<INVENTORY>` | Path to the authored `inventory.mix`. |
| `-o`, `--out <PATH>` | Required output path for the signed artifact. |
| `--valid-days <DAYS>` | Set `valid_until` to this many days after signing. Defaults to `90`. |
| `--recovery <GENERATION>` | Emit a recovery inventory with the supplied generation. |

Example:

```text
cosmix-mesh-sign \
  --secrets-db ./secrets.db \
  --mesh mesh.example.com \
  sign inventory.mix \
  --out inventory.signed \
  --valid-days 90
```

The input must contain a top-level `inventory` map with `unsigned: true`. It must not already contain signing or recovery fields.

The command adds:

- `canonical_encoding`;
- `signed_at`;
- `valid_until`;
- `verify_keys`;
- `recovery` and `recovery_generation` when requested.

The output contains one Ed25519 signature identified by the genesis key ID.

Before writing, the command serialises, reparses, and verifies the exact output text through `SignedInventory`. Failure leaves no newly written output from this invocation.

The self-check uses an epoch and recovery-generation baseline of zero. It proves cryptographic and structural validity, but does not prove freshness relative to a node's stored state.

## `verify`

Verify a signed inventory against the genesis public key derived from the stored signing key.

```text
cosmix-mesh-sign [GLOBAL OPTIONS] verify <SIGNED>
```

`<SIGNED>` is the path to a signed JSON inventory.

The command parses the artifact with `SignedInventory::parse` and verifies it against a trust state containing the active genesis key. Success prints the accepted epoch, recovery status, and verifying key identifiers.

## `pubkey`

Print the genesis public verify key.

```text
cosmix-mesh-sign [GLOBAL OPTIONS] pubkey
```

The command reads the private seed, derives the Ed25519 public key, and writes bare base64 to standard output. It emits no descriptive text on the output line.

## `d2-gen`

Generate a node-specific Ed25519 d2 admission keypair.

```text
cosmix-mesh-sign [GLOBAL OPTIONS] d2-gen <NODE> [--force]
```

`<NODE>` is the node name used as the database `vnode` value:

```text
cosmix-mesh-sign \
  --secrets-db ./secrets.db \
  --mesh mesh.example.com \
  d2-gen alpha
```

The private seed is stored under the node. The command prints the public key for use in that node's inventory `credentials` entry.

Without `--force`, the command atomically refuses an existing record. With `--force`, it replaces the existing node key. Replacement invalidates admission proofs made with the previous key.

## `d2-pubkey`

Print a node's stored d2 public key.

```text
cosmix-mesh-sign [GLOBAL OPTIONS] d2-pubkey <NODE>
```

The command looks up the node-specific private seed, derives the public key, and writes bare base64 to standard output.

It fails if the node has no stored d2 key, if the seed is not valid base64, or if the decoded seed is not 32 bytes.

## Database records

The key identity is the full SQLite tuple:

```text
(vnode, domain, service, username)
```

Genesis uses a mesh-wide vnode and the `genesis` key identity. D2 keys use the node name as vnode and the `d2` credential identity.

Private seeds are stored in the `password` column as base64. Public keys are derived when printed.

## Exit behaviour

The process exits successfully only when the requested operation completes.

Common errors include:

- the selected key does not exist;
- a key exists and replacement was not authorised with `--force`;
- the database cannot be opened or updated;
- an input or output file cannot be read or written;
- the authored inventory is not strict-data or is not marked unsigned;
- a signer-owned field is present in the authored inventory;
- numeric conversion would lose information;
- the signed payload does not match the typed inventory shape;
- signature verification fails.

## See also

[cosmix-mesh-sign](README.md)
