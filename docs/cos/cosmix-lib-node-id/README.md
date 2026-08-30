# cosmix-lib-node-id

`cosmix-lib-node-id` derives a stable, compact node identifier from a machine
identity string. It is a pure-logic substrate library in the `cos` repository,
downstream of `bus` and `mix` in the `bus <- mix <- cos` dependency chain, but
it has no internal Cosmix crate dependencies.

## Synopsis

The package name is `cosmix-lib-node-id`.

The Rust library name is `cosmix_node_id`:

```rust
use cosmix_node_id::{node_id, NodeIdError};

fn local_node_id() -> Result<String, NodeIdError> {
    node_id()
}
```

`node_id` reads `/etc/machine-id`. `node_id_from` accepts an identity string
directly and performs no filesystem access.

## Provided interface

| Item | Kind | Purpose |
|---|---|---|
| `node_id()` | Function | Reads the local machine ID and returns its derived node ID. |
| `node_id_from()` | Function | Derives a node ID from a supplied identity string. |
| `NodeIdError` | Error enum | Reports missing, empty, or unreadable machine identity data. |
| `MACHINE_ID_PATH` | Constant | Names the filesystem path read by `node_id`. |
| `NODE_ID_HEX_LEN` | Constant | Gives the output width in hexadecimal characters. |

The crate exposes its interface at the library root. It has no public
submodules.

## Derivation

The derivation is:

1. Trim surrounding whitespace from the machine identity string.
2. Reject the string if it is empty after trimming.
3. Compute its SHA-256 digest.
4. Hex-encode the first three digest bytes.
5. Truncate the encoded value to five characters.

The result is five lowercase hexadecimal characters and therefore represents
a 20-bit identifier:

```text
node_id = first_5_hex_characters(SHA-256(trim(machine_id)))
```

The output is deterministic for the same trimmed input. A trailing newline in
`/etc/machine-id` does not affect the result.

For `node_id()`, stability lasts for the lifetime of the host's machine ID.
Reimaging a host or otherwise replacing `/etc/machine-id` can change the
derived value.

## Local machine identity

`node_id()` reads the path in `MACHINE_ID_PATH`, which is
`/etc/machine-id`.

It distinguishes a missing file from other I/O failures. It then delegates
the derivation to `node_id_from`.

Use `node_id_from` for test fixtures or tooling that supplies machine identity
from another source:

```rust
use cosmix_node_id::{node_id_from, NodeIdError, NODE_ID_HEX_LEN};

fn derive_for_fixture() -> Result<String, NodeIdError> {
    let id = node_id_from("example-machine-identity")?;
    assert_eq!(id.len(), NODE_ID_HEX_LEN);
    Ok(id)
}
```

`node_id_from` trims its argument before hashing. Whitespace-only input is
invalid.

## Errors

`NodeIdError` has three variants:

| Variant | Condition |
|---|---|
| `MachineIdMissing` | `/etc/machine-id` does not exist. |
| `MachineIdEmpty` | The identity is empty after surrounding whitespace is trimmed. |
| `Io` | Reading `/etc/machine-id` fails for another I/O reason. |

`Io` retains the underlying `std::io::Error`.

## Identifier scope

Five hexadecimal characters provide 1,048,576 possible identifiers. The
crate documentation estimates a birthday-collision probability of about 0.5%
at 100 nodes and about 2% at 200 nodes.

The identifier is compact rather than globally unique. Collision resolution
belongs to the substrate allocation layer; this crate only performs
derivation.

## Cargo features

The package declares no Cargo features. Its complete test surface runs without
default features:

```sh
cargo test -p cosmix-lib-node-id --no-default-features
```

## Dependencies

| Crate | Use |
|---|---|
| `sha2` | Computes SHA-256. |
| `hex` | Encodes digest bytes as hexadecimal. |
| `thiserror` | Implements the public error type. |

The crate does not depend on Bus, client, configuration, or property-store
crates.

## Behavioural guarantees

The crate tests these properties:

- output is exactly five lowercase hexadecimal characters;
- identical inputs produce identical outputs;
- surrounding whitespace does not affect the result;
- empty and whitespace-only inputs fail;
- two distinct fixed fixtures produce different identifiers;
- a fixed input matches a checked SHA-256 prefix vector.

The derivation contains no random input and keeps no mutable state.
