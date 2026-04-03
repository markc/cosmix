fn main() {
    // Link against system libgtk-layer-shell
    pkg_config::Config::new()
        .atleast_version("0.6")
        .probe("gtk-layer-shell-0")
        .expect("gtk-layer-shell-0 not found via pkg-config");
}
