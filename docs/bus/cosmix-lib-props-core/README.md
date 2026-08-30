# cosmix-lib-props-core

`cosmix-lib-props-core` provides the pure Rust types and read-side helpers for the SPEC 07 property surface. It sits at the `bus` end of the `bus <- mix <- cos` dependency chain: protocol consumers can share property paths, values, schemas, snapshots, redaction, diffs, and revisioned in-memory writes without taking a storage backend or daemon substrate dependency.

The Cargo package is named `cosmix-lib-props-core`. Rust code imports it as `cosmix_props_core`.

## Synopsis

```toml
[dependencies]
cosmix-lib-props-core = { version = "0.2.0" }
```

Enable Bus request dispatch and publish-message builders when wire integration is required:

```toml
[dependencies]
cosmix-lib-props-core = { version = "0.2.0", features = ["bus"] }
```

```rust
use cosmix_props_core::{PropPath, PropValue};

let path = PropPath::new("config.endpoint")?;
let value = PropValue::from("https://alpha.example.com");

assert_eq!(path.as_str(), "config.endpoint");
assert_eq!(value.type_name(), "string");

# Ok::<(), Box<dyn std::error::Error>>(())
```

## Feature flags

| Feature | Default | Provides | Additional dependencies |
|---|---:|---|---|
| `bus` | No | `bus::dispatch_props`, response construction, topic helpers, and publish-message builders | `cosmix-lib-bus`, `chrono` |

The default feature set is empty. All pure types, tree helpers, diffs, redaction, and the `revwrite` module remain available without `bus`.

## Property paths

`PropPath` is a validated dotted path. Each segment accepts lowercase ASCII letters, digits, and `_`.

`PropPath::new` rejects:

- an empty path;
- an empty segment;
- uppercase or other unsupported characters;
- `*`, which is reserved.

`as_str` returns the original path, `segments` iterates its segments, and `starts_with` tests equality or dotted-path ancestry rather than a plain string prefix.

`PropPathError` reports `Empty`, `EmptySegment`, `InvalidChar`, or `Wildcard`.

## Property values

`PropValue` is a Serde-compatible, untagged value enum:

| Variant | Rust payload | `type_name()` |
|---|---|---|
| `Null` | none | `null` |
| `Bool` | `bool` | `bool` |
| `Int` | `i64` | `i64` |
| `UInt` | `u64` | `u64` |
| `Float` | `f64` | `f64` |
| `String` | `String` | `string` |
| `List` | `Vec<PropValue>` | `list` |
| `Object` | `BTreeMap<String, PropValue>` | `object` |

Objects use sorted keys for deterministic serialisation. `is_object` and `as_object` inspect object values. `From` implementations cover booleans, numeric primitives, strings, and vectors whose elements convert into `PropValue`.

Conversion from `&PropValue` to `serde_json::Value` supports Bus body construction. JSON does not distinguish a small non-negative `i64` from a `u64`; such values deserialise as `Int` because the enum is untagged and tries `Int` first. Values above `i64::MAX` remain unambiguous.

Non-finite floating-point values convert to JSON `null`.

## Property schemas

`PropDescribe` describes one leaf or subtree. Its fields cover:

- path, type, mutability, sensitivity, and description;
- optional format, child paths, enum values, numeric bounds, default, and `since` marker;
- deprecated and transient flags.

`PropType` serialises as one of `null`, `bool`, `number`, `string`, `list`, or `object`.

`PropDescribe::leaf` creates an immutable, non-sensitive, non-transient leaf with no optional metadata. Builder methods set sensitivity, mutability, transience, format, unit, minimum, maximum, default, and enum values.

`with_unit` stores the unit token in the same `format` field used by `with_format`.

## Property trees

Implement `PropTree` to expose a property surface:

```rust
pub trait PropTree {
    fn snapshot(&self) -> PropValue;
    fn list(&self) -> Vec<PropPath>;
    fn describe(&self, path: &PropPath) -> Option<PropDescribe>;
}
```

