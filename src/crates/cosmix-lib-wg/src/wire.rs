//! Pure-wire control-plane messages: `WG_GENL` (the kernel WireGuard
//! generic-netlink interface) plus rtnetlink link/address operations,
//! built as typed values that emit byte buffers.
//!
//! No sockets, no syscalls. The citizen-side reconciler owns the
//! `AF_NETLINK` file descriptor, the family-id resolution
//! (`CTRL_CMD_GETFAMILY` against `nlctrl`), and the request/response
//! sequencing. This module hands it ready-to-serialize messages.
//!
//! The test surface round-trips each message through `Emitable::emit` →
//! bytes → the corresponding `Parseable` impl, then structurally
//! compares. That gives confidence the byte format matches what the
//! kernel will see, without ever opening a socket — the tests run
//! anywhere, including CI containers that have no `wireguard` kernel
//! module.
//!
//! Secret-handling caveat — the underlying `netlink-packet-wireguard`
//! crate stores private-key and PSK bytes in plain `[u8; 32]` enum
//! payloads that do NOT zeroize on drop. We accept our zeroising key
//! types as inputs and only materialise the plain array inside the
//! returned [`GenlMessage`]; the byte buffer eventually fed to the
//! socket is the unavoidable copy. Callers MUST drop the returned
//! message as soon as it has been serialised — do not stash it in a
//! long-lived collection.

use std::net::{IpAddr, SocketAddr};

use netlink_packet_generic::GenlMessage;
use netlink_packet_route::{
    AddressFamily, RouteNetlinkMessage,
    address::{AddressAttribute, AddressMessage},
    link::{InfoKind, LinkAttribute, LinkFlags, LinkInfo, LinkMessage},
};
use netlink_packet_wireguard::{
    WireguardAddressFamily, WireguardAllowedIp, WireguardAllowedIpAttr, WireguardAttribute,
    WireguardCmd, WireguardDeviceFlags, WireguardMessage, WireguardPeer, WireguardPeerAttribute,
    WireguardPeerFlags,
};

use crate::ipam::Cidr;
use crate::keys::{WgPresharedKey, WgPrivateKey, WgPublicKey};

