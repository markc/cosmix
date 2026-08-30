//! Integration tests for the `cosmix-mds` operator binary.
//!
//! Phase 5.1 covers the operator contract that every later
//! subcommand inherits:
//!
//! - root resolution (`--root` flag, `$COSMIX_MDS_ROOT` env, error
//!   when neither is set),
//! - human vs `--json` output discipline (stdout = result only,
//!   stderr = errors),
//! - exit codes (0 success, 2 failure),
//! - `list-sets` output shape in both modes.
//!
//! These tests *spawn the real binary* via `assert_cmd` so the
//! contract is locked at the operator interface, not at the
//! `run()` helper. The unit tests in `src/bin/cosmix-mds.rs`
//! cover `resolve_root` in isolation.

use assert_cmd::Command;
use cosmix_mds::{ContainerAttrs, Flags, Mds, Membership, SetId, SqliteCasMds};
use predicates::prelude::*;
use tempfile::TempDir;

fn fresh_root() -> TempDir {
    TempDir::new().unwrap()
}

fn cmd() -> Command {
    let mut c = Command::cargo_bin("cosmix-mds").unwrap();
    // Strip the env so a developer's exported COSMIX_MDS_ROOT
    // never bleeds into the test process and accidentally
    // satisfies the "missing root" assertion.
    c.env_remove("COSMIX_MDS_ROOT");
    c
}

fn attrs() -> ContainerAttrs {
    ContainerAttrs {
        special_use: None,
        subscribed: true,
        extra: serde_json::json!({}),
    }
}

#[test]
fn missing_root_errors_with_exit_code_2_and_helpful_message() {
    cmd()
        .arg("list-sets")
        .assert()
        .code(2)
        .stderr(predicate::str::contains("--root"))
        .stderr(predicate::str::contains("COSMIX_MDS_ROOT"));
}

#[test]
fn root_arg_is_honored() {
    let d = fresh_root();
    SqliteCasMds::open(d.path()).unwrap();

    cmd()
        .arg("--root")
        .arg(d.path())
        .arg("list-sets")
        .assert()
        .code(0)
        .stdout("");
}

#[test]
fn root_env_var_is_honored() {
    let d = fresh_root();
    SqliteCasMds::open(d.path()).unwrap();

    cmd()
        .env("COSMIX_MDS_ROOT", d.path())
        .arg("list-sets")
        .assert()
        .code(0)
        .stdout("");
}

#[test]
fn root_arg_overrides_env_var() {
    // Env points at a path that would error if used (no parent
    // dir creation here); arg points at a real root. If the
    // priority is wrong, this fails on the env's bogus path.
    let real = fresh_root();
    SqliteCasMds::open(real.path()).unwrap();

    cmd()
        .env("COSMIX_MDS_ROOT", "/nonexistent/should-not-be-touched")
        .arg("--root")
        .arg(real.path())
        .arg("list-sets")
        .assert()
        .code(0);
}

#[test]
fn list_sets_human_output_lists_uuids_one_per_line() {
    let d = fresh_root();
    let mds = SqliteCasMds::open(d.path()).unwrap();
    let s1 = SetId(uuid::Uuid::now_v7());
    let s2 = SetId(uuid::Uuid::now_v7());
    mds.create_set(&s1).unwrap();
    mds.create_set(&s2).unwrap();
    drop(mds);

    let out = cmd()
        .arg("--root")
        .arg(d.path())
        .arg("list-sets")
        .output()
        .unwrap();
    assert!(out.status.success(), "exit code: {:?}", out.status.code());
    let stdout = String::from_utf8(out.stdout).unwrap();
    let lines: Vec<&str> = stdout.lines().collect();
    assert_eq!(lines.len(), 2, "expected 2 lines, got {stdout:?}");
    let s1s = s1.0.to_string();
    let s2s = s2.0.to_string();
    assert!(lines.iter().any(|l| *l == s1s));
    assert!(lines.iter().any(|l| *l == s2s));
}

