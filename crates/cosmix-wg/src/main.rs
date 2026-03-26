//! cosmix-wg — WireGuard mesh admin service for the cosmix appmesh.
//!
//! Registers as "wg" on the hub. Handles:
//! - `wg.status` — interface status (public key, listen port, fwmark)
//! - `wg.peers` — peer list with handshake/transfer stats
//! - `wg.interfaces` — list all WireGuard interfaces
//!
//! Reads WireGuard state via /proc/net and `wg show` parsing.
//! Accessible from any browser on the mesh.

use std::sync::Arc;

use dioxus::prelude::*;
use serde::{Deserialize, Serialize};

fn main() {
    cosmix_ui::desktop::init_linux_env();

    #[cfg(feature = "desktop")]
    {
        use dioxus_desktop::{Config, LogicalSize, WindowBuilder};

        let cfg = Config::new().with_window(
            WindowBuilder::new()
                .with_title("cosmix-wg")
                .with_inner_size(LogicalSize::new(900.0, 600.0)),
        );

        LaunchBuilder::new().with_cfg(cfg).launch(app);
        return;
    }

    #[allow(unreachable_code)]
    {
        eprintln!("Desktop feature not enabled");
        std::process::exit(1);
    }
}

// ── WireGuard data ──

#[derive(Clone, Debug, Serialize, Deserialize)]
struct WgInterface {
    name: String,
    public_key: String,
    listen_port: u16,
    #[serde(default)]
    fwmark: String,
    peers: Vec<WgPeer>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
struct WgPeer {
    public_key: String,
    #[serde(default)]
    endpoint: String,
    #[serde(default)]
    allowed_ips: Vec<String>,
    latest_handshake: u64,
    transfer_rx: u64,
    transfer_tx: u64,
    #[serde(default)]
    persistent_keepalive: u16,
}

/// Parse `wg show all dump` output into structured data.
fn parse_wg_dump(output: &str) -> Vec<WgInterface> {
    let mut interfaces: Vec<WgInterface> = Vec::new();

    for line in output.lines() {
        let fields: Vec<&str> = line.split('\t').collect();
        if fields.len() < 4 {
            continue;
        }

        let iface_name = fields[0];

        // Interface line: name, private_key, public_key, listen_port, fwmark
        if fields.len() == 5 || (fields.len() >= 4 && fields[3].parse::<u16>().is_ok() && !fields[1].contains('.')) {
            let public_key = fields[2].to_string();
            let listen_port = fields[3].parse::<u16>().unwrap_or(0);
            let fwmark = fields.get(4).unwrap_or(&"off").to_string();
            interfaces.push(WgInterface {
                name: iface_name.to_string(),
                public_key,
                listen_port,
                fwmark,
                peers: Vec::new(),
            });
        }
        // Peer line: iface, public_key, preshared_key, endpoint, allowed_ips, latest_handshake, transfer_rx, transfer_tx, persistent_keepalive
        else if fields.len() >= 9 {
            let peer = WgPeer {
                public_key: fields[1].to_string(),
                endpoint: fields[3].to_string(),
                allowed_ips: fields[4]
                    .split(',')
                    .map(|s| s.trim().to_string())
                    .filter(|s| !s.is_empty() && s != "(none)")
                    .collect(),
                latest_handshake: fields[5].parse().unwrap_or(0),
                transfer_rx: fields[6].parse().unwrap_or(0),
                transfer_tx: fields[7].parse().unwrap_or(0),
                persistent_keepalive: fields[8]
                    .trim()
                    .parse()
                    .unwrap_or(0),
            };

            if let Some(iface) = interfaces.iter_mut().find(|i| i.name == iface_name) {
                iface.peers.push(peer);
            }
        }
    }

    interfaces
}

fn gather_wg_status() -> Vec<WgInterface> {
    match std::process::Command::new("wg")
        .args(["show", "all", "dump"])
        .output()
    {
        Ok(output) => {
            if output.status.success() {
                let stdout = String::from_utf8_lossy(&output.stdout);
                parse_wg_dump(&stdout)
            } else {
                tracing::warn!("wg show failed: {}", String::from_utf8_lossy(&output.stderr));
                Vec::new()
            }
        }
        Err(e) => {
            tracing::warn!("failed to run wg: {e}");
            Vec::new()
        }
    }
}

fn format_bytes(bytes: u64) -> String {
    if bytes >= 1_073_741_824 {
        format!("{:.1} GiB", bytes as f64 / 1_073_741_824.0)
    } else if bytes >= 1_048_576 {
        format!("{:.1} MiB", bytes as f64 / 1_048_576.0)
    } else if bytes >= 1024 {
        format!("{:.1} KiB", bytes as f64 / 1024.0)
    } else {
        format!("{bytes} B")
    }
}

fn format_handshake(ts: u64) -> String {
    if ts == 0 {
        return "never".to_string();
    }
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    let ago = now.saturating_sub(ts);
    if ago < 60 {
        format!("{ago}s ago")
    } else if ago < 3600 {
        format!("{}m ago", ago / 60)
    } else if ago < 86400 {
        format!("{}h ago", ago / 3600)
    } else {
        format!("{}d ago", ago / 86400)
    }
}

fn handshake_color(ts: u64) -> &'static str {
    if ts == 0 {
        return TEXT_DIM;
    }
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    let ago = now.saturating_sub(ts);
    if ago < 180 {
        "#22c55e" // green — recent
    } else if ago < 600 {
        "#f59e0b" // yellow — stale
    } else {
        "#ef4444" // red — old
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
            "wg.status" | "wg.interfaces" => {
                let ifaces = gather_wg_status();
                serde_json::to_string(&ifaces).map_err(|e| e.to_string())
            }
            "wg.peers" => {
                let iface_filter = cmd
                    .args
                    .get("interface")
                    .and_then(|v| v.as_str())
                    .unwrap_or("");
                let ifaces = gather_wg_status();
                let peers: Vec<&WgPeer> = ifaces
                    .iter()
                    .filter(|i| iface_filter.is_empty() || i.name == iface_filter)
                    .flat_map(|i| i.peers.iter())
                    .collect();
                serde_json::to_string(&peers).map_err(|e| e.to_string())
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

// ── UI ──

fn app() -> Element {
    let mut interfaces: Signal<Vec<WgInterface>> = use_signal(Vec::new);
    let mut hub_client: Signal<Option<Arc<cosmix_client::HubClient>>> = use_signal(|| None);

    // Connect to hub + gather initial status
    use_effect(move || {
        spawn(async move {
            match cosmix_client::HubClient::connect_default("wg").await {
                Ok(client) => {
                    let client = Arc::new(client);
                    hub_client.set(Some(client.clone()));
                    tracing::info!("connected to cosmix-hub as 'wg'");
                    tokio::spawn(handle_hub_commands(client));
                }
                Err(_) => {
                    tracing::debug!("hub not available, running standalone");
                }
            }
        });

        // Gather status on startup + periodic refresh
        interfaces.set(gather_wg_status());
        spawn(async move {
            loop {
                tokio::time::sleep(std::time::Duration::from_secs(10)).await;
                interfaces.set(gather_wg_status());
            }
        });
    });

    let total_peers: usize = interfaces().iter().map(|i| i.peers.len()).sum();

    rsx! {
        document::Style { "{CSS}" }
        div {
            style: "width:100%; height:100vh; display:flex; flex-direction:column; background:{BG_BASE}; color:{TEXT_PRIMARY}; font-family:-apple-system,BlinkMacSystemFont,'Segoe UI',Helvetica,Arial,sans-serif; font-size:13px;",

            // Header
            div {
                style: "padding:12px 16px; background:{BG_SURFACE}; border-bottom:1px solid {BORDER}; display:flex; align-items:center; gap:12px;",
                span { style: "font-weight:600; font-size:15px;", "WireGuard Mesh" }
                span { style: "color:{TEXT_DIM}; font-size:12px;",
                    "{interfaces().len()} interface(s), {total_peers} peer(s)"
                }
                div { style: "margin-left:auto;",
                    button {
                        style: "background:{BG_ELEVATED}; border:1px solid {BORDER}; color:{TEXT_MUTED}; padding:4px 10px; border-radius:4px; cursor:pointer; font-size:12px;",
                        onclick: move |_| interfaces.set(gather_wg_status()),
                        "Refresh"
                    }
                }
            }

            // Content
            div { style: "flex:1; overflow-y:auto; padding:16px; display:flex; flex-direction:column; gap:16px;",
                for iface in interfaces().iter() {
                    div { style: "background:{BG_SURFACE}; border-radius:6px; overflow:hidden;",
                        // Interface header
                        div {
                            style: "padding:10px 12px; display:flex; align-items:center; gap:12px; border-bottom:1px solid {BORDER};",
                            span { style: "font-weight:600; color:#60a5fa;", "{iface.name}" }
                            span { style: "color:{TEXT_DIM}; font-size:11px; font-family:monospace;",
                                "port {iface.listen_port}"
                            }
                            span { style: "color:{TEXT_DIM}; font-size:11px; font-family:monospace;",
                                "pubkey {short_key(&iface.public_key)}"
                            }
                            span { style: "color:{TEXT_DIM}; font-size:11px;",
                                "{iface.peers.len()} peers"
                            }
                        }

                        // Peer table header
                        if !iface.peers.is_empty() {
                            div {
                                style: "display:grid; grid-template-columns:160px 160px 2fr 100px 100px 100px; gap:8px; padding:6px 12px; background:{BG_ELEVATED}; font-size:11px; color:{TEXT_DIM}; text-transform:uppercase; letter-spacing:0.05em;",
                                span { "Public Key" }
                                span { "Endpoint" }
                                span { "Allowed IPs" }
                                span { "Handshake" }
                                span { "RX" }
                                span { "TX" }
                            }
                        }

                        // Peers
                        for peer in iface.peers.iter() {
                            div {
                                style: "display:grid; grid-template-columns:160px 160px 2fr 100px 100px 100px; gap:8px; padding:6px 12px; border-top:1px solid {BORDER}; font-size:12px;",
                                span { style: "font-family:monospace; font-size:11px; color:{TEXT_SECONDARY}; overflow:hidden; text-overflow:ellipsis;",
                                    "{short_key(&peer.public_key)}"
                                }
                                span { style: "font-family:monospace; font-size:11px; color:{TEXT_MUTED};",
                                    if peer.endpoint.is_empty() || peer.endpoint == "(none)" {
                                        "-"
                                    } else {
                                        "{peer.endpoint}"
                                    }
                                }
                                span { style: "font-family:monospace; font-size:11px; color:{TEXT_DIM};",
                                    "{peer.allowed_ips.join(\", \")}"
                                }
                                span { style: "font-size:11px; color:{handshake_color(peer.latest_handshake)};",
                                    "{format_handshake(peer.latest_handshake)}"
                                }
                                span { style: "font-size:11px; color:{TEXT_DIM};",
                                    "{format_bytes(peer.transfer_rx)}"
                                }
                                span { style: "font-size:11px; color:{TEXT_DIM};",
                                    "{format_bytes(peer.transfer_tx)}"
                                }
                            }
                        }

                        if iface.peers.is_empty() {
                            div { style: "padding:16px; text-align:center; color:{TEXT_DIM};",
                                "No peers configured"
                            }
                        }
                    }
                }
                if interfaces().is_empty() {
                    div { style: "padding:24px; text-align:center; color:{TEXT_DIM};",
                        "No WireGuard interfaces found. Is WireGuard running?"
                    }
                }
            }
        }
    }
}

fn short_key(key: &str) -> String {
    if key.len() > 12 {
        format!("{}...{}", &key[..6], &key[key.len() - 6..])
    } else {
        key.to_string()
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
"#;
