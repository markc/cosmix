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
}

impl PanelLayerIdentities {
    pub fn get(&self, output: &OutputKey, edge: Edge) -> Option<&str> {
        self.0.iter().find(|(o, e, _)| o == output && *e == edge)
            .map(|(_, _, token)| token.as_str())
    }
}

/// A token no other layer of this boot shares: `prefix.<pid>.<ns>.<n>`.
/// The counter separates layers of one process; pid plus the monotonic
/// clock separates processes, since a reused pid cannot recur at the same
/// monotonic instant and that clock never steps back (the realtime clock
/// can). Comp's layer state does not outlive the boot, so neither must this.
/// Namespaces are not authenticated, so comp-side enforcement (hiding a panel,
/// excluding its input) must act on the surface comp resolved from the exact
/// token, never on a namespace prefix match.
pub(crate) fn new_layer_identity(prefix: &str) -> String {
    static NEXT: AtomicU64 = AtomicU64::new(1);
    let now = clock_gettime(ClockId::Monotonic);
    let nanos = now.tv_sec as u128 * 1_000_000_000 + now.tv_nsec as u128;
    format!("{prefix}.{}.{nanos}.{}", std::process::id(), NEXT.fetch_add(1, Ordering::Relaxed))
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
    }
}