#[test]
fn list_sets_json_output_round_trips_via_serde() {
    let d = fresh_root();
    let mds = SqliteCasMds::open(d.path()).unwrap();
    let s1 = SetId(uuid::Uuid::now_v7());
    let s2 = SetId(uuid::Uuid::now_v7());
    mds.create_set(&s1).unwrap();
    mds.create_set(&s2).unwrap();
    // Make sure a non-empty set is also present (a set with a
    // container) — list-sets shouldn't care, but it pins the
    // contract that container content doesn't leak into the JSON.
    mds.create_container(&s1, None, "INBOX", attrs()).unwrap();
    drop(mds);

    let out = cmd()
        .arg("--root")
        .arg(d.path())
        .arg("--json")
        .arg("list-sets")
        .output()
        .unwrap();
    assert!(out.status.success(), "exit code: {:?}", out.status.code());

    #[derive(serde::Deserialize)]
    struct Resp {
        sets: Vec<String>,
    }
    let resp: Resp = serde_json::from_slice(&out.stdout).expect("--json output must parse as JSON");
    assert_eq!(resp.sets.len(), 2);
    let s1s = s1.0.to_string();
    let s2s = s2.0.to_string();
    assert!(resp.sets.contains(&s1s));
    assert!(resp.sets.contains(&s2s));
}

#[test]
fn migrate_all_on_empty_root_reports_zero_sets() {
    let d = fresh_root();

    let out = cmd()
        .arg("--root")
        .arg(d.path())
        .arg("migrate-all")
        .output()
        .unwrap();
    assert!(out.status.success(), "exit code: {:?}", out.status.code());
    let stdout = String::from_utf8(out.stdout).unwrap();
    assert_eq!(stdout, "migrate-all: 0 sets opened, 0 errors\n");
}

#[test]
fn migrate_all_reports_set_count() {
    let d = fresh_root();
    let mds = SqliteCasMds::open(d.path()).unwrap();
    let s1 = SetId(uuid::Uuid::now_v7());
    let s2 = SetId(uuid::Uuid::now_v7());
    let s3 = SetId(uuid::Uuid::now_v7());
    mds.create_set(&s1).unwrap();
    mds.create_set(&s2).unwrap();
    mds.create_set(&s3).unwrap();
    drop(mds);

    let out = cmd()
        .arg("--root")
        .arg(d.path())
        .arg("migrate-all")
        .output()
        .unwrap();
    assert!(out.status.success(), "exit code: {:?}", out.status.code());
    let stdout = String::from_utf8(out.stdout).unwrap();
    assert_eq!(stdout, "migrate-all: 3 sets opened, 0 errors\n");
}

#[test]
fn migrate_all_json_round_trips_via_serde() {
    let d = fresh_root();
    let mds = SqliteCasMds::open(d.path()).unwrap();
    let s1 = SetId(uuid::Uuid::now_v7());
    let s2 = SetId(uuid::Uuid::now_v7());
    mds.create_set(&s1).unwrap();
    mds.create_set(&s2).unwrap();
    drop(mds);

    let out = cmd()
        .arg("--root")
        .arg(d.path())
        .arg("--json")
        .arg("migrate-all")
        .output()
        .unwrap();
    assert!(out.status.success(), "exit code: {:?}", out.status.code());

    #[derive(serde::Deserialize)]
    struct Resp {
        sets: u64,
        errors: u64,
    }
    let resp: Resp = serde_json::from_slice(&out.stdout).expect("--json output must parse as JSON");
    assert_eq!(resp.sets, 2);
    assert_eq!(resp.errors, 0);
}

#[test]
fn stats_on_empty_root_renders_zero_counters_and_unit_ratio() {
    let d = fresh_root();
    SqliteCasMds::open(d.path()).unwrap();

    let out = cmd()
        .arg("--root")
        .arg(d.path())
        .arg("stats")
        .output()
        .unwrap();
    assert!(out.status.success(), "exit code: {:?}", out.status.code());
    let stdout = String::from_utf8(out.stdout).unwrap();
    // Pin the exact line shape so the operator format is stable.
    let expected = "\
sets:        0
containers:  0
items:       0
blobs:       0
total:       0 B
dedup ratio: 1.00
";
    assert_eq!(stdout, expected);
}

#[test]
fn stats_human_renders_thousands_separators_and_dedup() {
    let d = fresh_root();
    let mds = SqliteCasMds::open(d.path()).unwrap();
    let s = SetId(uuid::Uuid::now_v7());
    mds.create_set(&s).unwrap();
    let inbox = mds.create_container(&s, None, "INBOX", attrs()).unwrap();
    // Two items with the same body — same blob, refcount=2 in the
    // set. Rendered counters: 1 set, 1 container, 2 items, 1 blob,
    // dedup ratio 2.00 (logical 8B / physical 4B).
    let body = b"abcd";
    let blob = mds.put_blob(body).unwrap();
    for _ in 0..2 {
        mds.add_item(
            &s,
            &blob,
            &[Membership {
                container: inbox,
                flags: Flags(0),
                added_at: 1,
            }],
        )
        .unwrap();
    }
    drop(mds);

    let out = cmd()
        .arg("--root")
        .arg(d.path())
        .arg("stats")
        .output()
        .unwrap();
    assert!(out.status.success(), "exit code: {:?}", out.status.code());
    let stdout = String::from_utf8(out.stdout).unwrap();
    assert!(stdout.contains("sets:        1"), "stdout: {stdout}");
    assert!(stdout.contains("containers:  1"), "stdout: {stdout}");
    assert!(stdout.contains("items:       2"), "stdout: {stdout}");
    assert!(stdout.contains("blobs:       1"), "stdout: {stdout}");
    assert!(stdout.contains("dedup ratio: 2.00"), "stdout: {stdout}");
}

