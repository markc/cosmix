use serde::Deserialize;
use std::collections::BTreeSet;
use std::path::Path;
use std::process::Command;

#[derive(Deserialize)]
struct CargoMetadata {
    metadata: WorkspaceMetadata,
    packages: Vec<PackageMetadata>,
}

#[derive(Deserialize)]
struct WorkspaceMetadata {
    #[serde(rename = "cosmix-install")]
    cosmix_install: InstallMetadata,
}

#[derive(Deserialize)]
struct InstallMetadata {
    quoin: ShippingSelection,
}

#[derive(Deserialize)]
struct ShippingSelection {
    package: String,
    features: Vec<String>,
    bin: String,
}

#[derive(Deserialize)]
struct PackageMetadata {
    name: String,
    targets: Vec<TargetMetadata>,
}

#[derive(Deserialize)]
struct TargetMetadata {
    name: String,
    kind: Vec<String>,
    #[serde(default, rename = "required-features")]
    required_features: Vec<String>,
}

fn cargo_command(desktop_root: &Path) -> Command {
    let mut command = Command::new(env!("CARGO"));
    command.current_dir(desktop_root);
    for (name, _) in std::env::vars_os() {
        if name.to_string_lossy().starts_with("CARGO_") {
            command.env_remove(name);
        }
    }
    command
        .env("CARGO_NET_OFFLINE", "true")
        .env("CARGO_TERM_COLOR", "never");
    command
}

fn output_or_panic(mut command: Command, purpose: &str) -> std::process::Output {
    let output = command
        .output()
        .unwrap_or_else(|error| panic!("{purpose}: {error}"));
    assert!(
        output.status.success(),
        "{purpose} failed:\n{}",
        String::from_utf8_lossy(&output.stderr)
    );
    output
}

#[test]
fn shipped_quoin_graph_has_wayland_without_x11() {
    let desktop_root = Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
    let mut metadata_command = cargo_command(&desktop_root);
    metadata_command.args([
        "metadata",
        "--locked",
        "--offline",
        "--no-deps",
        "--format-version",
        "1",
    ]);
    let metadata_output = output_or_panic(metadata_command, "read desktop shipping metadata");
    let metadata: CargoMetadata =
        serde_json::from_slice(&metadata_output.stdout).expect("parse desktop shipping metadata");
    let selection = metadata.metadata.cosmix_install.quoin;

    let package = metadata
        .packages
        .iter()
        .find(|package| package.name == selection.package)
        .expect("shipping package exists in the desktop workspace");
    let binary = package
        .targets
        .iter()
        .find(|target| target.name == selection.bin && target.kind.iter().any(|kind| kind == "bin"))
        .expect("shipping binary exists in the configured package");
    assert!(
        binary.required_features.is_empty(),
        "the installed Quoin binary must not be skipped by required-features"
    );

    let mut tree_command = cargo_command(&desktop_root);
    tree_command.args([
        "tree",
        "--locked",
        "--offline",
        "-e",
        "features",
        "--prefix",
        "none",
        "-p",
        &selection.package,
    ]);
    if !selection.features.is_empty() {
        tree_command
            .arg("--features")
            .arg(selection.features.join(","));
    }
    let output = output_or_panic(tree_command, "resolve the shipped Quoin feature graph");

    let graph = String::from_utf8_lossy(&output.stdout);
    let winit_packages = graph
        .lines()
        .filter(|line| line.starts_with("winit v"))
        .map(|line| line.trim_end_matches(" (*)"))
        .collect::<BTreeSet<_>>();
    assert_eq!(
        winit_packages.len(),
        1,
        "shipped Quoin graph must resolve exactly one winit package: {winit_packages:?}"
    );
    let x11_edges = graph
        .lines()
        .filter(|line| line.contains("winit feature \"x11\""))
        .collect::<Vec<_>>();
    assert!(
        x11_edges.is_empty(),
        "shipped Quoin graph enabled X11:\n{}",
        x11_edges.join("\n")
    );
    assert!(
        graph
            .lines()
            .any(|line| line.contains("winit feature \"wayland\"")),
        "shipped Quoin graph did not enable a Wayland winit backend"
    );
}
