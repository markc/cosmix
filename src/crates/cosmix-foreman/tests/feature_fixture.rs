//! End-to-end proof that rust tier 1 discovers and executes a gated test.

use std::process::Command;

use cosmix_foreman::config::FleetPolicy;
use cosmix_foreman::verify::{run_commands, tier_commands_in_dir_with_policy};

/// Sentinel marking the process an owned-helper test is allowed to run in.
const HELPER_ENV: &str = "COSMIX_FOREMAN_FEATURE_FIXTURE_HELPER";

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

#[test]
fn feature_gated_test_failure_is_caught_by_the_verifier_engine() {
    run_owned_helper("feature_gated_test_failure_is_caught_by_the_verifier_engine_owned_process");
}

/// Keep the proof independent of a workstation's optional memguard/sccache
/// wrappers: managed test sandboxes may expose both binaries while denying
/// the systemd/socket access they require.
#[test]
#[ignore = "run only in the process spawned by the parent test of the same name"]
fn feature_gated_test_failure_is_caught_by_the_verifier_engine_owned_process() {
    assert_owned_helper(
        "feature_gated_test_failure_is_caught_by_the_verifier_engine_owned_process",
    );
    let fixture = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("testdata")
        .join("feature-fixture");
    let policy = FleetPolicy::defaults();

    let tier1 = tier_commands_in_dir_with_policy("rust", 1, &fixture, &policy)
        .expect("resolve fixture tier 1");
    let feature_step = tier1
        .iter()
        .find(|command| {
            command.contains(&"--features".to_string()) && command.contains(&"broken".to_string())
        })
        .unwrap_or_else(|| panic!("tier 1 did not discover the fixture feature: {tier1:?}"))
        .clone();
    assert!(feature_step.contains(&"fixture-feature-gap".to_string()));

    let default = run_commands(
        "fixture-default",
        &[vec!["cargo".into(), "test".into()]],
        &fixture,
    )
    .expect("run fixture default tests");
    assert!(
        default.pass,
        "default-only fixture must be green: {default:?}"
    );

    let gated =
        run_commands("fixture-broken", &[feature_step], &fixture).expect("run fixture gated tests");
    assert!(!gated.pass, "gated failure must make the verifier red");
    let digest = gated.failure_digest();
    assert!(
        digest.contains("gated_test_fails") || digest.contains("gated test always fails"),
        "failure digest should identify the gated test: {digest}"
    );
}
