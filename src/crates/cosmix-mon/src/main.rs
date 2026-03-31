//! cosmix-mon — System monitor GUI for the cosmix appmesh.
//!
//! Master-detail layout:
//! - Startup shows a DataTable of all mesh nodes with summary stats
//! - Click a node to drill into full stats (cards + disks/processes tabs)
//! - Back button returns to node list
//!
//! Pure client: queries cosmix-mond on each node via the hub mesh.
//! Builds as both desktop (native window) and WASM (browser via cosmix-web).

use std::sync::Arc;

use dioxus::prelude::*;
use serde::Deserialize;
use serde_json::Value;
use cosmix_ui::app_init::{THEME, use_theme_css, use_theme_poll};
use cosmix_ui::components::{DataTable, DataColumn};
use cosmix_ui::menu::{menubar, standard_file_menu, MenuBar};

#[cfg(not(target_arch = "wasm32"))]
#[global_allocator]
static GLOBAL: mimalloc::MiMalloc = mimalloc::MiMalloc;

fn main() {
    cosmix_ui::app_init::launch_desktop("cosmix-mon", 780.0, 560.0, app);
}

// ── Data types ──

#[derive(Clone, Debug, Deserialize, Default)]
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
    #[allow(dead_code)]
    disks: Vec<Value>,
    load_avg: [f64; 3],
}

#[derive(Clone, Copy, PartialEq)]
enum Tab {
    Disks,
    Processes,
}

fn format_uptime(secs: u64) -> String {
    let days = secs / 86400;
    let hours = (secs % 86400) / 3600;
    let mins = (secs % 3600) / 60;
    if days > 0 {
        format!("{days}d {hours}h {mins}m")
    } else if hours > 0 {
        format!("{hours}h {mins}m")
    } else {
        format!("{mins}m")
    }
}

// ── Cell formatters ──

fn fmt_gb(v: &Value) -> String {
    format!("{:.1} GB", v.as_f64().unwrap_or(0.0))
}

fn fmt_pct(v: &Value) -> String {
    format!("{:.1}%", v.as_f64().unwrap_or(0.0))
}

fn fmt_mb_val(v: &Value) -> String {
    format!("{} MB", v.as_u64().unwrap_or(0))
}

fn fmt_uptime_val(v: &Value) -> String {
    format_uptime(v.as_u64().unwrap_or(0))
}

fn fmt_load(v: &Value) -> String {
    if let Some(arr) = v.as_array() {
        let vals: Vec<String> = arr.iter().map(|x| format!("{:.2}", x.as_f64().unwrap_or(0.0))).collect();
        vals.join(" ")
    } else {
        String::new()
    }
}

// ── Column definitions ──

fn node_columns() -> Vec<DataColumn> {
    vec![
        DataColumn { key: "node", label: "Node", width: "1fr", sortable: true, format: None },
        DataColumn { key: "status", label: "Status", width: "80px", sortable: true, format: None },
        DataColumn { key: "cpu_usage", label: "CPU", width: "70px", sortable: true, format: Some(fmt_pct) },
        DataColumn { key: "mem_percent", label: "Mem", width: "70px", sortable: true, format: Some(fmt_pct) },
        DataColumn { key: "uptime_secs", label: "Uptime", width: "100px", sortable: true, format: Some(fmt_uptime_val) },
        DataColumn { key: "load_avg", label: "Load", width: "120px", sortable: false, format: Some(fmt_load) },
    ]
}

fn disk_columns() -> Vec<DataColumn> {
    vec![
        DataColumn { key: "mount", label: "Mount", width: "1fr", sortable: true, format: None },
        DataColumn { key: "total_gb", label: "Total", width: "90px", sortable: true, format: Some(fmt_gb) },
        DataColumn { key: "used_gb", label: "Used", width: "90px", sortable: true, format: Some(fmt_gb) },
        DataColumn { key: "percent", label: "Usage", width: "80px", sortable: true, format: Some(fmt_pct) },
    ]
}

