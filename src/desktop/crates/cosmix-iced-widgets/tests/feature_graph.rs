//! Check both standalone renderer arms and feature unification with the shipping
//! shell selection. Run on a worker with the committed lock and cached crates.
use std::path::Path;
use std::process::Command;

fn graph(features: Option<&str>, with_shell: bool) -> String {
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
    let mut command = Command::new(env!("CARGO"));
    command.current_dir(root);
    for (key, _) in std::env::vars_os() {
        if key.to_string_lossy().starts_with("CARGO_") {
            command.env_remove(key);
        }
    }
    command.env("CARGO_NET_OFFLINE", "true").args([
        "tree",
        "--locked",
        "--offline",
        "-e",
        "features",
        "--prefix",
        "none",
        "-p",
        "cosmix-iced-widgets",
    ]);
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
fn defaults_select_neither_renderer_and_both_arms_keep_shell_wayland_only() {
    let default = graph(None, false);
    check_wayland(&default);
    assert!(
        !default
            .lines()
            .any(|line| line.starts_with("iced_wgpu v") || line.starts_with("iced_tiny_skia v"))
    );
    for (feature, renderer, other) in [
        (
            "cosmix-iced-widgets/gallery-wgpu",
            "iced_wgpu v",
            "iced_tiny_skia v",
        ),
        (
            "cosmix-iced-widgets/gallery-tiny-skia",
            "iced_tiny_skia v",
            "iced_wgpu v",
        ),
    ] {
        for with_shell in [false, true] {
            let graph = graph(Some(feature), with_shell);
            check_wayland(&graph);
            assert!(graph.lines().any(|line| line.starts_with(renderer)));
            assert!(!graph.lines().any(|line| line.starts_with(other)));
        }
    }
}
