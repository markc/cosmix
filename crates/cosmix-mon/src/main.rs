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
use cosmix_ui::app_init::{THEME, use_theme_css, use_theme_poll};
use cosmix_ui::menu::{menubar, standard_file_menu, MenuBar};

#[cfg(not(target_arch = "wasm32"))]
#[global_allocator]
static GLOBAL: mimalloc::MiMalloc = mimalloc::MiMalloc;

fn main() {
    cosmix_ui::app_init::launch_desktop("cosmix-mon", 720.0, 520.0, app);
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

    // Poll config every 30s for theme changes (desktop only)
    #[cfg(not(target_arch = "wasm32"))]
    use_theme_poll(30);

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

    let menu = menubar(vec![standard_file_menu(vec![])]);
    let on_action = move |id: String| match id.as_str() {
        "quit" => std::process::exit(0),
        _ => {}
    };

    use_theme_css();
    let theme = THEME.read();
    let fs = theme.font_size;
    let fs_sm = fs.saturating_sub(2);
    let fs_lg = fs + 2;

    // Render
    match status() {
        None => rsx! {
            div {
                style: "width:100%; height:100vh; display:flex; flex-direction:column; background:var(--bg-primary); font-family:var(--font-sans); font-size:{fs}px;",
                MenuBar { menu: menu.clone(), on_action }
                div {
                    style: "flex:1; display:flex; align-items:center; justify-content:center; color:var(--fg-muted);",
                    if let Some(err) = error_msg() {
                        div { style: "text-align:center;",
                            div { style: "font-size:{fs}px; color:var(--danger); margin-bottom:8px;", "{err}" }
                            div { style: "font-size:{fs_sm}px;", "Ensure cosmix-hub and cosmix-mond are running" }
                        }
                    } else {
                        "Connecting to hub..."
                    }
                }
            }
        },
        Some(s) => rsx! {
            div {
                style: "width:100%; height:100vh; display:flex; flex-direction:column; background:var(--bg-primary); color:var(--fg-primary); font-family:var(--font-sans); font-size:{fs}px;",

                MenuBar { menu: menu.clone(), on_action }

                // Scrollable content
                div {
                    style: "flex:1; overflow-y:auto; display:flex; flex-direction:column;",

                // Header
                div {
                    style: "padding:12px 16px; background:var(--bg-secondary); border-bottom:1px solid var(--border); display:flex; align-items:center; gap:12px;",
                    span { style: "font-weight:600; font-size:{fs_lg}px;", "{s.hostname}" }
                    span { style: "color:var(--fg-muted); font-size:{fs_sm}px;", "up {format_uptime(s.uptime_secs)}" }
                    span { style: "color:var(--fg-muted); font-size:{fs_sm}px;", "load {s.load_avg[0]:.2} {s.load_avg[1]:.2} {s.load_avg[2]:.2}" }

                    // Remote query
                    div { style: "margin-left:auto; display:flex; align-items:center; gap:6px;",
                        input {
                            style: "background:var(--bg-tertiary); border:1px solid var(--border); color:var(--fg-primary); padding:4px 8px; border-radius:var(--radius-sm); width:100px; font-size:{fs_sm}px;",
                            placeholder: "node name",
                            value: "{remote_node}",
                            oninput: move |e| remote_node.set(e.value()),
                        }
                        button {
                            style: "background:var(--bg-tertiary); border:1px solid var(--border); color:var(--fg-secondary); padding:4px 10px; border-radius:var(--radius-sm); cursor:pointer; font-size:{fs_sm}px;",
                            onclick: fetch_remote,
                            "Query"
                        }
                    }
                }

                // Main content
                div { style: "padding:16px; display:flex; flex-direction:column; gap:16px;",

                    // CPU + Memory row
                    div { style: "display:flex; gap:16px;",
                        {stat_card("CPU", &format!("{:.1}%", s.cpu_usage), &format!("{} cores", s.cpu_count), pct_color(s.cpu_usage), fs)}
                        {stat_card("Memory", &format!("{} / {} MB", s.mem_used_mb, s.mem_total_mb), &format!("{:.1}%", s.mem_percent), pct_color(s.mem_percent), fs)}
                        if s.swap_total_mb > 0 {
                            {stat_card("Swap", &format!("{} / {} MB", s.swap_used_mb, s.swap_total_mb), "", "var(--fg-muted)", fs)}
                        }
                    }

                    // Disks
                    if !s.disks.is_empty() {
                        div { style: "background:var(--bg-secondary); border-radius:var(--radius-md); padding:12px;",
                            div { style: "font-weight:600; margin-bottom:8px; color:var(--fg-muted);", "Disks" }
                            for disk in s.disks.iter() {
                                div { style: "display:flex; align-items:center; gap:12px; margin-bottom:6px;",
                                    span { style: "width:120px; color:var(--fg-secondary); font-size:{fs_sm}px;", "{disk.mount}" }
                                    div { style: "flex:1; height:8px; background:var(--bg-tertiary); border-radius:var(--radius-sm); overflow:hidden;",
                                        div { style: "height:100%; width:{disk.percent}%; background:{pct_color(disk.percent)}; border-radius:var(--radius-sm);" }
                                    }
                                    span { style: "width:120px; text-align:right; color:var(--fg-muted); font-size:{fs_sm}px;",
                                        "{disk.used_gb:.1} / {disk.total_gb:.1} GB"
                                    }
                                }
                            }
                        }
                    }

                    // Remote node status (if queried)
                    if let Some(ref rs) = remote_status() {
                        div { style: "background:var(--bg-secondary); border-radius:var(--radius-md); padding:12px; border:1px solid var(--accent-subtle);",
                            div { style: "font-weight:600; margin-bottom:8px; color:var(--accent);", "Remote: {rs.hostname}" }
                            div { style: "display:flex; gap:16px;",
                                {stat_card("CPU", &format!("{:.1}%", rs.cpu_usage), &format!("{} cores", rs.cpu_count), pct_color(rs.cpu_usage), fs)}
                                {stat_card("Memory", &format!("{} / {} MB", rs.mem_used_mb, rs.mem_total_mb), &format!("{:.1}%", rs.mem_percent), pct_color(rs.mem_percent), fs)}
                            }
                        }
                    }
                }
                } // end scrollable content div
            }
        },
    }
}

fn stat_card(title: &str, value: &str, subtitle: &str, accent: &str, font_size: u16) -> Element {
    let fs_sm = font_size.saturating_sub(2);
    let fs_val = font_size + 4;
    rsx! {
        div { style: "flex:1; background:var(--bg-secondary); border-radius:var(--radius-md); padding:12px;",
            div { style: "color:var(--fg-muted); font-size:{fs_sm}px; text-transform:uppercase; letter-spacing:0.05em; margin-bottom:4px;", "{title}" }
            div { style: "font-size:{fs_val}px; font-weight:600; color:{accent};", "{value}" }
            if !subtitle.is_empty() {
                div { style: "color:var(--fg-muted); font-size:{fs_sm}px; margin-top:2px;", "{subtitle}" }
            }
        }
    }
}

fn pct_color(pct: f32) -> &'static str {
    if pct > 90.0 { "var(--danger)" }
    else if pct > 70.0 { "var(--warning)" }
    else { "var(--success)" }
}
