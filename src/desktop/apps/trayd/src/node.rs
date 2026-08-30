//! On-demand noded reachability probe using the canonical node config.

use std::net::{IpAddr, Ipv4Addr, SocketAddr, TcpStream};
use std::time::Duration;

use cosmix_config::node::{load_node_config, NodeConfig};

const DEFAULT_NODED_PORT: u16 = 4200;
const PROBE_TIMEOUT: Duration = Duration::from_secs(1);

/// Whether the LOCAL noded accepts a connection.
///
/// Both probe targets — this node's own WG address and loopback on the same
/// port — terminate at the noded running on this machine, so a success proves
/// the local control plane is up and proves nothing about any mesh peer. The
/// UI must not call this "mesh reachable": with WireGuard down and every peer
/// unreachable, a noded listening on 127.0.0.1 still answers.
pub(crate) fn noded_is_reachable() -> bool {
    probe_targets()
        .into_iter()
        .any(|address| TcpStream::connect_timeout(&address, PROBE_TIMEOUT).is_ok())
}

fn probe_targets() -> Vec<SocketAddr> {
    match load_node_config() {
        Ok(Some(config)) => targets_for(&config),
        Ok(None) | Err(_) => vec![loopback(DEFAULT_NODED_PORT)],
    }
}

/// Resolve the canonical WG listener first and loopback on the same port
/// second. Daemons derive the listener through `NodeConfig::noded_listen()`;
/// this tray deliberately uses the identical derivation.
fn targets_for(config: &NodeConfig) -> Vec<SocketAddr> {
    let mut targets = Vec::with_capacity(2);
    if let Ok(canonical) = config.noded_listen().parse::<SocketAddr>() {
        targets.push(canonical);
    }
    let fallback = loopback(config.noded.port);
    if !targets.contains(&fallback) {
        targets.push(fallback);
    }
    targets
}

fn loopback(port: u16) -> SocketAddr {
    SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), port)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn strict_mix_config_resolves_wg_then_loopback() {
        let fixture = r#"
            node: "example"
            wg_ip: "192.0.2.42"
            noded: {
                port: 4312
            }
        "#;
        let config: NodeConfig =
            cosmix_config::from_conf_mix_str(fixture).expect("valid strict Mix fixture");
        assert_eq!(
            targets_for(&config),
            vec![
                "192.0.2.42:4312".parse().expect("valid fixture address"),
                "127.0.0.1:4312".parse().expect("valid fixture address"),
            ]
        );
    }

    #[test]
    fn empty_wg_address_uses_loopback_only() {
        let config = NodeConfig {
            wg_ip: String::new(),
            ..Default::default()
        };
        assert_eq!(targets_for(&config), vec![loopback(DEFAULT_NODED_PORT)]);
    }
}