fn process_columns() -> Vec<DataColumn> {
    vec![
        DataColumn { key: "pid", label: "PID", width: "80px", sortable: true, format: None },
        DataColumn { key: "name", label: "Process", width: "1fr", sortable: true, format: None },
        DataColumn { key: "cpu", label: "CPU %", width: "80px", sortable: true, format: Some(fmt_pct) },
        DataColumn { key: "mem_mb", label: "Memory", width: "90px", sortable: true, format: Some(fmt_mb_val) },
    ]
}

// ── App root ──

fn app() -> Element {
    // Hub connection
    let mut hub_client: Signal<Option<Arc<cosmix_client::HubClient>>> = use_signal(|| None);
    let mut error_msg: Signal<Option<String>> = use_signal(|| None);

    // Node list (master view)
    let mut node_rows: Signal<Vec<Value>> = use_signal(Vec::new);
    let mut selected_node_idx: Signal<Option<usize>> = use_signal(|| None);

    // Detail view state
    let mut detail_node: Signal<Option<String>> = use_signal(|| None);
    let mut detail_status: Signal<Option<SystemStatus>> = use_signal(|| None);
    let mut disk_rows: Signal<Vec<Value>> = use_signal(Vec::new);
    let mut proc_rows: Signal<Vec<Value>> = use_signal(Vec::new);
    let active_tab: Signal<Tab> = use_signal(|| Tab::Disks);
    let mut detail_selected: Signal<Option<usize>> = use_signal(|| None);

    // Connect to hub + discover nodes
    use_effect(move || {
        spawn(async move {
            let client = {
                #[cfg(not(target_arch = "wasm32"))]
                { cosmix_client::HubClient::connect_anonymous_default().await }
                #[cfg(target_arch = "wasm32")]
                { cosmix_client::HubClient::connect_anonymous_default() }
            };

            match client {
                Ok(c) => {
                    let client = Arc::new(c);
                    hub_client.set(Some(client.clone()));
                    error_msg.set(None);

                    // Discover nodes and fetch initial status
                    refresh_nodes(&client, &mut node_rows).await;
                }
                Err(e) => {
                    error_msg.set(Some(format!("Hub: {e}")));
                }
            }
        });

        // Periodic refresh every 10 seconds
        spawn(async move {
            loop {
                #[cfg(not(target_arch = "wasm32"))]
                tokio::time::sleep(std::time::Duration::from_secs(10)).await;
                #[cfg(target_arch = "wasm32")]
                gloo_timers::future::TimeoutFuture::new(10_000).await;

                if let Some(client) = hub_client() {
                    // Refresh node list if in master view
                    if detail_node().is_none() {
                        refresh_nodes(&client, &mut node_rows).await;
                    } else if let Some(ref node) = detail_node() {
                        // Refresh detail view
                        let target = mon_target(node);
                        fetch_detail(&client, &target, &mut detail_status, &mut disk_rows).await;
                        fetch_processes_from(&client, &target, &mut proc_rows).await;
                    }
                }
            }
        });
    });

    #[cfg(not(target_arch = "wasm32"))]
    use_theme_poll(30);

    let menu = menubar(vec![standard_file_menu(vec![])]);
    let on_action = move |id: String| match id.as_str() {
        "quit" => std::process::exit(0),
        _ => {}
    };

    use_theme_css();
    let theme = THEME.read();
    let fs = theme.font_size;
    let fs_sm = fs.saturating_sub(2);

    // Handle node click — drill into detail
    let on_node_click = move |idx: usize| {
        let rows = node_rows();
        if let Some(row) = rows.get(idx) {
            if let Some(target) = row.get("_target").and_then(|n| n.as_str()) {
                let node = target.to_string();
                detail_node.set(Some(node.clone()));
                detail_status.set(None);
                disk_rows.set(Vec::new());
                proc_rows.set(Vec::new());
                detail_selected.set(None);
                selected_node_idx.set(Some(idx));

                // Fetch detail immediately
                spawn(async move {
                    if let Some(client) = hub_client() {
                        let target = mon_target(&node);
                        fetch_detail(&client, &target, &mut detail_status, &mut disk_rows).await;
                        fetch_processes_from(&client, &target, &mut proc_rows).await;
                    }
                });
            }
        }
    };

    let on_back = EventHandler::new(move |_: ()| {
        detail_node.set(None);
        detail_status.set(None);
        // Refresh node list
        spawn(async move {
            if let Some(client) = hub_client() {
                refresh_nodes(&client, &mut node_rows).await;
            }
        });
    });

    rsx! {
        div {
            style: "width:100%;height:100vh;display:flex;flex-direction:column;background:var(--bg-primary);color:var(--fg-primary);font-family:var(--font-sans);font-size:{fs}px;",

            MenuBar { menu: menu.clone(), on_action }

            div {
                style: "flex:1;overflow-y:auto;display:flex;flex-direction:column;",

                match detail_node() {
                    None => rsx! {
                        // ── Master: Node List ──
                        div {
                            style: "padding:12px 16px;background:var(--bg-secondary);border-bottom:1px solid var(--border);",
                            span { style: "font-weight:600;font-size:{fs}px;", "Mesh Nodes" }
                            span { style: "color:var(--fg-muted);font-size:{fs_sm}px;margin-left:12px;",
                                "{node_rows().len()} node(s)"
                            }
                        }

                        if let Some(err) = error_msg() {
                            div {
                                style: "padding:16px;text-align:center;",
                                div { style: "color:var(--danger);margin-bottom:8px;", "{err}" }
                                div { style: "font-size:{fs_sm}px;color:var(--fg-muted);", "Ensure cosmix-hubd is running" }
                            }
                        } else if node_rows().is_empty() {
                            div {
                                style: "flex:1;display:flex;align-items:center;justify-content:center;color:var(--fg-muted);",
                                "Discovering nodes..."
                            }
                        } else {
                            div { style: "padding:16px;",
                                DataTable {
                                    columns: node_columns(),
                                    rows: node_rows(),
                                    on_row_click: on_node_click,
                                    selected: selected_node_idx(),
                                }
                            }
                        }
                    },
                    Some(node_name) => rsx! {
                        // ── Detail: Node Stats ──
                        {detail_view(
                            node_name,
                            detail_status,
                            disk_rows,
                            proc_rows,
                            active_tab,
                            detail_selected,
                            on_back,
                            fs,
                            fs_sm,
                        )}
                    },
                }
            }
        }
    }
}

