# cosmix-lib-bus

`cosmix-lib-bus` defines the Bus wire format and native IPC primitives for
Cosmix applications. It is the protocol-layer crate at the bottom of the
`bus <- mix <- cos` dependency chain: higher layers consume its messages,
addresses, discovery records, and local port API, while this crate has no
dependency on Mix or Cos.

Rust code imports the `cosmix-lib-bus` package as `cosmix_bus`.

```rust
use cosmix_bus::bus::{parse_strict, BusMessage};

let message = BusMessage::new()
    .with_header("type", "request")
    .with_header("to", "indexd.beta.bus")
    .with_header("command", "status")
    .with_body(r#"{"detail":true}"#);

let decoded = parse_strict(&message.to_wire())?;
assert_eq!(decoded.command_name(), Some("status"));
```

## What it provides

- `bus` defines Bus messages, parsers, validation, addresses, targets, and
  native stream helpers.
- `service_info` defines the service and node discovery records exchanged with
  a broker.
- With the `native` feature, the crate exports a Unix-socket command port,
  client call helpers, typed replies, and port events.

## Wire format

`BusMessage` contains an ordered `BTreeMap<String, String>` of headers and an
optional string body. The wire representation uses Markdown-style frontmatter:

```text
---
command: get
from: mix.alpha.bus
to: props.beta.bus
type: request
---
{"key":"value"}
```

`BusMessage::new` and `BusMessage::empty` create an empty message.
`BusMessage::command` constructs a header-only command. `with_header`,
`with_body`, `set`, and `get` build or inspect a message.

`to_wire` serialises to text and `to_bytes` serialises to UTF-8 bytes. `Display`
uses the same wire representation. `EMPTY_MESSAGE` is the canonical
header-free, body-free heartbeat or keepalive frame.

Convenience accessors expose `from`, `to`, `command`, `type`, JSON `args`, JSON
`json`, display-protocol fields, and the `ui.*` command test. `error_message`
prefers the `error` header, then a JSON-body `error`, then the raw body, and
finally `unknown error`.

## Parsing and validation

Choose the parser according to the input contract:

| Function | Behaviour |
|---|---|
| `bus::parse_strict` | Rejects malformed header lines and invalid JSON-looking header values. Use for canonical Bus content. |
| `bus::parse_lenient` | Returns a `BusMessage` and a `ParseReport` containing skipped lines and JSON parse errors. |
| `bus::parse` | Preserves the compatibility behaviour of silently ignoring the lenient parse report. |

Header lines use the exact `key: value` separator. Values beginning with `[` or
`{` are checked as JSON by the strictness report. Parsing trims trailing
whitespace from the body.

`bus::validate` performs permissive protocol checks and returns warnings. It
checks known headers, message type, JSON in `args` and `json`, and numeric `rc`
and `ttl` values. An empty warning vector means the message conforms.

The recognised message types are `request`, `response`, `event`, and `stream`.
Validation does not reject unknown headers or invalid values by itself.

## Addressing

`BusAddress` models the local portion of a Bus address:

- `<node>.bus` addresses the node; the broker service is implicit.
- `<service>.<node>[.bus]` addresses a service on a node.
- `<sub>.<service>.<node>[.bus]` adds an opaque sub-protocol or instance label.

`BusAddress::parse_local` accepts local addresses. A bare service name such as
`indexd` is registry shorthand, not a parseable Bus address.

`BusTarget::parse` handles local targets such as `indexd.alpha.bus` and the
reserved cross-mesh form `indexd.alpha.bus@example.com`.

`BusTarget::is_cross_mesh` makes the distinction explicit. Cross-mesh targets
are parsed and represented, but routing remains unimplemented and routers must
refuse them.

Address labels use lowercase ASCII letters, digits, and hyphens. A label is at
most 63 bytes and cannot start or end with a hyphen. A mesh FQDN must contain a
dot, use the same label rules, and cannot use `xn--` labels.

## Return codes and responses

The crate follows four ARexx-style return-code constants:

| Constant | Value | Meaning |
|---|---:|---|
| `RC_SUCCESS` | 0 | Success |
| `RC_WARNING` | 5 | Warning |
| `RC_ERROR` | 10 | Application error |
| `RC_FAILURE` | 20 | Failure |

`PortResponse` carries `rc`, `ok`, optional JSON `data`, and optional error
text. Its `success`, `ok`, `warning`, `error`, and `failure` constructors create
the standard response shapes.

For typed native calls, every `rc < 10` is `PortReply::Ok` and preserves the
exact code. Every `rc >= 10` is `PortReply::AppError`. Transport, framing,
UTF-8, size, and JSON failures remain the enclosing `Result::Err`.

## Service and node discovery

`ServiceInfo` is the broker registry record for one service. It contains the
required registered name and optional binary name, version, Git revision,
dirty-tree flag, build time, process ID, process start, registration time,
schema version, and open `meta` values.

`ServiceInfo::from_name` creates a name-only record. Deserialisation accepts
both the object form and a legacy bare service-name string. Unknown object
fields are ignored, and optional fields default when absent, so additive record
changes remain compatible.

`RegisterProvenance` contains the build and process facts a service sends when
registering. `RegisterProvenance::from_parts` assembles those fields, records
the current process ID, and leaves the caller responsible for supplying one
process-start timestamp that survives reconnects.

`NodeInfo` describes a node, its optional mesh identity and broker build,
current uptime, registered service count, schema version, and open metadata.
`SCHEMA_VERSION` is the current discovery-record format version.

## Native Unix-socket ports

The default `native` feature exports `Port`. A port:

- registers named JSON command handlers with descriptions;
- optionally emits `PortEvent` notifications;
- can generate `help`, `info`, and `activate` commands;
- listens at `/run/user/<uid>/cosmix/ports/<name>.sock`; and
- dispatches one Bus request per connection.

Requests carry the command in the `command` header and JSON arguments in the
body. Responses carry `rc` and optional `error` headers plus an optional JSON
body. The internal `__scripts__` command emits `PortEvent::ScriptsUpdated`.

`PortEvent` reports completed commands, activation requests, and script-list
updates. `ScriptInfo` supplies the display name and full path for one script
menu entry.

`call_port` is the compatibility client helper. It converts both transport
failures and peer replies with `rc >= 10` into errors. `call_port_typed`
preserves the distinction with `PortReply`.

The `bus::read_from_stream` and `bus::write_to_stream` helpers exchange a
single message on a Tokio Unix stream. The reader waits for EOF, applies a
10-second timeout, enforces the message-size limit, decodes UTF-8, and parses
the frame.

## Features

| Feature | Default | What it adds |
|---|---|---|
| `default` | Yes | Enables `native`. |
| `native` | Yes | Tokio Unix-stream transport, the Unix-socket `Port` server, call helpers, `PortReply`, and `libc`-based socket paths. |

With `--no-default-features`, the wire types, parsers, validation, addressing,
return-code types, and discovery records remain available without Tokio or
`libc`.

The core crate uses `serde` and `serde_json` for wire data, `anyhow` for
fallible parsing, and `tracing` for diagnostics. The `native` feature adds the
optional `tokio` and `libc` dependencies.

## Limits

`MAX_MESSAGE_BYTES` limits one native local-transport message to 16 MiB.
`MAX_HEADERS` limits parsing to 4096 non-empty header lines. Lenient parsing
records a header overflow; strict parsing rejects it.

These bounds apply to the native Unix-socket transport and parser. Other
transports enforce their own frame and message limits.

## See also

- [Bus wire format](../wire-format.md)
- [Bus overview](../overview.md)
- [broker client](../client.md)
