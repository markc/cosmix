# cosmix-lib-log

Unified logging surface for cosmix binaries — the **pure-core half**,
living in the bus repo. One crate, one `init` call, one `LogHandle`,
three sinks (stderr / rolling file / journald), two formats (human /
JSON), a live-reload filter handle, and the full stats recorder / JSONL
sink subsystem.

This crate carries no dependency on cos. The cos-coupled SPEC-12
property surface (`register_log_namespace`, `LogHandle::attach_props`,
the stats-namespace registration) moved out to a cos extension crate;
this crate exposes a `LogReloadHandle` so that extension (or any future
consumer) can drive live filter swaps without restarting the binary.

## Features

- **Core (`default = []`).** Pure logging + stats. CLI flags, sinks,
  filter parsing, JSON / human formats, the `--log-level none` →
  `EnvFilter::off()` path, the `LogReloadHandle` live-reload surface,
  and the stats recorder / JSONL sink. No cos dependency.
  `cargo test --no-default-features` is the gate that keeps this
  locally testable without the mesh.
- **`bus-handlers`.** Exposes the `<svc>.stats.snapshot` Bus verb
  handler (`stats::handle_snapshot_bus`). Pulls `cosmix-lib-client` for
  the `IncomingCommand` wire frame — which lives in bus, so this stays
  a pure-bus feature.
- **`prometheus`.** The per-daemon `/metrics` endpoint
  (`LogHandle::attach_prometheus`). Dependency-pure
  (`metrics-exporter-prometheus` + a `metrics-util` 0.20 alias +
  `tokio` for the WG-only HTTP listener) — decoupled from any cos
  surface.

## Bootstrap (every binary)

```rust
use cosmix_log::{init, LogDefaults, LogOpts, StatsOpts};

#[derive(clap::Parser)]
struct Cli {
    #[command(flatten)]
    log: LogOpts,
    #[command(flatten)]
    stats: StatsOpts,
    // ...binary-specific flags
}

fn main() -> anyhow::Result<()> {
    let cli = <Cli as clap::Parser>::parse();
    let _log = init(&cli.log, &cli.stats, LogDefaults::daemon("cosmix-maild"))?;
    tracing::info!("hello");
    Ok(())
}
```

The returned `LogHandle` carries the appender guards and the live
reload handle. Hold it for the life of the process — dropping it
flushes pending file writes.

## Defaults presets

- `LogDefaults::daemon(id)` — info/json, **journald-primary** (no
  rolling file), stats recorder on. Per the standing directive, every
  daemon logs to journald and leans on systemd; an operator opts into a
  file sink via `with_log_file(...)`.
- `LogDefaults::serve(id)` — for the Mix `--serve` runtime: journald +
  human format, stats recorder off.
- `LogDefaults::gui(id)` — warn/human, no file, journald off, stats off.
- `LogDefaults::default()` — GUI-class one-shot / mix-REPL defaults.

## Live filter reload

`init` always installs a subscriber — even `--log-level none` installs
`EnvFilter::off()` behind the reload layer — so `LogHandle::reload_handle()`
returns a `LogReloadHandle` on every successful init. A consumer (the
cos `cosmix-lib-log-props` watcher, or a future Mix Bus verb) raises or
retunes the filter at runtime:

```rust
if let Some(rh) = log.reload_handle() {
    rh.reload_filter("cosmix_maild=debug,cosmix_bus=warn".parse()?)?;
}
```

## `log` → tracing bridge

`init` installs a `tracing_log::LogTracer` (via tracing-subscriber's
`tracing-log` feature) so records emitted through the `log` facade by
dependencies are routed into tracing and reach the same sinks.

## Log levels

Six levels: `none | error | warn | info | debug | trace`. `none`
installs `EnvFilter::off()` behind the reload layer (a peer of the other
levels, not a derived state).
