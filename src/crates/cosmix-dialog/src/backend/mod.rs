//! Backend dispatch — selects between layer-shell (compact) and Dioxus (full) rendering.

#[cfg(feature = "desktop")]
pub mod dioxus_backend;

#[cfg(feature = "layer-shell")]
pub mod layer_backend;

pub mod blocking;

use crate::DialogRequest;

/// Which rendering backend to use.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum BackendKind {
    /// Dioxus Desktop (WebKitGTK) — full-featured, handles all dialog types.
    #[cfg(feature = "desktop")]
    Dioxus,
    /// GTK layer-shell — compact native dialogs, bypasses 240px minimum.
    #[cfg(feature = "layer-shell")]
    LayerShell,
}

/// CLI override for backend selection.
#[derive(Debug, Clone, Copy, PartialEq, clap::ValueEnum)]
pub enum BackendOverride {
    /// Auto-select based on dialog size and environment.
    Auto,
    /// Force Dioxus Desktop rendering.
    Dioxus,
    /// Force layer-shell rendering.
    Layer,
}

/// Select the best backend for the given request and environment.
pub fn select_backend(request: &DialogRequest, override_: Option<BackendOverride>) -> BackendKind {
    // Explicit override
    match override_ {
        #[cfg(feature = "desktop")]
        Some(BackendOverride::Dioxus) => return BackendKind::Dioxus,
        #[cfg(feature = "layer-shell")]
        Some(BackendOverride::Layer) => return BackendKind::LayerShell,
        _ => {}
    }

    // Auto-selection: use layer-shell for compact dialogs on Wayland
    #[cfg(feature = "layer-shell")]
    {
        let (_, h) = request.default_size();
        let on_wayland = std::env::var("WAYLAND_DISPLAY").is_ok();

        if on_wayland && h < 240 && layer_backend::is_available() {
            return BackendKind::LayerShell;
        }
    }

    // Default to Dioxus
    #[cfg(feature = "desktop")]
    {
        return BackendKind::Dioxus;
    }

    #[cfg(not(feature = "desktop"))]
    {
        #[cfg(feature = "layer-shell")]
        return BackendKind::LayerShell;

        #[cfg(not(feature = "layer-shell"))]
        compile_error!("At least one of 'desktop' or 'layer-shell' features must be enabled");
    }
}
