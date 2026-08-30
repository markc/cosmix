//! Tier-structure and review-verdict tests. No cargo runs, no tokens.

use cosmix_foreman::config::FleetPolicy;
use cosmix_foreman::review::{ChangedFile, ReviewSeverity, parse_review_response};
use cosmix_foreman::verify::{
    lookup_profile, run_commands, tier_commands, tier_commands_in_dir_with_policy,
};

#[test]
fn tier_shapes() {
    // Tier 0: the three fast gates.
    let t0 = tier_commands("rust", 0).unwrap();
    assert_eq!(t0.len(), 3);

    // Tier 1: fmt + clippy + WORKSPACE tests (replacing the crate-level
    // test step, not duplicating the suite); cargo-deny joins when
    // installed — host-dependent, so assert the floor.
    let t1 = tier_commands("rust", 1).unwrap();
    assert!(t1.len() >= 3, "{t1:?}");
    assert!(t1[..2] == t0[..2], "tier 1 keeps fmt + clippy");
    assert!(
        t1.iter().any(|c| c.contains(&"--workspace".to_string())),
        "{t1:?}"
    );
    assert_eq!(
        t1.iter()
            .filter(|c| c.contains(&"test".to_string()))
            .count(),
        1,
        "one test step, not a duplicated suite: {t1:?}"
    );

    // The opt-out profile is empty at every tier; unknown tiers and
    // profiles are errors.
    for tier in [0, 1, 2] {
        assert!(tier_commands("none", tier).unwrap().is_empty());
    }
    assert!(tier_commands("rust", 7).is_err());
    assert!(tier_commands("yolo", 1).is_err());
}

#[test]
fn rust_tier1_without_a_directory_reports_an_unknown_feature_dimension() {
    let commands = tier_commands("rust", 1).unwrap();
    assert!(
        commands.iter().any(|command| {
            command.first().map(String::as_str) == Some("foreman-verify-gap")
                && command.contains(&"feature-coverage-undiscoverable".to_string())
        }),
        "a directory-free command list must not claim feature coverage: {commands:?}"
    );
}

#[test]
fn an_explicitly_empty_feature_set_is_a_failing_report_step() {
    let fixture = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("testdata")
        .join("feature-fixture");
    let mut policy = FleetPolicy::defaults();
    policy.feature_sets.value = Some(String::new());
    let commands = tier_commands_in_dir_with_policy("rust", 1, &fixture, &policy).unwrap();
    let gap = commands
        .iter()
        .find(|command| command.first().map(String::as_str) == Some("foreman-verify-gap"))
        .expect("empty configured coverage must emit a gap")
        .clone();
    let report = run_commands("empty-feature-set", &[gap], &fixture).unwrap();
    assert!(!report.pass, "the gap step must fail, not merely warn");
    assert!(
        report.failure_digest().contains("entry #1 is empty"),
        "the persisted report should explain the gap: {report:?}"
    );
}

#[test]
fn configured_feature_sets_are_strict_and_crate_scoped() {
    let fixture = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("testdata")
        .join("feature-fixture");
    let mut policy = FleetPolicy::defaults();
    policy.feature_sets.value = Some("fixture-feature-gap:broken".into());
    let commands = tier_commands_in_dir_with_policy("rust", 1, &fixture, &policy).unwrap();
    assert!(commands.contains(&vec![
        "cargo".into(),
        "test".into(),
        "-p".into(),
        "fixture-feature-gap".into(),
        "--features".into(),
        "broken".into(),
    ]));

    policy.feature_sets.value = Some("fixture-feature-gap:".into());
    let malformed = tier_commands_in_dir_with_policy("rust", 1, &fixture, &policy).unwrap();
    assert!(malformed.iter().any(|command| {
        command.first().map(String::as_str) == Some("foreman-verify-gap")
            && command.contains(&"feature-coverage-misconfigured".to_string())
    }));

    policy.feature_sets.value = Some("fixture-feature-gap:unknown".into());
    let unknown = tier_commands_in_dir_with_policy("rust", 1, &fixture, &policy).unwrap();
    assert!(
        unknown.iter().any(|command| {
            command.first().map(String::as_str) == Some("foreman-verify-gap")
                && command
                    .iter()
                    .any(|arg| arg.contains("unknown") && arg.contains("not declared"))
        }),
        "an unknown configured feature must be a red gap: {unknown:?}"
    );
}

