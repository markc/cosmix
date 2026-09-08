//! Standalone Wayland layer-shell host for the shared Quoin application.
fn main() -> bevy::app::AppExit {
    cosmix_quoin::run_layer_host()
}