// ── Detail view ──

#[allow(clippy::too_many_arguments)]
fn detail_view(
    node_name: String,
    detail_status: Signal<Option<SystemStatus>>,
    disk_rows: Signal<Vec<Value>>,
    proc_rows: Signal<Vec<Value>>,
    active_tab: Signal<Tab>,
    mut detail_selected: Signal<Option<usize>>,
    on_back: EventHandler<()>,
    fs: u16,
    fs_sm: u16,
) -> Element {
    let fs_lg = fs + 2;

    match detail_status() {
        None => rsx! {
            div {
                style: "padding:12px 16px;background:var(--bg-secondary);border-bottom:1px solid var(--border);display:flex;align-items:center;gap:12px;",
                button {
                    style: "background:var(--bg-tertiary);border:1px solid var(--border);color:var(--fg-secondary);padding:4px 10px;border-radius:var(--radius-sm);cursor:pointer;font-size:{fs_sm}px;",
                    onclick: move |_| on_back.call(()),
                    "\u{2190} Back"
                }
                span { style: "font-weight:600;", "{node_name}" }
            }
            div {
                style: "flex:1;display:flex;align-items:center;justify-content:center;color:var(--fg-muted);",
                "Loading..."
            }
        },
        Some(s) => rsx! {
            // Header with back button
            div {
                style: "padding:12px 16px;background:var(--bg-secondary);border-bottom:1px solid var(--border);display:flex;align-items:center;gap:12px;",
                button {
                    style: "background:var(--bg-tertiary);border:1px solid var(--border);color:var(--fg-secondary);padding:4px 10px;border-radius:var(--radius-sm);cursor:pointer;font-size:{fs_sm}px;",
                    onclick: move |_| on_back.call(()),
                    "\u{2190} Back"
                }
                span { style: "font-weight:600;font-size:{fs_lg}px;", "{s.hostname}" }
                span { style: "color:var(--fg-muted);font-size:{fs_sm}px;", "up {format_uptime(s.uptime_secs)}" }
                span { style: "color:var(--fg-muted);font-size:{fs_sm}px;", "load {s.load_avg[0]:.2} {s.load_avg[1]:.2} {s.load_avg[2]:.2}" }
            }

            div { style: "padding:16px;display:flex;flex-direction:column;gap:16px;",

                // Stat cards
                div { style: "display:flex;gap:16px;",
                    {stat_card("CPU", &format!("{:.1}%", s.cpu_usage), &format!("{} cores", s.cpu_count), pct_color(s.cpu_usage), fs)}
                    {stat_card("Memory", &format!("{} / {} MB", s.mem_used_mb, s.mem_total_mb), &format!("{:.1}%", s.mem_percent), pct_color(s.mem_percent), fs)}
                    if s.swap_total_mb > 0 {
                        {stat_card("Swap", &format!("{} / {} MB", s.swap_used_mb, s.swap_total_mb), "", "var(--fg-muted)", fs)}
                    }
                }

                // Tab bar
                div {
                    style: "display:flex;gap:0;border-bottom:1px solid var(--border);",
                    {tab_button("Disks", Tab::Disks, active_tab, fs_sm)}
                    {tab_button("Processes", Tab::Processes, active_tab, fs_sm)}
                }

                // DataTable
                match active_tab() {
                    Tab::Disks => rsx! {
                        DataTable {
                            columns: disk_columns(),
                            rows: disk_rows(),
                            on_row_click: move |idx| detail_selected.set(Some(idx)),
                            selected: detail_selected(),
                        }
                    },
                    Tab::Processes => rsx! {
                        DataTable {
                            columns: process_columns(),
                            rows: proc_rows(),
                            on_row_click: move |idx| detail_selected.set(Some(idx)),
                            selected: detail_selected(),
                            page_size: 20,
                        }
                    },
                }
            }
        },
    }
}

