//! Regression test for fleet task 44: a shared `CARGO_TARGET_DIR` must never
//! let a verifier run in one worktree execute a binary compiled from
//! ANOTHER worktree's source — and, unlike the first (rejected) landing
//! attempt's mtime-touch, this must hold even against a writer that is
//! ACTIVELY compiling into the shared directory WHILE the verifier under
//! test runs. See `src/target_dir.rs` for the mechanism and the live
//! incidents this reproduces in miniature.

use std::path::Path;
use std::process::Command;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use cosmix_foreman::verify::run_commands;

mod support;

/// Sentinel marking the process an owned-helper test is allowed to run in.
const HELPER_ENV: &str = "COSMIX_FOREMAN_TARGET_ISOLATION_HELPER";
const HELPER_SHARED_TARGET_ENV: &str = "COSMIX_FOREMAN_TEST_SHARED_TARGET";

/// Run `name` (an `#[ignore]`d test below) in a process of its own and fail
/// loudly if the scenario fails. The parent installs each scenario's ambient
/// `CARGO_TARGET_DIR` on that child command, so no test mutates the parent
/// process and the scenarios can run concurrently.
///
/// Each child also gets a PRIVATE verify lane: its scenario calls
/// `run_commands`, which flocks a lane, and the default lane is host-global —
/// while this parent binary's own in-process tests hold it, a spawned child
/// is the holder's DESCENDANT, and `host_lane` refuses to wait on an ancestor
/// (waiting really would deadlock), so the child dies instantly with "would
/// deadlock on the host verify lane". The private absolute lane is the
/// operator affordance that error prescribes — same pattern as the verify-CLI
/// children further down this file.
fn run_owned_helper(name: &str) {
    let lane_dir = tempfile::tempdir().expect("create private verify lane dir");
    let shared_target = lane_dir.path().join("shared-target");
    let out = Command::new(std::env::current_exe().unwrap())
        .args([
            "--exact",
            name,
            "--ignored",
            "--nocapture",
            "--test-threads=1",
        ])
        .env(HELPER_ENV, name)
        .env("CARGO_TARGET_DIR", &shared_target)
        .env(HELPER_SHARED_TARGET_ENV, &shared_target)
        .env("FOREMAN_VERIFY_LANE", lane_dir.path().join("verify.lock"))
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

fn helper_shared_target() -> std::path::PathBuf {
    std::env::var_os(HELPER_SHARED_TARGET_ENV)
        .map(std::path::PathBuf::from)
        .expect("owned-helper parent supplies the scenario's shared target")
}

/// Backstop for the concurrent-writer thread below: at ~50ms per round on a
/// one-file crate this is minutes of contention, far more than the verifier
/// run it races needs, while still being a finite number of `cargo`
/// processes if that run stalls.
const MAX_WRITER_ROUNDS: usize = 2000;

fn write_probe_crate(dir: &Path, marker: &str) {
    std::fs::create_dir_all(dir.join("src")).unwrap();
    std::fs::write(
        dir.join("Cargo.toml"),
        "[package]\nname = \"foreman-target-dir-probe\"\nversion = \"0.1.0\"\nedition = \"2021\"\n",
    )
    .unwrap();
    write_probe_source(dir, marker, 0);
}

fn write_probe_source(dir: &Path, marker: &str, build_round: usize) {
    std::fs::write(
        dir.join("src/main.rs"),
        format!("fn main() {{ let _build_round = {build_round}; println!(\"{marker}\"); }}\n"),
    )
    .unwrap();
}

fn write_test_probe_crate(dir: &Path, marker: &str, body: &str) {
    std::fs::create_dir_all(dir.join("src")).unwrap();
    std::fs::write(
        dir.join("Cargo.toml"),
        "[package]\nname = \"foreman-tier-probe\"\nversion = \"0.1.0\"\nedition = \"2021\"\n",
    )
    .unwrap();
    std::fs::write(
        dir.join("src/lib.rs"),
        format!("pub const TREE: &str = {marker:?};\n{body}\n"),
    )
    .unwrap();
}

fn raw_cargo_test_no_run(dir: &Path, target_dir: &Path) {
    let output = Command::new("cargo")
        .args(["test", "--no-run", "--quiet"])
        .current_dir(dir)
        .env("CARGO_TARGET_DIR", target_dir)
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "cargo test --no-run in {}:\n{}",
        dir.display(),
        String::from_utf8_lossy(&output.stderr)
    );
}

