//! cosmix-mon — System monitor GUI for the cosmix appmesh.
//!
//! Pure client: queries cosmix-mond (the headless daemon) via the hub.
//! Builds as both desktop (native window) and WASM (browser via cosmix-web).
//!
//! Desktop: `cargo build -p cosmix-mon`
//! WASM:    `cd crates/cosmix-mon && dx build --platform web`

use std::sync::Arc;

use dioxus::prelude::*;
use serde::Deserialize;

fn main() {
    #[cfg(not(target_arch = "wasm32"))]
    cosmix_ui::desktop::init_linux_env();

    #[cfg(feature = "desktop")]
    {
        use dioxus_desktop::{Config, LogicalSize, WindowBuilder};

        let cfg = Config::new().with_window(
            WindowBuilder::new()
                .with_title("cosmix-mon")
                .with_inner_size(LogicalSize::new(720.0, 520.0)),
        );

        LaunchBuilder::new().with_cfg(cfg).launch(app);
        return;
    }

    #[allow(unreachable_code)]
    dioxus::launch(app);
}

// ── Data types (deserialized from mond responses) ──

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
    disks: Vec<DiskInfo>,
    load_avg: [f64; 3],
}

#[derive(Clone, Debug, Deserialize, Default)]
struct DiskInfo {
    mount: String,
    total_gb: f64,
    used_gb: f64,
    percent: f32,
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

// ── UI ──

fn app() -> Element {
    let mut status: Signal<Option<SystemStatus>> = use_signal(|| None);
    let mut remote_status: Signal<Option<SystemStatus>> = use_signal(|| None);
    let mut remote_node = use_signal(|| String::new());
    let mut hub_client: Signal<Option<Arc<cosmix_client::HubClient>>> = use_signal(|| None);
    let mut error_msg: Signal<Option<String>> = use_signal(|| None);

    // Connect to hub + periodic refresh
    use_effect(move || {
        spawn(async move {
            // Connect anonymously (we don't register, just query)
            let client = {
                #[cfg(not(target_arch = "wasm32"))]
                {
                    cosmix_client::HubClient::connect_anonymous_default().await
                }
                #[cfg(target_arch = "wasm32")]
                {
                    cosmix_client::HubClient::connect_anonymous_default()
                }
            };

            match client {
                Ok(c) => {
                    let client = Arc::new(c);
                    hub_client.set(Some(client.clone()));
                    error_msg.set(None);

                    // Initial fetch
                    if let Ok(val) = client.call("mon", "mon.status", serde_json::Value::Null).await {
                        if let Ok(s) = serde_json::from_value::<SystemStatus>(val) {
                            status.set(Some(s));
                        }
                    }
                }
                Err(e) => {
                    error_msg.set(Some(format!("Hub: {e}")));
                }
            }
        });

        // Periodic refresh every 5 seconds
        spawn(async move {
            loop {
                #[cfg(not(target_arch = "wasm32"))]
                tokio::time::sleep(std::time::Duration::from_secs(5)).await;
                #[cfg(target_arch = "wasm32")]
                gloo_timers::future::TimeoutFuture::new(5_000).await;

                if let Some(client) = hub_client() {
                    match client.call("mon", "mon.status", serde_json::Value::Null).await {
                        Ok(val) => {
                            if let Ok(s) = serde_json::from_value::<SystemStatus>(val) {
                                status.set(Some(s));
                                error_msg.set(None);
                            }
                        }
                        Err(e) => {
                            error_msg.set(Some(format!("Refresh: {e}")));
                        }
                    }
                }
            }
        });
    });

    let fetch_remote = move |_| {
        let node = remote_node();
        if node.is_empty() {
            return;
        }
        spawn(async move {
            if let Some(client) = hub_client() {
                let target = format!("mon.{node}.amp");
                match client.call(&target, "mon.status", serde_json::Value::Null).await {
                    Ok(val) => {
                        if let Ok(s) = serde_json::from_value::<SystemStatus>(val) {
                            remote_status.set(Some(s));
                        }
                    }
                    Err(e) => {
                        tracing::warn!(error = %e, "Failed to fetch remote status");
                        remote_status.set(None);
                    }
                }
            }
        });
    };

    // Render
    match status() {
        None => rsx! {
            document::Style { "{CSS}" }
            div {
                style: "width:100%; height:100vh; display:flex; align-items:center; justify-content:center; background:{BG_BASE}; color:{TEXT_MUTED}; font-family:-apple-system,BlinkMacSystemFont,'Segoe UI',Helvetica,Arial,sans-serif;",
                if let Some(err) = error_msg() {
                    div { style: "text-align:center;",
                        div { style: "font-size:14px; color:#ef4444; margin-bottom:8px;", "{err}" }
                        div { style: "font-size:12px;", "Ensure cosmix-hub and cosmix-mond are running" }
                    }
                } else {
                    "Connecting to hub..."
                }
            }
        },
        Some(s) => rsx! {
            document::Style { "{CSS}" }
            div {
                style: "width:100%; height:100vh; display:flex; flex-direction:column; background:{BG_BASE}; color:{TEXT_PRIMARY}; font-family:-apple-system,BlinkMacSystemFont,'Segoe UI',Helvetica,Arial,sans-serif; font-size:13px; overflow-y:auto;",

                // Header
                div {
                    style: "padding:12px 16px; background:{BG_SURFACE}; border-bottom:1px solid {BORDER}; display:flex; align-items:center; gap:12px;",
                    span { style: "font-weight:600; font-size:15px;", "{s.hostname}" }
                    span { style: "color:{TEXT_DIM}; font-size:12px;", "up {format_uptime(s.uptime_secs)}" }
                    span { style: "color:{TEXT_DIM}; font-size:12px;", "load {s.load_avg[0]:.2} {s.load_avg[1]:.2} {s.load_avg[2]:.2}" }

                    // Remote query
                    div { style: "margin-left:auto; display:flex; align-items:center; gap:6px;",
                        input {
                            style: "background:{BG_ELEVATED}; border:1px solid {BORDER}; color:{TEXT_PRIMARY}; padding:4px 8px; border-radius:4px; width:100px; font-size:12px;",
                            placeholder: "node name",
                            value: "{remote_node}",
                            oninput: move |e| remote_node.set(e.value()),
                        }
                        button {
                            style: "background:{BG_ELEVATED}; border:1px solid {BORDER}; color:{TEXT_MUTED}; padding:4px 10px; border-radius:4px; cursor:pointer; font-size:12px;",
                            onclick: fetch_remote,
                            "Query"
                        }
                    }
                }

                // Main content
                div { style: "padding:16px; display:flex; flex-direction:column; gap:16px;",

                    // CPU + Memory row
                    div { style: "display:flex; gap:16px;",
                        {stat_card("CPU", &format!("{:.1}%", s.cpu_usage), &format!("{} cores", s.cpu_count), pct_color(s.cpu_usage))}
                        {stat_card("Memory", &format!("{} / {} MB", s.mem_used_mb, s.mem_total_mb), &format!("{:.1}%", s.mem_percent), pct_color(s.mem_percent))}
                        if s.swap_total_mb > 0 {
                            {stat_card("Swap", &format!("{} / {} MB", s.swap_used_mb, s.swap_total_mb), "", TEXT_MUTED)}
                        }
                    }

                    // Disks
                    if !s.disks.is_empty() {
                        div { style: "background:{BG_SURFACE}; border-radius:6px; padding:12px;",
                            div { style: "font-weight:600; margin-bottom:8px; color:{TEXT_MUTED};", "Disks" }
                            for disk in s.disks.iter() {
                                div { style: "display:flex; align-items:center; gap:12px; margin-bottom:6px;",
                                    span { style: "width:120px; color:{TEXT_SECONDARY}; font-size:12px;", "{disk.mount}" }
                                    div { style: "flex:1; height:8px; background:{BG_ELEVATED}; border-radius:4px; overflow:hidden;",
                                        div { style: "height:100%; width:{disk.percent}%; background:{pct_color(disk.percent)}; border-radius:4px;" }
                                    }
                                    span { style: "width:120px; text-align:right; color:{TEXT_DIM}; font-size:12px;",
                                        "{disk.used_gb:.1} / {disk.total_gb:.1} GB"
                                    }
                                }
                            }
                        }
                    }

                    // Remote node status (if queried)
                    if let Some(ref rs) = remote_status() {
                        div { style: "background:{BG_SURFACE}; border-radius:6px; padding:12px; border:1px solid #2563eb44;",
                            div { style: "font-weight:600; margin-bottom:8px; color:#60a5fa;", "Remote: {rs.hostname}" }
                            div { style: "display:flex; gap:16px;",
                                {stat_card("CPU", &format!("{:.1}%", rs.cpu_usage), &format!("{} cores", rs.cpu_count), pct_color(rs.cpu_usage))}
                                {stat_card("Memory", &format!("{} / {} MB", rs.mem_used_mb, rs.mem_total_mb), &format!("{:.1}%", rs.mem_percent), pct_color(rs.mem_percent))}
                            }
                        }
                    }
                }
            }
        },
    }
}

fn stat_card(title: &str, value: &str, subtitle: &str, accent: &str) -> Element {
    rsx! {
        div { style: "flex:1; background:{BG_SURFACE}; border-radius:6px; padding:12px;",
            div { style: "color:{TEXT_DIM}; font-size:11px; text-transform:uppercase; letter-spacing:0.05em; margin-bottom:4px;", "{title}" }
            div { style: "font-size:18px; font-weight:600; color:{accent};", "{value}" }
            if !subtitle.is_empty() {
                div { style: "color:{TEXT_DIM}; font-size:11px; margin-top:2px;", "{subtitle}" }
            }
        }
    }
}

fn pct_color(pct: f32) -> &'static str {
    if pct > 90.0 { "#ef4444" }
    else if pct > 70.0 { "#f59e0b" }
    else { "#22c55e" }
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
"#;
