//! Standing guard for the spamlite engine pin.
//!
//! maild consumes the engine as a git dependency pinned by `rev` in this
//! crate's Cargo.toml. Two silent failure modes exist:
//!
//! 1. an ambient `[patch."https://github.com/markc/spamlite"]` (user-level
//!    or checkout-level) makes EVERY build — including a CI or foreman build —
//!    use a working tree instead of the pin, and nothing in the build output
//!    says so;
//! 2. the pin is edited by hand to a sha that cargo then resolves to something
//!    else (a moved tag, a fetch of a different fragment).
//!
//! `cargo metadata` reports the *resolved* source of every package. This test
//! asserts that exactly one `spamlite` package is resolved, that it comes from
//! the pinned git URL, and that its resolved commit fragment equals the `rev`
//! written in Cargo.toml. Hermetic: `--offline` against the already-fetched
//! registry/git caches (a build of this crate has necessarily fetched them).

use std::path::Path;
use std::process::Command;

fn pinned_rev(manifest: &str) -> String {
    let line = manifest
        .lines()
        .find(|l| l.trim_start().starts_with("spamlite") && l.contains("rev ="))
        .expect("Cargo.toml has a `spamlite = { git = …, rev = … }` line");
    let start = line.find("rev = \"").expect("rev = \"…\"") + "rev = \"".len();
    let rest = &line[start..];
    let end = rest.find('"').expect("closing quote after rev");
    rest[..end].to_string()
}

#[test]
fn resolved_spamlite_is_the_git_pin_not_a_patch() {
    let manifest_dir = Path::new(env!("CARGO_MANIFEST_DIR"));
    let manifest = std::fs::read_to_string(manifest_dir.join("Cargo.toml")).unwrap();
    let rev = pinned_rev(&manifest);
    assert!(
        rev.len() >= 7 && rev.chars().all(|c| c.is_ascii_hexdigit()),
        "pin rev must be a hex sha, got {rev:?}"
    );

    let out = Command::new(env!("CARGO"))
        .args([
            "metadata",
            "--format-version",
            "1",
            "--offline",
            "--manifest-path",
        ])
        .arg(manifest_dir.join("Cargo.toml"))
        .output()
        .expect("run cargo metadata");
    assert!(
        out.status.success(),
        "cargo metadata failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let meta: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
    let packages = meta["packages"].as_array().expect("packages array");
    let spamlite: Vec<&serde_json::Value> = packages
        .iter()
        .filter(|p| p["name"].as_str() == Some("spamlite"))
        .collect();
    assert_eq!(
        spamlite.len(),
        1,
        "expected exactly one resolved spamlite package, found {}",
        spamlite.len()
    );
    let source = spamlite[0]["source"].as_str().unwrap_or_else(|| {
        panic!(
            "spamlite has no `source` — a path/[patch] override is in effect: {}",
            spamlite[0]["manifest_path"]
        )
    });
    assert!(
        source.starts_with("git+https://github.com/markc/spamlite?rev="),
        "spamlite resolved from an unexpected source: {source}"
    );
    let resolved = source
        .rsplit('#')
        .next()
        .expect("git source carries a #<commit> fragment");
    assert!(
        resolved.starts_with(&rev) || rev.starts_with(resolved),
        "resolved spamlite commit {resolved} is not the pinned rev {rev}"
    );
}