/// A plain, unfenced `cargo build` into a shared target dir — exactly what
/// an ad hoc `cargo build`/`cargo test` in a task worktree looks like when
/// nothing isolates it.
fn raw_cargo_build(dir: &Path, target_dir: &Path) {
    let status = Command::new("cargo")
        .args(["build", "--quiet"])
        .current_dir(dir)
        .env("CARGO_TARGET_DIR", target_dir)
        .status()
        .unwrap();
    assert!(status.success(), "cargo build in {}", dir.display());
}

fn run_probe(target_dir: &Path) -> String {
    let bin = target_dir.join("debug").join("foreman-target-dir-probe");
    let out = Command::new(&bin)
        .output()
        .unwrap_or_else(|e| panic!("running {}: {e}", bin.display()));
    assert!(out.status.success(), "running {}", bin.display());
    String::from_utf8_lossy(&out.stdout).trim().to_string()
}

#[test]
fn shared_target_dir_launders_a_binary_across_worktrees_when_unfenced() {
    let tmp = tempfile::tempdir().unwrap();
    let shared_target = tmp.path().join("shared-target");

    let tree_a = tmp.path().join("tree-a");
    write_probe_crate(&tree_a, "TREE_A");
    let tree_b = tmp.path().join("tree-b");
    write_probe_crate(&tree_b, "TREE_B");

    raw_cargo_build(&tree_a, &shared_target);
    assert_eq!(run_probe(&shared_target), "TREE_A");

    // THE BUG, pinned: tree_b has never been built before — same package
    // identity, different absolute path, different content — but a plain,
    // unfenced `cargo build` in tree_b is judged FRESH against the artifact
    // tree_a just left in the same unit-hash slot.
    raw_cargo_build(&tree_b, &shared_target);
    assert_eq!(
        run_probe(&shared_target),
        "TREE_A",
        "pin: an unfenced cargo build in a never-before-built tree must reproduce \
         the laundering here — if this assertion starts failing, cargo's own \
         freshness check changed and target_dir.rs's premise needs re-verifying \
         against the new behaviour"
    );
}

/// THE FIX, proved end to end — including against a writer that is actively
/// compiling into the shared directory WHILE the verifier under test runs,
/// the exact shape the first (rejected) landing attempt's mtime-touch could
/// not close (task 44's second and third live incidents: a concurrent codex
/// session, and a concurrently building probe worktree, both racing a
/// verifier that had already "fenced" only once, at the start of its own
/// run).
#[test]
fn run_commands_isolates_cargo_from_a_concurrently_building_sibling() {
    run_owned_helper(
        "run_commands_isolates_cargo_from_a_concurrently_building_sibling_owned_process",
    );
}

