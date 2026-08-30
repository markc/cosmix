//! `wg.conf` rendering value types and formatter.
//!
//! Produces byte-for-byte the text that `wg setconf` / `wg-quick` accept as
//! input — the same lines and ordering `wg-admin` emits, so existing
//! deployments cross-check 1:1. The renderer is pure: no I/O, no validation
//! beyond what the field types already enforce, no opinions on where the
//! caller writes the result.
//!
//! Field shape mirrors `_doc/planned/cosmix-wgd.md` §3.2 / §3.3 *as a
//! rendering input*, not as the substrate row. The property-substrate
//! schema carries extra fields (`ownership`, `rotation_policy`,
//! `previous_pubkey`, `is_enabled`, …) that the reconciler resolves
//! *before* assembling a [`WgInterface`] / [`WgPeer`] pair for the
//! renderer. Disabled peers are filtered out by the caller; this module
//! does not know what an "enabled" peer is.
//!
//! **Hooks intentionally absent.** §3.2 freezes `PostUp` / `PostDown` out
//! of the Bus-writable schema (root-RCE surface). They live in
//! `/etc/cosmix/wgd/iface-hooks/<iface>.toml`, not in this value type, and
//! the renderer therefore never emits them. Operators write that file by
//! hand.
//!
//! **Client-side rendering is C4.** `generatePeerConfig`-equivalent output
//! (the peer's own `[Interface]` block with the server as its sole peer)
//! lives behind `render_peer_conf` in the next commit, alongside QR
//! encoding and `iface_name_for_mesh`.

use crate::keys::{WgPresharedKey, WgPrivateKey, WgPublicKey};
use std::fmt::Write;