// ── Data fetching ──

/// Build the AMP target for a node's mond. Local node uses "mon", remote uses "mon.{node}.amp".
fn mon_target(node: &str) -> String {
    // If this is the local node, just use "mon" directly
    // We store "local" as a sentinel in the node rows
    if node == "__local__" {
        "mon".to_string()
    } else {
        format!("mon.{node}.amp")
    }
}

/// Discover all mesh nodes and fetch summary status for each.
async fn refresh_nodes(
    client: &cosmix_client::HubClient,
    node_rows: &mut Signal<Vec<Value>>,
) {
    let mut rows: Vec<Value> = Vec::new();

    // Get local node name and peer list
    let (local_name, peers) = match client.call("hub", "hub.peers", Value::Null).await {
        Ok(val) => {
            let name = val.get("node").and_then(|n| n.as_str()).unwrap_or("local").to_string();
            let peers: Vec<String> = val.get("peers")
                .and_then(|p| p.as_array())
                .map(|arr| arr.iter().filter_map(|v| v.as_str().map(String::from)).collect())
                .unwrap_or_default();
            (name, peers)
        }
        Err(_) => ("local".to_string(), Vec::new()),
    };

    // Fetch local node status
    match client.call("mon", "mon.status", Value::Null).await {
        Ok(val) => {
            let mut row = serde_json::json!({
                "node": &local_name,
                "status": "online",
                "cpu_usage": val.get("cpu_usage").cloned().unwrap_or(Value::Null),
                "mem_percent": val.get("mem_percent").cloned().unwrap_or(Value::Null),
                "uptime_secs": val.get("uptime_secs").cloned().unwrap_or(Value::Null),
                "load_avg": val.get("load_avg").cloned().unwrap_or(Value::Null),
                "_target": "__local__",
            });
            // Preserve original node name for detail lookup
            if let Some(obj) = row.as_object_mut() {
                obj.insert("node".to_string(), Value::String(format!("{} (local)", local_name)));
                obj.insert("_target".to_string(), Value::String("__local__".to_string()));
            }
            rows.push(row);
        }
        Err(_) => {
            rows.push(serde_json::json!({
                "node": format!("{} (local)", local_name),
                "status": "offline",
                "cpu_usage": Value::Null,
                "mem_percent": Value::Null,
                "uptime_secs": Value::Null,
                "load_avg": Value::Null,
                "_target": "__local__",
            }));
        }
    }

    // Fetch each remote peer's status
    for peer in &peers {
        let target = format!("mon.{peer}.amp");
        match client.call(&target, "mon.status", Value::Null).await {
            Ok(val) => {
                rows.push(serde_json::json!({
                    "node": peer,
                    "status": "online",
                    "cpu_usage": val.get("cpu_usage").cloned().unwrap_or(Value::Null),
                    "mem_percent": val.get("mem_percent").cloned().unwrap_or(Value::Null),
                    "uptime_secs": val.get("uptime_secs").cloned().unwrap_or(Value::Null),
                    "load_avg": val.get("load_avg").cloned().unwrap_or(Value::Null),
                    "_target": peer,
                }));
            }
            Err(_) => {
                rows.push(serde_json::json!({
                    "node": peer,
                    "status": "offline",
                    "cpu_usage": Value::Null,
                    "mem_percent": Value::Null,
                    "uptime_secs": Value::Null,
                    "load_avg": Value::Null,
                    "_target": peer,
                }));
            }
        }
    }

    node_rows.set(rows);
}