#[test]
#[ignore = "run only in the process spawned by the parent test of the same name"]
fn run_commands_isolates_cargo_from_a_concurrently_building_sibling_owned_process() {
    assert_owned_helper(
        "run_commands_isolates_cargo_from_a_concurrently_building_sibling_owned_process",
    );
    let tmp = tempfile::tempdir().unwrap();
    let shared_target = helper_shared_target();

    let tree_a = tmp.path().join("tree-a");
    write_probe_crate(&tree_a, "TREE_A");
    let tree_b = tmp.path().join("tree-b");
    write_probe_crate(&tree_b, "TREE_B");

    raw_cargo_build(&tree_a, &shared_target);
    assert_eq!(run_probe(&shared_target), "TREE_A");

    // A sibling that never stops changing and recompiling tree_a into the SHARED
    // directory for as long as this test's verifier run takes — simulating
    // "a codex session was compiling into the shared target at that
    // moment" (task 44's second live incident) as closely as a unit test
    // reasonably can. The writer passes its target on its own Command.
    //
    // Bounded on both axes rather than a free spin: one cargo at a time, a
    // pause between rounds, and a hard iteration cap. This runs INSIDE
    // tier-0's own `cargo test` on a shared build host, and an unbounded
    // loop spawning cargo is the exact shape that OOMed this host once
    // already (see `verify::ensure_depth_budget`'s note — 840 concurrent
    // cargos, 2026-08-21). The cap is a backstop for the case where the
    // verifier run under test blocks far longer than expected (a contended
    // host lane, say); the assertions below do not depend on the writer
    // still being alive, only on it having hammered the shared directory
    // while the run was in flight.
    let stop = Arc::new(AtomicBool::new(false));
    let writer = {
        let stop = Arc::clone(&stop);
        let tree_a = tree_a.clone();
        let shared_target = shared_target.clone();
        std::thread::spawn(move || {
            for round in 1..=MAX_WRITER_ROUNDS {
                if stop.load(Ordering::Relaxed) {
                    break;
                }
                // Change compiled source every round. Merely rebuilding an
                // unchanged fresh crate would exercise repeated cargo
                // checks, not the active compilation race this test claims.
                write_probe_source(&tree_a, "TREE_A", round);
                raw_cargo_build(&tree_a, &shared_target);
                std::thread::sleep(std::time::Duration::from_millis(50));
            }
        })
    };

    let report = run_commands(
        "target-dir-probe",
        &[vec!["cargo".into(), "build".into(), "--quiet".into()]],
        &tree_b,
    );
    stop.store(true, Ordering::Relaxed);
    writer.join().unwrap();

    let report = report.unwrap();
    assert!(report.pass, "isolated cargo build must succeed: {report:?}");
    let private_dir = report
        .target_dir
        .as_deref()
        .expect("a cargo step must have recorded the resolved target dir");
    assert_ne!(
        private_dir,
        shared_target.to_str().unwrap(),
        "the report must name tree_b's OWN target dir, never the shared one \
         a concurrent sibling is actively writing to"
    );

    assert_eq!(
        run_probe(Path::new(private_dir)),
        "TREE_B",
        "tree_b's verifier run must execute a binary built from ITS OWN current \
         source, regardless of what a concurrent sibling did to the shared dir \
         at any point during the run"
    );
    // And the shared directory a concurrent sibling was hammering the whole
    // time is untouched by tree_b's run — proof the isolation is structural
    // (a directory tree_b's cargo never opens), not a timing race tree_b
    // happened to win.
    assert_eq!(run_probe(&shared_target), "TREE_A");
}

#[test]
fn each_step_reprobes_and_pins_target_after_an_earlier_step_rewrites_config() {
    let tmp = tempfile::tempdir().unwrap();
    let tree = tmp.path().join("tree");
    let shared_target = tmp.path().join("shared-target");
    write_test_probe_crate(
        &tree,
        "A",
        "#[test]\nfn owns_tree_a() { assert_eq!(TREE, \"A\"); }",
    );
    let writer = tree.join("rewrite-cargo-config");
    support::write_executable(
        &writer,
        "#!/bin/sh\nmkdir -p .cargo\nprintf '[build]\\ntarget-dir = \\\"%s\\\"\\n' \"$SHARED_TARGET\" > .cargo/config.toml\n",
    );
    let command = vec![
        "env".to_string(),
        format!("SHARED_TARGET={}", shared_target.display()),
        writer.to_string_lossy().into_owned(),
    ];

    let report = cosmix_foreman::verify::run_commands_with_landing_provenance(
        "config-toctou-probe",
        &[
            command,
            vec!["cargo".into(), "test".into(), "--quiet".into()],
        ],
        &tree,
    )
    .unwrap();

    assert!(report.pass, "pinned second step must pass: {report:?}");
    let private_target = tree.join("target").canonicalize().unwrap();
    assert_eq!(report.target_dir.as_deref(), private_target.to_str());
    let probe_artifact = std::fs::read_dir(private_target.join("debug/deps"))
        .unwrap()
        .filter_map(Result::ok)
        .find(|entry| {
            entry
                .file_name()
                .to_string_lossy()
                .starts_with("foreman_tier_probe-")
                && entry.path().is_file()
        });
    assert!(
        probe_artifact.is_some(),
        "the fixture's test executable must exist below the reported private target"
    );
    assert!(
        !shared_target.exists(),
        "the config rewrite must not make the second step touch the shared target"
    );
}

