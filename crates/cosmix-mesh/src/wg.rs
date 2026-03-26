//! WireGuard interface query — read-only status of existing WG tunnels.

use anyhow::Result;
use serde::Serialize;
use wireguard_control::{Backend, Device, InterfaceName};

/// Status of a single WireGuard peer.
#[derive(Debug, Clone, Serialize)]
pub struct WgPeerStatus {
    pub public_key: String,
    pub endpoint: Option<String>,
    pub allowed_ips: Vec<String>,
    pub last_handshake_secs: Option<u64>,
    pub tx_bytes: u64,
    pub rx_bytes: u64,
}

/// Status of a WireGuard interface.
#[derive(Debug, Clone, Serialize)]
pub struct WgInterfaceStatus {
    pub name: String,
    pub public_key: Option<String>,
    pub listen_port: Option<u16>,
    pub peers: Vec<WgPeerStatus>,
}

/// Query the status of a WireGuard interface (e.g. "wg1").
///
/// Requires CAP_NET_ADMIN or root.
pub fn query_interface(name: &str) -> Result<WgInterfaceStatus> {
    let iface_name: InterfaceName = name.parse()
        .map_err(|_| anyhow::anyhow!("Invalid interface name: '{name}'"))?;

    let device = Device::get(&iface_name, Backend::Kernel)
        .map_err(|e| anyhow::anyhow!("Failed to query WG interface '{name}': {e}"))?;

    let public_key = device.public_key.map(|k| base64_encode_key(&k));

    let peers = device.peers.iter().map(|p| {
        let last_handshake_secs = p.stats.last_handshake_time
            .and_then(|t| t.elapsed().ok())
            .map(|d| d.as_secs());

        WgPeerStatus {
            public_key: base64_encode_key(&p.config.public_key),
            endpoint: p.config.endpoint.map(|e| e.to_string()),
            allowed_ips: p.config.allowed_ips.iter()
                .map(|ip| format!("{}/{}", ip.address, ip.cidr))
                .collect(),
            last_handshake_secs,
            tx_bytes: p.stats.tx_bytes,
            rx_bytes: p.stats.rx_bytes,
        }
    }).collect();

    Ok(WgInterfaceStatus {
        name: name.to_string(),
        public_key,
        listen_port: device.listen_port,
        peers,
    })
}

/// List all WireGuard interface names on the system.
pub fn list_interfaces() -> Result<Vec<String>> {
    let devices = Device::list(Backend::Kernel)
        .map_err(|e| anyhow::anyhow!("Failed to enumerate WG interfaces: {e}"))?;

    Ok(devices.into_iter().map(|d| d.to_string()).collect())
}

fn base64_encode_key(key: &wireguard_control::Key) -> String {
    use base64::Engine;
    base64::engine::general_purpose::STANDARD.encode(key.as_bytes())
}