#[test]
fn stats_json_emits_top_level_object_with_explicit_null_sets_by_default() {
    let d = fresh_root();
    let mds = SqliteCasMds::open(d.path()).unwrap();
    let s = SetId(uuid::Uuid::now_v7());
    mds.create_set(&s).unwrap();
    let inbox = mds.create_container(&s, None, "INBOX", attrs()).unwrap();
    let blob = mds.put_blob(b"hello").unwrap();
    mds.add_item(
        &s,
        &blob,
        &[Membership {
            container: inbox,
            flags: Flags(0),
            added_at: 1,
        }],
    )
    .unwrap();
    drop(mds);

    let out = cmd()
        .arg("--root")
        .arg(d.path())
        .arg("--json")
        .arg("stats")
        .output()
        .unwrap();
    assert!(out.status.success(), "exit code: {:?}", out.status.code());

    // The CLI contract pins a stable top-level shape: `sets` is
    // always present, explicitly `null` by default and an array
    // under `--per-set`. Consumers branch on the value, not on key
    // presence — see _doc/2026-05-02-cosmix-mds-cli.md §stats.
    let v: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
    let obj = v.as_object().expect("top-level object");
    assert_eq!(obj["set_count"], 1);
    assert_eq!(obj["container_count"], 1);
    assert_eq!(obj["item_count"], 1);
    assert_eq!(obj["blob_count"], 1);
    assert_eq!(obj["total_bytes"], 5);
    assert!(obj["dedup_ratio"].is_number());
    assert!(
        obj.contains_key("sets"),
        "default stats must include `sets`"
    );
    assert!(
        obj["sets"].is_null(),
        "default stats must serialize `sets: null`"
    );
}

#[test]
fn stats_per_set_human_appends_table_after_global_summary() {
    let d = fresh_root();
    let mds = SqliteCasMds::open(d.path()).unwrap();
    let s1 = SetId(uuid::Uuid::now_v7());
    let s2 = SetId(uuid::Uuid::now_v7());
    mds.create_set(&s1).unwrap();
    mds.create_set(&s2).unwrap();
    mds.create_container(&s1, None, "INBOX", attrs()).unwrap();
    mds.create_container(&s2, None, "INBOX", attrs()).unwrap();
    drop(mds);

    let out = cmd()
        .arg("--root")
        .arg(d.path())
        .arg("stats")
        .arg("--per-set")
        .output()
        .unwrap();
    assert!(out.status.success(), "exit code: {:?}", out.status.code());
    let stdout = String::from_utf8(out.stdout).unwrap();
    // Global summary comes first, then a blank line, then a header
    // row, then one row per set.
    assert!(stdout.contains("sets:        2"));
    assert!(stdout.contains("SET"));
    assert!(stdout.contains("CONTAINERS"));
    assert!(stdout.contains("BYTES"));
    assert!(stdout.contains(&s1.0.to_string()));
    assert!(stdout.contains(&s2.0.to_string()));
}

#[test]
fn stats_per_set_json_includes_sets_array() {
    let d = fresh_root();
    let mds = SqliteCasMds::open(d.path()).unwrap();
    let s1 = SetId(uuid::Uuid::now_v7());
    let s2 = SetId(uuid::Uuid::now_v7());
    mds.create_set(&s1).unwrap();
    mds.create_set(&s2).unwrap();
    drop(mds);

    let out = cmd()
        .arg("--root")
        .arg(d.path())
        .arg("--json")
        .arg("stats")
        .arg("--per-set")
        .output()
        .unwrap();
    assert!(out.status.success(), "exit code: {:?}", out.status.code());
    let v: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
    let arr = v["sets"]
        .as_array()
        .expect("sets must be an array under --per-set");
    assert_eq!(arr.len(), 2);
    let ids: Vec<&str> = arr.iter().map(|r| r["set_id"].as_str().unwrap()).collect();
    assert!(ids.contains(&s1.0.to_string().as_str()));
    assert!(ids.contains(&s2.0.to_string().as_str()));
    // Field shape is stable: every row has the documented fields.
    for row in arr {
        let row = row.as_object().unwrap();
        assert!(row.contains_key("set_id"));
        assert!(row.contains_key("container_count"));
        assert!(row.contains_key("item_count"));
        assert!(row.contains_key("blob_count"));
        assert!(row.contains_key("total_bytes"));
    }
}