#[test]
fn unchanged_test_artifacts_have_complete_pre_and_post_provenance() {
    let tmp = tempfile::tempdir().unwrap();
    let tree = tmp.path().join("tree");
    write_test_probe_crate(
        &tree,
        "A",
        "#[test]\nfn unchanged() { assert_eq!(TREE, \"A\"); }",
    );

    let report = cosmix_foreman::verify::run_commands_with_landing_provenance(
        "unchanged-provenance",
        &[vec!["cargo".into(), "test".into(), "--quiet".into()]],
        &tree,
    )
    .unwrap();

    assert!(report.pass, "fixture test must pass: {report:?}");
    assert!(matches!(
        report.steps[0].executed_binaries,
        Some(cosmix_foreman::provenance::BinaryProvenance::Complete { ref binaries })
            if !binaries.is_empty()
    ));
    assert_eq!(report.provenance_tier, Some(1));
    let json = serde_json::to_string(&report).unwrap();
    assert_eq!(
        json.matches("executed_binaries").count(),
        1,
        "landing report must carry executable provenance exactly once: {json}"
    );
}

#[test]
fn test_replacing_its_own_executable_makes_provenance_unavailable() {
    let tmp = tempfile::tempdir().unwrap();
    let tree = tmp.path().join("tree");
    write_test_probe_crate(
        &tree,
        "A",
        r#"#[test]
fn replaces_itself() {
    use std::io::Write as _;
    let executable = std::env::current_exe().unwrap();
    let replacement = executable.with_extension("replacement");
    std::fs::copy(&executable, &replacement).unwrap();
    std::fs::OpenOptions::new().append(true).open(&replacement).unwrap()
        .write_all(b"changed").unwrap();
    std::fs::rename(replacement, executable).unwrap();
}"#,
    );

    let report = cosmix_foreman::verify::run_commands_with_landing_provenance(
        "self-replacing-provenance",
        &[vec!["cargo".into(), "test".into(), "--quiet".into()]],
        &tree,
    )
    .unwrap();

    assert!(report.pass, "the test itself succeeds: {report:?}");
    let Some(cosmix_foreman::provenance::BinaryProvenance::Unavailable { reason }) =
        &report.steps[0].executed_binaries
    else {
        panic!("replaced executable must not produce complete provenance: {report:?}");
    };
    assert!(
        reason.contains("artifacts changed during the step"),
        "{reason}"
    );
    assert!(
        reason.contains("before=") && reason.contains("after="),
        "{reason}"
    );
}

#[test]
fn test_changing_compiled_input_before_post_listing_makes_provenance_unavailable() {
    let tmp = tempfile::tempdir().unwrap();
    let tree = tmp.path().join("tree");
    write_test_probe_crate(
        &tree,
        "before",
        r#"#[test]
fn changes_its_input() {
    assert_eq!(TREE, "before");
    let source = std::fs::read_to_string("src/lib.rs").unwrap();
    std::fs::write("src/lib.rs", source.replace("before", "after!")).unwrap();
}"#,
    );

    let report = cosmix_foreman::verify::run_commands_with_landing_provenance(
        "input-changing-provenance",
        &[vec!["cargo".into(), "test".into(), "--quiet".into()]],
        &tree,
    )
    .unwrap();

    assert!(report.pass, "the pre-change test succeeds: {report:?}");
    let Some(cosmix_foreman::provenance::BinaryProvenance::Unavailable { reason }) =
        &report.steps[0].executed_binaries
    else {
        panic!("rebuilt executable must not produce complete provenance: {report:?}");
    };
    assert!(
        reason.contains("artifacts changed during the step"),
        "{reason}"
    );
    assert!(
        reason.contains("before=") && reason.contains("after="),
        "{reason}"
    );
}

