//! Stable 5-hex node identifier derived from `/etc/machine-id`.
//!
//! See `_doc/planned/cosmix-wgd.md` §12 *Node ID derivation* (rev 5) for
//! the shape rationale and the discarded alternatives. The short version:
//! pure-digit DNS labels are legal (RFC 1123), 5 hex chars give 1 048 576
//! combinations, ~0.5% birthday-collision at 100 nodes and ~2% at 200 —
//! comfortably above Cosmix's credible single-mesh ceiling. Collisions
//! are resolved at the substrate layer by first-allocate-wins on
//! `wgd.peer.allocate`.
//!
//! The crate is intentionally tiny and has no Cosmix-internal dependencies
//! so `cargo test --no-default-features` is the full test surface.

use sha2::{Digest, Sha256};

/// Filesystem path read by [`node_id`].
pub const MACHINE_ID_PATH: &str = "/etc/machine-id";

/// Width of the derived identifier in hex characters.
pub const NODE_ID_HEX_LEN: usize = 5;

/// Errors that can arise while deriving a node ID.
#[derive(Debug, thiserror::Error)]
pub enum NodeIdError {
    /// `/etc/machine-id` is not present on this host (systemd not installed
    /// or the file was removed). Provisioning bug — the file is created at
    /// install time on every distro Cosmix supports.
    #[error("/etc/machine-id is missing; cannot derive a stable node_id")]
    MachineIdMissing,

    /// The file exists but is empty after trimming surrounding whitespace.
    /// Hashing an empty input still yields a deterministic value, but it
    /// would be the same value on every host with an empty machine-id —
    /// the opposite of what a stable per-host identifier is for.
    #[error("/etc/machine-id is empty after trim; cannot derive a stable node_id")]
    MachineIdEmpty,

    /// Any other I/O failure (permission denied, etc.).
    #[error("failed to read /etc/machine-id: {0}")]
    Io(#[from] std::io::Error),
}

/// Read `/etc/machine-id` and derive this host's 5-hex node identifier.
///
/// The output is the first 5 hex digits of `SHA-256(trim(machine_id))`.
/// Stable for the life of the machine-id (i.e. until the host is reimaged).
pub fn node_id() -> Result<String, NodeIdError> {
    let machine_id = match std::fs::read_to_string(MACHINE_ID_PATH) {
        Ok(s) => s,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            return Err(NodeIdError::MachineIdMissing);
        }
        Err(e) => return Err(NodeIdError::Io(e)),
    };
    node_id_from(&machine_id)
}

/// Pure, deterministic derivation: trim, SHA-256, hex-encode first 3 bytes,
/// truncate to 5 chars.
///
/// Exposed so callers with a non-default source of machine identity (test
/// fixtures, future host-provisioning tooling) can derive an ID without
/// touching the real `/etc/machine-id`.
pub fn node_id_from(machine_id: &str) -> Result<String, NodeIdError> {
    let trimmed = machine_id.trim();
    if trimmed.is_empty() {
        return Err(NodeIdError::MachineIdEmpty);
    }
    let mut hasher = Sha256::new();
    hasher.update(trimmed.as_bytes());
    let digest = hasher.finalize();
    let mut hex = hex::encode(&digest[..3]);
    hex.truncate(NODE_ID_HEX_LEN);
    Ok(hex)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn derives_5_hex_chars() {
        let id = node_id_from("221599567c2845db9488eaeeab2b6f5b").unwrap();
        assert_eq!(id.len(), NODE_ID_HEX_LEN);
        assert!(id.chars().all(|c| c.is_ascii_hexdigit()));
        assert!(id.chars().all(|c| !c.is_ascii_uppercase()));
    }

    #[test]
    fn deterministic_for_same_input() {
        let a = node_id_from("221599567c2845db9488eaeeab2b6f5b").unwrap();
        let b = node_id_from("221599567c2845db9488eaeeab2b6f5b").unwrap();
        assert_eq!(a, b);
    }

    #[test]
    fn trims_trailing_newline() {
        let with_nl = node_id_from("221599567c2845db9488eaeeab2b6f5b\n").unwrap();
        let without = node_id_from("221599567c2845db9488eaeeab2b6f5b").unwrap();
        assert_eq!(with_nl, without);
    }

    #[test]
    fn trims_surrounding_whitespace() {
        let padded = node_id_from("  221599567c2845db9488eaeeab2b6f5b  \r\n").unwrap();
        let bare = node_id_from("221599567c2845db9488eaeeab2b6f5b").unwrap();
        assert_eq!(padded, bare);
    }

    #[test]
    fn rejects_empty() {
        assert!(matches!(node_id_from(""), Err(NodeIdError::MachineIdEmpty),));
    }

    #[test]
    fn rejects_whitespace_only() {
        assert!(matches!(
            node_id_from("   \n\t  "),
            Err(NodeIdError::MachineIdEmpty),
        ));
    }

    #[test]
    fn different_inputs_usually_differ() {
        // Not strictly guaranteed (SHA-256 has 2^-20 collision prob over the
        // 5-hex prefix between any two given inputs), but the two fixtures
        // here were checked manually; this guards against an accidental
        // "just return a constant" implementation.
        let a = node_id_from("221599567c2845db9488eaeeab2b6f5b").unwrap();
        let b = node_id_from("d4a5e0f17f7041c1a0a7e76ccd6e1b9c").unwrap();
        assert_ne!(a, b);
    }

    #[test]
    fn matches_known_vector() {
        // Verified via:
        //   printf '221599567c2845db9488eaeeab2b6f5b' | sha256sum | head -c 5
        // → "44c02".
        let id = node_id_from("221599567c2845db9488eaeeab2b6f5b").unwrap();
        assert_eq!(id, "44c02");
    }
}
