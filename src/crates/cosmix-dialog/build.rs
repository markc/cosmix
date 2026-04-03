fn main() {
    // Link against system libgtk-layer-shell when the feature is enabled.
    // The library is probed at build time; if missing, compilation fails with a clear message.
    #[cfg(feature = "layer-shell")]
    {
        pkg_config::Config::new()
            .atleast_version("0.6")
            .probe("gtk-layer-shell-0")
            .expect("gtk-layer-shell-0 >= 0.6 not found via pkg-config. Install gtk-layer-shell.");
    }
}
