fn main() {
    cosmix_buildinfo::emit();
    // `theme`'s winit event-loop wake fallback (bus.rs / theme.rs) needs a
    // winit backend, and CTK's base bevy feature set always compiles
    // bevy_winit — so a `theme` build without a platform feature dies deep
    // inside winit with an unrelated-looking "platform not supported" error
    // long after the real mistake. This is the fourth instance of the
    // feature-graph class that bit slice 3 three times; fail here, first and
    // by name, instead. (A consumer relying on another crate unifying
    // `bevy/wayland` into the graph is exactly the borrowed-facade fragility
    // slice 3 removed, so it is refused too.)
    let theme = std::env::var_os("CARGO_FEATURE_THEME").is_some();
    let platform = std::env::var_os("CARGO_FEATURE_PLATFORM_WAYLAND").is_some()
        || std::env::var_os("CARGO_FEATURE_PLATFORM_X11").is_some();
    if theme && !platform {
        panic!(
            "ctk feature `theme` requires `platform-wayland` or `platform-x11`: \
             its event-loop wake fallback rides winit, which has no backend \
             without one of them. Enable a platform feature on the ctk \
             dependency (the default features include platform-wayland)."
        );
    }
}
