//! cosmix-backup — Proxmox Backup Server dashboard for the cosmix appmesh.
//!
//! Registers as "backup" on the hub. Handles:
//! - `backup.datastores` — list datastores with usage stats
//! - `backup.snapshots` — list snapshots for a datastore
//! - `backup.tasks` — list recent backup tasks
//!
//! Talks to PBS API. Accessible from any browser on the mesh.

use std::sync::Arc;

use chrono::{DateTime, Utc};
use dioxus::prelude::*;
use serde::{Deserialize, Serialize};

fn main() {
    cosmix_ui::desktop::init_linux_env();

    #[cfg(feature = "desktop")]
    {
        let cfg = cosmix_ui::desktop::window_config("cosmix-backup", 960.0, 640.0);
        LaunchBuilder::new().with_cfg(cfg).launch(app);
        return;
    }

    #[allow(unreachable_code)]
    {
        eprintln!("Desktop feature not enabled");
        std::process::exit(1);
    }
}

// ── PBS API client ──

fn pbs_url() -> String {
    std::env::var("PBS_API_URL").unwrap_or_else(|_| "https://localhost:8007".to_string())
}

fn pbs_token() -> String {
    // Format: PBSAPIToken=user@realm!tokenname:uuid
    std::env::var("PBS_API_TOKEN").unwrap_or_default()
}

fn pbs_client() -> reqwest::Client {
    reqwest::Client::builder()
        .danger_accept_invalid_certs(true) // PBS uses self-signed certs
        .build()
        .unwrap_or_else(|_| reqwest::Client::new())
}

