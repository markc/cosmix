//! Standalone Wayland layer-shell host for the shared Quoin application.
fn main() -> bevy::app::AppExit {
    // --version/-V: answer and exit 0 before any other side effect.
    cosmix_buildinfo::exit_on_version!();
    cosmix_quoin::run_layer_host()
}
