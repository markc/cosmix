//! cosmix-logd — AMP traffic logger daemon.
//!
//! Registers on the hub, subscribes to all AMP traffic via `hub.tap`,
//! and writes each message to `~/.local/log/cosmix/amp.log` in a
//! human-readable, grep-friendly format.

use std::io::Write;
use std::sync::Arc;

use chrono::Local;
use cosmix_amp::amp;

#[global_allocator]
static GLOBAL: mimalloc::MiMalloc = mimalloc::MiMalloc;

#[tokio::main]
async fn main() {
    let _log = cosmix_daemon::init_tracing("cosmix_logd");

    tracing::info!("Starting cosmix-logd");

    let log_dir = cosmix_daemon::log_dir();
    let _ = std::fs::create_dir_all(&log_dir);
    let amp_log_path = log_dir.join("amp.log");

    // Connect to hub
    let client = match cosmix_client::HubClient::connect_default("log").await {
        Ok(c) => Arc::new(c),
        Err(e) => {
            tracing::error!("Failed to connect to hub: {e}");
            return;
        }
    };

    tracing::info!("Connected to hub as 'log'");

    // Subscribe to AMP traffic tap
    match client.call("hub", "hub.tap", serde_json::Value::Null).await {
        Ok(_) => tracing::info!("Subscribed to AMP traffic tap"),
        Err(e) => {
            tracing::error!("Failed to subscribe to tap: {e}");
            return;
        }
    }

    // Take the incoming message stream
    let Some(mut rx) = client.incoming_async().await else {
        tracing::error!("Failed to get incoming stream");
        return;
    };

    // Process tapped messages
    while let Some(cmd) = rx.recv().await {
        let now = Local::now().format("%Y-%m-%d %H:%M:%S%.3f");
        let from = &cmd.from;
        let command = &cmd.command;
        let body_len = cmd.body.len();

        let line = format!(
            "[{now}] {from} → {command}  body={body_len}B\n"
        );

        // Append to amp.log
        if let Ok(mut file) = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&amp_log_path)
        {
            let _ = file.write_all(line.as_bytes());
        }

        // Also log via tracing for stderr/file output
        tracing::debug!("{from} → {command} ({body_len}B)");
    }

    tracing::info!("Hub connection closed, exiting");
}
