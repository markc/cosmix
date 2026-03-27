//! cosmix-mond — Headless system monitor daemon for the cosmix appmesh.
//!
//! Registers as "mon" on the local hub and responds to:
//! - `mon.status` — CPU, memory, disk, load average, uptime
//! - `mon.processes` — top processes by CPU usage
//!
//! Designed to run on all mesh nodes (headless servers + workstations).
//! The companion `cosmix-mon` GUI queries this daemon via the hub.

use std::sync::Arc;

use anyhow::Result;
use clap::Parser;
use serde::Serialize;
use sysinfo::System;

#[global_allocator]
static GLOBAL: mimalloc::MiMalloc = mimalloc::MiMalloc;

// ── CLI ──

#[derive(Parser)]
#[command(name = "cosmix-mond", about = "Headless system monitor daemon for the cosmix appmesh")]
struct Cli {
    /// Hub WebSocket URL
    #[arg(long, default_value = "ws://localhost:4200/ws")]
    hub_url: String,

    /// Service name to register on the hub
    #[arg(long, default_value = "mon")]
    service_name: String,
}

// ── System data ──

#[derive(Clone, Debug, Serialize)]
struct SystemStatus {
    hostname: String,
    uptime_secs: u64,
    cpu_count: usize,
    cpu_usage: f32,
    mem_total_mb: u64,
    mem_used_mb: u64,
    mem_percent: f32,
    swap_total_mb: u64,
    swap_used_mb: u64,
    disks: Vec<DiskInfo>,
    load_avg: [f64; 3],
}

#[derive(Clone, Debug, Serialize)]
struct DiskInfo {
    mount: String,
    total_gb: f64,
    used_gb: f64,
    percent: f32,
}

#[derive(Clone, Debug, Serialize)]
struct ProcessInfo {
    pid: u32,
    name: String,
    cpu: f32,
    mem_mb: u64,
}

fn gather_status() -> SystemStatus {
    let mut sys = System::new_all();
    sys.refresh_all();

    let cpu_usage = sys.global_cpu_usage();
    let mem_total = sys.total_memory();
    let mem_used = sys.used_memory();
    let swap_total = sys.total_swap();
    let swap_used = sys.used_swap();

    let disks: Vec<DiskInfo> = sysinfo::Disks::new_with_refreshed_list()
        .iter()
        .filter(|d| {
            let mp = d.mount_point().to_string_lossy();
            mp == "/" || mp.starts_with("/home") || mp.starts_with("/data")
        })
        .map(|d| {
            let total = d.total_space() as f64 / 1_073_741_824.0;
            let used = (d.total_space() - d.available_space()) as f64 / 1_073_741_824.0;
            let pct = if total > 0.0 { (used / total * 100.0) as f32 } else { 0.0 };
            DiskInfo {
                mount: d.mount_point().to_string_lossy().to_string(),
                total_gb: (total * 10.0).round() / 10.0,
                used_gb: (used * 10.0).round() / 10.0,
                percent: pct,
            }
        })
        .collect();

    let load = System::load_average();

    SystemStatus {
        hostname: System::host_name().unwrap_or_else(|| "unknown".into()),
        uptime_secs: System::uptime(),
        cpu_count: sys.cpus().len(),
        cpu_usage,
        mem_total_mb: mem_total / 1_048_576,
        mem_used_mb: mem_used / 1_048_576,
        mem_percent: if mem_total > 0 { (mem_used as f32 / mem_total as f32) * 100.0 } else { 0.0 },
        swap_total_mb: swap_total / 1_048_576,
        swap_used_mb: swap_used / 1_048_576,
        disks,
        load_avg: [load.one, load.five, load.fifteen],
    }
}

fn gather_processes(limit: usize) -> Vec<ProcessInfo> {
    let mut sys = System::new_all();
    sys.refresh_all();

    let mut procs: Vec<ProcessInfo> = sys.processes().values().map(|p| {
        ProcessInfo {
            pid: p.pid().as_u32(),
            name: p.name().to_string_lossy().to_string(),
            cpu: p.cpu_usage(),
            mem_mb: p.memory() / 1_048_576,
        }
    }).collect();

    procs.sort_by(|a, b| b.cpu.partial_cmp(&a.cpu).unwrap_or(std::cmp::Ordering::Equal));
    procs.truncate(limit);
    procs
}

// ── Hub command handling ──

async fn handle_hub_commands(client: Arc<cosmix_client::HubClient>) {
    let mut rx = match client.incoming_async().await {
        Some(rx) => rx,
        None => return,
    };

    while let Some(cmd) = rx.recv().await {
        let result = match cmd.command.as_str() {
            "mon.status" => {
                let status = gather_status();
                serde_json::to_string(&status).map_err(|e| e.to_string())
            }
            "mon.processes" => {
                let limit = cmd.args.get("limit")
                    .and_then(|v| v.as_u64())
                    .unwrap_or(15) as usize;
                let procs = gather_processes(limit);
                serde_json::to_string(&procs).map_err(|e| e.to_string())
            }
            _ => Err(format!("unknown command: {}", cmd.command)),
        };

        match result {
            Ok(body) => {
                if let Err(e) = client.respond(&cmd, 0, &body).await {
                    tracing::warn!("failed to send response: {e}");
                }
            }
            Err(msg) => {
                let err_body = serde_json::json!({"error": msg}).to_string();
                if let Err(e) = client.respond(&cmd, 10, &err_body).await {
                    tracing::warn!("failed to send error response: {e}");
                }
            }
        }
    }
}

// ── Main ──

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "cosmix_mond=info".into()),
        )
        .init();

    let cli = Cli::parse();

    tracing::info!(
        service = %cli.service_name,
        hub = %cli.hub_url,
        "Starting cosmix-mond"
    );

    let client = cosmix_client::HubClient::connect(&cli.service_name, &cli.hub_url).await?;
    let client = Arc::new(client);

    tracing::info!(
        service = %cli.service_name,
        "Registered on hub, serving mon.status and mon.processes"
    );

    // Run the command handler until the hub connection drops
    handle_hub_commands(client).await;

    tracing::info!("Hub connection closed, exiting");
    Ok(())
}
