//! Client-side `wg.conf` rendering — the file an operator hands to a
//! peer (phone, laptop, remote node) so its WireGuard client can dial
//! the hub.
//!
//! Mirrors `wg-admin`'s `WireGuardService::generatePeerConfig`
//! (`WireGuardService.php:153`) byte-for-byte: the peer's `[Interface]`
//! block holds its own private key and assigned address; the single
//! `[Peer]` block holds the server's public key and routing data. We
//! deliberately do *not* re-create wg-admin's `$fullTunnel` /24-derivation
//! flag — the caller already knows which `AllowedIPs` it wants
//! (full-tunnel `0.0.0.0/0, ::/0`, lan-only `<peer subnet>`, or
//! anything else), so we keep this layer dumb and let the caller
//! compose. See `_doc/planned/cosmix-wgd.md` §1 for the C4 deliverable.
//!
//! `WgClientInterface` intentionally does not reuse `WgInterface` from
//! `render.rs`. The server-side type carries `listen_port` and `name`,
//! which a client never has (clients dial out and have no kernel-named
//! interface in the wg.conf — `wg-quick` infers the name from the file
//! path). Forcing one struct to model both would invite "what does
//! `listen_port = 0` mean on a client?" confusion.

use crate::keys::{WgPresharedKey, WgPrivateKey, WgPublicKey};
use crate::render::{RenderError, check_no_line_break};
use std::fmt::Write;

/// A client peer's `[Interface]` block — its own identity + address(es).
#[derive(Clone)]
pub struct WgClientInterface {
    /// The client's private key. Rendered as `PrivateKey = <base64>`.
    pub private_key: WgPrivateKey,
    /// CIDRs to assign to the client's interface, one `Address = ...`
    /// line per entry. Typically a single `/32` (or `/128`) carved from
    /// the mesh subnet by IPAM (C5).
    pub addresses: Vec<String>,
    /// Optional MTU. `None` omits the line.
    pub mtu: Option<u16>,
}

/// The server's `[Peer]` block from the client's point of view.
#[derive(Clone)]
pub struct WgClientPeer {
    /// Server's public key. Rendered as `PublicKey = <base64>`.
    pub public_key: WgPublicKey,
    /// Optional pre-shared key.
    pub preshared_key: Option<WgPresharedKey>,
    /// CIDRs the client should route through this tunnel. Caller picks
    /// the policy: `["0.0.0.0/0", "::/0"]` for full-tunnel, the mesh
    /// subnet for lan-only. Emitted as one comma-joined `AllowedIPs`
    /// line; always emitted (matches wg-admin).
    pub allowed_ips: Vec<String>,
    /// Server's `host:port`. Caller composes this; we never derive it.
    pub endpoint: Option<String>,
    /// Persistent keepalive in seconds, **always** emitted to match
    /// wg-admin's hard-coded `PersistentKeepalive = 25` line on every
    /// client config (WireGuardService.php:188). This is the line that
    /// keeps mobile / NAT-behind clients reachable; omitting it would
    /// silently break them once the NAT mapping expires. Use [`WgClientPeer::WG_ADMIN_KEEPALIVE_SECS`]
    /// (25) for parity with wg-admin, or `0` to explicitly disable
    /// keepalive on stationary peers behind a routable address.
    pub persistent_keepalive: u16,
}

impl WgClientPeer {
    /// The keepalive value `wg-admin` hard-codes on every client conf.
    /// Use this as a default when the caller does not have a specific
    /// reason to choose otherwise.
    pub const WG_ADMIN_KEEPALIVE_SECS: u16 = 25;
}

