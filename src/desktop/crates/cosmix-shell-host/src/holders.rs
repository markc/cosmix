//! Layer identities for the Bus holder plane. The token is also the layer-shell
//! namespace, so comp resolves the same identity without a client-local wl id.
use bevy::prelude::*;
use cosmix_shell::core::{Edge, OutputKey};
use rustix::time::{ClockId, clock_gettime};
use std::sync::atomic::{AtomicU64, Ordering};

#[derive(Resource, Default)]
pub struct PanelLayerIdentities(pub Vec<(OutputKey, Edge, String)>);

/// The menu can exist while its parent panel has no Wayland layer at all.
#[derive(Resource)]
pub struct PopupLayerIdentity {
    pub output: OutputKey,
    pub edge: Edge,
    pub surface: String,
    /// The [`CornerMenuRequest`](cosmix_shell::chrome::corner_menu::CornerMenuRequest)
    /// serial this popup shows: whoever queued a step can tell it was theirs.
    pub serial: u64,
}

impl PanelLayerIdentities {
    pub fn get(&self, output: &OutputKey, edge: Edge) -> Option<&str> {
        self.0.iter().find(|(o, e, _)| o == output && *e == edge)
            .map(|(_, _, token)| token.as_str())
    }
}

/// A token no other layer of this boot shares and no other client can guess:
/// `prefix.<pid>.<ns>.<n>.<128 random bits in hex>`. The counter separates
/// layers of one process; pid plus the monotonic clock separates processes,
/// since a reused pid cannot recur at the same monotonic instant and that
/// clock never steps back (the realtime clock can). The random part means a
/// foreign client cannot create a layer under a token before Quoin reports
/// it; one that can read the Bus can still copy it, and the mesh is the trust
/// boundary for that. Comp's layer state does not outlive the boot, so
/// neither must this. Namespaces are not authenticated, so comp-side
/// enforcement (hiding a panel, excluding its input) acts on the surface comp
/// resolved from the exact token, never on a namespace prefix match.
pub(crate) fn new_layer_identity(prefix: &str) -> String {
    static NEXT: AtomicU64 = AtomicU64::new(1);
    let now = clock_gettime(ClockId::Monotonic);
    let nanos = now.tv_sec as u128 * 1_000_000_000 + now.tv_nsec as u128;
    format!(
        "{prefix}.{}.{nanos}.{}.{:032x}",
        std::process::id(),
        NEXT.fetch_add(1, Ordering::Relaxed),
        unguessable()
    )
}

/// 128 bits from the kernel's CSPRNG. Should `/dev/urandom` be unreadable
/// the token keeps its uniqueness (pid, clock, counter) and loses only its
/// unguessability, which is logged.
fn unguessable() -> u128 {
    use std::io::Read;
    let mut bytes = [0u8; 16];
    match std::fs::File::open("/dev/urandom").and_then(|mut file| file.read_exact(&mut bytes)) {
        Ok(()) => u128::from_ne_bytes(bytes),
        Err(error) => {
            bevy::log::warn!(%error, "no randomness for layer tokens; they stay unique but guessable");
            0
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn layer_identities_are_unique_and_keep_their_prefix() {
        let first = new_layer_identity("dev.cosmix.quoin.panel");
        let second = new_layer_identity("dev.cosmix.quoin.panel");
        assert_ne!(first, second);
        assert!(first.starts_with("dev.cosmix.quoin.panel."));
        let menu = new_layer_identity("dev.cosmix.quoin-corner-menu");
        assert!(menu.starts_with("dev.cosmix.quoin-corner-menu."));
        assert!(!menu.contains(".panel."), "a menu token is not a panel token");
        // 128 random bits: two tokens never share the tail, and it is not
        // derived from anything a foreign client can observe.
        let tail = |token: &str| token.rsplit('.').next().unwrap().to_owned();
        assert_eq!(tail(&first).len(), 32);
        assert!(tail(&first).chars().all(|c| c.is_ascii_hexdigit()));
        assert_ne!(tail(&first), tail(&second));
    }
}
