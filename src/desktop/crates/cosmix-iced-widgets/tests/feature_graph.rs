//! Check that the library links no winit, that each renderer arm selects only
//! its backend (with geometry), and that the winit-based gallery arms stay
//! Wayland-only, alone and unified with the shipping shell selection. Uses the
//! committed lock; may fetch crate sources for an arm the worker has not built
//! yet (forcing offline made the result order-dependent).
use std::path::Path;
use std::process::Command;

fn graph(edges: &str, features: Option<&str>, with_shell: bool) -> String {
    cargo_tree(edges, None, features, with_shell)
}

/// Normal dependencies as `name vX.Y.Z feature,feature` lines.
fn enabled_features(features: &str) -> String {
    cargo_tree("normal", Some("{p} {f}"), Some(features), false)
}

fn cargo_tree(
    edges: &str,
    format: Option<&str>,
    features: Option<&str>,
    with_shell: bool,
) -> String {
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
    let mut command = Command::new(env!("CARGO"));
    command.current_dir(root);
    for (key, _) in std::env::vars_os() {
        if key.to_string_lossy().starts_with("CARGO_") {
            command.env_remove(key);
        }
    }
    command.args([
        "tree",
        "--locked",
        "-e",
        edges,
        "--prefix",
        "none",
        "-p",
        "cosmix-iced-widgets",
    ]);
    if let Some(format) = format {
        command.args(["--format", format]);
    }
    if with_shell {
        command.args(["-p", "cosmix-quoin"]);
    }
    if let Some(features) = features {
        command.args(["--features", features]);
    }
    let output = command.output().expect("run feature graph gate");
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8(output.stdout).expect("UTF-8 cargo tree")
}

fn has_package(graph: &str, name: &str) -> bool {
    let prefix = format!("{name} v");
    graph.lines().any(|line| line.starts_with(&prefix))
}

fn check_no_winit(graph: &str) {
    let lower = graph.to_lowercase();
    assert!(!lower.contains("winit"), "winit in the library graph");
    assert!(
        !has_package(graph, "iced"),
        "iced umbrella in the library graph"
    );
}

fn check_wayland(graph: &str) {
    assert!(graph.contains("winit feature \"wayland\""));
    for edge in [
        "winit feature \"x11\"",
        "iced feature \"x11\"",
        "iced feature \"default\"",
        "iced feature \"debug\"",
        "iced feature \"unconditional-rendering\"",
        "softbuffer feature \"x11\"",
        "window_clipboard feature \"x11\"",
    ] {
        assert!(!graph.contains(edge), "forbidden feature edge: {edge}");
    }
}

#[test]
fn library_links_no_winit_and_selects_no_renderer_by_default() {
    for edges in ["normal", "features"] {
        let default = graph(edges, None, false);
        check_no_winit(&default);
        assert!(!has_package(&default, "iced_wgpu"));
        assert!(!has_package(&default, "iced_tiny_skia"));
    }
}

#[test]
fn renderer_features_select_one_backend_with_geometry_and_no_winit() {
    for (feature, renderer, other) in [
        ("cosmix-iced-widgets/wgpu", "iced_wgpu", "iced_tiny_skia"),
        (
            "cosmix-iced-widgets/tiny-skia",
            "iced_tiny_skia",
            "iced_wgpu",
        ),
    ] {
        let graph = graph("features", Some(feature), false);
        check_no_winit(&graph);
        assert!(has_package(&graph, renderer));
        assert!(!has_package(&graph, other));
        let prefix = format!("{renderer} v");
        let enabled = enabled_features(feature);
        assert!(
            enabled.lines().any(|line| line.starts_with(&prefix)
                && line
                    .split_once(' ')
                    .and_then(|(_, rest)| rest.split_once(' '))
                    .is_some_and(|(_, features)| features
                        .split(',')
                        .any(|feature| feature.trim_end_matches(" (*)") == "geometry"))),
            "{renderer} lacks geometry:\n{enabled}"
        );
    }
}

#[test]
fn gallery_arms_keep_shell_wayland_only() {
    for (feature, renderer, other) in [
        (
            "cosmix-iced-widgets/gallery-wgpu",
            "iced_wgpu",
            "iced_tiny_skia",
        ),
        (
            "cosmix-iced-widgets/gallery-tiny-skia",
            "iced_tiny_skia",
            "iced_wgpu",
        ),
    ] {
        for with_shell in [false, true] {
            let graph = graph("features", Some(feature), with_shell);
            check_wayland(&graph);
            assert!(has_package(&graph, renderer));
            assert!(!has_package(&graph, other));
        }
    }
}