#[test]
fn review_json_parsing_and_inspection_coverage_fail_closed() {
    let changed = [ChangedFile {
        path: "src/lib.rs".into(),
        additions: Some(2),
        deletions: Some(1),
        hunks: 1,
    }];
    let approved = parse_review_response(
        r#"analysis
{"verdict":"APPROVE","findings":[],"files_inspected":["src/lib.rs"]}"#,
        &changed,
    )
    .unwrap();
    assert!(approved.approve);

    assert!(parse_review_response("looks fine\nVERDICT: APPROVE", &changed).is_err());
    let uncovered = parse_review_response(
        r#"{"verdict":"APPROVE","findings":[],"files_inspected":[]}"#,
        &changed,
    )
    .unwrap();
    assert!(!uncovered.approve);
    assert_eq!(uncovered.findings[0].severity, ReviewSeverity::Major);
}

/// The compositor profile (task 32) owns `desktop/` as its cwd — a
/// SEPARATE cargo workspace from the one `--subdir` normally points the
/// `rust` profile at — and every step is scoped to `cosmix-comp`, never a
/// blanket `--all-features`/`--workspace` across the whole `desktop/` tree
/// (mesh/citizen crates like trayd/mail/tower fail there for unrelated
/// reasons — fleet task 15's conclusion, restated here as an invariant).
#[test]
fn compositor_profile_owns_desktop_and_never_touches_the_rest_of_the_workspace() {
    let p = lookup_profile("compositor").unwrap();
    assert_eq!(p.name, "compositor");
    assert_eq!(p.cwd.as_deref(), Some("desktop"));

    for tier in [0, 1] {
        let cmds = tier_commands("compositor", tier).unwrap();
        assert!(
            !cmds.is_empty(),
            "tier {tier} must not silently pass: {cmds:?}"
        );
        for cmd in &cmds {
            assert!(
                !cmd.contains(&"--workspace".to_string())
                    && !cmd.contains(&"--all-features".to_string()),
                "compositor tier {tier} must stay scoped to cosmix-comp, \
                 never the whole desktop/ workspace: {cmd:?}"
            );
        }
    }
}

/// `kms-live` is non-default (`default = ["frame-capture"]`) — a gate that
/// never names it compiles none of it, the exact hole that let a 904-line
/// driver pass a gate that never built it. Pin that the profile actually
/// enables it, twice: once under clippy (reaches the `not(test)`-only
/// arms `--all-targets` alone cannot) and once under a real, non-test
/// `cargo build` (proves codegen, not just type-checking).
#[test]
fn compositor_tier0_compiles_kms_live_not_just_default_features() {
    let t0 = tier_commands("compositor", 0).unwrap();

    let clippy_step = t0
        .iter()
        .find(|c| c.contains(&"clippy".to_string()))
        .unwrap_or_else(|| panic!("no clippy step: {t0:?}"));
    assert!(
        clippy_step.iter().any(|a| a.contains("kms-live")),
        "clippy must enable kms-live to reach its not(test) arms: {clippy_step:?}"
    );
    assert!(
        clippy_step
            .iter()
            .any(|a| a.contains("explicit-sync-live-test")),
        "clippy must also enable explicit-sync-live-test, or its 20 gated \
         sites in protocol/tests.rs sit uncompiled and unverified: {clippy_step:?}"
    );

    let build_steps: Vec<_> = t0
        .iter()
        .filter(|c| c.contains(&"build".to_string()))
        .collect();
    assert!(
        build_steps
            .iter()
            .any(|c| c.contains(&"--features".to_string()) && c.contains(&"kms-live".to_string())),
        "a plain (non-clippy) cargo build must also enable kms-live, proving \
         the real binary's codegen path compiles, not just clippy's type-check: {t0:?}"
    );

    // Every step targets cosmix-comp specifically (except the workspace-level
    // [patch] assertion, which is unscoped by design — it must see the whole
    // desktop/ workspace to resolve the patched smithay at all).
    for cmd in &t0 {
        let is_patch_assertion = cmd.contains(&"tree".to_string());
        if is_patch_assertion {
            continue;
        }
        assert!(
            cmd.iter().any(|a| a.contains("cosmix-comp")),
            "non-assertion step must target cosmix-comp: {cmd:?}"
        );
    }
}

