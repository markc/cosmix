fn main() -> bevy::prelude::AppExit {
    // --version/-V: answer and exit 0 before any other side effect.
    cosmix_buildinfo::exit_on_version!();
    cosmix_bg_showcase::boids::run()
}