#[test]
fn tier_zero_foreman_runs_tree_a_bytes_and_the_unpinned_layout_runs_tree_b() {
    let tmp = tempfile::tempdir().unwrap();
    let tree_a = tmp.path().join("tree-a");
    let tree_b = tmp.path().join("tree-b");
    let shared_target = tmp.path().join("shared-target");
    let test_body = r#"#[test]
fn expected_tree() {
    assert_eq!(TREE, std::env::var("EXPECTED_TREE").unwrap());
}"#;
    write_test_probe_crate(&tree_a, "A", test_body);
    write_test_probe_crate(&tree_b, "B", test_body);
    raw_cargo_test_no_run(&tree_b, &shared_target);

    let fixed = Command::new(env!("CARGO_BIN_EXE_foreman"))
        .args([
            "verify",
            "--dir",
            tree_a.to_str().unwrap(),
            "--profile",
            "rust",
            "--tier",
            "0",
        ])
        .env("CARGO_TARGET_DIR", &shared_target)
        .env("EXPECTED_TREE", "A")
        .env("FOREMAN_VERIFY_LANE", tmp.path().join("verify.lock"))
        .env("FOREMAN_VERIFY_LANE_WAIT_SECS", "30")
        .output()
        .unwrap();
    assert!(
        fixed.status.success(),
        "pinned foreman tier 0 must execute tree A's test bytes:\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&fixed.stdout),
        String::from_utf8_lossy(&fixed.stderr)
    );
    assert!(tree_a.join("target/debug/deps").is_dir());

    let tier0 = cosmix_foreman::verify::profile_commands("rust").unwrap();
    for argv in &tier0[..2] {
        let status = Command::new(&argv[0])
            .args(&argv[1..])
            .current_dir(&tree_a)
            .env("CARGO_TARGET_DIR", &shared_target)
            .env("EXPECTED_TREE", "A")
            .status()
            .unwrap();
        assert!(
            status.success(),
            "legacy tier-0 setup command failed: {argv:?}"
        );
    }
    // A sibling write between the old tier's steps restores tree B's bytes
    // to the colliding slot. This is the old shared-target layout with the
    // strip/pin disabled, driven by the exact tier-0 command vector.
    raw_cargo_test_no_run(&tree_b, &shared_target);
    let test = &tier0[2];
    let unfenced = Command::new(&test[0])
        .args(&test[1..])
        .current_dir(&tree_a)
        .env("CARGO_TARGET_DIR", &shared_target)
        .env("EXPECTED_TREE", "A")
        .output()
        .unwrap();
    assert!(
        !unfenced.status.success(),
        "pin regression: the old shared-target tier-0 test must execute tree B and fail; stdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&unfenced.stdout),
        String::from_utf8_lossy(&unfenced.stderr)
    );
    let combined = format!(
        "{}\n{}",
        String::from_utf8_lossy(&unfenced.stdout),
        String::from_utf8_lossy(&unfenced.stderr)
    );
    assert!(
        combined.contains("left: \"B\"") && combined.contains("right: \"A\""),
        "{combined}"
    );
}

#[test]
fn run_commands_pin_outweighs_a_committed_shared_target_config() {
    run_owned_helper("run_commands_pin_outweighs_a_committed_shared_target_config_owned_process");
}

