# cosmix-lib-bus — Bus wire format

**`cosmix-lib-bus` defines the bytes and shared records that cross Bus
transports.** Its central type is `BusMessage`: an ordered string-header map
plus an optional string body. It can be used without a broker.

## What it is

Bus uses a small markdown-frontmatter-like frame:

```text
---
command: noded.ping
from: probe
id: 1
to: noded
type: request
---
{"detail":true}
```

`BusMessage` is one generic message type, not separate Rust request, reply, and
event enums. The `type` header identifies `request`, `response`, `event`, or
`stream`; `VALID_TYPES` lists those values. Common headers include `command`,
`from`, `to`, `id`, `args`, `json`, `rc`, and `error`. The body remains an
opaque string to the wire crate, although clients normally put JSON arguments
and results there.

Replies use the ARexx return-code bands exported as `RC_SUCCESS` (`0`),
`RC_WARNING` (`5`), `RC_ERROR` (`10`), and `RC_FAILURE` (`20`). `PortReply`
keeps a peer's `rc >= 10` application error separate from a transport failure.

## What it does

- `BusMessage::new`, `empty`, `command`, `with_header`, `with_body`, `set`, and `get` construct and inspect frames.
- `to_wire` and `to_bytes` serialise headers between `---` delimiters, followed by the body. The `BTreeMap` makes header order deterministic.
- `parse_strict` rejects non-conforming header lines or malformed JSON-shaped header values.
- `parse_lenient` returns both the message and a `ParseReport`; `parse` preserves the older behaviour that silently skips reported defects.
- `validate` reports unknown headers, invalid message types, malformed `args` / `json`, and non-numeric `rc` / `ttl` as warnings.
- `ServiceInfo`, `RegisterProvenance`, and `NodeInfo` carry broker registry and build-discovery data.

The empty frame `---\n---\n` is a valid heartbeat or keepalive. Parsing limits a
header block to `MAX_HEADERS` entries. Native Unix-stream reads also enforce
`MAX_MESSAGE_BYTES` (16 MiB).

## Addressing

The broker accepts two everyday target forms:

- `<service>` — local-only shorthand resolved directly through the local broker registry.
- `<service>.<node>.bus` — a service on a named node in the WireGuard mesh.

`BusAddress::parse_local` also accepts `<node>.bus` (that node's broker) and
`<sub>.<service>.<node>.bus`, where the leading label is opaque to the broker
and interpreted by the destination service. The `.bus` suffix is optional on
two- and three-label parser input; `Display` emits it.

`BusTarget::parse` recognises the reserved cross-mesh form
`<local-bus>@<mesh-fqdn>` and exposes it as `BusTarget::CrossMesh`. The current
router must refuse that variant: federation transport is not implemented.
Bare `<service>` is deliberately not parsed as a `BusAddress`; callers fall
back to registry lookup.

## The `native` feature

`native` is enabled by default. It adds Tokio Unix-stream helpers
`read_from_stream` / `write_to_stream`, the local `Port` command server, and
`call_port` / `call_port_typed`.

With default features disabled, message framing, parsing, validation,
addressing, return codes, and discovery records remain available without Tokio
or libc. This is the target-independent protocol core used by browser builds of
[cosmix-lib-client](client.md).

## See also

- [client](client.md) — WebSocket transport and request/reply correlation
- [property core](props-core.md) — typed property data carried over Bus
- [overview](overview.md) — the protocol family and repository boundary