The default `get` implementation walks the object returned by `snapshot`. Implementations may override it for efficiency.

The default `redacted_snapshot` implementation calls `describe` for every path returned by `list` and redacts leaves marked `sensitive`.

`tree::build_snapshot` builds a nested object from flat `(PropPath, PropValue)` pairs. Input order does not matter. When one path is a strict prefix of another, the longer path wins.

## Redaction

`redact` replaces sensitive strings with `"***"` and numeric, boolean, or null leaves with `null`. It recursively redacts lists and objects.

The function applies redaction unconditionally. Authentication and the decision to reveal a value remain caller responsibilities.

## Snapshot differences

`diff(old, new)` returns changed leaves as:

```rust
Vec<(PropPath, PropValue, PropValue)>
```

Results are ordered by path. Added or removed leaves use `PropValue::Null` for the missing side. Added or removed object subtrees produce one result per leaf. A non-object change at the root is skipped because it has no addressable property path.

## Revisioned writes

`revwrite` provides an in-memory control-write ledger. It is part of the default API but only affects programs that construct a `RevWriteStore`.

`RevWriteStore` maintains a global monotonic revision and current state per path. `seed` installs an initial value at revision zero without marking it changed. `apply` accepts a `RevWriteRequest` in call order and returns `RevWriteResponse::Accepted` or `RevWriteResponse::Rejected`.

`RevWriteRequest::if_revision` adds optimistic concurrency. A mismatch returns the current path revision and value without changing the store. `op_id` is a correlation value echoed in the response; it is not an idempotency key.

Every accepted write returns a `RevWriteAck` containing the authoritative revision, canonical value, source identity, path, and operation ID.

`drain_changed` returns one terminal `ChangedProp` per modified path, ordered by path, and clears the pending set. Repeated writes to one path coalesce to the latest state.

`accept_if_newer` updates a client-side `BTreeMap<PropPath, ChangedProp>` only when an incoming revision is strictly newer than the cached revision.

The store performs no domain validation, quantisation, persistence, audit signing, or internal synchronisation. Callers supply already-canonical values and provide locking when sharing a store across threads.

## Bus read surface

The `bus` feature adds `bus::dispatch_props` for `<svc>.props.*` commands:

| Command suffix | Arguments | Result |
|---|---|---|
| `get` | optional JSON string field `path` | full snapshot or the selected value |
| `list` | none | JSON array of leaf paths |
| `describe` | required JSON string field `path` | serialised `PropDescribe` |

`dispatch_props` receives a `PropTree`, the command suffix, optional JSON arguments, and a `redact_sensitive` flag. It returns `PropsResponse` with an integer return code, JSON body string, and optional error.

Success uses return code `0`. Invalid paths, missing paths, missing `describe` arguments, and unknown command suffixes use return code `10`.

Routing and prefix stripping remain caller responsibilities. `bus::build_response` converts a `PropsResponse` into a Bus response, mirrors the request sender into `to`, and sets `bus`, `type`, `from`, `command`, and `rc`.

## Bus publish helpers

The `publish` module is also gated by `bus`.

`world_topic("alpha")` returns `world.alpha`. `props_changed_topic("alpha")` returns `alpha.props.changed`.

`build_world_message` creates an inner Bus message whose `command` is the service name and whose body is the JSON snapshot.

`build_props_changed_message` creates one change event. Its JSON body contains `path`, `old`, `new`, an RFC 3339 UTC timestamp in `ts`, and `cause`. The message headers include `command=props.changed`, `path`, and `cause`.

Both builders return `BusMessage`. Transport, topic publication, retention, triggers, and extra headers remain caller responsibilities.

## Dependencies

The core API depends on `serde` and `serde_json`.

The `bus` feature additionally enables `cosmix-lib-bus` for `BusMessage` and `chrono` for change-event timestamps.