/// Errors returned by the fallible wire builders. These trip when the
/// caller hands us a value-typed struct whose public fields violate an
/// invariant our constructors normally enforce — defence-in-depth for
/// the "core crate accepts external callers" boundary, mirroring the
/// validation [`crate::ipam::next_free_host`] does on a hand-built
/// [`Cidr`].
#[derive(Debug, thiserror::Error)]
pub enum WireError {
    /// `SetPeer { remove: true, .. }` together with any other field
    /// the kernel would silently ignore (PSK, endpoint, keepalive,
    /// allowed_ips, replace_allowed_ips, update_only). Emitting these
    /// is wasted bytes at best and a caller-bug signal at worst.
    #[error(
        "SetPeer.remove cannot be combined with preshared_key, endpoint, persistent_keepalive, \
         allowed_ips, replace_allowed_ips, or update_only"
    )]
    RemovePeerWithExtraFields,
    /// `Cidr.prefix_len` exceeds the maximum for the family
    /// (32 for v4, 128 for v6). Constructors in [`crate::ipam`]
    /// catch this; only hand-built `Cidr` values reach here.
    #[error("CIDR prefix {prefix} too large for {family}")]
    CidrPrefixTooLarge { family: &'static str, prefix: u8 },
    /// Address-family / prefix mismatch when building an `RTM_NEWADDR`
    /// or `RTM_DELADDR`: e.g. an IPv4 address with prefix 64.
    #[error("address prefix {prefix} too large for {family}")]
    AddrPrefixTooLarge { family: &'static str, prefix: u8 },
}

/// Which device a `WG_GENL` message targets. WireGuard accepts either
/// the interface index (preferred once the citizen has resolved one
/// via rtnetlink) or the interface name (used during initial creation
/// before an index is known).
#[derive(Debug, Clone)]
pub enum WgIfaceSel {
    Name(String),
    Index(u32),
}

/// Per-peer payload of a [`wg_set_device_message`]. Mirrors the
/// `wgpeer_*` attribute group; the boolean flags map to
/// `WGPEER_F_*`.
#[derive(Debug, Clone)]
pub struct SetPeer {
    pub public_key: WgPublicKey,
    pub preshared_key: Option<WgPresharedKey>,
    pub endpoint: Option<SocketAddr>,
    pub persistent_keepalive: Option<u16>,
    pub allowed_ips: Vec<Cidr>,
    /// Drop the previous AllowedIPs set for this peer before applying
    /// the new one. Without this flag, the kernel unions the sets.
    pub replace_allowed_ips: bool,
    /// Only modify an existing peer; the kernel will reject the message
    /// if the peer is unknown. Used by reconcile-update paths that
    /// must not silently re-add a peer the operator just removed.
    pub update_only: bool,
    /// Delete this peer from the device entirely. Mutually exclusive
    /// with `update_only` and the AllowedIPs fields at the kernel
    /// boundary; we surface them as independent booleans for caller
    /// ergonomics and trust the kernel's validation.
    pub remove: bool,
}

/// Payload of a `WG_CMD_SET_DEVICE` message. Optional fields are not
/// emitted as attributes when absent, so the kernel preserves whatever
/// the previous setting was — exactly the same partial-update
/// semantics `wg set <iface> private-key …` uses.
#[derive(Debug, Clone)]
pub struct SetDeviceParams {
    pub iface: WgIfaceSel,
    pub private_key: Option<WgPrivateKey>,
    pub listen_port: Option<u16>,
    pub fwmark: Option<u32>,
    /// Drop the previous peer set entirely before applying the new
    /// one. Used during full reconciliation; incremental updates
    /// leave this `false`.
    pub replace_peers: bool,
    pub peers: Vec<SetPeer>,
}

/// Build a `WG_CMD_GET_DEVICE` query — the message the citizen sends
/// to read back what the kernel currently has.
pub fn wg_get_device_message(iface: &WgIfaceSel) -> GenlMessage<WireguardMessage> {
    let attributes = vec![iface_attribute(iface)];
    GenlMessage::from_payload(WireguardMessage {
        cmd: WireguardCmd::GetDevice,
        attributes,
    })
}

/// Build a `WG_CMD_SET_DEVICE` mutation. The returned [`GenlMessage`]
/// carries plain key bytes; serialise + drop promptly.
///
/// Returns [`WireError`] if any peer has `remove=true` set together
/// with other mutating fields (kernel would ignore them — surface
/// eagerly), or if a peer's [`Cidr`] has an out-of-range prefix.
pub fn wg_set_device_message(
    p: &SetDeviceParams,
) -> Result<GenlMessage<WireguardMessage>, WireError> {
    let mut attributes = Vec::with_capacity(6 + p.peers.len());
    attributes.push(iface_attribute(&p.iface));

    if let Some(pk) = &p.private_key {
        attributes.push(WireguardAttribute::PrivateKey(*pk.as_bytes()));
    }
    if let Some(port) = p.listen_port {
        attributes.push(WireguardAttribute::ListenPort(port));
    }
    if let Some(fwmark) = p.fwmark {
        attributes.push(WireguardAttribute::Fwmark(fwmark));
    }
    if p.replace_peers {
        attributes.push(WireguardAttribute::Flags(
            WireguardDeviceFlags::ReplacePeers,
        ));
    }
    if !p.peers.is_empty() {
        let peers: Vec<WireguardPeer> = p.peers.iter().map(build_peer).collect::<Result<_, _>>()?;
        attributes.push(WireguardAttribute::Peers(peers));
    }

    Ok(GenlMessage::from_payload(WireguardMessage {
        cmd: WireguardCmd::SetDevice,
        attributes,
    }))
}

fn iface_attribute(iface: &WgIfaceSel) -> WireguardAttribute {
    match iface {
        WgIfaceSel::Name(n) => WireguardAttribute::IfName(n.clone()),
        WgIfaceSel::Index(i) => WireguardAttribute::IfIndex(*i),
    }
}

fn build_peer(p: &SetPeer) -> Result<WireguardPeer, WireError> {
    if p.remove
        && (p.preshared_key.is_some()
            || p.endpoint.is_some()
            || p.persistent_keepalive.is_some()
            || !p.allowed_ips.is_empty()
            || p.replace_allowed_ips
            || p.update_only)
    {
        return Err(WireError::RemovePeerWithExtraFields);
    }

    let mut attrs: Vec<WireguardPeerAttribute> = Vec::with_capacity(8);
    attrs.push(WireguardPeerAttribute::PublicKey(*p.public_key.as_bytes()));

    if let Some(psk) = &p.preshared_key {
        attrs.push(WireguardPeerAttribute::PresharedKey(*psk.as_bytes()));
    }
    if let Some(ep) = p.endpoint {
        attrs.push(WireguardPeerAttribute::Endpoint(ep));
    }
    if let Some(ka) = p.persistent_keepalive {
        attrs.push(WireguardPeerAttribute::PersistentKeepalive(ka));
    }

    let mut peer_flags = WireguardPeerFlags::empty();
    if p.remove {
        peer_flags |= WireguardPeerFlags::RemoveMe;
    }
    if p.replace_allowed_ips {
        peer_flags |= WireguardPeerFlags::ReplaceAllowedIps;
    }
    if p.update_only {
        peer_flags |= WireguardPeerFlags::UpdateOnly;
    }
    if !peer_flags.is_empty() {
        attrs.push(WireguardPeerAttribute::Flags(peer_flags));
    }

    if !p.allowed_ips.is_empty() {
        let allowed: Vec<WireguardAllowedIp> = p
            .allowed_ips
            .iter()
            .map(build_allowed_ip)
            .collect::<Result<_, _>>()?;
        attrs.push(WireguardPeerAttribute::AllowedIps(allowed));
    }

    Ok(WireguardPeer(attrs))
}

fn build_allowed_ip(c: &Cidr) -> Result<WireguardAllowedIp, WireError> {
    let (family, max_prefix, family_name) = match c.network {
        IpAddr::V4(_) => (WireguardAddressFamily::Ipv4, 32, "IPv4"),
        IpAddr::V6(_) => (WireguardAddressFamily::Ipv6, 128, "IPv6"),
    };
    if c.prefix_len > max_prefix {
        return Err(WireError::CidrPrefixTooLarge {
            family: family_name,
            prefix: c.prefix_len,
        });
    }
    Ok(WireguardAllowedIp(vec![
        WireguardAllowedIpAttr::Family(family),
        WireguardAllowedIpAttr::IpAddr(c.network),
        WireguardAllowedIpAttr::Cidr(c.prefix_len),
    ]))
}

/// Build `RTM_NEWLINK` to create a fresh WireGuard interface by name.
/// The kernel allocates the index; the caller reads it back from the
/// `RTM_NEWLINK` echo or a subsequent `RTM_GETLINK`.
pub fn rtnl_new_link_wireguard(iface_name: &str) -> RouteNetlinkMessage {
    let mut msg = LinkMessage::default();
    msg.attributes = vec![
        LinkAttribute::IfName(iface_name.to_string()),
        LinkAttribute::LinkInfo(vec![LinkInfo::Kind(InfoKind::Wireguard)]),
    ];
    RouteNetlinkMessage::NewLink(msg)
}

/// Build `RTM_DELLINK` for the given interface index. Removes the
/// WireGuard interface and tears down all peer state with it.
pub fn rtnl_del_link(iface_index: u32) -> RouteNetlinkMessage {
    let mut msg = LinkMessage::default();
    msg.header.index = iface_index;
    RouteNetlinkMessage::DelLink(msg)
}

/// Build `RTM_SETLINK` that brings the link administratively up
/// (`IFF_UP`).
pub fn rtnl_set_link_up(iface_index: u32) -> RouteNetlinkMessage {
    rtnl_set_link_admin_state(iface_index, true)
}

/// Build `RTM_SETLINK` that brings the link administratively down.
pub fn rtnl_set_link_down(iface_index: u32) -> RouteNetlinkMessage {
    rtnl_set_link_admin_state(iface_index, false)
}

fn rtnl_set_link_admin_state(iface_index: u32, up: bool) -> RouteNetlinkMessage {
    let mut msg = LinkMessage::default();
    msg.header.index = iface_index;
    // change_mask tells the kernel which IFF_* bits we are touching;
    // flags carries the desired value for the bits in change_mask. We
    // only ever flip IFF_UP from this helper.
    msg.header.change_mask = LinkFlags::Up;
    msg.header.flags = if up {
        LinkFlags::Up
    } else {
        LinkFlags::empty()
    };
    RouteNetlinkMessage::SetLink(msg)
}

/// Build `RTM_NEWADDR` to add an IP/prefix to the given interface
/// index. The scope is set to `universe` (global); WireGuard mesh
/// addresses are routable across the tunnel.
///
/// Returns [`WireError::AddrPrefixTooLarge`] if `prefix_len` exceeds
/// the maximum for the family (32 for v4, 128 for v6).
pub fn rtnl_new_address(
    iface_index: u32,
    addr: IpAddr,
    prefix_len: u8,
) -> Result<RouteNetlinkMessage, WireError> {
    Ok(RouteNetlinkMessage::NewAddress(build_addr_message(
        iface_index,
        addr,
        prefix_len,
    )?))
}

/// Build `RTM_DELADDR` removing an IP/prefix from the given interface
/// index. Same validation as [`rtnl_new_address`].
pub fn rtnl_del_address(
    iface_index: u32,
    addr: IpAddr,
    prefix_len: u8,
) -> Result<RouteNetlinkMessage, WireError> {
    Ok(RouteNetlinkMessage::DelAddress(build_addr_message(
        iface_index,
        addr,
        prefix_len,
    )?))
}

fn build_addr_message(
    iface_index: u32,
    addr: IpAddr,
    prefix_len: u8,
) -> Result<AddressMessage, WireError> {
    let (family, max_prefix, family_name) = match addr {
        IpAddr::V4(_) => (AddressFamily::Inet, 32u8, "IPv4"),
        IpAddr::V6(_) => (AddressFamily::Inet6, 128u8, "IPv6"),
    };
    if prefix_len > max_prefix {
        return Err(WireError::AddrPrefixTooLarge {
            family: family_name,
            prefix: prefix_len,
        });
    }
    let mut msg = AddressMessage::default();
    msg.header.family = family;
    msg.header.prefix_len = prefix_len;
    msg.header.index = iface_index;
    // Address + Local are both set to the same value for unicast
    // addresses; the kernel uses `Local` as the "source" half of the
    // pair (only differs for IPv4 point-to-point links).
    msg.attributes = vec![
        AddressAttribute::Address(addr),
        AddressAttribute::Local(addr),
    ];
    Ok(msg)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ipam::parse_cidr;
    use netlink_packet_core::{Emitable, Parseable, ParseableParametrized};
    use netlink_packet_generic::GenlHeader;
    use netlink_packet_route::address::AddressMessageBuffer;
    use netlink_packet_route::link::LinkMessageBuffer;
    use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr};

    // ── helpers ────────────────────────────────────────────────────

    fn deterministic_keypair(seed: u8) -> crate::keys::WgKeyPair {
        crate::keys::WgKeyPair::from_private_bytes([seed; 32])
    }

    fn roundtrip_wg(msg: &WireguardMessage) -> WireguardMessage {
        let mut buf = vec![0u8; msg.buffer_len()];
        msg.emit(&mut buf);
        let header = GenlHeader {
            cmd: msg.cmd.into(),
            version: 1,
        };
        WireguardMessage::parse_with_param(&buf, header).expect("wireguard payload round-trip")
    }

    fn roundtrip_link(msg: &LinkMessage) -> LinkMessage {
        let mut buf = vec![0u8; msg.buffer_len()];
        msg.emit(&mut buf);
        let buffer = LinkMessageBuffer::new_checked(&buf).expect("link buf checked");
        LinkMessage::parse(&buffer).expect("link round-trip")
    }

    fn roundtrip_addr(msg: &AddressMessage) -> AddressMessage {
        let mut buf = vec![0u8; msg.buffer_len()];
        msg.emit(&mut buf);
        let buffer = AddressMessageBuffer::new_checked(&buf).expect("addr buf checked");
        AddressMessage::parse(&buffer).expect("addr round-trip")
    }

    // ── wg_get_device_message ──────────────────────────────────────

    #[test]
    fn get_device_by_name_round_trips() {
        let msg = wg_get_device_message(&WgIfaceSel::Name("wg0".to_string()));
        assert_eq!(msg.payload.cmd, WireguardCmd::GetDevice);
        let back = roundtrip_wg(&msg.payload);
        assert_eq!(back.cmd, WireguardCmd::GetDevice);
        assert!(
            back.attributes
                .iter()
                .any(|a| matches!(a, WireguardAttribute::IfName(n) if n == "wg0"))
        );
    }

    #[test]
    fn get_device_by_index_round_trips() {
        let msg = wg_get_device_message(&WgIfaceSel::Index(7));
        let back = roundtrip_wg(&msg.payload);
        assert!(
            back.attributes
                .iter()
                .any(|a| matches!(a, WireguardAttribute::IfIndex(7)))
        );
    }

    // ── wg_set_device_message ──────────────────────────────────────

    #[test]
    fn set_device_minimal_round_trips() {
        let params = SetDeviceParams {
            iface: WgIfaceSel::Name("wg-mesh".to_string()),
            private_key: None,
            listen_port: None,
            fwmark: None,
            replace_peers: false,
            peers: vec![],
        };
        let msg = wg_set_device_message(&params).unwrap();
        assert_eq!(msg.payload.cmd, WireguardCmd::SetDevice);
        let back = roundtrip_wg(&msg.payload);
        assert_eq!(back.cmd, WireguardCmd::SetDevice);
        // Only the IfName attribute when everything else is absent.
        assert_eq!(back.attributes.len(), 1);
    }

    #[test]
    fn set_device_full_round_trips() {
        let kp = deterministic_keypair(0xA1);
        let peer_kp = deterministic_keypair(0xB2);
        let psk = crate::keys::WgPresharedKey::generate();

        let params = SetDeviceParams {
            iface: WgIfaceSel::Index(42),
            private_key: Some(kp.private.clone()),
            listen_port: Some(51820),
            fwmark: Some(0xCAFE),
            replace_peers: true,
            peers: vec![SetPeer {
                public_key: peer_kp.public,
                preshared_key: Some(psk),
                endpoint: Some(SocketAddr::from(([10, 0, 0, 1], 51820))),
                persistent_keepalive: Some(25),
                allowed_ips: vec![
                    parse_cidr("10.0.0.0/24").unwrap(),
                    parse_cidr("fd00::/64").unwrap(),
                ],
                replace_allowed_ips: true,
                update_only: false,
                remove: false,
            }],
        };

        let msg = wg_set_device_message(&params).unwrap();
        let back = roundtrip_wg(&msg.payload);

        // Top-level attributes survive.
        assert!(
            back.attributes
                .iter()
                .any(|a| matches!(a, WireguardAttribute::IfIndex(42)))
        );
        assert!(
            back.attributes.iter().any(
                |a| matches!(a, WireguardAttribute::PrivateKey(b) if b == kp.private.as_bytes())
            )
        );
        assert!(
            back.attributes
                .iter()
                .any(|a| matches!(a, WireguardAttribute::ListenPort(51820)))
        );
        assert!(
            back.attributes
                .iter()
                .any(|a| matches!(a, WireguardAttribute::Fwmark(0xCAFE)))
        );
        assert!(back.attributes.iter().any(|a| matches!(
            a,
            WireguardAttribute::Flags(f) if f.contains(WireguardDeviceFlags::ReplacePeers)
        )));

        // Peer round-trip.
        let peers = back
            .attributes
            .iter()
            .find_map(|a| match a {
                WireguardAttribute::Peers(p) => Some(p),
                _ => None,
            })
            .expect("Peers attr present");
        assert_eq!(peers.len(), 1);
        let p = &peers[0];
        assert!(p.0.iter().any(|pa| matches!(
            pa,
            WireguardPeerAttribute::PublicKey(b) if b == peer_kp.public.as_bytes()
        )));
        assert!(
            p.0.iter()
                .any(|pa| matches!(pa, WireguardPeerAttribute::Endpoint(_)))
        );
        assert!(
            p.0.iter()
                .any(|pa| matches!(pa, WireguardPeerAttribute::PersistentKeepalive(25)))
        );
        assert!(p.0.iter().any(|pa| matches!(
            pa,
            WireguardPeerAttribute::Flags(f) if f.contains(WireguardPeerFlags::ReplaceAllowedIps)
        )));

        // AllowedIps preserves both families with correct prefix lengths.
        let allowed =
            p.0.iter()
                .find_map(|pa| match pa {
                    WireguardPeerAttribute::AllowedIps(v) => Some(v),
                    _ => None,
                })
                .expect("AllowedIps present");
        assert_eq!(allowed.len(), 2);
        let v4 = allowed.iter().find(|a| {
            a.0.iter()
                .any(|attr| matches!(attr, WireguardAllowedIpAttr::IpAddr(IpAddr::V4(_))))
        });
        let v6 = allowed.iter().find(|a| {
            a.0.iter()
                .any(|attr| matches!(attr, WireguardAllowedIpAttr::IpAddr(IpAddr::V6(_))))
        });
        assert!(v4.is_some() && v6.is_some());
        assert!(
            v4.unwrap()
                .0
                .iter()
                .any(|attr| matches!(attr, WireguardAllowedIpAttr::Cidr(24)))
        );
        assert!(
            v6.unwrap()
                .0
                .iter()
                .any(|attr| matches!(attr, WireguardAllowedIpAttr::Cidr(64)))
        );
    }

    #[test]
    fn set_device_remove_peer_emits_flag() {
        let kp = deterministic_keypair(0x77);
        let params = SetDeviceParams {
            iface: WgIfaceSel::Name("wg0".to_string()),
            private_key: None,
            listen_port: None,
            fwmark: None,
            replace_peers: false,
            peers: vec![SetPeer {
                public_key: kp.public,
                preshared_key: None,
                endpoint: None,
                persistent_keepalive: None,
                allowed_ips: vec![],
                replace_allowed_ips: false,
                update_only: false,
                remove: true,
            }],
        };
        let msg = wg_set_device_message(&params).unwrap();
        let back = roundtrip_wg(&msg.payload);
        let peers = back
            .attributes
            .iter()
            .find_map(|a| match a {
                WireguardAttribute::Peers(p) => Some(p),
                _ => None,
            })
            .expect("Peers attr");
        assert!(peers[0].0.iter().any(|pa| matches!(
            pa,
            WireguardPeerAttribute::Flags(f) if f.contains(WireguardPeerFlags::RemoveMe)
        )));
    }

    #[test]
    fn set_device_update_only_emits_flag() {
        let kp = deterministic_keypair(0x55);
        let params = SetDeviceParams {
            iface: WgIfaceSel::Name("wg0".to_string()),
            private_key: None,
            listen_port: None,
            fwmark: None,
            replace_peers: false,
            peers: vec![SetPeer {
                public_key: kp.public,
                preshared_key: None,
                endpoint: None,
                persistent_keepalive: Some(15),
                allowed_ips: vec![],
                replace_allowed_ips: false,
                update_only: true,
                remove: false,
            }],
        };
        let msg = wg_set_device_message(&params).unwrap();
        let back = roundtrip_wg(&msg.payload);
        let peers = back
            .attributes
            .iter()
            .find_map(|a| match a {
                WireguardAttribute::Peers(p) => Some(p),
                _ => None,
            })
            .expect("Peers attr");
        assert!(peers[0].0.iter().any(|pa| matches!(
            pa,
            WireguardPeerAttribute::Flags(f) if f.contains(WireguardPeerFlags::UpdateOnly)
        )));
    }

    // ── rtnl link helpers ──────────────────────────────────────────

    #[test]
    fn new_link_wireguard_carries_kind() {
        let m = rtnl_new_link_wireguard("wg-mesh");
        let RouteNetlinkMessage::NewLink(inner) = m else {
            panic!("expected NewLink variant");
        };
        let back = roundtrip_link(&inner);
        assert!(
            back.attributes
                .iter()
                .any(|a| matches!(a, LinkAttribute::IfName(n) if n == "wg-mesh"))
        );
        let info = back
            .attributes
            .iter()
            .find_map(|a| match a {
                LinkAttribute::LinkInfo(v) => Some(v),
                _ => None,
            })
            .expect("LinkInfo present");
        assert!(
            info.iter()
                .any(|li| matches!(li, LinkInfo::Kind(InfoKind::Wireguard)))
        );
    }

    #[test]
    fn del_link_targets_index() {
        let m = rtnl_del_link(11);
        let RouteNetlinkMessage::DelLink(inner) = m else {
            panic!("expected DelLink variant");
        };
        let back = roundtrip_link(&inner);
        assert_eq!(back.header.index, 11);
    }

    #[test]
    fn set_link_up_sets_iff_up_in_both_fields() {
        let m = rtnl_set_link_up(5);
        let RouteNetlinkMessage::SetLink(inner) = m else {
            panic!("expected SetLink variant");
        };
        let back = roundtrip_link(&inner);
        assert_eq!(back.header.index, 5);
        assert!(back.header.change_mask.contains(LinkFlags::Up));
        assert!(back.header.flags.contains(LinkFlags::Up));
    }

    #[test]
    fn set_link_down_clears_iff_up_in_flags_but_keeps_mask() {
        let m = rtnl_set_link_down(5);
        let RouteNetlinkMessage::SetLink(inner) = m else {
            panic!("expected SetLink variant");
        };
        let back = roundtrip_link(&inner);
        assert!(back.header.change_mask.contains(LinkFlags::Up));
        assert!(!back.header.flags.contains(LinkFlags::Up));
    }

    // ── rtnl address helpers ───────────────────────────────────────

    #[test]
    fn new_address_v4_round_trips() {
        let addr = IpAddr::V4(Ipv4Addr::new(192, 0, 2, 5));
        let m = rtnl_new_address(7, addr, 24).unwrap();
        let RouteNetlinkMessage::NewAddress(inner) = m else {
            panic!("expected NewAddress variant");
        };
        let back = roundtrip_addr(&inner);
        assert_eq!(back.header.family, AddressFamily::Inet);
        assert_eq!(back.header.prefix_len, 24);
        assert_eq!(back.header.index, 7);
        assert!(back
            .attributes
            .iter()
            .any(|a| matches!(a, AddressAttribute::Address(IpAddr::V4(v)) if *v == Ipv4Addr::new(192, 0, 2, 5))));
        assert!(back
            .attributes
            .iter()
            .any(|a| matches!(a, AddressAttribute::Local(IpAddr::V4(v)) if *v == Ipv4Addr::new(192, 0, 2, 5))));
    }

    #[test]
    fn new_address_v6_round_trips() {
        let addr = IpAddr::V6(Ipv6Addr::new(0xfd00, 0, 0, 0, 0, 0, 0, 1));
        let m = rtnl_new_address(9, addr, 64).unwrap();
        let RouteNetlinkMessage::NewAddress(inner) = m else {
            panic!("expected NewAddress variant");
        };
        let back = roundtrip_addr(&inner);
        assert_eq!(back.header.family, AddressFamily::Inet6);
        assert_eq!(back.header.prefix_len, 64);
        assert!(
            back.attributes
                .iter()
                .any(|a| matches!(a, AddressAttribute::Address(IpAddr::V6(_))))
        );
    }

    #[test]
    fn del_address_v4_round_trips() {
        let addr = IpAddr::V4(Ipv4Addr::new(10, 0, 0, 7));
        let m = rtnl_del_address(3, addr, 32).unwrap();
        let RouteNetlinkMessage::DelAddress(inner) = m else {
            panic!("expected DelAddress variant");
        };
        let back = roundtrip_addr(&inner);
        assert_eq!(back.header.family, AddressFamily::Inet);
        assert_eq!(back.header.prefix_len, 32);
        assert_eq!(back.header.index, 3);
    }

    // ── secret-handling structural guard ───────────────────────────

    #[test]
    fn set_device_omits_private_key_attr_when_none() {
        // If the caller passes private_key: None, we MUST NOT emit a
        // PrivateKey attribute — that would otherwise zero-overwrite
        // whatever the kernel currently has.
        let params = SetDeviceParams {
            iface: WgIfaceSel::Name("wg0".to_string()),
            private_key: None,
            listen_port: Some(51820),
            fwmark: None,
            replace_peers: false,
            peers: vec![],
        };
        let msg = wg_set_device_message(&params).unwrap();
        assert!(
            !msg.payload
                .attributes
                .iter()
                .any(|a| matches!(a, WireguardAttribute::PrivateKey(_)))
        );
    }

    #[test]
    fn set_device_emits_private_key_attr_when_some() {
        let kp = deterministic_keypair(0x33);
        let params = SetDeviceParams {
            iface: WgIfaceSel::Name("wg0".to_string()),
            private_key: Some(kp.private.clone()),
            listen_port: None,
            fwmark: None,
            replace_peers: false,
            peers: vec![],
        };
        let msg = wg_set_device_message(&params).unwrap();
        let bytes_match =
            msg.payload.attributes.iter().any(
                |a| matches!(a, WireguardAttribute::PrivateKey(b) if b == kp.private.as_bytes()),
            );
        assert!(bytes_match);
    }

    // ── error paths (defence-in-depth on public value-typed inputs) ─

    #[test]
    fn set_device_rejects_remove_combined_with_other_fields() {
        let kp = deterministic_keypair(0x44);
        let params = SetDeviceParams {
            iface: WgIfaceSel::Name("wg0".to_string()),
            private_key: None,
            listen_port: None,
            fwmark: None,
            replace_peers: false,
            peers: vec![SetPeer {
                public_key: kp.public,
                preshared_key: None,
                endpoint: None,
                persistent_keepalive: Some(25),
                allowed_ips: vec![],
                replace_allowed_ips: false,
                update_only: false,
                remove: true,
            }],
        };
        let err = wg_set_device_message(&params).unwrap_err();
        assert!(matches!(err, WireError::RemovePeerWithExtraFields));
    }

    #[test]
    fn set_device_rejects_overlong_v4_cidr() {
        let kp = deterministic_keypair(0x66);
        // Hand-built Cidr bypasses parse_cidr's validation.
        let bogus = Cidr {
            network: IpAddr::V4(Ipv4Addr::new(10, 0, 0, 0)),
            prefix_len: 33,
        };
        let params = SetDeviceParams {
            iface: WgIfaceSel::Name("wg0".to_string()),
            private_key: None,
            listen_port: None,
            fwmark: None,
            replace_peers: false,
            peers: vec![SetPeer {
                public_key: kp.public,
                preshared_key: None,
                endpoint: None,
                persistent_keepalive: None,
                allowed_ips: vec![bogus],
                replace_allowed_ips: false,
                update_only: false,
                remove: false,
            }],
        };
        let err = wg_set_device_message(&params).unwrap_err();
        assert!(matches!(
            err,
            WireError::CidrPrefixTooLarge {
                family: "IPv4",
                prefix: 33
            }
        ));
    }

    #[test]
    fn rtnl_new_address_rejects_v4_with_v6_prefix() {
        let addr = IpAddr::V4(Ipv4Addr::new(10, 0, 0, 1));
        let err = rtnl_new_address(7, addr, 128).unwrap_err();
        assert!(matches!(
            err,
            WireError::AddrPrefixTooLarge {
                family: "IPv4",
                prefix: 128
            }
        ));
    }
}