#[test]
fn migrate_all_creates_root_dirs_on_fresh_path() {
    // SqliteCasMds::open creates containers/ and blobs/ if missing,
    // so migrate-all on a fresh root is the documented "first-run"
    // ergonomic. Pin it: pointing at an empty tempdir succeeds and
    // leaves the canonical layout behind.
    let d = fresh_root();

    let out = cmd()
        .arg("--root")
        .arg(d.path())
        .arg("migrate-all")
        .output()
        .unwrap();
    assert!(out.status.success(), "exit code: {:?}", out.status.code());
    assert!(d.path().join("containers").is_dir());
    assert!(d.path().join("blobs").is_dir());
    assert!(d.path().join("blobs.sqlite").is_file());
}

#[test]
fn verify_on_empty_root_reports_zero_blobs_zero_mismatches() {
    let d = fresh_root();
    SqliteCasMds::open(d.path()).unwrap();

    let out = cmd()
        .arg("--root")
        .arg(d.path())
        .arg("verify")
        .output()
        .unwrap();
    assert!(out.status.success(), "exit code: {:?}", out.status.code());
    let stdout = String::from_utf8(out.stdout).unwrap();
    assert!(stdout.contains("verified 0 blobs"));
    assert!(stdout.contains("mismatches: 0"));
}

#[test]
fn verify_clean_blobs_exits_zero_with_zero_mismatches() {
    let d = fresh_root();
    let mds = SqliteCasMds::open(d.path()).unwrap();
    let s = SetId(uuid::Uuid::now_v7());
    mds.create_set(&s).unwrap();
    let inbox = mds.create_container(&s, None, "INBOX", attrs()).unwrap();
    let blob = mds.put_blob(b"hello world").unwrap();
    mds.add_item(
        &s,
        &blob,
        &[Membership {
            container: inbox,
            flags: Flags(0),
            added_at: 1,
        }],
    )
    .unwrap();
    drop(mds);

    let out = cmd()
        .arg("--root")
        .arg(d.path())
        .arg("verify")
        .output()
        .unwrap();
    assert!(out.status.success(), "exit code: {:?}", out.status.code());
    let stdout = String::from_utf8(out.stdout).unwrap();
    assert!(stdout.contains("verified 1 blobs"));
    assert!(stdout.contains("mismatches: 0"));
}

#[test]
fn verify_finds_mismatches_but_still_exits_zero() {
    // Per CLI doc §exit-codes: verify-with-mismatches is a
    // *finding*, not a failure. Operators who want to gate on
    // findings pipe `--json | jq`; the exit code stays 0.
    let d = fresh_root();
    let mds = SqliteCasMds::open(d.path()).unwrap();
    let s = SetId(uuid::Uuid::now_v7());
    mds.create_set(&s).unwrap();
    let inbox = mds.create_container(&s, None, "INBOX", attrs()).unwrap();
    let h_clean = mds.put_blob(b"clean").unwrap();
    let h_corrupt = mds.put_blob(b"will-be-corrupted").unwrap();
    let h_missing = mds.put_blob(b"will-be-deleted").unwrap();
    for h in [&h_clean, &h_corrupt, &h_missing] {
        mds.add_item(
            &s,
            h,
            &[Membership {
                container: inbox,
                flags: Flags(0),
                added_at: 1,
            }],
        )
        .unwrap();
    }
    drop(mds);

    // Corrupt one blob: rewrite its CAS file with different bytes.
    // Delete another: remove the CAS file entirely. Lay out the
    // path the way the store does: blobs/<aa>/<bb>/<full-hex>.
    let blobs_root = d.path().join("blobs");
    let mutate_blob = |hash: &cosmix_mds::BlobHash, action: BlobAction| {
        let hex = hash
            .0
            .iter()
            .map(|b| format!("{b:02x}"))
            .collect::<String>();
        let path = blobs_root.join(&hex[0..2]).join(&hex[2..4]).join(&hex);
        match action {
            BlobAction::Corrupt => std::fs::write(&path, b"different bytes").unwrap(),
            BlobAction::Delete => std::fs::remove_file(&path).unwrap(),
        }
    };
    mutate_blob(&h_corrupt, BlobAction::Corrupt);
    mutate_blob(&h_missing, BlobAction::Delete);

    let out = cmd()
        .arg("--root")
        .arg(d.path())
        .arg("verify")
        .output()
        .unwrap();
    assert!(out.status.success(), "exit code: {:?}", out.status.code());
    let stdout = String::from_utf8(out.stdout).unwrap();
    assert!(stdout.contains("verified 3 blobs"));
    assert!(
        stdout.contains("mismatches: 2 (1 hash-mismatch, 1 missing)"),
        "stdout: {stdout}"
    );
}

