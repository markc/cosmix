use std::path::Path;
use std::process::Command;

#[test]
fn quoin_demo_graph_has_wayland_without_x11() {
    let desktop_root = Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
    let output = Command::new(env!("CARGO"))
        .current_dir(desktop_root)
        .args([
            "tree",
            "--locked",
            "--offline",
            "-e",
            "features",
            "-i",
            "winit@0.30.13",
            "-p",
            "cosmix-quoin",
            "--features",
            "demo",
        ])
        .output()
        .expect("run cargo tree for the Quoin demo graph");

    assert!(
        output.status.success(),
        "cargo tree failed:\n{}",
        String::from_utf8_lossy(&output.stderr)
    );

    let graph = String::from_utf8_lossy(&output.stdout);
    let x11_edges = graph
        .lines()
        .filter(|line| line.contains("winit feature \"x11\""))
        .collect::<Vec<_>>();
    assert!(
        x11_edges.is_empty(),
        "Quoin demo graph enabled X11:\n{}",
        x11_edges.join("\n")
    );
    assert!(
        graph
            .lines()
            .any(|line| line.contains("winit feature \"wayland\"")),
        "Quoin demo graph did not enable a Wayland winit backend"
    );
}