/// `desktop/Cargo.toml` patches `smithay` (and `wgpu`/`wgpu-core`) to
/// vendored forks via `[patch.crates-io]`. An unsatisfied patch is only a
/// cargo WARNING — silently falling back to whatever crates.io last
/// published under that name — so the profile must assert it resolves
/// rather than hope.
#[test]
fn compositor_tier0_asserts_the_smithay_patch_resolves() {
    let t0 = tier_commands("compositor", 0).unwrap();
    assert!(
        t0[0].contains(&"tree".to_string())
            && t0[0].contains(&"-i".to_string())
            && t0[0].contains(&"smithay".to_string()),
        "first step must assert the smithay patch resolves: {t0:?}"
    );
}

/// Tier 1 is IDENTICAL to tier 0 for this profile — unlike `rust`'s tier 1,
/// nothing safe exists to widen to: `--workspace` would hit the rest of
/// `desktop/` (forbidden, same reason as the `--all-features` ban), and
/// `cargo deny check` has no per-crate scoping AND no policy file to run
/// against — `src/deny.toml` exists, `desktop/deny.toml` does not, so
/// cargo-deny falls back to its defaults and reports 748 license
/// rejections plus 6 advisories workspace-wide (measured). Wiring it in
/// would make this tier permanently red on any host with cargo-deny
/// installed, for a policy gap this crate does not own.
#[test]
fn compositor_tier1_is_identical_to_tier0() {
    let t0 = tier_commands("compositor", 0).unwrap();
    let t1 = tier_commands("compositor", 1).unwrap();
    assert_eq!(t0, t1, "tier 1 must not widen scope for this profile");
    assert_eq!(
        t1.iter()
            .filter(|c| c.contains(&"test".to_string()))
            .count(),
        1,
        "one test step, not a duplicated suite: {t1:?}"
    );
}

#[test]
fn compositor_unknown_profile_and_tier_are_refused() {
    assert!(lookup_profile("comp").is_err(), "no silent alias");
    assert!(tier_commands("compositor", 7).is_err());
}

/// A `--features` flag with an empty value compiles exactly the default
/// feature set while still *reading* as non-default coverage — the quiet
/// failure the task calls out. Pin that every feature argument the profile
/// emits actually names something, and that scoping is by package (`-p`),
/// which cannot go stale if the crate moves inside `desktop/`.
#[test]
fn compositor_feature_args_are_never_empty_and_scoping_is_by_package() {
    let t0 = tier_commands("compositor", 0).unwrap();
    let mut saw_features = 0;
    for cmd in &t0 {
        for (i, arg) in cmd.iter().enumerate() {
            if arg == "--features" {
                let val = cmd.get(i + 1).unwrap_or_else(|| {
                    panic!("--features with no value would silently mean default: {cmd:?}")
                });
                assert!(
                    !val.trim().is_empty() && val.split(',').all(|f| !f.trim().is_empty()),
                    "empty feature name claims coverage it does not have: {cmd:?}"
                );
                saw_features += 1;
            }
        }
        assert!(
            !cmd.iter().any(|a| a == "--manifest-path"),
            "scope by package, not a hard-coded manifest path: {cmd:?}"
        );
    }
    assert_eq!(
        saw_features, 2,
        "clippy and build both carry features: {t0:?}"
    );
}

