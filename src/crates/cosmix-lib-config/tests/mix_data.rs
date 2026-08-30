//! Smoke tests for the strict-data Mix loader exposed by
//! `cosmix-lib-config`. Confirms the public surface delegates correctly
//! to `cosmix-lib-mix` and that file-not-found surfaces as a runtime
//! error rather than panicking.

use cosmix_config::{load_mix_data, parse_mix_data};
use cosmix_mix::value::Value;
use std::path::Path;

#[test]
fn parse_mix_data_accepts_minimal_map() {
    let v = parse_mix_data("name: \"alpha\"\npriority: 2\n").unwrap();
    let Value::Map(m) = &v else {
        panic!("expected map")
    };
    assert_eq!(m.get("name"), Some(&Value::String("alpha".into())));
    assert_eq!(m.get("priority"), Some(&Value::Number(2.0)));
}

#[test]
fn parse_mix_data_rejects_executable_construct() {
    let err = parse_mix_data("name: \"hi ${user}\"\n").unwrap_err();
    let msg = format!("{err}");
    assert!(msg.contains("Strict-data violation"), "got: {msg}");
}

#[test]
fn load_mix_data_reads_fixture() {
    let path = manifest_dir().join("tests/fixtures/sample.spec.mix");
    let v = load_mix_data(&path).unwrap();
    let Value::Map(m) = &v else {
        panic!("expected map")
    };
    assert_eq!(m.get("name"), Some(&Value::String("sample".into())));
    assert_eq!(m.get("priority"), Some(&Value::Number(3.0)));
}

#[test]
fn load_mix_data_missing_file_is_runtime_error() {
    let err = load_mix_data(Path::new("/nonexistent/path.spec.mix")).unwrap_err();
    let msg = format!("{err}");
    assert!(msg.contains("Runtime error"), "got: {msg}");
}

/// Run-time manifest directory rather than the `env!`-baked one: cargo exports
/// `CARGO_MANIFEST_DIR` into the test process, and that names the tree cargo is
/// actually running in, whereas `env!` records whichever tree last *compiled*
/// the binary. The two diverge when one `CARGO_TARGET_DIR` is shared across
/// several git worktrees of this repo — cargo writes workspace-relative paths
/// into its dep-info, so an artefact built in a sibling worktree is judged
/// fresh and rerun here, still pointing at that tree's fixtures. Falls back to
/// the compile-time value when the binary is run outside cargo.
fn manifest_dir() -> std::path::PathBuf {
    std::env::var_os("CARGO_MANIFEST_DIR")
        .map(std::path::PathBuf::from)
        .unwrap_or_else(|| std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR")))
}
