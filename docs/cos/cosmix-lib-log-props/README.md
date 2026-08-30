# cosmix-lib-log-props

`cosmix-lib-log-props` provides the SPEC-12 `<service>.log` property namespace and a live-reload watcher for the `cosmix_log` logging core. It belongs to the `cos` layer of the `bus <- mix <- cos` dependency chain: the crate extends the logging core from `bus` with the daemon-facing property substrate supplied by `cos`, and does not depend on `mix`.

The Cargo package is `cosmix-lib-log-props`. Rust code imports it as `cosmix_log_props`.

## Purpose

The crate gives a service an agent-operable logging surface.

A service registers the reserved `log` namespace with its `PropsRouter` and `SqliteStore`. It then attaches the returned runtime to an initialised `cosmix_log::LogHandle`.

Writes through `<service>.props.set` update the stored singleton. Accepted changes wake a background task, which reads the committed row and replaces the subscriber's active `tracing_subscriber::EnvFilter`.

The crate does not initialise the logging subscriber or create logging sinks. Those responsibilities remain in `cosmix_log`.

## Namespace

The unqualified namespace is `log`. The host router supplies the service prefix, producing `<service>.log`.

The namespace has singleton cardinality. Wire requests use an empty key and responses use the canonical key `current`.

The namespace uses the property store's SQLite backend and simple lifecycle.

| Property | Type | Default | Effect |
| --- | --- | --- | --- |
| `level` | enum | `info` | Sets the global default filter level. |
| `filter` | string | empty | Adds per-target `EnvFilter` directives. |
| `format` | enum | `human` | Records the selected output format; changing it does not reinitialise sinks. |
| `applied_at` | string | empty | Holds an RFC 3339 UTC timestamp value. |
| `applied_by` | enum | `default` | Records an origin value. |
| `source` | string | empty | Holds the literal accepted directive. |

Valid `level` values are:

- `none`
- `error`
- `warn`
- `info`
- `debug`

Valid `format` values are:

- `human`
- `json`

Valid `applied_by` values are:

- `cli`
- `env`
- `props`
- `default`

All six fields are substrate-mutable in version 0.1. The watcher only derives the live filter from `level` and `filter`. It does not write `applied_at`, `applied_by`, or `source` after applying a change.

## Filter behaviour

The watcher combines `level` and `filter` into one `EnvFilter` directive.

An empty `filter` leaves only the global level. A non-empty value is appended after the global level with a comma.

`level=none` maps to the `off` directive. This drops all events while leaving the subscriber installed.

Missing or non-string `level` and `filter` fields fall back to `info` and an empty filter when a stored row is read. This permits a patch containing only one of those fields to produce a usable live filter.

Malformed filter text is rejected before the property write commits. The currently installed filter remains unchanged.

## Validation and deletion

`LogNamespaceHooks` implements the namespace hooks.

Before a set, it:

- requires the body to be an object;
- checks `level`, `format`, and `applied_by` against their fixed variants;
- requires enum and filter values to be strings;
- parses every non-empty `filter` with `EnvFilter::try_new`.

After a successful set, it signals the namespace's private `Notify`. The watcher then reads the committed row and converges on the latest value.

Every delete is rejected. Reset the singleton with `<service>.props.set` using replacement values instead.

The exported error prefixes are:

| Constant | Prefix | Meaning |
| --- | --- | --- |
| `INVALID_FILTER_PREFIX` | `log_filter_invalid:` | The filter does not parse. |
| `UNKNOWN_ENUM_VARIANT_PREFIX` | `log_unknown_enum_variant:` | An enum field contains an unsupported value. |
| `UNDELETABLE_PREFIX` | `log_undeletable:` | A delete was attempted. |

## Public API

### Registration and attachment

`register_log_namespace(router, store) -> Result<LogPropsRuntime>` registers the schema and mapping with the SQLite store, creates its runtime, and registers that runtime with the router.

`attach_props(handle, runtime) -> Result<()>` reads and applies the current row, then starts the live watcher. It returns after the initial read and swap.

If the log handle has no reload handle, `attach_props` returns successfully without starting the watcher.

The spawned watcher owns an `Arc` clone of the property runtime. It runs until the Tokio runtime shuts down. Dropping the caller's `LogPropsRuntime` does not retire it.

### Types

`LogPropsRuntime` bundles the namespace property runtime with its private wake signal. Pass it from `register_log_namespace` to `attach_props`. The `runtime()` accessor exposes the underlying `Arc<Runtime>`.

`LogNamespaceHooks` validates sets, refuses deletes, and signals accepted changes. `LogNamespaceHooks::new` accepts the private `Arc<Notify>`.

### Schema helpers

`namespace_name()` returns the validated `log` namespace name.

`schema()` returns the six-field `PropertySchema`.

`auth_policy(service)` returns the capability policy for the fully qualified namespace.

`spec(service, hooks)` builds the singleton `NamespaceSpec`.

`level_variants()`, `format_variants()`, and `applied_by_variants()` return the fixed enum values used by the schema and hooks.

### Constants

`NAMESPACE` is `log`.

`CANONICAL_KEY` is `current`.

The three validation error-prefix constants are described above.

## Capabilities

The policy builds five capabilities scoped to `<service>.log`:

- `props.read:<service>.log`
- `props.describe:<service>.log:public`
- `props.describe:<service>.log:full`
- `props.write:<service>.log`
- `props.audit:<service>.log`

Version 0.1 grants this complete set to every peer resolved by the policy.

Write access is sensitive. A writer can set `level=none` and silence the service's logging. Deployments must treat `props.write:<service>.log` as a log-control capability.

## Runtime semantics

The initial read has two outcomes:

- If the singleton exists and yields a filter, the watcher installs it before `attach_props` returns.
- If the singleton does not exist, the bootstrap filter remains installed.

Each committed set wakes the watcher through a namespace-private signal. The signal is separate from the property runtime's event signal so the property dispatcher cannot consume the watcher's notification.

Transient read failures and filter-reload failures do not terminate the watcher. A read failure is retried after the next accepted write. A reload failure leaves the previous filter installed.

## Dependencies

The crate uses:

- `cosmix-lib-log` as `cosmix_log` for the subscriber reload handle;
- `cosmix-lib-props-store` for the router, namespace schema, hooks, runtime, values, and SQLite store;
- `tracing` for watcher diagnostics;
- `tracing-subscriber` with `env-filter` support;
- `tokio` for the wake signal and background task;
- `anyhow` for public operation errors.

The crate declares no Cargo features.
