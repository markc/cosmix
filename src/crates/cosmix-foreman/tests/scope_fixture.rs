//! End-to-end proof that task crate scope includes transitive reverse deps.

use std::process::Command;

use cosmix_foreman::config::FleetPolicy;
use cosmix_foreman::verify::{run_commands, tier_commands_for_crates_in_dir_with_policy};

/// Sentinel marking the process an owned-helper test is allowed to run in.
const HELPER_ENV: &str = "COSMIX_FOREMAN_SCOPE_FIXTURE_HELPER";

/// Run `name` (the `#[ignore]`d scenario below) in a process of its own and
/// fail loudly if the scenario fails. Its curated tool path is installed on
/// the child command, so the parent test process is never mutated.
fn run_owned_helper(name: &str) {
    let out = Command::new(std::env::current_exe().unwrap())
        .args([
            "--exact",
            name,
            "--ignored",
            "--nocapture",
            "--test-threads=1",
        ])
        .env(HELPER_ENV, name)
        .env("PATH", "/usr/sbin:/usr/bin")
        .env("RUSTC_WRAPPER", "")
        .output()
        .expect("spawn owned helper test process");
    assert!(
        out.status.success(),
        "owned helper {name} failed\n--- stdout ---\n{}\n--- stderr ---\n{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
}

fn assert_owned_helper(name: &str) {
    assert_eq!(
        std::env::var(HELPER_ENV).as_deref(),
        Ok(name),
        "this test must only run inside the process spawned for it"
    );
}

fn fixture() -> std::path::PathBuf {
    std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("testdata")
        .join("scope-fixture")
}

#[test]
fn scoped_tier0_catches_a_broken_reverse_dependency_test() {
    run_owned_helper("scoped_tier0_catches_a_broken_reverse_dependency_test_owned_process");
}

/// Keep the proof independent of a workstation's optional memguard/sccache
/// wrappers: managed test sandboxes may expose both binaries while denying
/// the systemd/socket access they require.
#[test]
#[ignore = "run only in the process spawned by the parent test of the same name"]
fn scoped_tier0_catches_a_broken_reverse_dependency_test_owned_process() {
    assert_owned_helper("scoped_tier0_catches_a_broken_reverse_dependency_test_owned_process");
    let fixture = fixture();
    let commands = tier_commands_for_crates_in_dir_with_policy(
        "rust",
        0,
        &fixture,
        &FleetPolicy::defaults(),
        &["scope-leaf".to_string()],
    )
    .expect("resolve task crate scope");
    let test = commands
        .iter()
        .find(|command| command.get(1).map(String::as_str) == Some("test"))
        .expect("scoped tier 0 test step")
        .clone();
    assert!(
        test.windows(2)
            .any(|pair| pair == ["--package", "scope-leaf"])
    );
    assert!(
        test.windows(2)
            .any(|pair| pair == ["--package", "scope-consumer"]),
        "reverse dependency missing from scoped test: {test:?}"
    );
    assert!(!test.iter().any(|arg| arg == "scope-outsider"), "{test:?}");

    let report = run_commands("reverse-dependency-fixture", &[test], &fixture)
        .expect("execute scoped reverse-dependency tests");
    assert!(
        !report.pass,
        "broken reverse dependency must make tier 0 red"
    );
    assert!(
        report
            .failure_digest()
            .contains("deliberately_broken_reverse_dependency_test"),
        "{}",
        report.failure_digest()
    );
    assert!(report.provenance_tier.is_none());
    assert!(
        report
            .steps
            .iter()
            .all(|step| step.executed_binaries.is_none()),
        "tier 0 must record target_dir only: {report:?}"
    );
}

#[test]
fn tier1_keeps_one_workspace_test_and_scopes_the_feature_matrix() {
    let commands = tier_commands_for_crates_in_dir_with_policy(
        "rust",
        1,
        &fixture(),
        &FleetPolicy::defaults(),
        &["scope-leaf".to_string()],
    )
    .expect("resolve scoped tier 1");
    assert_eq!(
        commands
            .iter()
            .filter(|command| command.as_slice() == ["cargo", "test", "--workspace"])
            .count(),
        1,
        "tier 1 must keep exactly one full workspace suite: {commands:?}"
    );
    let feature_packages = commands
        .iter()
        .filter(|command| command.iter().any(|arg| arg == "--features"))
        .filter_map(|command| command.get(3).map(String::as_str))
        .collect::<Vec<_>>();
    assert!(feature_packages.contains(&"scope-leaf"), "{commands:?}");
    assert!(feature_packages.contains(&"scope-consumer"), "{commands:?}");
    assert!(
        !feature_packages.contains(&"scope-outsider"),
        "{commands:?}"
    );
}