#[derive(Clone, Debug, Serialize, Deserialize)]
struct Datastore {
    #[serde(default)]
    store: String,
    #[serde(default)]
    total: u64,
    #[serde(default)]
    used: u64,
    #[serde(default)]
    avail: u64,
    #[serde(default)]
    gc_status: Option<GcStatus>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
struct GcStatus {
    #[serde(default)]
    upid: Option<String>,
    #[serde(default, rename = "removed-bytes")]
    removed_bytes: u64,
    #[serde(default, rename = "removed-chunks")]
    removed_chunks: u64,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
struct Snapshot {
    #[serde(rename = "backup-type", default)]
    backup_type: String,
    #[serde(rename = "backup-id", default)]
    backup_id: String,
    #[serde(rename = "backup-time", default)]
    backup_time: i64,
    #[serde(default)]
    size: Option<u64>,
    #[serde(default)]
    files: Option<Vec<serde_json::Value>>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
struct BackupTask {
    upid: String,
    #[serde(default)]
    node: String,
    #[serde(default)]
    user: String,
    #[serde(rename = "worker_type", default)]
    worker_type: String,
    #[serde(rename = "worker_id", default)]
    worker_id: Option<String>,
    #[serde(default)]
    status: Option<String>,
    #[serde(default)]
    starttime: i64,
    #[serde(default)]
    endtime: Option<i64>,
}

#[derive(Clone, Debug, Deserialize)]
struct PbsResponse<T> {
    data: T,
}

async fn fetch_datastores() -> Result<Vec<Datastore>, String> {
    let client = pbs_client();
    let resp = client
        .get(format!("{}/api2/json/status/datastore-usage", pbs_url()))
        .header("Authorization", format!("PBSAPIToken={}", pbs_token()))
        .send()
        .await
        .map_err(|e| e.to_string())?;

    if !resp.status().is_success() {
        return Err(format!("PBS API error: {}", resp.status()));
    }

    let body: PbsResponse<Vec<Datastore>> = resp.json().await.map_err(|e| e.to_string())?;
    Ok(body.data)
}

async fn fetch_snapshots(store: &str) -> Result<Vec<Snapshot>, String> {
    let client = pbs_client();
    let resp = client
        .get(format!(
            "{}/api2/json/admin/datastore/{}/snapshots",
            pbs_url(),
            store
        ))
        .header("Authorization", format!("PBSAPIToken={}", pbs_token()))
        .send()
        .await
        .map_err(|e| e.to_string())?;

    if !resp.status().is_success() {
        return Err(format!("PBS API error: {}", resp.status()));
    }

    let body: PbsResponse<Vec<Snapshot>> = resp.json().await.map_err(|e| e.to_string())?;
    Ok(body.data)
}

async fn fetch_tasks() -> Result<Vec<BackupTask>, String> {
    let client = pbs_client();
    let resp = client
        .get(format!("{}/api2/json/nodes/localhost/tasks", pbs_url()))
        .query(&[("limit", "50")])
        .header("Authorization", format!("PBSAPIToken={}", pbs_token()))
        .send()
        .await
        .map_err(|e| e.to_string())?;

    if !resp.status().is_success() {
        return Err(format!("PBS API error: {}", resp.status()));
    }

    let body: PbsResponse<Vec<BackupTask>> = resp.json().await.map_err(|e| e.to_string())?;
    Ok(body.data)
}

fn format_bytes(bytes: u64) -> String {
    if bytes >= 1_099_511_627_776 {
        format!("{:.1} TiB", bytes as f64 / 1_099_511_627_776.0)
    } else if bytes >= 1_073_741_824 {
        format!("{:.1} GiB", bytes as f64 / 1_073_741_824.0)
    } else if bytes >= 1_048_576 {
        format!("{:.1} MiB", bytes as f64 / 1_048_576.0)
    } else {
        format!("{:.0} KiB", bytes as f64 / 1024.0)
    }
}

fn format_timestamp(ts: i64) -> String {
    DateTime::<Utc>::from_timestamp(ts, 0)
        .map(|dt| dt.format("%Y-%m-%d %H:%M").to_string())
        .unwrap_or_else(|| "-".to_string())
}

fn usage_percent(used: u64, total: u64) -> f32 {
    if total == 0 {
        0.0
    } else {
        (used as f32 / total as f32) * 100.0
    }
}

fn pct_color(pct: f32) -> &'static str {
    if pct > 90.0 {
        "#ef4444"
    } else if pct > 70.0 {
        "#f59e0b"
    } else {
        "#22c55e"
    }
}

fn task_status_color(status: &str) -> &'static str {
    if status == "OK" {
        "#22c55e"
    } else if status.starts_with("WARNINGS") {
        "#f59e0b"
    } else if status.is_empty() {
        "#60a5fa" // running
    } else {
        "#ef4444" // error
    }
}

// ── Hub command handling ──

async fn handle_hub_commands(client: Arc<cosmix_client::HubClient>) {
    let mut rx = match client.incoming_async().await {
        Some(rx) => rx,
        None => return,
    };

    while let Some(cmd) = rx.recv().await {
        let result = match cmd.command.as_str() {
            "backup.datastores" => match fetch_datastores().await {
                Ok(ds) => serde_json::to_string(&ds).map_err(|e| e.to_string()),
                Err(e) => Err(e),
            },
            "backup.snapshots" => {
                let store = cmd
                    .args
                    .get("store")
                    .and_then(|v| v.as_str())
                    .unwrap_or("");
                if store.is_empty() {
                    Err("missing store argument".into())
                } else {
                    match fetch_snapshots(store).await {
                        Ok(snaps) => serde_json::to_string(&snaps).map_err(|e| e.to_string()),
                        Err(e) => Err(e),
                    }
                }
            }
            "backup.tasks" => match fetch_tasks().await {
                Ok(tasks) => serde_json::to_string(&tasks).map_err(|e| e.to_string()),
                Err(e) => Err(e),
            },
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

// ── UI ──

#[derive(Clone, PartialEq)]
enum View {
    Datastores,
    Snapshots(String),
    Tasks,
}

fn app() -> Element {
    let mut datastores: Signal<Vec<Datastore>> = use_signal(Vec::new);
    let mut snapshots: Signal<Vec<Snapshot>> = use_signal(Vec::new);
    let mut tasks: Signal<Vec<BackupTask>> = use_signal(Vec::new);
    let mut view: Signal<View> = use_signal(|| View::Datastores);
    let mut error_msg: Signal<Option<String>> = use_signal(|| None);
    let mut loading = use_signal(|| false);
    let mut hub_client: Signal<Option<Arc<cosmix_client::HubClient>>> = use_signal(|| None);

    // Connect to hub + load datastores
    use_effect(move || {
        spawn(async move {
            match cosmix_client::HubClient::connect_default("backup").await {
                Ok(client) => {
                    let client = Arc::new(client);
                    hub_client.set(Some(client.clone()));
                    tracing::info!("connected to cosmix-hub as 'backup'");
                    tokio::spawn(handle_hub_commands(client));
                }
                Err(_) => {
                    tracing::debug!("hub not available, running standalone");
                }
            }
        });

        spawn(async move {
            loading.set(true);
            match fetch_datastores().await {
                Ok(ds) => {
                    datastores.set(ds);
                    error_msg.set(None);
                }
                Err(e) => error_msg.set(Some(e)),
            }
            loading.set(false);
        });
    });

    let mut show_snapshots = move |store: String| {
        view.set(View::Snapshots(store.clone()));
        spawn(async move {
            loading.set(true);
            match fetch_snapshots(&store).await {
                Ok(s) => {
                    snapshots.set(s);
                    error_msg.set(None);
                }
                Err(e) => error_msg.set(Some(e)),
            }
            loading.set(false);
        });
    };

    let show_tasks = move |_| {
        view.set(View::Tasks);
        spawn(async move {
            loading.set(true);
            match fetch_tasks().await {
                Ok(t) => {
                    tasks.set(t);
                    error_msg.set(None);
                }
                Err(e) => error_msg.set(Some(e)),
            }
            loading.set(false);
        });
    };

    let show_datastores = move |_| {
        view.set(View::Datastores);
        spawn(async move {
            loading.set(true);
            match fetch_datastores().await {
                Ok(ds) => {
                    datastores.set(ds);
                    error_msg.set(None);
                }
                Err(e) => error_msg.set(Some(e)),
            }
            loading.set(false);
        });
    };

    rsx! {
        document::Style { "{CSS}" }
        div {
            style: "width:100%; height:100vh; display:flex; flex-direction:column; background:{BG_BASE}; color:{TEXT_PRIMARY}; font-family:-apple-system,BlinkMacSystemFont,'Segoe UI',Helvetica,Arial,sans-serif; font-size:13px;",

            // Header
            div {
                style: "padding:12px 16px; background:{BG_SURFACE}; border-bottom:1px solid {BORDER}; display:flex; align-items:center; gap:12px;",
                span { style: "font-weight:600; font-size:15px;", "Backup Dashboard" }
                if loading() {
                    span { style: "color:{TEXT_DIM}; font-size:12px;", "loading..." }
                }
                {
                    let ds_bg = if matches!(*view.read(), View::Datastores) { BG_ELEVATED } else { BG_SURFACE };
                    let tasks_bg = if matches!(*view.read(), View::Tasks) { BG_ELEVATED } else { BG_SURFACE };
                    rsx! {
                        div { style: "margin-left:auto; display:flex; gap:6px;",
                            button {
                                style: "background:{ds_bg}; border:1px solid {BORDER}; color:{TEXT_MUTED}; padding:4px 10px; border-radius:4px; cursor:pointer; font-size:12px;",
                                onclick: show_datastores,
                                "Datastores"
                            }
                            button {
                                style: "background:{tasks_bg}; border:1px solid {BORDER}; color:{TEXT_MUTED}; padding:4px 10px; border-radius:4px; cursor:pointer; font-size:12px;",
                                onclick: show_tasks,
                                "Tasks"
                            }
                        }
                    }
                }
            }

            // Error banner
            if let Some(ref err) = error_msg() {
                div {
                    style: "padding:8px 16px; background:#7f1d1d; color:#fca5a5; font-size:12px;",
                    "{err}"
                }
            }

            // Content
            div { style: "flex:1; overflow-y:auto; padding:16px;",
                match &*view.read() {
                    View::Datastores => rsx! {
                        div { style: "display:flex; flex-direction:column; gap:12px;",
                            for ds in datastores().iter() {
                                {
                                    let pct = usage_percent(ds.used, ds.total);
                                    let store = ds.store.clone();
                                    rsx! {
                                        div {
                                            style: "background:{BG_SURFACE}; border-radius:6px; padding:12px; cursor:pointer;",
                                            onclick: move |_| show_snapshots(store.clone()),

                                            div { style: "display:flex; align-items:center; gap:12px; margin-bottom:8px;",
                                                span { style: "font-weight:600; font-size:14px;", "{ds.store}" }
                                                span { style: "color:{TEXT_DIM}; font-size:12px; margin-left:auto;",
                                                    "{format_bytes(ds.used)} / {format_bytes(ds.total)}"
                                                }
                                                span { style: "color:{pct_color(pct)}; font-weight:500; font-size:12px;",
                                                    "{pct:.1}%"
                                                }
                                            }

                                            // Usage bar
                                            div { style: "height:8px; background:{BG_ELEVATED}; border-radius:4px; overflow:hidden;",
                                                div { style: "height:100%; width:{pct}%; background:{pct_color(pct)}; border-radius:4px;" }
                                            }
                                        }
                                    }
                                }
                            }
                            if datastores().is_empty() && !loading() {
                                div { style: "padding:24px; text-align:center; color:{TEXT_DIM};",
                                    "No datastores found. Set PBS_API_URL and PBS_API_TOKEN environment variables."
                                }
                            }
                        }
                    },
                    View::Snapshots(store_name) => rsx! {
                        div {
                            div { style: "display:flex; align-items:center; gap:8px; margin-bottom:12px;",
                                button {
                                    style: "background:{BG_ELEVATED}; border:1px solid {BORDER}; color:{TEXT_MUTED}; padding:4px 10px; border-radius:4px; cursor:pointer; font-size:12px;",
                                    onclick: show_datastores,
                                    "< Datastores"
                                }
                                span { style: "font-weight:600;", "Snapshots: {store_name}" }
                                span { style: "color:{TEXT_DIM}; font-size:12px;",
                                    "{snapshots().len()} snapshot(s)"
                                }
                            }

                            div { style: "background:{BG_SURFACE}; border-radius:6px; overflow:hidden;",
                                // Header
                                div {
                                    style: "display:grid; grid-template-columns:100px 2fr 160px 100px; gap:8px; padding:8px 12px; background:{BG_ELEVATED}; font-size:11px; color:{TEXT_DIM}; text-transform:uppercase; letter-spacing:0.05em;",
                                    span { "Type" }
                                    span { "ID" }
                                    span { "Time" }
                                    span { "Size" }
                                }
                                for snap in snapshots().iter() {
                                    div {
                                        style: "display:grid; grid-template-columns:100px 2fr 160px 100px; gap:8px; padding:6px 12px; border-top:1px solid {BORDER}; font-size:12px;",
                                        span { style: "color:#a78bfa; font-weight:500;", "{snap.backup_type}" }
                                        span { style: "color:{TEXT_SECONDARY};", "{snap.backup_id}" }
                                        span { style: "color:{TEXT_DIM}; font-size:11px; font-family:monospace;",
                                            "{format_timestamp(snap.backup_time)}"
                                        }
                                        span { style: "color:{TEXT_DIM}; font-size:11px;",
                                            if let Some(size) = snap.size {
                                                "{format_bytes(size)}"
                                            } else {
                                                "-"
                                            }
                                        }
                                    }
                                }
                                if snapshots().is_empty() {
                                    div { style: "padding:16px; text-align:center; color:{TEXT_DIM};",
                                        "No snapshots found"
                                    }
                                }
                            }
                        }
                    },
                    View::Tasks => rsx! {
                        div { style: "background:{BG_SURFACE}; border-radius:6px; overflow:hidden;",
                            // Header
                            div {
                                style: "display:grid; grid-template-columns:120px 2fr 160px 80px; gap:8px; padding:8px 12px; background:{BG_ELEVATED}; font-size:11px; color:{TEXT_DIM}; text-transform:uppercase; letter-spacing:0.05em;",
                                span { "Type" }
                                span { "Worker" }
                                span { "Started" }
                                span { "Status" }
                            }
                            for task in tasks().iter() {
                                {
                                    let status = task.status.as_deref().unwrap_or("running");
                                    rsx! {
                                        div {
                                            style: "display:grid; grid-template-columns:120px 2fr 160px 80px; gap:8px; padding:6px 12px; border-top:1px solid {BORDER}; font-size:12px;",
                                            span { style: "color:{TEXT_SECONDARY}; font-weight:500;",
                                                "{task.worker_type}"
                                            }
                                            span { style: "color:{TEXT_DIM}; font-size:11px; font-family:monospace;",
                                                "{task.worker_id.as_deref().unwrap_or(\"-\")}"
                                            }
                                            span { style: "color:{TEXT_DIM}; font-size:11px; font-family:monospace;",
                                                "{format_timestamp(task.starttime)}"
                                            }
                                            span { style: "color:{task_status_color(status)}; font-weight:500; font-size:11px;",
                                                "{status}"
                                            }
                                        }
                                    }
                                }
                            }
                            if tasks().is_empty() {
                                div { style: "padding:16px; text-align:center; color:{TEXT_DIM};",
                                    "No recent tasks"
                                }
                            }
                        }
                    },
                }
            }
        }
    }
}

// ── Theme ──

const BG_BASE: &str = cosmix_ui::theme::BG_BASE;
const BG_SURFACE: &str = cosmix_ui::theme::BG_SURFACE;
const BG_ELEVATED: &str = cosmix_ui::theme::BG_ELEVATED;
const BORDER: &str = cosmix_ui::theme::BORDER_DEFAULT;
const TEXT_PRIMARY: &str = cosmix_ui::theme::TEXT_PRIMARY;
const TEXT_SECONDARY: &str = cosmix_ui::theme::TEXT_SECONDARY;
const TEXT_MUTED: &str = cosmix_ui::theme::TEXT_MUTED;
const TEXT_DIM: &str = cosmix_ui::theme::TEXT_DIM;

const CSS: &str = r#"
html, body, #main {
    margin: 0; padding: 0;
    width: 100%; height: 100%;
    overflow: hidden;
}
::-webkit-scrollbar { width: 8px; }
::-webkit-scrollbar-track { background: transparent; }
::-webkit-scrollbar-thumb { background: #374151; border-radius: 4px; }
::-webkit-scrollbar-thumb:hover { background: #4b5563; }
button:hover { background: #374151 !important; }
div[style*="cursor:pointer"]:hover { background: #1e293b !important; }
"#;
