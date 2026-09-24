//! `mix --version --json` (0.63.0) — machine-readable build provenance.
//! The release-B gate compares a recorded 40-hex source_commit against
//! `git_sha_full`; these pins keep the contract shape stable.

use std::process::Command;

#[test]
fn version_json_shape_and_full_sha() {
    let out = Command::new(env!("CARGO_BIN_EXE_mix"))
        .args(["--version", "--json"])
        .env("MIX_STATS", "off")
        .output()
        .expect("run mix --version --json");
    assert!(out.status.success());
    let v: serde_json::Value =
        serde_json::from_str(&String::from_utf8_lossy(&out.stdout)).expect("valid JSON");
    let obj = v.as_object().unwrap();
    let mut keys: Vec<&str> = obj.keys().map(|s| s.as_str()).collect();
    keys.sort_unstable();
    assert_eq!(
        keys,
        [
            "build_time",
            // Added with the substrate-wide --version contract: the JSON
            // names its component like every other cosmix binary's.
            "component",
            "git_dirty",
            "git_sha",
            "git_sha_full",
            "version"
        ]
    );
    assert!(v["git_dirty"].is_boolean());
    let full = v["git_sha_full"].as_str().unwrap();
    // A real build carries the 40-hex object id; a gitless build carries
    // the explicit "unknown" — never a short sha in the full field.
    assert!(
        full == "unknown" || (full.len() == 40 && full.chars().all(|c| c.is_ascii_hexdigit())),
        "git_sha_full must be 40-hex or unknown, got {full:?}"
    );
    if full != "unknown" {
        let short = v["git_sha"].as_str().unwrap();
        assert!(full.starts_with(short), "short sha is a prefix of full");
    }
}

/// The embedded provenance marker (cosmix-lib-buildinfo `find_markers`): the
/// mix binary carries, as plain bytes, the same provenance its
/// `--version --json` prints — readable without executing it. `component`
/// is the one field that differs: the marker names the crate (`cosmix-mix`),
/// the JSON answer names the binary (`mix`).
#[test]
fn embedded_marker_matches_version_json() {
    let bin = env!("CARGO_BIN_EXE_mix");
    let out = Command::new(bin)
        .args(["--version", "--json"])
        .env("MIX_STATS", "off")
        .output()
        .expect("run mix --version --json");
    let answer: serde_json::Value =
        serde_json::from_str(&String::from_utf8_lossy(&out.stdout)).expect("valid JSON");
    let bytes = std::fs::read(bin).expect("read the mix binary");
    let markers: Vec<serde_json::Value> = cosmix_buildinfo::find_markers(&bytes)
        .iter()
        .map(|m| serde_json::from_str(m).expect("marker is JSON"))
        .filter(|m: &serde_json::Value| m["component"] == "cosmix-mix")
        .collect();
    assert_eq!(markers.len(), 1, "exactly one cosmix-mix marker in {bin}");
    let marker = &markers[0];
    for key in ["version", "git_sha", "git_sha_full", "git_dirty", "build_time"] {
        assert_eq!(marker[key], answer[key], "marker {key} equals --version --json");
    }
}

#[test]
fn plain_version_line_unchanged() {
    let out = Command::new(env!("CARGO_BIN_EXE_mix"))
        .args(["--version"])
        .env("MIX_STATS", "off")
        .output()
        .expect("run mix --version");
    let s = String::from_utf8_lossy(&out.stdout);
    assert!(s.starts_with("mix "), "{s}");
    assert!(!s.contains('{'), "plain form stays a line: {s}");
}
