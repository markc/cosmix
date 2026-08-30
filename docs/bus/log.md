# cosmix-lib-log — logging and statistics

**`cosmix-lib-log` is the common logging bootstrap for Cosmix binaries.** It
combines CLI options, per-binary defaults, tracing filters, stderr / rolling
file / journald sinks, live reload, and the core metrics recorder behind one
`init` call.

It is the pure-core logging half: there is no dependency on cos, and stdout is
never used for logs because it is reserved for protocol output.

## What it is

Consumers flatten `LogOpts` and, when needed, `StatsOpts` into their Clap
parser, then call:

```rust
let mut log = cosmix_log::init(
    &cli.log,
    &cli.stats,
    cosmix_log::LogDefaults::daemon("cosmix-maild"),
)?;
```

The returned `LogHandle` owns non-blocking appender guards and the installed
stats lifecycle. Hold it for the process lifetime. `shutdown()` performs the
final stats roll-up and sink flush; call it explicitly before
`std::process::exit`, which skips destructors.

## What it does

- Parses `none | error | warn | info | debug | trace`, human or JSON output, `EnvFilter` directives, sink selection, rotation, and ANSI colour.
- Resolves the bootstrap filter as `RUST_LOG` over CLI over `LogDefaults`.
- Routes records from the `log` facade into the same tracing subscriber.
- Supports stderr, daily or non-rotating file, and journald sinks without writing to stdout.
- Returns a `LogReloadHandle`; `reload_filter` swaps the active `EnvFilter` without restarting.
- Installs the core `StatsRecorder`, cardinality controls, process gauges, snapshots, roll-ups, and optional JSONL persistence when stats are enabled.

The presets are `LogDefaults::daemon` (info/JSON, journald, stats on),
`LogDefaults::serve` (info/human, journald, stats off), and
`LogDefaults::gui` (warn/human, no file or journald, stats off). Each preset is
customisable through `with_*` builders.

## Optional features

| Feature | What it adds |
|---|---|
| default | Logging, live filter reload, metrics facade recorder, snapshots, roll-up, and JSONL stats sink. |
| `bus-handlers` | `stats::handle_snapshot_bus` and `SnapshotCaps` for a `<svc>.stats.snapshot` verb. |
| `prometheus` | `LogHandle::attach_prometheus`, which attaches a redaction-first child recorder and serves `/metrics` on the caller-supplied address. |

The cos-coupled SPEC 12 log property namespace is not in this crate.
`cosmix-lib-log-props` in cos registers that namespace and drives runtime filter
changes through `LogReloadHandle`.

## See also

- [client](client.md) — the Bus command type used by the optional stats handler
- [property core](props-core.md) — the protocol-side property types
- [overview](overview.md) — the crate family and protocol boundary