#[derive(Copy, Clone)]
enum BlobAction {
    Corrupt,
    Delete,
}

#[test]
fn verify_json_emits_breakdown_and_scope_tag() {
    let d = fresh_root();
    let mds = SqliteCasMds::open(d.path()).unwrap();
    let s = SetId(uuid::Uuid::now_v7());
    mds.create_set(&s).unwrap();
    let inbox = mds.create_container(&s, None, "INBOX", attrs()).unwrap();
    let blob = mds.put_blob(b"hello world").unwrap();
    mds.add_item(
        &s,
        &blob,
        &[Membership {
            container: inbox,
            flags: Flags(0),
            added_at: 1,
        }],
    )
    .unwrap();
    drop(mds);

    let out = cmd()
        .arg("--root")
        .arg(d.path())
        .arg("--json")
        .arg("verify")
        .output()
        .unwrap();
    assert!(out.status.success(), "exit code: {:?}", out.status.code());
    let v: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
    let obj = v.as_object().expect("top-level object");
    assert_eq!(obj["blobs_checked"], 1);
    assert_eq!(obj["mismatches"], 0);
    assert_eq!(obj["mismatches_hash"], 0);
    assert_eq!(obj["mismatches_missing"], 0);
    assert_eq!(obj["scope"], "full");
    // duration_ms is u64, not the serde Duration default — pin it
    // so the wire shape stays operator-tooling-friendly.
    assert!(obj["duration_ms"].is_u64());
}

#[test]
fn verify_since_is_accepted_and_tagged() {
    let d = fresh_root();
    SqliteCasMds::open(d.path()).unwrap();

    let out = cmd()
        .arg("--root")
        .arg(d.path())
        .arg("--json")
        .arg("verify")
        .arg("--since")
        .arg("1h")
        .output()
        .unwrap();
    assert!(out.status.success(), "exit code: {:?}", out.status.code());
    let v: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(v["scope"], "since");
}

#[test]
fn verify_container_uuid_is_accepted_and_tagged() {
    let d = fresh_root();
    SqliteCasMds::open(d.path()).unwrap();
    let cid = uuid::Uuid::now_v7();

    let out = cmd()
        .arg("--root")
        .arg(d.path())
        .arg("--json")
        .arg("verify")
        .arg("--container")
        .arg(cid.to_string())
        .output()
        .unwrap();
    assert!(out.status.success(), "exit code: {:?}", out.status.code());
    let v: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(v["scope"], "container");
}

#[test]
fn verify_rejects_mutually_exclusive_scope_flags() {
    let d = fresh_root();
    SqliteCasMds::open(d.path()).unwrap();

    // --full + --since at the same time should fail at clap parse
    // time. Exit code from clap arg-validation failures is 2 by
    // default — matching our EXIT_FAIL semantics.
    cmd()
        .arg("--root")
        .arg(d.path())
        .arg("verify")
        .arg("--full")
        .arg("--since")
        .arg("1h")
        .assert()
        .failure();
}

#[test]
fn verify_bad_duration_exits_two() {
    let d = fresh_root();
    SqliteCasMds::open(d.path()).unwrap();

    cmd()
        .arg("--root")
        .arg(d.path())
        .arg("verify")
        .arg("--since")
        .arg("not-a-duration")
        .assert()
        .code(2);
}

#[test]
fn verify_bad_container_uuid_exits_two() {
    let d = fresh_root();
    SqliteCasMds::open(d.path()).unwrap();

    cmd()
        .arg("--root")
        .arg(d.path())
        .arg("verify")
        .arg("--container")
        .arg("not-a-uuid")
        .assert()
        .code(2);
}

