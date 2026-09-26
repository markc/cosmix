//! Fatal startup refusals — the exact texts and the one way out.
//!
//! A refusal must reach the operator on both channels before the exit:
//! stderr via `eprintln!` (a binary run by hand — or the live gate
//! asserting the token — must see the reason without `journalctl`) and
//! journald via `tracing::error!`. The texts are the gate's tokens
//! ("is not this node's WG address" is matched verbatim): change
//! nothing here without changing the gate.

use std::net::SocketAddr;
use std::path::Path;

use tracing::error;

/// Report `message` on stderr and journald, then exit 2.
pub fn fatal(message: &str) -> ! {
    eprintln!("{message}");
    error!("{message}");
    std::process::exit(2);
}

/// The WG bind proof failed: `bind` is not this node's WG address.
pub fn not_wg(bind: SocketAddr, wg_ip: &str) -> String {
    let shown = if wg_ip.is_empty() { "<absent>" } else { wg_ip };
    format!(
        "lane_bind {bind} is not this node's WG address (wg_ip {shown:?}) — the lane serves only the mesh; refusing to start"
    )
}

/// Another instance holds the root's flock: one GC owner per root.
pub fn root_locked(path: &Path) -> String {
    format!(
        "another instance holds {}; one GC owner per root — exiting",
        path.display()
    )
}

/// The config file could not be read.
pub fn config_read(path: &Path, error: &std::io::Error) -> String {
    format!("read config {}: {error}", path.display())
}

/// The config file did not parse or validate.
pub fn config_parse(path: &Path, error: &str) -> String {
    format!("parse config {}: {error}", path.display())
}

/// The lane's TCP bind failed after the WG proof passed.
pub fn lane_bind_io(bind: SocketAddr, error: &std::io::Error) -> String {
    format!("bind byte lane {bind}: {error}")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn not_wg_keeps_the_gate_token() {
        assert_eq!(
            not_wg("127.0.0.1:4210".parse().unwrap(), "10.7.0.9"),
            "lane_bind 127.0.0.1:4210 is not this node's WG address (wg_ip \"10.7.0.9\") — the lane serves only the mesh; refusing to start"
        );
        // An absent wg_ip shows as the quoted <absent> token, not "".
        assert_eq!(
            not_wg("127.0.0.1:4210".parse().unwrap(), ""),
            "lane_bind 127.0.0.1:4210 is not this node's WG address (wg_ip \"<absent>\") — the lane serves only the mesh; refusing to start"
        );
    }

    #[test]
    fn root_locked_keeps_the_flock_text() {
        assert_eq!(
            root_locked(Path::new("/var/lib/cosmix/blobd")),
            "another instance holds /var/lib/cosmix/blobd; one GC owner per root — exiting"
        );
    }

    #[test]
    fn config_messages_keep_their_texts() {
        assert_eq!(
            config_read(Path::new("/etc/blobd.conf"), &std::io::Error::other("nope")),
            "read config /etc/blobd.conf: nope"
        );
        assert_eq!(
            config_parse(Path::new("/etc/blobd.conf"), "root: bad value"),
            "parse config /etc/blobd.conf: root: bad value"
        );
    }

    #[test]
    fn lane_bind_io_keeps_the_bind_text() {
        assert_eq!(
            lane_bind_io(
                "10.7.0.9:4210".parse().unwrap(),
                &std::io::Error::other("address in use"),
            ),
            "bind byte lane 10.7.0.9:4210: address in use"
        );
    }
}