#[test]
#[ignore = "run only in the process spawned by the parent test of the same name"]
fn run_commands_pin_outweighs_a_committed_shared_target_config_owned_process() {
    assert_owned_helper(
        "run_commands_pin_outweighs_a_committed_shared_target_config_owned_process",
    );
    let tmp = tempfile::tempdir().unwrap();
    let shared_target = helper_shared_target();
    let tree = tmp.path().join("tree");
    write_probe_crate(&tree, "TREE");
    std::fs::create_dir_all(tree.join(".cargo")).unwrap();
    std::fs::write(
        tree.join(".cargo/config.toml"),
        format!(
            "[build]\ntarget-dir = {:?}\n",
            shared_target.to_str().unwrap()
        ),
    )
    .unwrap();

    let report = run_commands(
        "target-dir-probe",
        &[vec!["cargo".into(), "build".into(), "--quiet".into()]],
        &tree,
    )
    .unwrap();

    assert!(
        report.pass,
        "the verifier-owned environment pin must win: {report:?}"
    );
    assert_eq!(report.target_dir.as_deref(), tree.join("target").to_str());
    assert!(!shared_target.exists());
}

#[test]
fn run_commands_preflights_the_real_target_dir_argument_before_running() {
    let tmp = tempfile::tempdir().unwrap();
    let shared_target = tmp.path().join("shared-target");
    let tree = tmp.path().join("tree");
    write_probe_crate(&tree, "TREE");
    let command = vec![
        "cargo".into(),
        "test".into(),
        "--target-dir".into(),
        shared_target.to_string_lossy().into_owned(),
    ];

    let result = run_commands("explicit-target-probe", &[command], &tree);

    let err = result.unwrap_err().to_string();
    assert!(
        err.contains("Isolation cannot be established"),
        "the real command's outside --target-dir must fail closed: {err}"
    );
    assert!(
        !shared_target.exists(),
        "the verifier must refuse before the unsafe cargo command creates its target"
    );
}

#[test]
fn transparent_wrapper_inherits_the_pinned_env_and_builds_privately() {
    run_owned_helper(
        "transparent_wrapper_inherits_the_pinned_env_and_builds_privately_owned_process",
    );
}

#[test]
#[ignore = "run only in the process spawned by the parent test of the same name"]
fn transparent_wrapper_inherits_the_pinned_env_and_builds_privately_owned_process() {
    assert_owned_helper(
        "transparent_wrapper_inherits_the_pinned_env_and_builds_privately_owned_process",
    );
    let tmp = tempfile::tempdir().unwrap();
    let shared_target = helper_shared_target();
    let tree = tmp.path().join("tree");
    write_probe_crate(&tree, "WRAPPED");

    let report = run_commands(
        "wrapped-target-dir-probe",
        &[vec![
            "env".into(),
            format!("CARGO_TARGET_DIR={}", shared_target.display()),
            "cargo".into(),
            "build".into(),
            "--quiet".into(),
        ]],
        &tree,
    );

    let report = report.unwrap();
    assert!(report.pass, "wrapped cargo build must pass: {report:?}");
    let private_dir = tree.join("target");
    assert_eq!(run_probe(&private_dir), "WRAPPED");
    assert!(
        !shared_target.exists(),
        "the wrapper's cargo must never inherit or create the shared target"
    );
    assert_eq!(
        report.target_dir.as_deref(),
        Some(private_dir.canonicalize().unwrap().to_str().unwrap()),
        "the transparent wrapper preflight must record its workspace-private default"
    );
}

#[test]
fn opaque_shell_wrapper_is_refused_before_it_runs() {
    let tmp = tempfile::tempdir().unwrap();
    let tree = tmp.path().join("tree");
    write_probe_crate(&tree, "OPAQUE");
    let marker = tree.join("must-not-run");
    let command = format!("touch {} && cargo test", marker.display());

    let error = run_commands(
        "opaque-target-dir-probe",
        &[vec!["sh".into(), "-c".into(), command]],
        &tree,
    )
    .unwrap_err()
    .to_string();

    assert!(error.contains("opaque verifier command"), "{error}");
    assert!(
        !marker.exists(),
        "opaque command must be refused before execution"
    );
}