#[test]
fn gc_on_empty_root_reports_zero_deletions_with_real_verb() {
    let d = fresh_root();
    SqliteCasMds::open(d.path()).unwrap();

    let out = cmd()
        .arg("--root")
        .arg(d.path())
        .arg("gc")
        .output()
        .unwrap();
    assert!(out.status.success(), "exit code: {:?}", out.status.code());
    let stdout = String::from_utf8(out.stdout).unwrap();
    assert!(
        stdout.starts_with("gc: deleted 0 blobs, freed 0 B in "),
        "stdout: {stdout}"
    );
    // Each counter line is present so a future change to the
    // template doesn't silently drop a field operators look at.
    for needle in [
        "pass 1 candidates:",
        "pass 2 deleted:",
        "skipped (re-ref):",
        "skipped (re-touched):",
        "orphan rows swept:",
        "refcount_pending:",
    ] {
        assert!(stdout.contains(needle), "missing line {needle:?}: {stdout}");
    }
}

#[test]
fn gc_dry_run_uses_would_delete_phrasing() {
    let d = fresh_root();
    SqliteCasMds::open(d.path()).unwrap();

    let out = cmd()
        .arg("--root")
        .arg(d.path())
        .arg("gc")
        .arg("--dry-run")
        .output()
        .unwrap();
    assert!(out.status.success(), "exit code: {:?}", out.status.code());
    let stdout = String::from_utf8(out.stdout).unwrap();
    // The prefix and verb must distinguish dry-run output from
    // real output even when counters are zero — operators scrape
    // these strings.
    assert!(
        stdout.starts_with("gc dry-run: would delete 0 blobs, free 0 B in "),
        "stdout: {stdout}"
    );
}

#[test]
fn gc_json_emits_dry_run_field_and_full_counter_set() {
    let d = fresh_root();
    SqliteCasMds::open(d.path()).unwrap();

    // Default mode (dry_run=false).
    let out = cmd()
        .arg("--root")
        .arg(d.path())
        .arg("--json")
        .arg("gc")
        .output()
        .unwrap();
    assert!(out.status.success(), "exit code: {:?}", out.status.code());
    let v: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
    let obj = v.as_object().expect("top-level object");
    assert_eq!(obj["dry_run"], false);
    for key in [
        "blobs_deleted",
        "bytes_freed",
        "duration_ms",
        "candidates_pass1",
        "skipped_re_referenced",
        "skipped_re_touched",
        "orphan_rows_swept",
        "pending_rows_observed",
    ] {
        assert!(obj.contains_key(key), "missing field {key}: {obj:?}");
    }
    assert!(obj["duration_ms"].is_u64());

    // --dry-run flips the wire field.
    let out = cmd()
        .arg("--root")
        .arg(d.path())
        .arg("--json")
        .arg("gc")
        .arg("--dry-run")
        .output()
        .unwrap();
    assert!(out.status.success());
    let v: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(v["dry_run"], true);
}

#[test]
fn rebuild_index_on_empty_root_reports_zero_counters() {
    let d = fresh_root();
    SqliteCasMds::open(d.path()).unwrap();

    let out = cmd()
        .arg("--root")
        .arg(d.path())
        .arg("rebuild-index")
        .output()
        .unwrap();
    assert!(out.status.success(), "exit code: {:?}", out.status.code());
    let stdout = String::from_utf8(out.stdout).unwrap();
    assert!(
        stdout.starts_with("rebuild-index: 0 sets, 0 items, 0 blobs in "),
        "stdout: {stdout}"
    );
    assert!(stdout.contains("orphan blobs found: 0"), "stdout: {stdout}");
}

#[test]
fn rebuild_index_human_reports_set_item_blob_counts() {
    let d = fresh_root();
    let mds = SqliteCasMds::open(d.path()).unwrap();
    let s = SetId(uuid::Uuid::now_v7());
    mds.create_set(&s).unwrap();
    let inbox = mds.create_container(&s, None, "INBOX", attrs()).unwrap();
    let blob = mds.put_blob(b"hello world").unwrap();
    mds.add_item(
        &s,
        &blob,
        &[Membership {
            container: inbox,
            flags: Flags(0),
            added_at: 1,
        }],
    )
    .unwrap();
    drop(mds);

    let out = cmd()
        .arg("--root")
        .arg(d.path())
        .arg("rebuild-index")
        .output()
        .unwrap();
    assert!(out.status.success(), "exit code: {:?}", out.status.code());
    let stdout = String::from_utf8(out.stdout).unwrap();
    assert!(
        stdout.starts_with("rebuild-index: 1 sets, 1 items, 1 blobs in "),
        "stdout: {stdout}"
    );
    assert!(stdout.contains("orphan blobs found: 0"));
}

