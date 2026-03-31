//! cosmix-wg — WireGuard mesh admin service for the cosmix appmesh.
//!
//! Registers as "wg" on the hub. Handles:
//! - `wg.status` — interface status (public key, listen port, fwmark)
//! - `wg.peers` — peer list with handshake/transfer stats
//! - `wg.interfaces` — list all WireGuard interfaces
//!
//! Reads WireGuard state via /proc/net and `wg show` parsing.
//! Accessible from any browser on the mesh.

use dioxus::prelude::*;
use serde::{Deserialize, Serialize};
use cosmix_ui::app_init::{THEME, use_hub_client, use_hub_handler, use_theme_css, use_theme_poll};
use cosmix_ui::menu::{menubar, standard_file_menu, MenuBar};

#[global_allocator]
static GLOBAL: mimalloc::MiMalloc = mimalloc::MiMalloc;

fn main() {
    cosmix_ui::app_init::launch_desktop("cosmix-wg", 900.0, 600.0, app);
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
        return "var(--fg-muted)";
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

fn dispatch_command(cmd: &cosmix_client::IncomingCommand) -> Result<String, String> {
    match cmd.command.as_str() {
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
    }
}

// ── UI ──

fn app() -> Element {
    let mut interfaces: Signal<Vec<WgInterface>> = use_signal(Vec::new);

    // Poll config every 30s for theme changes
    use_theme_poll(30);

    // Connect to hub + dispatch commands
    let hub = use_hub_client("wg");
    use_hub_handler(hub, "wg", dispatch_command);

    // Gather status on startup + periodic refresh
    use_effect(move || {
        interfaces.set(gather_wg_status());
        spawn(async move {
            loop {
                tokio::time::sleep(std::time::Duration::from_secs(10)).await;
                interfaces.set(gather_wg_status());
            }
        });
    });

    let total_peers: usize = interfaces().iter().map(|i| i.peers.len()).sum();

    use_theme_css();
    let theme = THEME.read();
    let fs = theme.font_size;
    let fs_sm = fs.saturating_sub(2);

    let app_menu = menubar(vec![standard_file_menu(vec![])]);

    rsx! {
        div {
            style: "width:100%; height:100vh; display:flex; flex-direction:column; background:var(--bg-primary); color:var(--fg-primary); font-family:var(--font-sans); font-size:{fs}px;",

            MenuBar {
                menu: app_menu,
                on_action: move |id: String| match id.as_str() {
                    "quit" => std::process::exit(0),
                    _ => {}
                },
            }

            // Header
            div {
                style: "padding:12px 16px; background:var(--bg-secondary); border-bottom:1px solid var(--border); display:flex; align-items:center; gap:12px;",
                span { style: "font-weight:600; font-size:var(--font-size-lg);", "WireGuard Mesh" }
                span { style: "color:var(--fg-muted); font-size:{fs_sm}px;",
                    "{interfaces().len()} interface(s), {total_peers} peer(s)"
                }
                div { style: "margin-left:auto;",
                    button {
                        style: "background:var(--bg-tertiary); border:1px solid var(--border); color:var(--fg-muted); padding:4px 10px; border-radius:4px; cursor:pointer; font-size:{fs_sm}px;",
                        onclick: move |_| interfaces.set(gather_wg_status()),
                        "Refresh"
                    }
                }
            }

            // Content
            div { style: "flex:1; overflow-y:auto; padding:16px; display:flex; flex-direction:column; gap:16px;",
                for iface in interfaces().iter() {
                    div { style: "background:var(--bg-secondary); border-radius:6px; overflow:hidden;",
                        // Interface header
                        div {
                            style: "padding:10px 12px; display:flex; align-items:center; gap:12px; border-bottom:1px solid var(--border);",
                            span { style: "font-weight:600; color:var(--accent);", "{iface.name}" }
                            span { style: "color:var(--fg-muted); font-size:var(--font-size-sm); font-family:monospace;",
                                "port {iface.listen_port}"
                            }
                            span { style: "color:var(--fg-muted); font-size:var(--font-size-sm); font-family:monospace;",
                                "pubkey {short_key(&iface.public_key)}"
                            }
                            span { style: "color:var(--fg-muted); font-size:var(--font-size-sm);",
                                "{iface.peers.len()} peers"
                            }
                        }

                        // Peer table header
                        if !iface.peers.is_empty() {
                            div {
                                style: "display:grid; grid-template-columns:160px 160px 2fr 100px 100px 100px; gap:8px; padding:6px 12px; background:var(--bg-tertiary); font-size:var(--font-size-sm); color:var(--fg-muted); text-transform:uppercase; letter-spacing:0.05em;",
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
                                style: "display:grid; grid-template-columns:160px 160px 2fr 100px 100px 100px; gap:8px; padding:6px 12px; border-top:1px solid var(--border); font-size:{fs_sm}px;",
                                span { style: "font-family:monospace; font-size:var(--font-size-sm); color:var(--fg-secondary); overflow:hidden; text-overflow:ellipsis;",
                                    "{short_key(&peer.public_key)}"
                                }
                                span { style: "font-family:monospace; font-size:var(--font-size-sm); color:var(--fg-muted);",
                                    if peer.endpoint.is_empty() || peer.endpoint == "(none)" {
                                        "-"
                                    } else {
                                        "{peer.endpoint}"
                                    }
                                }
                                span { style: "font-family:monospace; font-size:var(--font-size-sm); color:var(--fg-muted);",
                                    "{peer.allowed_ips.join(\", \")}"
                                }
                                span { style: "font-size:var(--font-size-sm); color:{handshake_color(peer.latest_handshake)};",
                                    "{format_handshake(peer.latest_handshake)}"
                                }
                                span { style: "font-size:var(--font-size-sm); color:var(--fg-muted);",
                                    "{format_bytes(peer.transfer_rx)}"
                                }
                                span { style: "font-size:var(--font-size-sm); color:var(--fg-muted);",
                                    "{format_bytes(peer.transfer_tx)}"
                                }
                            }
                        }

                        if iface.peers.is_empty() {
                            div { style: "padding:16px; text-align:center; color:var(--fg-muted);",
                                "No peers configured"
                            }
                        }
                    }
                }
                if interfaces().is_empty() {
                    div { style: "padding:24px; text-align:center; color:var(--fg-muted);",
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
