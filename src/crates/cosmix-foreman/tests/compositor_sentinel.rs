//! Proof by construction for the compositor verifier profile.
//!
//! The fixture has the repository's real split: a clean `src/` workspace and
//! a separate `desktop/` workspace containing `cosmix-comp`. The test proves
//! both holes serially:
//!
//! - a `src`-scoped rust profile passes while unconditional compositor code is
//!   broken, but the compositor profile fails with the compiler's real error;
//! - after removing that sentinel, the compositor profile passes;
//! - a default-feature compositor gate passes while `kms-live` code is broken,
//!   but the full compositor profile fails with the compiler's real error.
//!
//! Never point this test at the repository containing the test itself. An
//! earlier version did that and recursively spawned nested `cargo test` runs.
//! Every mutation and nested Cargo invocation below is confined to one small
//! temporary fixture, and the two dimensions run in one test so the Rust test
//! harness cannot fan them out concurrently.

use std::fs;
use std::path::Path;

use cosmix_foreman::verify::{self, VerifyReport};

const CLEAN_COMP: &str = "pub fn compositor_value() -> u32 {\n    42\n}\n";
const BROKEN_COMP: &str = "pub fn compositor_value() -> u32 {\n    \"sentinel is not a u32\"\n}\n";
const BROKEN_KMS: &str = "#[cfg(feature = \"kms-live\")]\n\
                          pub fn kms_value() -> u32 {\n    \"kms sentinel is not a u32\"\n}\n\
                          \n\
                          pub fn compositor_value() -> u32 {\n    42\n}\n";

fn write_fixture(root: &Path, comp_source: &str) -> anyhow::Result<()> {
    let src = root.join("src");
    let desktop = root.join("desktop");
    let comp = desktop.join("crates/cosmix-comp");
    let smithay = desktop.join("vendor/smithay");
    fs::create_dir_all(src.join("src"))?;
    fs::create_dir_all(comp.join("src"))?;
    fs::create_dir_all(smithay.join("src"))?;

    fs::write(
        src.join("Cargo.toml"),
        "[package]\nname = \"fixture-src\"\nversion = \"0.1.0\"\nedition = \"2021\"\n",
    )?;
    fs::write(
        src.join("src/lib.rs"),
        "pub fn source_value() -> u32 {\n    7\n}\n",
    )?;

    fs::write(
        desktop.join("Cargo.toml"),
        "[workspace]\nmembers = [\"crates/cosmix-comp\"]\nresolver = \"2\"\n",
    )?;
    fs::write(
        comp.join("Cargo.toml"),
        "[package]\n\
         name = \"cosmix-comp\"\n\
         version = \"0.1.0\"\n\
         edition = \"2021\"\n\
         \n\
         [features]\n\
         kms-live = []\n\
         explicit-sync-live-test = []\n\
         \n\
         [dependencies]\n\
         smithay = { path = \"../../vendor/smithay\" }\n",
    )?;
    fs::write(comp.join("src/lib.rs"), comp_source)?;

    fs::write(
        smithay.join("Cargo.toml"),
        "[package]\nname = \"smithay\"\nversion = \"0.1.0\"\nedition = \"2021\"\n",
    )?;
    fs::write(
        smithay.join("src/lib.rs"),
        "pub fn fixture_dependency() {}\n",
    )?;
    Ok(())
}

fn assert_pass(report: &VerifyReport, label: &str) {
    assert!(
        report.pass,
        "{label} unexpectedly failed:\n{}",
        report.failure_digest()
    );
    assert!(
        report
            .steps
            .iter()
            .all(|step| step.pass && step.exit_code == Some(0)),
        "{label} did not report exit 0 for every step: {:?}",
        report
            .steps
            .iter()
            .map(|step| step.exit_code)
            .collect::<Vec<_>>()
    );
}

fn assert_real_type_error(report: &VerifyReport, label: &str) {
    assert!(!report.pass, "{label} unexpectedly passed");
    assert_eq!(
        report.steps.last().and_then(|step| step.exit_code),
        Some(101),
        "{label} should retain Cargo's compiler-failure exit code"
    );
    let digest = report.failure_digest();
    assert!(
        digest.contains("crates/cosmix-comp/src/lib.rs")
            && digest.contains("error[E0308]: mismatched types")
            && digest.contains("expected `u32`, found `&str`"),
        "{label} digest lost the real compiler error:\n{digest}"
    );
}

fn default_feature_commands() -> Vec<Vec<String>> {
    vec![
        vec!["cargo".into(), "tree".into(), "-i".into(), "smithay".into()],
        vec![
            "cargo".into(),
            "fmt".into(),
            "--check".into(),
            "-p".into(),
            "cosmix-comp".into(),
        ],
        vec![
            "cargo".into(),
            "clippy".into(),
            "--all-targets".into(),
            "-p".into(),
            "cosmix-comp".into(),
            "--".into(),
            "-D".into(),
            "warnings".into(),
        ],
        vec![
            "cargo".into(),
            "test".into(),
            "-p".into(),
            "cosmix-comp".into(),
        ],
    ]
}

#[test]
fn compositor_profile_proves_scope_and_feature_gates() {
    let tmpdir = tempfile::TempDir::new().expect("create fixture directory");
    let root = tmpdir.path();

    // Scope sentinel: src remains green while desktop is unconditionally red.
    write_fixture(root, BROKEN_COMP).expect("write unconditional sentinel fixture");
    let src_report =
        verify::run_profile("rust", root, Some("src")).expect("run src-scoped profile");
    assert_pass(&src_report, "src-scoped profile with broken desktop");

    let compositor_report = verify::run_profile("compositor", root, Some("src"))
        .expect("run compositor profile on unconditional sentinel");
    assert_real_type_error(&compositor_report, "unconditional compositor sentinel");

    // Reverting the sentinel restores a genuinely green compositor profile.
    fs::write(
        root.join("desktop/crates/cosmix-comp/src/lib.rs"),
        CLEAN_COMP,
    )
    .expect("remove unconditional sentinel");
    let restored_report = verify::run_profile("compositor", root, Some("src"))
        .expect("run compositor profile after sentinel removal");
    assert_pass(&restored_report, "restored compositor profile");

    // Feature sentinel: a full default-feature gate is green, including
    // clippy and tests, while the real compositor profile enables kms-live
    // and must retain the compiler's type error in its digest.
    fs::write(
        root.join("desktop/crates/cosmix-comp/src/lib.rs"),
        BROKEN_KMS,
    )
    .expect("write kms-live sentinel");
    let default_report = verify::run_commands(
        "compositor-default-features",
        &default_feature_commands(),
        &root.join("desktop"),
    )
    .expect("run default-feature compositor gate");
    assert_pass(
        &default_report,
        "default-feature gate with broken kms-live code",
    );

    let feature_report = verify::run_profile("compositor", root, Some("src"))
        .expect("run compositor profile on kms-live sentinel");
    assert_real_type_error(&feature_report, "kms-live compositor sentinel");
}