#[test]
fn verify_cli_prints_target_and_executed_binary_sha256() {
    let tmp = tempfile::tempdir().unwrap();
    let tree = tmp.path().join("tree");
    write_probe_crate(&tree, "PROVENANCE");
    std::fs::write(
        tree.join("src/main.rs"),
        "fn main() {\n    let _build_round = 0;\n    println!(\"PROVENANCE\");\n}\n",
    )
    .unwrap();
    let shared_target = tmp.path().join("shared-target");
    // Tier 1 conditionally runs cargo-deny when it is on PATH. This fixture
    // is about CLI provenance, not the host's advisory database, so give the
    // child a complete Rust tool PATH without cargo-deny.
    // Keep this tool shim outside `/tmp`: the managed test sandbox places a
    // synthetic `/tmp/.git`, so trusted-cargo containment deliberately treats
    // all of `/tmp` as the fixture checkout.
    let bin_parent = tempfile::Builder::new()
        .prefix("foreman-cli-tools-")
        .tempdir_in(env!("CARGO_TARGET_TMPDIR"))
        .unwrap();
    let bin = bin_parent.path().join("bin");
    std::fs::create_dir(&bin).unwrap();
    for (name, source) in [
        ("cargo", "/usr/bin/cargo"),
        ("rustc", "/usr/bin/rustc"),
        ("rustdoc", "/usr/bin/rustdoc"),
        ("cargo-fmt", "/usr/bin/cargo-fmt"),
        ("cargo-clippy", "/usr/bin/cargo-clippy"),
        ("rustfmt", "/usr/bin/rustfmt"),
        ("clippy-driver", "/usr/bin/clippy-driver"),
        ("git", "/usr/bin/git"),
        ("timeout", "/usr/bin/timeout"),
        ("cc", "/usr/bin/cc"),
    ] {
        std::os::unix::fs::symlink(source, bin.join(name)).unwrap();
    }

    for run in ["cold", "second warm"] {
        let output = Command::new(env!("CARGO_BIN_EXE_foreman"))
            .args([
                "verify",
                "--dir",
                tree.to_str().unwrap(),
                "--profile",
                "rust",
                "--tier",
                "1",
            ])
            .env("CARGO_TARGET_DIR", &shared_target)
            .env("FOREMAN_VERIFY_LANE", tmp.path().join("verify.lock"))
            .env("FOREMAN_VERIFY_LANE_WAIT_SECS", "30")
            .env("PATH", &bin)
            .env("RUSTC_WRAPPER", "")
            .output()
            .unwrap();

        assert!(
            output.status.success(),
            "{run} verify failed:\nstdout:\n{}\nstderr:\n{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
        let stdout = String::from_utf8_lossy(&output.stdout);
        assert!(
            stdout.contains(&format!(
                "target directory: {}",
                tree.join("target").display()
            )),
            "{run}: {stdout}"
        );
        assert!(
            stdout.contains("executable provenance: tier 1 (one principal test step)"),
            "{run} report must identify the provenance-carrying tier:\n{stdout}"
        );
        assert!(
            stdout.contains("executed binary: debug/deps/foreman_target_dir_probe-")
                && stdout.contains(" sha256:"),
            "{run} report must show the executed binary and hash:\n{stdout}"
        );
    }
    assert!(
        !shared_target.exists(),
        "CLI verifier must keep using the private target"
    );
}

/// Item 3 of task 80: the three scenarios above used to share one
/// process-wide environment lock. Each now runs in a process of its own, so
/// all three must pass at the same time — each seeing its own fixture and
/// none observing another's ambient variable.
#[test]
fn formerly_conflicting_scenarios_run_concurrently_each_in_their_own_process() {
    let names = [
        "run_commands_isolates_cargo_from_a_concurrently_building_sibling_owned_process",
        "run_commands_pin_outweighs_a_committed_shared_target_config_owned_process",
        "transparent_wrapper_inherits_the_pinned_env_and_builds_privately_owned_process",
    ];
    let mut joins = Vec::new();
    for name in names {
        joins.push(std::thread::spawn(move || run_owned_helper(name)));
    }
    for join in joins {
        join.join().expect("concurrent scenario thread panicked");
    }
}