/// Tier 2 is the operator-defined nightly surface, so merely putting the
/// physical command behind a conspicuous subcommand is not enough: an
/// accidental `FOREMAN_TIER2_COMMANDS` entry must still fail before cargo,
/// the compositor, a VT or KMS is touched.
#[test]
fn nightly_tier_cannot_select_physical_acceptance() {
    let temp = tempfile::tempdir().unwrap();
    let repo = temp.path().join("repo");
    std::fs::create_dir_all(repo.join("desktop")).unwrap();
    let foreman = env!("CARGO_BIN_EXE_foreman");
    let nested_db = temp.path().join("nested.db");
    let physical = format!(
        "{foreman} --db {} physical-acceptance --dir {} --device /dev/dri/card0 \
         --connector DP-1 --max-secs 1 --take-vt-and-display",
        nested_db.display(),
        repo.display()
    );

    let output = std::process::Command::new(foreman)
        .arg("--db")
        .arg(temp.path().join("outer.db"))
        .args(["verify", "--profile", "compositor", "--tier", "2", "--dir"])
        .arg(&repo)
        .env("FOREMAN_TIER2_COMMANDS", physical)
        .env("FOREMAN_VERIFY_LANE", temp.path().join("verify.lock"))
        .env("FOREMAN_VERIFY_LANE_WAIT_SECS", "30")
        // This test owns no cargo work; skip the unrelated host-wide cargo
        // lane so an external verifier cannot make the regression hang.
        .env(cosmix_foreman::verify::LANE_HELD_ENV, "1")
        .output()
        .expect("running the headless nightly verifier");

    assert!(
        !output.status.success(),
        "the nightly tier must refuse, never run, physical acceptance"
    );
    let transcript = format!(
        "{}\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(
        transcript.contains(
            "physical acceptance is unavailable from a HEADLESS verifier or nightly tier"
        ),
        "the refusal must name the headless/physical boundary: {transcript}"
    );
    assert!(
        transcript.contains("HEADLESS UNCOVERED kms-live"),
        "even a red compositor report must carry its physical coverage boundary: {transcript}"
    );
    assert!(
        !transcript.contains("Compiling ") && !transcript.contains("kms-live tracing"),
        "refusal must happen before cargo or the compositor starts: {transcript}"
    );
}

/// The profile's owned `cwd` is the whole point of building on task 29's
/// mechanism rather than reinventing one: `desktop/` must win over any
/// `--subdir` the caller passes, and a worktree without a `desktop/` must
/// fail LOUDLY rather than quietly verify the repo root (which would be the
/// `src/` workspace — green for the wrong reason, the exact "coverage it
/// does not have" failure this task exists to prevent).
#[test]
fn compositor_cwd_beats_subdir_and_a_missing_desktop_is_loud() {
    let root = tempfile::tempdir().unwrap();
    std::fs::create_dir(root.path().join("desktop")).unwrap();
    std::fs::create_dir(root.path().join("src")).unwrap();
    let p = lookup_profile("compositor").unwrap();

    for subdir in [None, Some("src"), Some(".")] {
        let dir = p.resolve_cwd(root.path(), subdir).unwrap();
        assert_eq!(
            dir,
            root.path().canonicalize().unwrap().join("desktop"),
            "--subdir {subdir:?} must not divert the compositor profile"
        );
    }

    let bare = tempfile::tempdir().unwrap();
    assert!(
        p.resolve_cwd(bare.path(), Some("src")).is_err(),
        "no desktop/ must be an error, not a silent fallback to the repo root"
    );
}
