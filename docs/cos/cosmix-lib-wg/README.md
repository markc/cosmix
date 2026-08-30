# cosmix-lib-wg

`cosmix-lib-wg` is the pure-logic WireGuard substrate library for the CosMix daemon family. It supplies key types, configuration rendering, address allocation, status parsing, mesh interface naming, QR rendering, and typed netlink message construction without owning a daemon, socket, or reconciliation loop. It belongs to the `cos` layer of the `bus <- mix <- cos` dependency chain and has no dependency on the Bus or Mix crates.

## Scope

The Rust library name is `cosmix_wg`. Its public API is also re-exported from the crate root.

The crate owns deterministic value handling and wire construction. A caller owns:

- Files, permissions, and secret storage.
- Process execution and captured `wg` output.
- Netlink sockets, family-ID resolution, sequencing, and responses.
- Schema validation beyond the invariants documented below.
- Reconciliation, locking, policy, and lifecycle.

The operating-system CSPRNG is the only required source of side effects. Netlink helpers construct messages but do not send them.

## Modules

| Module | Purpose |
|---|---|
| `keys` | Generate, parse, encode, derive, and hold WireGuard key material. |
| `render` | Render a server-side interface and its peers as `wg.conf` text. |
| `client` | Render a client peer configuration with one server peer. |
| `ipam` | Parse strict IPv4 and IPv6 CIDRs and select the lowest free host. |
| `dump` | Parse tab-separated `wg show <iface> dump` output and derive peer status. |
| `mesh` | Derive a Linux interface name from the first label of a mesh FQDN. |
| `qr` | Encode client configuration text as an SVG QR code. |
| `wire` | Build typed WireGuard generic-netlink and rtnetlink messages. |

## Key material

`WgPrivateKey`, `WgPublicKey`, and `WgPresharedKey` hold the 32-byte forms used by WireGuard. `KEY_LEN` is `32`; standard padded base64 is 44 characters.

`WgKeyPair` provides:

- `generate` for an OS-CSPRNG-backed Curve25519 keypair.
- `generate_from_rng` for a caller-supplied cryptographic RNG.
- `from_private_bytes` to clamp a scalar and derive its public key.

`WgPrivateKey` parses and emits standard padded base64 and derives its public key. Private input is clamped when constructed. `WgPublicKey` also supports direct construction from 32 raw bytes. `WgPresharedKey` generates uniformly random bytes and supports the same base64 form.

Private keys and pre-shared keys zeroise on drop and use redacted `Debug` output. Public keys are neither zeroised nor redacted. `KeyError` reports malformed base64 and decoded lengths other than 32 bytes.

## Configuration rendering

`WgInterface` and `WgPeer` are server-side rendering inputs. `render_interface_conf` emits one `[Interface]` section followed by peer sections in input order.

Interface output is ordered as:

1. `Address` lines.
2. `ListenPort`.
3. `PrivateKey`.
4. Optional `MTU`.

Each peer contains `PublicKey`, optional `PresharedKey`, `AllowedIPs`, optional `Endpoint`, and optional `PersistentKeepalive`. Allowed IPs are joined with commas on one line. Output ends with a newline.

`WgClientInterface` and `WgClientPeer` model the client view. `render_peer_conf` emits the client's private key and addresses, then one server `[Peer]` section. Client `AllowedIPs` and `PersistentKeepalive` are always emitted. `WgClientPeer::WG_ADMIN_KEEPALIVE_SECS` is `25`; passing `0` explicitly disables keepalive while preserving the line.

Both renderers reject carriage returns, line feeds, and NUL bytes in caller-supplied addresses, allowed IPs, and endpoints. They do not validate CIDR syntax, endpoint resolution, or non-zero ports.

## Address management

`parse_cidr` accepts strict `address/prefix` text and returns `Cidr`. It rejects malformed input, invalid addresses, out-of-range prefixes, surrounding whitespace, and addresses with host bits set. `Cidr::contains` tests membership and returns `false` for an address-family mismatch or an invalid hand-built prefix.

