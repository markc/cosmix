//! Layer identities for the Bus holder plane. The token is also the layer-shell
//! namespace, so comp resolves the same identity without a client-local wl id.
use bevy::prelude::*;
use cosmix_shell::core::{Edge, OutputKey};
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

pub(crate) fn new_panel_identity(namespace: &str) -> String {
    static NEXT: AtomicU64 = AtomicU64::new(1);
    let epoch = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default().as_nanos();
    format!("{namespace}.panel.{}.{epoch}.{}", std::process::id(), NEXT.fetch_add(1, Ordering::Relaxed))
}