#[test]
fn rebuild_index_json_round_trips_via_serde() {
    let d = fresh_root();
    let mds = SqliteCasMds::open(d.path()).unwrap();
    let s = SetId(uuid::Uuid::now_v7());
    mds.create_set(&s).unwrap();
    let inbox = mds.create_container(&s, None, "INBOX", attrs()).unwrap();
    let blob = mds.put_blob(b"hello").unwrap();
    mds.add_item(
        &s,
        &blob,
        &[Membership {
            container: inbox,
            flags: Flags(0),
            added_at: 1,
        }],
    )
    .unwrap();
    drop(mds);

    let out = cmd()
        .arg("--root")
        .arg(d.path())
        .arg("--json")
        .arg("rebuild-index")
        .output()
        .unwrap();
    assert!(out.status.success(), "exit code: {:?}", out.status.code());
    let v: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
    let obj = v.as_object().expect("top-level object");
    assert_eq!(obj["sets_scanned"], 1);
    assert_eq!(obj["items_indexed"], 1);
    assert_eq!(obj["blobs_indexed"], 1);
    assert_eq!(obj["orphan_blobs_found"], 0);
    assert!(obj["duration_ms"].is_u64());
}

#[test]
fn export_human_writes_summary_line() {
    let d = fresh_root();
    let mds = SqliteCasMds::open(d.path()).unwrap();
    let s = SetId(uuid::Uuid::now_v7());
    mds.create_set(&s).unwrap();
    let inbox = mds.create_container(&s, None, "INBOX", attrs()).unwrap();
    let h = mds.put_blob(b"hello world").unwrap();
    mds.add_item(
        &s,
        &h,
        &[Membership {
            container: inbox,
            flags: Flags(0),
            added_at: 1,
        }],
    )
    .unwrap();
    drop(mds);

    let dest = d.path().join("out.tar");
    let out = cmd()
        .arg("--root")
        .arg(d.path())
        .arg("export")
        .arg(s.0.to_string())
        .arg(&dest)
        .output()
        .unwrap();
    assert!(out.status.success(), "exit code: {:?}", out.status.code());
    let stdout = String::from_utf8(out.stdout).unwrap();
    assert!(
        stdout.starts_with(&format!("exported set {}", s.0)),
        "stdout: {stdout}"
    );
    assert!(stdout.contains("1 blobs"), "stdout: {stdout}");
    assert!(dest.exists(), "tarball should exist at {}", dest.display());
}

#[test]
fn export_json_round_trips_via_serde() {
    let d = fresh_root();
    let mds = SqliteCasMds::open(d.path()).unwrap();
    let s = SetId(uuid::Uuid::now_v7());
    mds.create_set(&s).unwrap();
    let inbox = mds.create_container(&s, None, "INBOX", attrs()).unwrap();
    let h = mds.put_blob(b"json test").unwrap();
    mds.add_item(
        &s,
        &h,
        &[Membership {
            container: inbox,
            flags: Flags(0),
            added_at: 1,
        }],
    )
    .unwrap();
    drop(mds);

    let dest = d.path().join("out.tar");
    let out = cmd()
        .arg("--root")
        .arg(d.path())
        .arg("--json")
        .arg("export")
        .arg(s.0.to_string())
        .arg(&dest)
        .output()
        .unwrap();
    assert!(out.status.success(), "exit code: {:?}", out.status.code());
    let v: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
    let obj = v.as_object().expect("top-level object");
    assert_eq!(obj["set_id"], s.0.to_string());
    assert_eq!(obj["item_count"], 1);
    assert_eq!(obj["blob_count"], 1);
    assert!(obj["bytes_written"].is_u64());
    assert!(obj["duration_ms"].is_u64());
    assert_eq!(obj["tarball"], dest.display().to_string());
}

#[test]
fn export_unknown_set_exits_2() {
    let d = fresh_root();
    SqliteCasMds::open(d.path()).unwrap();
    let bogus = uuid::Uuid::now_v7();
    let dest = d.path().join("missing.tar");
    cmd()
        .arg("--root")
        .arg(d.path())
        .arg("export")
        .arg(bogus.to_string())
        .arg(&dest)
        .assert()
        .code(2);
    assert!(!dest.exists());
}