/// Errors from [`render_interface_conf`]. The renderer is pure formatting,
/// not a schema validator — it does **not** check that addresses are valid
/// CIDRs, that endpoints resolve, or that ports are non-zero (the property
/// substrate schema is the right layer for that). It does, however, refuse
/// any value that contains a line terminator or NUL byte, because the wg
/// config format is line-oriented and silently splicing an attacker-
/// controlled `"192.0.2.5/32\n[Peer]\nPublicKey = ...\n"` past a less-
/// strict substrate writer would otherwise let the renderer forge a new
/// `[Peer]` block (the file-format equivalent of SQL injection).
#[derive(Debug, thiserror::Error)]
pub enum RenderError {
    /// One of `addresses[i]`, `allowed_ips[i]`, or `endpoint` contained a
    /// `\n`, `\r`, or `\0`. The field name identifies which struct field
    /// (not the index — callers iterate to localise).
    #[error("control character (CR/LF/NUL) in field `{field}`")]
    ControlCharacter { field: &'static str },
}

/// Reject any string that would inject a newline into the rendered conf.
/// Tabs and other in-line whitespace are allowed (they pass through
/// untouched and the kernel parser tolerates them); only line terminators
/// and NUL are denied.
///
/// `pub(crate)` so the client-side renderer in `client.rs` can share the
/// same injection-defence pass without duplicating the byte scan.
pub(crate) fn check_no_line_break(s: &str, field: &'static str) -> Result<(), RenderError> {
    if s.bytes().any(|b| b == b'\n' || b == b'\r' || b == 0) {
        return Err(RenderError::ControlCharacter { field });
    }
    Ok(())
}

/// A renderable WireGuard interface. Holds only the fields that appear in
/// the rendered `[Interface]` block; substrate-only fields (ownership,
/// rotation policy, audit metadata) are not part of this type.
///
/// `private_key` is inlined into the output because `wg setconf` requires
/// it. On disk in the property substrate it lives at
/// `secrets/<name>.key`; the reconciler reads that file and threads the
/// loaded [`WgPrivateKey`] into this struct on the way to render.
#[derive(Clone)]
pub struct WgInterface {
    /// Kernel interface name (e.g. `wg0`). Constructed by the caller —
    /// see `iface_name_for_mesh` in C4 for the mesh-aware variant.
    pub name: String,
    /// UDP listen port; required by WireGuard.
    pub listen_port: u16,
    /// Interface private key. Rendered as `PrivateKey = <base64>`.
    pub private_key: WgPrivateKey,
    /// Addresses to assign to the interface, one CIDR per entry. Emitted
    /// as one `Address = ...` line per element, preserving order. Empty
    /// vec emits no line (kernel will refuse to come up — caller's
    /// problem, not the renderer's).
    pub addresses: Vec<String>,
    /// Optional MTU. `None` omits the line and lets the kernel pick.
    pub mtu: Option<u16>,
}

/// A renderable WireGuard peer. One of these becomes one `[Peer]` block
/// in `render_interface_conf` output.
#[derive(Clone)]
pub struct WgPeer {
    /// Peer's public key. Rendered as `PublicKey = <base64>`.
    pub public_key: WgPublicKey,
    /// Optional pre-shared key. Rendered as `PresharedKey = <base64>`
    /// when present; the line is omitted entirely when `None`. PSK is
    /// optional per `_doc/planned/cosmix-wgd.md` §3.4 Q3.
    pub preshared_key: Option<WgPresharedKey>,
    /// CIDRs the peer is authorised to claim as source / receive as
    /// destination. Emitted as a single comma-joined `AllowedIPs = ...`
    /// line (matches `wg-admin`'s single-line form; the kernel coalesces
    /// either form, so this is purely a formatting choice).
    ///
    /// Empty vec omits the line, which makes the resulting `[Peer]`
    /// block kernel-rejected — caller's responsibility to filter
    /// peers without any allowed-IPs before rendering.
    pub allowed_ips: Vec<String>,
    /// Optional `host:port` for site-to-site / NAT-traversal peers.
    /// `None` for client peers (server-side `[Peer]` blocks).
    pub endpoint: Option<String>,
    /// Optional persistent-keepalive interval in seconds. `None` omits
    /// the line, leaving the peer with no keepalive.
    pub persistent_keepalive: Option<u16>,
}

/// Render a server-side `wg.conf` for `iface` with `peers`, matching the
/// byte shape `wg-admin` emits so deployments can cross-check existing
/// wg-admin configs against new cosmix-wgd output 1:1.
///
/// Ordering (frozen — golden tests gate on this):
/// 1. `[Interface]` header
/// 2. `Address` lines (one per entry, in input order)
/// 3. `ListenPort`
/// 4. `PrivateKey`
/// 5. `MTU` if `Some`
/// 6. For each peer (in input order):
///    a. blank line + `[Peer]` header
///    b. `PublicKey`
///    c. `PresharedKey` if `Some`
///    d. `AllowedIPs` (always emitted; empty vec emits `AllowedIPs = `,
///    matching wg-admin's unconditional emit — the kernel will reject
///    such a peer at install time, but the renderer stays byte-faithful)
///    e. `Endpoint` if `Some`
///    f. `PersistentKeepalive` if `Some`
///
/// Trailing newline on the final line — wg-admin's output does so too,
/// and the kernel parser accepts either form.
///
/// Errors with [`RenderError::ControlCharacter`] if any caller-supplied
/// string (`addresses`, `allowed_ips`, `endpoint`) contains `\n`, `\r`,
/// or NUL. See the [`RenderError`] docs for the threat model.
pub fn render_interface_conf(iface: &WgInterface, peers: &[WgPeer]) -> Result<String, RenderError> {
    // Validate every text field that gets spliced into the output before
    // writing a byte. Fail-fast keeps the partial-render-then-abort path
    // (which a stream-style writer would have) entirely off the table.
    for addr in &iface.addresses {
        check_no_line_break(addr, "addresses")?;
    }
    for peer in peers {
        for cidr in &peer.allowed_ips {
            check_no_line_break(cidr, "allowed_ips")?;
        }
        if let Some(endpoint) = &peer.endpoint {
            check_no_line_break(endpoint, "endpoint")?;
        }
    }

    // Pre-size for the common case: ~200 B header, ~150 B per peer.
    let mut out = String::with_capacity(256 + 160 * peers.len());

    out.push_str("[Interface]\n");
    for addr in &iface.addresses {
        let _ = writeln!(out, "Address = {addr}");
    }
    let _ = writeln!(out, "ListenPort = {}", iface.listen_port);
    let _ = writeln!(out, "PrivateKey = {}", iface.private_key.to_base64());
    if let Some(mtu) = iface.mtu {
        let _ = writeln!(out, "MTU = {mtu}");
    }

    for peer in peers {
        out.push_str("\n[Peer]\n");
        let _ = writeln!(out, "PublicKey = {}", peer.public_key.to_base64());
        if let Some(psk) = &peer.preshared_key {
            let _ = writeln!(out, "PresharedKey = {}", psk.to_base64());
        }
        // Always emit AllowedIPs (matches wg-admin's unconditional emit —
        // see WireGuardService.php:134). Empty vec → empty value.
        let _ = writeln!(out, "AllowedIPs = {}", peer.allowed_ips.join(", "));
        if let Some(endpoint) = &peer.endpoint {
            let _ = writeln!(out, "Endpoint = {endpoint}");
        }
        if let Some(ka) = peer.persistent_keepalive {
            let _ = writeln!(out, "PersistentKeepalive = {ka}");
        }
    }

    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::keys::{WgKeyPair, WgPresharedKey, WgPrivateKey, WgPublicKey};

    // Deterministic test vectors — all-zero scalar / all-zero pubkey /
    // all-zero PSK. `from_private_bytes` clamps so the stored bytes are
    // not literally zero; the corresponding base64 strings are stable
    // and used as golden text below.
    //
    // The clamped all-zero scalar is `[0x00, 0x00, …, 0x40]` →
    // base64 `AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAEA=`.
    // Its public key (computed once during test development) is
    // `Twj/SQuwnvjo2P0ektPp7+JcVeLI5OINeyKvN1HuFW8=`.
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
    fn renders_minimal_interface_no_peers() {
        let iface = WgInterface {
            name: "wg0".into(),
            listen_port: 51820,
            private_key: zero_priv(),
            addresses: vec!["192.0.2.1/24".into()],
            mtu: None,
        };
        let out = render_interface_conf(&iface, &[]).unwrap();
        assert_eq!(
            out,
            "[Interface]\n\
             Address = 192.0.2.1/24\n\
             ListenPort = 51820\n\
             PrivateKey = AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAEA=\n",
        );
    }

    #[test]
    fn renders_mtu_when_present() {
        let iface = WgInterface {
            name: "wg0".into(),
            listen_port: 51820,
            private_key: zero_priv(),
            addresses: vec!["192.0.2.1/24".into()],
            mtu: Some(1420),
        };
        let out = render_interface_conf(&iface, &[]).unwrap();
        assert!(out.contains("\nMTU = 1420\n"));
    }

    #[test]
    fn renders_multiple_address_lines_preserving_order() {
        let iface = WgInterface {
            name: "wg0".into(),
            listen_port: 51820,
            private_key: zero_priv(),
            addresses: vec!["192.0.2.1/24".into(), "fd00:1::1/64".into()],
            mtu: None,
        };
        let out = render_interface_conf(&iface, &[]).unwrap();
        let v4 = out.find("Address = 192.0.2.1/24").unwrap();
        let v6 = out.find("Address = fd00:1::1/64").unwrap();
        assert!(v4 < v6, "v4 must precede v6 (input order): {out}");
    }

    #[test]
    fn omits_addresses_when_empty() {
        let iface = WgInterface {
            name: "wg0".into(),
            listen_port: 51820,
            private_key: zero_priv(),
            addresses: vec![],
            mtu: None,
        };
        let out = render_interface_conf(&iface, &[]).unwrap();
        assert!(!out.contains("Address"), "no Address line expected: {out}");
        assert!(out.contains("ListenPort"));
    }

    #[test]
    fn renders_single_peer_no_optionals() {
        let iface = WgInterface {
            name: "wg0".into(),
            listen_port: 51820,
            private_key: zero_priv(),
            addresses: vec!["192.0.2.1/24".into()],
            mtu: None,
        };
        let peer = WgPeer {
            public_key: zero_pub(),
            preshared_key: None,
            allowed_ips: vec!["192.0.2.5/32".into()],
            endpoint: None,
            persistent_keepalive: None,
        };
        let out = render_interface_conf(&iface, std::slice::from_ref(&peer)).unwrap();
        assert_eq!(
            out,
            "[Interface]\n\
             Address = 192.0.2.1/24\n\
             ListenPort = 51820\n\
             PrivateKey = AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAEA=\n\
             \n\
             [Peer]\n\
             PublicKey = AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA=\n\
             AllowedIPs = 192.0.2.5/32\n",
        );
    }

    #[test]
    fn renders_full_peer_block_with_all_optionals() {
        let iface = WgInterface {
            name: "wg0".into(),
            listen_port: 51820,
            private_key: zero_priv(),
            addresses: vec!["192.0.2.1/24".into()],
            mtu: Some(1420),
        };
        let peer = WgPeer {
            public_key: zero_pub(),
            preshared_key: Some(zero_psk()),
            allowed_ips: vec!["192.0.2.5/32".into(), "198.51.100.10/32".into()],
            endpoint: Some("203.0.113.7:51820".into()),
            persistent_keepalive: Some(25),
        };
        let out = render_interface_conf(&iface, std::slice::from_ref(&peer)).unwrap();
        assert_eq!(
            out,
            "[Interface]\n\
             Address = 192.0.2.1/24\n\
             ListenPort = 51820\n\
             PrivateKey = AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAEA=\n\
             MTU = 1420\n\
             \n\
             [Peer]\n\
             PublicKey = AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA=\n\
             PresharedKey = AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA=\n\
             AllowedIPs = 192.0.2.5/32, 198.51.100.10/32\n\
             Endpoint = 203.0.113.7:51820\n\
             PersistentKeepalive = 25\n",
        );
    }

    #[test]
    fn renders_multiple_peers_in_input_order() {
        let iface = WgInterface {
            name: "wg0".into(),
            listen_port: 51820,
            private_key: zero_priv(),
            addresses: vec!["192.0.2.1/24".into()],
            mtu: None,
        };
        let kp_a = WgKeyPair::from_private_bytes([1u8; 32]);
        let kp_b = WgKeyPair::from_private_bytes([2u8; 32]);
        let peers = vec![
            WgPeer {
                public_key: kp_a.public,
                preshared_key: None,
                allowed_ips: vec!["192.0.2.5/32".into()],
                endpoint: None,
                persistent_keepalive: None,
            },
            WgPeer {
                public_key: kp_b.public,
                preshared_key: None,
                allowed_ips: vec!["192.0.2.6/32".into()],
                endpoint: None,
                persistent_keepalive: None,
            },
        ];
        let out = render_interface_conf(&iface, &peers).unwrap();
        let a_at = out.find(&kp_a.public.to_base64()).expect("peer A present");
        let b_at = out.find(&kp_b.public.to_base64()).expect("peer B present");
        assert!(a_at < b_at, "peer A must precede peer B");
        // Exactly two [Peer] blocks.
        assert_eq!(out.matches("\n[Peer]\n").count(), 2);
    }

    #[test]
    fn renders_empty_allowed_ips_as_empty_value() {
        // Matches wg-admin (WireGuardService.php:134 emits the line
        // unconditionally). Byte-fidelity is the contract; the resulting
        // peer block is kernel-rejected at install time, which is the
        // higher layer's job to surface.
        let iface = WgInterface {
            name: "wg0".into(),
            listen_port: 51820,
            private_key: zero_priv(),
            addresses: vec!["192.0.2.1/24".into()],
            mtu: None,
        };
        let peer = WgPeer {
            public_key: zero_pub(),
            preshared_key: None,
            allowed_ips: vec![],
            endpoint: None,
            persistent_keepalive: None,
        };
        let out = render_interface_conf(&iface, std::slice::from_ref(&peer)).unwrap();
        assert!(
            out.contains("\nAllowedIPs = \n"),
            "expected empty AllowedIPs line: {out}"
        );
    }

    #[test]
    fn renders_no_peer_block_when_peers_empty() {
        let iface = WgInterface {
            name: "wg0".into(),
            listen_port: 51820,
            private_key: zero_priv(),
            addresses: vec!["192.0.2.1/24".into()],
            mtu: None,
        };
        let out = render_interface_conf(&iface, &[]).unwrap();
        assert!(!out.contains("[Peer]"));
        // Exactly one trailing newline after PrivateKey, no spurious blank line.
        assert!(out.ends_with("PrivateKey = AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAEA=\n"));
    }

    #[test]
    fn no_hooks_field_can_be_rendered() {
        // §3.2 freezes PostUp / PostDown out of the schema. The value type
        // has no field for them; the renderer cannot emit them. This test
        // documents the invariant as a compile-time + runtime check —
        // grep the output for the forbidden strings.
        let iface = WgInterface {
            name: "wg0".into(),
            listen_port: 51820,
            private_key: zero_priv(),
            addresses: vec!["192.0.2.1/24".into()],
            mtu: None,
        };
        let out = render_interface_conf(&iface, &[]).unwrap();
        assert!(!out.contains("PostUp"));
        assert!(!out.contains("PostDown"));
    }

    #[test]
    fn rejects_newline_in_address() {
        let iface = WgInterface {
            name: "wg0".into(),
            listen_port: 51820,
            private_key: zero_priv(),
            addresses: vec!["192.0.2.1/24\n[Peer]\nPublicKey = injected".into()],
            mtu: None,
        };
        match render_interface_conf(&iface, &[]) {
            Err(RenderError::ControlCharacter { field }) => assert_eq!(field, "addresses"),
            other => panic!("expected ControlCharacter, got {other:?}"),
        }
    }

    #[test]
    fn rejects_newline_in_allowed_ips() {
        let iface = WgInterface {
            name: "wg0".into(),
            listen_port: 51820,
            private_key: zero_priv(),
            addresses: vec!["192.0.2.1/24".into()],
            mtu: None,
        };
        let peer = WgPeer {
            public_key: zero_pub(),
            preshared_key: None,
            // Classic config-format injection: append a forged [Peer] block.
            allowed_ips: vec!["192.0.2.5/32\n[Peer]\nPublicKey = aaa".into()],
            endpoint: None,
            persistent_keepalive: None,
        };
        match render_interface_conf(&iface, std::slice::from_ref(&peer)) {
            Err(RenderError::ControlCharacter { field }) => assert_eq!(field, "allowed_ips"),
            other => panic!("expected ControlCharacter, got {other:?}"),
        }
    }

    #[test]
    fn rejects_carriage_return_in_endpoint() {
        let iface = WgInterface {
            name: "wg0".into(),
            listen_port: 51820,
            private_key: zero_priv(),
            addresses: vec!["192.0.2.1/24".into()],
            mtu: None,
        };
        let peer = WgPeer {
            public_key: zero_pub(),
            preshared_key: None,
            allowed_ips: vec!["192.0.2.5/32".into()],
            endpoint: Some("vpn.example:51820\r\nAllowedIPs = 0.0.0.0/0".into()),
            persistent_keepalive: None,
        };
        match render_interface_conf(&iface, std::slice::from_ref(&peer)) {
            Err(RenderError::ControlCharacter { field }) => assert_eq!(field, "endpoint"),
            other => panic!("expected ControlCharacter, got {other:?}"),
        }
    }

    #[test]
    fn rejects_nul_byte_in_address() {
        let iface = WgInterface {
            name: "wg0".into(),
            listen_port: 51820,
            private_key: zero_priv(),
            addresses: vec!["192.0.2.1/24\0".into()],
            mtu: None,
        };
        match render_interface_conf(&iface, &[]) {
            Err(RenderError::ControlCharacter { field }) => assert_eq!(field, "addresses"),
            other => panic!("expected ControlCharacter, got {other:?}"),
        }
    }

    #[test]
    fn ordering_matches_wg_admin_for_full_config() {
        // Cross-reference shape: wg-admin's WireGuardService::generateServerConfig
        // emits Address, ListenPort, PrivateKey, (MTU), then per peer:
        // PublicKey, (PresharedKey), AllowedIPs, (Endpoint),
        // (PersistentKeepalive). This is the exact ordering we assert here
        // so deployments swapping wg-admin for cosmix-wgd see byte-identical
        // output for the same logical config.
        let iface = WgInterface {
            name: "wg0".into(),
            listen_port: 51820,
            private_key: zero_priv(),
            addresses: vec!["192.0.2.1/24".into()],
            mtu: Some(1420),
        };
        let peer = WgPeer {
            public_key: zero_pub(),
            preshared_key: Some(zero_psk()),
            allowed_ips: vec!["192.0.2.5/32".into()],
            endpoint: Some("vpn.example:51820".into()),
            persistent_keepalive: Some(25),
        };
        let out = render_interface_conf(&iface, std::slice::from_ref(&peer)).unwrap();
        // Walk every required substring left-to-right; if any is out of
        // order, the rfind→find slicing will catch it.
        let mut cursor = 0;
        for needle in [
            "[Interface]",
            "Address = ",
            "ListenPort = ",
            "PrivateKey = ",
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
    fn debug_redaction_holds_on_value_types() {
        // The struct holds a WgPrivateKey, whose Debug is redacted; this
        // test guards against an accidental #[derive(Debug)] on
        // WgInterface that would defeat that redaction by printing fields.
        // (As of C3 we do not derive Debug on the value types for exactly
        // this reason. If a future commit adds it, the field must use the
        // redacted Debug impl from keys.rs — which it does, transitively.)
        let iface = WgInterface {
            name: "wg0".into(),
            listen_port: 51820,
            private_key: zero_priv(),
            addresses: vec![],
            mtu: None,
        };
        // Confirm the private key's own Debug is still redacted, which is
        // the property the (non-derived) WgInterface Debug would inherit.
        assert_eq!(format!("{:?}", iface.private_key), "WgPrivateKey(***)");
    }
}
