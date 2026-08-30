# cosmix-lib-log

`cosmix-lib-log` is the shared logging and process-statistics library for Cosmix binaries. It installs a reloadable `tracing` subscriber, selects stderr, rolling-file, and journald sinks, bridges records from the `log` facade, and can install a cardinality-bounded `metrics` recorder with JSONL, snapshot, Bus, and Prometheus surfaces. The crate lives in the `bus` protocol layer of the `bus <- mix <- cos` dependency chain: its core has no dependency on `mix` or `cos`, and its optional Bus handler depends only on `cosmix-lib-client` in the same layer.

The Cargo package is `cosmix-lib-log`; its Rust library name is `cosmix_log`.

## Features

| Feature | Default | Provides |
|---|---:|---|
| `default` | Yes | Empty feature set. Core logging, reload, stats recorder, snapshots, roll-ups, and JSONL support remain available. |
| `bus-handlers` | No | `stats::handle_snapshot_bus`, `stats::SnapshotCaps`, and the `<svc>.stats.snapshot` wire adapter. Adds `cosmix-lib-client`. |
| `prometheus` | No | `LogHandle::attach_prometheus`, `stats::PrometheusChild`, and the HTTP `/metrics` exporter. Adds the Prometheus exporter, its matching `metrics-util`, and Tokio. |

## Logging bootstrap

`init` is the normal entry point:

```rust
use cosmix_log::{init, LogDefaults, LogOpts, StatsOpts};

#[derive(clap::Parser)]
struct Cli {
    #[command(flatten)]
    log: LogOpts,

    #[command(flatten)]
    stats: StatsOpts,
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let cli = <Cli as clap::Parser>::parse();
    let _log = init(
        &cli.log,
        &cli.stats,
        LogDefaults::daemon("alpha-service"),
    )?;

    tracing::info!("service started");
    Ok(())
}
```

Hold the returned `LogHandle` for the process lifetime. It owns non-blocking appender guards, flushes file output when dropped, and performs the final stats roll-up and sink flush. Call `LogHandle::shutdown` explicitly before `std::process::exit`, because that function skips destructors.

When stats are enabled, `init` installs the stats recorder before it installs the tracing subscriber. Admitted tracing events therefore reach the built-in event counter from the first event.

stdout is reserved for protocol output. Logging sinks write to stderr, files, or journald.

## Filter selection and reload

The bootstrap filter precedence is:

1. A non-empty `RUST_LOG` value.
2. `LogOpts.log_filter`, appended to the selected base directive.
3. `LogOpts.log_level`.
4. `LogDefaults.default_filter`, or a directive derived from `default_target` and `level`.

Later directives in an `EnvFilter` take precedence over earlier directives. `LogLevel` accepts `none`, `error`, `warn`, `info`, `debug`, and `trace`.

`none` installs an `EnvFilter::off()` subscriber behind the reload layer. It does not remove the subscriber, so a later filter reload can enable logging without a restart.

```rust
let reload = log.reload_handle().expect("subscriber installed");
reload.reload_filter("alpha_service=debug,cosmix_bus=warn".parse()?)?;
```

`LogReloadHandle` is cloneable. `reload_filter` returns `LogError::Reload` if its subscriber no longer exists.

## Sinks and formats

`LogFormat` selects `human` or `json` output. `TriState` supplies `auto`, `always`, and `never` for stderr and colour selection.

The rolling-file sink uses `RotationMode::Daily` or `RotationMode::Never`. File and journald setup failures are non-fatal: the crate reports the failure to stderr when possible and continues with the remaining sinks.

The `tracing-subscriber` `tracing-log` integration installs the `log`-to-`tracing` bridge. Dependencies that emit through the `log` facade therefore use the same filters and sinks.

## Defaults

| Constructor | Level and format | Sinks | Stats |
|---|---|---|---|
| `LogDefaults::daemon(identity)` | `info`, JSON | journald; no rolling file | Enabled, 60-second interval value, no JSONL file |
| `LogDefaults::serve(identity)` | `info`, human | journald; no rolling file | Disabled |
| `LogDefaults::gui(identity)` | `warn`, human | stderr fallback; no file or journald | Disabled |
| `LogDefaults::default()` | `warn`, human | stderr fallback; no file or journald | Disabled |

Builder-style methods set the Bus service, log directory, rotation, journald state, level, format, baseline filter, stats state, roll-up interval, stats file, byte budget, and Prometheus listener.

`default_log_dir()` returns `$HOME/.local/log/cosmix` and falls back to `/tmp/cosmix-log` when `HOME` is unset.

## Flattened logging options

`LogOpts` is a `clap::Args` type. Every field is optional; an absent flag leaves the corresponding `LogDefaults` value in force.

| Flag | Values or effect |
|---|---|
| `--log-level` | `none`, `error`, `warn`, `info`, `debug`, or `trace` |
| `--log-filter` | An `EnvFilter` directive appended to the base filter |
| `--log-format` | `human` or `json` |
| `--log-stderr` | `auto`, `always`, or `never` |
| `--log-file` | Rolling-file directory; an empty value disables the sink |
| `--log-journald` | Boolean journald override |
| `--log-color` | `auto`, `always`, or `never` |