`next_free_host` returns the lowest address not present in the supplied `taken` slice. The slice need not be sorted; duplicates and out-of-subnet entries are tolerated.

Host selection follows these rules:

- IPv4 prefixes from `/0` through `/30` skip the network and broadcast addresses.
- IPv4 `/31` treats both addresses as hosts.
- IPv4 `/32` treats its sole address as a host.
- IPv6 prefixes through `/127` skip the network address and have no broadcast exclusion.
- IPv6 `/128` treats its sole address as a host.

The function returns `None` when the subnet is full or when a caller manually constructs an invalid `Cidr`.

```rust
use cosmix_wg::{iface_name_for_mesh, next_free_host, parse_cidr};
use std::net::{IpAddr, Ipv4Addr};

let subnet = parse_cidr("192.0.2.0/29").unwrap();
let taken = [IpAddr::V4(Ipv4Addr::new(192, 0, 2, 1))];

assert_eq!(
    next_free_host(&subnet, &taken),
    Some(IpAddr::V4(Ipv4Addr::new(192, 0, 2, 2))),
);
assert_eq!(iface_name_for_mesh("alpha.example.com").unwrap(), "alpha");
```

## Status parsing

`parse_wg_show_dump` decodes the headerless, tab-separated output from `wg show <iface> dump` into `WgShowDump`, `WgInterfaceDump`, and `PeerDump`.

The parser:

- Discards the interface private-key field.
- Parses interface public key, listen port, and decimal or hexadecimal firewall mark.
- Records whether a peer has a pre-shared key without retaining the key.
- Decodes optional endpoints, allowed IPs, handshake times, counters, and keepalive.
- Preserves peer emission order.
- Tolerates blank lines and trailing newlines.

`PeerStatus::from_handshake` classifies a timestamp as `Pending`, `Connected`, or `Offline`. A zero timestamp is pending. A non-zero handshake less than `CONNECTED_THRESHOLD_SECS` old is connected; the threshold is 180 seconds. Future-dated timestamps are treated as connected.

## Interface names and QR output

`iface_name_for_mesh` lowercases the first label of an FQDN and validates it as a Linux interface name. The label must be non-empty, at most `IFNAMSIZ_MINUS_NUL` bytes (`15`), contain only ASCII letters, digits, and hyphens after lowercasing, and have no leading or trailing hyphen. The function does not truncate invalid names.

`render_qr_svg` returns a complete SVG document using the QR encoder's default styling. It rejects empty input and propagates encoding failures, including payloads too large for a QR code.

## Netlink message construction

`WgIfaceSel` selects a device by name or index. `SetDeviceParams` and `SetPeer` describe partial device and peer updates.

The generic-netlink builders are:

- `wg_get_device_message` for `WG_CMD_GET_DEVICE`.
- `wg_set_device_message` for `WG_CMD_SET_DEVICE`.

Absent optional device fields are omitted so existing kernel values remain unchanged. Peer flags support replacing allowed IPs, updating only an existing peer, removing a peer, and replacing the complete device peer set. Removal rejects combinations with other peer mutations. CIDR prefixes are checked before message construction.

The rtnetlink builders are:

- `rtnl_new_link_wireguard` and `rtnl_del_link`.
- `rtnl_set_link_up` and `rtnl_set_link_down`.
- `rtnl_new_address` and `rtnl_del_address`.

Address builders validate the prefix length for the IP family. All helpers return typed message values; the caller serialises, sends, and processes them. A set-device message contains temporary plain key-byte copies required by the underlying packet type and should be serialised and dropped promptly.

## Cargo features

| Feature | Default | Effect |
|---|---:|---|
| `default` | Yes | Empty feature set; enables no optional behaviour. |

The full test surface is available with `--no-default-features`.

## Dependencies

The crate uses `base64`, `thiserror`, `zeroize`, `rand_core`, and `x25519-dalek` for keys and errors. SVG QR output uses `qrcode` with only its `svg` feature. Typed wire messages use `netlink-packet-core`, `netlink-packet-generic`, `netlink-packet-wireguard`, and `netlink-packet-route`.