/// Render a client peer's `wg.conf`. Errors with
/// [`RenderError::ControlCharacter`] on the same threat model as
/// [`crate::render::render_interface_conf`] — see the `RenderError`
/// docs for the rationale.
///
/// Ordering (frozen — golden tests gate on this):
/// 1. `[Interface]` header
/// 2. `PrivateKey`
/// 3. `Address` lines (one per entry, in input order)
/// 4. `MTU` if `Some`
/// 5. blank line + `[Peer]` header
/// 6. `PublicKey`
/// 7. `PresharedKey` if `Some`
/// 8. `AllowedIPs` (always emitted)
/// 9. `Endpoint` if `Some`
/// 10. `PersistentKeepalive` (always emitted — matches wg-admin's
///     hard-coded line; pass `0` to disable in the wg sense).
pub fn render_peer_conf(
    iface: &WgClientInterface,
    peer: &WgClientPeer,
) -> Result<String, RenderError> {
    for addr in &iface.addresses {
        check_no_line_break(addr, "addresses")?;
    }
    for cidr in &peer.allowed_ips {
        check_no_line_break(cidr, "allowed_ips")?;
    }
    if let Some(endpoint) = &peer.endpoint {
        check_no_line_break(endpoint, "endpoint")?;
    }

    let mut out = String::with_capacity(320);

    out.push_str("[Interface]\n");
    let _ = writeln!(out, "PrivateKey = {}", iface.private_key.to_base64());
    for addr in &iface.addresses {
        let _ = writeln!(out, "Address = {addr}");
    }
    if let Some(mtu) = iface.mtu {
        let _ = writeln!(out, "MTU = {mtu}");
    }

    out.push_str("\n[Peer]\n");
    let _ = writeln!(out, "PublicKey = {}", peer.public_key.to_base64());
    if let Some(psk) = &peer.preshared_key {
        let _ = writeln!(out, "PresharedKey = {}", psk.to_base64());
    }
    let _ = writeln!(out, "AllowedIPs = {}", peer.allowed_ips.join(", "));
    if let Some(endpoint) = &peer.endpoint {
        let _ = writeln!(out, "Endpoint = {endpoint}");
    }
    let _ = writeln!(out, "PersistentKeepalive = {}", peer.persistent_keepalive);

    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    // Reuse C3's deterministic test vectors so the golden text aligns.
    const CLAMPED_ZERO_PRIV_B64: &str = "AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAEA=";
    const ZERO_PUB_B64: &str = "AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA=";
    const ZERO_PSK_B64: &str = "AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA=";

    fn zero_priv() -> WgPrivateKey {
        WgPrivateKey::from_base64(CLAMPED_ZERO_PRIV_B64).unwrap()
    }
    fn zero_pub() -> WgPublicKey {
        WgPublicKey::from_base64(ZERO_PUB_B64).unwrap()
    }
    fn zero_psk() -> WgPresharedKey {
        WgPresharedKey::from_base64(ZERO_PSK_B64).unwrap()
    }

    #[test]
    fn renders_minimal_client_conf() {
        let iface = WgClientInterface {
            private_key: zero_priv(),
            addresses: vec!["192.0.2.5/32".into()],
            mtu: None,
        };
        let peer = WgClientPeer {
            public_key: zero_pub(),
            preshared_key: None,
            allowed_ips: vec!["192.0.2.0/24".into()],
            endpoint: Some("vpn.example:51820".into()),
            persistent_keepalive: WgClientPeer::WG_ADMIN_KEEPALIVE_SECS,
        };
        let out = render_peer_conf(&iface, &peer).unwrap();
        assert_eq!(
            out,
            "[Interface]\n\
             PrivateKey = AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAEA=\n\
             Address = 192.0.2.5/32\n\
             \n\
             [Peer]\n\
             PublicKey = AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA=\n\
             AllowedIPs = 192.0.2.0/24\n\
             Endpoint = vpn.example:51820\n\
             PersistentKeepalive = 25\n",
        );
    }

    #[test]
    fn keepalive_zero_still_emits_line() {
        // wg(8) accepts `PersistentKeepalive = 0` as "off". The line is
        // still emitted — byte-shape parity with wg-admin demands an
        // unconditional emit; the *value* is what the caller controls.
        let iface = WgClientInterface {
            private_key: zero_priv(),
            addresses: vec!["192.0.2.5/32".into()],
            mtu: None,
        };
        let peer = WgClientPeer {
            public_key: zero_pub(),
            preshared_key: None,
            allowed_ips: vec!["192.0.2.0/24".into()],
            endpoint: Some("vpn.example:51820".into()),
            persistent_keepalive: 0,
        };
        let out = render_peer_conf(&iface, &peer).unwrap();
        assert!(
            out.ends_with("PersistentKeepalive = 0\n"),
            "expected trailing keepalive=0 line: {out}"
        );
    }

    #[test]
    fn renders_full_tunnel_with_all_optionals() {
        let iface = WgClientInterface {
            private_key: zero_priv(),
            addresses: vec!["192.0.2.5/32".into(), "fd00:2::5/128".into()],
            mtu: Some(1420),
        };
        let peer = WgClientPeer {
            public_key: zero_pub(),
            preshared_key: Some(zero_psk()),
            allowed_ips: vec!["0.0.0.0/0".into(), "::/0".into()],
            endpoint: Some("vpn.example:51820".into()),
            persistent_keepalive: 25,
        };
        let out = render_peer_conf(&iface, &peer).unwrap();
        assert_eq!(
            out,
            "[Interface]\n\
             PrivateKey = AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAEA=\n\
             Address = 192.0.2.5/32\n\
             Address = fd00:2::5/128\n\
             MTU = 1420\n\
             \n\
             [Peer]\n\
             PublicKey = AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA=\n\
             PresharedKey = AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA=\n\
             AllowedIPs = 0.0.0.0/0, ::/0\n\
             Endpoint = vpn.example:51820\n\
             PersistentKeepalive = 25\n",
        );
    }

    #[test]
    fn ordering_matches_wg_admin_for_full_client_config() {
        // wg-admin's WireGuardService::generatePeerConfig emits in this
        // exact order — PrivateKey, Address, (MTU), [Peer], PublicKey,
        // (PresharedKey), AllowedIPs, Endpoint, PersistentKeepalive.
        let iface = WgClientInterface {
            private_key: zero_priv(),
            addresses: vec!["192.0.2.5/32".into()],
            mtu: Some(1420),
        };
        let peer = WgClientPeer {
            public_key: zero_pub(),
            preshared_key: Some(zero_psk()),
            allowed_ips: vec!["192.0.2.0/24".into()],
            endpoint: Some("vpn.example:51820".into()),
            persistent_keepalive: 25,
        };
        let out = render_peer_conf(&iface, &peer).unwrap();
        let mut cursor = 0;
        for needle in [
            "[Interface]",
            "PrivateKey = ",
            "Address = ",
            "MTU = ",
            "[Peer]",
            "PublicKey = ",
            "PresharedKey = ",
            "AllowedIPs = ",
            "Endpoint = ",
            "PersistentKeepalive = ",
        ] {
            let idx = out[cursor..]
                .find(needle)
                .unwrap_or_else(|| panic!("missing or out-of-order: {needle} in:\n{out}"));
            cursor += idx + needle.len();
        }
    }

    #[test]
    fn rejects_newline_in_address() {
        let iface = WgClientInterface {
            private_key: zero_priv(),
            addresses: vec!["192.0.2.5/32\n[Peer]\nPublicKey = injected".into()],
            mtu: None,
        };
        let peer = WgClientPeer {
            public_key: zero_pub(),
            preshared_key: None,
            allowed_ips: vec!["0.0.0.0/0".into()],
            endpoint: None,
            persistent_keepalive: 0,
        };
        match render_peer_conf(&iface, &peer) {
            Err(RenderError::ControlCharacter { field }) => assert_eq!(field, "addresses"),
            other => panic!("expected ControlCharacter, got {other:?}"),
        }
    }

    #[test]
    fn rejects_newline_in_allowed_ips() {
        let iface = WgClientInterface {
            private_key: zero_priv(),
            addresses: vec!["192.0.2.5/32".into()],
            mtu: None,
        };
        let peer = WgClientPeer {
            public_key: zero_pub(),
            preshared_key: None,
            allowed_ips: vec!["0.0.0.0/0\nEndpoint = attacker:443".into()],
            endpoint: None,
            persistent_keepalive: 0,
        };
        match render_peer_conf(&iface, &peer) {
            Err(RenderError::ControlCharacter { field }) => assert_eq!(field, "allowed_ips"),
            other => panic!("expected ControlCharacter, got {other:?}"),
        }
    }

    #[test]
    fn rejects_carriage_return_in_endpoint() {
        let iface = WgClientInterface {
            private_key: zero_priv(),
            addresses: vec!["192.0.2.5/32".into()],
            mtu: None,
        };
        let peer = WgClientPeer {
            public_key: zero_pub(),
            preshared_key: None,
            allowed_ips: vec!["0.0.0.0/0".into()],
            endpoint: Some("vpn.example:51820\r\nAllowedIPs = 0.0.0.0/0".into()),
            persistent_keepalive: 0,
        };
        match render_peer_conf(&iface, &peer) {
            Err(RenderError::ControlCharacter { field }) => assert_eq!(field, "endpoint"),
            other => panic!("expected ControlCharacter, got {other:?}"),
        }
    }

    #[test]
    fn empty_allowed_ips_emits_empty_value() {
        // wg-admin parity: line always emitted, even with empty value.
        let iface = WgClientInterface {
            private_key: zero_priv(),
            addresses: vec!["192.0.2.5/32".into()],
            mtu: None,
        };
        let peer = WgClientPeer {
            public_key: zero_pub(),
            preshared_key: None,
            allowed_ips: vec![],
            endpoint: None,
            persistent_keepalive: 0,
        };
        let out = render_peer_conf(&iface, &peer).unwrap();
        assert!(out.contains("\nAllowedIPs = \n"));
    }
}
