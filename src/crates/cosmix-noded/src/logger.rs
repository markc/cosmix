//! Logger module — observes Bus traffic and writes to bus.log.
//!
//! Two separate broker connections by design:
//!
//! 1. **Service connection** registers as `log` and serves the `log.props.*`
//!    surface. Low-volume request/response — must deliver.
//! 2. **Observer connection** registers under a process-scoped identity,
//!    subscribes to metadata-only `noded.observe`, and receives the bounded,
//!    drop-accounted broker stream. It requires an operator allowlist entry
//!    matching `log-observe-*`.
//!
//! Splitting the connections gives each its own bounded outbound mpsc, so a
//! slow observer consumer (disk contention, log file rotation) can never starve
//! the props surface or trigger the symmetric route_local + broadcast_tap
//! drop pattern that crashed the box on 2026-04-28.
//!
//! Each tap message is appended to `~/.local/log/cosmix/bus.log`.

use std::io::Write;
use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::time::Instant;

use anyhow::Result;
use chrono::Local;

use crate::log_props::{LogPropsSnapshot, LogStats};

#[derive(serde::Deserialize)]
struct ObserveEvent {
    ts: String,
    #[serde(default)]
    from: Option<String>,
    #[serde(default)]
    to: Option<String>,
    #[serde(default)]
    verb: Option<String>,
    size: usize,
    dropped_count: u64,
}

pub async fn run(noded_url: &str) -> Result<()> {
    let started_instant = Instant::now();
    let started_at_iso = chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Secs, true);

    let log_dir = cosmix_daemon::log_dir();
    let _ = std::fs::create_dir_all(&log_dir);
    let bus_log_path = log_dir.join("bus.log");
    let log_path_str = bus_log_path.to_string_lossy().to_string();

    let stats = LogStats::new();

    // Connection 1 — registered service for log.props.* request/response.
    let svc_client = Arc::new(cosmix_client::NodedClient::connect("log", noded_url).await?);
    tracing::info!("Logger props connection registered as 'log'");

    // Connection 2 — registered observer only. Isolating it onto its own
    // bounded outbound channel means a saturated observation queue cannot
    // block log.props.* delivery on the service connection.
    let observer_name = format!("log-observe-{}", std::process::id());
    let observer_client =
        Arc::new(cosmix_client::NodedClient::connect(&observer_name, noded_url).await?);
    observer_client
        .call(
            "noded",
            "noded.observe.start",
            serde_json::json!({
                "filter": {
                    "directions": ["local", "mesh_in", "mesh_out"],
                },
                "body": "none",
                "capacity": 1024,
            }),
        )
        .await?;
    stats.tap_subscribed.store(true, Ordering::Relaxed);
    tracing::info!("Logger observer connection subscribed to Bus traffic");

    // Spawn the props-surface task on the service connection.
    let svc_stats = stats.clone();
    let svc_log_path = log_path_str.clone();
    let svc_started_at = started_at_iso.clone();
    let svc_task = tokio::spawn(async move {
        let Some(mut rx) = svc_client.incoming_async().await else {
            tracing::error!("Logger service connection has no incoming stream");
            return;
        };
        while let Some(cmd) = rx.recv().await {
            let Some(suffix) = cmd.command.strip_prefix("log.props.") else {
                // Service connection only handles log.props.* — anything
                // else is an addressing mistake by the caller. Ignore.
                continue;
            };
            let snapshot = LogPropsSnapshot {
                log_path: svc_log_path.clone(),
                started_at: svc_started_at.clone(),
                uptime_s: started_instant.elapsed().as_secs(),
                tap_subscribed: svc_stats.tap_subscribed.load(Ordering::Relaxed),
                events_seen: svc_stats.events_seen.load(Ordering::Relaxed),
                bytes_logged: svc_stats.bytes_logged.load(Ordering::Relaxed),
            };
            let args_json = crate::log_props::parse_args(cmd.header("args"))
                .or_else(|| serde_json::from_str(&cmd.body).ok());
            let resp_inner = cosmix_props::bus::dispatch_props(
                &snapshot,
                suffix,
                args_json.as_ref(),
                /* redact = */ true,
            );
            let rc_u8: u8 = resp_inner.rc.clamp(0, 255) as u8;
            if let Err(e) = svc_client.respond(&cmd, rc_u8, &resp_inner.body).await {
                tracing::warn!("log.props.{suffix} respond failed: {e}");
            }
        }
        tracing::info!("Logger service connection closed");
    });

    // Spawn the observation-write task on the dedicated connection.
    let tap_stats = stats.clone();
    let tap_log_path = bus_log_path.clone();
    let tap_task = tokio::spawn(async move {
        let Some(mut rx) = observer_client.incoming_async().await else {
            tracing::error!("Logger observer connection has no incoming stream");
            return;
        };
        while let Some(cmd) = rx.recv().await {
            if cmd.command != "noded.observe.event" {
                continue;
            }
            let Ok(event) = serde_json::from_str::<ObserveEvent>(&cmd.body) else {
                tracing::warn!("Logger received malformed noded.observe.event");
                continue;
            };
            let now = if event.ts.is_empty() {
                Local::now().format("%Y-%m-%d %H:%M:%S%.3f").to_string()
            } else {
                event.ts
            };
            let from = event.from.as_deref().unwrap_or("?");
            let to = event.to.as_deref().unwrap_or("?");
            let verb = event.verb.as_deref().unwrap_or("?");
            let dropped = if event.dropped_count == 0 {
                String::new()
            } else {
                format!(" dropped={}", event.dropped_count)
            };
            let line = format!(
                "[{now}] {from} → {to}  {verb}  wire={}B{dropped}\n",
                event.size
            );

            if let Ok(mut file) = std::fs::OpenOptions::new()
                .create(true)
                .append(true)
                .open(&tap_log_path)
                && file.write_all(line.as_bytes()).is_ok()
            {
                tap_stats.record(line.len() as u64);
            }
        }
        tap_stats.tap_subscribed.store(false, Ordering::Relaxed);
        tracing::info!("Logger observer connection closed");
    });

    let _ = tokio::join!(svc_task, tap_task);
    tracing::info!("Logger module stopped");
    Ok(())
}