In automatic stderr mode, stderr is enabled when journald is not active. This avoids duplicate entries when journald is available and preserves a fallback when it is not.

## Flattened stats options

`StatsOpts` is independent of `LogOpts`. A binary includes it by flattening it into its own parser.

| Flag | Values or effect |
|---|---|
| `--stats` | `off` or `on` |
| `--stats-interval` | Roll-up seconds; `0` means final flush only, otherwise `1..=3600` |
| `--stats-file` | JSONL output stem; an empty value disables disk persistence |
| `--stats-on-exit` | Parsed as `off` or `on`; the current `init` path does not consult it |
| `--stats-byte-budget` | Daily soft budget in MiB; accepted range is 16 through 1024 |
| `--stats-prometheus-listen` | Listener override exposed to the caller; `init` does not attach the exporter |

Invalid filter syntax, double subscriber installation, failed reloads, and invalid stats bounds are represented by `LogError`.

`init` validates `stats_interval` but does not schedule periodic roll-ups. The host runtime calls `stats::perform_rollup` at its chosen cadence. `LogHandle` always performs the final roll-up from `shutdown` and `Drop`.

## Stats recorder

`stats::StatsRecorderBuilder` constructs a `StatsRecorder` for a service identity. It accepts a default per-metric cardinality cap and named overrides. Valid caps range from `CARDINALITY_FLOOR` (16) through `CARDINALITY_CEILING` (4096); the default is `CARDINALITY_DEFAULT` (1024).

`StatsRecorder::install` claims the process-wide `metrics::Recorder` slot. Only one recorder can be installed. `StatsRecorder::add_sink` registers `Arc<dyn StatsSink>` backends, while `stats::add_sink_to_installed` adds a sink to the process-installed recorder.

The recorder accepts the standard `metrics` counter, gauge, and histogram macros. New label sets above a metric's cap receive no-op handles and increment `cosmix_stats_cardinality_drops_total`.

## Classification and redaction

Call `stats::classify(name, LabelSensitivity::Safe)` for metric families whose label values are bounded and non-sensitive. Unclassified families default to `Restricted`.

Restricted families use `SeriesLabels::Hash` on cross-process and JSONL surfaces unless the caller is authorised for raw labels. `stats::labels_hash` returns the canonical, lowercase, 16-character FxHash digest; `stats::labels_hash_bytes` exposes the frozen length-prefixed input encoding.

The digest is a stable correlation identifier, not a cryptographic redaction primitive.

## Snapshots and roll-ups

`stats::local_snapshot()` returns the installed recorder as a `Snapshot`. In-process snapshots always carry raw labels. If no recorder is installed, it returns an empty snapshot.

The snapshot model consists of:

- `Snapshot`, containing the service identity, capture time, and metric families.
- `MetricFamily`, containing the name, kind, optional description, and series.
- `Series`, with `SeriesLabels` and a `SeriesValue`.
- `SeriesValue`, covering counters, gauges, and histogram summaries.
- `HistogramSummary`, containing count, sum, p50, p95, and p99.

`stats::snapshot_dispatch` applies an optional `MetricPattern`, label predicates, label hashes, and raw-label capability state to a recorder snapshot. Exact, prefix, and suffix metric patterns are supported.

`stats::perform_rollup` sends one `PeriodSnapshot` to every registered `StatsSink`. Each `PeriodRecord` carries both the cumulative value and the current-period delta. `flush_all_sinks` flushes all sinks, and `shutdown_installed_recorder` performs the final zero-second roll-up before flushing.

## JSONL sink

`stats::JsonlSink::daemon` and `stats::JsonlSink::delta` create per-process JSONL sinks. The live filename ends in `.jsonl.open`; `StatsSink::flush` fsyncs it and renames it to `.jsonl.done`.

The configurable byte budget is a soft daily warning threshold. `HARD_BUDGET_BYTES` fixes the hard daily ceiling at 1 GiB; crossing it pauses disk appends until the next UTC day without stopping in-memory recording.

## Bus snapshot handler

With `bus-handlers`, `stats::handle_snapshot_bus` handles `<svc>.stats.snapshot`. The request body is empty and the optional headers are:

| Header | Form |
|---|---|
| `metric` | Exact name, `prefix*`, or `*suffix` |
| `labels` | JSON object; string values mean exact match and `null` means key presence |
| `labels_hash` | Comma-separated 16-character hexadecimal digests |

`SnapshotCaps.has_snapshot` controls admission to the verb. `SnapshotCaps.has_raw_labels` permits raw labels for restricted families and exact-value filters that would otherwise create a probe oracle.

The handler returns the `(rc, body)` pair expected by the broker client response path. Successful bodies contain `service`, `captured_at`, and `metrics`. Responses larger than `SNAPSHOT_MAX_RESPONSE_BYTES` (1 MiB) are rejected.

## Prometheus exporter

With `prometheus`, call `LogHandle::attach_prometheus` after `init`, with stats enabled and a Tokio runtime active:

```rust
let log = log
    .attach_prometheus("192.0.2.5:9100".parse()?)
    .await?;
```

The method consumes and returns the `LogHandle`. It attaches one redaction-first Prometheus child to the installed stats recorder and starts the HTTP listener. The caller supplies the bind address explicitly; the defaults do not select one.