#[test]
fn export_bad_set_uuid_exits_2() {
    let d = fresh_root();
    SqliteCasMds::open(d.path()).unwrap();
    let dest = d.path().join("never-touched.tar");
    cmd()
        .arg("--root")
        .arg(d.path())
        .arg("export")
        .arg("not-a-uuid")
        .arg(&dest)
        .assert()
        .code(2)
        .stderr(predicate::str::contains("invalid set UUID"));
}

/// Helper: produce a tarball by exporting a freshly-built set on a
/// throwaway root, then drop the source so the same UUID is free to
/// be re-installed by import on a different root. Returns the
/// (tarball path, source TempDir to keep alive, set id) tuple — keep
/// the TempDir alive for the duration of the import test.
fn make_export_tarball() -> (TempDir, std::path::PathBuf, SetId) {
    let src = TempDir::new().unwrap();
    let mds = SqliteCasMds::open(src.path()).unwrap();
    let s = SetId(uuid::Uuid::now_v7());
    mds.create_set(&s).unwrap();
    let inbox = mds.create_container(&s, None, "INBOX", attrs()).unwrap();
    let h = mds.put_blob(b"importable bytes").unwrap();
    mds.add_item(
        &s,
        &h,
        &[Membership {
            container: inbox,
            flags: Flags(0),
            added_at: 1,
        }],
    )
    .unwrap();
    let dest = src.path().join("out.tar");
    mds.export_set(&s, &dest).unwrap();
    drop(mds);
    (src, dest, s)
}

#[test]
fn import_human_writes_summary_line() {
    let (_src, tarball, s) = make_export_tarball();
    let dst = fresh_root();
    SqliteCasMds::open(dst.path()).unwrap();

    let out = cmd()
        .arg("--root")
        .arg(dst.path())
        .arg("import")
        .arg(&tarball)
        .output()
        .unwrap();
    assert!(out.status.success(), "exit code: {:?}", out.status.code());
    let stdout = String::from_utf8(out.stdout).unwrap();
    assert!(
        stdout.starts_with(&format!("imported set {}", s.0)),
        "stdout: {stdout}"
    );
    assert!(stdout.contains("1 blobs"), "stdout: {stdout}");
}

#[test]
fn import_json_round_trips_via_serde() {
    let (_src, tarball, s) = make_export_tarball();
    let dst = fresh_root();
    SqliteCasMds::open(dst.path()).unwrap();

    let out = cmd()
        .arg("--root")
        .arg(dst.path())
        .arg("--json")
        .arg("import")
        .arg(&tarball)
        .output()
        .unwrap();
    assert!(out.status.success(), "exit code: {:?}", out.status.code());
    let v: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
    let obj = v.as_object().expect("top-level object");
    assert_eq!(obj["set_id"], s.0.to_string());
    assert_eq!(obj["item_count"], 1);
    assert_eq!(obj["blob_count"], 1);
    assert!(obj["bytes_read"].is_u64());
    assert!(obj["duration_ms"].is_u64());
    assert_eq!(obj["tarball"], tarball.display().to_string());
}

#[test]
fn import_missing_tarball_exits_2() {
    let dst = fresh_root();
    SqliteCasMds::open(dst.path()).unwrap();
    cmd()
        .arg("--root")
        .arg(dst.path())
        .arg("import")
        .arg(dst.path().join("does-not-exist.tar"))
        .assert()
        .code(2);
}

#[test]
fn import_set_already_exists_exits_2() {
    let (_src, tarball, _s) = make_export_tarball();
    let dst = fresh_root();
    SqliteCasMds::open(dst.path()).unwrap();
    cmd()
        .arg("--root")
        .arg(dst.path())
        .arg("import")
        .arg(&tarball)
        .assert()
        .code(0);
    cmd()
        .arg("--root")
        .arg(dst.path())
        .arg("import")
        .arg(&tarball)
        .assert()
        .code(2)
        .stderr(predicate::str::contains("already exists"));
}

#[test]
fn empty_root_lists_zero_sets_in_both_modes() {
    let d = fresh_root();
    SqliteCasMds::open(d.path()).unwrap();

    cmd()
        .arg("--root")
        .arg(d.path())
        .arg("list-sets")
        .assert()
        .code(0)
        .stdout("");

    let out = cmd()
        .arg("--root")
        .arg(d.path())
        .arg("--json")
        .arg("list-sets")
        .output()
        .unwrap();
    assert!(out.status.success());
    let trimmed = String::from_utf8(out.stdout).unwrap();
    assert_eq!(trimmed.trim(), r#"{"sets":[]}"#);
}
