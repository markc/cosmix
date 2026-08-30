# cosmix-lib-props-core — property read surface

**`cosmix-lib-props-core` is the pure-type half of the SPEC 07 property
surface.** It defines how a daemon exposes a typed, dotted property tree
without pulling in storage, hooks, audit, or the SPEC 12 mutation router.

The default feature set has no Bus dependency. Enable `bus` only when a
consumer needs wire dispatch or publication-message builders.

## What it is

The three central types are:

- `PropPath` — a validated dotted path. Segments contain lowercase ASCII letters, digits, or `_`; empty segments and `*` are rejected.
- `PropValue` — the serialisable value set: null, bool, signed `i64`, unsigned `u64`, `f64`, string, list, or deterministically ordered object.
- `PropTree` — the trait a property producer implements with `snapshot`, `list`, and `describe`.

`PropTree` supplies default `get` and `redacted_snapshot` implementations.
`get` walks the object snapshot by path. Redaction consults each
`PropDescribe.sensitive` flag and replaces sensitive leaves with the
type-appropriate redacted value.

`PropDescribe` and `PropType` describe the schema for a leaf or subtree.
The crate also exports `diff`, `redact`, `tree::build_snapshot`, and the
lightweight in-memory revisioned-write types under `revwrite`.

## The SPEC 07 read surface

With the `bus` feature enabled, `bus::dispatch_props` maps the suffixes `get`,
`list`, and `describe` onto a `&dyn PropTree`:

- `get` returns the full snapshot or the value at `args.path`;
- `list` returns every leaf path; and
- `describe` returns the schema entry for the required `args.path`.

It returns a `PropsResponse` containing `rc`, body, and optional error text.
`bus::build_response` turns that result into a `BusMessage`, setting
`type=response`, service and requester addresses, command, and return code.
Routing and transport remain the daemon's responsibility.

## Publication helpers

The `bus` feature also exposes `publish`:

- `world_topic(svc)` produces the retained `world.<svc>` snapshot topic.
- `props_changed_topic(svc)` produces `<svc>.props.changed`.
- `build_world_message` serialises a property snapshot into a `BusMessage`.
- `build_props_changed_message` builds the per-leaf `{path, old, new, ts, cause}` event and matching filter headers.

The builders return inner `BusMessage` values. The caller chooses the transport,
topic wrapper, retention policy, and trigger.

## SPEC 07 and SPEC 12

This crate owns the common read contract. The cos repo's
`cosmix-lib-props-store` pairs it with the substrate side:

- namespace specifications and lifecycle;
- persistent and in-memory storage backends;
- mutation hooks, capability checks, and the SPEC 12 mutation router; and
- audit and per-record integrity machinery.

Keeping that split means a consumer can parse paths, values, descriptions, and
read replies without acquiring storage or daemon dependencies.

## Features

| Feature | What it adds |
|---|---|
| default | Pure property types, schema helpers, redaction, diff, and revisioned in-memory write helpers. |
| `bus` | `bus::{dispatch_props, build_response, PropsResponse}` plus `publish::*`; pulls `cosmix-lib-bus` and `chrono`. |

## See also

- [wire format](wire-format.md) — the `BusMessage` carrying property replies and events
- [client](client.md) — calling `<svc>.props.*` and subscribing to change topics
- [overview](overview.md) — the bus/cos protocol and substrate boundary
