//! Pure-logic primitives for cosmix-wgd: WireGuard key material today;
//! interface / peer value types, config rendering, IPAM, `wg show` parsing,
//! and netlink message construction in subsequent commits.
//!
//! No daemon, no socket, no syscall beyond what the underlying CSPRNG needs
//! to seed itself. `cargo test --no-default-features` is the full test
//! surface — see `_doc/planned/cosmix-wgd.md` §1 for the layering rule
//! (citizen owns side effects; this crate owns logic).

pub mod client;
pub mod dump;
pub mod ipam;
pub mod keys;
pub mod mesh;
pub mod qr;
pub mod render;
pub mod wire;

pub use client::{WgClientInterface, WgClientPeer, render_peer_conf};
pub use dump::{
    CONNECTED_THRESHOLD_SECS, DumpError, PeerDump, PeerStatus, WgInterfaceDump, WgShowDump,
    parse_wg_show_dump,
};
pub use ipam::{Cidr, CidrError, next_free_host, parse_cidr};
pub use keys::{KEY_LEN, KeyError, WgKeyPair, WgPresharedKey, WgPrivateKey, WgPublicKey};
pub use mesh::{IFNAMSIZ_MINUS_NUL, IfaceNameError, iface_name_for_mesh};
pub use qr::{QrError, render_qr_svg};
pub use render::{RenderError, WgInterface, WgPeer, render_interface_conf};
pub use wire::{
    SetDeviceParams, SetPeer, WgIfaceSel, WireError, rtnl_del_address, rtnl_del_link,
    rtnl_new_address, rtnl_new_link_wireguard, rtnl_set_link_down, rtnl_set_link_up,
    wg_get_device_message, wg_set_device_message,
};