async fn fetch_detail(
    client: &cosmix_client::HubClient,
    target: &str,
    status: &mut Signal<Option<SystemStatus>>,
    disk_rows: &mut Signal<Vec<Value>>,
) {
    match client.call(target, "mon.status", Value::Null).await {
        Ok(val) => {
            if let Some(disks) = val.get("disks").and_then(|d| d.as_array()) {
                disk_rows.set(disks.clone());
            }
            if let Ok(s) = serde_json::from_value::<SystemStatus>(val) {
                status.set(Some(s));
            }
        }
        Err(_) => {}
    }
}

async fn fetch_processes_from(
    client: &cosmix_client::HubClient,
    target: &str,
    proc_rows: &mut Signal<Vec<Value>>,
) {
    match client.call(target, "mon.processes", Value::Null).await {
        Ok(val) => {
            if let Some(arr) = val.as_array() {
                proc_rows.set(arr.clone());
            }
        }
        Err(_) => {}
    }
}

// ── Helper components ──

fn stat_card(title: &str, value: &str, subtitle: &str, accent: &str, font_size: u16) -> Element {
    let fs_sm = font_size.saturating_sub(2);
    let fs_val = font_size + 4;
    rsx! {
        div { style: "flex:1;background:var(--bg-secondary);border-radius:var(--radius-md);padding:12px;",
            div { style: "color:var(--fg-muted);font-size:{fs_sm}px;text-transform:uppercase;letter-spacing:0.05em;margin-bottom:4px;", "{title}" }
            div { style: "font-size:{fs_val}px;font-weight:600;color:{accent};", "{value}" }
            if !subtitle.is_empty() {
                div { style: "color:var(--fg-muted);font-size:{fs_sm}px;margin-top:2px;", "{subtitle}" }
            }
        }
    }
}

fn tab_button(label: &str, tab: Tab, mut active_tab: Signal<Tab>, fs_sm: u16) -> Element {
    let is_active = active_tab() == tab;
    let border = if is_active { "var(--accent)" } else { "transparent" };
    let color = if is_active { "var(--fg-primary)" } else { "var(--fg-muted)" };
    let bg = if is_active { "var(--bg-secondary)" } else { "transparent" };

    rsx! {
        button {
            style: "padding:8px 16px;background:{bg};border:none;border-bottom:2px solid {border};color:{color};cursor:pointer;font-size:{fs_sm}px;font-family:var(--font-sans);",
            onclick: move |_| active_tab.set(tab),
            "{label}"
        }
    }
}

fn pct_color(pct: f32) -> &'static str {
    if pct > 90.0 { "var(--danger)" }
    else if pct > 70.0 { "var(--warning)" }
    else { "var(--success)" }
}
